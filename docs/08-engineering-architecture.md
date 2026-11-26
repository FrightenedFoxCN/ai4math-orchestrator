# 08. 工程实现架构与 DSH 插件集成方案（Rust Core + DSH Plugin）

## 1. 总体进程与分层拓扑

结合单机多核性能、强沙箱安全约束以及 macOS 实测探针结果，工程实现采用 **“Rust 原生确定性底座（独立宿主机守护进程）+ 极薄 TypeScript DSH 插件（编排工具面 + Live HUD）”** 的两层架构：

```text
┌─────────────────────────────────────────────────────────────────────────┐
│ DSH Web / Electron Desktop GUI（浏览器或桌面壳）                         │
│  ├─ [shell.bottom slot]      常驻生命体征条 (Live Vital Signs HUD)       │
│  └─ [ctx.sidebarRightTabs]   双语复盘简报 / 前沿 DAG / Inbox / Wiki 面板 │
└───────────────────────────────▲─────────────────────────────────────────┘
                                │ Hop 2: DSH 官方 Typert RPC + Remote Stream
                                │        (@deepseek-ai/dsh-api-gateway & dsh-client-connection)
                                │        Web 走 /api/remote.mux WS；Electron 走 IPC，自动认证与重连
┌───────────────────────────────▼─────────────────────────────────────────┐
│ DSH Host Server Plugin（plugins/dsh-ai4math，薄 TypeScript Cordis 插件） │
│  ├─ 按 7 角色物理注册隔离工具面（落实 docs/02 权限矩阵）                 │
│  ├─ TypertRemoteService 暴露给前端 HUD 的一元 RPC 与实时生命体征流       │
│  └─ 承接 P0 Push Alert 与 PivotActionPlan 落盘联动                      │
└───────────────────────────────▲─────────────────────────────────────────┘
                                │ Hop 1: 本地 Unix Domain Socket (.runtime/ai4mathd.sock, 0600)
                                │        NDJSON JSON-RPC 2.0 + Server Push 流（零暴露 TCP 端口）
┌───────────────────────────────▼─────────────────────────────────────────┐
│ 独立宿主机常驻进程：ai4mathd（Rust Workspace，运行于非嵌套沙箱环境）     │
│  ├─ ai4math-core    : 强类型契约 / Trust 偏序 / 分引擎静态禁入扫描器     │
│  ├─ ai4math-store   : rusqlite(bundled+fts5) state.sqlite & .mathwiki   │
│  ├─ ai4math-broker  : Seatbelt(canonicalize)/bwrap + PGID + APFS 黄金池  │
│  │                    + Lean 4 / Agda / Rocq 洁净室 VerificationReceipt  │
│  └─ ai4math-daemon  : UDS RPC + 实时生命体征聚合 + Fact Pack 编译器      │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## 2. 本机探针实测约束（Darwin arm64 必须遵守的硬不变量）

在动工前对本机（`Darwin arm64`：`rustc 1.99.1`、`Lean 4.35.0`、`Agda 2.8.0.2`、`Rocq 9.3.0`、`Z3 5.4.1`）执行 `sandbox-exec` 与 APFS `cp -cR` 实测后，确认以下两条不可违背的工程不变量：

1. **`ai4mathd` 必须作为独立非沙箱宿主机进程运行**：
   macOS Seatbelt 的 `sandbox_apply` **不支持嵌套**（在受沙箱约束的子进程内再调 `sandbox-exec` 会直接报 `Operation not permitted`）。因此 `ai4mathd` 绝不可作为受工具沙箱限制的 Agent 子进程启动，必须以独立宿主机服务运行，由 DSH Host 插件通过 UDS 连接。
2. **Seatbelt 策略路径必须强制 `std::fs::canonicalize`**：
   macOS 下 `/tmp` 等路径为指向 `/private/tmp` 的符号链接，Seatbelt 内核仅按真实 canonical 路径匹配规则。若未规范化，既会导致写白名单误拦（如 Agda 写 `.agdai` 报权限拒绝），也会导致读黑名单穿透。Rust 侧生成任何 `.sb` profile 前，**每一条读写路径必须先经 `std::fs::canonicalize` 处理**。

实测验证：规范化真实路径后，Seatbelt 100% 拦截越界写、机密读与外连网络；同时 `lean`（含 `#print axioms`）、`agda --safe --without-K`、`coqc`（含 `Print Assumptions`）、`z3` 及 APFS `cp -cR` 全部在沙箱内一次通过。

---

## 3. Rust Workspace 模块划分

```text
crates/
├── ai4math-core/       # 零 IO 纯逻辑层（仅 serde + sha2）
│   ├── contracts       # 与 schemas/*.json 对应的 serde 强类型结构
│   ├── domain / pack_itp # 通用 EpistemicLevel / EpistemicDebt / ScientificTrust / DomainPack + 内置 ItpDomainPack
│   ├── compute         # 多维 ResourceRequest (CPU/RAM/GPU/Scratch/Wall) 与 SlurmClusterPolicy
│   ├── trust           # L0–L5 偏序、AND-OR effective_trust_level 聚合纯函数（委托 domain 通用代数）
│   ├── preflight       # Lean 4 / Agda / Rocq 静态禁入扫描器与 Diff 范围审计
│   └── [规划] belief / knowledge # 双通道 Subjective Logic BP 引擎与四层知识本体（ADR-0007 阶段 1）
├── ai4math-evidence/   # 零 IO 证据 wire/hash、ProtocolSpec 编译与 unsafe 只读封印端口；不持有构造权
│   ├── sealed          # 领域无关 SealedReceipt 与 ProtocolRunEvidence 封印端口
│   ├── protocol        # 冻结 ProtocolSpec、单遍槽位编译、物理健全性门禁、盲测对照与预注册审计
│   ├── compute         # StagedJobBundle、BlindedArrayPlan、ClusterMountMap 与防注入 SLURM 原语
│   └── [规划] literature # 领域特异论文抽取溯源门禁与 6 维离线漂移测试床（ADR-0007 阶段 4 后置）
├── ai4math-store/      # rusqlite (bundled + fts5) WAL 持久化层，只依赖 core/evidence（严禁依赖 broker）
│   ├── state           # state-store.sql + 迁移 1–8（含 protocol_trials 迁移 7 与 compute_jobs 迁移 8）
│   └── wiki            # wiki-index.sql、.mathwiki/*.md 双向同步、符号/FTS5 混合检索与 Shadow Hint
│                       # [规划] 迁移 9 泛型主图 + 平滑演进四层 KnowledgeVault 与可插拔 SemanticIndex（阶段 2）
├── ai4math-broker/     # 验证、沙箱、多维 GPU/SLURM 算力与洁净室准入引擎；独占构造封印证据
│   ├── sandbox         # macOS Seatbelt (canonicalize)；Linux bwrap 后端尚未实现
│   ├── resources       # 既有 ITP Fast/Heavy 双车道优先级队列与内存水位线准入控制
│   ├── compute/        # LocalComputePool (GPU逐卡/统一内存) + ComputeBackend + SlurmTransport (LocalCLI/SSH)
│   ├── protocol / protocol_async # 同步与 3 阶段异步协议验证 + 匿名盲测 Job Array 解盲封印
│   ├── golden_cache    # Commit-keyed 黄金快照池（APFS cp -cR / clonefile + pinned 保活 + LRU GC）
│   ├── cleanroom       # Lean Comparator / Agda --safe / Rocq Print Assumptions 裁判包装器与 Receipt 签发
│   └── [规划] idle_scheduler # 空闲算力复现/形式化排期器（VoI + 人类队列，ADR-0007 阶段 3）
├── ai4math-project/    # 项目根/runtime/SQLite 安全门禁与原生租约门面
├── ai4math-daemon/     # 二进制 ai4mathd：UDS JSON-RPC 2.0 + 实时生命体征流 + Fact Pack 编译器
└── ai4math-cli/        # 二进制 ai4math：项目初始化 / 状态查询 / Wiki 索引重建 / Red-Team 安全自检
```

### 关键 Crate 选型
* **SQLite**：`rusqlite`（启用 `bundled` + `fts5` 特性）；当前使用 `std::thread` 与 SQLite 写事务串行化，不依赖 tokio。
* **异步与并发**：当前 daemon 使用 `std::thread`、UDS 与有界 channel；tokio 是早期设计目标，尚未引入。
* **序列化与哈希**：`serde` / `serde_json` / `serde_yaml` + `sha2`（`patch_sha256`、`receipt_sha256`、`def_fingerprint`）。
* **证据边界**：`ai4math-evidence` 只定义 wire/hash 和 `unsafe` 只读端口；Broker 独占构造封印类型，Store 不反向依赖 Broker。端口防止普通 safe Rust/JSON 伪造，不防御恶意 `unsafe` 依赖或直接篡改 SQLite。

---

## 4. DSH 插件集成规范（`plugins/dsh-ai4math/`）

### 4.1 通信协议（遵循 DSH 官方传输层）
* **Host ↔ Rust Daemon（Hop 1）**：DSH Host 插件以普通本机文件权限（`0600`）连接 `.runtime/ai4mathd.sock`，复用长连接发送 NDJSON JSON-RPC 2.0 请求并接收异步推送（`vital_signs.tick`、`alert.p0`、`breaker.state_changed`）；
* **Host ↔ Client GUI（Hop 2）**：DSH Host 插件继承 `TypertRemoteService`，通过 `@deepseek-ai/dsh-api-gateway` 注册强类型 RPC 与多路复用 Remote Stream；前端通过 `@deepseek-ai/dsh-client-connection` 的 `ctx.remote` 消费，天然适配 Web GUI（`/api/remote.mux`）与 Desktop Electron（IPC），无需自建认证或暴露额外本地端口。

### 4.2 前端 UI 挂载点（零侵入 DSH 布局）
1. **常驻生命体征条（Always-On Live HUD）**：挂载至 `@deepseek-ai/dsh-client-ui-layout` 的全局 **`shell.bottom`** slot：
   * 默认组合未占用该席位；切换主面板时保持常驻；
   * 实时展示单机 RAM 水位、Broker 双车道负载、Trust Ledger 实时盘口、熔断指示灯与 Inbox P0/P1 徽章（0 LLM 消耗）。
2. **交互式数学工作台面板**：通过 `@deepseek-ai/dsh-client-ui-sidebar-right` 的 **`ctx.sidebarRightTabs`** 注册可停靠/分栏/全屏 Tab：
   * **复盘简报 Tab**：双语并排卡片、按需点击拉取（`pull` / `--peek`）、一键展开 L2 证据；
   * **前沿与熔断 Tab**：Active Frontier DAG 切片、可恢复熔断器状态与 `RESUME_AS_IS` / `PATCH` / `PIVOT` 快捷入口；
   * **Wiki 考古 Tab**：默认 `CORE` 检索 + `Shadow Hint` 展开 `SEDIMENT` / `GRAVEYARD`。

---

## 5. 分阶段实现顺序

* **Phase 1（Rust 核心底座 + Red-Team 闭环）**：落地 `ai4math-core` + `ai4math-store` + `ai4math-broker` + `ai4math-cli`，在真实 Lean 4.35 与 Agda 2.8 上跑通静态禁入、Seatbelt 沙箱、洁净室 `VerificationReceipt`、APFS 黄金快照、Wiki Shadow Hint 与 T3 标脏洗白自动化测试；
* **Phase 2（常驻守护进程）**：落地 `ai4math-daemon`（UDS NDJSON JSON-RPC 2.0、实时生命体征推流、确定性 Fact Pack 编译器与 P0 规则触发器）；
* **Phase 3（DSH 插件与 Live HUD）**：落地 `plugins/dsh-ai4math`（Host Cordis 角色工具面 + `TypertRemoteService` 桥接 + Client `shell.bottom` Live HUD 与 `sidebarRightTabs` 面板）。
