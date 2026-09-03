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
