//! Task 15：CLI 流程层集成测试。直接调用 `rgc::cli` 的库 API（不 spawn 二进制），
//! 覆盖 A.8 契约：实例锁先于 load_plan/reconcile、指纹不匹配大声报错绝不静默重规划、
//! status 严格只读。

mod common;
use common::*;

use fs2::FileExt;
use rgc::cli::{acquire_instance_lock, clone_flow, resume_flow, status, CloneOptions};
use rgc::planner::{build_plan, load_plan, plan_path, save_plan, PlannerConfig, Plan};
use rgc::refs::{ls_remote, RefEntry, RemoteRefs};
use rgc::state::{PieceStatus, State};

fn small_opts() -> CloneOptions {
    CloneOptions { jobs: 1, piece_target: 600.0, keep_state: false, initial_step: 10 }
}

/// 合成 plan（不触网）：单分支，url 唯一 → 指纹互异
fn synthetic_plan(url: &str) -> Plan {
    let refs = RemoteRefs {
        default_branch: Some("main".into()),
        branches: vec![RefEntry {
            full_name: "refs/heads/main".into(),
            short_name: "main".into(),
            oid: "0123456789abcdef0123456789abcdef01234567".into(),
        }],
        tags: vec![],
    };
    build_plan(url, &refs, &PlannerConfig::default()).unwrap()
}

/// 端到端：真实 origin → clone_flow 全流程 → finalize 按 keep_state 落地，
/// 产物与 git clone 等价。
#[test]
fn clone_flow_end_to_end() {
    let origin = build_origin(40, &[("dev", 10)], &[("v1", 5)]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();

    // keep_state=false（默认）：finalize 后 .rgc 必须移除
    let dest = td.path().join("r");
    clone_flow(url, &dest, &small_opts()).unwrap();
    assert!(dest.join(".git").exists(), "dest must be a populated git repo");
    assert!(!dest.join(".rgc").exists(), "finalize(keep_state=false) must remove .rgc/");
    // 与 git clone 产物等价（等价 oracle，同源比较）
    let reference = td.path().join("ref");
    git(td.path(), &["clone", "--quiet", url, reference.to_str().unwrap()]);
    let a = rgc::equiv::snapshot(&dest).unwrap();
    let b = rgc::equiv::snapshot(&reference).unwrap();
    rgc::equiv::assert_same_refs(&a, &b).unwrap();
    rgc::equiv::assert_same_objects(&a, &b).unwrap();

    // keep_state=true：.rgc 保留，全部片 Done
    let dest2 = td.path().join("r2");
    clone_flow(url, &dest2, &CloneOptions { keep_state: true, ..small_opts() }).unwrap();
    assert!(dest2.join(".rgc").exists(), "keep_state=true must retain .rgc/");
    let st = State::load(&dest2).unwrap().unwrap();
    assert!(st.pieces.iter().all(|p| p.status == PieceStatus::Done), "all pieces Done after success");
}

/// 中断恢复：.rgc 已有 plan.json + state.json（首片标记 Done）→ flow 必须原样
/// 复用 plan（绝不重规划），跑完剩余片。
#[test]
fn resume_after_interrupt() {
    let origin = build_origin(30, &[("dev", 15)], &[("v1", 8)]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let dest = td.path().join("r");

    // 预置"中断现场"：plan 已存，state 首片 Done
    std::fs::create_dir_all(dest.join(".rgc")).unwrap();
    let plan = build_plan(url, &ls_remote(url).unwrap(), &PlannerConfig { initial_step: 10, ..Default::default() }).unwrap();
    save_plan(&dest, &plan).unwrap();
    let mut st = State::new(&plan);
    st.pieces[0].status = PieceStatus::Done;
    st.save(&dest).unwrap();
    let plan_before = std::fs::read(plan_path(&dest)).unwrap();

    // keep_state=true 以便事后核对 plan.json 原封未动
    resume_flow(&dest, &CloneOptions { keep_state: true, ..small_opts() }).unwrap();

    let plan_after = std::fs::read(plan_path(&dest)).unwrap();
    assert_eq!(plan_before, plan_after, "resume must never rebuild plan.json");
    assert!(dest.join(".git").exists());
    let st = State::load(&dest).unwrap().unwrap();
    assert!(st.pieces.iter().all(|p| p.status == PieceStatus::Done), "resume must complete remaining pieces");
}

/// A.2：plan.json 与 state.json 指纹不一致（state 来自另一 url 的规划）→
/// 大声报错，绝不覆盖 plan.json、绝不静默重规划。
#[test]
fn fingerprint_mismatch_bails() {
    let td = tempfile::tempdir().unwrap();
    let dest = td.path().join("r");
    std::fs::create_dir_all(dest.join(".rgc")).unwrap();
    // 磁盘上是 url A 的 plan；state 却携带 url B 的指纹
    let plan_a = synthetic_plan("https://a.example/x.git");
    let plan_b = synthetic_plan("https://b.example/y.git");
    assert_ne!(plan_a.fingerprint(), plan_b.fingerprint());
    save_plan(&dest, &plan_a).unwrap();
    State::new(&plan_b).save(&dest).unwrap();
    let before = std::fs::read(plan_path(&dest)).unwrap();

    let err = clone_flow(&plan_a.url, &dest, &small_opts()).unwrap_err();
    let msg = format!("{:#}", err);
    assert!(msg.contains("plan/state mismatch"), "expected fingerprint bail, got: {msg}");

    let after = std::fs::read(plan_path(&dest)).unwrap();
    assert_eq!(before, after, "plan.json must not be overwritten");
}

/// 指向已属于另一个远端的 dest（plan.json 的 url 与请求 url 不同）→ 报错，
/// 不覆盖 plan.json（绝不悄悄改克隆别的远端）。
#[test]
fn clone_into_foreign_dest_bails() {
    let td = tempfile::tempdir().unwrap();
    let dest = td.path().join("r");
    std::fs::create_dir_all(dest.join(".rgc")).unwrap();
    let plan_a = synthetic_plan("https://a.example/x.git");
    save_plan(&dest, &plan_a).unwrap();
    State::new(&plan_a).save(&dest).unwrap();
    let before = std::fs::read(plan_path(&dest)).unwrap();

    let err = clone_flow("https://b.example/y.git", &dest, &small_opts()).unwrap_err();
    let msg = format!("{:#}", err);
    assert!(msg.contains("different URL"), "expected url-conflict bail, got: {msg}");
    assert_eq!(before, std::fs::read(plan_path(&dest)).unwrap(), "plan.json must not be overwritten");
}

/// A.4/A.8：实例锁冲突 → "another rgc instance"；且锁先于规划取得
/// （被拒绝的调用不得在 dest 留下 plan.json / .git）。释放后可再次取得。
#[test]
fn instance_lock_rejects_second() {
    let origin = build_origin(10, &[], &[]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let dest = td.path().join("r");
    std::fs::create_dir_all(dest.join(".rgc")).unwrap();

    // 测试进程自己持有排他锁（flock 对同进程不同 fd 同样互斥）
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dest.join(".rgc").join("lock"))
        .unwrap();
    lock_file.try_lock_exclusive().unwrap();

    let err = clone_flow(url, &dest, &small_opts()).unwrap_err();
    let msg = format!("{:#}", err);
    assert!(msg.contains("another rgc instance"), "expected lock bail, got: {msg}");
    // 锁先于 load_plan/reconcile：被拒绝的调用零副作用
    assert!(!plan_path(&dest).exists(), "lock must precede planning");
    assert!(!dest.join(".git").exists(), "lock must precede any git mutation");

    drop(lock_file); // 释放 → 可重新取得（guard drop 语义）
    let _guard = acquire_instance_lock(&dest).unwrap();
}

/// 回归：相对 dest（`rgc clone <url>` 缺省目录、或显式相对路径）必须成功。
/// 修复前：gitio 把相对 piece 路径嵌进 `git fetch` argv，git 以 main 为 cwd
/// 解析成 `main/piece` 嵌套路径 → transport 永远失败，重试退避伪装成挂死。
/// 通过真实二进制 + 独立 cwd 驱动（不污染测试进程的全局 cwd）。
#[test]
fn clone_with_relative_dest_dir() {
    let origin = build_origin(5, &[], &[]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_rgc"))
        .args(["clone", url, "rel"])
        .current_dir(&td)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "relative-dest clone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(td.path().join("rel").join(".git").exists());
    // 缺省目录名：URL 末段去 .git
    let out2 = std::process::Command::new(env!("CARGO_BIN_EXE_rgc"))
        .args(["clone", url])
        .current_dir(&td)
        .output()
        .unwrap();
    assert!(
        out2.status.success(),
        "default-dir clone failed: {}",
        String::from_utf8_lossy(&out2.stderr)
    );
    assert!(td.path().join("origin.git".trim_end_matches(".git")).join(".git").exists());
}

/// A.8：status 严格只读 —— 不改写 state.json（内容与 mtime 均不变）；
/// state 缺失/损坏 → 提示 resume 重建且不创建文件；plan 缺失 → 报错。
#[test]
fn status_read_only() {
    let td = tempfile::tempdir().unwrap();
    let dest = td.path().join("r");
    std::fs::create_dir_all(dest.join(".rgc")).unwrap();
    let plan = synthetic_plan("https://a.example/x.git");
    save_plan(&dest, &plan).unwrap();
    // 一片 Done、一片 Failed、其余 Pending
    let mut st = State::new(&plan);
    st.pieces[0].status = PieceStatus::Done;
    if st.pieces.len() > 1 {
        st.pieces[1].status = PieceStatus::Failed;
    }
    st.save(&dest).unwrap();
    let state_file = dest.join(".rgc").join("state.json");
    let before_bytes = std::fs::read(&state_file).unwrap();
    let before_mtime = std::fs::metadata(&state_file).unwrap().modified().unwrap();

    status(&dest).unwrap();

    assert_eq!(before_bytes, std::fs::read(&state_file).unwrap(), "status must not rewrite state.json");
    assert_eq!(before_mtime, std::fs::metadata(&state_file).unwrap().modified().unwrap(), "status must not touch state.json mtime");

    // 损坏 state → 视同缺失：提示、不写任何文件
    std::fs::write(&state_file, "{ not json").unwrap();
    status(&dest).unwrap();
    assert_eq!(std::fs::read(&state_file).unwrap(), b"{ not json", "status must not repair in place");

    // 缺失 state → 仍 Ok，且绝不创建 state.json（绝不 reconcile）
    std::fs::remove_file(&state_file).unwrap();
    status(&dest).unwrap();
    assert!(!state_file.exists(), "status must not create state.json");

    // plan 缺失 → 报错
    assert!(status(&td.path().join("nope")).is_err());
    // load_plan 对完好 plan 仍可读（sanity：上面的 status 走的是真 plan）
    assert!(load_plan(&dest).unwrap().is_some());
}
