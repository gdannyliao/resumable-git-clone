use crate::gitio::run_git;
use crate::planner::{Piece, Plan};
use anyhow::{bail, Context, Result};
use std::path::Path;

pub fn finalize(plan: &Plan, main: &Path, keep_state: bool) -> Result<()> {
    // 0. remote 配置先行（catch-up fetch 依赖 refspec）
    run_git(&["config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"], Some(main))?;
    run_git(&["config", "remote.origin.url", &plan.url], Some(main))?;
    // 1. 收尾 fetch：捕获 ls-remote 之后的漂移 + 一切遗漏的兜底
    run_git(&["fetch", "--quiet", "--prune", "--tags", "origin"], Some(main))?;
    // 2. 默认分支 + checkout（与 git clone 布局对齐）
    let db = default_branch(plan);
    run_git(&["checkout", "-B", &db, &format!("refs/remotes/origin/{}", db)], Some(main))
        .with_context(|| format!("checkout {}", db))?;
    run_git(&["config", &format!("branch.{}.remote", db), "origin"], Some(main))?;
    run_git(&["config", &format!("branch.{}.merge", db), &format!("refs/heads/{}", db)], Some(main))?;
    // 3. 校验
    verify(plan, main)?;
    // 4. 清理
    if !keep_state {
        let _ = std::fs::remove_dir_all(main.join(".rgc"));
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
        bail!("verification failed, missing/mismatched refs: {:?}", bad);
    }
    run_git(&["rev-list", "--objects", "--all", "--quiet"], Some(main))?;
    Ok(())
}
