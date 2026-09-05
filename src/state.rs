use crate::jsonio::{load_json, save_json};
use crate::planner::{Plan, Piece};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PieceStatus {
    Pending,
    Running,
    Done,
    /// 终态：GiveUp（预算内重试耗尽或不可恢复错误）。run 结束统一报告失败片；
    /// rerun 时 run() 把 Failed 折返 Pending —— 认领时 rebill 给全新预算。
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainState {
    pub depth_done: u32,
    pub step: u32,
    pub no_shallow: bool,
}

impl ChainState {
    /// 初始链状态的唯一出处（depth 0 / plan 初始步长 / shallow 可用）。
    pub fn initial(step: u32) -> ChainState {
        ChainState { depth_done: 0, step, no_shallow: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PieceState {
    pub id: String,
    pub status: PieceStatus,
    pub attempts: u32,
    pub bytes: u64,
    /// spec §5：限流不计片的重试预算 —— 连续限流次数独立记账（出现非限流
    /// 失败即复位；rerun 认领时清零，同预算 rebill），超
    /// SchedulerConfig::max_rate_limits 才放弃。serde(default)：
    /// 旧 state.json 无此字段仍可加载。
    #[serde(default)]
    pub rate_limits: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<ChainState>,
}

impl PieceState {
    /// 新片状态的唯一出处：链式片带初始链状态（初始步长来自 plan），
    /// 标签批片无链状态。State::new / recovery::reconcile / Ledger::claim
    /// 的链状态补全都经此构造，不再各处手写字面量。
    pub fn new(piece: &Piece, initial_step: u32) -> PieceState {
        PieceState {
            id: piece.id(),
            status: PieceStatus::Pending,
            attempts: 0,
            bytes: 0,
            rate_limits: 0,
            chain: match piece {
                Piece::Chain { .. } => Some(ChainState::initial(initial_step)),
                Piece::TagBatch { .. } => None,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct State {
    /// Appendix A.2：plan 指纹（url + 有序 piece ids）。与 plan.fingerprint()
    /// 不匹配 = state 来自另一次规划 → 上层必须报错而非静默重规划。
    pub fingerprint: String,
    pub pieces: Vec<PieceState>,
}

pub fn pieces_dir(dir: &Path) -> PathBuf {
    dir.join(".rgc").join("pieces")
}
fn state_path(dir: &Path) -> PathBuf {
    dir.join(".rgc").join("state.json")
}

impl State {
    pub fn new(plan: &Plan) -> State {
        let pieces = plan.pieces.iter().map(|p| PieceState::new(p, plan.initial_step)).collect();
        State { fingerprint: plan.fingerprint(), pieces }
    }

    /// 崩溃恢复：Running 一律折返 Pending（write-ahead：先记账后执行）
    pub fn load(dir: &Path) -> anyhow::Result<Option<State>> {
        match load_json::<State>(&state_path(dir))? {
            None => Ok(None),
            Some(mut s) => {
                for p in &mut s.pieces {
                    if p.status == PieceStatus::Running {
                        p.status = PieceStatus::Pending;
                    }
                }
                Ok(Some(s))
            }
        }
    }

    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        save_json(&state_path(dir), self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::{build_plan, PlannerConfig};
    use crate::refs::{RefEntry, RemoteRefs};

    /// Appendix A.1：build_plan 返回 Result；空远端直接 bail，
    /// 故样例 plan 至少带一个分支（此前正文样例为空远端，已按契约修订）。
    fn sample_plan() -> Plan {
        let refs = RemoteRefs {
            default_branch: Some("main".into()),
            branches: vec![RefEntry {
                full_name: "refs/heads/main".into(),
                short_name: "main".into(),
                oid: "a".into(),
            }],
            tags: vec![],
        };
        build_plan("https://x/y.git", &refs, &PlannerConfig::default()).unwrap()
    }

    #[test]
    fn fresh_piece_state_knows_piece_kind() {
        let plan = sample_plan();
        // 链式片：带初始链状态（初始步长来自 plan）；初始构造的唯一出处
        let ps = PieceState::new(&plan.pieces[0], plan.initial_step);
        let c = ps.chain.expect("chain piece must carry initial chain state");
        assert_eq!(c.depth_done, 0);
        assert_eq!(c.step, plan.initial_step);
        assert!(!c.no_shallow);
        assert_eq!(ps.status, PieceStatus::Pending);
        assert_eq!(ps.id, plan.pieces[0].id());
    }

    #[test]
    fn state_roundtrip_and_crash_recovery() {
        let td = tempfile::tempdir().unwrap();
        let plan = sample_plan();
        let mut st = State::new(&plan);
        st.pieces[0].status = PieceStatus::Running;
        st.save(td.path()).unwrap();
        let loaded = State::load(td.path()).unwrap().unwrap();
        assert_eq!(loaded.pieces[0].status, PieceStatus::Pending);
        // Appendix A.2：state 携带 plan 指纹，serde roundtrip 必须保留
        assert_eq!(loaded.fingerprint, plan.fingerprint());
    }

    #[test]
    fn load_missing_returns_none() {
        let td = tempfile::tempdir().unwrap();
        assert!(State::load(td.path()).unwrap().is_none());
    }

    /// Task 13：rate_limits 是新增字段 —— 旧版 state.json（无此字段）必须仍可
    /// 加载（serde(default)），升级 rgc 后首次 resume 不得反序列化失败。
    #[test]
    fn load_legacy_state_without_rate_limits() {
        let td = tempfile::tempdir().unwrap();
        let plan = sample_plan();
        let legacy = format!(
            r#"{{"fingerprint":"{}","pieces":[{{"id":"chain:refs/heads/main","status":"Pending","attempts":2,"bytes":10}}]}}"#,
            plan.fingerprint()
        );
        std::fs::create_dir_all(td.path().join(".rgc")).unwrap();
        std::fs::write(td.path().join(".rgc/state.json"), legacy).unwrap();
        let st = State::load(td.path()).unwrap().unwrap();
        assert_eq!(st.pieces[0].attempts, 2);
        assert_eq!(st.pieces[0].rate_limits, 0, "missing rate_limits must default to 0");
    }
}
