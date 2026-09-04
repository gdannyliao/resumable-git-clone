//! Task 14（附录 A.15 验收标准）：并行端到端 + 全局 429 断路器 + 冷却加固。
//!
//! - A.15a：run 级总 429 断路器（超阈值整个 run 立即放弃，"upstream rate-limited
//!   — rerun later"），同时作为并发减半的信号源；
//! - A.15b：冷却等待循环重读（醒来后再查，直到归零）+ 每 worker 抖动；
//! - A.15c：state Mutex 化（认领/落盘全部锁内完成，保存串行化）；
//! - A.15d：Send+Sync 静态断言；
//! - 顺手项：风暴退避 60<<min(2) 升级；冷却消费测试；精确 Duration（不截断
//!   as_secs）；jobs 钳制。

mod common;
use common::*;
use rgc::equiv::{assert_same_objects, assert_same_refs, snapshot};
use rgc::finalizer::finalize;
use rgc::planner::{build_plan, PlannerConfig};
use rgc::refs::ls_remote;
use rgc::scheduler::{rate_limit_backoff_secs, run, FetchHook, SchedulerConfig, SleepHook};
use rgc::state::{PieceStatus, State};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn no_sleep() -> SleepHook {
    Arc::new(|_| {})
}

/// 单链片 fixture：只有 main 的 origin → 恰好一个链式片。
fn plan_for(url: &str, initial_step: u32) -> rgc::planner::Plan {
    let plan = build_plan(url, &ls_remote(url).unwrap(), &PlannerConfig { initial_step, ..Default::default() }).unwrap();
    assert_eq!(plan.pieces.len(), 1, "fixture assumes a single chain piece");
    plan
}

/// A.15d：Send+Sync 静态断言 —— worker 以 std::thread::scope 借用 cfg 跨线程，
/// cfg / 测试缝隙类型一旦失去 Send+Sync，本测试连同整个并行调度编译失败。
#[test]
fn send_sync_assert() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SchedulerConfig>();
    assert_send_sync::<FetchHook>();
    assert_send_sync::<SleepHook>();
}

/// jobs 钳制（1..=8）：误配 0 会得到零 worker（静默假完成），过大爆线程。
#[test]
fn jobs_are_clamped() {
    assert_eq!(SchedulerConfig { jobs: 0, ..Default::default() }.clamped_jobs(), 1);
    assert_eq!(SchedulerConfig { jobs: 3, ..Default::default() }.clamped_jobs(), 3);
    assert_eq!(SchedulerConfig { jobs: 999, ..Default::default() }.clamped_jobs(), 8);
}

/// 顺手项：风暴退避按连续 429 数升级 —— 60s → 120s → 240s 封顶
/// （附录公式 60<<rate_limits.min(2)，以 0 基步数计）。
#[test]
fn storm_backoff_escalates_and_caps() {
    assert_eq!(rate_limit_backoff_secs(1), 60);
    assert_eq!(rate_limit_backoff_secs(2), 120);
    assert_eq!(rate_limit_backoff_secs(3), 240);
    assert_eq!(rate_limit_backoff_secs(99), 240, "escalation caps at 240s");
}

/// 并行端到端：jobs=3 真并行跑完 → 全部 Done → finalize → 产物布局与
/// git clone 等价（scheduler_serial 串行 e2e 的并行版）。
#[test]
fn parallel_end_to_end_clone() {
    let origin = build_origin(120, &[("dev", 60)], &[("v1", 30), ("v2", 60), ("v3", 100)]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    let plan = build_plan(url, &ls_remote(url).unwrap(), &PlannerConfig { initial_step: 20, ..Default::default() }).unwrap();
    assert!(plan.pieces.len() >= 3, "fixture must offer enough pieces for 3 workers");
    run(&plan, &main, &SchedulerConfig { jobs: 3, ..Default::default() }).unwrap();
    let st = State::load(&main).unwrap().unwrap();
    assert!(st.pieces.iter().all(|p| p.status == PieceStatus::Done), "all pieces must be Done, got {:?}", st.pieces);
    assert_eq!(st.fingerprint, plan.fingerprint());
    finalize(&plan, &main, false).unwrap();
    assert!(!main.join(".rgc").exists(), "finalize(keep_state=false) must remove .rgc/");
    let reference = td.path().join("ref");
    git(td.path(), &["clone", "--quiet", url, reference.to_str().unwrap()]);
    let (a, b) = (snapshot(&main).unwrap(), snapshot(&reference).unwrap());
    assert_same_refs(&a, &b).unwrap();
    assert_same_objects(&a, &b).unwrap();
}

/// A.15a：全局 429 断路器 —— 片级限流上限被刻意调高也不许逐片烧穿：
/// 总 429 数越过断路器阈值，整个 run 立即以 "upstream rate-limited — rerun
/// later" 放弃；fetch 次数封顶在阈值+1（有界，绝非 pieces×cap）；
/// 且退避如实按 60→120→240 升级（顺手项）。
#[test]
fn rate_limit_storm_bails_run() {
    let origin = build_origin(10, &[], &[]);
    let url = origin.to_str().unwrap();
    let plan = plan_for(url, 20);
    let calls = Arc::new(AtomicUsize::new(0));
    let storm: FetchHook = {
        let calls = calls.clone();
        Arc::new(move |_real: &dyn Fn() -> anyhow::Result<()>| {
            calls.fetch_add(1, Ordering::Relaxed);
            Err(rgc::errors::RgcError::RateLimited("HTTP 429".into()).into())
        })
    };
    let sleep_log: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let sleep: SleepHook = {
        let log = sleep_log.clone();
        Arc::new(move |d: Duration| log.lock().unwrap().push(d.as_secs()))
    };
    let cfg = SchedulerConfig {
        fetch: storm,
        sleep,
        max_rate_limits: 100,   // 片级上限调高：止损必须由全局断路器完成
        rate_limit_breaker: 5,  // 总 429 > 5 → 放弃整个 run
        ..Default::default()
    };
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    let err = run(&plan, &main, &cfg).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("upstream rate-limited — rerun later"), "unexpected error: {msg}");
    assert_eq!(calls.load(Ordering::Relaxed), 6, "breaker threshold+1 fetches — bounded, not pieces×cap");
    assert_eq!(
        *sleep_log.lock().unwrap(),
        vec![60, 120, 240, 240, 240],
        "storm backoff must escalate 60<<min(2) and cap at 240s"
    );
}

/// 真并行证明：fetch 钩子自带 200ms 真睡并统计并发峰值 —— jobs=3 时峰值必须 >1
/// （串行执行峰值恒为 1）；200ms 窗口远宽于锁内认领的微秒级开销，断言余量充足。
#[test]
fn concurrency_actually_parallel() {
    let origin = build_origin(30, &[("dev", 15)], &[("v1", 5), ("v2", 10), ("v3", 20)]);
    let url = origin.to_str().unwrap();
    let plan = build_plan(url, &ls_remote(url).unwrap(), &PlannerConfig::default()).unwrap();
    assert!(plan.pieces.len() >= 3, "fixture must offer enough pieces for 3 workers");
    let cur = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let fetch: FetchHook = {
        let cur = cur.clone();
        let peak = peak.clone();
        Arc::new(move |real: &dyn Fn() -> anyhow::Result<()>| {
            let now = cur.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(200));
            let r = real();
            cur.fetch_sub(1, Ordering::SeqCst);
            r
        })
    };
    let cfg = SchedulerConfig { jobs: 3, fetch, sleep: no_sleep(), ..Default::default() };
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    run(&plan, &main, &cfg).unwrap();
    let st = State::load(&main).unwrap().unwrap();
    assert!(st.pieces.iter().all(|p| p.status == PieceStatus::Done), "got {:?}", st.pieces);
    assert!(peak.load(Ordering::SeqCst) >= 2, "fetches must genuinely overlap (serial peak == 1), got peak {}", peak.load(Ordering::SeqCst));
}

/// A.15b + 顺手项：冷却等待必须"循环重读直到归零"。睡眠钩子真实微睡（cap 200ms
/// 推进时钟），首次睡眠期间把冷却再布防到 +2s。循环重读的实现会跟着剩余时间
/// 递减（≥3 条、严格递减、亚秒精确）；一次性 sleep 只记 1 条就带着未来冷却放行。
/// 有界性：+2s ÷ 200ms/次 ≈ 10 次迭代封顶，绝不挂死。
#[test]
fn cooldown_wait_loops_until_past_and_keeps_precision() {
    let origin = build_origin(8, &[], &[("v1", 4)]);
    let url = origin.to_str().unwrap();
    // 2 片（1 链 + 1 tag 批）+ jobs=1：串行认领，全部冷却睡眠可归因
    let plan = build_plan(url, &ls_remote(url).unwrap(), &PlannerConfig::default()).unwrap();
    assert_eq!(plan.pieces.len(), 2);
    // 冷却 Arc 先于 cfg 构造：睡眠钩子需要在睡眠窗口内再布防
    let cooldown = Arc::new(Mutex::new(Instant::now()));
    *cooldown.lock().unwrap() = Instant::now() + Duration::from_secs(60); // 预布防：首片认领前必须消费
    let re_armed = Arc::new(AtomicBool::new(false));
    let sleep_log: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));
    let sleep: SleepHook = {
        let log = sleep_log.clone();
        let cooldown = cooldown.clone();
        let re_armed = re_armed.clone();
        Arc::new(move |d: Duration| {
            log.lock().unwrap().push(d);
            // 真实微睡推进时钟（no-op 钩子 + 未来冷却 + 不走的时钟 = 死循环）
            std::thread::sleep(d.min(Duration::from_millis(200)));
            // 首次冷却睡眠期间，"其他 worker"又撞了一次 429：冷却再布防到 +2s
            if !re_armed.swap(true, Ordering::SeqCst) {
                *cooldown.lock().unwrap() = Instant::now() + Duration::from_secs(2);
            }
        })
    };
    let cfg = SchedulerConfig { jobs: 1, cooldown, fetch: Arc::new(|real: &dyn Fn() -> anyhow::Result<()>| real()), sleep, ..Default::default() };
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    run(&plan, &main, &cfg).unwrap();
    let st = State::load(&main).unwrap().unwrap();
    assert!(st.pieces.iter().all(|p| p.status == PieceStatus::Done), "got {:?}", st.pieces);
    let waits = sleep_log.lock().unwrap().clone();
    assert!(waits.len() >= 3, "冷却等待必须循环重读再布防的冷却（一次性 sleep 只有 1 条），got {:?}", waits);
    assert!(*waits.first().unwrap() > Duration::from_secs(59), "初始等待必须是完整剩余时间且亚秒精确（不得 as_secs 截断），got {:?}", waits.first());
    let rest = &waits[1..];
    assert!(!rest.is_empty() && rest.windows(2).all(|w| w[0] > w[1]), "再布防后的等待必须随剩余时间严格递减（每轮重读），got {:?}", rest);
    assert!(rest.iter().all(|w| *w < Duration::from_secs(3)), "重读后的每轮只睡剩余量（不得重复整段 60s），got {:?}", rest);
}
