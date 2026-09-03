mod common;
use common::*;

#[test]
fn finalize_produces_standard_layout() {
    let origin = build_origin(15, &[("dev", 8)], &[("v1", 4)]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    rgc::gitio::init_main_repo(&main, url).unwrap();
    rgc::gitio::run_git(
        &["fetch", "--quiet", url, "+refs/heads/*:refs/remotes/origin/*", "+refs/tags/*:refs/tags/*"],
        Some(&main),
    )
    .unwrap();
    std::fs::create_dir_all(main.join(".rgc")).unwrap(); // 模拟残留状态目录
    let plan =
        rgc::planner::build_plan(url, &rgc::refs::ls_remote(url).unwrap(), &rgc::planner::PlannerConfig::default()).unwrap();
    rgc::finalize::finalize(&plan, &main, false).unwrap();
    assert!(main.join("f0.txt").exists(), "worktree checked out");
    assert!(!main.join(".rgc").exists(), "state cleaned");
    let head = rgc::gitio::run_git(&["symbolic-ref", "HEAD"], Some(&main)).unwrap();
    assert_eq!(head.stdout.trim(), "refs/heads/main");
}
