# 02. 角色拓扑、权限边界与通信契约（Roles, Permissions & Contracts）

## 1. 角色拓扑（5 核心角色 + 受控二级区域编排）

本系统采用 **1 个主编排器 + 4 类执行角色** 的最小完备拓扑；当单个基建 RFC 规模较大（涉及多文件与数十条引理）时，允许主编排器派生一层 **Regional Infra Orchestrator**，但**硬锁最大编排层级 = 2**，防止无限套娃。

| 角色 | 推荐模型层级 | 核心职责 | 严禁越界红线 |
| :--- | :--- | :--- | :--- |
| **Chief Orchestrator**（主编排器） | 最强前沿推理模型 | 全局数学战略、维护 Claim Graph、双轨调度与预算分配、审批 RFC、管理 Decision Inbox 与人类对话、落盘 ADR | 物理禁写任何形式化源码；不可直接调用原生编译命令；不可绕过 Comparator 合入主干 |
| **Regional Infra Orchestrator**（可选二级） | 中强推理模型 | 接管单个大型 `InfraRFC`，在独立 CoW worktree 内协调该模块的 Builder 与 Prover 批处理，整包回归后向 Chief 上报 | 仅限所分配的模块与 worktree；不可派发新的跨模块 RFC；不可派生第三层 Orchestrator |
| **Explorer / Falsifier**（探索与证伪） | 强推理 + 强代码 | Track A：非形式化推演、写 Python/Sage/SMT 脚本做数值探测与**主动证伪**、写含 `sorry` 的最小骨架、起草 `InfraRFC` | 严禁修改基建稳定区代码；产物仅限于 `scratch` 与 RFC 草案 |
| **Infra Builder**（基建架构） | 强推理模型（重抽象设计） | Track B：消费 RFC，在隔离 worktree 编写定义、notation、typeclass 实例，并**亲自证通 2–3 条核心特征化引理 + 非空烟雾测试**，达标后进入冻结窗口 | 不做大规模常规 `sorry` 暴力填充（移交 Prover 池）；不可绕过 Reviewer 直接冻结核心定义 |
| **Prover Worker**（证明工兵池） | 廉价高并发 / 专有 Prover | Track B：针对前缀冻结的目标命题，在 Fast Lane 热池高频试错，填写 proof body；探索派生引理默认支持 `PROVE_OR_REFUTE` 双向证伪 | 严禁修改定义、定理签名、公理或全局属性；不可操作主干合并 |
| **Tool / Automation Subagent**（工具与锤子子代理） | 轻量代码模型 / 确定性路由 | 由 Orchestrator 按需派发：执行前提选择 + Hammer 重建（`LeanHammer` / `lean-smt` / `CoqHammer` 等）、SMT/ATP/CAS Oracle 探测与剪枝前交叉印证、`L2a → L2b` witness 核验 | 仅通过受限接口提交重建补丁或结构化 Oracle 证据；不得私自修改定义或直接剪枝主干路径 |
| **Independent Reviewer**（独立审查） | 强推理模型（**强制盲审上下文**） | 对照非形式化意图审查正式声明（查空真/弱化/语义漂移）；审计高风险 Oracle 假设或特批 `L4b` 桥接兼容性；可选执行 `L5` 后证明压缩（Golfing 模式） | 不可读取 Writer/Builder 的对话辩解历史；审查模式下只输出结构化裁决票据，不私改源码 |

---

## 2. 工具权限矩阵（Capability-Level Hard Enforcement）

所有安全与职责边界必须通过 **Tool Capability 物理强制执行**，而非仅靠 System Prompt 约定。

> 图例：`R` = 只读；`W` = 受控写；`Run` = 执行受控动作；`—` = 该角色工具表中根本不暴露此工具。

| 工具 / 能力类别 | Chief Orchestrator | Regional Orchestrator | Explorer | Infra Builder | Prover Worker | Independent Reviewer |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: |
| `claim_graph_read` / `update` | **R / W** | R / 局部 W | R | R | — | R / 写审计字段 |
| `decision_inbox_push` / `adr_write` | **W** | —（仅上报 Chief） | — | — | — | — |
| `rfc_create` / `rfc_approve` | **R / Approve** | R | **Create 草案** | R | — | R / 审签 |
| `spawn_subagent` / `cancel_subagent` | **Run**（全角色） | **Run**（限 Builder/Prover） | — | — | 仅浅层子引理递归（depth ≤ 1） | — |
| `repo_read` / `mathlib_search` | **R** | **R** | **R** | **R** | **R** | **R** |
| `wiki_search` | **R**（全层） | **R**（默认 core） | **R**（全层） | **R**（默认 core） | **R**（默认 core） | **R**（全层） |
| `wiki_check_graveyard` | **Run** | **Run** | **Run**（提猜想前必调） | **Run**（建定义前必调） | — | **Run** |
| `wiki_rehydrate` | **Run** | **Run** | — | **Run** | — | — |
| `python_sandbox` / `smt_probe` | — | — | **W / Run** | **W / Run**（烟雾探针） | **Run**（仅反例/SMT 辅助） | **Run**（独立空真复核） |
| `scratch_write` | — | — | **W** | — | — | — |
| `worktree_write` | — | — | — | **W**（限指定 worktree） | — | — |
| `submit_proof_body`（前缀冻结写入） | — | — | — | — | **W** | 仅 Golfing 模式 |
| `broker_fast_check`（热池 REPL） | — | — | **Run** | **Run** | **Run** | **Run** |
| `broker_heavy_build`（限流增量构建） | — | **Run** | — | **Run** | — | **Run** |
| `comparator_verify` / `promote_patch` | **触发合并**（须持 Reviewer 票据或 L5 回执） | 申请提升 | — | — | — | **签发审签票据** |

### 四道不可逾越的物理防火墙

1. **Orchestrator 物理禁写源码**：工具集中不存在任何文件写工具，从根源避免主控上下文被 tactic 报错污染。
2. **Prover 前缀物理锁死**：Prover 仅能调用 `submit_proof_body(target_id, helper_lemmas, proof_body)`，后端自动将其拼接到受信任的冻结前缀之后送检，彻底阻断“改弱定理签名”类 reward hacking。
3. **全员禁止直调原生编译 Shell**：不向任何 Agent 开放原生 `lake build` / `coq_makefile` / `agda` 命令权限；全部经由 Local Verification Broker 统一限流、记账与隔离。
4. **Reviewer 强制盲审隔离**：调用 Reviewer 时，运行时仅装配「非形式化数学陈述 + 形式化声明及依赖定义切片 + 独立探测工具」，严格剥离 Writer/Builder 的思考过程与解释文本。

---

## 3. 核心通信契约（Structured Contracts）

所有跨角色交接必须使用结构化契约（对应机器校验定义见 [../schemas/contracts.schema.json](../schemas/contracts.schema.json)）。**子代理的完整试错轨迹只落盘到 `.runtime/traces/`，绝不原样回灌进编排器上下文。**

### 3.1 下发契约：`DispatchContract`（Orchestrator → Subagent）

* **必需字段**：
  * `task_id`, `role`, `engine`（`lean4 | rocq | agda | smt | python`）, `claim_id`, `base_commit`
  * `workspace_mode`（`readonly | scratch | cow_worktree`）
  * `objective_mode`（`PROVE_ONLY | PROVE_OR_REFUTE | BUILD_INFRA | EXPLORE | AUDIT_SPEC | GOLF_PROOF`）
  * `frozen_targets`：明确锁死的签名与文件列表
  * `relevant_wiki_ids`：由编排器按需注入的精简引理 ID 列表
  * `budget`：四维硬预算——`max_wall_seconds`、`max_turns`、`max_broker_calls`、`max_recursion_depth`
  * `early_escalate_if`：明确的主动提前止损条件（避免死磕耗尽预算）

### 3.2 回报契约：`ResultContract`（Subagent → Orchestrator）

* **状态枚举（`status`）**：
  * `PROVED`：达到目标信任等级（`L5` / `L4a` / 特批 `L4b`），必须附带对应内核回执、Oracle 交叉印证记录或 Bridge 审计票据；
  * `REFUTED`：找到反例或证出 `¬P`，必须附带可复现 witness（自动入库 `GRAVEYARD`）；
  * `PARTIAL_PROGRESS`：未终结但形成可复用的明确阶段产物；
  * `BLOCKED_MISSING_INFRA`：缺失上游形式化库支持（附所需引理清单）；
  * `BLOCKED_SPEC_DEFECT`：怀疑命题前提不足、定义有误或存在空真；
  * `BUDGET_EXHAUSTED`：四维预算任一触顶。
* **强制副产品回收（`harvested_lemmas`）**：即便主任务以失败或超预算结束，退出前须抽离所有已独立编译通过（0 `sorry`）、具清晰签名且不依赖匿名局部环境的辅助引理，送入 Wiki 入库流程（默认沉降进 `SEDIMENT`）。
* **结构化根因诊断（`diagnosis`）**：非 `PROVED` 状态必填，归类为 `conceptual_gap | missing_mathlib_api | tactic_timeout | missing_hypothesis | definition_mismatch | engine_limitation`，并附建议动作。

### 3.3 基建需求契约：`InfraRFC`（Explorer → Orchestrator → Infra Builder）

每一份基建需求单必须同时给出**需求动机**与**消费者用法测试**，杜绝“造出优雅但下游根本调不起来的抽象”：
* `rfc_id`, `requested_by_claim`, `engine`, `motivation`
* `proposed_definitions` 与 `required_api_lemmas`
* `consumer_usage_sketch`：探索轨给出的目标调用片段（必须能在定义完成后零警告通过类型检查）
* `smoke_test_criteria`：Infra Builder 必须先证通 2–3 条核心特征化引理并给出至少一个非平凡实例，证明前提非空真、typeclass 推导顺畅，随后才允许进入 Freeze Window 并行铺证明。

### 3.4 跨引擎桥接契约：`CrossEngineBridgeContract`（`L4b` 特批专用，默认禁用）

依据 [ADR-0003](adr/ADR-0003-multi-engine-oracles-and-hammers.md)，跨 ITP 内核公理桥接默认状态为 **`FORBIDDEN`**；仅在主引擎面临极端困难并经人类显式 ADR 解锁 `EXCEPTIONAL_ADR_ONLY` 后，方可登记：
* `source_engine`, `source_repo_commit`, `source_decl_path`
* `target_engine`, `target_axiom_name`, `target_signature`
* `logic_compatibility_audit`：明确核对 Classical / UIP / Funext / 宇宙多态 / 终止性假设是否存在冲突，并附 Independent Reviewer 审签结论。

---

## 4. 关键策略约束汇总

本规范固化了 5 项核心架构拍板（详见 [adr/ADR-0001-core-workflow-decisions.md](adr/ADR-0001-core-workflow-decisions.md)）：
1. **两层编排上限**：支持 Chief + Regional Infra Orchestrator，硬锁最大层级 = 2；
2. **定义与核心 API 捆绑验收**：Infra Builder 必须亲自证通核心特征化引理 + 非空实例，再冻结并移交并行 Prover 池；
3. **默认双向证伪契约**：探索派生引理默认开启 `PROVE_OR_REFUTE`，仅人类/文献确认真值的经典基建引理设为 `PROVE_ONLY`；
4. **项目级可配置主引擎 + 跨 ITP 桥接默认禁用**（已由 [ADR-0003](adr/ADR-0003-multi-engine-oracles-and-hammers.md) 修订）：初始化时选定主引擎（Lean 4 / Agda / Rocq），默认禁止 `L4b` 公理桥接；Hammer 与 Oracle 统一由 Orchestrator 按需路由给 `tool_automation` 子代理，且剪枝前须经补充交叉验证；
5. **无损引理回收 + 分层沉降**：开启失败任务的独立引理回收，但默认沉降入 `SEDIMENT`，永不删除且零默认上下文污染。
