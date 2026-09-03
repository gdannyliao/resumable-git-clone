use crate::gitio::run_git;
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug)]
pub struct RepoSnapshot {
    pub refs: BTreeMap<String, String>,
    pub objects: BTreeSet<String>,
}

/// snapshot(repo) 语义：ref 名→OID 全集（比较时再过滤）+ 可达对象 OID 集合（--all）。
/// 刻意的严格性：对象集用 --all（含 refs/heads 等全部 ref 的可达闭包），比 ref 过滤范围更宽——
/// 只可能"多报差异"，绝不假通过。fixture/中等仓库规模设计（BTreeSet 全量驻内存），
/// chromium 级（~15M 对象）不适用：内存 ~GB/侧、耗时分钟级。
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
        .filter(|l| !l.is_empty())
        .map(|l| l.split(' ').next().unwrap().to_string())
        .collect();
    Ok(RepoSnapshot { refs, objects })
}

/// 产物等价性关心 origin 跟踪分支与 tags。本地分支/HEAD 是 finalize 的产物；
/// refs/remotes/origin/HEAD 是默认分支指针（与 HEAD 同类），且 git < 2.48 的
/// fetch 不创建它——必须排除，否则 oracle 在老 git 上对完全等价的产物假失败。
fn relevant_refs(s: &RepoSnapshot) -> BTreeMap<String, String> {
    s.refs
        .iter()
        .filter(|(k, _)| {
            (k.starts_with("refs/remotes/origin/") || k.starts_with("refs/tags/"))
                && k.as_str() != "refs/remotes/origin/HEAD"
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

pub fn assert_same_refs(a: &RepoSnapshot, b: &RepoSnapshot) -> Result<()> {
    let (ra, rb) = (relevant_refs(a), relevant_refs(b));
    if ra != rb {
        let only_a: Vec<_> = ra.iter().filter(|(k, _)| !rb.contains_key(*k)).take(10).collect();
        let only_b: Vec<_> = rb.iter().filter(|(k, _)| !ra.contains_key(*k)).take(10).collect();
        let changed: Vec<_> = ra
            .iter()
            .filter(|(k, v)| rb.get(*k).is_some_and(|vb| vb != *v))
            .take(10)
            .collect();
        bail!(
            "refs differ (counts {} vs {})\nonly A: {:?}\nonly B: {:?}\nchanged: {:?}",
            ra.len(),
            rb.len(),
            only_a,
            only_b,
            changed
        );
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
