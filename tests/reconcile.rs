mod common;
use common::*;
use rgc::planner::{build_plan, PlannerConfig};
use rgc::refs::ls_remote;
use rgc::state::{reconcile, PieceStatus};

#[test]
fn reconcile_marks_completed_refs_done() {
    let origin = build_origin(30, &[("dev", 10)], &[]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    rgc::gitio::init_main_repo(&main, url).unwrap();
    // 手工伪造一个已完成的分支
    rgc::gitio::run_git(&["fetch", "--quiet", url, "+refs/heads/dev:refs/remotes/origin/dev"], Some(&main)).unwrap();
    let plan = build_plan(url, &ls_remote(url).unwrap(), &PlannerConfig::default()).unwrap();
    let st = reconcile(&plan, &main);
    let dev = st.pieces.iter().find(|p| p.id.contains("dev")).unwrap();
    assert_eq!(dev.status, PieceStatus::Done);
    assert_eq!(st.pieces.iter().filter(|p| p.status == PieceStatus::Done).count(), 1);
}

#[test]
fn reconcile_wipes_refs_when_repo_unhealthy() {
    let origin = build_origin(10, &[], &[]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    rgc::gitio::init_main_repo(&main, url).unwrap();
    // 指向不存在对象的假 ref → fsck 失败。
    // 注意：update-ref 拒收不存在的对象（实测 exit 128），只能手写 loose ref 文件。
    let origin_refs = main.join(".git").join("refs").join("remotes").join("origin");
    std::fs::create_dir_all(&origin_refs).unwrap();
    std::fs::write(origin_refs.join("main"), "0123456789012345678901234567890123456789\n").unwrap();
    let plan = build_plan(url, &ls_remote(url).unwrap(), &PlannerConfig::default()).unwrap();
    let st = reconcile(&plan, &main);
    assert!(st.pieces.iter().all(|p| p.status == PieceStatus::Pending));
    let refs = rgc::gitio::run_git(&["for-each-ref", "refs/remotes/origin"], Some(&main)).unwrap();
    assert!(refs.stdout.trim().is_empty());
}
