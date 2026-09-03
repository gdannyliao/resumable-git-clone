use crate::errors::{classify, RgcError};
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

pub struct GitOutput {
    pub stdout: String,
    pub stderr: String,
}

/// 运行 git 子进程；非零退出按 stderr 分类映射为 RgcError。
pub fn run_git(args: &[&str], cwd: Option<&Path>) -> Result<GitOutput> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd.output().with_context(|| format!("failed to spawn git {:?}", args))?;
    let res = GitOutput {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    };
    if !out.status.success() {
        let msg = format!("git {:?} failed: {}", args, res.stderr.trim_end());
        return Err(RgcError::from_kind(classify(&res.stderr), msg).into());
    }
    Ok(res)
}

/// 主仓库是否已有任何 refs（决定片仓库能否 --shared 起步）
pub fn main_has_refs(main: &Path) -> bool {
    match run_git(&["for-each-ref", "--count=1", "refs/remotes/origin", "refs/tags", "refs/heads"], Some(main)) {
        Ok(o) => !o.stdout.trim().is_empty(),
        Err(_) => false,
    }
}

pub fn init_main_repo(main: &Path, url: &str) -> Result<()> {
    std::fs::create_dir_all(main)?;
    if !main.join(".git").exists() {
        run_git(&["init", "--quiet", main.to_str().unwrap()], None)?;
    }
    if run_git(&["remote", "get-url", "origin"], Some(main)).is_err() {
        run_git(&["remote", "add", "origin", url], Some(main))?;
    }
    // clone 期间禁用主仓库 auto-gc：片仓库经 alternates 借用的对象不可被回收
    run_git(&["config", "gc.auto", "0"], Some(main))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_main_repo_idempotent() {
        let td = tempfile::tempdir().unwrap();
        let main = td.path().join("r");
        init_main_repo(&main, "https://example.com/x.git").unwrap();
        init_main_repo(&main, "https://example.com/x.git").unwrap();
        assert!(main.join(".git").exists());
        let out = run_git(&["remote", "get-url", "origin"], Some(&main)).unwrap();
        assert_eq!(out.stdout.trim(), "https://example.com/x.git");
        assert!(!main_has_refs(&main));
    }
}
