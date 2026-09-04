//! Ledger —— 台账（state.json）write-ahead 协议的 mutating interface。
//! 数据类型与序列化在 [`crate::state`]；本模块独占协议本身：
//! open（载入 + 指纹守卫 + 对账回退 + 崩溃/终态折返 + 首次落盘）、
//! claim（锁内 rebill + Running 落盘，先记账后执行）、snapshot/complete
//! （锁内克隆出私有副本，执行不持锁，完成后锁内整体回写 + 落盘）。
//!
//! write-ahead 不变式：片开始执行前状态先落盘（Running），崩溃后
//! `State::load` 把 Running 折返 Pending；执行结束后再落盘终态。
//! 保存天然串行化：全部状态变更与落盘都在内部 Mutex 锁内完成（A.15c）。
//!
//! 只读路径（status 命令）不走本模块 —— A.8：status 严格只读，
//! 用 `State::load` 直接读，绝不 reconcile。

use crate::planner::Plan;
use crate::state::{PieceState, PieceStatus, State};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// 台账的 mutating interface。内部持有 `Arc<Mutex<State>>` + 主仓库路径；
/// worker 经 scope 借用 `&Ledger` 跨线程共享（Send+Sync 由 SchedulerConfig
/// 断言套件覆盖调度侧）。
pub struct Ledger {
    dir: PathBuf,
    state: Arc<Mutex<State>>,
}

impl Ledger {
    /// 启动协议单点：载入 → 指纹守卫（A.2：不匹配大声报错，绝不静默重规划）
    /// → 缺失/损坏则对账重建 → Failed 折返 Pending（C1：rerun 重试，认领时
    /// rebill 全新预算；Running 折返已在 State::load 内完成）→ 首次落盘。
    /// 调用方须先 init_main_repo：reconcile 的 fsck 要在合法仓库上跑。
    pub fn open(dir: &Path, plan: &Plan) -> Result<Ledger> {
        let mut st = match State::load(dir)? {
            // A.2：指纹不匹配 = state 来自另一次规划 → 绝不静默重规划
            Some(s) if s.fingerprint == plan.fingerprint() => s,
            Some(_) => bail!("plan/state mismatch — delete .rgc/ or restore plan.json"),
            // state 缺失/损坏（load_json 把损坏视同缺失）→ 对账重建
            None => crate::state::reconcile(plan, dir),
        };
        // C1：Failed 是上一轮的终态 —— rerun 折返 Pending 重新出发
        for p in &mut st.pieces {
            if p.status == PieceStatus::Failed {
                p.status = PieceStatus::Pending;
            }
        }
        st.save(dir)?;
        Ok(Ledger { dir: dir.to_path_buf(), state: Arc::new(Mutex::new(st)) })
    }

    /// write-ahead 认领（A.15c）：状态变更 + 落盘在锁内完成 —— Running 先落盘
    /// 再执行。C2：认领时给耗尽预算的片重新计费（attempts 只约束单次 run 内的
    /// 重试，rerun 给每个片全新预算）；限流计数同哲学，认领时清零。
    /// 返回 None = 已无 Pending 片。
    pub fn claim(&self, max_attempts: u32) -> Result<Option<(usize, String)>> {
        let mut g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let Some(idx) = g.pieces.iter().position(|p| p.status == PieceStatus::Pending) else {
            return Ok(None);
        };
        if g.pieces[idx].attempts > max_attempts {
            g.pieces[idx].attempts = 0;
        }
        g.pieces[idx].rate_limits = 0;
        g.pieces[idx].status = PieceStatus::Running;
        g.save(&self.dir)?;
        Ok(Some((idx, g.pieces[idx].id.clone())))
    }

    /// 锁内克隆出片的私有副本：执行（可能数分钟）全程不持锁。
    pub fn snapshot(&self, idx: usize) -> PieceState {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).pieces[idx].clone()
    }

    /// 执行完成后整体回写 + 落盘（片认领后无人再动它的槽位，idx 稳定有效；
    /// 即使执行期间他人多次落盘，重放顺序也不影响各片字段的归属）。
    pub fn complete(&self, idx: usize, ps: PieceState) -> Result<()> {
        let mut g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        g.pieces[idx] = ps;
        g.save(&self.dir)
    }

    /// 锁内快照：是否还有 Pending 片（worker 与 Throttle 闸门的探测）。
    pub fn has_pending(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pieces
            .iter()
            .any(|p| p.status == PieceStatus::Pending)
    }

    /// 历史最大片字节数（调度器磁盘预检的估算基数）。
    pub fn max_piece_bytes(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pieces
            .iter()
            .map(|p| p.bytes)
            .max()
            .unwrap_or(0)
    }

    /// run 收尾报告：所有非 Done 片的 id。
    pub fn unfinished_ids(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pieces
            .iter()
            .filter(|p| p.status != PieceStatus::Done)
            .map(|p| p.id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonio::load_json;
    use crate::planner::{build_plan, PlannerConfig};
    use crate::refs::{RefEntry, RemoteRefs};
    use crate::state::{PieceStatus, State};

    /// 样例 plan：两分支一 tag（空远端在 build_plan 即报错，A.1 契约）
    fn sample_plan() -> crate::planner::Plan {
        let refs = RemoteRefs {
            default_branch: Some("main".into()),
            branches: vec![
                RefEntry { full_name: "refs/heads/main".into(), short_name: "main".into(), oid: "a".into() },
                RefEntry { full_name: "refs/heads/dev".into(), short_name: "dev".into(), oid: "b".into() },
            ],
            tags: vec![],
        };
        build_plan("https://x/y.git", &refs, &PlannerConfig::default()).unwrap()
    }

    /// 在 dir 里放一个合法空主仓库（open 的 reconcile 回退需要 fsck 能跑）
    fn init_main(dir: &std::path::Path) {
        crate::gitio::init_main_repo(dir, "https://x/y.git").unwrap();
    }

    /// 手写一份 state.json 落盘（绕过 Ledger，模拟历史现场）
    fn plant_state(dir: &std::path::Path, st: &State) {
        st.save(dir).unwrap();
    }

    #[test]
    fn open_fresh_reconciles_to_all_pending() {
        let td = tempfile::tempdir().unwrap();
        let main = td.path().join("repo");
        init_main(&main);
        let plan = sample_plan();
        let ledger = Ledger::open(&main, &plan).unwrap();
        assert!(ledger.has_pending());
        assert_eq!(ledger.unfinished_ids().len(), plan.pieces.len());
        // open 必须落盘（首次写入 state.json）
        let on_disk = State::load(&main).unwrap().unwrap();
        assert!(on_disk.pieces.iter().all(|p| p.status == PieceStatus::Pending));
        assert_eq!(on_disk.fingerprint, plan.fingerprint());
    }

    #[test]
    fn open_folds_crashed_running_and_terminal_failed() {
        let td = tempfile::tempdir().unwrap();
        let main = td.path().join("repo");
        init_main(&main);
        let plan = sample_plan();
        let mut st = State::new(&plan);
        st.pieces[0].status = PieceStatus::Running; // 崩溃现场
        st.pieces[1].status = PieceStatus::Failed; // 上一轮终态
        plant_state(&main, &st);
        let ledger = Ledger::open(&main, &plan).unwrap();
        let on_disk = State::load(&main).unwrap().unwrap();
        assert!(
            on_disk.pieces.iter().all(|p| p.status == PieceStatus::Pending),
            "Running（崩溃折返）与 Failed（rerun 折返）都必须回到 Pending"
        );
        assert!(ledger.has_pending());
    }

    #[test]
    fn open_bails_on_fingerprint_mismatch() {
        let td = tempfile::tempdir().unwrap();
        let main = td.path().join("repo");
        init_main(&main);
        let plan = sample_plan();
        let mut st = State::new(&plan);
        st.fingerprint = "0000000000000000".into();
        plant_state(&main, &st);
        let err = match Ledger::open(&main, &plan) {
            Ok(_) => panic!("fingerprint mismatch must bail, not open"),
            Err(e) => e,
        };
        assert!(
            format!("{:#}", err).contains("plan/state mismatch"),
            "A.2：指纹不匹配必须大声报错，绝不静默重规划: {err:#}"
        );
    }

    #[test]
    fn claim_is_write_ahead_and_rebills_budget() {
        let td = tempfile::tempdir().unwrap();
        let main = td.path().join("repo");
        init_main(&main);
        let plan = sample_plan();
        let mut st = State::new(&plan);
        // 预算耗尽的片：认领时必须 rebill（attempts>max 清零、rate_limits 清零）
        st.pieces[0].attempts = 99;
        st.pieces[0].rate_limits = 7;
        plant_state(&main, &st);
        let ledger = Ledger::open(&main, &plan).unwrap();
        let (idx, id) = ledger.claim(5).unwrap().expect("one piece claimable");
        assert_eq!(idx, 0);
        assert_eq!(id, plan.pieces[0].id());
        let ps = ledger.snapshot(idx);
        assert_eq!(ps.status, PieceStatus::Running);
        assert_eq!(ps.attempts, 0, "exhausted budget must be rebilled at claim (C2)");
        assert_eq!(ps.rate_limits, 0, "consecutive rate-limit count resets at claim");
        // write-ahead：磁盘上必须先见 Running（绕过 State::load 的折返，直读原文）
        let raw: State = load_json(&main.join(".rgc/state.json")).unwrap().unwrap();
        assert_eq!(raw.pieces[0].status, PieceStatus::Running, "Running must hit disk before execution");
    }

    #[test]
    fn claim_drains_to_none() {
        let td = tempfile::tempdir().unwrap();
        let main = td.path().join("repo");
        init_main(&main);
        let plan = sample_plan();
        let ledger = Ledger::open(&main, &plan).unwrap();
        assert!(ledger.claim(5).unwrap().is_some());
        assert!(ledger.claim(5).unwrap().is_some());
        assert!(ledger.claim(5).unwrap().is_none(), "no Pending left");
        assert!(!ledger.has_pending());
    }

    #[test]
    fn complete_writes_back_and_persists() {
        let td = tempfile::tempdir().unwrap();
        let main = td.path().join("repo");
        init_main(&main);
        let plan = sample_plan();
        let ledger = Ledger::open(&main, &plan).unwrap();
        let (idx, _) = ledger.claim(5).unwrap().unwrap();
        let mut ps = ledger.snapshot(idx);
        ps.status = PieceStatus::Done;
        ps.bytes = 4096;
        ledger.complete(idx, ps).unwrap();
        let on_disk = State::load(&main).unwrap().unwrap();
        assert_eq!(on_disk.pieces[idx].status, PieceStatus::Done);
        assert_eq!(on_disk.pieces[idx].bytes, 4096);
        assert_eq!(ledger.max_piece_bytes(), 4096);
        assert_eq!(ledger.unfinished_ids(), vec![plan.pieces[1].id()]);
    }
}
