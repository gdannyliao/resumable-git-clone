use crate::gitio;
use crate::jsonio::{load_json, save_json};
use crate::planner::{Plan, Piece};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PieceStatus {
    Pending,
    Running,
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainState {
    pub depth_done: u32,
    pub step: u32,
    pub no_shallow: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PieceState {
    pub id: String,
    pub status: PieceStatus,
    pub attempts: u32,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<ChainState>,
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
        let pieces = plan
            .pieces
            .iter()
            .map(|p| PieceState {
                id: p.id(),
                status: PieceStatus::Pending,
                attempts: 0,
                bytes: 0,
                chain: match p {
                    Piece::Chain { .. } => Some(ChainState { depth_done: 0, step: plan.initial_step, no_shallow: false }),
                    Piece::TagBatch { .. } => None,
                },
            })
            .collect();
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

/// state.json 缺失/损坏时的对账重建：
/// 先 fsck 验证主仓库完整性；不健康 → 清掉 remotes/wip 引用与片仓库从头来
/// （防止"撒谎的 have"）；健康 → 按已存在 refs 标 Done，其余 Pending。
pub fn reconcile(plan: &Plan, main: &Path) -> State {
    let healthy = gitio::run_git(&["fsck", "--connectivity-only"], Some(main)).is_ok();
    if !healthy {
        if let Ok(out) = gitio::run_git(&["for-each-ref", "--format=%(refname)", "refs/remotes/origin", "refs/rgc/wip"], Some(main)) {
            for r in out.stdout.lines() {
                let _ = gitio::run_git(&["update-ref", "-d", r], Some(main));
            }
        }
        let _ = std::fs::remove_dir_all(pieces_dir(main));
    }
    let pieces = plan
        .pieces
        .iter()
        .map(|p| {
            let done = healthy
                && match p {
                    Piece::Chain { short_name, .. } => gitio::run_git(&["rev-parse", "--verify", &format!("refs/remotes/origin/{}", short_name)], Some(main)).is_ok(),
                    Piece::TagBatch { tags } => tags.iter().all(|t| gitio::run_git(&["rev-parse", "--verify", &t.full_name], Some(main)).is_ok()),
                };
            PieceState {
                id: p.id(),
                status: if done { PieceStatus::Done } else { PieceStatus::Pending },
                attempts: 0,
                bytes: 0,
                chain: if done {
                    None
                } else {
                    match p {
                        Piece::Chain { .. } => Some(ChainState { depth_done: 0, step: plan.initial_step, no_shallow: false }),
                        Piece::TagBatch { .. } => None,
                    }
                },
            }
        })
        .collect();
    State { fingerprint: plan.fingerprint(), pieces }
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
}
