mod common;
use common::*;

#[test]
fn ls_remote_reads_fixture() {
    let origin = build_origin(5, &[("dev", 3)], &[("v1", 2)]);
    let r = rgc::refs::ls_remote(origin.to_str().unwrap()).unwrap();
    assert_eq!(r.default_branch.as_deref(), Some("main"));
    assert_eq!(r.branches.len(), 2);
    assert_eq!(r.tags.len(), 1);
}
