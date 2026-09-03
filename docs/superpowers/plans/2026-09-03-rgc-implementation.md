# rgc（Resumable Git Clone）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 构建一个 Rust CLI `rgc`，把超大仓库的 git clone 切成独立、幂等的小分片（分支 = shallow/deepen 步进链，tags = 批量片），实现分片级断点续传，产物与 `git clone` 完全等价。

**Architecture:** 编排 `git` 子进程（不碰传输协议）。主仓库 + 独立片仓库（`--shared` 起步、复制主仓库 refs 作 have）隔离并发；每片成功即搬运进主仓库；`state.json`/`plan.json` 落盘为断点真相源；Finalizer 收尾 fetch + 校验。

**Tech Stack:** Rust stable、clap 4（derive）、serde/serde_json、anyhow、indicatif、fs2；dev: tempfile。git ≥ 2.26 子进程。

**Spec:** `docs/superpowers/specs/2026-09-03-resumable-git-clone-design.md`（必读，特别是 §4 关键机制与 §5 错误处理）。

---

## 文件结构

```
Cargo.toml
.gitignore
README.md
.github/workflows/ci.yml
src/
  lib.rs          # 模块声明（每个任务追加一行）
  main.rs         # CLI 入口（Task 15 前 为 stub）
  jsonio.rs       # 原子 JSON 落盘（tmp + rename），plan/state 共用
  errors.rs       # 失败分类（RateLimited/Network/ShallowUnsupported/Fatal）
  gitio.rs        # git 子进程封装 + 全部 git 操作原语
  refs.rs         # ls-remote 解析与远程 refs 获取
  planner.rs      # 切片计划 + 自适应步长
  state.rs        # 断点状态、崩溃恢复、对账重建
  scheduler.rs    # 状态机调度：claim/执行/重试/退避/降档
  finalizer.rs    # 收尾 fetch、布局规整、校验、清理
  equiv.rs        # 等价性校验器（测试 oracle，也供人工核对）
tests/
  common/mod.rs   # 造仓库 fixture
  refs_gitio.rs   # ls_remote 集成
  reconcile.rs    # 对账重建
  chain.rs        # deepen 链原语
  tags.rs         # tags 批量片
  equiv_test.rs   # 校验器
  finalizer.rs    # 收尾布局
  scheduler_e2e.rs    # 串行端到端 vs git clone
  retry.rs        # 重试预算重置
  parallel_e2e.rs # 并行端到端
  cli.rs          # 二进制端到端
  kill9.rs        # 杀进程恢复端到端（ignored，慢）
```

任务顺序即依赖顺序：1→2→3→4→5→(6 jsonio+planner)→(7 state)→(8 chain)→(9 tags)→(10 equiv)→(11 finalizer)→(12 scheduler 串行)→(13 重试)→(14 并行)→(15 CLI)→(16 kill9+CI+README)。

关键约束（所有任务都要遵守）：
- **have 诚实性**：链进行中的增量只进主仓库 `refs/rgc/wip/<branch>`，链完成才写 `refs/remotes/origin/<branch>` 并删 wip——中途 tip ref 一旦被别的片当 have，服务端会按"拥有完整祖先"误判而静默丢历史。
- **片仓库 refs 复制**：`--shared` 只共享对象不共享 refs，have 协商依据本地 refs，所以初始化后必须把主仓库 `refs/remotes/origin/*`、`refs/tags/*` 复制过来。
- **write-ahead**：先置 Running 落盘，再执行；Done 只在成功后写。Ctrl-C 不需要信号处理器（子进程随进程组死，Running 在 load 时折返 Pending）。
- **挂死策略（显式决策）**：`run_git` 层设 `GIT_TERMINAL_PROMPT=0`（杜绝凭据提示挂死）与 `LC_ALL=C`（classify 依赖英文 stderr）。通用 wall-clock 看门狗属调度层增强，MVP 明确不做：worker 挂死即进程挂死，用户 Ctrl-C 后 resume 即兜底（断点状态使其无损）。

---

### Task 1: 项目脚手架与依赖

**Files:**
- Create: `Cargo.toml`
- Create: `.gitignore`
- Create: `src/lib.rs`
- Create: `src/main.rs`

- [ ] **Step 1: 创建 Cargo.toml**

```toml
[package]
name = "rgc"
version = "0.1.0"
edition = "2021"
description = "Resumable git clone for very large repositories"

[dependencies]
anyhow = "1"
clap = { version = "4", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
indicatif = "0.17"
fs2 = "0.4"

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: 创建 .gitignore**

```
/target
```

- [ ] **Step 3: 创建 src/lib.rs（冒烟测试）**

```rust
#[cfg(test)]
mod scaffold_tests {
    #[test]
    fn it_builds() {
        assert!(true);
    }
}
```

- [ ] **Step 4: 创建 src/main.rs（stub，Task 15 替换）**

```rust
fn main() {
    println!("rgc scaffold");
}
```

- [ ] **Step 5: 编译并跑测试**

Run: `cargo test -q`
Expected: `test result: ok. 1 passed`

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml .gitignore src/
git commit -m "chore: scaffold rgc crate"
```

---

### Task 2: 错误分类模块

**Files:**
- Create: `src/errors.rs`
- Modify: `src/lib.rs`（追加 `pub mod errors;`）

- [ ] **Step 1: lib.rs 追加模块声明**

在 `src/lib.rs` 顶部加：

```rust
pub mod errors;
```

- [ ] **Step 2: 写 src/errors.rs（含失败测试）**

```rust
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    RateLimited,
    Congestion,
    Network,
    ShallowUnsupported,
    Fatal,
}

#[derive(Debug)]
pub enum RgcError {
    RateLimited(String),
    Congestion(String),
    Network(String),
    ShallowUnsupported(String),
    Fatal(String),
}

impl RgcError {
    pub fn kind(&self) -> FailureKind {
        match self {
            RgcError::RateLimited(_) => FailureKind::RateLimited,
            RgcError::Congestion(_) => FailureKind::Congestion,
            RgcError::Network(_) => FailureKind::Network,
            RgcError::ShallowUnsupported(_) => FailureKind::ShallowUnsupported,
            RgcError::Fatal(_) => FailureKind::Fatal,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            RgcError::RateLimited(m)
            | RgcError::Congestion(m)
            | RgcError::Network(m)
            | RgcError::ShallowUnsupported(m)
            | RgcError::Fatal(m) => m,
        }
    }

    /// Task 4 的 run_git 用这个构造，避免 match 重复
    pub fn from_kind(kind: FailureKind, msg: String) -> Self {
        match kind {
            FailureKind::RateLimited => RgcError::RateLimited(msg),
            FailureKind::Congestion => RgcError::Congestion(msg),
            FailureKind::Network => RgcError::Network(msg),
            FailureKind::ShallowUnsupported => RgcError::ShallowUnsupported(msg),
            FailureKind::Fatal => RgcError::Fatal(msg),
        }
    }
}

impl fmt::Display for RgcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{:?}] {}", self.kind(), self.message())
    }
}

impl std::error::Error for RgcError {}

/// 从 anyhow::Error 提取失败类别（无法识别 → Fatal）
pub fn kind_of(err: &anyhow::Error) -> FailureKind {
    err.downcast_ref::<RgcError>().map(|e| e.kind()).unwrap_or(FailureKind::Fatal)
}

/// 按 git stderr 关键词分类。
/// RateLimited/Congestion 触发全局降速（不计片重试预算）；Network 计预算可退避重试；
/// ShallowUnsupported 触发"退化整支 fetch"；Fatal 放弃。
/// 关键词锚定真实 git/curl 输出（勿用臆造字符串）。
pub fn classify(stderr: &str) -> FailureKind {
    let s = stderr.to_lowercase();
    if ["http 429", "returned error: 429", "error: 429"].iter().any(|k| s.contains(k))
        || s.contains("rate limit")
        || s.contains("secondary rate")
        || s.contains("abuse")
    {
        FailureKind::RateLimited
    } else if s.contains("connection reset") {
        FailureKind::Congestion
    } else if [
        "could not resolve host",
        "connection refused",
        "the remote end hung up",
        "could not read from remote repository",
        "timed out",
        "early eof",
        "rpc failed",
        "unable to access",
        "temporary failure",
    ]
    .iter()
    .any(|k| s.contains(k))
    {
        FailureKind::Network
    } else if s.contains("does not support shallow") || (s.contains("shallow") && s.contains("dumb http")) {
        FailureKind::ShallowUnsupported
    } else {
        FailureKind::Fatal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // —— 真实 git/curl 输出（从 git 2.50 二进制/源码锚定） ——

    #[test]
    fn classify_network_error() {
        assert_eq!(
            classify("fatal: unable to access 'https://github.com/x/y/': Could not resolve host: github.com"),
            FailureKind::Network
        );
        assert_eq!(classify("fatal: the remote end hung up unexpectedly"), FailureKind::Network);
        assert_eq!(classify("fatal: Could not read from remote repository."), FailureKind::Network);
        assert_eq!(classify("error: RPC failed; curl 56 OpenSSL SSL_read: error"), FailureKind::Network);
    }

    #[test]
    fn classify_rate_limit() {
        assert_eq!(classify("error: RPC failed; The requested URL returned error: 429"), FailureKind::RateLimited);
        assert_eq!(classify("remote: abuse detection triggered"), FailureKind::RateLimited);
        // 进度计数里的 "1429" 不得误判
        assert_eq!(
            classify("remote: Enumerating objects: 1429, done.\nfatal: the remote end hung up unexpectedly"),
            FailureKind::Network
        );
        // 优先级：429 与网络关键词同时出现 → RateLimited
        assert_eq!(
            classify("fatal: unable to access 'https://x/': The requested URL returned error: 429"),
            FailureKind::RateLimited
        );
    }

    #[test]
    fn classify_congestion() {
        assert_eq!(classify("error: RPC failed; curl 56 Recv failure: Connection reset by peer"), FailureKind::Congestion);
    }

    #[test]
    fn classify_shallow_unsupported() {
        assert_eq!(classify("fatal: Server does not support shallow clients"), FailureKind::ShallowUnsupported);
        assert_eq!(classify("fatal: Server does not support shallow requests"), FailureKind::ShallowUnsupported);
        assert_eq!(classify("dumb http transport does not support shallow capabilities"), FailureKind::ShallowUnsupported);
    }

    #[test]
    fn classify_fatal_by_default() {
        assert_eq!(classify("fatal: Authentication failed"), FailureKind::Fatal);
    }

    #[test]
    fn kind_of_wraps_through_anyhow() {
        let e: anyhow::Error = RgcError::Network("boom".into()).into();
        assert_eq!(kind_of(&e), FailureKind::Network);
    }

    #[test]
    fn kind_of_unknown_is_fatal() {
        let e = anyhow::anyhow!("disk low");
        assert_eq!(kind_of(&e), FailureKind::Fatal);
    }

    #[test]
    fn from_kind_roundtrips() {
        for (k, msg) in [
            (FailureKind::RateLimited, "a"),
            (FailureKind::Congestion, "b"),
            (FailureKind::Network, "c"),
            (FailureKind::ShallowUnsupported, "d"),
            (FailureKind::Fatal, "e"),
        ] {
            assert_eq!(RgcError::from_kind(k, msg.into()).kind(), k);
        }
    }
}
```

- [ ] **Step 3: 跑测试**

Run: `cargo test -q errors`
Expected: `test result: ok. 8 passed`

- [ ] **Step 4: Commit**

```bash
git add src/errors.rs src/lib.rs
git commit -m "feat: failure classification (rate-limit/network/shallow/fatal)"
```

---

### Task 3: 测试 fixture（造仓库）

**Files:**
- Create: `tests/common/mod.rs`

- [ ] **Step 1: 写 tests/common/mod.rs**

```rust
#![allow(dead_code)]
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git").args(args).current_dir(dir).status().unwrap();
    assert!(st.success(), "git {:?} failed in {:?}", args, dir);
}

/// 构建裸仓库 origin：<total> 个 commit 的 main；
/// branches 在第 at 个 commit 处创建，tags 打在第 at 个 commit。
/// 返回 origin 裸仓库路径（临时目录泄漏到测试进程结束，可接受）。
pub fn build_origin(total: u32, branches: &[(&str, u32)], tags: &[(&str, u32)]) -> PathBuf {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().to_path_buf();
    std::mem::forget(td);
    let origin = root.join("origin.git");
    git(&root, &["init", "--quiet", "--bare", "-b", "main", origin.to_str().unwrap()]);
    let work = root.join("work");
    git(&root, &["clone", "--quiet", origin.to_str().unwrap(), work.to_str().unwrap()]);
    git(&work, &["config", "user.email", "t@t"]);
    git(&work, &["config", "user.name", "t"]);
    for i in 0..total {
        std::fs::write(work.join(format!("f{}.txt", i % 7)), format!("content {}", i)).unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "--quiet", "-m", &format!("c{}", i)]);
        for (name, at) in branches {
            if *at == i + 1 {
                git(&work, &["branch", name]);
            }
        }
        for (name, at) in tags {
            if *at == i + 1 {
                git(&work, &["tag", name]);
            }
        }
    }
    git(&work, &["push", "--quiet", "origin", "--all"]);
    git(&work, &["push", "--quiet", "origin", "--tags"]);
    origin
}
```

- [ ] **Step 2: 验证可编译**

Run: `cargo test -q --no-run`
Expected: 编译通过（common 尚无使用者，允许 dead_code）

- [ ] **Step 3: Commit**

```bash
git add tests/
git commit -m "test: repo fixture builder"
```

---

### Task 4: gitio 基础（run_git / init_main_repo / main_has_refs）

**Files:**
- Create: `src/gitio.rs`
- Modify: `src/lib.rs`（追加 `pub mod gitio;`）

- [ ] **Step 1: lib.rs 追加 `pub mod gitio;`**

- [ ] **Step 2: 写 src/gitio.rs（先只放本任务的原语，含测试）**

```rust
use crate::errors::{classify, RgcError};
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;

pub struct GitOutput {
    pub stdout: String,
    pub stderr: String,
}

/// 运行 git 子进程；非零退出按 stderr 分类映射为 RgcError。
pub fn run_git(args: &[&str], cwd: Option<&Path>) -> Result<GitOutput> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd.output().with_context(|| format!("failed to spawn git {:?}", args))?;
    let res = GitOutput {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    };
    if !out.status.success() {
        let msg = format!("git {:?} failed: {}", args, res.stderr.trim_end());
        return Err(RgcError::from_kind(classify(&res.stderr), msg).into());
    }
    Ok(res)
}

/// 主仓库是否已有任何 refs（决定片仓库能否 --shared 起步）
pub fn main_has_refs(main: &Path) -> bool {
    match run_git(&["for-each-ref", "--count=1", "refs/remotes/origin", "refs/tags", "refs/heads"], Some(main)) {
        Ok(o) => !o.stdout.trim().is_empty(),
        Err(_) => false,
    }
}

pub fn init_main_repo(main: &Path, url: &str) -> Result<()> {
    std::fs::create_dir_all(main)?;
    if !main.join(".git").exists() {
        run_git(&["init", "--quiet", main.to_str().unwrap()], None)?;
    }
    if run_git(&["remote", "get-url", "origin"], Some(main)).is_err() {
        run_git(&["remote", "add", "origin", url], Some(main))?;
    }
    // clone 期间禁用主仓库 auto-gc：片仓库经 alternates 借用的对象不可被回收
    run_git(&["config", "gc.auto", "0"], Some(main))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_main_repo_idempotent() {
        let td = tempfile::tempdir().unwrap();
        let main = td.path().join("r");
        init_main_repo(&main, "https://example.com/x.git").unwrap();
        init_main_repo(&main, "https://example.com/x.git").unwrap();
        assert!(main.join(".git").exists());
        let out = run_git(&["remote", "get-url", "origin"], Some(&main)).unwrap();
        assert_eq!(out.stdout.trim(), "https://example.com/x.git");
        assert!(!main_has_refs(&main));
    }
}
```

- [ ] **Step 3: 跑测试**

Run: `cargo test -q gitio`
Expected: `test result: ok. 1 passed`

- [ ] **Step 4: Commit**

```bash
git add src/gitio.rs src/lib.rs
git commit -m "feat: git subprocess wrapper and main repo init"
```

---

### Task 5: refs 解析与 ls-remote

**Files:**
- Create: `src/refs.rs`
- Modify: `src/lib.rs`（追加 `pub mod refs;`）
- Create: `tests/refs_gitio.rs`

- [ ] **Step 1: lib.rs 追加 `pub mod refs;`**

- [ ] **Step 2: 写 src/refs.rs**

```rust
use crate::gitio::run_git;
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefEntry {
    pub full_name: String,
    pub short_name: String,
    pub oid: String,
}

#[derive(Debug, Clone)]
pub struct RemoteRefs {
    pub default_branch: Option<String>,
    pub branches: Vec<RefEntry>,
    pub tags: Vec<RefEntry>,
}

/// 剥离单一命名空间前缀。不用 trim_start_matches：避免 "refs/heads/refs/heads/x"
/// 这类字面名字被连环剥皮。
fn short_name(full: &str) -> String {
    match full.strip_prefix("refs/heads/") {
        Some(s) => s.to_string(),
        None => full.strip_prefix("refs/tags/").unwrap_or(full).to_string(),
    }
}

pub fn parse_ls_remote(output: &str) -> RemoteRefs {
    let mut default_branch = None;
    let mut branches = Vec::new();
    let mut tags = Vec::new();
    for line in output.lines() {
        // --symref 对每个符号引用输出一行 "ref: <target>\t<refname>"。
        // 只有 refname == HEAD 的那一行定义默认分支（远端可能广播其他 symref）；
        // 且 target 必须是分支，否则留给 finalize 的 fallback 逻辑。
        if let Some(rest) = line.strip_prefix("ref: ") {
            if let Some((target, refname)) = rest.split_once('\t') {
                if refname.trim() == "HEAD" {
                    if let Some(name) = target.trim().strip_prefix("refs/heads/") {
                        default_branch = Some(name.to_string());
                    }
                }
            }
            continue;
        }
        let Some((oid, refname)) = line.split_once('\t') else { continue };
        let refname = refname.trim();
        if refname.ends_with("^{}") {
            continue; // annotated tag 的 peeled 行：抓 tag 本身即可
        }
        match refname {
            "HEAD" => {}
            n if n.starts_with("refs/heads/") => branches.push(RefEntry {
                full_name: n.to_string(),
                short_name: short_name(n),
                oid: oid.trim().to_string(),
            }),
            n if n.starts_with("refs/tags/") => tags.push(RefEntry {
                full_name: n.to_string(),
                short_name: short_name(n),
                oid: oid.trim().to_string(),
            }),
            _ => {} // refs/pull/* 等一律不拉
        }
    }
    RemoteRefs { default_branch, branches, tags }
}

pub fn ls_remote(url: &str) -> Result<RemoteRefs> {
    let out = run_git(&["ls-remote", "--symref", url], None)?;
    Ok(parse_ls_remote(&out.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "ref: refs/heads/main\tHEAD\nabc111\tHEAD\nabc111\trefs/heads/main\ndef222\trefs/heads/dev\n789aaa\trefs/tags/v1\nbbb222\trefs/tags/v1^{}\nccc333\trefs/pull/1/head\n";

    #[test]
    fn parse_ls_remote_basic() {
        let r = parse_ls_remote(SAMPLE);
        assert_eq!(r.default_branch.as_deref(), Some("main"));
        assert_eq!(r.branches.len(), 2);
        assert_eq!(r.branches[1].short_name, "dev");
        assert_eq!(r.tags.len(), 1);
        assert_eq!(r.tags[0].oid, "789aaa");
    }

    #[test]
    fn second_symref_does_not_override_head() {
        // 真实形状：HEAD 的 symref 行之后，还有其他符号引用的行
        let out = "ref: refs/heads/main\tHEAD\n7b8e\tHEAD\nref: refs/heads/b2\trefs/heads/alias\n7b8e\trefs/heads/alias\n7b8e\trefs/heads/main\n";
        let r = parse_ls_remote(out);
        assert_eq!(r.default_branch.as_deref(), Some("main"), "HEAD 的 symref 必须获胜");
        assert_eq!(r.branches.len(), 2);
    }

    #[test]
    fn head_symref_to_non_branch_leaves_default_none() {
        let out = "ref: refs/tags/v1\tHEAD\nabc\tHEAD\nabc\trefs/heads/main\n";
        let r = parse_ls_remote(out);
        assert_eq!(r.default_branch, None);
        assert_eq!(r.branches.len(), 1);
    }

    #[test]
    fn repeated_prefix_names_kept_literal() {
        let out = "abc\trefs/heads/refs/heads/x\ndef\trefs/tags/refs/tags/y\n";
        let r = parse_ls_remote(out);
        assert_eq!(r.branches[0].short_name, "refs/heads/x");
        assert_eq!(r.tags[0].short_name, "refs/tags/y");
    }

    #[test]
    fn branch_names_with_spaces_parse() {
        let out = "abc\trefs/heads/feature with space\n";
        let r = parse_ls_remote(out);
        assert_eq!(r.branches[0].short_name, "feature with space");
    }

    #[test]
    fn crlf_output_parses() {
        let out = "ref: refs/heads/main\tHEAD\r\nabc111\trefs/heads/main\r\n";
        let r = parse_ls_remote(out);
        assert_eq!(r.default_branch.as_deref(), Some("main"));
        assert_eq!(r.branches.len(), 1);
        assert_eq!(r.branches[0].oid, "abc111");
    }
}
```

- [ ] **Step 3: 写 tests/refs_gitio.rs**

```rust
mod common;
use common::*;

#[test]
fn ls_remote_reads_fixture() {
    let origin = build_origin(5, &[("dev", 3)], &[("v1", 2)]);
    let r = rgc::refs::ls_remote(origin.to_str().unwrap()).unwrap();
    assert_eq!(r.default_branch.as_deref(), Some("main"));
    assert_eq!(r.branches.len(), 2);
    assert_eq!(r.tags.len(), 1);
}
```

- [ ] **Step 4: 跑测试**

Run: `cargo test -q refs`
Expected: 单元 1 passed + 集成 1 passed

- [ ] **Step 5: Commit**

```bash
git add src/refs.rs src/lib.rs tests/refs_gitio.rs
git commit -m "feat: ls-remote parsing and remote refs"
```

---

### Task 6: jsonio + planner（切片计划与自适应步长）

**Files:**
- Create: `src/jsonio.rs`
- Create: `src/planner.rs`
- Modify: `src/lib.rs`（追加 `pub mod jsonio;` 与 `pub mod planner;`）

- [ ] **Step 1: lib.rs 追加模块声明**

```rust
pub mod jsonio;
pub mod planner;
```

- [ ] **Step 2: 写 src/jsonio.rs**

```rust
use serde::{de::DeserializeOwned, Serialize};
use std::path::Path;

/// 原子写：tmp + rename，防写坏
pub fn save_json<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(value)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 读取；损坏视同缺失（由调用方决定对账重建）
pub fn load_json<T: DeserializeOwned>(path: &Path) -> anyhow::Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)?;
    match serde_json::from_str(&text) {
        Ok(v) => Ok(Some(v)),
        Err(e) => {
            eprintln!("warning: {}: {} — treating as absent", path.display(), e);
            Ok(None)
        }
    }
}
```

- [ ] **Step 3: 写 src/planner.rs**

```rust
use crate::jsonio::{load_json, save_json};
use crate::refs::{RefEntry, RemoteRefs};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const INITIAL_STEP: u32 = 10_000;
pub const MIN_STEP: u32 = 100;

#[derive(Debug, Clone)]
pub struct PlannerConfig {
    pub tags_per_batch: usize,
    pub initial_step: u32,
}
impl Default for PlannerConfig {
    fn default() -> Self {
        Self { tags_per_batch: 32, initial_step: INITIAL_STEP }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Piece {
    Chain { full_ref: String, short_name: String },
    TagBatch { tags: Vec<RefEntry> },
}

impl Piece {
    pub fn id(&self) -> String {
        match self {
            Piece::Chain { full_ref, .. } => format!("chain:{}", full_ref),
            Piece::TagBatch { tags } => format!("tags:{}", tags[0].short_name),
        }
    }
    pub fn piece_dir_name(&self) -> String {
        self.id().replace(['/', ':'], "_")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub url: String,
    pub default_branch: Option<String>,
    pub initial_step: u32,
    pub pieces: Vec<Piece>,
}

pub fn build_plan(url: &str, refs: &RemoteRefs, cfg: &PlannerConfig) -> Plan {
    let mut pieces: Vec<Piece> = refs
        .branches
        .iter()
        .map(|b| Piece::Chain { full_ref: b.full_name.clone(), short_name: b.short_name.clone() })
        .collect();
    for batch in refs.tags.chunks(cfg.tags_per_batch) {
        pieces.push(Piece::TagBatch { tags: batch.to_vec() });
    }
    // 主干链排最前（串行关键路径尽早开始），其余分支按 ls-remote 顺序，tags 最后
    if let Some(db) = &refs.default_branch {
        pieces.sort_by_key(|p| match p {
            Piece::Chain { short_name, .. } if short_name == db => 0u8,
            Piece::Chain { .. } => 1,
            Piece::TagBatch { .. } => 2,
        });
    }
    Plan { url: url.to_string(), default_branch: refs.default_branch.clone(), initial_step: cfg.initial_step, pieces }
}

pub fn plan_path(dir: &Path) -> PathBuf {
    dir.join(".rgc").join("plan.json")
}
pub fn save_plan(dir: &Path, plan: &Plan) -> Result<()> {
    save_json(&plan_path(dir), plan)
}
pub fn load_plan(dir: &Path) -> Result<Option<Plan>> {
    load_json(&plan_path(dir))
}

#[derive(Debug, Clone, Copy)]
pub struct StepMeasurement {
    pub bytes: u64,
    pub secs: f64,
    pub commits: u32,
}

/// 自适应步长：按上一片实测吞吐与字节密度，让下一片接近 target_secs 秒
pub fn next_step(m: &StepMeasurement, target_secs: f64) -> u32 {
    if m.bytes == 0 || m.secs <= 0.0 || m.commits == 0 {
        return INITIAL_STEP;
    }
    let density = m.bytes as f64 / m.commits as f64; // 字节/commit
    let throughput = m.bytes as f64 / m.secs;        // 字节/秒
    let n = (throughput * target_secs / density).round() as u32;
    n.clamp(MIN_STEP, 5_000_000)
}

/// 失败降档：步长减半
pub fn halve_step(step: u32) -> u32 {
    (step / 2).max(MIN_STEP)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_plan_orders_default_first() {
        let refs = RemoteRefs {
            default_branch: Some("main".into()),
            branches: vec![
                RefEntry { full_name: "refs/heads/zlib".into(), short_name: "zlib".into(), oid: "a".into() },
                RefEntry { full_name: "refs/heads/main".into(), short_name: "main".into(), oid: "b".into() },
            ],
            tags: vec![RefEntry { full_name: "refs/tags/v1".into(), short_name: "v1".into(), oid: "c".into() }],
        };
        let plan = build_plan("https://x/y.git", &refs, &PlannerConfig::default());
        assert!(matches!(&plan.pieces[0], Piece::Chain { short_name, .. } if short_name == "main"));
        assert!(matches!(plan.pieces.last().unwrap(), Piece::TagBatch { .. }));
        assert_eq!(plan.initial_step, INITIAL_STEP);
    }

    #[test]
    fn build_plan_batches_tags() {
        let tags: Vec<RefEntry> = (0..5)
            .map(|i| RefEntry { full_name: format!("refs/tags/t{}", i), short_name: format!("t{}", i), oid: "o".into() })
            .collect();
        let refs = RemoteRefs { default_branch: None, branches: vec![], tags };
        let plan = build_plan("u", &refs, &PlannerConfig { tags_per_batch: 2, ..Default::default() });
        assert_eq!(plan.pieces.len(), 3);
        assert_eq!(plan.pieces[0].piece_dir_name(), "tags_t0");
    }

    #[test]
    fn next_step_adapts() {
        // 1000B/s 吞吐 × 100B/commit 密度 × 600s 目标 → 6000 commits
        let m = StepMeasurement { bytes: 1000, secs: 1.0, commits: 10 };
        assert_eq!(next_step(&m, 600.0), 6000);
        assert_eq!(next_step(&StepMeasurement { bytes: 0, secs: 1.0, commits: 10 }, 600.0), INITIAL_STEP);
    }

    #[test]
    fn halve_step_floors_at_min() {
        assert_eq!(halve_step(1000), 500);
        assert_eq!(halve_step(100), 100);
    }

    #[test]
    fn plan_roundtrip() {
        let td = tempfile::tempdir().unwrap();
        let refs = RemoteRefs { default_branch: Some("main".into()), branches: vec![], tags: vec![] };
        let plan = build_plan("https://x/y.git", &refs, &PlannerConfig::default());
        save_plan(td.path(), &plan).unwrap();
        let loaded = load_plan(td.path()).unwrap().unwrap();
        assert_eq!(loaded.url, plan.url);
        assert_eq!(loaded.pieces.len(), plan.pieces.len());
    }
}
```

- [ ] **Step 4: 跑测试**

Run: `cargo test -q planner`
Expected: `test result: ok. 5 passed`

- [ ] **Step 5: Commit**

```bash
git add src/jsonio.rs src/planner.rs src/lib.rs
git commit -m "feat: slice planner with adaptive deepen step"
```

---

### Task 7: state（断点状态 + 崩溃恢复 + 对账重建）

**Files:**
- Create: `src/state.rs`
- Modify: `src/lib.rs`（追加 `pub mod state;`）
- Create: `tests/reconcile.rs`

- [ ] **Step 1: lib.rs 追加 `pub mod state;`**

- [ ] **Step 2: 写 src/state.rs**

```rust
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
        State { pieces }
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
    State { pieces }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::{build_plan, PlannerConfig};
    use crate::refs::RemoteRefs;

    fn sample_plan() -> Plan {
        let refs = RemoteRefs { default_branch: Some("main".into()), branches: vec![], tags: vec![] };
        build_plan("https://x/y.git", &refs, &PlannerConfig::default())
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
    }

    #[test]
    fn load_missing_returns_none() {
        let td = tempfile::tempdir().unwrap();
        assert!(State::load(td.path()).unwrap().is_none());
    }
}
```

- [ ] **Step 3: 写 tests/reconcile.rs**

```rust
mod common;
use common::*;
use rgc::planner::{build_plan, PlannerConfig};
use rgc::refs::ls_remote;
use rgc::state::{reconcile, PieceStatus};

#[test]
fn reconcile_marks_completed_refs_done() {
    let origin = build_origin(30, &[("dev", 10)], &[]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    rgc::gitio::init_main_repo(&main, url).unwrap();
    // 手工伪造一个已完成的分支
    rgc::gitio::run_git(&["fetch", "--quiet", url, "+refs/heads/dev:refs/remotes/origin/dev"], Some(&main)).unwrap();
    let plan = build_plan(url, &ls_remote(url).unwrap(), &PlannerConfig::default());
    let st = reconcile(&plan, &main);
    let dev = st.pieces.iter().find(|p| p.id.contains("dev")).unwrap();
    assert_eq!(dev.status, PieceStatus::Done);
    assert_eq!(st.pieces.iter().filter(|p| p.status == PieceStatus::Done).count(), 1);
}

#[test]
fn reconcile_wipes_refs_when_repo_unhealthy() {
    let origin = build_origin(10, &[], &[]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    rgc::gitio::init_main_repo(&main, url).unwrap();
    // 指向不存在对象的假 ref → fsck 失败
    rgc::gitio::run_git(&["update-ref", "refs/remotes/origin/main", "0123456789012345678901234567890123456789"], Some(&main)).unwrap();
    let plan = build_plan(url, &ls_remote(url).unwrap(), &PlannerConfig::default());
    let st = reconcile(&plan, &main);
    assert!(st.pieces.iter().all(|p| p.status == PieceStatus::Pending));
    let refs = rgc::gitio::run_git(&["for-each-ref", "refs/remotes/origin"], Some(&main)).unwrap();
    assert!(refs.stdout.trim().is_empty());
}
```

- [ ] **Step 4: 跑测试**

Run: `cargo test -q state && cargo test -q --test reconcile`
Expected: 全部通过（单测 2 + 集成 2）

- [ ] **Step 5: Commit**

```bash
git add src/state.rs src/lib.rs tests/reconcile.rs
git commit -m "feat: resumable state, crash recovery and reconcile"
```

---

### Task 8: gitio 链式片原语（片仓库 / deepen 步进 / 搬运）

**Files:**
- Modify: `src/gitio.rs`（追加原语）
- Create: `tests/chain.rs`

- [ ] **Step 1: 在 src/gitio.rs 追加（放在 init_main_repo 之后、#[cfg(test)] 之前）**

```rust
/// 确保片仓库存在且健康；损坏则重建。
/// 初始化：主仓库有 refs → `clone --shared`（对象经 alternates 共享）；
/// 否则普通 init。随后把主仓库 refs/remotes/origin/* 与 refs/tags/*
/// 复制为片仓库本地同名 refs —— have 协商依据本地 refs，仅 alternates 不够。
pub fn ensure_piece_repo(main: &Path, piece: &Path, url: &str) -> Result<()> {
    if piece.join(".git").exists() {
        if run_git(&["rev-parse", "--git-dir"], Some(piece)).is_ok() {
            return Ok(());
        }
        std::fs::remove_dir_all(piece)?; // 损坏 → 重建
    }
    std::fs::create_dir_all(piece)?;
    let main_s = main.to_string_lossy().into_owned();
    let piece_s = piece.to_string_lossy().into_owned();
    if main_has_refs(main) {
        run_git(&["clone", "--shared", "--no-checkout", "--quiet", &main_s, &piece_s], None)?;
    } else {
        run_git(&["init", "--quiet", &piece_s], None)?;
    }
    if run_git(&["remote", "set-url", "origin", url], Some(piece)).is_err() {
        run_git(&["remote", "add", "origin", url], Some(piece))?;
    }
    if main_has_refs(main) {
        run_git(
            &["fetch", "--quiet", &main_s, "+refs/remotes/origin/*:refs/remotes/origin/*", "+refs/tags/*:refs/tags/*"],
            Some(piece),
        )?;
    }
    run_git(&["config", "gc.auto", "0"], Some(piece))?;
    Ok(())
}

/// shallow 边界是否仍在（文件缺失或空 = 历史已完整）
pub fn is_shallow(repo: &Path) -> bool {
    match std::fs::metadata(repo.join(".git").join("shallow")) {
        Ok(m) => m.len() > 0,
        Err(_) => false,
    }
}

fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                total += dir_size(&e.path());
            } else {
                total += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    total
}

/// (对象库字节, 指定分支可达 commit 数) —— 用于步长自适应测量
pub fn repo_stats(repo: &Path, short: &str) -> Result<(u64, u32)> {
    let commits = match run_git(&["rev-list", "--count", &format!("refs/heads/{}", short)], Some(repo)) {
        Ok(o) => o.stdout.trim().parse().unwrap_or(0),
        Err(_) => 0,
    };
    Ok((dir_size(&repo.join(".git")), commits))
}

/// 链式片执行一步。三种模式：
/// - no_shallow：服务端不支持 shallow 时的退化，整支 fetch；
/// - 已有本地分支且仍 shallow：--deepen=<step>；
/// - 无本地分支：--depth=<depth_done + step>
///   （首片 depth_done=0 即 --depth=step；崩溃恢复重建的片 depth_done>0，
///   借 --shared 已有对象去重，网络代价 ≈ 增量）。
pub fn fetch_chain_step(piece: &Path, full_ref: &str, short: &str, step: u32, depth_done: u32, no_shallow: bool) -> Result<()> {
    let refspec = format!("+{}:refs/heads/{}", full_ref, short);
    if no_shallow {
        return run_git(&["fetch", "--quiet", "origin", &refspec], Some(piece)).map(|_| ());
    }
    let has_local = run_git(&["rev-parse", "--verify", &format!("refs/heads/{}", short)], Some(piece)).is_ok();
    if has_local && is_shallow(piece) {
        let d = format!("--deepen={}", step);
        run_git(&["fetch", "--quiet", &d, "origin", &refspec], Some(piece))?;
    } else if !has_local {
        let d = format!("--depth={}", depth_done + step);
        run_git(&["fetch", "--quiet", &d, "origin", &refspec], Some(piece))?;
    }
    // has_local && !shallow → 已完整，无需操作
    Ok(())
}

/// 搬运片仓库成果进主仓库。final_dst=false 写 refs/rgc/wip/<short>（链进行中，
/// 防止中途 tip 被其他片当 have 导致"撒谎的 have"丢历史）；final_dst=true 写
/// refs/remotes/origin/<short> 并删除 wip 引用。
pub fn transport_to_main(main: &Path, piece: &Path, full_ref: &str, short: &str, final_dst: bool) -> Result<()> {
    let dst = if final_dst { format!("refs/remotes/origin/{}", short) } else { format!("refs/rgc/wip/{}", short) };
    let refspec = format!("+{}:{}", full_ref, dst);
    run_git(&["fetch", "--quiet", piece.to_string_lossy().as_ref(), &refspec], Some(main))?;
    if final_dst {
        let _ = run_git(&["update-ref", "-d", &format!("refs/rgc/wip/{}", short)], Some(main));
    }
    Ok(())
}
```

- [ ] **Step 2: 写 tests/chain.rs**

```rust
mod common;
use common::*;
use rgc::gitio::*;

#[test]
fn chain_steps_fetch_full_history_and_transport() {
    let origin = build_origin(50, &[], &[]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    init_main_repo(&main, url).unwrap();
    let piece = main.join(".rgc").join("pieces").join("chain_main");
    ensure_piece_repo(&main, &piece, url).unwrap();

    let mut guard = 0;
    loop {
        fetch_chain_step(&piece, "refs/heads/main", "main", 10, 0, false).unwrap();
        guard += 1;
        if !is_shallow(&piece) || guard > 10 {
            break;
        }
    }
    assert!(!is_shallow(&piece), "chain should complete within steps");
    // 中途命名空间：wip
    transport_to_main(&main, &piece, "refs/heads/main", "main", false).unwrap();
    assert!(run_git(&["rev-parse", "--verify", "refs/rgc/wip/main"], Some(&main)).is_ok());
    // 完成命名空间：正式 refs，wip 删除
    transport_to_main(&main, &piece, "refs/heads/main", "main", true).unwrap();
    let tip = run_git(&["rev-parse", "refs/remotes/origin/main"], Some(&main)).unwrap();
    let want = run_git(&["rev-parse", "refs/heads/main"], Some(origin.as_path())).unwrap();
    assert_eq!(tip.stdout.trim(), want.stdout.trim());
    assert!(run_git(&["rev-parse", "--verify", "refs/rgc/wip/main"], Some(&main)).is_err());
}
```

- [ ] **Step 3: 跑测试**

Run: `cargo test -q --test chain`
Expected: `test result: ok. 1 passed`

- [ ] **Step 4: Commit**

```bash
git add src/gitio.rs tests/chain.rs
git commit -m "feat: piece repo lifecycle and deepen chain primitives"
```

---

### Task 9: tags 批量片

**Files:**
- Modify: `src/gitio.rs`（追加两个函数）
- Create: `tests/tags.rs`

- [ ] **Step 1: 在 src/gitio.rs 追加（transport_to_main 之后）**

```rust
/// 一次连接批量 fetch tags（片仓库内执行）
pub fn fetch_tag_batch(piece: &Path, tags: &[crate::refs::RefEntry]) -> Result<()> {
    let mut args: Vec<String> = vec!["fetch".into(), "--quiet".into(), "--no-tags".into(), "origin".into()];
    for t in tags {
        args.push(format!("+{}:{}", t.full_name, t.full_name));
    }
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_git(&refs, Some(piece))?;
    Ok(())
}

/// tags 成果搬运进主仓库（tag fetch 原子完整，直接写正式命名空间）
pub fn transport_tags_to_main(main: &Path, piece: &Path, tags: &[crate::refs::RefEntry]) -> Result<()> {
    let mut args: Vec<String> = vec!["fetch".into(), "--quiet".into(), piece.to_string_lossy().into_owned()];
    for t in tags {
        args.push(format!("+{}:{}", t.full_name, t.full_name));
    }
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    run_git(&refs, Some(main))?;
    Ok(())
}
```

- [ ] **Step 2: 写 tests/tags.rs**

```rust
mod common;
use common::*;
use rgc::gitio::*;

#[test]
fn tag_batch_transports() {
    let origin = build_origin(20, &[], &[("v1", 5), ("v2", 15)]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    init_main_repo(&main, url).unwrap();
    let piece = main.join(".rgc").join("pieces").join("tags_v1");
    ensure_piece_repo(&main, &piece, url).unwrap();
    let refs = rgc::refs::ls_remote(url).unwrap();
    fetch_tag_batch(&piece, &refs.tags).unwrap();
    transport_tags_to_main(&main, &piece, &refs.tags).unwrap();
    for t in &refs.tags {
        let got = run_git(&["rev-parse", &t.full_name], Some(&main)).unwrap();
        assert_eq!(got.stdout.trim(), t.oid);
    }
}
```

- [ ] **Step 3: 跑测试**

Run: `cargo test -q --test tags`
Expected: `test result: ok. 1 passed`

- [ ] **Step 4: Commit**

```bash
git add src/gitio.rs tests/tags.rs
git commit -m "feat: tag batch fetch and transport"
```

---

### Task 10: 等价性校验器（测试 oracle）

**Files:**
- Create: `src/equiv.rs`
- Modify: `src/lib.rs`（追加 `pub mod equiv;`）
- Create: `tests/equiv_test.rs`

- [ ] **Step 1: lib.rs 追加 `pub mod equiv;`**

- [ ] **Step 2: 写 src/equiv.rs**

```rust
use crate::gitio::run_git;
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug)]
pub struct RepoSnapshot {
    pub refs: BTreeMap<String, String>,
    pub objects: BTreeSet<String>,
}

pub fn snapshot(repo: &Path) -> Result<RepoSnapshot> {
    let out = run_git(&["for-each-ref", "--format=%(refname) %(objectname)"], Some(repo))?;
    let mut refs = BTreeMap::new();
    for line in out.stdout.lines() {
        if let Some((name, oid)) = line.split_once(' ') {
            refs.insert(name.to_string(), oid.to_string());
        }
    }
    let obj = run_git(&["rev-list", "--objects", "--all"], Some(repo))?;
    let objects: BTreeSet<String> = obj
        .stdout
        .lines()
        .map(|l| l.split(' ').next().unwrap().to_string())
        .collect();
    Ok(RepoSnapshot { refs, objects })
}

/// 产物等价性关心 origin 跟踪分支与 tags（本地分支/HEAD 是 finalize 的产物）
fn relevant_refs(s: &RepoSnapshot) -> BTreeMap<String, String> {
    s.refs
        .iter()
        .filter(|(k, _)| k.starts_with("refs/remotes/origin/") || k.starts_with("refs/tags/"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

pub fn assert_same_refs(a: &RepoSnapshot, b: &RepoSnapshot) -> Result<()> {
    let (ra, rb) = (relevant_refs(a), relevant_refs(b));
    if ra != rb {
        bail!("refs differ:\nonly A: {:?}\nonly B: {:?}", ra, rb);
    }
    Ok(())
}

pub fn assert_same_objects(a: &RepoSnapshot, b: &RepoSnapshot) -> Result<()> {
    if a.objects != b.objects {
        let only_a: Vec<_> = a.objects.difference(&b.objects).take(10).collect();
        let only_b: Vec<_> = b.objects.difference(&a.objects).take(10).collect();
        bail!(
            "objects differ (counts {} vs {})\nonly A: {:?}\nonly B: {:?}",
            a.objects.len(),
            b.objects.len(),
            only_a,
            only_b
        );
    }
    Ok(())
}
```

- [ ] **Step 3: 写 tests/equiv_test.rs**

```rust
mod common;
use common::*;
use rgc::equiv::{assert_same_objects, assert_same_refs, snapshot};

#[test]
fn snapshots_of_two_clones_are_equivalent() {
    let origin = build_origin(25, &[("dev", 8)], &[("v1", 12)]);
    let td = tempfile::tempdir().unwrap();
    let a = td.path().join("a");
    let b = td.path().join("b");
    git(td.path(), &["clone", "--quiet", origin.to_str().unwrap(), a.to_str().unwrap()]);
    git(td.path(), &["clone", "--quiet", origin.to_str().unwrap(), b.to_str().unwrap()]);
    let (sa, sb) = (snapshot(&a).unwrap(), snapshot(&b).unwrap());
    assert_same_refs(&sa, &sb).unwrap();
    assert_same_objects(&sa, &sb).unwrap();
}

#[test]
fn missing_tag_breaks_ref_equivalence() {
    let origin = build_origin(10, &[], &[("v1", 5)]);
    let td = tempfile::tempdir().unwrap();
    let a = td.path().join("a");
    let b = td.path().join("b");
    git(td.path(), &["clone", "--quiet", origin.to_str().unwrap(), a.to_str().unwrap()]);
    git(td.path(), &["clone", "--quiet", origin.to_str().unwrap(), b.to_str().unwrap()]);
    git(&b, &["tag", "-d", "v1"]);
    let (sa, sb) = (snapshot(&a).unwrap(), snapshot(&b).unwrap());
    assert!(assert_same_refs(&sa, &sb).is_err());
}
```

- [ ] **Step 4: 跑测试**

Run: `cargo test -q --test equiv_test`
Expected: `test result: ok. 2 passed`

- [ ] **Step 5: Commit**

```bash
git add src/equiv.rs src/lib.rs tests/equiv_test.rs
git commit -m "feat: equivalence checker oracle"
```

---

### Task 11: Finalizer（收尾 fetch / 布局规整 / 校验 / 清理）

**Files:**
- Create: `src/finalizer.rs`
- Modify: `src/lib.rs`（追加 `pub mod finalizer;`）
- Create: `tests/finalizer.rs`

- [ ] **Step 1: lib.rs 追加 `pub mod finalizer;`**

- [ ] **Step 2: 写 src/finalizer.rs**

```rust
use crate::gitio::run_git;
use crate::planner::{Piece, Plan};
use anyhow::{bail, Context, Result};
use std::path::Path;

pub fn finalize(plan: &Plan, main: &Path, keep_state: bool) -> Result<()> {
    // 0. remote 配置先行（catch-up fetch 依赖 refspec）
    run_git(&["config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"], Some(main))?;
    run_git(&["config", "remote.origin.url", &plan.url], Some(main))?;
    // 1. 收尾 fetch：捕获 ls-remote 之后的漂移 + 一切遗漏的兜底
    run_git(&["fetch", "--quiet", "--prune", "--tags", "origin"], Some(main))?;
    // 2. 默认分支 + checkout（与 git clone 布局对齐）
    let db = default_branch(plan);
    run_git(&["checkout", "-B", &db, &format!("refs/remotes/origin/{}", db)], Some(main))
        .with_context(|| format!("checkout {}", db))?;
    run_git(&["config", &format!("branch.{}.remote", db), "origin"], Some(main))?;
    run_git(&["config", &format!("branch.{}.merge", db), &format!("refs/heads/{}", db)], Some(main))?;
    // 3. 校验
    verify(plan, main)?;
    // 4. 清理
    if !keep_state {
        let _ = std::fs::remove_dir_all(main.join(".rgc"));
    }
    Ok(())
}

fn default_branch(plan: &Plan) -> String {
    match &plan.default_branch {
        Some(d) => d.clone(),
        None => plan
            .pieces
            .iter()
            .find_map(|p| match p {
                Piece::Chain { short_name, .. } => Some(short_name.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "main".to_string()),
    }
}

/// refs 与服务端实时快照一致 + 全对象图连通
pub fn verify(plan: &Plan, main: &Path) -> Result<()> {
    let remote = crate::refs::ls_remote(&plan.url)?;
    let mut bad = Vec::new();
    for b in &remote.branches {
        let r = format!("refs/remotes/origin/{}", b.short_name);
        match run_git(&["rev-parse", "--verify", &r], Some(main)) {
            Ok(o) if o.stdout.trim() == b.oid => {}
            _ => bad.push(r),
        }
    }
    for t in &remote.tags {
        match run_git(&["rev-parse", "--verify", &t.full_name], Some(main)) {
            Ok(o) if o.stdout.trim() == t.oid => {}
            _ => bad.push(t.full_name.clone()),
        }
    }
    if !bad.is_empty() {
        bail!("verification failed, missing/mismatched refs: {:?}", bad);
    }
    run_git(&["rev-list", "--objects", "--all", "--quiet"], Some(main))?;
    Ok(())
}
```

- [ ] **Step 3: 写 tests/finalizer.rs**

```rust
mod common;
use common::*;

#[test]
fn finalize_produces_standard_layout() {
    let origin = build_origin(15, &[("dev", 8)], &[("v1", 4)]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    rgc::gitio::init_main_repo(&main, url).unwrap();
    rgc::gitio::run_git(
        &["fetch", "--quiet", url, "+refs/heads/*:refs/remotes/origin/*", "+refs/tags/*:refs/tags/*"],
        Some(&main),
    )
    .unwrap();
    std::fs::create_dir_all(main.join(".rgc")).unwrap(); // 模拟残留状态目录
    let plan = rgc::planner::build_plan(url, &rgc::refs::ls_remote(url).unwrap(), &rgc::planner::PlannerConfig::default());
    rgc::finalizer::finalize(&plan, &main, false).unwrap();
    assert!(main.join("f0.txt").exists(), "worktree checked out");
    assert!(!main.join(".rgc").exists(), "state cleaned");
    let head = rgc::gitio::run_git(&["symbolic-ref", "HEAD"], Some(&main)).unwrap();
    assert_eq!(head.stdout.trim(), "refs/heads/main");
}
```

- [ ] **Step 4: 跑测试**

Run: `cargo test -q --test finalizer`
Expected: `test result: ok. 1 passed`

- [ ] **Step 5: Commit**

```bash
git add src/finalizer.rs src/lib.rs tests/finalizer.rs
git commit -m "feat: finalizer with catch-up fetch, layout and verification"
```

---

### Task 12: Scheduler（串行核心 + 端到端）

**Files:**
- Create: `src/scheduler.rs`
- Modify: `src/lib.rs`（追加 `pub mod scheduler;`）
- Create: `tests/scheduler_e2e.rs`

- [ ] **Step 1: lib.rs 追加 `pub mod scheduler;`**

- [ ] **Step 2: 写 src/scheduler.rs（完整状态机；并行槽位/冷却已内建，jobs=1 即串行）**

```rust
use crate::errors::{kind_of, FailureKind};
use crate::gitio;
use crate::planner::{halve_step, next_step, Piece, Plan, StepMeasurement};
use crate::state::{ChainState, PieceState, PieceStatus, State};
use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub jobs: usize,
    pub max_attempts: u32,
    pub target_secs: f64,
}
impl Default for SchedulerConfig {
    fn default() -> Self {
        Self { jobs: 2, max_attempts: 5, target_secs: 600.0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    RetryAfter(u64),
    HalveAndRetry,
    RetryNoShallow,
    GiveUp,
}

/// 网络退避：2s 起倍增，封顶 60s
pub fn backoff_secs(attempts: u32) -> u64 {
    2u64.saturating_pow(attempts).min(60)
}

/// 重试决策：Fatal→放弃；Shallow 不支持→退化整支 fetch；
/// 限流→长退避（不计片预算）；拥塞(reset)→退避+全局冷却；网络失败第 2 次起步长减半。
pub fn decide(attempts: u32, kind: FailureKind, max_attempts: u32) -> Action {
    if attempts > max_attempts {
        return Action::GiveUp;
    }
    match kind {
        FailureKind::ShallowUnsupported => Action::RetryNoShallow,
        FailureKind::Fatal => Action::GiveUp,
        FailureKind::RateLimited => Action::RetryAfter(60u64 << attempts.min(2)),
        FailureKind::Congestion => Action::RetryAfter(backoff_secs(attempts)),
        FailureKind::Network if attempts >= 2 => Action::HalveAndRetry,
        FailureKind::Network => Action::RetryAfter(backoff_secs(attempts)),
    }
}

pub fn run(plan: &Plan, main: &Path, state: &Arc<Mutex<State>>, cfg: &SchedulerConfig) -> Result<()> {
    gitio::init_main_repo(main, &plan.url)?;
    let total = state.lock().unwrap().pieces.len();
    if total == 0 {
        return Ok(());
    }
    let jobs = cfg.jobs.max(1).min(total);
    let cooldown = Arc::new(Mutex::new(Instant::now()));
    let cursor = Arc::new(AtomicUsize::new(0));
    let plan = Arc::new(plan.clone());
    let main: Arc<Path> = Arc::from(main);
    let mut handles = Vec::new();
    for _ in 0..jobs {
        let (plan, main, state, cfg, cooldown, cursor) =
            (plan.clone(), main.clone(), state.clone(), cfg.clone(), cooldown.clone(), cursor.clone());
        handles.push(std::thread::spawn(move || worker(plan, main, state, &cfg, cooldown, cursor)));
    }
    for h in handles {
        h.join().map_err(|_| anyhow!("scheduler worker panicked"))??;
    }
    let stuck: Vec<String> = state
        .lock()
        .unwrap()
        .pieces
        .iter()
        .filter(|p| p.status != PieceStatus::Done)
        .map(|p| p.id.clone())
        .collect();
    if !stuck.is_empty() {
        bail!("pieces failed permanently: {:?} — rerun to resume", stuck);
    }
    Ok(())
}

fn worker(
    plan: Arc<Plan>,
    main: Arc<Path>,
    state: Arc<Mutex<State>>,
    cfg: &SchedulerConfig,
    cooldown: Arc<Mutex<Instant>>,
    cursor: Arc<AtomicUsize>,
) -> Result<()> {
    loop {
        let wait = cooldown.lock().unwrap().saturating_duration_since(Instant::now());
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
        // 原子认领：Running 状态防双占
        let claimed = {
            let mut st = state.lock().unwrap();
            let n = st.pieces.len();
            let start = cursor.load(Ordering::Relaxed);
            let mut found = None;
            for k in 0..n {
                let i = (start + k) % n;
                if st.pieces[i].status == PieceStatus::Pending {
                    st.pieces[i].status = PieceStatus::Running;
                    cursor.store(i, Ordering::Relaxed);
                    found = Some(i);
                    break;
                }
            }
            if found.is_some() {
                let _ = st.save(&main);
            }
            found
        };
        let Some(i) = claimed else { break };
        execute_piece(&plan, &main, &state, i, cfg, &cooldown)?;
    }
    Ok(())
}

fn execute_piece(plan: &Plan, main: &Path, state: &Arc<Mutex<State>>, idx: usize, cfg: &SchedulerConfig, cooldown: &Arc<Mutex<Instant>>) -> Result<()> {
    // 磁盘预检：按历史最大片字节 × 1.5 估算
    let max_seen = state.lock().unwrap().pieces.iter().map(|p| p.bytes).max().unwrap_or(0);
    let estimate = (max_seen as f64 * 1.5) as u64;
    if estimate > 0 {
        if let Ok(avail) = fs2::available_volume(main) {
            if avail < estimate {
                bail!("disk low: {} bytes available, next piece may need ~{}", avail, estimate);
            }
        }
    }
    let piece = plan.pieces[idx].clone();
    let piece_path: PathBuf = crate::state::pieces_dir(main).join(piece.piece_dir_name());
    let mut ps = state.lock().unwrap().pieces[idx].clone();
    let res = run_piece(&piece, plan, main, &piece_path, &mut ps, state, idx, cfg);
    {
        let mut st = state.lock().unwrap();
        st.pieces[idx] = ps.clone();
        let _ = st.save(main);
    }
    // 全局降速：限流/拥塞 → 全体 worker 冷却 60s（spec §4.4）
    if let Err(e) = &res {
        if matches!(kind_of(e), FailureKind::RateLimited | FailureKind::Congestion) {
            *cooldown.lock().unwrap() = Instant::now() + Duration::from_secs(60);
        }
    }
    res
}

#[allow(clippy::too_many_arguments)]
fn run_piece(
    piece: &Piece,
    plan: &Plan,
    main: &Path,
    piece_path: &Path,
    ps: &mut PieceState,
    state: &Arc<Mutex<State>>,
    idx: usize,
    cfg: &SchedulerConfig,
) -> Result<()> {
    // 新进程恢复时给耗尽预算的片重新计费
    if ps.status == PieceStatus::Pending && ps.attempts > cfg.max_attempts {
        ps.attempts = 0;
    }
    loop {
        let res = match piece {
            Piece::Chain { full_ref, short_name } => {
                run_chain(plan, main, piece_path, full_ref, short_name, ps, state, idx, cfg)
            }
            Piece::TagBatch { tags } => run_tags(plan, main, piece_path, tags, ps),
        };
        match res {
            Ok(()) => {
                ps.status = PieceStatus::Done;
                return Ok(());
            }
            Err(e) => {
                ps.attempts += 1;
                match decide(ps.attempts, kind_of(&e), cfg.max_attempts) {
                    Action::GiveUp => {
                        ps.status = PieceStatus::Pending;
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
                        std::thread::sleep(Duration::from_secs(backoff_secs(ps.attempts)));
                    }
                    Action::RetryAfter(secs) => std::thread::sleep(Duration::from_secs(secs)),
                }
            }
        }
    }
}

fn persist(state: &Arc<Mutex<State>>, idx: usize, ps: &PieceState, main: &Path) {
    let mut st = state.lock().unwrap();
    if let Some(sp) = st.pieces.get_mut(idx) {
        sp.chain = ps.chain.clone();
        sp.bytes = ps.bytes;
        sp.attempts = ps.attempts;
    }
    let _ = st.save(main);
}

#[allow(clippy::too_many_arguments)]
fn run_chain(
    plan: &Plan,
    main: &Path,
    piece_path: &Path,
    full_ref: &str,
    short: &str,
    ps: &mut PieceState,
    state: &Arc<Mutex<State>>,
    idx: usize,
    cfg: &SchedulerConfig,
) -> Result<()> {
    let chain = ps.chain.clone().unwrap_or(ChainState { depth_done: 0, step: plan.initial_step, no_shallow: false });
    let mut depth_done = chain.depth_done;
    let mut step = chain.step;
    gitio::ensure_piece_repo(main, piece_path, &plan.url)?;
    loop {
        let (b0, c0) = gitio::repo_stats(piece_path, short)?;
        let t0 = Instant::now();
        gitio::fetch_chain_step(piece_path, full_ref, short, step, depth_done, chain.no_shallow)?;
        let secs = t0.elapsed().as_secs_f64();
        let (b1, c1) = gitio::repo_stats(piece_path, short)?;
        depth_done = c1;
        let added_bytes = b1.saturating_sub(b0);
        ps.bytes += added_bytes;
        // 每步立即搬运；链完成才写正式 refs（中途只进 wip，防"撒谎的 have"）
        let complete = !gitio::is_shallow(piece_path);
        gitio::transport_to_main(main, piece_path, full_ref, short, complete)?;
        ps.chain = Some(ChainState { depth_done, step, no_shallow: chain.no_shallow });
        persist(state, idx, ps, main);
        if complete {
            return Ok(());
        }
        step = next_step(&StepMeasurement { bytes: added_bytes, secs, commits: c1.saturating_sub(c0) }, cfg.target_secs);
    }
}

fn run_tags(plan: &Plan, main: &Path, piece_path: &Path, tags: &[crate::refs::RefEntry], ps: &mut PieceState) -> Result<()> {
    gitio::ensure_piece_repo(main, piece_path, &plan.url)?;
    let (b0, _) = gitio::repo_stats(piece_path, "")?;
    gitio::fetch_tag_batch(piece_path, tags)?;
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
        assert_eq!(decide(0, FailureKind::Network, 5), Action::RetryAfter(2));
        assert_eq!(decide(2, FailureKind::Network, 5), Action::HalveAndRetry);
        assert_eq!(decide(0, FailureKind::RateLimited, 5), Action::RetryAfter(60));
        assert_eq!(decide(1, FailureKind::RateLimited, 5), Action::RetryAfter(120));
        assert_eq!(decide(0, FailureKind::ShallowUnsupported, 5), Action::RetryNoShallow);
        assert_eq!(decide(0, FailureKind::Fatal, 5), Action::GiveUp);
        assert_eq!(decide(6, FailureKind::Network, 5), Action::GiveUp);
    }
}
```

- [ ] **Step 3: 写 tests/scheduler_e2e.rs**

```rust
mod common;
use common::*;
use rgc::equiv::{assert_same_objects, assert_same_refs, snapshot};
use rgc::finalizer::finalize;
use rgc::planner::{build_plan, PlannerConfig};
use rgc::refs::ls_remote;
use rgc::scheduler::{run, SchedulerConfig};
use rgc::state::State;
use std::sync::{Arc, Mutex};

#[test]
fn serial_clone_equals_git_clone() {
    let origin = build_origin(120, &[("dev", 40)], &[("v1", 25), ("v2", 80)]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    let plan = build_plan(url, &ls_remote(url).unwrap(), &PlannerConfig { initial_step: 30, ..Default::default() });
    let state = Arc::new(Mutex::new(State::new(&plan)));
    run(&plan, &main, &state, &SchedulerConfig { jobs: 1, ..Default::default() }).unwrap();
    finalize(&plan, &main, false).unwrap();
    // 参照物
    let reference = td.path().join("ref");
    git(td.path(), &["clone", "--quiet", url, reference.to_str().unwrap()]);
    let (a, b) = (snapshot(&main).unwrap(), snapshot(&reference).unwrap());
    assert_same_refs(&a, &b).unwrap();
    assert_same_objects(&a, &b).unwrap();
}
```

- [ ] **Step 4: 跑测试**

Run: `cargo test -q scheduler`
Expected: 单元 2 passed + 集成 1 passed

- [ ] **Step 5: Commit**

```bash
git add src/scheduler.rs src/lib.rs tests/scheduler_e2e.rs
git commit -m "feat: scheduler state machine with serial e2e equivalence"
```

---

### Task 13: 重试语义集成（预算重置）

**Files:**
- Create: `tests/retry.rs`

- [ ] **Step 1: 写 tests/retry.rs**

```rust
mod common;
use common::*;
use rgc::planner::{build_plan, PlannerConfig};
use rgc::refs::ls_remote;
use rgc::scheduler::{run, SchedulerConfig};
use rgc::state::{State, PieceStatus};
use std::sync::{Arc, Mutex};

#[test]
fn exhausted_budget_is_reset_on_new_run() {
    let origin = build_origin(30, &[], &[]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    let plan = build_plan(url, &ls_remote(url).unwrap(), &PlannerConfig { initial_step: 10, ..Default::default() });
    let mut st = State::new(&plan);
    st.pieces[0].attempts = 99; // 预算耗尽的片
    let state = Arc::new(Mutex::new(st));
    run(&plan, &main, &state, &SchedulerConfig { jobs: 1, ..Default::default() }).unwrap();
    assert!(state.lock().unwrap().pieces.iter().all(|p| p.status == PieceStatus::Done));
}
```

- [ ] **Step 2: 跑测试**

Run: `cargo test -q --test retry`
Expected: `test result: ok. 1 passed`

- [ ] **Step 3: Commit**

```bash
git add tests/retry.rs
git commit -m "test: retry budget reset on resume"
```

---

### Task 14: 并行端到端

**Files:**
- Create: `tests/parallel_e2e.rs`

- [ ] **Step 1: 写 tests/parallel_e2e.rs**

```rust
mod common;
use common::*;
use rgc::equiv::{assert_same_objects, assert_same_refs, snapshot};
use rgc::finalizer::finalize;
use rgc::planner::{build_plan, PlannerConfig};
use rgc::refs::ls_remote;
use rgc::scheduler::{run, SchedulerConfig};
use rgc::state::{State, PieceStatus};
use std::sync::{Arc, Mutex};

#[test]
fn parallel_clone_equals_git_clone() {
    let origin = build_origin(150, &[("dev", 60), ("rel", 90)], &[("v1", 30), ("v2", 100)]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let main = td.path().join("repo");
    let plan = build_plan(url, &ls_remote(url).unwrap(), &PlannerConfig { initial_step: 20, ..Default::default() });
    let state = Arc::new(Mutex::new(State::new(&plan)));
    run(&plan, &main, &state, &SchedulerConfig { jobs: 3, ..Default::default() }).unwrap();
    assert!(state.lock().unwrap().pieces.iter().all(|p| p.status == PieceStatus::Done));
    finalize(&plan, &main, false).unwrap();
    let reference = td.path().join("ref");
    git(td.path(), &["clone", "--quiet", url, reference.to_str().unwrap()]);
    let (a, b) = (snapshot(&main).unwrap(), snapshot(&reference).unwrap());
    assert_same_refs(&a, &b).unwrap();
    assert_same_objects(&a, &b).unwrap();
}
```

- [ ] **Step 2: 跑测试**

Run: `cargo test -q --test parallel_e2e`
Expected: `test result: ok. 1 passed`

- [ ] **Step 3: Commit**

```bash
git add tests/parallel_e2e.rs
git commit -m "test: parallel clone equivalence"
```

---

### Task 15: CLI（clone / resume / status + 进度）

**Files:**
- Modify: `src/main.rs`（整文件替换）

- [ ] **Step 1: 替换 src/main.rs**

```rust
use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Parser)]
#[command(name = "rgc", version, about = "Resumable git clone for very large repositories")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 断点续传 clone（目标目录已有 .rgc/ 时自动恢复）
    Clone {
        url: String,
        dir: Option<String>,
        #[arg(long, default_value_t = 2)]
        jobs: usize,
        /// 单片目标时长（秒）
        #[arg(long, default_value_t = 600.0)]
        piece_target: f64,
        #[arg(long)]
        keep_state: bool,
        /// 测试旋钮：初始 deepen 步长
        #[arg(long, default_value_t = 10_000, hide = true)]
        initial_step: u32,
    },
    /// 显式恢复中断的 clone
    Resume {
        dir: String,
        #[arg(long, default_value_t = 2)]
        jobs: usize,
        #[arg(long, default_value_t = 600.0)]
        piece_target: f64,
        #[arg(long)]
        keep_state: bool,
    },
    /// 查看各片进度
    Status { dir: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Clone { url, dir, jobs, piece_target, keep_state, initial_step } => {
            let dir: PathBuf = dir.map(PathBuf::from).unwrap_or_else(|| default_dir(&url));
            clone_flow(&url, &dir, jobs, piece_target, keep_state, initial_step)
        }
        Cmd::Resume { dir, jobs, piece_target, keep_state } => {
            let dir = PathBuf::from(dir);
            let plan = rgc::planner::load_plan(&dir)?.ok_or_else(|| anyhow!("no .rgc/plan.json in {}", dir.display()))?;
            clone_existing(&plan, &dir, jobs, piece_target, keep_state)
        }
        Cmd::Status { dir } => status(Path::new(&dir)),
    }
}

fn default_dir(url: &str) -> PathBuf {
    let base = url.trim_end_matches('/').rsplit('/').next().unwrap_or("repo");
    PathBuf::from(base.trim_end_matches(".git"))
}

fn clone_flow(url: &str, dir: &Path, jobs: usize, piece_target: f64, keep_state: bool, initial_step: u32) -> Result<()> {
    if rgc::planner::load_plan(dir)?.is_some() {
        eprintln!("rgc: found existing state — resuming");
        return clone_existing_dir(dir, jobs, piece_target, keep_state);
    }
    let remote_refs = rgc::refs::ls_remote(url)?;
    let plan = rgc::planner::build_plan(url, &remote_refs, &rgc::planner::PlannerConfig { tags_per_batch: 32, initial_step });
    rgc::planner::save_plan(dir, &plan)?;
    rgc::state::State::new(&plan).save(dir)?;
    clone_existing(&plan, dir, jobs, piece_target, keep_state)
}

fn clone_existing_dir(dir: &Path, jobs: usize, piece_target: f64, keep_state: bool) -> Result<()> {
    let plan = rgc::planner::load_plan(dir)?.ok_or_else(|| anyhow!("no .rgc/plan.json"))?;
    clone_existing(&plan, dir, jobs, piece_target, keep_state)
}

fn clone_existing(plan: &rgc::planner::Plan, dir: &Path, jobs: usize, piece_target: f64, keep_state: bool) -> Result<()> {
    let state = match rgc::state::State::load(dir)? {
        Some(s) => Arc::new(Mutex::new(s)),
        None => {
            eprintln!("rgc: state.json missing/corrupt — reconciling from repository");
            Arc::new(Mutex::new(rgc::state::reconcile(plan, dir)))
        }
    };
    let total = state.lock().unwrap().pieces.len() as u64;
    let bar = indicatif::ProgressBar::new(total);
    bar.set_style(indicatif::ProgressStyle::default_bar().template("{msg} [{bar:40}] {pos}/{len} pieces").unwrap());
    let mon_state = state.clone();
    let mon_bar = bar.clone();
    let mon = std::thread::spawn(move || loop {
        let done = mon_state
            .lock()
            .unwrap()
            .pieces
            .iter()
            .filter(|p| p.status == rgc::state::PieceStatus::Done)
            .count() as u64;
        mon_bar.set_position(done);
        if done >= total {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    });
    let cfg = rgc::scheduler::SchedulerConfig { jobs, max_attempts: 5, target_secs: piece_target };
    let res = rgc::scheduler::run(plan, dir, &state, &cfg);
    let _ = mon.join();
    res?;
    bar.finish_with_message("fetch complete");
    rgc::finalizer::finalize(plan, dir, keep_state)
}

fn status(dir: &Path) -> Result<()> {
    let plan = rgc::planner::load_plan(dir)?.ok_or_else(|| anyhow!("no plan in {}", dir.display()))?;
    let state = rgc::state::State::load(dir)?.unwrap_or_else(|| rgc::state::reconcile(&plan, dir));
    for ps in &state.pieces {
        let extra = match &ps.chain {
            Some(c) => format!("  depth={} step={}", c.depth_done, c.step),
            None => String::new(),
        };
        println!("{:?}  {}{}", ps.status, ps.id, extra);
    }
    let done = state.pieces.iter().filter(|p| p.status == rgc::state::PieceStatus::Done).count();
    println!("{}/{} pieces done", done, state.pieces.len());
    Ok(())
}
```

- [ ] **Step 2: 写 tests/cli.rs**

```rust
mod common;
use common::*;

#[test]
fn cli_clone_end_to_end() {
    let origin = build_origin(40, &[("dev", 10)], &[("v1", 5)]);
    let td = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_rgc"))
        .args(["clone", origin.to_str().unwrap(), td.path().join("r").to_str().unwrap(), "--jobs", "1"])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert!(td.path().join("r").join(".git").exists());
}
```

- [ ] **Step 3: 跑全部测试**

Run: `cargo test -q`
Expected: 全部通过（含 cli 1 项）

- [ ] **Step 4: 手工冒烟（可选但推荐）**

Run: `cargo run -q -- clone https://github.com/rust-lang/git2.rs /tmp/rgc-smoke && rm -rf /tmp/rgc-smoke`
Expected: 完整 clone 成功，产物可 `cd` 使用

- [ ] **Step 5: Commit**

```bash
git add src/main.rs tests/cli.rs
git commit -m "feat: rgc CLI (clone/resume/status) with progress"
```

---

### Task 16: kill -9 端到端 + CI + README

**Files:**
- Create: `tests/kill9.rs`
- Create: `.github/workflows/ci.yml`
- Create: `README.md`

- [ ] **Step 1: 写 tests/kill9.rs**

```rust
mod common;
use common::*;

/// 验收标准：任意时刻杀掉进程，resume 后产物与 git clone 完全等价。
#[test]
#[ignore = "slow end-to-end; run with `cargo test -- --ignored`"]
fn kill_and_resume_equals_git_clone() {
    let origin = build_origin(1200, &[("dev", 600)], &[("v1", 300)]);
    let url = origin.to_str().unwrap();
    let td = tempfile::tempdir().unwrap();
    let target = td.path().join("repo");
    let mut killed_once = false;
    for _ in 0..40 {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_rgc"))
            .args([
                "clone", url, target.to_str().unwrap(),
                "--jobs", "1", "--piece-target", "2", "--initial-step", "100",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
        if !killed_once {
            let _ = child.kill();
            child.wait().unwrap();
            killed_once = true;
            continue;
        }
        if child.wait().unwrap().success() {
            break;
        }
    }
    let reference = td.path().join("ref");
    git(td.path(), &["clone", "--quiet", url, reference.to_str().unwrap()]);
    let a = rgc::equiv::snapshot(&target).unwrap();
    let b = rgc::equiv::snapshot(&reference).unwrap();
    rgc::equiv::assert_same_refs(&a, &b).unwrap();
    rgc::equiv::assert_same_objects(&a, &b).unwrap();
}
```

- [ ] **Step 2: 跑慢速套件**

Run: `cargo test -q --test kill9 -- --ignored --nocapture`
Expected: `test result: ok. 1 passed`（约 1–3 分钟）

- [ ] **Step 3: 写 .github/workflows/ci.yml**

```yaml
name: ci
on: [push, pull_request]
jobs:
  test:
    strategy:
      fail-fast: false
      matrix:
        os: [macos-latest, ubuntu-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo test --quiet
      - run: cargo test --quiet -- --ignored
  windows-check:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo check
```

- [ ] **Step 4: 写 README.md**

```markdown
# rgc — resumable git clone

超大仓库的断点续传 clone 工具。设计：`docs/superpowers/specs/`，实现计划：`docs/superpowers/plans/`。

## 使用

    rgc clone <url> [dir]     # 中断后重跑同一命令即自动恢复
    rgc resume <dir>
    rgc status <dir>

要求 PATH 上有 git ≥ 2.26。`--jobs` 默认 2（1–8）；`--piece-target` 单片目标秒数（默认 600）。

## 原理（一句话）

git 传输协议不支持字节级续传，rgc 把一次巨型 clone 切成许多独立、幂等的小请求
（分支 = shallow/deepen 步进链，tags = 批量片），每片独立下载、独立重试，断在哪片就只重做那片。

## 限制

- 分片使 delta 跨片失效，总传输量约为一次 clone 的 1.2–1.5 倍。
- 代理/镜像用 git 自身配置（http.proxy 等），rgc 透传。
- 并行是尾部优化器：单主干仓库的墙钟时间 ≈ 主干链时长（串行）。

## 开发

    cargo test                # 快速套件
    cargo test -- --ignored   # 含慢速 kill -9 端到端
```

- [ ] **Step 5: 全量回归**

Run: `cargo test -q && cargo test -q -- --ignored`
Expected: 全部通过

- [ ] **Step 6: Commit**

```bash
git add tests/kill9.rs .github/ README.md
git commit -m "test: kill-resume e2e acceptance, CI and README"
```

---

## 自审记录（写计划时已核对）

1. **Spec 覆盖**：§3 CLI→T15；Planner→T6；Scheduler→T12–14；State→T7；Finalizer→T11；gitio→T4/8/9；equiv→T10；§4.1 双维度→T6（chains+tag batches）；§4.2 自适应→T6 next_step + T12 halving；§4.3 持久化/恢复/--shared/refs 复制→T7/T8；wip 命名空间→T8/T12；§4.4 jobs→T12/T14；§4.5 等价→T10/T11/T12/T14/T16；§5 错误表→T2/T12（decide、磁盘预检、预算重置、Running 折返）；shallow 不支持退化→T12 RetryNoShallow；§6 测试→T3/10/12/14/16。
2. **占位符**：无 TBD/TODO；所有步骤含完整代码。
3. **类型一致性**：`fetch_chain_step(piece, full_ref, short, step, depth_done, no_shallow)`（无 url 参数，origin 在 ensure_piece_repo 配置）；`Plan{url, default_branch, initial_step, pieces}`；`ChainState{depth_done, step, no_shallow}`；`Action` derive PartialEq 供测试断言；`kind_of` 经 anyhow 下转。
