# ai4math-orchestrator

面向**开放数学问题探索**的单机多核 AI4Math 编排与形式化验证工作流设计仓库。

## 核心场景与约束

1. **开放数学问题探索**：中间猜想高度流动（经常需要证伪、修正或重构抽象），非形式化创意与形式化验证深度交织。
2. **大规模边探边建基建**：Mathlib 现有覆盖不足以支撑目标问题，必须在探索过程中并行派发子代理搭建长期可复用的形式化基础设施。
3. **多引擎协同**：项目初始化时显式选定单一主引擎（**Lean 4 / Agda / Rocq**，默认 Lean 4），默认禁用跨 ITP 公理桥接防语义漏洞；按需经由 `tool_automation` 子代理调度 Hammer、SMT/ATP 与 CAS（含剪枝前交叉印证防线）。
4. **单机多核本地运行**：在严格的内存与 CPU 水位线约束下，通过中央验证调度器与写时复制（CoW）隔离工作区支撑高频回归。
5. **无人值守 + 异步人机共驾**：系统可长时间自主推进，同时提供非阻塞决策收件箱（Decision Inbox）与 5 分钟复盘简报，支持人类随时介入讨论、纠偏与拍板。

## 设计文档目录

| 文档 | 内容说明 |
| :--- | :--- |
| [docs/01-architecture-overview.md](docs/01-architecture-overview.md) | 总体架构蓝图：四大本质张力、双轨编排模型、多引擎信任账本（L0–L5）、单机验证调度与四级回归节奏、异步人机共驾协议及分阶段路线图 |
| [docs/02-roles-permissions-contracts.md](docs/02-roles-permissions-contracts.md) | 5 角色拓扑、物理工具权限矩阵与四道防火墙、结构化交接契约（Dispatch / Result / InfraRFC / Escalation） |
| [docs/03-local-math-wiki.md](docs/03-local-math-wiki.md) | 本地数学 Wiki 索引设计：三层沉降库（CORE / SEDIMENT / GRAVEYARD）、惰性重验、三级渐进唤醒（含 Shadow Hint）、四维混合检索与双层存储 |
| [docs/04-review-briefing-and-hitl.md](docs/04-review-briefing-and-hitl.md) | 常驻生命体征监控（Live HUD）、按需+P0推送双语复盘简报、可恢复 Definition Smell 局部熔断器与 Pivot Dry-Run 闭环 |
| [docs/05-multi-engine-and-hammers.md](docs/05-multi-engine-and-hammers.md) | 多引擎协同、对称 Trust Ledger（`L2a/L2b`, `L4a/L4b`）、项目级可配置主引擎、按需 Hammer 工具子代理与 Oracle 剪枝前补充验证防线 |
| [docs/06-verification-broker.md](docs/06-verification-broker.md) | Local Verification Broker：四层纵深防御（静态禁入、OS 混合沙箱、洁净室形式化准入与 `VerificationReceipt`）、双车道调度、内存水位线与黄金 `.olean` 快照复用 |
| [docs/07-claim-graph-state-machine.md](docs/07-claim-graph-state-machine.md) | AND-OR Claim Graph 状态机、双信任度（`local` vs. `effective`）、T3 标脏传播与机会主义快速洗白、`L2b` 证伪传播与单文件 SQLite Event Log |
| [docs/08-engineering-architecture.md](docs/08-engineering-architecture.md) | 工程实现架构：Rust 原生 Workspace（`rusqlite` + Seatbelt 规范化探针结论）、UDS + DSH `dsh-api-gateway` 两跳通信与 `shell.bottom` Live HUD 集成 |
| [docs/09-scientific-generalization-and-belief-vault.md](docs/09-scientific-generalization-and-belief-vault.md) | 跨学科泛化架构：`DomainPack` / 反漂移 `ProtocolSpec`、多维 GPU/SLURM 算力调度、四层 `KnowledgeVault` + 可插拔向量检索、泛型主状态图双通道 Belief Propagation、空闲算力复现排期（含人类队列）与后置论文抽取漂移测试床 |
| [docs/adr/ADR-0001-core-workflow-decisions.md](docs/adr/ADR-0001-core-workflow-decisions.md) | 架构决策记录 #0001：首轮核心编排、权限与知识库策略拍板（D1–D6） |
| [docs/adr/ADR-0002-review-briefing-and-telemetry.md](docs/adr/ADR-0002-review-briefing-and-telemetry.md) | 架构决策记录 #0002：生命体征解耦、Push Alert、双语呈现与可恢复熔断器决策（D7–D11） |
| [docs/adr/ADR-0003-multi-engine-oracles-and-hammers.md](docs/adr/ADR-0003-multi-engine-oracles-and-hammers.md) | 架构决策记录 #0003：对称 Trust Ledger、主引擎可配置、默认禁用跨 ITP 桥接、动态 Hammer 与 Oracle 防线（D12–D15，修订 D4） |
| [docs/adr/ADR-0004-verification-broker-and-sandboxing.md](docs/adr/ADR-0004-verification-broker-and-sandboxing.md) | 架构决策记录 #0004：Broker 四层防御、`L5` 禁入 `native_decide`、混合 OS 沙箱与 T2 外部内核校验（D16–D19） |
| [docs/adr/ADR-0005-claim-graph-and-artifact-cache.md](docs/adr/ADR-0005-claim-graph-and-artifact-cache.md) | 架构决策记录 #0005：AND-OR Claim Graph、机会主义洗白、黄金 `.olean` 快照池与 SQLite 状态底座（D20–D23） |
| [docs/adr/ADR-0006-implementation-stack-and-dsh-integration.md](docs/adr/ADR-0006-implementation-stack-and-dsh-integration.md) | 架构决策记录 #0006：Rust + `rusqlite` 原生核心、非嵌套守护进程与路径规范化、UDS + DSH 官方网关两跳通信与 Live HUD 席位（D24–D27） |
| [docs/adr/ADR-0007-scientific-generalization-compute-vault-and-belief.md](docs/adr/ADR-0007-scientific-generalization-compute-vault-and-belief.md) | 架构决策记录 #0007：跨学科领域包、反漂移 `ProtocolSpec`、多维 GPU/SLURM 调度、四层知识库与向量检索、双通道 Belief Propagation、空闲复现排期与后置论文抽取（D28–D33） |
| [schemas/contracts.schema.json](schemas/contracts.schema.json) | 编排契约、信任账本与决策收件箱的 JSON Schema 定义 |
| [schemas/briefing-telemetry.schema.json](schemas/briefing-telemetry.schema.json) | 常驻生命体征、复盘 Fact Pack、Push Alert、可恢复熔断器与 Pivot 计划的 JSON Schema |
| [schemas/multi-engine.schema.json](schemas/multi-engine.schema.json) | 项目主引擎配置、Oracle 交叉印证记录、Hammer 探测结果与适配器能力的 JSON Schema |
| [schemas/verification-broker.schema.json](schemas/verification-broker.schema.json) | Broker 配置、静态禁入扫描、资源租约、洁净室 `VerificationReceipt` 与 Promotion 结果的 JSON Schema |
| [schemas/wiki-index.sql](schemas/wiki-index.sql) | 本地 Wiki SQLite 索引库 DDL（关系表、符号倒排、图谱边表、FTS5 与可选向量表） |
| [schemas/state-store.sql](schemas/state-store.sql) | 运行时状态与 Append-Only Event Log SQLite DDL（Claims / ProofPlans / Golden Snapshots / Receipts / Breakers / Inbox / Cursors） |
| [schemas/protocol-verification.sql](schemas/protocol-verification.sql) | 迁移 7：不可变预注册与只追加协议试错账本 DDL（`protocol_preregistrations` / `protocol_trials`） |
| [schemas/compute-jobs.sql](schemas/compute-jobs.sql) | 迁移 8：本地 GPU 与 SLURM 异步作业持久状态与重启恢复账本 DDL（`compute_jobs`） |

## 阶段状态（Roadmap）

交付顺序以[工程架构 §5](docs/08-engineering-architecture.md#5-分阶段实现顺序)为准；实现与验收见 [Phase 1](docs/phase-1.md) 和 [Phase 2 / 3](docs/phase-2-3.md)。

- [x] **架构与工程选型设计完成（Docs 01–08、ADR-0001–0006、4 份 JSON Schema 与 2 份 SQLite DDL）**
- [x] **Phase 1（Rust 核心底座 + Red-Team 闭环）**：`ai4math-core` + `ai4math-store`（`rusqlite`）+ `ai4math-broker`（Seatbelt 规范化 + APFS 黄金快照 + Lean 4/Agda/Rocq 受限洁净室 `VerificationReceipt`）+ `ai4math-cli`
- [x] **Phase 2（本机守护进程）**：`ai4math-daemon`（UDS NDJSON JSON-RPC 2.0 + 实时生命体征推流 + 确定性 Fact Pack 编译器）
- [x] **Phase 3（DSH 一体化工作台插件）**：`plugins/dsh-ai4math`（内置 `ai4math-chief` Preset、左侧 `sidebar.panellist` + 全屏 `main:ai4math` 多项目工作台、Host 托管/外部接管 `ai4mathd` 与热切桥接、底部 `shell.bottom` Live HUD、4 个右侧栏面板、会话目录联动、只读 CLI 助手与中英文实时切换）

- [x] **跨学科泛化底座（[ADR-0007](docs/adr/ADR-0007-scientific-generalization-compute-vault-and-belief.md) D28–D30）**：`DomainPack` / `EpistemicLevel` / `ScientificTrust` / `SealedReceipt`、反漂移 `ProtocolSpec` 引擎 + 迁移 7 试错账本（`7648418`），以及本地 GPU 逐卡/统一内存池 + 双层 `ComputeBackend` / `SlurmTransport` + 迁移 8 异步作业账本（`97a5028`）
- [ ] **泛化后续阶段（[09 §7](docs/09-scientific-generalization-and-belief-vault.md#7-分阶段实施路线图)）**：阶段 1（双通道 Subjective Logic BP 与知识本体）→ 阶段 2（泛型主状态图迁移 9 + 平滑演进四层知识库与可插拔向量检索）→ 阶段 3（空闲算力复现排期 + 人类队列）→ 阶段 4（先针对数学基线文献验证论文抽取溯源门禁与漂移测试）→ 阶段 5（拓展 CompChem / CompBio 文献卡型与计算试点）

以上勾选表示阶段实现与本机/Desktop GUI 工作台验收已落地，不表示全平台生产交付。当前交接基线为 `97a5028`：Rust 标准套件 614 通过 / 0 失败 / 77 默认忽略，DSH 插件 119 通过 / 0 失败；本机 daemon、安装版 DSH SDK 集成测试与现用 Desktop GUI 工作台（Owner 视角）已验收通过。Owner 归档仍只开放本地签名 CLI，不开放远程写入口；Linux 验证后端与真实 SLURM 集群实跑仍待验收；Rocq 独立 T2 原生复核未通过，Agda 独立 T2 和 Lean 全仓发布门禁仍待完成。

### 快速开始（DSH Desktop 工作台）

1. 编译本地 Rust 二进制：`cargo build --locked -p ai4math-cli -p ai4math-daemon`。
2. 在 [plugins/dsh-ai4math](plugins/dsh-ai4math/README.md) 执行 `pnpm install`，于 DSH **插件 → 添加插件** 中添加该目录并启用 `dsh-ai4math`。
3. 点击左侧导航栏 **AI4Math** 打开工作台 → **选择文件夹**初始化或接管数学项目（macOS 需保证 `.runtime/ai4mathd.sock` 路径 ≤ 103 字节）→ 点击 **新建 Chief 主编排会话** 即可开始编排探索。

当前验证结果和恢复顺序见[交接快照](docs/implementation-status.md#交接快照)与[交付清单](docs/delivery-roadmap.md)；历史证据见[验收矩阵](docs/architecture-acceptance.md)。
