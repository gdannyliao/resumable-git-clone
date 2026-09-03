//! Task 12：串行调度核心 —— write-ahead 状态机驱动的片执行引擎。
//!
//! write-ahead 不变式：片开始执行前，状态先落盘（Pending→Running，崩溃后
//! `State::load` 把 Running 折返 Pending）；执行结束后再落盘（Done / attempts++）。
//! 片仓库的实际 fetch 进度（refs/heads/*、shallow 文件）自描述，重跑零浪费。
//!
//! 附录 A.6：git 拒绝从浅仓搬运（shallow roots 禁更新，exit 0 静默）——链式片
//! 只在完成后（`!is_shallow`）做一次 final 搬运，中途不做 wip 搬运
//! （plan 正文"每步立即搬运"的注释作废）。B1 断言在 transport_to_main 内（A.9）。

use crate::errors::{kind_of, FailureKind, RgcError};
use crate::gitio;
use crate::planner::{halve_step, next_step, Piece, Plan, StepMeasurement};
use crate::state::{pieces_dir, ChainState, PieceState, PieceStatus, State};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub max_attempts: u32,
    pub target_secs: f64,
    /// 全局冷却时刻：限流/拥塞后，后续片一律推迟到此之后（spec §4.4）
    pub cooldown: Arc<Mutex<Instant>>,
}
impl Default for SchedulerConfig {
    fn default() -> Self {
        Self { max_attempts: 4, target_secs: 600.0, cooldown: Arc::new(Mutex::new(Instant::now())) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    RetryAfter(u64),
    HalveAndRetry,
    RetryNoShallow,
    GiveUp,
}

/// 网络退避：2s 起倍增，封顶 60s。
/// 注意：plan 正文的 `2u64.saturating_pow(attempts)` 有 off-by-one（2^0=1），
/// 与正文自带的测试（backoff(0)==2, 4, 8…）及本注释矛盾，按测试修正为 2^(attempts+1)。
pub fn backoff_secs(attempts: u32) -> u64 {
    2u64.saturating_pow(attempts.saturating_add(1)).min(60)
}

/// 重试决策：Fatal→放弃；Shallow 不支持→退化整支 fetch；
/// 限流→长退避（长退避本身即全局减速的片内分量）；拥塞(reset)→退避+全局冷却；
/// 网络失败第 2 次起步长减半。
pub fn decide(attempts: u32, kind: FailureKind, max_attempts: u32) -> Action {
    if attempts > max_attempts {
        return Action::GiveUp;
    }
    match kind {
        FailureKind::ShallowUnsupported => Action::RetryNoShallow,
        FailureKind::Fatal => Action::GiveUp,
        FailureKind::RateLimited => Action::RetryAfter(60u64 << attempts.min(2)),
        FailureKind::Congestion => Action::RetryAfter(backoff_secs(attempts)),
        FailureKind::Network if attempts >= 2 => Action::HalveAndRetry,
        FailureKind::Network => Action::RetryAfter(backoff_secs(attempts)),
    }
}

/// 入口：初始化主仓库 → 恢复/对账 state → 串行认领执行全部 Pending 片
/// → 全部 Done 则 Ok（finalize 由调用方执行），否则 Err 列出失败片。
pub fn run(plan: &Plan, main: &Path, cfg: &SchedulerConfig) -> Result<()> {
    gitio::init_main_repo(main, &plan.url)?;
    let mut st = match State::load(main)? {
        // A.2：指纹不匹配 = state 来自另一次规划 → 绝不静默重规划
        Some(s) if s.fingerprint == plan.fingerprint() => s,
        Some(_) => bail!("plan/state mismatch — delete .rgc/ or restore plan.json"),
        // state 缺失/损坏（load_json 把损坏视同缺失）→ 对账重建
        None => crate::state::reconcile(plan, main),
    };
    st.save(main)?;
    worker(plan, main, &mut st, cfg)?;
    let failed: Vec<String> = st
        .pieces
        .iter()
        .filter(|p| p.status != PieceStatus::Done)
        .map(|p| p.id.clone())
        .collect();
    if !failed.is_empty() {
        bail!("pieces failed permanently: {:?} — rerun to resume", failed);
    }
    Ok(())
}

/// 串行工作循环：认领 → 执行 → 记账，直至无 Pending 片。
/// 单片失败不中止整个 run（否则"最后统一列出失败片"就是死代码）：
/// 片留在 Pending（attempts 已落盘），run 结束时统一报告，rerun 续跑。
fn worker(plan: &Plan, main: &Path, st: &mut State, cfg: &SchedulerConfig) -> Result<()> {
    loop {
        // 全局冷却：限流/拥塞后，后续片一律推迟（spec §4.4）
        let wait = cfg.cooldown.lock().unwrap().saturating_duration_since(Instant::now());
        if !wait.is_zero() {
            eprintln!("rgc: global cooldown, sleeping {:?} before next piece", wait);
            std::thread::sleep(wait);
        }
        let Some(idx) = st.pieces.iter().position(|p| p.status == PieceStatus::Pending) else {
            break;
        };
        // write-ahead 认领：Running 先落盘再执行；保存失败绝不带伤执行
        st.pieces[idx].status = PieceStatus::Running;
        st.save(main)?;
        let id = st.pieces[idx].id.clone();
        if let Err(e) = execute_piece(plan, main, st, idx, cfg) {
            eprintln!("rgc: piece {} failed this run: {e:#}", id);
        }
    }
    Ok(())
}

fn execute_piece(plan: &Plan, main: &Path, st: &mut State, idx: usize, cfg: &SchedulerConfig) -> Result<()> {
    // 磁盘预检：按历史最大片字节 × 1.5 估算
    let max_seen = st.pieces.iter().map(|p| p.bytes).max().unwrap_or(0);
    let estimate = (max_seen as f64 * 1.5) as u64;
    if estimate > 0 {
        if let Ok(avail) = fs2::available_space(main) {
            if avail < estimate {
                bail!("disk low: {} bytes available, next piece may need ~{}", avail, estimate);
            }
        }
    }
    let piece = plan.pieces[idx].clone();
    let piece_path: PathBuf = pieces_dir(main).join(piece.piece_dir_name());
    let mut ps = st.pieces[idx].clone();
    let res = run_piece(&piece, plan, main, &piece_path, &mut ps, cfg);
    st.pieces[idx] = ps;
    st.save(main)?;
    // 全局降速：限流/拥塞 → 后续片冷却 60s（spec §4.4）
    if let Err(e) = &res {
        if matches!(kind_of(e), FailureKind::RateLimited | FailureKind::Congestion) {
            *cfg.cooldown.lock().unwrap() = Instant::now() + Duration::from_secs(60);
        }
    }
    res
}

/// 片执行 + 重试循环。错误计预算（attempts++），按 decide 分诊：
/// GiveUp 留在 Pending（片仓库进度自描述，rerun 重新计费后可续跑）。
fn run_piece(piece: &Piece, plan: &Plan, main: &Path, piece_path: &Path, ps: &mut PieceState, cfg: &SchedulerConfig) -> Result<()> {
    // 新进程恢复：给耗尽预算的片重新计费（attempts 只约束单次 run 内的重试）
    if ps.status == PieceStatus::Pending && ps.attempts > cfg.max_attempts {
        ps.attempts = 0;
    }
    loop {
        let res = match piece {
            Piece::Chain { .. } => {
                let chain = ps
                    .chain
                    .clone()
                    .unwrap_or(ChainState { depth_done: 0, step: plan.initial_step, no_shallow: false });
                run_chain(piece, plan, main, ps, chain, cfg)
            }
            Piece::TagBatch { tags } => run_tags(plan, main, piece_path, tags, ps),
        };
        match res {
            Ok(()) => {
                ps.status = PieceStatus::Done;
                return Ok(());
            }
            Err(e) => {
                ps.attempts += 1;
                match decide(ps.attempts, kind_of(&e), cfg.max_attempts) {
                    Action::GiveUp => {
                        ps.status = PieceStatus::Pending;
                        return Err(e);
                    }
                    Action::RetryNoShallow => {
                        if let Some(c) = &mut ps.chain {
                            c.no_shallow = true;
                            c.depth_done = 0;
                        }
                        eprintln!("rgc: shallow unsupported on {} — falling back to full fetch", ps.id);
                    }
                    Action::HalveAndRetry => {
                        if let Some(c) = &mut ps.chain {
                            c.step = halve_step(c.step);
                        }
                        std::thread::sleep(Duration::from_secs(backoff_secs(ps.attempts)));
                    }
                    Action::RetryAfter(secs) => std::thread::sleep(Duration::from_secs(secs)),
                }
            }
        }
    }
}

/// 链式片执行：循环 fetch 直到浅边界消失（历史完整）。
/// 附录 A.6：git 禁止从浅仓搬运（shallow roots 禁更新，exit 0 静默）——
/// 只在链完成后调用一次 transport_to_main(final_dst=true)，中途不做 wip 搬运
/// （plan 正文"每步立即搬运"的注释作废；B1 断言在 transport_to_main 内，A.9）。
fn run_chain(piece: &Piece, plan: &Plan, main: &Path, ps: &mut PieceState, chain: ChainState, cfg: &SchedulerConfig) -> Result<()> {
    let Piece::Chain { full_ref, short_name: short } = piece else {
        bail!("run_chain on non-chain piece {}", piece.id());
    };
    let piece_path = pieces_dir(main).join(piece.piece_dir_name());
    let mut depth_done = chain.depth_done;
    let mut step = chain.step;
    gitio::ensure_piece_repo(main, &piece_path, &plan.url)?;
    loop {
        let (b0, c0) = gitio::repo_stats(&piece_path, short)?;
        let t0 = Instant::now();
        // TODO(A.10a): 远端在克隆中途删分支 → "couldn't find remote ref" 目前分类
        // 为 Fatal → GiveUp → 整个 clone 失败。按计划实现；重查 ls-remote 后标记
        // skipped 而非 failed 的分诊留给 Task 13/复审决定（附录 A.10）。
        gitio::fetch_chain_step(&piece_path, full_ref, short, step, depth_done, chain.no_shallow)?;
        let secs = t0.elapsed().as_secs_f64();
        let (b1, c1) = gitio::repo_stats(&piece_path, short)?;
        depth_done = c1;
        let added_bytes = b1.saturating_sub(b0);
        ps.bytes += added_bytes;
        // 记账在分诊之前：run_piece 的重试动作（减半/退化）都从最新状态出发
        ps.chain = Some(ChainState { depth_done, step, no_shallow: chain.no_shallow });
        let complete = !gitio::is_shallow(&piece_path);
        // A.5 零推进守卫：单次 fetch 新增 0 字节且浅边界未消失 → 可重试错误
        //（计入 attempts 预算），防止空 pack 死循环。
        if added_bytes == 0 && !complete {
            return Err(RgcError::Network(format!(
                "zero-progress fetch on {} (depth_done={depth_done}, step={step}) — empty pack, shallow boundary persists",
                ps.id
            ))
            .into());
        }
        if complete {
            gitio::transport_to_main(main, &piece_path, full_ref, short, true)?;
            return Ok(());
        }
        step = next_step(&StepMeasurement { bytes: added_bytes, secs, commits: c1.saturating_sub(c0) }, cfg.target_secs);
    }
}

/// tag 批片：一次连接按钉住 OID fetch，再原子搬运进主仓库正式 refs/tags/*。
/// tag fetch 永不加 --depth（片绝不能浅，A.6），故无需浅边界检查。
fn run_tags(plan: &Plan, main: &Path, piece_path: &Path, tags: &[crate::refs::RefEntry], ps: &mut PieceState) -> Result<()> {
    gitio::ensure_piece_repo(main, piece_path, &plan.url)?;
    let (b0, _) = gitio::repo_stats(piece_path, "")?;
    // TODO(A.11): 远端漂移导致 tag 批 "not our ref" / "couldn't find remote ref"
    // 目前按分类重试/放弃处理；重查 ls-remote 后标记 skipped 的分诊同 A.10a，
    // 留给 Task 13/复审决定（附录 A.11）。
    gitio::fetch_tag_batch(piece_path, tags)?;
    let (b1, _) = gitio::repo_stats(piece_path, "")?;
    ps.bytes += b1.saturating_sub(b0);
    gitio::transport_tags_to_main(main, piece_path, tags)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff_secs(0), 2);
        assert_eq!(backoff_secs(1), 4);
        assert_eq!(backoff_secs(2), 8);
        assert_eq!(backoff_secs(10), 60);
    }

    #[test]
    fn decide_retry_table() {
        assert_eq!(decide(0, FailureKind::Network, 5), Action::RetryAfter(2));
        assert_eq!(decide(2, FailureKind::Network, 5), Action::HalveAndRetry);
        assert_eq!(decide(0, FailureKind::RateLimited, 5), Action::RetryAfter(60));
        assert_eq!(decide(1, FailureKind::RateLimited, 5), Action::RetryAfter(120));
        assert_eq!(decide(0, FailureKind::ShallowUnsupported, 5), Action::RetryNoShallow);
        assert_eq!(decide(0, FailureKind::Fatal, 5), Action::GiveUp);
        assert_eq!(decide(6, FailureKind::Network, 5), Action::GiveUp);
    }

    /// 重试循环与步长自适应的集成行为：Network 失败第 2 次起步长减半（下限
    /// MIN_STEP），恢复后按实测吞吐回升 —— run_piece 用 halve_step 降档、
    /// run_chain 用 next_step 升档，两者都作用在同一 ps.chain.step 上。
    #[test]
    fn retry_loop_halves_step_then_adapts() {
        let mut c = ChainState { depth_done: 0, step: 10_000, no_shallow: false };
        // 第 2 次网络失败 → HalveAndRetry：步长减半
        c.step = halve_step(c.step);
        assert_eq!(c.step, 5_000);
        c.step = halve_step(c.step);
        assert_eq!(c.step, 2_500);
        // 链步成功后按实测回升：1000B/s 吞吐 × 100B/commit 密度 × 600s → 6000
        let m = StepMeasurement { bytes: 1000, secs: 1.0, commits: 10 };
        c.step = next_step(&m, 600.0);
        assert_eq!(c.step, 6_000);
        // 降档下限：MIN_STEP 不再减半
        assert_eq!(halve_step(crate::planner::MIN_STEP), crate::planner::MIN_STEP);
    }
}
