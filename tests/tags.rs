mod common;
use common::*;
use rgc::gitio::*;

#[test]
fn tag_batch_transports() {
    let origin = build_origin(20, &[], &[("v1", 5), ("v2", 15)]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    init_main_repo(&main, url).unwrap();
    let piece = main.join(".rgc").join("pieces").join("tags_v1");
    ensure_piece_repo(&main, &piece, url).unwrap();
    let refs = rgc::refs::ls_remote(url).unwrap();
    fetch_tag_batch(&piece, &refs.tags).unwrap();
    transport_tags_to_main(&main, &piece, &refs.tags).unwrap();
    for t in &refs.tags {
        let got = run_git(&["rev-parse", &t.full_name], Some(&main)).unwrap();
        assert_eq!(got.stdout.trim(), t.oid);
    }
}

/// annotated tag：OID 是 tag 对象本身，必须原样落地（piece 与 main 一致）
#[test]
fn annotated_tag_lands_as_tag_object() {
    let origin = build_origin(10, &[], &[]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    // 用独立 work repo 打一个 annotated tag 并推送
    let work = td.path().join("work");
    git(td.path(), &["clone", "--quiet", url, work.to_str().unwrap()]);
    git(&work, &["config", "user.email", "t@t"]);
    git(&work, &["config", "user.name", "t"]);
    git(&work, &["tag", "-a", "vAnn", "-m", "release r1"]);
    git(&work, &["push", "--quiet", "origin", "vAnn"]);

    let main = td.path().join("repo");
    init_main_repo(&main, url).unwrap();
    let piece = main.join(".rgc").join("pieces").join("tags_ann");
    ensure_piece_repo(&main, &piece, url).unwrap();
    let refs = rgc::refs::ls_remote(url).unwrap();
    let ann: Vec<rgc::refs::RefEntry> = refs.tags.iter().filter(|t| t.short_name == "vAnn").cloned().collect();
    assert_eq!(ann.len(), 1);
    fetch_tag_batch(&piece, &ann).unwrap();
    transport_tags_to_main(&main, &piece, &ann).unwrap();
    // main 中 ref 指向 tag 对象，且类型是 tag
    let got = run_git(&["rev-parse", "refs/tags/vAnn"], Some(&main)).unwrap();
    assert_eq!(got.stdout.trim(), ann[0].oid);
    let ty = run_git(&["cat-file", "-t", &ann[0].oid], Some(&main)).unwrap();
    assert_eq!(ty.stdout.trim(), "tag");
}

/// 空批次必须是无操作（不能触发默认 refspec 全量拉取或 HEAD 报错）
#[test]
fn empty_batch_is_noop() {
    let origin = build_origin(5, &[], &[]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    init_main_repo(&main, url).unwrap();
    let piece = main.join(".rgc").join("pieces").join("tags_empty");
    ensure_piece_repo(&main, &piece, url).unwrap();
    fetch_tag_batch(&piece, &[]).unwrap();
    transport_tags_to_main(&main, &piece, &[]).unwrap();
    // piece 不得被静默拉满分支
    let branches = run_git(&["for-each-ref", "refs/remotes/origin"], Some(&piece)).unwrap();
    assert!(branches.stdout.trim().is_empty(), "空批次不得触发默认 refspec 拉取");
}
