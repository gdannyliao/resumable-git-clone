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
