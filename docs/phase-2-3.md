# Phase 2 / 3：本地守护进程与 DSH 工作台

## 启动顺序

1. 在目标项目的宿主机终端执行 `cargo run --locked -p ai4math-daemon -- --project /absolute/project`。启用实际证明检查时，daemon 自身不应处在 DSH 工具沙箱内；验证子进程仍必须由 Broker 的 Seatbelt 隔离。默认每 2 秒采样，`--tick-ms N` 可在 100–60000 毫秒间调整。
2. Daemon 在该项目使用 `.runtime/ai4mathd.sock` 和原有 SQLite、Wiki 索引。Socket 为 `0600`，父目录 `0700`，仅接同 UID Unix peer；没有 TCP 监听端口。建议先停止 daemon 再清理；若上次退出留下节点，确认没有运行实例和连接后再由操作者清理，不自动覆盖、跟随或移除已有路径。macOS 上若绝对路径超出 Unix socket 长度，使用物理路径更短的项目目录。
3. 在 DSH 的本地插件加载器将 [dsh-ai4math](../plugins/dsh-ai4math/package.json) 作为项目插件加载。通过插件配置指定 `socketPath` 为绝对路径（或在 DSH Host 进程设置 `AI4MATH_SOCKET`）。Host 需要 DSH 0.2.2-rc.5 的 Typert Registry/Gateway；Client 使用 Remote、layout slots 和 sidebar。Web 复用 DSH 自身网关，不新增 WebSocket 路由或鉴权层。此实现不会自行安装、启动新 DSH GUI 或更改现有 profile。
4. 角色工具默认不注册。受信任操作者可配置 `rolePresets`，将 `chief / regional / explorer / builder / prover / automation / reviewer` 映射到互不重复的已组合 Agent preset ID；授权源是实时 Agent 绑定而不是工具参数。普通角色没有任意读写文件或命令执行入口。Reviewer 不读全局简报/Inbox；其上下文装配由上游编排器负责，不应在同一个混合权限 Agent 中启用。

## 接口及约定

Socket 接受 NDJSON JSON-RPC 2.0 对象参数：`status`、`frontier.list`、`inbox.list`、`wiki.search`、`briefing.peek`、`briefing.pull`、`briefing.get`、`alerts.list`、`subscribe`、`unsubscribe`。详细参数看 [Host 共用端点定义](../plugins/dsh-ai4math/host/remote-contract.mjs)；浏览器端不直接接触 UDS。推送主题为 `vital_signs.tick`、`alert.p0`、`breaker.state_changed`。连接、帧长度、未完成请求和各方向缓冲均有界。Host 不自动重放请求。重连后 P0 通过持久游标分页补读，超出本轮上限会明确显示；熔断事件本身只做实时通知，当前状态可从 Fact Pack 重读。

- **生命体征**：内存、信任账本、标脏、熔断/Inbox、最近 P0。配置证明槽后上报 daemon 内 Broker 的租约指标；独立 CLI 不计入，真实 worker 数缺少上游遥测时为未知。
- **简报**：同一 SQLite 读事务内固定游标和事件头，生成有界、确定性的事实包，包括信任迁移、形式/非形式陈述、P0 源事件、前沿及依赖、风险、熔断和 Inbox。形式签名未记录时不编造。Peek 不写文件；Pull 和有效 P0 按内容哈希留存 JSON 和 Markdown；Owner 可按摘要读回并验证内容完整性。两者都不推进人类 checkpoint。当前 RPC 输出是实施版投影，不宣称与 [设计版简报 Schema](../schemas/briefing-telemetry.schema.json) 完全等价。
- **P0**：可信 Claim Graph 的内核硬证伪和关键路线全部失效可进入告警流；支持消费 `ALL_TRACKS_BLOCKED`、`CRITICAL_BREAKER_TRIPPED`、`BROKER_REGRESSION_MELTDOWN` 的确定性领域事件。去重和冷却持久化；重复事件不会新增简报，保留失败不推进告警游标。不会依据空仓、内存瞬时波动、Oracle 或 Agent 声称制造 P0。
- **可选 T0 证明检查**：以 `--proof-targets /absolute/operator.json` 读操作者的可信目录清单。清单根对象 `{"version":1,"targets":[...]}`，每项为 `{id,frozen,executable,runtime_reads?,agda_data?,rocq_data?}`，`frozen` 包含 `engine,target_id,file_path,prefix,suffix`。Agda 与 Rocq 分别只接受对应的绝对数据目录；Lean 不接受该字段。Host 只将 `proofSlots.prover` / `proofSlots.automation` 中批准的槽暴露给对应 preset；Agent 只能传 `target_id` 和 `proof_body`，不能更改模板/编译器或发起合入。UI 无此接口，T0 返回通过、拒绝或不可用，没有合入回执。Host 中止等待不等于取消已进入 Broker 的工作；Broker 自身有有限运行时。

## 验收与复现

- `cargo fmt --all -- --check`；`cargo test --workspace --locked`。宿主机上按 [Phase 1](phase-1.md) 的环境变量补齐工具链，运行 `cargo test --workspace --locked -- --include-ignored`，macOS arm64 **36 项全部通过**（含 Lean、Agda、T3 和 daemon 冻结槽）；随后新增的三类可信 P0 信号路由测试单独通过。Seatbelt 自检通过。
- `npm --prefix plugins/dsh-ai4math test`：Host/Client 共 **13 项**。Browser VM 确认 `shell.bottom`、四个侧栏标签注册/销毁，穿过 Host 的实际 UDS 通知、重连、P0 补读和取消。部署前 `node plugins/dsh-ai4math/client/build.mjs --check` 检查发布包是否匹配源码。
- 已用本机安装的 DSH Cordis + Typert Registry/Gateway，运行 [installed-dsh.mjs](../plugins/dsh-ai4math/host/test/installed-dsh.mjs)，验证 Owner unary、非 Owner 拒绝、严格参数及从真实 Rust UDS 向 Gateway 流式转发。这是独立进程内的集成测试，不是对当前用户 GUI 的装载测试。

```sh
APP='/Applications/DeepSeek Harness.app'
cargo build --locked -p ai4math-daemon -p ai4math-cli
ELECTRON_RUN_AS_NODE=1 "$APP/Contents/MacOS/DeepSeek Harness" \
  plugins/dsh-ai4math/host/test/installed-dsh.mjs \
  "$APP/Contents/Resources/app.asar/dsh" "$PWD/target/debug/ai4mathd" "$PWD/target/debug/ai4math"
```

最后一个 `ai4math` 参数可选；传入后临时项目会用正式 CLI 登记探索任务及其基建依赖、触发非关键熔断，并断言 Gateway 收到全轨停摆 P0，且能读取相同的留存 Fact Pack。

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


## 尚未覆盖的完整产品设计

当前 GUI 插件未安装；真实 Web/Electron 界面的认证、布局冲突及生产 preset 生命周期仍需部署验收。标准 Unix UID 是 daemon 的信任边界，不隔离同 UID 的其他本机进程；如给模型启用其他 shell/文件工具，部署者还需禁止其直接访问 UDS/状态库。角色授权是 preset 级别，不是完整任务租约。

当前未接自动排程器/worker 生命周期、其他三类 P0 的生产信号源、外部桌面/Webhook 通知、任意历史简报窗口、LLM 叙事与 Pivot/熔断恢复/ADR 事务。面板只读观察或留存，不会模拟这些操作成功。
