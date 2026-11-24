# 05. 多引擎协同、Hammer 调度与 Oracle 可信度防线（Multi-Engine, Hammers & Oracles）

## 1. 核心原则

面向开放数学探索，形式化工具生态涵盖三类性质截然不同的计算体：**证明助手内核（ITP：Lean 4 / Agda / Rocq）**、**内核重建型自动化与锤子（Hammers / Proof-Producing SMT）**、以及**无内核保证的外部求解器与符号计算引擎（SMT / ATP / CAS 如 Mathematica、SymPy、Sage）**。本系统遵循四条核心纪律：

1. **项目级可配置单一主内核（Project-Level Primary ITP）**：每个项目在初始化 ADR 中显式选定一个主引擎（`lean4 | agda | rocq`，默认 `lean4`；构造性或同伦类型论问题可直接选 `agda`）。所有 `L5` 终态认证与 `CORE` 回归均以该主引擎为准。
2. **默认禁止跨 ITP 内核公理桥接（Default-Forbidden Cross-ITP Axioms）**：Lean 4、Agda、Rocq 之间的逻辑底座与编码细节差异极易引入隐蔽语义漏洞。因此默认策略为 `cross_itp_bridge_mode = "FORBIDDEN"`。其他 ITP 仅可作为探索期零公理洞的原型/参考环境，其有效思路须在主引擎内独立重证到 `L5`；仅在主引擎极端受阻时，经人类显式 ADR 才可临时解锁 `EXCEPTIONAL_ADR_ONLY`。
3. **Hammer 不写死为必经前置，改由 Orchestrator 动态派发给 Tool/Automation Subagent**：重型 Hammer（`LeanHammer` / `CoqHammer` / `lean-smt`）单机算力消耗极大，不应绑定进每个 Prover 的固定循环，而是按需路由给专门的工具子代理统一控预算执行。
4. **对称细分 Trust Ledger + Oracle 剪枝前补充验证**：严格区分“外部 Oracle 结论”与“内核检验结论”（`L2a/L2b` 与 `L4a/L4b`）；针对 Mathematica 符号积分等高风险 CAS/SMT 结果，**在据其剪枝数学方向前必须通过补充交叉判断**，严防静默工具 bug 误杀真正可行的开放问题路径。

---

## 2. 细化后的对称 Trust Ledger（L0–L5）

| 等级 | 名称 | 语义定义 | 信任与使用边界 |
| :--- | :--- | :--- | :--- |
| **`L0`** | `Conjecture` | 初始猜想 | 待验证 |
| **`L1`** | `Probed` | 有限样本数值/小模型测试未见反例 | 仅提供弱启发，不可作为证明依据 |
| **`L2a`** | **`Oracle-Refuted`** | 外部求解器（SMT / ATP / CAS / 数值）给出疑似反例或否定结论，**尚未经内核核验** | **仅支持软剪枝（降权/挂起）**，需附带风险标签与补充核验记录；**不直接判死刑、不直接写入硬 `GRAVEYARD`、不直接触发 P0 证伪告警** |
| **`L2b`** | **`Kernel-Refuted`** | 在主引擎内核证出 `¬P`，或将 `L2a` 具体 witness 代入主引擎完成机器核验 | **硬证伪**：永久归档 `GRAVEYARD`；若位于主关键路径，触发 P0 Push Alert |
| **`L3`** | `Formal-Sketch` | 形式化声明在主引擎编译通过（含 `sorry` / `postulate`） | 需通过盲审语义对齐与非空探针 |
| **`L4a`** | **`Oracle-Backed`** | 外部 SMT / ATP / CAS 判真（如 `unsat`、符号恒等），但未完成内核证明重建 | **显式挂 `oracle_debt`**：探索轨可自由用于排除分支与临时占位打通主骨架；基建轨不得静默冒充内核证明，交付主定理前须消债或经人类 ADR 特批 |
| **`L4b`** | **`Cross-ITP-Verified`** | 在辅助 ITP 内核证完并桥接至主引擎（**默认禁用**） | 仅当人类 ADR 显式解锁 `EXCEPTIONAL_ADR_ONLY` 时可用；强制经过盲审逻辑兼容性审计与持续信任债告警 |
| **`L5`** | **`Kernel-Verified`** | 主引擎内核端到端零 `sorry` / `postulate` + 白名单公理 | 包含手写/LLM 证明、反射校验、以及**所有成功完成主引擎内核重建的 Hammer / SMT 证明** |

---

## 3. 按需调度的 Tool / Automation Subagent（替代硬编码 Hammer-First）

为了避免把重型 Hammer 塞进普通 Prover Worker 导致单机 CPU/RAM 抖动，在角色拓扑中增加一类轻量化、无源码主干写权的专职角色：**`tool_automation`（工具与自动化子代理）**。

### 3.1 何时由 Orchestrator 派发 `tool_automation`？
Orchestrator 根据当前目标的数学形态和单机 Broker 负载动态决定是否调用，而非写死顺序：
* **适合派发 Hammer / Proof-Producing SMT**：目标呈现大量等式改写、一阶组合逻辑、线性/非线性算术或明显可由已有库 3–8 条前提直接闭合的“体力型”引理；
* **不适合直接砸 Hammer（优先 LLM 拆解或构造性手写）**：涉及深层归纳法构造、新定义展开、高阶依赖类型/Cubical 路径构造（尤其是 Agda 场景）、或依赖大量复杂高阶结构推理的目标。

### 3.2 三类可执行模式（受 Broker 预算严格限流）
1. **`HAMMER_RECONSTRUCT`（内核重建模式）**：
   * 负责先从本地 Math Wiki + 外部库筛选小而精的前提示（控制前提数量以省算力），再按主引擎调用对应重武器：
     * **Lean 4**：`grind` / `bv_decide` / `aesop` / `LeanHammer`（`lean-auto` + `Duper`/`Zipperposition`）/ `lean-smt`（cvc5 重建）/ `polyrith`；
     * **Rocq**：`lia`/`nia`/`nra` / `CoqHammer`（Vampire/E/Z3 + 重建）/ `SMTCoq` / `Sniper`；
     * **Agda**：`agda --auto` / 结构化前提推荐 / 反射求解器（如 `ring-solver`）。
   * **三态输出**：
     * 重建成功 → 直接产出 `L5` 补丁；
     * 外部求解成功但内核重建超时 → 输出 `L4a` + **提取 `used_premises` / Unsat Core 线索**，回传 Orchestrator 指导 LLM 做精准子目标拆分；
     * 发现反例模型 → 输出 `L2a` 候选 witness。
2. **`ORACLE_PRUNE`（快速排除与剪枝模式）**：
   * 调度 Z3 / cvc5 / Vampire / E / Mace4 / Mathematica / SymPy / SageMath / GAP / Singular 做快速判伪、排除不可能分支或计算不变量 witness。
3. **`WITNESS_VERIFY`（反例/证人核验模式）**：
   * 专门承接 `L2a` 候选反例或 CAS 证人（如 Gröbner 基系数、数值反例点），尝试在主引擎内核或精确有理算术下复核，将其升级为 `L2b` 或推翻误报。

---

## 4. Oracle 正确性防线：剪枝前补充判断协议（Corroborated Pruning Gate）

像 **Mathematica 符号积分、极限化简、参数消元** 或 **依赖类型到 SMT-LIB 的手工/LLM 编码**，都存在极高的静默错误风险（例如：隐式假定参数为正、忽略多值函数支割线 branch cut、漏掉测度零/奇点退化情形、浮点数舍入或无界量词截断）。

因此，系统对所有 Oracle 按风险分级，并强制执行**剪枝前补充判断**：

| 风险等级 | 典型工具与操作 | 常见静默故障模式 | 剪枝前强制补充判断要求 |
| :--- | :--- | :--- | :--- |
| **`HIGH`** | CAS 符号积分 / 符号求和 / 渐近展开 / 复变化简（Mathematica, Maple, SymPy） | 支割线跳变、隐式 `Generic` 参数假设、漏掉奇点/边界条件、忽略可积性/收敛性 | **1. 定义域与隐式假设显式审计**（枚举分母、奇点、分支切割与收敛域）<br>**2. 独立数值/区间交叉印证**（高精度多点数值积分 `mpmath`/区间算术，必须覆盖内部点与边界/极端参数点） |
| **`MEDIUM`** | SMT / ATP 外部编码（Z3, cvc5, Vampire, E） | 依赖类型翻译失真、整数/实数/位宽溢出、公理截断漏前提 | **1. 翻译保真自检**（核对量词域与隐藏前提）<br>**2. 交叉求解器确认**（如 Z3 与 cvc5 双跑一致，或提取 `sat` 模型做精确代回计算） |
| **`LOW`** | 精确有限/离散计算（精确有理数/整数算术、有限群 GAP、有限图枚举、LRAT/DRAT 校验） | 仅限脚本逻辑编码错误 | 独立小样本交叉脚本或直接将 witness 交主引擎内核校验 |

### 软剪枝原则（Soft Pruning Only for `L2a`）
* **未通过补充判断的单一高风险 Oracle 输出**：**严禁用于剪枝**，仅记作 `L1` 备忘线索；
* **通过补充判断但未入主引擎内核的否定结果（`L2a`）**：仅允许对该探索分支执行 **软剪枝（降优先级 / 暂停挂起）**，保留分支状态；
* **若该分支位于主关键路径**：系统自动派发一个轻量的 `WITNESS_VERIFY` 任务，尝试将具体反例代入主引擎内核升级为 `L2b`；只有确认 `L2b` 后，才正式入 `GRAVEYARD` 并触发 P0 Push Alert。

---

## 5. 多引擎适配器抽象与辅助 ITP 安全用法

### 5.1 项目级主引擎配置（`EngineProjectConfig`）
项目初始化时在配置及初始化 ADR 中锁定：
* `primary_engine`: `"lean4" | "agda" | "rocq"`（默认 `"lean4"`）；
* `enabled_oracles`: 启用的外部工具白名单（如 `["z3", "cvc5", "sympy", "mathematica", "sage"]`）；
* `cross_itp_bridge_mode`: 默认 `"FORBIDDEN"`（可选人类 ADR 显式解锁 `"EXCEPTIONAL_ADR_ONLY"`）；
* `allow_secondary_itp_prototyping`: `true`（允许辅助 ITP 做零公理洞的思路验证，见下节）。

### 5.2 辅助 ITP 的安全用法（零语义漏洞原则）
即使 `cross_itp_bridge_mode = "FORBIDDEN"`，系统仍可发挥多证明助手生态优势：
* **允许（无公理洞）**：在探索轨中，使用辅助 ITP（例如主引擎为 Agda 时借 Lean/Mathlib，或主引擎为 Lean 时借 Rocq/MathComp 或 Agda/Cubical）快速验证某段数学构造思路是否走得通；验证成功后提取**非形式化证明骨架 + 引理结构**存入 Wiki `SEDIMENT`，随后在**主引擎内独立形式化并证至 `L5`**；
* **默认禁止（高风险公理洞）**：将辅助 ITP 的定理直接作为 `axiom` / `postulate` 粘贴进主引擎（`L4b`）。仅当主引擎在极高代价下确实难以承载（且经人类 ADR 显式批准解锁）时，才启用 `CrossEngineBridgeContract`，强制接受独立盲审对 Classical / UIP-K / Funext / 宇宙多态 / 终止性假设的逐项核对，并永久在常驻生命体征和复盘简报中以高优先级信任债高亮警示。

### 5.3 统一 ITP 适配器接口（公共原语 + 能力标志）
所有 ITP 后端（Lean 4: `lean-lsp-mcp` / 预热 REPL；Agda: `agda --interaction-json` / `agda-mcp`；Rocq: `rocq-lsp` / Pytanque）对上层暴露统一公共原语，同时通过 `capabilities` 声明各自特色原语（避免削足适履）：

| 原语类别 | 接口 | Lean 4 | Agda | Rocq |
| :--- | :--- | :---: | :---: | :---: |
| **公共原语** | `check_snippet` / `inspect_goal` / `extract_symbols` / `audit_trust` | ✅ | ✅ | ✅ |
| **信任审计实现** | `audit_trust` 底层机制 | 0 `sorry` + `#print axioms` 白名单 | `--safe` + 无 `postulate`/`TERMINATING` + 核对 `--without-K`/`--cubical` | 0 `Admitted` + `Print Assumptions` 白名单 |
| **特色扩展槽** | Tactic 并行探测 / Hole 交互 | `multi_attempt`, `grind`, `bv_decide` | `case_split`, `refine`, `give`, 文件指纹校验 | `coqhammer`, `smtcoq`, MathComp 检索 |
