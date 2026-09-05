//! Recovery —— 台账缺失/损坏时的对账重建（自 state.rs 迁入）。
//! 破坏性 git 手术集中于此：fsck 健康门 → 擦除 remote 侧引用与片仓库 →
//! 二次 fsck → 按已存在 refs 重建台账。审计轨迹（eprintln）与不可逆
//! 销毁同处一个 module，便于事后追溯。
//!
//! 调用方：Ledger::open（state 缺失/损坏时）。status 命令严格只读（A.8），
//! 绝不走这里。

use crate::gitio;
use crate::planner::{Piece, Plan};
use crate::state::{pieces_dir, ChainState, PieceState, PieceStatus, State};

/// state.json 缺失/损坏时的对账重建：
/// 先 fsck 验证主仓库完整性；不健康 → 清掉 remote 侧全部引用（origin/tags/rgc）与片仓库
/// （防止"撒谎的 have"），二次 fsck 仍失败 → 损坏在擦除范围之外（对象库本体），
/// 擦除无法治愈它会陷入永久循环，直接放弃并提示删目录重克；
/// 健康 → 按已存在 refs 标 Done，其余 Pending。
pub fn reconcile(plan: &Plan, main: &std::path::Path) -> State {
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
