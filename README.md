# ai4math-orchestrator

面向**开放数学问题探索**的单机多核 AI4Math 编排与形式化验证工作流设计仓库。

## 核心场景与约束

1. **开放数学问题探索**：中间猜想高度流动（经常需要证伪、修正或重构抽象），非形式化创意与形式化验证深度交织。
2. **大规模边探边建基建**：Mathlib 现有覆盖不足以支撑目标问题，必须在探索过程中并行派发子代理搭建长期可复用的形式化基础设施。
3. **多引擎协同**：以 **Lean 4 + SMT（Z3 / cvc5）** 为默认主线，在显式准入与跨引擎契约约束下协同 **Rocq** 与 **Agda**。
4. **单机多核本地运行**：在严格的内存与 CPU 水位线约束下，通过中央验证调度器与写时复制（CoW）隔离工作区支撑高频回归。
5. **无人值守 + 异步人机共驾**：系统可长时间自主推进，同时提供非阻塞决策收件箱（Decision Inbox）与 5 分钟复盘简报，支持人类随时介入讨论、纠偏与拍板。

## 设计文档目录

| 文档 | 内容说明 |
| :--- | :--- |
| [docs/01-architecture-overview.md](docs/01-architecture-overview.md) | 总体架构蓝图：四大本质张力、双轨编排模型、多引擎信任账本（L0–L5）、单机验证调度与四级回归节奏、异步人机共驾协议及分阶段路线图 |
| [docs/02-roles-permissions-contracts.md](docs/02-roles-permissions-contracts.md) | 5 角色拓扑、物理工具权限矩阵与四道防火墙、结构化交接契约（Dispatch / Result / InfraRFC / Escalation） |
| [docs/03-local-math-wiki.md](docs/03-local-math-wiki.md) | 本地数学 Wiki 索引设计：三层沉降库（CORE / SEDIMENT / GRAVEYARD）、惰性重验、三级渐进唤醒（含 Shadow Hint）、四维混合检索与双层存储 |
| [docs/adr/ADR-0001-core-workflow-decisions.md](docs/adr/ADR-0001-core-workflow-decisions.md) | 架构决策记录 #0001：首轮核心编排、权限与知识库策略拍板 |
| [schemas/contracts.schema.json](schemas/contracts.schema.json) | 编排契约、信任账本与决策收件箱的 JSON Schema 定义 |
| [schemas/wiki-index.sql](schemas/wiki-index.sql) | 本地 Wiki SQLite 索引库 DDL（关系表、符号倒排、图谱边表、FTS5 与可选向量表） |

## 后续待细化模块（Roadmap）

- [ ] `docs/04-verification-broker.md`：单机多核 Local Verification Broker（热池 REPL、Fast/Heavy 双车道、APFS/Btrfs CoW Worktree、内存准入控制）
- [ ] `docs/05-claim-graph-and-hitl.md`：Claim Graph 状态机、自动标脏传播（Taint Propagation）与 5 分钟复盘简报协议
- [ ] `docs/06-multi-engine-adapters.md`：Lean 4 / Rocq / Agda / SMT 统一适配器接口与 `L4` Bridge Contract 审计规范
