# 01. 总体架构蓝图（Architecture Overview）

## 1. 背景与四个本质张力

相较于面向竞赛题的单题 Prover 或面向静态代码库的迁移流水线，本系统同时面对五个耦合约束：**开放问题探索、大规模边探边建基建、多形式化引擎协同、单机多核运行、无人值守与不定期人机共驾**。这带来四个核心工程张力：

1. **探索的流动性 vs. 基建的稳定性（Moving Foundations）**
   开放问题探索中，大量中间引理会被证伪或改写；但并发搭建大规模形式化基建要求上游定义稳定。若无隔离屏障，一次上游定义微调就会引发下游并行子代理的级联编译雪崩。
2. **多引擎协同的信任边界（Cross-Engine Soundness Gap）**
   Lean 4（古典默认 / `propext` / `Quot`）、Agda（构造性 / 可选 Cubical / 严格终止与正性）、Rocq（宇宙多态 / MathComp）与 SMT（一阶理论求解）的逻辑底座并不等价。跨引擎结果若缺乏显式公理与前提差异登记，极易引入隐蔽不一致。
3. **单机多核的物理资源墙（RAM & Cache Thrashing）**
   Lean 4 + Mathlib、Rocq、Agda 均为高内存工具链。若允许多个并行子代理各自自由触发全量编译，单机极易进入 swap 雪崩；而直接软链接共享项目构建目录则会导致 `.olean` / `.trace` 并发写坏。
4. **无人值守后的认知可读性（Legibility for Intermittent HITL）**
   长时间无人值守运行后，人类随时回来必须能在数分钟内看清“哪些已内核证明、哪些靠外部假设、哪些方向已证伪、当前需要拍板什么”，而不能靠翻阅原始日志。

---

## 2. 核心设计哲学：柔性编排层 + 刚性底座层

借鉴业界（如 Numina Fuse、EasyProver、HOA、CompCert4Lean）的实践经验，系统将**随模型演进的调度策略**与**模型无关的确定性基础设施**严格分层：

```text
┌──────────────────────────────────────────────────────────────────────┐
│ Layer 1: 动态编排层（Chief + Regional Orchestrator，禁直接写源码）     │
│  - 数学战略规划 / 双轨算力分配 / Claim Graph 推演 / 异步人机共驾       │
└───────────────────┬──────────────────────────────────┬───────────────┘
                    │ Track A: 探索与证伪               │ Track B: 基建与证明
          ┌─────────▼──────────┐             ┌─────────▼──────────┐
          │ Explorer/Falsifier │──InfraRFC──▶│ Infra Builder +    │
          │ - 非形式化推演     │             │ Prover Worker 池   │
          │ - SMT/Python 反例  │             │ - 定义烟雾测试     │
          │ - 轻量骨架验证     │             │ - 冻结窗口并行消坑 │
          └─────────┬──────────┘             └─────────┬──────────┘
                    │                                  │ 独立盲审 / 准入票据
                    │                        ┌─────────▼──────────┐
                    │                        │ Independent Review │
                    │                        │ - Spec 对齐/查空真 │
                    │                        │ - 跨引擎公理审计   │
                    │                        └─────────┬──────────┘
┌───────────────────▼──────────────────────────────────▼───────────────┐
│ Layer 2: 刚性验证与回归基础设施（模型无关、确定性门禁）                │
│  [Local Verification Broker]   [Comparator 防作弊]   [四级回归调度]   │
│  - Fast Lane: SMT + 预热 REPL  - 冻结前缀 defeq 核验  - T0/T1/T2/T3   │
│  - Heavy Lane: 限流构建队列    - 0 sorry + Axiom白名单 - 标脏传播     │
└───────────────────┬──────────────────────────────────┬───────────────┘
                    │ 唯一事实源                       │ 沉降与混合检索
┌───────────────────▼──────────────────────────────────▼───────────────┐
│ Layer 3: 持久化状态层                                                 │
│  Git 仓库（基建代码 + ADR + RFC） | Claim Graph & Trust Ledger        │
│  Local Math Wiki（CORE / SEDIMENT / GRAVEYARD + SQLite 混合索引）     │
└──────────────────────────────────────────────────────────────────────┘
```

---

## 3. 双轨制工作流（Dual-Track Orchestration）

### Track A：探索轨（Exploration Track）
* **目标**：以最低成本快速试探数学方向、主动证伪假命题、验证顶层证明骨架的逻辑闭环。
* **规则**：工作在隔离草稿区（`scratch`），优先调用 Python/Sage/SMT 做反例探测，允许骨架含 `sorry`；**严禁直接修改基建轨的稳定代码**。
* **输出**：更新后的 Claim Graph + 结构化 **`InfraRFC`**（必须附带最小用法草稿 `consumer_usage_sketch`）。

### Track B：基建轨（Infrastructure Track）
* **目标**：消费 RFC，并行搭建可长期复用的形式化定义、API 引理与定理库。
* **两阶段冻结屏障**：
  1. **定义与烟雾测试阶段**：Infra Builder 编写定义并亲自证通 2–3 条核心特征化引理，通过非空/可满足探针，确保定义抽象合理且下游可调用；
  2. **Freeze Window 并行证明阶段**：冻结声明前缀后，大规模派发 Prover Worker 并行消解剩余 `sorry`；
  3. **受控回流（Escalation）**：Prover 禁止私改定义；若多路并行受阻且指向定义缺陷，触发正式升级回到定义修订或人类决策。

---

## 4. 统一信任账本（Trust Ledger: L0–L5）

系统对所有数学命题维护显式信任等级，支持跨引擎（Lean 4 / Rocq / Agda / SMT）统一记账与上游变更自动标脏（Taint Propagation）：

| 等级 | 名称 | 定义与准入条件 |
| :--- | :--- | :--- |
| **L0** | `Conjecture` | 尚未验证的初始猜想 |
| **L1** | `Probed` | 经过数值/SMT 有限样本测试未见反例（附脚本与范围） |
| **L2** | `Refuted` | **已证伪**（附具体反例 witness 或 `¬P` 检验回执，永久入库 `GRAVEYARD`） |
| **L3** | `Formal-Sketch` | 形式化声明编译通过但含 `sorry`；已通过独立 Reviewer 语义对齐与非空探针 |
| **L4** | `Cross-Verified` | 由 SMT Oracle 或辅助引擎（Rocq / Agda）证完，在主引擎暂挂受控 `axiom`，并登记 **Bridge Contract**（含外部源码 commit、公理映射与逻辑前提差异审查） |
| **L5** | `Kernel-Verified` | 主引擎端到端完全证明：0 `sorry` + 白名单 `#print axioms` + 前缀冻结 Comparator 通过 |

---

## 5. 单机多核资源治理与四级回归节奏

### 5.1 Local Verification Broker（双车道资源调度）
所有 Agent 禁止直接通过 shell 调用原生编译器，统一经由本地 Broker 申请资源租约：
* **Fast Lane（高并发、低内存、强超时）**：SMT 探针（2–15s 上限）、常驻预热 Mathlib 的 Lean REPL 池（2–4 个 worker）及按需加载的轻量 Rocq/Agda 会话；内存低于警戒水位时自动淘汰闲置非主引擎 worker。
* **Heavy Lane（严格限流，并发度 1–2）**：增量 `lake build`、全模块编译、Agda 终止性/正性检查、Comparator 最终验收；主干合并任务优先级高于探索构建。
* **安全隔离缓存**：全局只读共享上游大库（Mathlib / opam / Agda stdlib）；单引理证明用内存/scratch 走热池；大块基建开发用独立 `git worktree` + 文件系统写时复制（APFS `cp -c` / Btrfs `cp --reflink=auto`）克隆项目构建缓存。

### 5.2 四级回归节奏（T0–T3）
* **T0（秒级）**：单步 tactic / 草稿交互检查与 SMT 反例探针（Fast Lane）。
* **T1（数十秒，加锁串行 Promotion）**：候选补丁合并主干前执行前缀冻结检查 + Comparator & Axiom 白名单 + 受影响下游切片（Affected Cone）增量编译；失败自动回滚。
* **T2（周期性后台任务）**：全仓增量编译 + `maxHeartbeats` / 编译耗时回归监测（捕获劣化全局性能的 `@[simp]` 或 typeclass 实例）+ 跨引擎 Bridge 哈希一致性扫描。
* **T3（架构/定义变更后）**：上游核心定义修改或人类拍板转向后，沿 Claim Graph 自动标脏受影响子树并触发重检。

---

## 6. 异步人机共驾协议（Intermittent HITL）

1. **自治绿区（无人值守自动推进）**：反例探测、冻结接口内并行证明、常规辅助引理补全、`L5` 后证明压缩（golfing）、死胡同分支的局部回溯与引理回收。
2. **非阻塞决策收件箱（Amber Zone — Decision Inbox）**：遇到以下情况时挂起当前受阻子分支并写入 Inbox，同时继续推进其他无阻塞分支：
   * 需破坏性修改稳定基建定义；
   * 出现多条高成本且互斥的数学进攻路线待剪枝；
   * 关键引理超预算且既证不出也证不伪；
   * 需引入跨引擎（Rocq / Agda）新分支或登记 `L4` Bridge 公理。
3. **常驻生命体征面 + 按需/P0推送双语复盘简报**（详见 [04-review-briefing-and-hitl.md](04-review-briefing-and-hitl.md)）：
   * **常驻生命体征条**长期静默刷新（0 LLM 消耗），随时监控单机资源、双轨状态、Trust Ledger 存量与告警计数；
   * **双语复盘简报**仅在触发 P0 Push Alert 或人类主动拉取时生成留存，基于确定性 Fact Pack 呈现数学前沿增量、活跃瓶颈地图、早期风险雷达（含可恢复 Definition Smell 局部熔断）、Top-3 决策收件箱与 Wiki 考古线索。
4. **ADR 转向语义**：人类决策固化为 ADR 后，系统自动优雅取消废弃分支在跑子任务、将可用中间引理沉降入 Wiki、对受影响节点触发 T3 标脏并下发新 RFC。

---

## 7. 分阶段落地路线图

* **Phase 1（最小闭环底座）**：Lean 4 + SMT（Z3/cvc5）主线；落地 Local Verification Broker、Comparator 门禁、基础 Claim Graph（L0–L5）及 5 角色核心权限隔离。
* **Phase 2（双轨协作 + 分层 Wiki + 异步共驾）**：落地 Exploration/Infra 双轨（RFC + Smoke Test + Freeze Window）、三层本地 Wiki 索引（CORE/SEDIMENT/GRAVEYARD）、Decision Inbox 与 5 分钟复盘简报。
* **Phase 3（多引擎扩展）**：通过统一 Proof-Assistant Adapter 接入 Rocq（`rocq-lsp`/Pytanque）与 Agda（`agda-mcp`），启用 `L4` Bridge Contract 审计与跨引擎 T2 回归。
