use crate::errors::{classify, RgcError};
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

#[derive(Debug)]
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
    // 无人值守工具：禁止 git 打开 /dev/tty 索要凭据（会永久挂死）；固定 C locale（classify 依赖英文 stderr）
    cmd.env("GIT_TERMINAL_PROMPT", "0").env("LC_ALL", "C");
    let out = cmd.output().with_context(|| {
        let shown: Vec<String> = args.iter().map(|a| redact(a)).collect();
        format!("failed to spawn git {:?}", shown)
    })?;
    let res = GitOutput {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    };
    if !out.status.success() {
        let shown: Vec<String> = args.iter().map(|a| redact(a)).collect();
        let msg = format!("git {:?} failed (exit {:?}): {}", shown, out.status.code(), res.stderr.trim_end());
        return Err(RgcError::from_kind(classify(&res.stderr), msg).into());
    }
    Ok(res)
}

/// 错误消息专用：剥离 URL 中的 userinfo（git 自身会对 URL 脱敏，args 原样会泄漏）
fn redact(arg: &str) -> String {
    // 匹配 scheme://user:pass@host → scheme://host
    if let Some(scheme_end) = arg.find("://") {
        let (scheme, rest) = arg.split_at(scheme_end + 3);
        if let Some(at) = rest.find('@') {
            let host = &rest[at + 1..];
            if !host.is_empty() {
                return format!("{}{}", scheme, host);
            }
        }
    }
    arg.to_string()
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
        run_git(&["init", "--quiet"], Some(main))?;
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

    #[test]
    fn run_git_classifies_failures() {
        let td = tempfile::tempdir().unwrap();
        // 非 git 目录 → Fatal（不匹配任何网络关键词）
        let err = run_git(&["for-each-ref"], Some(td.path())).unwrap_err();
        assert_eq!(crate::errors::kind_of(&err), crate::errors::FailureKind::Fatal);
        // 连接拒绝（本地回环立即失败，无真实网络）→ Network
        let err = run_git(&["ls-remote", "https://127.0.0.1:1/x.git"], None).unwrap_err();
        assert_eq!(crate::errors::kind_of(&err), crate::errors::FailureKind::Network);
    }

    #[test]
    fn run_git_redacts_credentials_in_message() {
        // git 自身会对 URL 脱敏，但我们的消息模板携带原始 args——必须再脱敏
        let err = run_git(&["ls-remote", "https://alice:supersecret-token@127.0.0.1:1/repo.git"], None).unwrap_err();
        assert!(!err.to_string().contains("supersecret-token"), "message leaked credentials: {}", err);
        assert!(err.to_string().contains("127.0.0.1:1/repo.git"));
    }

    #[test]
    fn redact_strips_userinfo() {
        assert_eq!(redact("https://user:pass@host.com/x"), "https://host.com/x");
        assert_eq!(redact("https://host.com/x"), "https://host.com/x");
        assert_eq!(redact("/plain/path"), "/plain/path");
        assert_eq!(redact("no-scheme-but@has-at"), "no-scheme-but@has-at");
    }
}
