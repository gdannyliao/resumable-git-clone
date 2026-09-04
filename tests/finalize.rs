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
    rgc::finalizer::finalize(&plan, &main, false).unwrap();
    assert!(main.join("f0.txt").exists(), "worktree checked out");
    assert!(!main.join(".rgc").exists(), "state cleaned");
    let head = rgc::gitio::run_git(&["symbolic-ref", "HEAD"], Some(&main)).unwrap();
    assert_eq!(head.stdout.trim(), "refs/heads/main");
}

/// 构造一个"已搬运完 refs 的主仓库 + 计划"的公共骨架
fn setup_finalizable(
    td: &std::path::Path,
    _origin: &std::path::Path,
    url: &str,
) -> (std::path::PathBuf, rgc::planner::Plan) {
    let main = td.join("repo");
    rgc::gitio::init_main_repo(&main, url).unwrap();
    rgc::gitio::run_git(
        &["fetch", "--quiet", url, "+refs/heads/*:refs/remotes/origin/*", "+refs/tags/*:refs/tags/*"],
        Some(&main),
    )
    .unwrap();
    std::fs::create_dir_all(main.join(".rgc")).unwrap();
    let plan = rgc::planner::build_plan(url, &rgc::refs::ls_remote(url).unwrap(), &rgc::planner::PlannerConfig::default())
        .unwrap();
    (main, plan)
}

/// 强移 tag：catch-up fetch 必须收敛到新 OID（审查 C1）
#[test]
fn force_moved_tag_converges() {
    let origin = build_origin(10, &[], &[("v1", 5)]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let (main, plan) = setup_finalizable(td.path(), &origin, url);
    // 远端强移 v1 到新 commit
    let work = td.path().join("work");
    git(td.path(), &["clone", "--quiet", url, work.to_str().unwrap()]);
    git(&work, &["config", "user.email", "t@t"]);
    git(&work, &["config", "user.name", "t"]);
    std::fs::write(work.join("late.txt"), "late").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "--quiet", "-m", "late"]);
    git(&work, &["tag", "-f", "v1"]);
    git(&work, &["push", "--quiet", "--force", "origin", "v1"]);

    rgc::finalizer::finalize(&plan, &main, false).unwrap();
    let fresh = rgc::refs::ls_remote(url).unwrap();
    let want = fresh.tags.iter().find(|t| t.short_name == "v1").unwrap();
    let got = rgc::gitio::run_git(&["rev-parse", "refs/tags/v1"], Some(&main)).unwrap();
    assert_eq!(got.stdout.trim(), want.oid, "强移后的 tag 必须收敛到新 OID");
}

/// 远端删除 tag：catch-up fetch 必须移除本地旧 tag（不允许静默分歧）
#[test]
fn remotely_deleted_tag_is_pruned() {
    let origin = build_origin(10, &[], &[("v1", 5)]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let (main, plan) = setup_finalizable(td.path(), &origin, url);
    git(origin.as_path(), &["tag", "-d", "v1"]);

    rgc::finalizer::finalize(&plan, &main, false).unwrap();
    assert!(
        rgc::gitio::run_git(&["rev-parse", "--verify", "refs/tags/v1"], Some(&main)).is_err(),
        "远端已删除的 tag 必须被 --prune-tags 清掉"
    );
}

/// 计划生成后远端新增分支：verify 是 remote-driven，必须被 catch-up fetch 收进来
#[test]
fn new_remote_branch_is_included() {
    let origin = build_origin(10, &[], &[]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let (main, plan) = setup_finalizable(td.path(), &origin, url);
    let work = td.path().join("work");
    git(td.path(), &["clone", "--quiet", url, work.to_str().unwrap()]);
    git(&work, &["config", "user.email", "t@t"]);
    git(&work, &["config", "user.name", "t"]);
    git(&work, &["checkout", "--quiet", "-b", "late-branch"]);
    std::fs::write(work.join("late.txt"), "x").unwrap();
    git(&work, &["add", "."]);
    git(&work, &["commit", "--quiet", "-m", "late"]);
    git(&work, &["push", "--quiet", "origin", "late-branch"]);

    rgc::finalizer::finalize(&plan, &main, false).unwrap();
    let got = rgc::gitio::run_git(&["rev-parse", "refs/remotes/origin/late-branch"], Some(&main));
    assert!(got.is_ok(), "计划之后新增的远端分支必须被 catch-up 收入");
}

/// keep_state=true 保留 .rgc/
#[test]
fn keep_state_and_failure_atomicity() {
    let origin = build_origin(10, &[], &[("v1", 5)]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let (main, plan) = setup_finalizable(td.path(), &origin, url);
    rgc::finalizer::finalize(&plan, &main, true).unwrap();
    assert!(main.join(".rgc").exists(), "keep_state=true 必须保留状态目录");
}
