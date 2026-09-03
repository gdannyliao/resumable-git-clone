use crate::gitio::run_git;
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug)]
pub struct RepoSnapshot {
    pub refs: BTreeMap<String, String>,
    pub objects: BTreeSet<String>,
}

pub fn snapshot(repo: &Path) -> Result<RepoSnapshot> {
    let out = run_git(&["for-each-ref", "--format=%(refname) %(objectname)"], Some(repo))?;
    let mut refs = BTreeMap::new();
    for line in out.stdout.lines() {
        if let Some((name, oid)) = line.split_once(' ') {
            refs.insert(name.to_string(), oid.to_string());
        }
    }
    let obj = run_git(&["rev-list", "--objects", "--all"], Some(repo))?;
    let objects: BTreeSet<String> = obj
        .stdout
        .lines()
        .map(|l| l.split(' ').next().unwrap().to_string())
        .collect();
    Ok(RepoSnapshot { refs, objects })
}

/// 产物等价性关心 origin 跟踪分支与 tags（本地分支/HEAD 是 finalize 的产物）
fn relevant_refs(s: &RepoSnapshot) -> BTreeMap<String, String> {
    s.refs
        .iter()
        .filter(|(k, _)| k.starts_with("refs/remotes/origin/") || k.starts_with("refs/tags/"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

pub fn assert_same_refs(a: &RepoSnapshot, b: &RepoSnapshot) -> Result<()> {
    let (ra, rb) = (relevant_refs(a), relevant_refs(b));
    if ra != rb {
        bail!("refs differ:\nonly A: {:?}\nonly B: {:?}", ra, rb);
    }
    Ok(())
}

pub fn assert_same_objects(a: &RepoSnapshot, b: &RepoSnapshot) -> Result<()> {
    if a.objects != b.objects {
        let only_a: Vec<_> = a.objects.difference(&b.objects).take(10).collect();
        let only_b: Vec<_> = b.objects.difference(&a.objects).take(10).collect();
        bail!(
            "objects differ (counts {} vs {})\nonly A: {:?}\nonly B: {:?}",
            a.objects.len(),
            b.objects.len(),
            only_a,
            only_b
        );
    }
    Ok(())
}
