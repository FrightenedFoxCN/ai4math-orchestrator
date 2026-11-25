# Phase 2 / 3：本地守护进程与 DSH 工作台

## 启动顺序

### 方式 A：DSH 一体化工作台（推荐）

1. 在本仓库构建本地二进制：`cargo build --locked -p ai4math-cli -p ai4math-daemon`。
2. 在 [plugins/dsh-ai4math](../plugins/dsh-ai4math) 目录执行一次 `pnpm install`，随后在 DSH 的 **插件 → 添加插件** 中选择该目录或打包后的 tarball。Bundle 会自动注册内置的 `AI4Math Chief` Preset（`id: ai4math-chief`）并插入默认禁用的 `dsh-ai4math` 行（默认 `rolePresets: { chief: 'ai4math-chief' }`、`proofSlots: {}`）。Host 需要 DSH 0.2.2-rc.5；Web 复用 DSH 的 Typert 网关，不新增监听或鉴权层。
3. 启用 `dsh-ai4math` 后，点击左侧导航栏的 **AI4Math** 打开全屏工作台：
   - 点击 **选择文件夹** 选择空目录（自动运行 `ai4math init`）或已有项目目录（自动接管），实时核对 Unix socket 路径长度（macOS/BSD ≤ 103 字节，Linux ≤ 107 字节），多项目列表持久化在 `~/.dsh/ai4math/workbench.json`。
   - 工作台支持由 Host 托管启停 `ai4mathd`，也会自动识别并只读挂载外部终端已启动的 `ai4mathd`（绝不终止外部进程）；切换项目时 Host 热切换 UDS bridge，无需重启 DSH。
   - 点击 **新建 Chief 主编排会话**，自动注册工作区并打开绑定该项目目录与 `ai4math-chief` Preset 的新对话；对话内底部 HUD 与右侧 4 个标签页实时伴随监控，并跟随 DSH 语言在中文/英文间无刷新切换。

### 方式 B：外部终端独立启动 `ai4mathd`（含可选 T0 证明检查）

1. 在目标项目的宿主机终端执行 `cargo run --locked -p ai4math-daemon -- --project /absolute/project`（需启用实际证明检查或 `--proof-targets` 时，daemon 应在非 DSH 工具沙箱的宿主机终端运行；验证子进程仍由 Broker 的 Seatbelt 隔离）。默认每 2 秒采样，`--tick-ms N` 可在 100–60000 毫秒间调整。
2. Daemon 在该项目使用 `.runtime/ai4mathd.sock` 和原有 SQLite、Wiki 索引。Socket 为 `0600`，父目录 `0700`，仅接同 UID Unix peer；没有 TCP 监听端口。建议先停止 daemon 再清理；若上次退出留下节点，确认没有运行实例和连接后再由操作者清理，不自动覆盖、跟随或移除已有路径。
3. 在工作台中添加该项目目录（工作台会自动识别为 **外部 · 在线** 并不接管其进程），或在 `dsh-ai4math` 插件配置中显式指定 `socketPath`（亦支持 `AI4MATH_SOCKET` 环境变量）。
4. 其他角色工具默认不注册。受信任操作者可在 `rolePresets` 中将 `chief / regional / explorer / builder / prover / automation / reviewer` 映射到互不重复的已组合 Agent preset ID；授权源是实时 Agent 绑定而不是工具参数。普通角色没有任意读写文件或命令执行入口。Reviewer 不读全局简报/Inbox；其上下文装配由上游编排器负责，不应在同一个混合权限 Agent 中启用。

## 接口及约定

Socket 接受 NDJSON JSON-RPC 2.0 对象参数：`status`、`frontier.list`、`inbox.list`、`wiki.search`、`briefing.peek`、`briefing.pull`、`briefing.get`、`alerts.list`、`subscribe`、`unsubscribe`。此外 Host 侧提供 6 个仅限已认证 Owner 调用的 `hostOnly` 工作台控制端点：`workbench.state`、`workbench.project_init`、`workbench.project_select`、`workbench.project_remove`、`workbench.daemon_start`、`workbench.daemon_stop`（不经过 UDS，且永不向任何 Agent 角色暴露）。详细参数看 [Host 共用端点定义](../plugins/dsh-ai4math/host/remote-contract.mjs)；浏览器端不直接接触 UDS。推送主题为 `vital_signs.tick`、`alert.p0`、`breaker.state_changed`。连接、帧长度、未完成请求和各方向缓冲均有界。Host 不自动重放请求。重连后 P0 通过持久游标分页补读，超出本轮上限会明确显示；熔断事件本身只做实时通知，当前状态可从 Fact Pack 重读。

- **生命体征**：内存、信任账本、标脏、熔断/Inbox、最近 P0。配置证明槽后上报 daemon 内 Broker 的租约指标；独立 CLI 不计入，真实 worker 数缺少上游遥测时为未知。
- **简报**：同一 SQLite 读事务内固定游标和事件头，生成有界、确定性的事实包，包括信任迁移、形式/非形式陈述、P0 源事件、前沿及依赖、风险、熔断和 Inbox。形式签名未记录时不编造。Peek 不写文件；Pull 和有效 P0 按内容哈希留存 JSON 和 Markdown；Owner 可按摘要读回并验证内容完整性。两者都不推进人类 checkpoint。当前 RPC 输出是实施版投影，不宣称与 [设计版简报 Schema](../schemas/briefing-telemetry.schema.json) 完全等价。
- **P0**：可信 Claim Graph 的内核硬证伪和关键路线全部失效可进入告警流；支持消费 `ALL_TRACKS_BLOCKED`、`CRITICAL_BREAKER_TRIPPED`、`BROKER_REGRESSION_MELTDOWN` 的确定性领域事件。去重和冷却持久化；重复事件不会新增简报，保留失败不推进告警游标。不会依据空仓、内存瞬时波动、Oracle 或 Agent 声称制造 P0。
- **可选 T0 证明检查**：以 `--proof-targets /absolute/operator.json` 读操作者的可信目录清单。清单根对象 `{"version":1,"targets":[...]}`，每项为 `{id,frozen,executable,runtime_reads?,agda_data?,rocq_data?}`，`frozen` 包含 `engine,target_id,file_path,prefix,suffix`。Agda 与 Rocq 分别只接受对应的绝对数据目录；Lean 不接受该字段。Host 只将 `proofSlots.prover` / `proofSlots.automation` 中批准的槽暴露给对应 preset；Agent 只能传 `target_id` 和 `proof_body`，不能更改模板/编译器或发起合入。UI 无此接口，T0 返回通过、拒绝或不可用，没有合入回执。Host 中止等待不等于取消已进入 Broker 的工作；Broker 自身有有限运行时。

## 验收与复现

- `cargo fmt --all -- --check`；`cargo test --workspace --locked`。宿主机上按 [Phase 1](phase-1.md) 的环境变量补齐工具链，可运行 `cargo test --workspace --locked -- --include-ignored`；macOS 原生 Seatbelt 用例必须在非外层沙箱终端执行。历史验收数量以[验收矩阵](architecture-acceptance.md)对应批次为准，当前实现状态见 [implementation-status.md](implementation-status.md)。
- `npm --prefix plugins/dsh-ai4math test`：Host/Client 标准测试，并先执行 `node plugins/dsh-ai4math/client/build.mjs --check` 验证发布 bundle 与源码一致。Browser VM 确认 `shell.bottom`、四个侧栏标签注册/销毁，穿过 Host 的实际 UDS 通知、重连、P0 补读和取消。
- 已用本机安装的 DSH Cordis + Typert Registry/Gateway，运行 [installed-dsh.mjs](../plugins/dsh-ai4math/host/test/installed-dsh.mjs)，验证合成 Owner/Guest peer 的角色门禁、严格参数、从真实 Rust UDS 流式转发、真实 daemon 重启重连及 Cordis 卸载后释放 stream 和服务注册。这是独立进程内的真实 SDK 集成测试；不测试浏览器登录、当前 profile 或真实 GUI 布局。

```sh
APP='/Applications/DeepSeek Harness.app'
cargo build --locked -p ai4math-daemon -p ai4math-cli
ELECTRON_RUN_AS_NODE=1 "$APP/Contents/MacOS/DeepSeek Harness" \
  plugins/dsh-ai4math/host/test/installed-dsh.mjs \
  "$APP/Contents/Resources/app.asar/dsh" "$PWD/target/debug/ai4mathd" "$PWD/target/debug/ai4math"
```

最后一个 `ai4math` 参数可选；传入后临时项目会用正式 CLI 登记双轨任务、触发非关键熔断并经 Gateway 验证全轨停摆 P0 与留存 Fact Pack。之后用正式 CLI 预览和批准仅归档熔断，经 Gateway 核对精确归档事件、持续队列门禁与计数，并确认没有远程审批入口。

### 驻留 T2 经安装版 DSH 网关验收

在允许项目自身 Seatbelt 的 macOS 宿主上运行。测试只使用其临时 Git/SQLite 项目，不启动或更改当前 GUI：

```sh
APP='/Applications/DeepSeek Harness.app'
LEAN="$(lean --print-prefix)/bin/lean"
cargo build --locked -p ai4math-daemon -p ai4math-cli
env -i PATH=/usr/bin:/bin ELECTRON_RUN_AS_NODE=1 "$APP/Contents/MacOS/DeepSeek Harness" \
  plugins/dsh-ai4math/host/test/installed-t2-audit.mjs \
  "$APP/Contents/Resources/app.asar/dsh" "$PWD/target/debug/ai4mathd" "$PWD/target/debug/ai4math" "$LEAN"
```

在上面的命令最后加 `--independent` 可验收配置版本 2：临时库提交固定清单，真实 Nanoda 复核后，安装版 Typert 的 Owner 状态/推流仅返回脱敏计数。需要 Lean 安装目录中的原生 `leanexport` 和 `nanoda_bin`。 在同一命令最后改用 `--heartbeats` 可验收配置版本 3：临时 Git 基线与候选触发真实成对 Lean 采样，并验证安装版 Typert 只转发脱敏的阈值判定。


## 尚未覆盖的完整产品设计

现用 DSH Desktop GUI 已完成 Owner 视角实装验收（左侧 **AI4Math** 全屏工作台、多项目初始化/热切换、托管与外部 daemon 识别、底栏 HUD 与 4 个右侧栏面板、断线自动重连、禁用热移除、一键新建 `ai4math-chief` 会话与中英文跟随）。以下边界仍待后续或目标环境单独验收：

- **访客/登出态浏览器实测与多角色生产 Preset**：Owner/Guest 门禁已在安装版 SDK 测试验证，浏览器侧退出登录或 Guest 视角及 `chief` 以外的独立角色 Preset（如盲审 `reviewer`、`prover` 证明槽）仍待真实多会话验收。
- **主机信任边界**：标准 Unix UID 是 daemon 的信任边界，不隔离同 UID 的其他本机进程；如给模型启用其他 shell/文件工具，部署者还需禁止其直接访问 UDS/状态库。角色授权是 preset 级别，不是完整任务租约。若托管 `ai4mathd` 需开启 `--proof-targets` 执行 Broker Seatbelt，仍建议先按目标系统核对 Electron Host 子进程与 Seatbelt 的兼容性，必要时采用外部终端启动 daemon。
- **只读与写操作分离**：GUI 未接任务调度控制面、外部桌面/Webhook 通知、任意历史简报窗口、LLM 叙事，也未暴露 Pivot/熔断恢复/ADR 写事务（熔断与收件箱卡片仅提供可复制的只读操作者 CLI 模板）；这些不等同于 CLI 或独立 Host 功能不存在。工作台可观察和留存 Fact Pack，但不会批准、解决 Inbox 或推进 checkpoint。后续写入口必须满足 [Owner 人工确认边界](owner-approval-boundary.md)。
