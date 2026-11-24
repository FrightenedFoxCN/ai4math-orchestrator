# ADR-0006：Rust 原生核心底座、rusqlite 持久化与 DSH 官方两跳通信集成决策

- **状态**：已确认（Accepted）
- **日期**：2026-11-24
- **关联文档**：[08-engineering-architecture.md](../08-engineering-architecture.md)

## 决策清单

### D24. 确定性核心底座采用 Rust 原生 Workspace + `rusqlite`（bundled + FTS5）
- **决定**：将契约校验、静态禁入扫描、AND-OR Claim Graph 与本地 Math Wiki 存储、OS 沙箱调度、APFS 黄金快照管理、洁净室 `VerificationReceipt` 签发及确定性 Fact Pack / 生命体征聚合全部以 Rust Workspace 实现；SQLite 驱动采用 `rusqlite`（`bundled` + `fts5`）配合 `tokio::task::spawn_blocking` 串行写事务。
- **理由**：单机高并发进程管控、内存看门狗与文件系统操作需要极低的运行时开销与确定性资源回收；`rusqlite` bundled 模式彻底消除跨机器 SQLite 库与 FTS5 版本差异。

### D25. `ai4mathd` 独立非沙箱运行 + Seatbelt 路径强制 `canonicalize`
- **决定**：
  1. Rust 守护进程 `ai4mathd` 作为独立宿主机常驻进程启动，绝不运行在嵌套的 Agent 工具沙箱内部；
  2. 生成 macOS Seatbelt `.sb` 策略前，所有文件路径强制调用 `std::fs::canonicalize` 解析符号链接（如 `/tmp → /private/tmp`）。
- **理由**：本机 `Darwin arm64` 探针实测证实 macOS 不允许嵌套 `sandbox_apply`，且 Seatbelt 仅按 canonical 真实路径匹配规则；遵循该约束后 `lean` 4.35、`agda --safe` 2.8、`coqc` 9.3、`z3` 5.4 与 APFS `cp -cR` 全部通过验证。

### D26. 遵循 DSH 官方两跳通信架构（UDS + `dsh-api-gateway`）
- **决定**：
  1. **Hop 1（`ai4mathd` ↔ DSH Host 插件）**：本地 Unix Domain Socket（`.runtime/ai4mathd.sock`，`0600` 权限），采用 NDJSON JSON-RPC 2.0 + 服务端推送流，零暴露本地 TCP 端口；
  2. **Hop 2（DSH Host 插件 ↔ DSH Client 插件）**：复用 DSH 官方 `@deepseek-ai/dsh-api-gateway`（`TypertRemoteService` 一元 RPC + 多路复用 Remote Stream）与 `@deepseek-ai/dsh-client-connection`。
- **理由**：天然复用 DSH 认证、断线重连与流控，且同时透明兼容 DSH Web GUI（`/api/remote.mux`）与 Desktop Electron（IPC）。

### D27. Orchestrator 工具面与常驻 Live HUD 以 DSH 原生插件落地
- **决定**：
  1. 编排器与角色隔离工具面通过 DSH Host Cordis 插件注册；
  2. 常驻生命体征条挂载至 `@deepseek-ai/dsh-client-ui-layout` 全局 `shell.bottom` slot；
  3. 双语复盘简报、前沿 DAG、Decision Inbox 与 Math Wiki 面板通过 `@deepseek-ai/dsh-client-ui-sidebar-right` 的 `ctx.sidebarRightTabs` 注册为停靠标签页。
- **理由**：零侵入复用 DSH 成熟的对话交互、模型路由与停靠布局能力，把全部研发精力聚焦在 AI4Math 领域核心底座上。
