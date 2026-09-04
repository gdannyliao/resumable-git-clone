//! Task 12：串行调度核心 —— write-ahead 状态机驱动的片执行引擎。
//! Task 14（附录 A.15）：并行化 —— jobs 个 worker 经 `Arc<Mutex<State>>` 共享
//! 状态（认领/落盘全部锁内完成，保存串行化）。
//! 降速收口：run 级限流信号（冷却 / 断路器 / 并发减半闸门 / 风暴退避升级）
//! 全部归 [`crate::throttle::Throttle`]；本模块只剩片级重试决策（decide）与
//! 台账记账。限流不进 decide 的决策表（spec §5 不计片预算，见 run_piece）。
//!
//! write-ahead 不变式：片开始执行前，状态先落盘（Pending→Running，崩溃后
//! `State::load` 把 Running 折返 Pending）；执行结束后再落盘（Done / Failed
//! 终态 / attempts++）。片仓库的实际 fetch 进度（refs/heads/*、shallow 文件）
//! 自描述，重跑零浪费。
//!
//! 附录 A.6：git 拒绝从浅仓搬运（shallow roots 禁更新，exit 0 静默）——链式片
//! 只在完成后（`!is_shallow`）做一次 final 搬运，中途不做 wip 搬运
//! （plan 正文"每步立即搬运"的注释作废）。B1 断言在 transport_to_main 内（A.9）。

use crate::errors::{kind_of, FailureKind, RgcError};
use crate::gitio;
use crate::planner::{halve_step, next_step, Piece, Plan, StepMeasurement};
use crate::state::{pieces_dir, ChainState, PieceState, PieceStatus, State};
use crate::throttle::Throttle;
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// —— 测试缝隙（附录 A.14 / Task 12 审查 I2）——
/// 包装每次真实 fetch：参数是"本步真实 fetch"闭包，默认直通调用。
/// 测试注入：(a) 失败 N 次后调 real()；(b) 恒返回错误；(c) 返回 Ok 但不调
/// real()（假成功 → 零推进停滞）。无 trait 层级，仅一个配置字段。
pub type FetchHook = Arc<dyn Fn(&dyn Fn() -> Result<()>) -> Result<()> + Send + Sync>;
/// 退避/冷却休眠缝隙：参数为精确 Duration（A.15 顺手项：冷却等待绝不
/// as_secs 截断）；默认真实 thread::sleep；测试注入 no-op 使退避瞬时。
pub type SleepHook = Arc<dyn Fn(Duration) + Send + Sync>;

/// A.15：并行 worker 数上限（jobs 钳制 1..=MAX_JOBS —— 误配 0 会得到零
/// worker 的静默假完成，过大爆线程）。
pub const MAX_JOBS: usize = 8;

#[derive(Clone)]
pub struct SchedulerConfig {
    pub max_attempts: u32,
    /// spec §5：限流不计片的重试预算 —— 独立护栏：连续限流超过此上限
    /// （未出现非限流失败即"连续"）才放弃，防 429 风暴死循环。
    pub max_rate_limits: u32,
    /// A.15：并行 worker 数（run() 钳制到 1..=MAX_JOBS，默认 2）。
    pub jobs: usize,
    /// A.15a：run 级总 429 断路器阈值 —— run 全程累计的限流次数一旦超过，
    /// 整个 run 立即放弃（"upstream rate-limited — rerun later"）。
    pub rate_limit_breaker: u32,
    /// A.15a：并发减半软阈值 —— 总 429 超过后，新片认领按一半并行进行
    ///（effective_jobs 减半并保持到 run 结束；计数只增不减）。
    pub rate_limit_soft_cap: u32,
    /// 限流/拥塞后的全局冷却时长（秒）。默认 [`crate::throttle::COOLDOWN_SECS`]；测试可调小。
    pub cooldown_secs: u64,
    pub target_secs: f64,
    /// 测试缝隙（A.14 idiom）：注入预构造的 Throttle（预布防冷却 / 预推计数）；
    /// None = run() 以本配置的旋钮自造一个全新降速器。
    pub throttle: Option<Throttle>,
    /// 测试缝隙：包装每次真实 fetch（默认直通，见 [`FetchHook`]）
    pub fetch: FetchHook,
    /// 测试缝隙：退避/冷却休眠（默认真实 sleep，见 [`SleepHook`]）
    pub sleep: SleepHook,
}
impl Default for SchedulerConfig {
    fn default() -> Self {
        // spec §5：默认 5 次；限流独立上限取宽裕的 10 次；断路器 40/软阈值 10
        Self {
            max_attempts: 5,
            max_rate_limits: 10,
            jobs: 2,
            rate_limit_breaker: 40,
            rate_limit_soft_cap: 10,
            cooldown_secs: crate::throttle::COOLDOWN_SECS,
            target_secs: 600.0,
            throttle: None,
            fetch: Arc::new(|real: &dyn Fn() -> Result<()>| real()),
            sleep: Arc::new(|d: Duration| std::thread::sleep(d)),
        }
    }
}
impl SchedulerConfig {
    /// A.15：jobs 钳制（1..=MAX_JOBS）—— 在 run() 入口统一收敛，配置原样保留。
    pub fn clamped_jobs(&self) -> usize {
        self.jobs.clamp(1, MAX_JOBS)
    }
}
impl std::fmt::Debug for SchedulerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // fetch/sleep 是函数指针，无 Debug —— 手动实现保住结构体可诊断性
        f.debug_struct("SchedulerConfig")
            .field("max_attempts", &self.max_attempts)
            .field("max_rate_limits", &self.max_rate_limits)
            .field("jobs", &self.jobs)
            .field("rate_limit_breaker", &self.rate_limit_breaker)
            .field("rate_limit_soft_cap", &self.rate_limit_soft_cap)
            .field("target_secs", &self.target_secs)
            .finish_non_exhaustive()
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

/// 片级重试决策（非限流类别）：Fatal→放弃；Shallow 不支持→退化整支 fetch；
/// 拥塞(reset)→退避；网络失败第 2 次起步长减半。
/// 限流返回 None —— 不进片级决策表：spec §5 不计片预算（attempts 原地踏步），
/// 冷却布防 / 风暴退避升级 / 断路器全部归 [`Throttle`] 路由（见 run_piece）。
pub fn decide(attempts: u32, kind: FailureKind, max_attempts: u32) -> Option<Action> {
    if kind == FailureKind::RateLimited {
        return None;
    }
    if attempts > max_attempts {
        return Some(Action::GiveUp);
    }
    Some(match kind {
        FailureKind::RateLimited => unreachable!("rate-limited returned early"),
        FailureKind::ShallowUnsupported => Action::RetryNoShallow,
        FailureKind::Fatal => Action::GiveUp,
        FailureKind::Congestion => Action::RetryAfter(backoff_secs(attempts)),
        FailureKind::Network if attempts >= 2 => Action::HalveAndRetry,
        FailureKind::Network => Action::RetryAfter(backoff_secs(attempts)),
    })
}

/// 入口：初始化主仓库 → 恢复/对账 state → 并行认领执行全部 Pending 片
/// → 全部 Done 则 Ok（finalize 由调用方执行），否则 Err 列出失败片。
///
/// Task 14（A.15c）：state 由 run() 以 `Arc<Mutex<State>>` 持有，jobs 个
/// worker 共享；全部片状态变更与落盘都在锁内完成（保存天然串行化，
/// write-ahead 认领协议语义不变）。A.15a：断路器触发时整个 run 立即
/// 放弃，优先于逐片失败报告。
pub fn run(plan: &Plan, main: &Path, cfg: &SchedulerConfig) -> Result<()> {
    gitio::init_main_repo(main, &plan.url)?;
    let mut st = match State::load(main)? {
        // A.2：指纹不匹配 = state 来自另一次规划 → 绝不静默重规划
        Some(s) if s.fingerprint == plan.fingerprint() => s,
        Some(_) => bail!("plan/state mismatch — delete .rgc/ or restore plan.json"),
        // state 缺失/损坏（load_json 把损坏视同缺失）→ 对账重建
        None => crate::state::reconcile(plan, main),
    };
    // C1：Failed 是上一轮的终态 —— rerun 折返 Pending 重新出发
    //（认领时 rebill 给全新预算），终态片不占坑、可重试。
    for p in &mut st.pieces {
        if p.status == PieceStatus::Failed {
            p.status = PieceStatus::Pending;
        }
    }
    st.save(main)?;
    let st = Arc::new(Mutex::new(st));
    // 降速器：测试可经 SchedulerConfig::throttle 注入预布防的实例；否则自造
    let throttle = cfg.throttle.clone().unwrap_or_else(|| Throttle::new(cfg));
    // A.15d 前提：worker 经 scope 借用 plan/main/cfg/throttle 跨线程运行
    //（SchedulerConfig 的 Send+Sync 静态断言见 tests/scheduler_parallel.rs），
    // 免去 Arc<Plan> 克隆。
    let failures: Vec<String> = std::thread::scope(|scope| {
        // 引用先行绑定（& 为 Copy）：move 闭包只按值拿引用与 worker_idx，
        // 不会搬走 Arc/Throttle 本体
        let st = &st;
        let throttle = &throttle;
        let configured_jobs = cfg.clamped_jobs();
        let handles: Vec<_> = (0..configured_jobs)
            .map(|worker_idx| scope.spawn(move || worker(plan, main, st, cfg, throttle, worker_idx)))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_else(|_| vec!["<worker panicked>".to_string()]))
            .collect()
    });
    // A.15a：断路器优先 —— 全局 429 超阈值时整个 run 立即放弃
    //（片停在 Running 无妨：rerun 的 load 把 Running 折返 Pending）
    if throttle.tripped() {
        bail!("{:#}", throttle.abort_error());
    }
    let failed: Vec<String> = {
        let g = st.lock().unwrap_or_else(|p| p.into_inner());
        g.pieces
            .iter()
            .filter(|p| p.status != PieceStatus::Done)
            .map(|p| p.id.clone())
            .collect()
    };
    if !failed.is_empty() {
        // 逐片带上最后错误：调用方不必翻 stderr 就能看到失败原因
        // （限流风暴 → "rate-limited too many times"，停滞 → "zero-progress" 等）
        bail!("pieces failed permanently: {:?} — rerun to resume. {}", failed, failures.join(" | "));
    }
    Ok(())
}

/// 并行工作循环（A.15）：认领 → 执行 → 记账，直至无 Pending 片或断路器触发。
/// 单片失败不中止整个 run（否则"最后统一列出失败片"就是死代码）：
/// GiveUp 片落终态 Failed，逐片失败明细随 worker 返回，run 结束统一报告；
/// rerun 由 run() 折返重试。断路器触发则立即收工，由 run() 统一报告。
fn worker(plan: &Plan, main: &Path, st: &Arc<Mutex<State>>, cfg: &SchedulerConfig, throttle: &Throttle, worker_idx: usize) -> Vec<String> {
    let mut failures: Vec<String> = Vec::new();
    loop {
        if throttle.tripped() {
            break;
        }
        // 先看有没有活再等：无片可认领时耗完冷却毫无意义（终局前白睡 60s）。
        // Pending 集合在单次 run 内只缩不涨（Pending→Running→Done/Failed 单向），
        // 此处看到空即安全收工；竞态下 claim_piece 返回 None 同样收工。
        if !has_pending(st) {
            break;
        }
        // 认领前的统一等待：全局冷却（循环重读 + 每 worker 抖动，spec §4.4/A.15b）
        // + 并发减半闸门（A.15a；Pending 清空即放行被闸 worker，A.17）——
        // 全部归 Throttle；台账探测以闭包注入，Throttle 保持台账无关。
        throttle.wait_turn(worker_idx, &|| has_pending(st));
        if throttle.tripped() {
            break;
        }
        let claimed = match claim_piece(main, st, cfg) {
            Ok(claimed) => claimed,
            Err(e) => {
                // 保存失败绝不带伤执行：本 worker 就此收工，run 统一报告
                eprintln!("rgc: worker {worker_idx}: state save failed — stopping: {e:#}");
                failures.push(format!("state save failed: {e:#}"));
                break;
            }
        };
        let Some((idx, id)) = claimed else {
            break;
        };
        if let Err(e) = execute_piece(plan, main, st, idx, cfg, throttle) {
            // A.15a：全局放弃优先于逐片失败报告
            if throttle.tripped() {
                break;
            }
            eprintln!("rgc: piece {} failed this run: {e:#}", id);
            failures.push(format!("{}: {e:#}", id));
        }
    }
    failures
}

/// 锁内快照：是否还有 Pending 片（只为避免无活 worker 白睡冷却）。
fn has_pending(st: &Arc<Mutex<State>>) -> bool {
    let g = st.lock().unwrap_or_else(|p| p.into_inner());
    g.pieces.iter().any(|p| p.status == PieceStatus::Pending)
}

/// write-ahead 认领（A.15c）：全部状态变更 + 落盘在状态锁内完成（保存天然
/// 串行化，串行协议语义不变）—— Running 先落盘再执行。
/// C2：认领时给耗尽预算的片重新计费 —— attempts 只约束单次 run 内的重试，
/// rerun 给每个片全新预算。（旧位置在 run_piece 里判 `status == Pending`，
/// 而此处 write-ahead 认领早已置 Running → 条件永假，是死代码。）
/// 限流计数同预算哲学：rerun 认领时清零，风暴后的重跑以全新配额出发
///（否则上轮积累到顶的 rate_limits 会让 rerun 一次 429 就立刻放弃）。
/// 返回 None = 已无 Pending 片。
fn claim_piece(main: &Path, st: &Arc<Mutex<State>>, cfg: &SchedulerConfig) -> Result<Option<(usize, String)>> {
    let mut g = st.lock().unwrap_or_else(|p| p.into_inner());
    let Some(idx) = g.pieces.iter().position(|p| p.status == PieceStatus::Pending) else {
        return Ok(None);
    };
    if g.pieces[idx].attempts > cfg.max_attempts {
        g.pieces[idx].attempts = 0;
    }
    g.pieces[idx].rate_limits = 0;
    g.pieces[idx].status = PieceStatus::Running;
    g.save(main)?;
    Ok(Some((idx, g.pieces[idx].id.clone())))
}

/// 片执行（A.15c）：预检与结果回写在状态锁内进行；片执行本身（可能数分钟）
/// 完全不持锁 —— 其他 worker 的认领/落盘不受阻塞。ps 是锁内克隆出的私有
/// 副本，回写时整体覆盖（片认领后无人再动它的槽位，idx 稳定有效；
/// 即使本片执行期间他人多次落盘，重放顺序也不影响各片字段的归属）。
fn execute_piece(plan: &Plan, main: &Path, st: &Arc<Mutex<State>>, idx: usize, cfg: &SchedulerConfig, throttle: &Throttle) -> Result<()> {
    let (piece, mut ps) = {
        let g = st.lock().unwrap_or_else(|p| p.into_inner());
        // 磁盘预检：按历史最大片字节 × 1.5 估算
        let max_seen = g.pieces.iter().map(|p| p.bytes).max().unwrap_or(0);
        let estimate = (max_seen as f64 * 1.5) as u64;
        if estimate > 0 {
            if let Ok(avail) = fs2::available_space(main) {
                if avail < estimate {
                    bail!("disk low: {} bytes available, next piece may need ~{}", avail, estimate);
                }
            }
        }
        (plan.pieces[idx].clone(), g.pieces[idx].clone())
    };
    let piece_path: PathBuf = pieces_dir(main).join(piece.piece_dir_name());
    let res = run_piece(&piece, plan, main, &piece_path, &mut ps, cfg, throttle);
    {
        let mut g = st.lock().unwrap_or_else(|p| p.into_inner());
        g.pieces[idx] = ps;
        g.save(main)?;
    }
    // 全局冷却布防在 run_piece 内逐次进行（限流/拥塞的每次出现，spec §4.4，
    // 经 Throttle::on_failure）
    res
}

/// 片执行 + 重试循环。错误计预算（attempts++），按 decide 分诊：
/// GiveUp 落终态 Failed（片已定局：本轮不再碰它，rerun 由 run() 折返重试）。
/// spec §5 例外：429/限流/滥用检测不计入片的重试次数 —— attempts 原地踏步，
/// 连续限流次数独立记账（非限流失败即复位），超过 max_rate_limits 才放弃。
/// run 级降速信号（冷却布防 / 总 429 计数 / 断路器 / 并发减半）全部经
/// `throttle.on_failure` 上报，由 Throttle 独占（A.15a 语义不变）。
fn run_piece(piece: &Piece, plan: &Plan, main: &Path, piece_path: &Path, ps: &mut PieceState, cfg: &SchedulerConfig, throttle: &Throttle) -> Result<()> {
    loop {
        // A.15a：断路器已触发 → 立即止损（退避睡眠醒来后第一时间退出）
        if throttle.tripped() {
            return Err(throttle.abort_error());
        }
        let res = match piece {
            Piece::Chain { .. } => {
                let chain = ps
                    .chain
                    .clone()
                    .unwrap_or(ChainState { depth_done: 0, step: plan.initial_step, no_shallow: false });
                run_chain(piece, plan, main, ps, chain, cfg, throttle)
            }
            Piece::TagBatch { tags } => run_tags(plan, main, piece_path, tags, ps, cfg),
        };
        match res {
            Ok(()) => {
                ps.status = PieceStatus::Done;
                // A.16 不变量：Done 片的 rate_limits 持久化为 0（链片在成功步
                // 已重置；此处兜底 tag 片——run_tags 无逐步重试点）。
                ps.rate_limits = 0;
                return Ok(());
            }
            Err(e) => {
                let kind = kind_of(&e);
                // 全局降速信号上报（spec §4.4 / A.15a）：限流/拥塞的每一次出现
                // 都布防冷却 —— 即便片随后重试成功，后续片也应推迟（原逻辑只看
                // 片终态，会漏布防"中间限流但最终成功"的情形）；限流同时喂
                // 总 429 计数（断路器 + 并发减半的信号源）。
                throttle.on_failure(kind);
                if kind == FailureKind::RateLimited {
                    ps.rate_limits += 1;
                    if ps.rate_limits > cfg.max_rate_limits {
                        // 不计预算 ≠ 无限重试：429 风暴到此为止
                        ps.status = PieceStatus::Failed;
                        return Err(anyhow::anyhow!(
                            "piece {} rate-limited too many times ({} consecutive 429/abuse responses, cap {}) — giving up this run",
                            ps.id,
                            ps.rate_limits,
                            cfg.max_rate_limits
                        ));
                    }
                    // A.15a：总 429 刚越过断路器阈值 → 立即放弃整个 run，
                    // 不再退避重试（绝不把 run 拖过一次长退避）
                    if throttle.tripped() {
                        return Err(throttle.abort_error());
                    }
                    // 退避时长按"连续限流数"升级（A.15 顺手项）：60→120→240
                    // 封顶；限流不计预算，attempts 在风暴中原地踏步，故时长
                    // 取自连续计数（Throttle::storm_backoff），与 attempts 无关
                    (cfg.sleep)(Throttle::storm_backoff(ps.rate_limits));
                } else {
                    ps.attempts += 1;
                    ps.rate_limits = 0; // "连续"限流：非限流失败打断计数
                    match decide(ps.attempts, kind, cfg.max_attempts).expect("non-rate-limit kinds always yield an action") {
                        Action::GiveUp => {
                            // C1：终态 Failed —— 认领只挑 Pending，工作循环不再
                            // 重认领本片（修复前折回 Pending → 无限重认领热循环，
                            // run() 永不返回）；run 结束统一报告，rerun 折返重试。
                            ps.status = PieceStatus::Failed;
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
                            (cfg.sleep)(Duration::from_secs(backoff_secs(ps.attempts)));
                        }
                        Action::RetryAfter(secs) => (cfg.sleep)(Duration::from_secs(secs)),
                    }
                }
            }
        }
    }
}

/// 链式片执行：循环 fetch 直到浅边界消失（历史完整）。
/// 附录 A.6：git 禁止从浅仓搬运（shallow roots 禁更新，exit 0 静默）——
/// 只在链完成后调用一次 transport_to_main(final_dst=true)，中途不做 wip 搬运
/// （plan 正文"每步立即搬运"的注释作废；B1 断言在 transport_to_main 内，A.9）。
fn run_chain(piece: &Piece, plan: &Plan, main: &Path, ps: &mut PieceState, chain: ChainState, cfg: &SchedulerConfig, throttle: &Throttle) -> Result<()> {
    let Piece::Chain { full_ref, short_name: short } = piece else {
        bail!("run_chain on non-chain piece {}", piece.id());
    };
    let piece_path = pieces_dir(main).join(piece.piece_dir_name());
    let mut depth_done = chain.depth_done;
    let mut step = chain.step;
    gitio::ensure_piece_repo(main, &piece_path, &plan.url)?;
    // A.5：单次 run_chain 调用的迭代上限 —— 步长自适应失灵（或上游状态机出 bug）
    // 时兜底：返回可重试的 Network 错误，让 attempts 预算机制接管，绝不无限打转。
    const MAX_CHAIN_ITERATIONS: u32 = 100_000;
    let mut iterations: u32 = 0;
    loop {
        iterations += 1;
        if iterations > MAX_CHAIN_ITERATIONS {
            return Err(RgcError::Network(format!(
                "chain piece {} exceeded {MAX_CHAIN_ITERATIONS} fetch iterations without completing — aborting this run (retryable)",
                ps.id
            ))
            .into());
        }
        // A.15a：断路器已触发 → 不再发起新一步 fetch（run 由 run_piece 收口）
        if throttle.tripped() {
            return Err(throttle.abort_error());
        }
        let (b0, c0) = gitio::repo_stats(&piece_path, short)?;
        let t0 = Instant::now();
        // TODO(A.10a): 远端在克隆中途删分支 → "couldn't find remote ref" 目前分类
        // 为 Fatal → GiveUp → 整个 clone 失败。按计划实现；重查 ls-remote 后标记
        // skipped 而非 failed 的分诊留给 Task 13/复审决定（附录 A.10）。
        // 测试缝隙：fetch 经 cfg.fetch 包装（默认直通；见 FetchHook）。
        let real = || gitio::fetch_chain_step(&piece_path, full_ref, short, step, depth_done, chain.no_shallow);
        (cfg.fetch)(&real)?;
        // A.15 顺手项：成功步打断"连续限流"计数（连续性以成功/其他失败为界）
        ps.rate_limits = 0;
        let secs = t0.elapsed().as_secs_f64();
        let (b1, c1) = gitio::repo_stats(&piece_path, short)?;
        depth_done = c1;
        let added_bytes = b1.saturating_sub(b0);
        ps.bytes += added_bytes;
        // 记账在分诊之前：run_piece 的重试动作（减半/退化）都从最新状态出发
        ps.chain = Some(ChainState { depth_done, step, no_shallow: chain.no_shallow });
        let complete = !gitio::is_shallow(&piece_path);
        // A.5 零推进守卫：单次 fetch 新增 0 字节、提交计数不变且浅边界未消失
        // → 可重试错误（计入 attempts 预算），防止空 pack 死循环。dir_size 粒度
        // 可能掩盖真实推进（同字节数的引用更新），故提交计数不变才是必要条件。
        if added_bytes == 0 && c1 == c0 && !complete {
            return Err(RgcError::Network(format!(
                "zero-progress fetch on {} (depth_done={depth_done}, step={step}, +0 bytes, +0 commits) — empty pack, shallow boundary persists",
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
fn run_tags(plan: &Plan, main: &Path, piece_path: &Path, tags: &[crate::refs::RefEntry], ps: &mut PieceState, cfg: &SchedulerConfig) -> Result<()> {
    gitio::ensure_piece_repo(main, piece_path, &plan.url)?;
    let (b0, _) = gitio::repo_stats(piece_path, "")?;
    // TODO(A.11): 远端漂移导致 tag 批 "not our ref" / "couldn't find remote ref"
    // 目前按分类重试/放弃处理；重查 ls-remote 后标记 skipped 的分诊同 A.10a，
    // 留给 Task 13/复审决定（附录 A.11）。
    // 测试缝隙：fetch 经 cfg.fetch 包装（默认直通；见 FetchHook）。
    let real = || gitio::fetch_tag_batch(piece_path, tags);
    (cfg.fetch)(&real)?;
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
        assert_eq!(decide(0, FailureKind::Network, 5), Some(Action::RetryAfter(2)));
        assert_eq!(decide(2, FailureKind::Network, 5), Some(Action::HalveAndRetry));
        assert_eq!(decide(0, FailureKind::ShallowUnsupported, 5), Some(Action::RetryNoShallow));
        assert_eq!(decide(0, FailureKind::Fatal, 5), Some(Action::GiveUp));
        assert_eq!(decide(6, FailureKind::Network, 5), Some(Action::GiveUp));
        // 限流不进片级决策表：由 Throttle 路由（冷却布防 / 风暴升级 / 断路器）。
        // spec §5：429 不计片预算；限流风暴由 max_rate_limits 与断路器拦截
        // （见 scheduler_retry / scheduler_parallel 套件）。
        assert_eq!(decide(0, FailureKind::RateLimited, 5), None);
        assert_eq!(decide(6, FailureKind::RateLimited, 5), None);
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
