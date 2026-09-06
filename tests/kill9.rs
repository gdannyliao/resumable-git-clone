//! Task 16：kill -9 韧性验收（spec §6 验收标准：任意时刻杀掉进程，resume 后
//! 产物必须与 `git clone` 完全等价）。
//!
//! 确定性设计（章程给定两案中的"轮询 state.json"案，不用计时猜测）：
//!
//! 1. 被杀的是真实二进制（`CARGO_BIN_EXE_rgc`），对本地裸 origin 跑
//!    `clone --jobs 1 --piece-target 2 --initial-step 100`（1200 commit 主干
//!    ⇒ 多片多 fetch，整个 clone 需数秒，远长于轮询周期）。
//! 2. write-ahead 协议保证"先记账后执行"：claim 把片置 Running 并落盘之后
//!    才开始该片的 git 工作。测试进程以 15ms 间隔轮询 `<dest>/.rgc/state.json`
//!    的**原始 JSON**（必须绕过 `State::load` —— 它会把 Running 折返 Pending，
//!    探不到在跑片），首次观察到 Running 片立即 SIGKILL。此刻调度器从构造上
//!    必然处于某片 claim 与 settle 之间 ⇒ kill 落在 clone 中途，而非碰运气。
//! 3. 失败模式全部大声化：10s 内未见 Running → panic；kill 前进程已自行
//!    成功退出（fixture/参数漂移使 clone 快于轮询）→ panic；kill 未送达
//!    （signal ≠ 9）→ panic。绝不静默放过假通过。
//! 4. kill 后断言现场：state.json 可完整解析（save_json 的 pid-tmp + rename
//!    原子写 ⇒ 任意时刻被杀都不撕裂）、Done 片记账在案；随后
//!    `resume --keep-state` 跑完剩余片，断言：plan.json 字节不变（绝不重规划）、
//!    既有 Done 片原样保留（bytes/attempts 未动 = 未重做）、全部片 Done、
//!    工作区已 checkout；最后以等价 oracle（refs + 全对象图）对照新鲜
//!    `git clone`。

mod common;
use common::*;

use std::path::Path;
use std::time::{Duration, Instant};

/// 轮询原始 state.json，直到出现"至少一片 Done 且有一片 Running"的中途现场
/// （write-ahead 认领痕迹），返回观察到的 Done 片数。必须读原始 JSON：
/// `State::load` 会把 Running 折返 Pending，看不见在跑片。
/// 只等首个 Running 就杀会落在第 0 片执行途中 —— Done 集为空，"保留已完片"
/// 断言沦为空真；等"Done≥1 且 Running"则每次运行都同时命中部分进度与在途杀。
fn wait_for_partial_progress(state_file: &Path, max_wait: Duration) -> usize {
    let start = Instant::now();
    while start.elapsed() < max_wait {
        if let Ok(s) = std::fs::read_to_string(state_file) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                if let Some(pieces) = v["pieces"].as_array() {
                    let done = pieces.iter().filter(|p| p["status"] == "Done").count();
                    let running = pieces.iter().any(|p| p["status"] == "Running");
                    if done >= 1 && running {
                        return done;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    panic!("state.json never showed partial progress (Done>=1 && Running) within the wait budget");
}

#[test]
#[ignore = "slow end-to-end (builds a 1200-commit origin, ~1-3 min); run with `cargo test -- --ignored`"]
fn kill_and_resume_equals_git_clone() {
    let origin = build_origin(1200, &[("dev", 600)], &[("v1", 300)]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let target = td.path().join("repo");
    let state_file = target.join(".rgc").join("state.json");

    // —— 第一轮：spawn 真实 clone，首次出现 Running 认领即 SIGKILL ——
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_rgc"))
        .args([
            "clone",
            url,
            target.to_str().unwrap(),
            "--jobs",
            "1",
            "--piece-target",
            "2",
            "--initial-step",
            "100",
        ])
        .current_dir(&td)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait_for_partial_progress(&state_file, Duration::from_secs(10));
    child.kill().expect("SIGKILL/TerminateProcess failed");
    let status = child.wait().unwrap();
    assert!(
        !status.success(),
        "clone exited successfully before the kill landed — fixture/args no longer guarantee a mid-flight kill"
    );
    // kill 送达断言分平台：Unix 必须见 SIGKILL；Windows 无信号，
    // Child::kill = TerminateProcess(handle, 1) → 退出码 1。
    #[cfg(unix)]
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(9),
        "expected SIGKILL delivery, got {status:?}"
    );
    #[cfg(windows)]
    assert_eq!(
        status.code(),
        Some(1),
        "expected TerminateProcess exit code 1, got {status:?}"
    );

    // —— 被杀现场的断言：state 永不撕裂（原子写），Done 记账在案 ——
    let state = rgc::state::State::load(&target)
        .expect("state.json must parse after SIGKILL (pid-tmp + rename atomic write)")
        .expect("state.json must exist");
    let plan_file = target.join(".rgc").join("plan.json");
    let plan_before = std::fs::read(&plan_file).unwrap();
    let done_before: Vec<(String, u64, u32)> = state
        .pieces
        .iter()
        .filter(|p| p.status == rgc::state::PieceStatus::Done)
        .map(|p| (p.id.clone(), p.bytes, p.attempts))
        .collect();
    assert!(
        !done_before.is_empty(),
        "kill must land after at least one piece settled — partial-progress precondition violated"
    );
    assert_eq!(state.fingerprint, {
        // 指纹与磁盘 plan 一致（load_plan 校验在 resume 时也会再做一遍）
        let plan = rgc::planner::load_plan(&target).unwrap().expect("plan.json readable");
        plan.fingerprint()
    });

    // —— 第二轮：resume 跑完剩余片（--keep-state 以便核对 plan.json 未动）——
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_rgc"))
        .args(["resume", target.to_str().unwrap(), "--keep-state"])
        .current_dir(&td)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "resume after kill -9 failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // plan.json 字节不变：resume 绝不重规划
    assert_eq!(plan_before, std::fs::read(&plan_file).unwrap(), "resume must never rebuild plan.json");

    // 全部片 Done；被杀前已 Done 的片原样保留（bytes/attempts 未动 = 未重做）
    let state = rgc::state::State::load(&target).unwrap().unwrap();
    assert!(
        state.pieces.iter().all(|p| p.status == rgc::state::PieceStatus::Done),
        "all pieces must be Done after resume"
    );
    for (id, bytes, attempts) in &done_before {
        let p = state.pieces.iter().find(|p| &p.id == id).unwrap();
        assert_eq!(p.status, rgc::state::PieceStatus::Done);
        assert_eq!(p.bytes, *bytes, "piece {id}: bytes changed — Done piece was redone");
        assert_eq!(p.attempts, *attempts, "piece {id}: attempts changed — Done piece was redone");
    }

    // 工作区已 checkout（build_origin 的 f0.txt 在主干 tip 上必然存在）
    assert!(target.join("f0.txt").exists(), "worktree must be checked out");

    // —— 验收标准：与新鲜 `git clone` 完全等价（refs + 全对象图）——
    let reference = td.path().join("ref");
    git(td.path(), &["clone", "--quiet", url, reference.to_str().unwrap()]);
    let a = rgc::equiv::snapshot(&target).unwrap();
    let b = rgc::equiv::snapshot(&reference).unwrap();
    rgc::equiv::assert_same_refs(&a, &b).unwrap();
    rgc::equiv::assert_same_objects(&a, &b).unwrap();
}
