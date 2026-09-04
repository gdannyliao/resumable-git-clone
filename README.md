# rgc — resumable git clone

`rgc`（"resumable git clone"）面向**超大仓库**（chromium / monorepo 级）的断点续传克隆工具：
把一次 `git clone` 拆成大量独立、幂等的小任务（每分支一条 `--depth/--deepen` 链 + 标签批片），
write-ahead 状态机记账，**任意时刻杀掉进程（kill -9）、断网、429 限流，重跑即续传**，
产物与 `git clone` 等价（全历史 + 全分支 + 全标签，含等价性 oracle 验收）。

## Why

git smart protocol 无法字节级续传：一次 `git clone` 中断后只能从头再来。rgc 把克隆切分成
独立幂等的片（piece），每片完成后落盘记账；失败只重做当前片，而不是整个仓库。

## Requirements

- git ≥ 2.26（测试套件需要 ≥ 2.28）
- macOS / Linux（Windows 未支持：路径与文件锁行为未验证）
- 本工具不碰代理/镜像，直连远端

## Install

```sh
cargo install --path .   # 开发安装；暂无发布渠道
```

## Usage

```sh
# 克隆（中断后重跑同一条命令即续传）
rgc clone https://example.com/huge.git my-dir

# 显式恢复（也可直接重跑 clone，二者等价）
rgc resume my-dir

# 查看各片进度（只读，不加锁，可在克隆进行中随时查看）
rgc status my-dir
```

| Flag | Default | 说明 |
|---|---|---|
| `--jobs <N>` | 2 | 并行 worker 数（1–8；过高易触发服务端限流） |
| `--piece-target <s>` | 600 | 每片目标耗时（秒）：自适应步长按吞吐换算单步提交数 |
| `--keep-state` | off | 完成后保留 `dest/.rgc/`（调试/取证用） |

## How it works

1. **Plan**：`ls-remote` 把远端切成片——每分支一条链式片（`--depth` 起步、`--deepen`
   逐步加深至浅边界消失），全部标签一批片（按 OID 钉住 fetch）。plan.json 记录片的
   指纹（URL + 片集合），换远端/换片集会在 resume 时大声报错，绝不静默重规划。
2. **State**：`dest/.rgc/state.json` write-ahead——先落盘 Running 再执行，成功/失败都
   回写；进程被杀后 Running 折返 Pending 重做。每片在独立片仓库 fetch（`--shared`
   借用主仓库对象），完成后才搬运进主仓库。
3. **Scheduler**：N 个 worker 认领片；网络错误按类分诊——限流（429）**不计入片的重试
   预算**（独立连续计数 + 全局冷却 + run 级断路器），网络错误指数退避，致命错误立即
   终止并保留现场。
4. **Finalize**：catch-up fetch 收敛 ls-remote 之后的漂移（强移 tag / 删 tag / 新分支），
   checkout 默认分支，最后以等价性 oracle（refs + `rev-list --objects --all` 对象闭包）
   对照新鲜远端验证。

### Layout

```
dest/
├── .git/            # 主仓库（等价 git clone 的产物）
└── .rgc/            # rgc 现场：plan.json + state.json + pieces/（完成后默认删除）
```

## Error model

| 情形 | 行为 |
|---|---|
| 429 / 滥用检测 | 不烧片预算；全局冷却 + 退避升级；连续超限放弃该片；run 级总量超阈值整轮放弃（重跑续传） |
| 网络错误 | 指数退避重试，片步长减半 |
| 致命（远端 ref 消失、仓库损坏等） | 片终态 Failed，run 报告明细后退出；修复后重跑继续 |
| kill -9 / 断电 | 状态文件原子写，永不撕裂；重跑即续传（有验收测试） |

## Limitations（诚实清单）

- 等价性以 **plan 时刻的远端快照** 为基准；finalize 会收敛 ls-remote 之后的漂移，但
  plan 之前即存在的本地分支不会被克隆（`git clone` 的默认行为也是只有远端分支）。
- 等价 oracle（开发/验收用）把全对象图驻留内存，chromium 级（~15M 对象）不适用；
  生产路径不运行 oracle。
- 分支/标签名与已有 ref 构成 D/F 冲突的远端不受支持（git 自身限制）。
- 不校验远端证书策略之外的网络配置；不处理交互式凭据（`GIT_TERMINAL_PROMPT=0`）。

## Development

```sh
cargo test            # 全量测试
cargo test -- --ignored   # kill -9 韧性验收（较慢，~30s）
```

## License

TBD
