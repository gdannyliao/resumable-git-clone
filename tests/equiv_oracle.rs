mod common;
use common::*;
use rgc::equiv::{assert_same_objects, assert_same_refs, snapshot};

#[test]
fn snapshots_of_two_clones_are_equivalent() {
    let origin = build_origin(25, &[("dev", 8)], &[("v1", 12)]);
    let td = tempfile::tempdir().unwrap();
    let a = td.path().join("a");
    let b = td.path().join("b");
    git(td.path(), &["clone", "--quiet", origin.to_str().unwrap(), a.to_str().unwrap()]);
    git(td.path(), &["clone", "--quiet", origin.to_str().unwrap(), b.to_str().unwrap()]);
    let (sa, sb) = (snapshot(&a).unwrap(), snapshot(&b).unwrap());
    assert_same_refs(&sa, &sb).unwrap();
    assert_same_objects(&sa, &sb).unwrap();
}

#[test]
fn missing_tag_breaks_ref_equivalence() {
    let origin = build_origin(10, &[], &[("v1", 5)]);
    let td = tempfile::tempdir().unwrap();
    let a = td.path().join("a");
    let b = td.path().join("b");
    git(td.path(), &["clone", "--quiet", origin.to_str().unwrap(), a.to_str().unwrap()]);
    git(td.path(), &["clone", "--quiet", origin.to_str().unwrap(), b.to_str().unwrap()]);
    git(&b, &["tag", "-d", "v1"]);
    let (sa, sb) = (snapshot(&a).unwrap(), snapshot(&b).unwrap());
    assert!(assert_same_refs(&sa, &sb).is_err());
}
