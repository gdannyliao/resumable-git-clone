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
    // 无人值守工具：禁止 git 打开 /dev/tty 索要凭据（会永久挂死）；固定 C locale（classify 依赖英文 stderr）。
    // 同时擦除继承的 GIT_* 定位变量：rgc 常从 git hook / wrapper 里被调用，
    // 调用方环境里的 GIT_DIR 等会把子 git 重定向到别的仓库（静默写错仓库，极难排查）。
    cmd.env("GIT_TERMINAL_PROMPT", "0").env("LC_ALL", "C")
        .env_remove("GIT_DIR").env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE").env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES");
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

/// 片仓库健康探针：git 可识别且 alternates 完好。
/// 主仓库已有 refs 时片仓库应为 --shared 克隆——alternates 缺失或指向不存在的
/// 对象库同样判损坏（rev-parse 不触碰对象库，单凭它探不出断掉的 alternates）。
/// 注意：片目录嵌在主仓库内，.git 损坏时 rev-parse 会向上发现主仓库而"成功"——
/// 必须校验 git-dir 确实是片自身的 .git。
fn piece_repo_healthy(main: &Path, piece: &Path) -> bool {
    let Ok(o) = run_git(&["rev-parse", "--absolute-git-dir"], Some(piece)) else {
        return false;
    };
    let own = piece.join(".git");
    match (std::fs::canonicalize(&own), std::fs::canonicalize(o.stdout.trim())) {
        (Ok(e), Ok(a)) if e == a => {}
        _ => return false,
    }
    let alt = piece.join(".git").join("objects").join("info").join("alternates");
    match std::fs::read_to_string(&alt) {
        Ok(content) => content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .all(|l| {
                let p = Path::new(l);
                let p = if p.is_absolute() { p.to_path_buf() } else { piece.join(".git").join("objects").join(p) };
                p.is_dir()
            }),
        Err(_) => !main_has_refs(main), // 主仓库有 refs 而片无 alternates = 非共享旧片/损坏 → 重建
    }
}

/// 确保片仓库存在且健康；损坏则重建。
/// 初始化：主仓库有 refs → `clone --shared`（对象经 alternates 共享）；
/// 否则普通 init。随后把主仓库 refs/remotes/origin/* 与 refs/tags/*
/// 复制为片仓库本地同名 refs —— have 协商依据本地 refs，仅 alternates 不够。
pub fn ensure_piece_repo(main: &Path, piece: &Path, url: &str) -> Result<()> {
    if piece.join(".git").exists() {
        if piece_repo_healthy(main, piece) {
            // 同下：HEAD 不得指在待 fetch 的分支上（幂等修复旧片）
            run_git(&["symbolic-ref", "HEAD", "refs/rgc/piece-head"], Some(piece))?;
            return Ok(());
        }
        std::fs::remove_dir_all(piece)?; // 损坏 → 重建
    }
    std::fs::create_dir_all(piece)?;
    let main_s = main.to_string_lossy().into_owned();
    let piece_s = piece.to_string_lossy().into_owned();
    if main_has_refs(main) {
        run_git(&["clone", "--shared", "--no-checkout", "--quiet", &main_s, &piece_s], None)?;
    } else {
        run_git(&["init", "--quiet", &piece_s], None)?;
    }
    if run_git(&["remote", "set-url", "origin", url], Some(piece)).is_err() {
        run_git(&["remote", "add", "origin", url], Some(piece))?;
    }
    // git ≥ 2.27 拒绝 fetch 进 HEAD 所指分支（即使 unborn）；片仓库无工作区用途，
    // 把 HEAD 指到永不使用的占位 ref，让链式 fetch 写 refs/heads/<short> 不受阻。
    run_git(&["symbolic-ref", "HEAD", "refs/rgc/piece-head"], Some(piece))?;
    if main_has_refs(main) {
        run_git(
            &["fetch", "--quiet", &main_s, "+refs/remotes/origin/*:refs/remotes/origin/*", "+refs/tags/*:refs/tags/*"],
            Some(piece),
        )?;
    }
    run_git(&["config", "gc.auto", "0"], Some(piece))?;
    Ok(())
}

/// shallow 边界是否仍在（文件缺失或空 = 历史已完整）
pub fn is_shallow(repo: &Path) -> bool {
    match std::fs::metadata(repo.join(".git").join("shallow")) {
        Ok(m) => m.len() > 0,
        Err(_) => false,
    }
}

/// 远端历史重写检测：所有浅边界必须是当前 tip 的祖先，否则 deepen 永远无法推进
///（force-push / 历史重写后旧浅边界悬空 → 死循环）
fn stale_shallow_boundary(piece: &Path, short: &str) -> bool {
    if !is_shallow(piece) {
        return false;
    }
    let local_ref = format!("refs/heads/{}", short);
    let content = match std::fs::read_to_string(piece.join(".git").join("shallow")) {
        Ok(c) => c,
        Err(_) => return false,
    };
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .any(|oid| {
            run_git(&["merge-base", "--is-ancestor", oid.trim(), &local_ref], Some(piece)).is_err()
        })
}

fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                total += dir_size(&e.path());
            } else {
                total += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// (对象库字节, 指定分支可达 commit 数) —— 用于步长自适应测量
pub fn repo_stats(repo: &Path, short: &str) -> Result<(u64, u32)> {
    let commits = match run_git(&["rev-list", "--count", &format!("refs/heads/{}", short)], Some(repo)) {
        Ok(o) => o.stdout.trim().parse().unwrap_or(0),
        Err(_) => 0,
    };
    Ok((dir_size(&repo.join(".git")), commits))
}

/// 链式片执行一步。三种模式：
/// - no_shallow：服务端不支持 shallow 时的退化，整支 fetch；
/// - 已有本地分支且仍 shallow：--deepen=<step>；
/// - 无本地分支：--depth=<depth_done + step>
///   （首片 depth_done=0 即 --depth=step；崩溃恢复重建的片 depth_done>0，
///   借 --shared 已有对象去重，网络代价 ≈ 增量）。
pub fn fetch_chain_step(piece: &Path, full_ref: &str, short: &str, step: u32, depth_done: u32, no_shallow: bool) -> Result<()> {
    let refspec = format!("+{}:refs/heads/{}", full_ref, short);
    if no_shallow {
        return run_git(&["fetch", "--quiet", "origin", &refspec], Some(piece)).map(|_| ());
    }
    let has_local = run_git(&["rev-parse", "--verify", &format!("refs/heads/{}", short)], Some(piece)).is_ok();
    // B2: 远端历史重写检测——旧浅边界悬空（不在当前 ref 历史中）→ deepen 永远无法推进 → 重置后 --depth 重取
    let has_local = if stale_shallow_boundary(piece, short) {
        let _ = run_git(&["update-ref", "-d", &format!("refs/heads/{}", short)], Some(piece));
        let _ = std::fs::remove_file(piece.join(".git").join("shallow"));
        false
    } else {
        has_local
    };
    if has_local && is_shallow(piece) {
        let d = format!("--deepen={}", step);
        run_git(&["fetch", "--quiet", &d, "origin", &refspec], Some(piece))?;
    } else if !has_local {
        let d = format!("--depth={}", depth_done + step);
        run_git(&["fetch", "--quiet", &d, "origin", &refspec], Some(piece))?;
    }
    // has_local && !shallow → 已完整，无需操作
    Ok(())
}

/// 搬运片仓库成果进主仓库。final_dst=false 写 refs/rgc/wip/<short>（链进行中，
/// 防止中途 tip 被其他片当 have 导致"撒谎的 have"丢历史）；final_dst=true 写
/// refs/remotes/origin/<short> 并删除 wip 引用。
pub fn transport_to_main(main: &Path, piece: &Path, full_ref: &str, short: &str, final_dst: bool) -> Result<()> {
    let dst = if final_dst { format!("refs/remotes/origin/{}", short) } else { format!("refs/rgc/wip/{}", short) };
    let refspec = format!("+{}:{}", full_ref, dst);
    run_git(&["fetch", "--quiet", piece.to_string_lossy().as_ref(), &refspec], Some(main))?;
    // B1: 从浅仓搬运会被 git 静默拒绝（shallow roots 禁更新 → warning + exit 0 静默），必须显式断言目标 ref 已落地（审查 B1）
    run_git(&["rev-parse", "--verify", &dst], Some(main))
        .map_err(|_| anyhow::anyhow!("transport_to_main: ref {} not created (piece 可能仍为浅仓, 或搬运被拒 — shallow roots 禁更新)", dst))?;
    if final_dst {
        let _ = run_git(&["update-ref", "-d", &format!("refs/rgc/wip/{}", short)], Some(main));
    }
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
