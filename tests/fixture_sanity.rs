mod common;
use common::*;

#[test]
fn build_origin_produces_bare_repo_with_expected_refs() {
    let origin = build_origin(6, &[("dev", 3)], &[("v1", 2)]);
    let out = std::process::Command::new("git")
        .args(["-C", origin.to_str().unwrap(), "for-each-ref", "--format=%(refname)"])
        .output()
        .unwrap();
    let refs = String::from_utf8(out.stdout).unwrap();
    assert!(refs.contains("refs/heads/main"));
    assert!(refs.contains("refs/heads/dev"));
    assert!(refs.contains("refs/tags/v1"));
    assert_eq!(refs.lines().count(), 3);
}
