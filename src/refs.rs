use crate::gitio::run_git;
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefEntry {
    pub full_name: String,
    pub short_name: String,
    pub oid: String,
}

#[derive(Debug, Clone)]
pub struct RemoteRefs {
    pub default_branch: Option<String>,
    pub branches: Vec<RefEntry>,
    pub tags: Vec<RefEntry>,
}

pub fn parse_ls_remote(output: &str) -> RemoteRefs {
    let mut default_branch = None;
    let mut branches = Vec::new();
    let mut tags = Vec::new();
    for line in output.lines() {
        // --symref 首行形如 "ref: refs/heads/main\tHEAD"
        if let Some(rest) = line.strip_prefix("ref: ") {
            if let Some(name) = rest.split('\t').next() {
                default_branch = Some(name.trim().trim_start_matches("refs/heads/").to_string());
            }
            continue;
        }
        let Some((oid, refname)) = line.split_once('\t') else { continue };
        let refname = refname.trim();
        if refname.ends_with("^{}") {
            continue; // annotated tag 的 peeled 行：抓 tag 本身即可
        }
        match refname {
            "HEAD" => {}
            n if n.starts_with("refs/heads/") => branches.push(RefEntry {
                full_name: n.to_string(),
                short_name: n.trim_start_matches("refs/heads/").to_string(),
                oid: oid.trim().to_string(),
            }),
            n if n.starts_with("refs/tags/") => tags.push(RefEntry {
                full_name: n.to_string(),
                short_name: n.trim_start_matches("refs/tags/").to_string(),
                oid: oid.trim().to_string(),
            }),
            _ => {} // refs/pull/* 等一律不拉
        }
    }
    RemoteRefs { default_branch, branches, tags }
}

pub fn ls_remote(url: &str) -> Result<RemoteRefs> {
    let out = run_git(&["ls-remote", "--symref", url], None)?;
    Ok(parse_ls_remote(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "ref: refs/heads/main\tHEAD\nabc111\tHEAD\nabc111\trefs/heads/main\ndef222\trefs/heads/dev\n789aaa\trefs/tags/v1\nbbb222\trefs/tags/v1^{}\nccc333\trefs/pull/1/head\n";

    #[test]
    fn parse_ls_remote_basic() {
        let r = parse_ls_remote(SAMPLE);
        assert_eq!(r.default_branch.as_deref(), Some("main"));
        assert_eq!(r.branches.len(), 2);
        assert_eq!(r.branches[1].short_name, "dev");
        assert_eq!(r.tags.len(), 1);
        assert_eq!(r.tags[0].oid, "789aaa");
    }
}
