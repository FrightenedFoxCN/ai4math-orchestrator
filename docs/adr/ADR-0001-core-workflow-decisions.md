# ADR-0001：AI4Math 工作流核心编排、权限与本地知识库决策

- **状态**：已确认（Accepted）
- **日期**：2026-11-24
- **决策背景**：搭建面向开放数学问题探索、单机多核运行、支持大规模边探边建基建、多引擎协同（Lean 4 + Rocq / Agda + SMT）以及无人值守+不定期人类介入的 AI4Math 编排系统。

## 决策清单

### D1. 双层受控编排上限（Two-Tier Orchestrator Cap）
- **决定**：采用 **Chief Orchestrator + 可选 Regional Infra Orchestrator** 的两层架构，**硬锁最大编排深度 = 2**（Prover 仅允许 1 层浅层子引理递归）。
- **理由**：避免单层编排器被大规模基建进度淹没，同时防止无界嵌套子代理导致单机资源失控与状态不可追溯。

### D2. Infra Builder 捆绑“定义 + 核心特征化引理烟雾证明”
- **决定**：Infra Builder 完成 RFC 定义后，必须亲自证通 2–3 条核心特征化引理（构造/消除/基本等价）并给出至少一个非平凡实例（防空真）及通过下游 `consumer_usage_sketch` 类型检查，随后才允许进入 Freeze Window 并行派发 Prover 池。
- **理由**：避免只搭空壳 `sorry` 骨架导致写出劣质抽象，拖垮下游所有并发证明任务。

### D3. 探索派生引理默认启用 `PROVE_OR_REFUTE` 双向契约
- **决定**：所有由开放探索派生的中间引理默认赋予 Prover `PROVE_OR_REFUTE` 权限（卡住时允许调用 SMT 或构造反例证 `¬P`）；仅人工或文献确认真值的经典基建定理锁死为 `PROVE_ONLY`。
- **理由**：开放探索中大量中间命题为假，允许 Prover 主动证伪可将算力浪费提前止损并沉淀为 `GRAVEYARD` 资产。

### D4. 单主引擎优先 + 显式准入跨引擎（Lean 4 + SMT 默认） *(已由 [ADR-0003](ADR-0003-multi-engine-oracles-and-hammers.md) D13 修订收紧：支持项目级配置主引擎含 Agda/Rocq，且默认禁用 `L4b` 跨 ITP 公理桥接)*
- **决定**：所有主线形式化默认采用 **Lean 4 + SMT（Z3 / cvc5）**；启用 Rocq 或 Agda 分支必须满足显式条件（复用现成成熟库或人类 ADR 指定），并强制走 `L4` Bridge Contract 公理兼容性审计（默认划入 Amber Zone）。
- **理由**：防止模型在主引擎遇阻时“逃避式切换引擎”，避免项目碎片化与隐蔽逻辑不一致性。

### D5. 永不删除的无损引理回收 + 三层沉降本地 Wiki（CORE / SEDIMENT / GRAVEYARD）
- **决定**：
  1. 开启失败任务的独立闭合引理回收（Lemma Harvesting），且**永不自动清理删除 `unreferenced` 资产**；
  2. 采用三层沉降架构：活跃主干留 `CORE`，`unreferenced` 与废弃分支成果作为不可变快照沉入 `SEDIMENT`（脱离日常 CI 回归、零默认检索噪声），反例与死胡同入 `GRAVEYARD`；
  3. 通过 **Level 1（默认静默）→ Level 2（Shadow Hit 轻量提示）→ Level 3（卡壳/RFC/转向硬触发）** 实现按需唤醒与惰性重验（Lazy Re-hydration）；
  4. 存储采用 **Git Markdown 源文件 + 可重建单文件 SQLite（符号倒排 + FTS5 + 可选 `sqlite-vec`）**。
- **理由**：兼顾开放数学研究中“历史伏笔永不丢失”与单机多核场景下“零回归负担、低上下文噪声、跨引擎精准检索”的双重要求。

### D6. 物理能力隔离替代纯 Prompt 约束
- **决定**：通过工具权限物理封死四条红线——Orchestrator 禁写源码、Prover 仅通过冻结前缀接口 `submit_proof_body` 提交、全员禁直调原生编译 Shell（统一走 Local Verification Broker）、Reviewer 强制盲审上下文隔离。
- **理由**：长程自主运行中纯 Prompt 约束易被模型绕过，必须以确定性工具层与 Comparator 门禁兜底。
