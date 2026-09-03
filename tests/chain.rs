mod common;
use common::*;
use rgc::gitio::*;

#[test]
fn chain_steps_fetch_full_history_and_transport() {
    let origin = build_origin(50, &[], &[]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    init_main_repo(&main, url).unwrap();
    let piece = main.join(".rgc").join("pieces").join("chain_main");
    ensure_piece_repo(&main, &piece, url).unwrap();

    let mut guard = 0;
    loop {
        fetch_chain_step(&piece, "refs/heads/main", "main", 10, 0, false).unwrap();
        guard += 1;
        if !is_shallow(&piece) || guard > 10 {
            break;
        }
    }
    assert!(!is_shallow(&piece), "chain should complete within steps");
    // 中途命名空间：wip
    transport_to_main(&main, &piece, "refs/heads/main", "main", false).unwrap();
    assert!(run_git(&["rev-parse", "--verify", "refs/rgc/wip/main"], Some(&main)).is_ok());
    // 完成命名空间：正式 refs，wip 删除
    transport_to_main(&main, &piece, "refs/heads/main", "main", true).unwrap();
    let tip = run_git(&["rev-parse", "refs/remotes/origin/main"], Some(&main)).unwrap();
    let want = run_git(&["rev-parse", "refs/heads/main"], Some(origin.as_path())).unwrap();
    assert_eq!(tip.stdout.trim(), want.stdout.trim());
    assert!(run_git(&["rev-parse", "--verify", "refs/rgc/wip/main"], Some(&main)).is_err());
}

/// B1：浅仓搬运必须显式失败（git 只 warning + exit 0 静默，断言必须兜住）
#[test]
fn transport_fails_from_shallow_piece() {
    let origin = build_origin(20, &[], &[]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    init_main_repo(&main, url).unwrap();
    let piece = main.join(".rgc").join("pieces").join("chain_main");
    ensure_piece_repo(&main, &piece, url).unwrap();
    // 仍浅（20 commits，只取了 10）
    fetch_chain_step(&piece, "refs/heads/main", "main", 10, 0, false).unwrap();
    assert!(is_shallow(&piece));
    let r = transport_to_main(&main, &piece, "refs/heads/main", "main", true);
    assert!(r.is_err(), "shallow 搬运必须显式失败，不能静默 no-op");
    assert!(run_git(&["rev-parse", "--verify", "refs/remotes/origin/main"], Some(&main)).is_err());
}

/// B2：远端历史重写（force-push）后旧浅边界悬空，必须检测并重置恢复
#[test]
fn force_push_rewrite_resets_shallow_wedge() {
    let origin = build_origin(10, &[], &[]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    init_main_repo(&main, url).unwrap();
    let piece = main.join(".rgc").join("pieces").join("chain_main");
    ensure_piece_repo(&main, &piece, url).unwrap();
    fetch_chain_step(&piece, "refs/heads/main", "main", 5, 0, false).unwrap();
    assert!(is_shallow(&piece));

    // 重写远端历史：孤立根 + 强制推送，使旧浅边界不在新历史中
    let work2 = td.path().join("work2");
    git(td.path(), &["clone", "--quiet", url, work2.to_str().unwrap()]);
    git(&work2, &["config", "user.email", "t@t"]);
    git(&work2, &["config", "user.name", "t"]);
    git(&work2, &["checkout", "--quiet", "--orphan", "rewrite"]);
    git(&work2, &["rm", "--quiet", "-rf", "."]);
    std::fs::write(work2.join("new.txt"), "new root").unwrap();
    git(&work2, &["add", "."]);
    git(&work2, &["commit", "--quiet", "-m", "new root"]);
    git(&work2, &["push", "--quiet", "--force", "origin", "rewrite:refs/heads/main"]);

    // 继续链式拉取：应在有限步内完成（旧浅边界被检测并重置，而非死锁）
    let mut guard = 0;
    loop {
        fetch_chain_step(&piece, "refs/heads/main", "main", 5, 0, false).unwrap();
        guard += 1;
        if !is_shallow(&piece) || guard > 10 {
            break;
        }
    }
    assert!(!is_shallow(&piece), "历史重写后链必须恢复并完成，不能楔死");
    let n = run_git(&["rev-list", "--count", "refs/heads/main"], Some(&piece)).unwrap();
    assert_eq!(n.stdout.trim(), "1", "新历史只有 1 个 commit");
}
