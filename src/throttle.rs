//! Throttle（降速器）—— run 级限流信号的独占 module：冷却、断路器、
//! 并发减半闸门、风暴退避升级。设计定稿见 CONTEXT.md「Throttle」。
//!
//! 职责边界：只持有 run 级信号；片级连续限流计数（PieceState.rate_limits）
//! 是持久化台账状态，留在 scheduler/state，不在此处。
//!
//! 不变式（自 scheduler 迁入，语义不变）：
//! - 计数只增不减；断路器一经触发不可撤销（rerun 才能以全新配额重来）；
//!   effective_jobs 减半后保持到 run 结束。原子量用 Relaxed 即可：漏看一拍
//!   最多多认领一片/多睡一拍，无正确性影响（协作式止损与降速）。
//! - 冷却等待循环重读直到归零（A.15b）：睡眠窗口内冷却可能被其他 worker
//!   再次推后，一次性睡眠会带着未过期的冷却放行下一片。
//! - 闸门放行（A.17）：风暴消退（计数停在软/硬阈值之间）时 halving 永不恢复，
//!   被闸 worker 必须在 Pending 清空后退出，否则 run() 在 join 处挂死。

use crate::errors::FailureKind;
use crate::scheduler::{SchedulerConfig, SleepHook};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// spec §4.4：限流/拥塞后的全局冷却时长默认值。
pub const COOLDOWN_SECS: u64 = 60;
/// A.15b：每 worker 抖动上限（毫秒）——确定性哈希错峰，防 thundering herd。
pub const COOLDOWN_JITTER_MS_MAX: u64 = 500;

/// run 级降速器。Clone 共享同一份 internals（worker 线程与注入测试都持有 clone）。
#[derive(Clone)]
pub struct Throttle {
    /// 全局冷却时刻：限流/拥塞后，后续片一律推迟到此之后（spec §4.4）
    cooldown: Arc<Mutex<Instant>>,
    total_rate_limits: Arc<AtomicU32>,
    breaker: Arc<AtomicBool>,
    effective_jobs: Arc<AtomicUsize>,
    configured_jobs: usize,
    soft_cap: u32,
    breaker_threshold: u32,
    cooldown_secs: u64,
    /// 退避/冷却休眠缝隙（A.14 idiom：与 SchedulerConfig::sleep 同一 hook）
    sleep: SleepHook,
}

impl Throttle {
    /// 从调度器配置取旋钮（冷却时长 / 软阈值 / 断路器阈值 / 钳制后的 jobs / sleep hook）。
    pub fn new(cfg: &SchedulerConfig) -> Throttle {
        Throttle {
            cooldown: Arc::new(Mutex::new(Instant::now())),
            total_rate_limits: Arc::new(AtomicU32::new(0)),
            breaker: Arc::new(AtomicBool::new(false)),
            effective_jobs: Arc::new(AtomicUsize::new(cfg.clamped_jobs())),
            configured_jobs: cfg.clamped_jobs(),
            soft_cap: cfg.rate_limit_soft_cap,
            breaker_threshold: cfg.rate_limit_breaker,
            cooldown_secs: cfg.cooldown_secs,
            sleep: cfg.sleep.clone(),
        }
    }

    /// 失败信号单一入口：限流/拥塞的每一次出现都布防冷却（spec §4.4）——
    /// 即便片随后重试成功，后续片也应推迟；限流同时计入 run 级总 429 计数，
    /// 越过断路器阈值则触发全局放弃，越过软阈值则新片认领并行度减半（A.15a）。
    pub fn on_failure(&self, kind: FailureKind) {
        if matches!(kind, FailureKind::RateLimited | FailureKind::Congestion) {
            self.arm_cooldown(Duration::from_secs(self.cooldown_secs));
        }
        if kind == FailureKind::RateLimited {
            let total = self.total_rate_limits.fetch_add(1, Ordering::Relaxed) + 1;
            if total > self.breaker_threshold {
                self.breaker.store(true, Ordering::Relaxed);
            }
            if total > self.soft_cap {
                self.effective_jobs.store((self.configured_jobs / 2).max(1), Ordering::Relaxed);
            }
        }
    }

    /// 布防冷却：后续片一律推迟到 now + d 之后（on_failure 用配置时长；
    /// 显式时长留给测试预布防/再布防与未来的外部信号）。
    pub fn arm_cooldown(&self, d: Duration) {
        *self.cooldown.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now() + d;
    }

    /// 冷却是否仍在生效（测试探针：替代旧时对冷却时刻字段的直接断言）。
    pub fn is_cooling(&self) -> bool {
        !self
            .cooldown
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .saturating_duration_since(Instant::now())
            .is_zero()
    }

    pub fn tripped(&self) -> bool {
        self.breaker.load(Ordering::Relaxed)
    }

    pub fn effective_jobs(&self) -> usize {
        self.effective_jobs.load(Ordering::Relaxed)
    }

    /// A.15a：断路器触发后的统一放弃消息（spec 原文）。
    pub fn abort_error(&self) -> anyhow::Error {
        anyhow::anyhow!("upstream rate-limited — rerun later")
    }

    /// 风暴退避按连续 429 数升级 —— 60s → 120s → 240s 封顶
    ///（附录公式 `60 << rate_limits.min(2)`，以 0 基步数计：第 1 次 429 仍是 60s）。
    /// 纯策略函数：与实例无关，时长基数恒为 COOLDOWN_SECS（不随 cooldown_secs 旋钮）。
    pub fn storm_backoff(consecutive_rate_limits: u32) -> Duration {
        Duration::from_secs(COOLDOWN_SECS << consecutive_rate_limits.saturating_sub(1).min(2))
    }

    /// 认领前的统一等待：先消费全局冷却（循环重读 + 每 worker 抖动），再过减半闸门。
    /// has_pending 由调用方注入（Throttle 保持台账无关）；断路器触发即提前返回。
    pub fn wait_turn(&self, worker_idx: usize, has_pending: &dyn Fn() -> bool) {
        self.wait_out_cooldown(worker_idx);
        self.wait_for_gate(worker_idx, has_pending);
    }

    /// A.15b：冷却等待循环 —— 读冷却 → 睡（精确 Duration，绝不 as_secs 截断，
    /// 叠加 worker 抖动）→ 醒来重读 → 直到归零。断路器触发即提前返回
    ///（放弃在即，别把 run 拖过冷却）。
    fn wait_out_cooldown(&self, worker_idx: usize) {
        let jitter = cooldown_jitter(worker_idx);
        let mut logged = false; // 每次布防 episode 只记一次（no-op sleep 钩子下循环会热转，逐圈打日志会刷屏）
        while !self.tripped() {
            let wait = self
                .cooldown
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .saturating_duration_since(Instant::now());
            if wait.is_zero() {
                return;
            }
            if !logged {
                eprintln!("rgc: worker {worker_idx}: global cooldown, sleeping {wait:?} (+{jitter:?} jitter) before next piece");
                logged = true;
            }
            (self.sleep)(wait + jitter);
        }
    }

    /// A.15a：并发减半闸门 —— 总 429 超过软阈值后 effective_jobs 减半，序号不在
    /// 前一半的 worker 在此让路（真实短睡轮询；属调度原语而非退避，不走 sleep
    /// 缝隙以免测试中把"等待"误记为"退避"）。退出条件除断路器外还有
    /// **无 Pending 片**（A.17）：风暴消退时 halving 永不恢复，若只等恢复，
    /// 被闸 worker 会在活跃 worker 清完所有片后永远滞留 → run 挂死。
    /// configured_jobs == 1 时减半仍为 1，闸门天然失效（不会饿死单 worker）。
    fn wait_for_gate(&self, worker_idx: usize, has_pending: &dyn Fn() -> bool) {
        while worker_idx >= self.effective_jobs() {
            if self.tripped() || !has_pending() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// A.15b：每 worker 确定性抖动（哈希 worker 序号，0..=500ms）——避免冷却
/// 到点后全部 worker 同一瞬间扑向上游（thundering herd）。
fn cooldown_jitter(worker_idx: usize) -> Duration {
    Duration::from_millis((worker_idx as u64).wrapping_mul(137).wrapping_add(61) % (COOLDOWN_JITTER_MS_MAX + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::FailureKind;
    use crate::scheduler::{SchedulerConfig, SleepHook};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn no_sleep() -> SleepHook {
        Arc::new(|_| {})
    }

    fn test_cfg() -> SchedulerConfig {
        SchedulerConfig { sleep: no_sleep(), ..Default::default() }
    }

    /// 布防冷却的类别只有 RateLimited/Congestion（spec §4.4）；普通网络失败不布防。
    #[test]
    fn on_failure_arms_cooldown_only_for_throttled_kinds() {
        let t = Throttle::new(&test_cfg());
        assert!(!t.is_cooling(), "fresh throttle must not be cooling");
        t.on_failure(FailureKind::Network);
        assert!(!t.is_cooling(), "plain network failure must not arm the cooldown");
        t.on_failure(FailureKind::Fatal);
        assert!(!t.is_cooling());
        t.on_failure(FailureKind::RateLimited);
        assert!(t.is_cooling(), "rate limit must arm the global cooldown");
    }

    #[test]
    fn congestion_also_arms_cooldown() {
        let t = Throttle::new(&test_cfg());
        t.on_failure(FailureKind::Congestion);
        assert!(t.is_cooling(), "congestion (reset) must arm the global cooldown");
        assert!(!t.tripped(), "congestion never feeds the 429 breaker");
    }

    /// A.15a：总 429 计数越过断路器阈值 → tripped（只增不减，不可撤销）。
    #[test]
    fn breaker_trips_after_threshold() {
        let cfg = SchedulerConfig { rate_limit_breaker: 3, ..test_cfg() };
        let t = Throttle::new(&cfg);
        for _ in 0..3 {
            t.on_failure(FailureKind::RateLimited);
            assert!(!t.tripped(), "at/below threshold must not trip");
        }
        t.on_failure(FailureKind::RateLimited);
        assert!(t.tripped(), "total 429s beyond threshold must trip the breaker");
    }

    /// A.15a：总 429 越过软阈值 → effective_jobs 减半（下限 1），保持到 run 结束。
    #[test]
    fn soft_cap_halves_effective_jobs() {
        let cfg = SchedulerConfig { jobs: 4, rate_limit_soft_cap: 2, ..test_cfg() };
        let t = Throttle::new(&cfg);
        assert_eq!(t.effective_jobs(), 4);
        t.on_failure(FailureKind::RateLimited);
        t.on_failure(FailureKind::RateLimited);
        assert_eq!(t.effective_jobs(), 4, "at the soft cap, not yet halved");
        t.on_failure(FailureKind::RateLimited);
        assert_eq!(t.effective_jobs(), 2, "beyond soft cap, new claims run at half parallelism");
    }

    /// 风暴退避按连续 429 数升级：60 → 120 → 240 封顶（A.15 顺手项公式 60<<min(2)）。
    #[test]
    fn storm_backoff_escalates_and_caps() {
        assert_eq!(Throttle::storm_backoff(1), Duration::from_secs(60));
        assert_eq!(Throttle::storm_backoff(2), Duration::from_secs(120));
        assert_eq!(Throttle::storm_backoff(3), Duration::from_secs(240));
        assert_eq!(Throttle::storm_backoff(99), Duration::from_secs(240), "escalation caps at 240s");
    }

    /// A.15b：冷却等待循环重读直到归零 —— 睡眠窗口内被再布防的冷却必须被捕获；
    /// 每轮只睡剩余量（亚秒精确，绝不 as_secs 截断），等待序列严格递减。
    #[test]
    fn wait_turn_rereads_cooldown_until_past() {
        let sleep_log: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));
        let re_armed = Arc::new(AtomicBool::new(false));
        let slot: Arc<Mutex<Option<Throttle>>> = Arc::new(Mutex::new(None));
        let sleep: SleepHook = {
            let log = sleep_log.clone();
            let slot = slot.clone();
            let re_armed = re_armed.clone();
            Arc::new(move |d: Duration| {
                log.lock().unwrap().push(d);
                // 真实微睡推进时钟（no-op + 未来冷却 + 不走的时钟 = 死循环）
                std::thread::sleep(d.min(Duration::from_millis(100)));
                if !re_armed.swap(true, Ordering::SeqCst) {
                    slot.lock().unwrap().as_ref().unwrap().arm_cooldown(Duration::from_millis(300));
                }
            })
        };
        let cfg = SchedulerConfig { jobs: 1, sleep, ..Default::default() };
        let t = Throttle::new(&cfg);
        t.arm_cooldown(Duration::from_millis(400));
        *slot.lock().unwrap() = Some(t.clone());
        t.wait_turn(0, &|| true);
        let waits = sleep_log.lock().unwrap().clone();
        assert!(waits.len() >= 3, "再布防的冷却必须被循环重读捕获（一次性 sleep 只有 1 条），got {:?}", waits);
        assert!(*waits.first().unwrap() >= Duration::from_millis(400), "首轮必须睡完整剩余量 + 抖动，got {:?}", waits.first());
        assert!(waits.windows(2).all(|w| w[0] > w[1]), "每轮只睡剩余量，序列必须严格递减，got {:?}", waits);
    }

    /// A.17 回归：被减半闸门滞留的 worker 在 Pending 清空后必须放行 ——
    /// 否则活跃 worker 清完片后 run() 在 join 处永久挂死。
    /// cooldown_secs=0：布防即过期，本测试只演闸门，不演冷却。
    #[test]
    fn gate_releases_parked_worker_when_pending_drains() {
        let cfg = SchedulerConfig { jobs: 2, rate_limit_soft_cap: 0, cooldown_secs: 0, ..test_cfg() };
        let t = Throttle::new(&cfg);
        t.on_failure(FailureKind::RateLimited); // total=1 > soft_cap=0 → 减半到 1
        assert_eq!(t.effective_jobs(), 1);
        let start = Instant::now();
        t.wait_turn(1, &|| false); // worker 1 被闸，但 Pending 已空 → 立即放行
        assert!(start.elapsed() < Duration::from_secs(2), "parked worker must be released once work drains (A.17)");
    }

    /// Throttle 是跨 worker 共享的运行时信号：Send+Sync + Clone 共享 internals。
    #[test]
    fn throttle_is_send_sync_and_shared() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Throttle>();
        let t = Throttle::new(&test_cfg());
        let t2 = t.clone();
        t.on_failure(FailureKind::RateLimited);
        assert!(t2.is_cooling(), "clone must share the same internals");
    }
}
