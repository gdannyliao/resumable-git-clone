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
