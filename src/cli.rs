//! Task 15：CLI 流程层 —— clone/resume/status 的前端编排，以库 API 暴露
//! （main.rs 只做 clap 解析；tests/cli.rs 直接调用本模块，不 spawn 二进制）。
//!
//! 契约（附录 A.2/A.4/A.8）：
//! - fs2 实例锁先于 load_plan/reconcile 取得（reconcile 有破坏性 wipe）；
//!   锁文件 `<dest>/.rgc/lock`，try_lock_exclusive 冲突 → bail，进程退出释放。
//! - plan/state 指纹不匹配 → `plan/state mismatch` 大声报错，绝不静默重规划；
//!   `.rgc` 有规划痕迹但 plan.json 缺失/损坏 → 同样 bail（损坏经 load_json 视同缺失，
//!   以文件存在性甄别）。
//! - status 严格只读：不加锁、绝不 reconcile；state 缺失/损坏 → 提示 resume 重建。
//!
//! 与计划正文的偏差：正文 clone_existing 自行 load/reconcile，现 API（A.16c）由
//! `scheduler::run` 内部持有 `Arc<Mutex<State>>` 完成 load/指纹守卫/reconcile，
//! 流程层只调 run()；正文的 indicatif 监视线程因此无法窥视状态，进度以调度器
//! 既有的 eprintln 呈现（派发说明允许"keep it simple"）。

use crate::finalizer;
use crate::planner::{build_plan, load_plan, plan_path, save_plan, PlannerConfig, Plan, INITIAL_STEP};
use crate::refs::ls_remote;
use crate::scheduler::{self, SchedulerConfig};
use crate::state::{PieceStatus, State};
use anyhow::{anyhow, bail, Context, Result};
use fs2::FileExt;
use std::fs::File;
use std::path::{Path, PathBuf};

/// clone/resume 共享旋钮（与 CLI flag 一一对应）。
#[derive(Debug, Clone)]
pub struct CloneOptions {
    /// 并行 worker 数（scheduler::run 钳制 1..=MAX_JOBS）
    pub jobs: usize,
    /// 单片目标时长（秒）
    pub piece_target: f64,
    /// finalize 后保留 .rgc/（false = 成功即清理）
    pub keep_state: bool,
    /// 初始 deepen 步长（测试旋钮）
    pub initial_step: u32,
}

impl Default for CloneOptions {
    fn default() -> Self {
        Self { jobs: 2, piece_target: 600.0, keep_state: false, initial_step: INITIAL_STEP }
    }
}

// —— 实例锁（A.4 / A.8）——

/// 实例锁 guard：持有锁文件 fd，drop 即释放（含进程退出）。
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // 显式 LOCK_UN 而非依赖 close()：macOS 上 close 释放 flock 存在毫秒级
        // 异步窗口（压测实证：紧随 drop 的重取会短暂 EWOULDBLOCK，1ms 后即恢复），
        // 显式解锁同步生效，保证"guard 释放后立即可重入"。
        let _ = self._file.unlock();
    }
}

pub fn lock_path(dir: &Path) -> PathBuf {
    dir.join(".rgc").join("lock")
}

/// 取得 dest 上的排他实例锁。必须在 load_plan/reconcile 之前调用。
/// 冲突（EWOULDBLOCK/EAGAIN）→ bail（不等待）：多进程同抢一个 dest 只有一个能走。
/// 其他 I/O 错误 → 原样上抛（绝不把未知故障伪装成"已有实例在跑"）。
pub fn acquire_instance_lock(dir: &Path) -> Result<InstanceLock> {
    std::fs::create_dir_all(dir.join(".rgc"))?;
    let path = lock_path(dir);
    let file = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("cannot open lock file {}", path.display()))?;
    if let Err(e) = file.try_lock_exclusive() {
        match e.kind() {
            std::io::ErrorKind::WouldBlock => {
                bail!("another rgc instance is running on this destination (lock: {})", path.display())
            }
            _ => return Err(e).with_context(|| format!("cannot lock {}", path.display())),
        }
    }
    Ok(InstanceLock { _file: file })
}

// —— 流程入口 ——

/// `rgc clone <url> [dir]`：dest 已有本远端的 plan → resume（幂等重跑）；
/// 全新 dest → ls_remote → 规划 → 调度 → finalize。
pub fn clone_flow(url: &str, dest: &Path, opts: &CloneOptions) -> Result<()> {
    // 相对 dest 一律先绝对化：调度器以 dest 为 cwd 派生 git 命令（cwd/路径参数
    // 交错），相对路径会被 git 按错误基准解析（实测 transport 永远失败）。
    let dest = crate::gitio::absolute_path(dest);
    let dest = dest.as_path();
    // 本地路径形式的 url 同样绝对化（cwd=piece 的 fetch 按存好的 url 找远端）；
    // 网络式 URL（https:/ssh:…）`exists()` 恒 false，原样透传。
    let url_owned = if Path::new(url).exists() {
        crate::gitio::absolute_path(Path::new(url)).to_string_lossy().into_owned()
    } else {
        url.to_string()
    };
    let url = url_owned.as_str();
    // A.8：实例锁必须先于 load_plan/reconcile
    let _lock = acquire_instance_lock(&dest)?;
    match load_plan(dest)? {
        Some(plan) => {
            if plan.url != url {
                bail!(
                    "destination {} already holds an rgc plan for a different URL ({}) — delete .rgc/ or rerun with the original URL",
                    dest.display(),
                    plan.url
                );
            }
            eprintln!("rgc: found existing plan — resuming {}", dest.display());
            run_and_finalize(&plan, dest, opts)
        }
        None => {
            // A.2：.rgc 有规划痕迹（plan.json 在但损坏 / 孤儿 state.json）→ 大声报错，
            // 绝不静默重规划（重规划会覆盖 plan.json 并可能孤儿化现场）。
            let plan_file = plan_path(dest);
            let state_file = dest.join(".rgc").join("state.json");
            if plan_file.exists() || state_file.exists() {
                bail!(
                    "plan/state mismatch — delete .rgc/ or restore plan.json (plan.json missing or unreadable in {})",
                    dest.display()
                );
            }
            eprintln!("rgc: planning {} → {}", url, dest.display());
            ensure_dest_vacant_for_fresh(dest)?;
            let remote = ls_remote(url)?;
            let plan = build_plan(url, &remote, &PlannerConfig { initial_step: opts.initial_step, ..Default::default() })?;
            save_plan(dest, &plan)?;
            State::new(&plan).save(dest)?;
            run_and_finalize(&plan, dest, opts)
        }
    }
}

/// 全新 dest 的预检：已存在的 dest 只允许含 `.rgc` 骨架（实例锁所在，孤儿
/// plan/state 已在上一步拦截）。真实 `git clone` 拒绝非空 dest —— rgc 同样拒绝，
/// 否则 finalize 的 `checkout -B` 会孤儿化既有仓库的本地提交、静默改写 origin
/// url，或下载数小时后才在非空目录的 checkout 冲突上失败。
fn ensure_dest_vacant_for_fresh(dest: &Path) -> Result<()> {
    if !dest.exists() {
        return Ok(());
    }
    let foreign: Vec<_> = std::fs::read_dir(dest)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .filter(|n| n != ".rgc")
        .collect();
    if foreign.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = foreign.iter().map(|n| n.to_string_lossy().into_owned()).collect();
    bail!(
        "destination {} is not empty (contains {}) — rgc only clones into an empty directory; choose a new destination or delete its contents",
        dest.display(),
        names.join(", ")
    );
}

/// `rgc resume <dir>`：显式恢复；无 plan 即报错。
pub fn resume_flow(dest: &Path, opts: &CloneOptions) -> Result<()> {
    let dest = crate::gitio::absolute_path(dest);
    let dest = dest.as_path();
    let _lock = acquire_instance_lock(&dest)?;
    let plan = load_plan(dest)?.ok_or_else(|| anyhow!("no .rgc/plan.json in {} — nothing to resume", dest.display()))?;
    eprintln!("rgc: resuming {}", dest.display());
    run_and_finalize(&plan, dest, opts)
}

/// 调度 + 收尾（clone/resume 共用尾部）。
fn run_and_finalize(plan: &Plan, dest: &Path, opts: &CloneOptions) -> Result<()> {
    // A.2（Task 15 份内的指纹守卫）：先于任何 git 变更大声报错
    //（scheduler::run 内部还有一道，此处提前到 init_main_repo 之前）。
    if let Some(st) = State::load(dest)? {
        if st.fingerprint != plan.fingerprint() {
            bail!("plan/state mismatch — delete .rgc/ or restore plan.json");
        }
    }
    let cfg = SchedulerConfig { jobs: opts.jobs, target_secs: opts.piece_target, ..Default::default() };
    eprintln!("rgc: fetching {} pieces with {} jobs (piece target {}s)", plan.pieces.len(), cfg.clamped_jobs(), opts.piece_target);
    scheduler::run(plan, dest, &cfg)?;
    finalizer::finalize(plan, dest, opts.keep_state)?;
    println!("rgc: done → {}", dest.display());
    Ok(())
}

/// `rgc status <dir>`：渲染各片进度。A.8：严格只读 —— 不加锁、绝不 reconcile；
/// state 缺失/损坏 → 提示 resume 重建（scheduler::run 会在下次 resume 时对账）。
pub fn status(dir: &Path) -> Result<()> {
    let plan = load_plan(dir)?.ok_or_else(|| anyhow!("no readable .rgc/plan.json in {} — nothing to report", dir.display()))?;
    println!("plan: {} ({} pieces)", plan.url, plan.pieces.len());
    match State::load(dir)? {
        Some(state) => {
            if state.fingerprint != plan.fingerprint() {
                eprintln!("warning: state.json does not match plan.json in {} — display may be misleading", dir.display());
            }
            for ps in &state.pieces {
                let extra = match &ps.chain {
                    Some(c) => format!("  depth={} step={}", c.depth_done, c.step),
                    None => String::new(),
                };
                println!("{:?}  {}{}", ps.status, ps.id, extra);
            }
            let done = state.pieces.iter().filter(|p| p.status == PieceStatus::Done).count();
            println!("{}/{} pieces done", done, state.pieces.len());
        }
        None => {
            println!(
                "rgc: no readable state.json in {} — run `rgc resume {}` to rebuild progress",
                dir.display(),
                dir.display()
            );
        }
    }
    Ok(())
}

/// `rgc clone <url>` 缺省 dest：URL 末段去 .git。
pub fn default_dir(url: &str) -> PathBuf {
    let base = url.trim_end_matches('/').rsplit('/').next().unwrap_or("repo");
    PathBuf::from(base.trim_end_matches(".git"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_dir_variants() {
        assert_eq!(default_dir("https://example.com/foo/bar.git"), PathBuf::from("bar"));
        assert_eq!(default_dir("https://example.com/foo/bar"), PathBuf::from("bar"));
        assert_eq!(default_dir("https://example.com/foo/bar/"), PathBuf::from("bar"));
        assert_eq!(default_dir("/tmp/x.git"), PathBuf::from("x"));
    }

    #[test]
    fn lock_is_exclusive_then_releasable() {
        let td = tempfile::tempdir().unwrap();
        let dest = td.path().join("r");
        let _g1 = acquire_instance_lock(&dest).unwrap();
        let err = acquire_instance_lock(&dest).unwrap_err();
        assert!(format!("{:#}", err).contains("another rgc instance"));
        drop(_g1);
        let _g2 = acquire_instance_lock(&dest).unwrap();
    }

    /// 压力回归：acquire → 探针冲突 → drop → 重取，循环 2000 次。
    /// 守 macOS 内核怪癖：close() 释放 flock 有毫秒级异步窗口（压测实证，
    /// 紧随 drop 的重取可能短暂 EWOULDBLOCK）——Drop 里的显式 LOCK_UN 治它。
    /// 若回归，失败后立即重试 200×1ms 并回报恢复位置：瞬态还是永久。
    #[test]
    fn lock_release_survives_hammering() {
        use std::time::Duration;
        let td = tempfile::tempdir().unwrap();
        let dest = td.path().join("hammer");
        for i in 0..2000 {
            let g = acquire_instance_lock(&dest).unwrap();
            assert!(
                acquire_instance_lock(&dest).is_err(),
                "iteration {i}: probe must conflict while guard holds"
            );
            drop(g);
            match acquire_instance_lock(&dest) {
                Ok(_) => {}
                Err(e) => {
                    let mut recovered_at = None;
                    for r in 0..200 {
                        std::thread::sleep(Duration::from_millis(1));
                        if acquire_instance_lock(&dest).is_ok() {
                            recovered_at = Some(r);
                            break;
                        }
                    }
                    panic!(
                        "iteration {i}: re-acquire after drop failed: {e:#}; recovered_at_retry={recovered_at:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn clone_options_defaults_match_cli_defaults() {
        let o = CloneOptions::default();
        assert_eq!(o.jobs, 2);
        assert_eq!(o.piece_target, 600.0);
        assert!(!o.keep_state);
        assert_eq!(o.initial_step, INITIAL_STEP);
    }
}
