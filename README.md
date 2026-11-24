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
| [docs/adr/ADR-0001-core-workflow-decisions.md](docs/adr/ADR-0001-core-workflow-decisions.md) | 架构决策记录 #0001：首轮核心编排、权限与知识库策略拍板（D1–D6） |
| [docs/adr/ADR-0002-review-briefing-and-telemetry.md](docs/adr/ADR-0002-review-briefing-and-telemetry.md) | 架构决策记录 #0002：生命体征解耦、Push Alert、双语呈现与可恢复熔断器决策（D7–D11） |
| [docs/adr/ADR-0003-multi-engine-oracles-and-hammers.md](docs/adr/ADR-0003-multi-engine-oracles-and-hammers.md) | 架构决策记录 #0003：对称 Trust Ledger、主引擎可配置、默认禁用跨 ITP 桥接、动态 Hammer 与 Oracle 防线（D12–D15，修订 D4） |
| [docs/adr/ADR-0004-verification-broker-and-sandboxing.md](docs/adr/ADR-0004-verification-broker-and-sandboxing.md) | 架构决策记录 #0004：Broker 四层防御、`L5` 禁入 `native_decide`、混合 OS 沙箱与 T2 外部内核校验（D16–D19） |
| [docs/adr/ADR-0005-claim-graph-and-artifact-cache.md](docs/adr/ADR-0005-claim-graph-and-artifact-cache.md) | 架构决策记录 #0005：AND-OR Claim Graph、机会主义洗白、黄金 `.olean` 快照池与 SQLite 状态底座（D20–D23） |
| [schemas/contracts.schema.json](schemas/contracts.schema.json) | 编排契约、信任账本与决策收件箱的 JSON Schema 定义 |
| [schemas/briefing-telemetry.schema.json](schemas/briefing-telemetry.schema.json) | 常驻生命体征、复盘 Fact Pack、Push Alert、可恢复熔断器与 Pivot 计划的 JSON Schema |
| [schemas/multi-engine.schema.json](schemas/multi-engine.schema.json) | 项目主引擎配置、Oracle 交叉印证记录、Hammer 探测结果与适配器能力的 JSON Schema |
| [schemas/verification-broker.schema.json](schemas/verification-broker.schema.json) | Broker 配置、静态禁入扫描、资源租约、洁净室 `VerificationReceipt` 与 Promotion 结果的 JSON Schema |
| [schemas/wiki-index.sql](schemas/wiki-index.sql) | 本地 Wiki SQLite 索引库 DDL（关系表、符号倒排、图谱边表、FTS5 与可选向量表） |
| [schemas/state-store.sql](schemas/state-store.sql) | 运行时状态与 Append-Only Event Log SQLite DDL（Claims / ProofPlans / Golden Snapshots / Receipts / Breakers / Inbox / Cursors） |

## 阶段状态（Roadmap）

- [x] **架构设计阶段完成（Docs 01–07、ADR-0001–0005、全部 JSON Schema 与 SQLite DDL）**
- [ ] **Phase 1 MVP 实现**：SQLite 状态底座 + 本地 Math Wiki 索引器 + Local Verification Broker（macOS Seatbelt / Linux bwrap + Clean-Room Receipt）
- [ ] **Phase 2 实现**：双轨 Orchestrator、常驻生命体征 HUD、双语复盘 Fact Pack 编译器与可恢复熔断器
- [ ] **Phase 3 实现**：多引擎统一 Adapter（Lean 4 / Agda / Rocq）与 `tool_automation` Hammer / Oracle 交叉印证管线
