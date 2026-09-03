use crate::gitio::run_git;
use crate::planner::{Piece, Plan};
use anyhow::{bail, Context, Result};
use std::path::Path;

pub fn finalize(plan: &Plan, main: &Path, keep_state: bool) -> Result<()> {
    // 0. remote 配置先行（catch-up fetch 依赖 refspec）
    run_git(&["config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"], Some(main))?;
    run_git(&["config", "remote.origin.url", &plan.url], Some(main))?;
    // 1. 收尾 fetch：捕获 ls-remote 之后的漂移 + 一切遗漏的兜底。
    // --force --prune-tags：被强移的 tag 更新到新 OID、被删的 tag 移除——与新鲜远端收敛
    //（非强制的 --tags 对 moved tag 会 "would clobber" 永久失败，对 deleted tag 静默保留旧值）。
    // 不加 --quiet：run_git 已捕获两路输出，失败时 git 的拒绝原因必须进错误消息。
    run_git(&["fetch", "--force", "--prune", "--prune-tags", "--tags", "origin"], Some(main))?;
    // 2. 校验（在 checkout 之前：失败时不留下看似可用的半成品布局）
    verify(plan, main)?;
    // 3. 默认分支 + checkout（与 git clone 布局对齐）
    let db = default_branch(plan);
    run_git(&["checkout", "-B", &db, &format!("refs/remotes/origin/{}", db)], Some(main))
        .with_context(|| format!("checkout {}", db))?;
    run_git(&["config", &format!("branch.{}.remote", db), "origin"], Some(main))?;
    run_git(&["config", &format!("branch.{}.merge", db), &format!("refs/heads/{}", db)], Some(main))?;
    // 4. 清理
    if !keep_state {
        if let Err(e) = std::fs::remove_dir_all(main.join(".rgc")) {
            eprintln!("warning: failed to remove .rgc/ after successful finalize: {}", e);
        }
    }
    Ok(())
}

fn default_branch(plan: &Plan) -> String {
    match &plan.default_branch {
        Some(d) => d.clone(),
        None => plan
            .pieces
            .iter()
            .find_map(|p| match p {
                Piece::Chain { short_name, .. } => Some(short_name.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "main".to_string()),
    }
}

/// refs 与服务端实时快照一致 + 全对象图连通
pub fn verify(plan: &Plan, main: &Path) -> Result<()> {
    let remote = crate::refs::ls_remote(&plan.url)?;
    let mut bad = Vec::new();
    for b in &remote.branches {
        let r = format!("refs/remotes/origin/{}", b.short_name);
        match run_git(&["rev-parse", "--verify", &r], Some(main)) {
            Ok(o) if o.stdout.trim() == b.oid => {}
            _ => bad.push(r),
        }
    }
    for t in &remote.tags {
        match run_git(&["rev-parse", "--verify", &t.full_name], Some(main)) {
            Ok(o) if o.stdout.trim() == t.oid => {}
            _ => bad.push(t.full_name.clone()),
        }
    }
    if !bad.is_empty() {
        // 诊断封顶（同 A.12 oracle 决策）：只报数量 + 前 10 条
        bad.sort();
        let shown: Vec<String> = bad.iter().take(10).cloned().collect();
        bail!(
            "verification failed, missing/mismatched refs ({} total, showing up to 10): {:?}",
            bad.len(),
            shown
        );
    }
    run_git(&["rev-list", "--objects", "--all", "--quiet"], Some(main))?;
    Ok(())
}
