# rgc — Resumable Git Clone 设计文档

日期：2026-09-03
状态：已与需求方评审通过，待实现
工作代号：`rgc`（resumable git clone，名称为占位，可再议）

---

## 1. 背景与动机

`git clone` 无法断点续传，原因在协议层面：

1. **Pack 是现场计算的一次性输出**。服务端根据协商结果实时遍历对象图、做 delta 压缩、流式生成 pack，同一仓库两次请求生成的字节序列不保证相同，不存在"可 Range 续传的静态文件"。
2. **协议没有位置语义**。want/have 协商粒度是提交，不是字节或对象序号；连接断开后服务端即丢弃生成到一半的 pack 与计算状态。
3. **Pack 内部是 delta 链**。缺失任意中间段，后续对象均无法解出，半截 pack 不可用。

Git 官方的路线是 bundle-uri（Git 2.38+）：把"动态生成"变为"预生成静态文件放 CDN"，但依赖服务端支持，GitHub 至今未实现。

**推论**：客户端侧唯一可行的通用断点方案，是把"一个易断的巨大流"替换为"许多小的、完整的、幂等的请求"。本工具是该思想的完整工程实现。

### 现有方案与差距

| 方案 | 局限 |
|---|---|
| [ResumableGitClone (rgit.sh)](https://github.com/johnzeng/ResumableGitClone) | bash 脚本、串行、盲探步长、无状态管理/重试/进度 |
| bundle-uri（Git 官方） | 依赖服务端支持；GitHub 未实现 |
| `--filter=blob:none` | 只减小传输量，非续传 |
| `git bundle` + `curl -C -` | 需预生成 bundle，不适用任意仓库 |

## 2. 目标与非目标

### 目标

- 完整克隆超大仓库（GB 级、十万级 commit、数百分支/tags），结果与 `git clone` 完全等价。
- 分片级断点续传：任意时刻中断（Ctrl-C、断网、进程被杀、断电），resume 后只重做未完成的片。
- 片间并行下载，含限流自适应。

### 非目标

- 代理/镜像源管理（用户通过 git 自身配置解决，rgc 透传环境与 git 配置）。
- 字节/对象级精确续传（协议不允许，见背景）。
- partial clone 模式、LFS 特殊处理（远期可选）。
- bundle-uri 探测（列入远期增强，MVP 不做）。
- 作为库被复用（仅 CLI）。

### 需求确认记录

| 维度 | 结论 |
|---|---|
| 目标场景 | 超大仓库（chromium/android/大 monorepo 级） |
| 续传语义 | 分片级：断在哪片重试哪片，绝不从头再来 |
| 完整性 | 等价于完整 `git clone`（全部历史 + 全部分支/tags） |
| 技术栈 | Rust |
| 网络策略 | 专注续传，代理/镜像由用户自配 |

## 3. 架构

### 核心决策：Piece Repo 隔离模式

每个分片在独立的临时"片仓库"（piece repo）中下载，成功后一次性本地搬运进主仓库，随即删除片仓库（分支链的片仓库例外：链存续期间持久化，见 §4.3）。

该决策同时获得：并行无 `.git/shallow` 锁竞争、崩溃不污染主仓库、重试天然幂等。代价：磁盘峰值多出"在跑片"的占用（随 `--jobs` 线性），搬运为本地 IO（不走网络、无服务端限制）。

### 组件与 CLI

```
rgc clone <url> [dir]        # 断点续传 clone；目录已有 .rgc/ 时自动变为 resume（幂等）
rgc resume [dir]             # 显式恢复
rgc status [dir]             # 进度查看
```

| 组件 | 职责 |
|---|---|
| **Planner** | `git ls-remote` 获取全部 refs（去除 peeled `^{}` 重复）→ 生成分片计划：每分支一条 deepen 步进链 + tags 批量片，写入 `plan.json` |
| **Scheduler** | 状态机驱动：取 pending 片 → 准备片仓库 → `git fetch`（自适应步长）→ 本地搬运进主仓库 → 标记 done。`--jobs` 控制片间并行度 |
| **State** | `<dir>/.rgc/{plan,state}.json` 落盘，断点唯一真相源；每片记录 pending/running/done、字节数、尝试次数、（链式片）当前 depth |
| **Finalizer** | 全片完成后：一次无 depth 的收尾 `git fetch`（补跨片遗漏）→ 校验无 shallow 边界 → 校验 refs 与初始 `ls-remote` 快照一致 → 规整标准 clone 布局（`refs/remotes/origin/*`、origin remote 配置、HEAD 指向默认分支）→ checkout → 清理 `.rgc/`（`--keep-state` 保留） |

### 技术选型

- Rust stable；`clap`（CLI）、`serde`/`serde_json`（状态）、`indicatif`（进度）。
- 并发用 `std::thread` + 子进程，**不引入 async runtime**（网络操作全部由 git 子进程完成）。
- 全程编排 `git` 子进程（要求 PATH 上有 git ≥ 2.26，建议协议 v2）；不内嵌 libgit2/gitoxide——凭据、SSH、代理配置免费继承。
- 平台：macOS/Linux 全支持；Windows 为编译目标验证。

## 4. 关键机制

### 4.1 双维度分片

- **宽度（refs 维度）**：分支各成一条独立链，tags 按批成片，片间可并行。
- **深度（历史维度）**：单分支历史用 shallow 链切分：

```
片1: fetch --depth=20000        → 最近 2 万 commit
片2: fetch --deepen=20000       → 第 2~4 万
片3: fetch --deepen=20000       → 第 4~6 万
 ...
片N: deepen 后 shallow 边界消失  → 该分支历史完整
```

链内天然串行（第 k+1 片依赖第 k 片画出的边界）；并行只发生在分支之间。

### 4.2 自适应步长

- 步长 = 每片新增 commit 数，即断点损失粒度。
- 初始探测步长 10,000 commit；此后按实测吞吐调整：`步长 = 吞吐(字节/秒) × 单片目标时长 ÷ 实测字节密度(字节/commit)`。
- 单片目标时长默认 10 分钟（`--piece-target` 可配）。
- 单片反复失败时步长自动减半重切（见 §5）。

### 4.3 链式片的持久化与恢复

- 分支链的片仓库放在 `<dir>/.rgc/pieces/<ref>/` **持久化**（非系统临时目录），shallow 边界保存在其 `.git/shallow` 中。
- 每步完成后立即把增量搬运进主仓库并更新 state（含当前 depth），崩溃最多损失当前一步。
- 崩溃恢复：片仓库完好 → 直接从 state 记录的 depth 续链；片仓库损坏（shallow 文件缺失 / fsck 快速档失败）→ 从主仓库 `git clone --shared` 重建片仓库，并以 `--depth=<已完成>` 重建边界（对象均在本地，协商结果接近空包，网络代价近零）。
- 分支链完成后删除该片仓库。

### 4.4 并发模型

- `--jobs` 默认 2，范围 1–8。一个 job = 一个片仓库上的一次 git fetch（一条服务端连接）。
- 保守默认的原因：每片对服务端都是一次"遍历 + 打包"的 CPU 计算，且 GitHub 对同 IP 并发/高频 fetch 有滥用检测（429 / connection reset）。
- 自动降速：检测到 429/限流/reset → 并发减半 + 指数退避，最低 1；平稳后不自动升回。
- 磁盘峰值 = 主仓库 + 在跑片（jobs 份），每步搬运完即删，不积压。

### 4.5 产物等价性

最终产物与 `git clone <url>` 等价：

- refs 集合及 OID 与 `ls-remote` 快照完全一致；
- 全量对象图完整（Finalizer 收尾 fetch 兜底 + 校验）；
- 标准 clone 布局：`origin` remote、`refs/remotes/origin/*` 跟踪分支、HEAD 指向默认分支、工作区已 checkout；
- 无残留 shallow 边界。

已知代价：切片使 delta 跨片失效，总传输量约为一次 clone 的 1.2–1.5 倍；服务端 CPU 请求次数多于单次 clone。此为协议约束下的固有取舍。

## 5. 错误处理

原则：**state.json 是唯一真相源，任何故障的恢复路径都收敛到"重读状态、继续调度"**。

| 故障 | 处理 |
|---|---|
| 网络超时/reset | 片级指数退避重试（默认 5 次：2s 起倍增，封顶 60s）；重试代价 = 一片 |
| 429 / 限流 / 滥用检测 | 并发减半 + 长退避；此类不计入片的重试次数 |
| 进程被杀 / 断电 | 写入顺序：置 running → 执行 → 置 done（先写后做，宁可重做不可漏记）；启动时扫描 `.rgc/pieces/` 清理孤儿片仓库 |
| 片仓库损坏 | 按 §4.3 重建 |
| 单片反复失败 | 重试 2 次后步长减半重切该片；链式片按新步长继续 |
| 磁盘不足 | 每片开始前预检（按上一片字节数估算余量），不足则干净退出（进度已落盘） |
| Ctrl-C | 信号处理：终止当前 git 子进程 → 当前片标回 pending → 退出；任意时刻可 resume |
| state.json 写坏 | 原子写（tmp + rename）预防；万一损坏，用主仓库现有 refs + 存活片仓库对账重建 |
| 服务端不支持 shallow（罕见） | 探测到后该分支退化为整支单片 + 普通重试 |

## 6. 测试策略

**验收标准：任意时刻杀掉进程，resume 后产物必须与 `git clone` 完全等价。**

- **等价性校验器**（核心测试资产）：对比 refs 集合与 OID、`rev-list --all --objects` 全对象图 diff、工作区 checkout 状态。所有集成测试以此为 oracle。
- **单元测试**：Planner 切片（多小分支 / 单巨支 / 纯 tags / 非常规 ref 名）、自适应步长计算、状态机转换、状态文件原子写与损坏恢复。
- **集成测试**：本地 `file://` / `git daemon` 测试仓库；跑 rgc → 随机时机 `kill -9` → resume → 校验器判定等价，循环进行。
- **故障注入**：本地代理随机掐断连接、模拟 429，验证重试与降速。
- **真实 E2E**（手动/夜间）：linux 内核（单巨支主干链）、数百分支/tags 的中型仓库（并行路径）。
- **CI**：GitHub Actions，macOS + Linux 集成套件；Windows 仅编译验证。

## 7. 远期增强（不在本期）

- bundle-uri 探测：`clone` 时先探测服务端 bundle-uri 广告，命中则静态下载（可叠加 HTTP Range 字节级续传），未命中走现有流程。
- 镜像源 fallback、partial clone 模式、LFS 感知。
- 命名与打包：crates.io / brew 分发。

## 8. 风险

| 风险 | 缓解 |
|---|---|
| GitHub 滥用检测中断长拉取 | 保守并发 + 自动降速 + 片级重试；请求头/凭证透传用户配置 |
| 传输量放大 1.2–1.5x | 文档明示；对"按小时计、断则全废"的场景净收益仍为正 |
| 大量片请求的服务端 CPU 成本 | 自适应步长控制请求数；`--jobs` 封顶 8 |
| git 子进程行为跨版本漂移 | 集成测试锁定最低版本 2.26；CI 多版本矩阵（远期） |
