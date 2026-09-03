mod common;
use common::*;
use rgc::equiv::{assert_same_objects, assert_same_refs, snapshot};
use rgc::finalizer::finalize;
use rgc::planner::{build_plan, PlannerConfig};
use rgc::refs::ls_remote;
use rgc::scheduler::{run, SchedulerConfig};
use rgc::state::{PieceStatus, State};

/// 串行端到端：真实调度器跑完 → finalize → 产物布局与 git clone 等价。
#[test]
fn serial_end_to_end_clone() {
    let origin = build_origin(50, &[("dev", 20)], &[("v1", 10), ("v2", 40)]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    // 附录 A.1：build_plan 返回 Result
    let plan = build_plan(url, &ls_remote(url).unwrap(), &PlannerConfig { initial_step: 10, ..Default::default() }).unwrap();
    run(&plan, &main, &SchedulerConfig::default()).unwrap();
    // 全部片 Done，state 携带 plan 指纹（A.2）
    let st = State::load(&main).unwrap().unwrap();
    assert!(st.pieces.iter().all(|p| p.status == PieceStatus::Done), "all pieces must be Done");
    assert_eq!(st.fingerprint, plan.fingerprint());
    // A.6：调度器永不写 wip —— 链片只在完成后做一次 final 搬运
    let wip = rgc::gitio::run_git(&["for-each-ref", "refs/rgc/wip"], Some(&main)).unwrap();
    assert!(wip.stdout.trim().is_empty(), "scheduler must never write wip refs");
    // finalize（A.13 签名）→ 与 git clone 布局等价
    finalize(&plan, &main, false).unwrap();
    assert!(!main.join(".rgc").exists(), "finalize(keep_state=false) must remove .rgc/");
    let reference = td.path().join("ref");
    git(td.path(), &["clone", "--quiet", url, reference.to_str().unwrap()]);
    let (a, b) = (snapshot(&main).unwrap(), snapshot(&reference).unwrap());
    assert_same_refs(&a, &b).unwrap();
    assert_same_objects(&a, &b).unwrap();
}

/// C1：GiveUp 必须落在终态 Failed —— run() 终止并报告失败片。
/// 修复前：GiveUp 把片折回 Pending → 工作循环无条件重认领第一个 Pending 片
/// → 永久失败片被无限重认领（实测 ~22 次 fetch/秒），run() 永不返回、
/// 失败报告永不交付 —— 本测试会挂死（超时即回归信号）。
#[test]
fn giveup_terminates_with_failure_report() {
    let origin = build_origin(10, &[], &[]);
    let url = origin.to_str().unwrap();
    let plan = build_plan(url, &ls_remote(url).unwrap(), &PlannerConfig::default()).unwrap();
    // 指向不可达端点：fetch 必然反复失败（connection refused → Network → 预算耗尽 → GiveUp）。
    // 指纹按改后的 url 计算，state 由 run() 现建，无 mismatch 问题。
    let mut bad = plan.clone();
    bad.url = "https://127.0.0.1:1/x.git".to_string();
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    let cfg = SchedulerConfig { max_attempts: 1, ..Default::default() };
    let err = run(&bad, &main, &cfg).unwrap_err();
    assert!(err.to_string().contains("failed permanently"), "unexpected error: {err:#}");
    // 终态必须落盘（而非折回 Pending）：rerun 时由 run() 折返重试
    let st = State::load(&main).unwrap().unwrap();
    assert!(st.pieces.iter().all(|p| p.status == PieceStatus::Failed), "GiveUp must settle pieces in Failed, got {:?}", st.pieces);
}

/// C2：attempts 只约束单次 run 内的重试 —— 认领时重新计费。
/// 修复前 rebill 在 run_piece 里判 `status == Pending`，而 write-ahead 认领
/// 早已把片置 Running → 条件永假（死代码），遗留的 99 次 attempts 永远清不掉。
/// 本测试预置 attempts=99 的 Pending 片：修复后认领即清零并跑完 Done；
/// 修复前片照样 Done 但 attempts 停在 99 → 断言失败。
#[test]
fn resume_resets_exhausted_budget() {
    let origin = build_origin(10, &[], &[]);
    let url = origin.to_str().unwrap();
    let plan = build_plan(url, &ls_remote(url).unwrap(), &PlannerConfig::default()).unwrap();
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    // 预置 state：上次 run 耗尽遗留 attempts=99 —— 必须在 run() 之前落盘
    let mut st = State::new(&plan);
    st.pieces[0].status = PieceStatus::Pending;
    st.pieces[0].attempts = 99;
    st.save(&main).unwrap();
    run(&plan, &main, &SchedulerConfig::default()).unwrap();
    let st = State::load(&main).unwrap().unwrap();
    assert_eq!(st.pieces[0].status, PieceStatus::Done);
    assert_eq!(st.pieces[0].attempts, 0, "claim-time rebill must reset stale attempts");
}
