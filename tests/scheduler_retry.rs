//! Task 13（附录 A.14 扩大章程）：失败路径测试套件。
//!
//! 测试缝隙（I2）：`SchedulerConfig::fetch` 包装每次真实 fetch —— 参数是
//! "本步真实 fetch"闭包，默认直通。测试注入 (a) 失败 N 次后放行、(b) 恒败、
//! (c) 假成功（返回 Ok 但不调闭包 → 零推进）；`SchedulerConfig::sleep`
//! 注入 no-op，使 60s 级限流退避/冷却在测试中瞬时完成。
//! 全部用例驱动真实 run()/run_piece 状态机并断言落盘 state，而非 decide 纯函数。

mod common;
use common::*;
use rgc::errors::RgcError;
use rgc::planner::{build_plan, PlannerConfig};
use rgc::refs::ls_remote;
use rgc::scheduler::{run, FetchHook, SchedulerConfig, SleepHook};
use rgc::state::{PieceStatus, State};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// fetch 缝隙的调用计数（Arc 共享，供断言 fetch 恰好发生 N 次）
type Calls = Arc<Mutex<usize>>;

fn no_sleep() -> SleepHook {
    Arc::new(|_| {})
}

/// (a) 前 `fail_times` 次 fetch 返回注入错误（`usize::MAX` = 恒败），此后放行真实 fetch。
fn fail_first_then_real(err: RgcError, fail_times: usize) -> (FetchHook, Calls) {
    let calls: Calls = Arc::new(Mutex::new(0));
    let hook: FetchHook = {
        let calls = calls.clone();
        Arc::new(move |real: &dyn Fn() -> anyhow::Result<()>| -> anyhow::Result<()> {
            let mut n = calls.lock().unwrap();
            *n += 1;
            if *n <= fail_times {
                // RgcError 无 Clone：经公开访问ors 按类别+消息重建一份注入错误
                let e = RgcError::from_kind(err.kind(), err.message().to_owned());
                return Err(e.into());
            }
            drop(n);
            real()
        })
    };
    (hook, calls)
}

/// (c) 仅第 1 次放行真实 fetch（造出浅边界），此后假成功：Ok 但不 fetch
/// → 0 字节 0 提交、浅边界仍在 → A.5 零推进停滞。
fn real_once_then_stall() -> (FetchHook, Calls) {
    let calls: Calls = Arc::new(Mutex::new(0));
    let hook: FetchHook = {
        let calls = calls.clone();
        Arc::new(move |real: &dyn Fn() -> anyhow::Result<()>| -> anyhow::Result<()> {
            let mut n = calls.lock().unwrap();
            *n += 1;
            let first = *n == 1;
            drop(n);
            if first { real() } else { Ok(()) }
        })
    };
    (hook, calls)
}

/// 单链片 fixture：只有 main 的 origin → 恰好一个链式片。
fn plan_for(url: &str, initial_step: u32) -> rgc::planner::Plan {
    let plan = build_plan(url, &ls_remote(url).unwrap(), &PlannerConfig { initial_step, ..Default::default() }).unwrap();
    assert_eq!(plan.pieces.len(), 1, "fixture assumes a single chain piece");
    plan
}

/// 测试用配置：退避/冷却瞬时化 + 小预算（快速收敛，不改变语义）。
fn cfg_with(fetch: FetchHook) -> SchedulerConfig {
    SchedulerConfig { max_attempts: 2, fetch, sleep: no_sleep(), ..Default::default() }
}

/// C1 回归：片预算内重试耗尽 → 终态 Failed，run() 以 "failed permanently"
/// 终止并带上片的最后错误；fetch 恰好发生 max_attempts+1 次
/// （修复前 GiveUp 折回 Pending → 无限重认领热循环，本测试会挂死）。
#[test]
fn exhaustion_terminates_cleanly() {
    let origin = build_origin(10, &[], &[]);
    let url = origin.to_str().unwrap();
    let plan = plan_for(url, 20);
    let (fetch, calls) = fail_first_then_real(RgcError::Network("injected network failure".into()), usize::MAX);
    let cfg = cfg_with(fetch);
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    let err = run(&plan, &main, &cfg).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("failed permanently"), "unexpected error: {msg}");
    assert!(msg.contains("injected network failure"), "run must surface the piece's last error: {msg}");
    assert_eq!(*calls.lock().unwrap(), cfg.max_attempts as usize + 1, "exactly budget+1 fetch attempts — no Failed-piece reclaim spin (C1)");
    let st = State::load(&main).unwrap().unwrap();
    assert_eq!(st.pieces[0].status, PieceStatus::Failed);
    assert_eq!(st.pieces[0].attempts, cfg.max_attempts + 1);
}

/// 两次 Network 失败后恢复：片 Done，两次重试如实计入片预算（持久化 attempts==2）。
#[test]
fn retry_then_success() {
    let origin = build_origin(8, &[], &[]);
    let url = origin.to_str().unwrap();
    let plan = plan_for(url, 20);
    let (fetch, calls) = fail_first_then_real(RgcError::Network("injected flake".into()), 2);
    let cfg = cfg_with(fetch);
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    run(&plan, &main, &cfg).unwrap();
    assert!(*calls.lock().unwrap() >= 3, "two injected failures + at least one real fetch");
    let st = State::load(&main).unwrap().unwrap();
    assert_eq!(st.pieces[0].status, PieceStatus::Done);
    assert_eq!(st.pieces[0].attempts, 2, "two injected network failures must be billed to the piece budget");
    assert_eq!(st.pieces[0].rate_limits, 0);
}

/// spec §5：429 不计片的重试次数 —— 3 次限流后恢复，片 Done 且 attempts 仍为 0
/// （预算分毫未动）；且每次限流即便片最终成功也必须布防全局冷却（spec §4.4）。
#[test]
fn rate_limited_does_not_burn_budget() {
    let origin = build_origin(8, &[], &[]);
    let url = origin.to_str().unwrap();
    let plan = plan_for(url, 20);
    let (fetch, calls) = fail_first_then_real(RgcError::RateLimited("HTTP 429".into()), 3);
    let cfg = cfg_with(fetch);
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    run(&plan, &main, &cfg).unwrap();
    assert!(*calls.lock().unwrap() >= 4, "3 rate-limits + at least one real fetch");
    let st = State::load(&main).unwrap().unwrap();
    assert_eq!(st.pieces[0].status, PieceStatus::Done);
    assert_eq!(st.pieces[0].attempts, 0, "429s must not consume the piece attempt budget (spec §5)");
    // A.15 顺手项：成功步重置"连续"计数，Done 后持久化值归零；
    // 限流确曾发生由 calls>=4 证明，计数的持久化由 storm 测试（终态 Failed）覆盖。
    assert_eq!(st.pieces[0].rate_limits, 0, "successful step resets the consecutive counter");
    assert!(*cfg.cooldown.lock().unwrap() > Instant::now(), "rate-limited fetches must arm the global cooldown");
}

/// 429 风暴：限流不计预算 ≠ 无限重试 —— 超过独立限流上限后片 Failed，
/// 报告含明确消息 "rate-limited too many times"；片预算全程未被消费。
#[test]
fn rate_limit_storm_gives_up() {
    let origin = build_origin(10, &[], &[]);
    let url = origin.to_str().unwrap();
    let plan = plan_for(url, 20);
    let (fetch, calls) = fail_first_then_real(RgcError::RateLimited("HTTP 429".into()), usize::MAX);
    let mut cfg = cfg_with(fetch);
    cfg.max_rate_limits = 3;
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    let err = run(&plan, &main, &cfg).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("failed permanently"), "unexpected error: {msg}");
    assert!(msg.contains("rate-limited too many times"), "cap must surface a clear reason: {msg}");
    assert_eq!(*calls.lock().unwrap(), 4, "3 tolerated + 1 over the cap");
    let st = State::load(&main).unwrap().unwrap();
    assert_eq!(st.pieces[0].status, PieceStatus::Failed);
    assert_eq!(st.pieces[0].attempts, 0, "even a storm of 429s must leave the attempt budget untouched");
    assert_eq!(st.pieces[0].rate_limits, 4);
}

/// 限流计数与片预算同哲学：attempts 在认领时 rebill（C2），rate_limits 同样
/// 在 rerun 认领时清零 —— 否则风暴耗尽后的 rerun 一次 429 就立刻放弃。
#[test]
fn rate_limit_budget_is_rebilled_on_rerun() {
    let origin = build_origin(8, &[], &[]);
    let url = origin.to_str().unwrap();
    let plan = plan_for(url, 20);
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    // 第一轮：429 风暴（上限 3）→ Failed，rate_limits 已到顶
    let (storm, _) = fail_first_then_real(RgcError::RateLimited("HTTP 429".into()), usize::MAX);
    let cfg = SchedulerConfig { max_rate_limits: 3, fetch: storm, sleep: no_sleep(), ..Default::default() };
    assert!(run(&plan, &main, &cfg).is_err());
    // rerun：仅 1 次限流后放行 → 必须以全新限流配额出发并跑完
    let (one_flake, calls) = fail_first_then_real(RgcError::RateLimited("HTTP 429".into()), 1);
    let cfg2 = SchedulerConfig { max_rate_limits: 3, fetch: one_flake, sleep: no_sleep(), ..Default::default() };
    run(&plan, &main, &cfg2).unwrap();
    let st = State::load(&main).unwrap().unwrap();
    assert_eq!(st.pieces[0].status, PieceStatus::Done);
    assert_eq!(st.pieces[0].attempts, 0);
    // rebill 的行为证据就是上一行的 run 成功 + calls>=2：若无 claim 时清零，
    // 陈旧计数(=3) + 首个 429 → 4 > cap(3) 会在第一次限流就放弃。
    // 持久化 rate_limits 经成功步重置（A.15）归零，不再承载 rebill 证据。
    assert_eq!(st.pieces[0].rate_limits, 0);
    assert!(*calls.lock().unwrap() >= 2);
}

/// A.5 回归（非空转版）：fetch "成功"但零字节零提交、浅边界仍在
/// → 每次停滞计入片预算，最终 Failed；报告指明 zero-progress。
#[test]
fn zero_progress_stall_counts_as_attempt() {
    let origin = build_origin(30, &[], &[]);
    let url = origin.to_str().unwrap();
    let plan = plan_for(url, 10); // 首步 --depth=10 < 30 提交：真 fetch 后浅边界仍在
    let (fetch, calls) = real_once_then_stall();
    let cfg = cfg_with(fetch);
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    let err = run(&plan, &main, &cfg).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("failed permanently"), "unexpected error: {msg}");
    assert!(msg.contains("zero-progress"), "stall must be reported as such: {msg}");
    let st = State::load(&main).unwrap().unwrap();
    assert_eq!(st.pieces[0].status, PieceStatus::Failed);
    assert_eq!(st.pieces[0].attempts, cfg.max_attempts + 1, "each zero-progress stall must bill the piece budget (A.5 non-vacuous)");
    assert_eq!(*calls.lock().unwrap(), cfg.max_attempts as usize + 2, "1 real fetch + one stalled fetch per attempt");
}
