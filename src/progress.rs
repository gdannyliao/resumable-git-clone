//! 进度呈现 —— clone/resume 运行期的三种用户可见信息：
//! 进度（片完成数 + 已下载字节）、下载速度（窗口速度 + 全程平均）、
//! 恢复提示（重跑时报告从哪续）。全部为纯函数/小结构体，
//! 调度器与 cli 的接线处不做任何格式化逻辑（便于单测）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

/// 人类可读字节数：二进制单位；<100 保留一位小数，>=100 取整。
pub fn human_bytes(n: u64) -> String {
    if n < 1024 {
        return format!("{n} B");
    }
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v >= 100.0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// 人类可读速度（字节/秒）。
pub fn human_speed(bytes_per_sec: f64) -> String {
    let bps = if bytes_per_sec.is_finite() && bytes_per_sec > 0.0 { bytes_per_sec as u64 } else { 0 };
    format!("{}/s", human_bytes(bps))
}

/// 人类可读时长：59s / 3m12s / 2h05m。
pub fn human_duration(secs: f64) -> String {
    let s = if secs.is_finite() && secs > 0.0 { secs as u64 } else { 0 };
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// 渲染一行监视状态：
/// `rgc: 3/10 pieces done, 1.2 GiB, 5.4 MiB/s (avg 4.0 MiB/s)`
/// cur=None（本轮尚未测到任何字节）→ `measuring…` 占位，绝不显示误导的 0。
pub fn render_line(done: usize, total: usize, bytes: u64, cur: Option<f64>, avg: f64) -> String {
    match cur {
        None => format!("rgc: {done}/{total} pieces done, {}, measuring…", human_bytes(bytes)),
        Some(c) => format!(
            "rgc: {done}/{total} pieces done, {}, {} (avg {})",
            human_bytes(bytes),
            human_speed(c),
            human_speed(avg)
        ),
    }
}

/// 重跑恢复提示（可能两行：总览 + 续跑明细）。
/// state 必须是与本 plan 指纹匹配的（调用方负责核对，不匹配不提示）。
pub fn resume_hint(state: &crate::state::State) -> String {
    use crate::state::PieceStatus;
    let total = state.pieces.len();
    let done = state.pieces.iter().filter(|p| p.status == PieceStatus::Done).count();
    let bytes: u64 = state.pieces.iter().map(|p| p.bytes).sum();
    if done == total {
        return format!("rgc: all {total} pieces already done ({} kept) — finalizing", human_bytes(bytes));
    }
    let rest: Vec<&crate::state::PieceState> = state.pieces.iter().filter(|p| p.status != PieceStatus::Done).collect();
    let shown: Vec<String> = rest
        .iter()
        .take(3)
        .map(|p| {
            let mut d = p.id.clone();
            if let Some(c) = &p.chain {
                if c.depth_done > 0 {
                    d.push_str(&format!(" (depth {})", c.depth_done));
                }
            }
            if p.status == PieceStatus::Failed {
                d.push_str(" (will retry)");
            }
            d
        })
        .collect();
    let mut line = format!("rgc: continuing: {}", shown.join(", "));
    if rest.len() > 3 {
        line.push_str(&format!(", +{} more", rest.len() - 3));
    }
    format!("rgc: resuming — {done}/{total} pieces already done ({} kept)\n{line}", human_bytes(bytes))
}

/// ls-remote 完成后的远端摘要行：
/// `rgc: remote: 1 branch, 12 tags → 3 pieces`（单复数正确）。
pub fn remote_summary(remote: &crate::refs::RemoteRefs, pieces: usize) -> String {
    fn n(count: usize, singular: &str, plural: &str) -> String {
        if count == 1 {
            format!("1 {singular}")
        } else {
            format!("{count} {plural}")
        }
    }
    format!(
        "rgc: remote: {}, {} → {}",
        n(remote.branches.len(), "branch", "branches"),
        n(remote.tags.len(), "tag", "tags"),
        n(pieces, "piece", "pieces")
    )
}

/// 跨 worker 共享的运行期进度：每片一个"在途字节"槽位 —— worker 每完成
/// 一步 fetch 累加，`Ledger::complete` 落盘后清零（字节已并入台账持久计数，
/// 清零避免与台账重复计数）。监视线程据此算出"已下载总量"的实时值。
pub struct Progress {
    started: Instant,
    in_flight: Vec<AtomicU64>,
}

impl Progress {
    pub fn new(pieces: usize) -> Progress {
        Progress { started: Instant::now(), in_flight: (0..pieces).map(|_| AtomicU64::new(0)).collect() }
    }

    pub fn add(&self, idx: usize, bytes: u64) {
        if let Some(slot) = self.in_flight.get(idx) {
            slot.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    pub fn clear(&self, idx: usize) {
        if let Some(slot) = self.in_flight.get(idx) {
            slot.store(0, Ordering::Relaxed);
        }
    }

    pub fn in_flight_bytes(&self) -> u64 {
        self.in_flight.iter().map(|s| s.load(Ordering::Relaxed)).sum()
    }

    pub fn elapsed(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::{build_plan, PlannerConfig};
    use crate::refs::{RefEntry, RemoteRefs};
    use crate::state::{PieceStatus, State};

    #[test]
    fn human_bytes_scales_and_rounds() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(120 * 1024), "120 KiB"); // >=100 取整
        assert_eq!(human_bytes(1_258_291), "1.2 MiB");
        assert_eq!(human_bytes(1_288_490_188), "1.2 GiB");
    }

    #[test]
    fn human_speed_formats_rate() {
        assert_eq!(human_speed(0.0), "0 B/s");
        assert_eq!(human_speed(5_662_310.4), "5.4 MiB/s");
        assert_eq!(human_speed(300.0), "300 B/s");
    }

    #[test]
    fn human_duration_compact() {
        assert_eq!(human_duration(12.3), "12s");
        assert_eq!(human_duration(192.0), "3m12s");
        assert_eq!(human_duration(7500.0), "2h05m");
    }

    #[test]
    fn render_line_with_and_without_measurement() {
        assert_eq!(
            render_line(0, 10, 0, None, 0.0),
            "rgc: 0/10 pieces done, 0 B, measuring…"
        );
        assert_eq!(
            render_line(3, 10, 1_288_490_188, Some(5_662_310.4), 4_194_304.0),
            "rgc: 3/10 pieces done, 1.2 GiB, 5.4 MiB/s (avg 4.0 MiB/s)"
        );
    }

    /// 样例 state：两分支一 tag 批（三个片）
    fn sample_state() -> State {
        let refs = RemoteRefs {
            default_branch: Some("main".into()),
            branches: vec![
                RefEntry { full_name: "refs/heads/main".into(), short_name: "main".into(), oid: "a".into() },
                RefEntry { full_name: "refs/heads/dev".into(), short_name: "dev".into(), oid: "b".into() },
            ],
            tags: vec![RefEntry { full_name: "refs/tags/v1".into(), short_name: "v1".into(), oid: "c".into() }],
        };
        let plan = build_plan("https://x/y.git", &refs, &PlannerConfig::default()).unwrap();
        State::new(&plan)
    }

    #[test]
    fn resume_hint_reports_done_bytes_and_continuation() {
        let mut st = sample_state();
        // 3 片：main Done，dev 中途（depth 45000），tags 上轮 Failed
        st.pieces[0].status = PieceStatus::Done;
        st.pieces[0].bytes = 1_288_490_188;
        st.pieces[1].chain.as_mut().unwrap().depth_done = 45_000;
        st.pieces[1].bytes = 104_857_600;
        st.pieces[2].status = PieceStatus::Failed;
        let hint = resume_hint(&st);
        let lines: Vec<&str> = hint.lines().collect();
        assert_eq!(
            lines[0],
            "rgc: resuming — 1/3 pieces already done (1.3 GiB kept)",
            "总览行：完成数 + 全部已下载字节（含中途片）"
        );
        assert_eq!(
            lines[1],
            "rgc: continuing: chain:refs/heads/dev (depth 45000), tags:v1 (will retry)",
            "续跑明细：链片带 depth，Failed 片标注重试"
        );
    }

    #[test]
    fn resume_hint_all_done() {
        let mut st = sample_state();
        for p in &mut st.pieces {
            p.status = PieceStatus::Done;
        }
        st.pieces[0].bytes = 1_048_576;
        let hint = resume_hint(&st);
        assert_eq!(hint, "rgc: all 3 pieces already done (1.0 MiB kept) — finalizing");
    }

    #[test]
    fn resume_hint_truncates_long_continuation_list() {
        let refs = RemoteRefs {
            default_branch: Some("main".into()),
            branches: (0..6)
                .map(|i| RefEntry {
                    full_name: format!("refs/heads/b{i}"),
                    short_name: format!("b{i}"),
                    oid: format!("{i:040}"),
                })
                .collect(),
            tags: vec![],
        };
        let plan = build_plan("https://x/y.git", &refs, &PlannerConfig::default()).unwrap();
        let st = State::new(&plan); // 6 链片全 Pending
        let hint = resume_hint(&st);
        let last = hint.lines().last().unwrap();
        assert!(last.contains("+3 more"), "超过 3 个待跑片必须折叠: {last}");
        assert!(!last.contains("chain:refs/heads/b4"), "折叠的片不出现在明细里: {last}");
    }

    #[test]
    fn remote_summary_plurals() {
        let remote = |b: usize, t: usize| RemoteRefs {
            default_branch: None,
            branches: (0..b)
                .map(|i| RefEntry { full_name: format!("refs/heads/b{i}"), short_name: format!("b{i}"), oid: format!("{i:040}") })
                .collect(),
            tags: (0..t)
                .map(|i| RefEntry { full_name: format!("refs/tags/t{i}"), short_name: format!("t{i}"), oid: format!("{i:040}") })
                .collect(),
        };
        assert_eq!(remote_summary(&remote(1, 12), 3), "rgc: remote: 1 branch, 12 tags → 3 pieces");
        assert_eq!(remote_summary(&remote(2, 1), 3), "rgc: remote: 2 branches, 1 tag → 3 pieces");
        assert_eq!(remote_summary(&remote(1, 0), 1), "rgc: remote: 1 branch, 0 tags → 1 piece");
    }

    #[test]
    fn progress_in_flight_accounting() {
        let p = Progress::new(3);
        assert_eq!(p.in_flight_bytes(), 0);
        p.add(1, 100);
        p.add(1, 50);
        p.add(2, 7);
        assert_eq!(p.in_flight_bytes(), 157);
        p.clear(1);
        assert_eq!(p.in_flight_bytes(), 7);
        assert!(p.elapsed() >= 0.0);
    }
}
