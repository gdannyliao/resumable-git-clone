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
                rate_limits: 0,
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
/// 先 fsck 验证主仓库完整性；不健康 → 清掉 remote 侧全部引用（origin/tags/rgc）与片仓库
/// （防止"撒谎的 have"），二次 fsck 仍失败 → 损坏在擦除范围之外（对象库本体），
/// 擦除无法治愈它会陷入永久循环，直接放弃并提示删目录重克；
/// 健康 → 按已存在 refs 标 Done，其余 Pending。
pub fn reconcile(plan: &Plan, main: &Path) -> State {
    // fsck 在大仓库上可能要几分钟，先打招呼避免用户误以为挂死
    eprintln!("verifying repository integrity, this can take minutes on large repos...");
    let healthy = gitio::run_git(&["fsck", "--connectivity-only"], Some(main)).is_ok();
    if !healthy {
        // 审计轨迹：不可逆销毁前必须留诊断，便于事后追溯
        eprintln!("warning: fsck failed — wiping all remote-side refs (origin/tags/rgc) and piece repos, then re-verifying");
        if let Ok(out) = gitio::run_git(&["for-each-ref", "--format=%(refname)", "refs/remotes/origin", "refs/tags", "refs/rgc"], Some(main)) {
            for r in out.stdout.lines() {
                let _ = gitio::run_git(&["update-ref", "-d", r], Some(main));
            }
        }
        let _ = std::fs::remove_dir_all(pieces_dir(main));
        // 二次 fsck：仍失败说明损坏在擦除范围之外（refs/tags 或不可达对象），
        // 继续跑只会每轮擦除-重拉-再失败，永久循环；直接放弃，让用户删目录重克。
        gitio::run_git(&["fsck", "--connectivity-only"], Some(main))
            .expect("fatal: repository unrecoverable — delete the directory and re-clone");
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
                rate_limits: 0,
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
