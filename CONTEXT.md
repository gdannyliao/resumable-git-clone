# rgc — Resumable Git Clone

rgc 把一次 `git clone` 拆成大量独立、幂等的小任务，write-ahead 记账，任意时刻中断后重跑即续传，产物与 `git clone` 等价。

## Language

**片（Piece）**:
一次独立、幂等的下载任务，断点续传的最小重做单位。两种：每分支一条**链式片**，全部标签按批成**标签批片**。
_Avoid_: 任务、分块、chunk

**链式片（Chain piece）**:
单分支历史用 shallow 链切分出的一条 `--depth`/`--deepen` 步进链；链内串行，浅边界消失即完成。

**标签批片（Tag batch）**:
一批标签按钉住 OID 在一次连接里 fetch 的片；绝不加 `--depth`。

**主仓库（Main repo）**:
`dest/.git`，等价 `git clone` 的最终产物。

**片仓库（Piece repo）**:
`dest/.rgc/pieces/<name>/` 下的临时仓库，片在此下载，经 `--shared` 借用主仓库对象；完成后搬运进主仓库。
_Avoid_: 临时仓库、缓存仓库

**搬运（Transport）**:
把片仓库的成果以本地 fetch 写进主仓库；链式片只在链完成后做一次 final 搬运（git 禁止从浅仓搬运）。

**Plan / 指纹（Fingerprint）**:
`plan.json` 记录片集合；指纹是 url + 有序片 id 的 FNV-1a 哈希，state.json 携带之，不匹配 = 大声报错，绝不静默重规划。

**台账（State / Ledger）**:
`state.json`，断点唯一真相源；write-ahead——先落盘 Running 再执行。任何故障的恢复路径都收敛到"重读台账、继续调度"。
_Avoid_: 进度文件、检查点

**认领（Claim）**:
worker 在台账锁内把 Pending 片置 Running 并落盘；认领时对耗尽预算 rebill（attempts/rate_limits 清零）。

**对账（Reconcile）**:
台账缺失/损坏时，fsck 验证主仓库完整性后按已存在 refs 重建台账；不健康则擦除 remote 侧引用与片仓库再验证，仍失败即放弃。

**Throttle（降速器）**:
run 级降速 module，独占全部 run 级限流信号：冷却、断路器、并发减半闸门、风暴退避升级。interface：`on_failure(kind)` / `tripped()` / `storm_backoff(n)` / `wait_turn(worker, has_pending)`。片级连续限流计数不在其中——那是台账状态。
_Avoid_: Governor、RateLimiter、冷却器（冷却是它的内部机制之一，不是整体）

**冷却（Cooldown）**:
限流/拥塞后布防的全局等待窗口，后续片一律推迟到其之后；等待时循环重读直到归零。

**断路器（Breaker）**:
run 级总 429 计数超阈值即整个 run 放弃（"upstream rate-limited — rerun later"），重跑续传。

**并发减半闸门（Halving gate）**:
总 429 越过软阈值后，新片认领按一半并行进行，保持到 run 结束；Pending 清空即放行被闸 worker。

## 示例对话

> Dev：这片被 429 打回来了，预算烧了吗？
> 领域：不烧。限流不计片的重试预算，连续次数记在台账里；但 Throttle 会给整个 run 布防冷却，总 429 数也 +1。
> Dev：那什么时候放弃？
> 领域：两层。片级：连续限流超上限，片落 Failed；run 级：总 429 越过断路器，整个 run 直接收工，重跑续传。过了软阈值还会把并发减半。
> Dev：worker 被减半闸门拦下后一直等着？
> 领域：等到 Pending 清空就放行退出——风暴消退时 halving 不恢复，不放行会挂死 join。
