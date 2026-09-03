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

/// 对象侧负测试：一边有额外可达对象（多一个 commit）必须被检出
#[test]
fn extra_commit_breaks_object_equivalence() {
    let origin = build_origin(10, &[], &[]);
    let td = tempfile::tempdir().unwrap();
    let a = td.path().join("a");
    let b = td.path().join("b");
    git(td.path(), &["clone", "--quiet", origin.to_str().unwrap(), a.to_str().unwrap()]);
    git(td.path(), &["clone", "--quiet", origin.to_str().unwrap(), b.to_str().unwrap()]);
    // b 多一个本地 commit（不推送；本地 refs/heads 仍进 --all 的对象闭包）
    git(&b, &["config", "user.email", "t@t"]);
    git(&b, &["config", "user.name", "t"]);
    std::fs::write(b.join("extra.txt"), "x").unwrap();
    git(&b, &["add", "."]);
    git(&b, &["commit", "--quiet", "-m", "extra"]);
    let (sa, sb) = (snapshot(&a).unwrap(), snapshot(&b).unwrap());
    assert!(assert_same_refs(&sa, &sb).is_ok(), "ref 过滤后应相等（本地分支被排除）");
    assert!(assert_same_objects(&sa, &sb).is_err(), "对象集必须检出差异（--all 的刻意严格性）");
}

/// refs/remotes/origin/HEAD（默认分支指针，git < 2.48 的 fetch 不创建）必须被排除
#[test]
fn origin_head_symref_is_excluded() {
    let origin = build_origin(8, &[], &[]);
    let td = tempfile::tempdir().unwrap();
    let a = td.path().join("a");
    let b = td.path().join("b");
    git(td.path(), &["clone", "--quiet", origin.to_str().unwrap(), a.to_str().unwrap()]);
    git(td.path(), &["clone", "--quiet", origin.to_str().unwrap(), b.to_str().unwrap()]);
    // 模拟老 git：一侧没有 origin/HEAD（symbolic-ref 删除 symref 本身；
    // update-ref -d 会解引用误删 origin/main）
    git(&b, &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"]);
    let (sa, sb) = (snapshot(&a).unwrap(), snapshot(&b).unwrap());
    assert!(sa.refs.contains_key("refs/remotes/origin/HEAD"));
    assert!(!sb.refs.contains_key("refs/remotes/origin/HEAD"));
    assert_same_refs(&sa, &sb).unwrap();
}
