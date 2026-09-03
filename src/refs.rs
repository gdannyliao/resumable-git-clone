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

/// 剥离单一命名空间前缀。不用 trim_start_matches：避免 "refs/heads/refs/heads/x"
/// 这类字面名字被连环剥皮。
fn short_name(full: &str) -> String {
    match full.strip_prefix("refs/heads/") {
        Some(s) => s.to_string(),
        None => full.strip_prefix("refs/tags/").unwrap_or(full).to_string(),
    }
}

pub fn parse_ls_remote(output: &str) -> RemoteRefs {
    let mut default_branch = None;
    let mut branches = Vec::new();
    let mut tags = Vec::new();
    for line in output.lines() {
        // --symref 对每个符号引用输出一行 "ref: <target>\t<refname>"。
        // 只有 refname == HEAD 的那一行定义默认分支（远端可能广播其他 symref）；
        // 且 target 必须是分支，否则留给 finalize 的 fallback 逻辑。
        if let Some(rest) = line.strip_prefix("ref: ") {
            if let Some((target, refname)) = rest.split_once('\t') {
                if refname.trim() == "HEAD" {
                    if let Some(name) = target.trim().strip_prefix("refs/heads/") {
                        default_branch = Some(name.to_string());
                    }
                }
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
                short_name: short_name(n),
                oid: oid.trim().to_string(),
            }),
            n if n.starts_with("refs/tags/") => tags.push(RefEntry {
                full_name: n.to_string(),
                short_name: short_name(n),
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

    #[test]
    fn second_symref_does_not_override_head() {
        // 真实形状：HEAD 的 symref 行之后，还有其他符号引用的行
        let out = "ref: refs/heads/main\tHEAD\n7b8e\tHEAD\nref: refs/heads/b2\trefs/heads/alias\n7b8e\trefs/heads/alias\n7b8e\trefs/heads/main\n";
        let r = parse_ls_remote(out);
        assert_eq!(r.default_branch.as_deref(), Some("main"), "HEAD 的 symref 必须获胜");
        assert_eq!(r.branches.len(), 2);
    }

    #[test]
    fn head_symref_to_non_branch_leaves_default_none() {
        let out = "ref: refs/tags/v1\tHEAD\nabc\tHEAD\nabc\trefs/heads/main\n";
        let r = parse_ls_remote(out);
        assert_eq!(r.default_branch, None);
        assert_eq!(r.branches.len(), 1);
    }

    #[test]
    fn repeated_prefix_names_kept_literal() {
        let out = "abc\trefs/heads/refs/heads/x\ndef\trefs/tags/refs/tags/y\n";
        let r = parse_ls_remote(out);
        assert_eq!(r.branches[0].short_name, "refs/heads/x");
        assert_eq!(r.tags[0].short_name, "refs/tags/y");
    }

    #[test]
    fn branch_names_with_spaces_parse() {
        let out = "abc\trefs/heads/feature with space\n";
        let r = parse_ls_remote(out);
        assert_eq!(r.branches[0].short_name, "feature with space");
    }

    #[test]
    fn crlf_output_parses() {
        let out = "ref: refs/heads/main\tHEAD\r\nabc111\trefs/heads/main\r\n";
        let r = parse_ls_remote(out);
        assert_eq!(r.default_branch.as_deref(), Some("main"));
        assert_eq!(r.branches.len(), 1);
        assert_eq!(r.branches[0].oid, "abc111");
    }
}
