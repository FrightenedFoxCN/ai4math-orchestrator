# ADR-0007：跨学科科学验证泛化、多维 GPU/SLURM 算力调度、四层知识库与双通道信念传播

- **状态**：已确认（Accepted；D28–D30 已代码落地，D31–D33 架构已定稿待分阶段实施）
- **日期**：2026-11-27
- **关联文档**：[09-scientific-generalization-and-belief-vault.md](../09-scientific-generalization-and-belief-vault.md)、[03-local-math-wiki.md](../03-local-math-wiki.md)、[07-claim-graph-state-machine.md](../07-claim-graph-state-machine.md)、[08-engineering-architecture.md](../08-engineering-architecture.md)

## 决策清单

### D28. 抽离领域无关认识论核心与 `DomainPack` 扩展边界（已落地 `7648418`）
- **决定**：在 `ai4math-core` 中抽象 `EpistemicLevel`、`EpistemicDebt`、`EpistemicAssessment`、`ScientificTrust (E0–E5)`、`BackendDescriptor` 双上限（正向 + 证伪）与 `DomainPack` trait，将既有 Lean 4 / Agda / Rocq / Oracle 规则封装为内置 `ItpDomainPack`；在 `ai4math-evidence` 中引入领域无关封印端口 `SealedReceipt` 与 `ProtocolRunEvidence`，现有 `VerificationReceipt` 无侵入实现 `SealedReceipt`。
- **理由**：在不改动既有 ITP 线格式、哈希前像与测试基线的前提下，使系统可承载计算化学、计算生物学等非二值内核学科。

### D29. 采用反漂移 `ProtocolSpec` 引擎替代自由脚本验证（已落地 `7648418`）
- **决定**：将 `oracle.rs` 的“拒绝原始脚本、只收强类型查询”范式泛化为 `ProtocolSpec`（冻结参数 + 强类型单行槽位单遍编译 + 确定性物理/数值健全性门禁 + 正交多验证器分歧显式化 + 无环 Merkle 盲测对照包 `BlindedControlPack` + 迁移 7 不可变预注册与只追加试错账本）。
- **理由**：计算科学缺乏形式化 `defeq` 内核，必须通过物理隔离协议冻结、隐藏对照与试错全记账，从机制上阻断指标漂移、参数暗调、`exit 0` 假收敛、代理洗白与 p-hacking。

### D30. 拆分本地多维 GPU 资源池与双层 `ComputeBackend` / `SlurmTransport`（已落地 `97a5028`）
- **决定**：
  1. 本地宿主机通过 `LocalComputePool` + RAII `ComputeLease` 原子分配 CPU 核、RAM、scratch 与逐卡 GPU 显存/独占锁（含 Apple Silicon 统一内存联动扣减与 `CUDA_VISIBLE_DEVICES` 强制注入/CPU-only 置空）；
  2. 上层 `ComputeBackend` 统一 `LocalComputeBackend` 与 `SlurmComputeBackend<T>` 生命周期；
  3. 下层窄接口 `SlurmTransport`（禁任意 shell 命令）统一 `LocalSlurmCli` 与固定 `BatchMode` / `StrictHostKeyChecking=yes` 的 `SshSlurmTransport`（纯 Rust 有界 ustar tar 收发），配合匿名盲测 Job Array、容器 SHA-256 强制核验与迁移 8 `compute_jobs` 持久表实现重启恢复与按分支批量 `scancel`。
- **理由**：本地硬件归 Broker 管、集群硬件归 `slurmctld` 管，双层拆分避免上帝 trait，同时在远端 HPC 场景下保持与本机同等强度的防漂移与封印边界。

### D31. 平滑演进现有 Wiki 为四层 `KnowledgeVault` 并预留可插拔向量检索（待实施 · 阶段 2）
- **决定**：在现有 `.mathwiki` 与 `wiki-index.sql` 上向下兼容扩展第 4 层 `LITERATURE`、通用分面倒排 `entry_facets`（兼容保留 ITP `entry_symbols` 与 `WikiQuery`）、三通道 Shadow Hint、冲突簇以及可插拔 `EmbeddingProvider` / `SemanticIndex` trait（默认 `NoopSemanticIndex`，内置纯 Rust 内存余弦 + 三路 RRF 融合）。
- **理由**：外部文献知识必须与本项目已验证 `CORE` / `SEDIMENT` 物理隔离，同时复用既有的不可变归档、安全扫描与离线重建能力。

### D32. 泛化主状态图并引入“准入打标 + 双通道 Belief Propagation”（待实施 · 阶段 1–2）
- **决定**：
  1. 通过迁移 9 泛化 `claims` / `proof_plans` 并新增 `claim_conflict_edges`，由触发器保持 ITP 领域仍严锁 `L0..L5`、非 ITP 领域使用 `E0..E5`；
  2. 文献条目遇到缺实验条件等非致命缺陷时采用**准入但强制打标签**策略，不在入口硬杀；
  3. 在 `ai4math-core::belief` 中建立双通道传播：**通道 1** 维持硬认识论底线、不可抵消债务并集与硬证伪/撤稿否决；**通道 2** 使用 Subjective Logic `(b, d, u)` + 标签确定性衰减 + 共享祖先去重的阻尼因子图 BP，输出结论置信度、冲突度、归因链及反向敏感度 `VoI`。
- **理由**：兼顾真实科研中“文献条件不全仍具启发价值”的实用性与形式化/物理验证的不可妥协底线，同时避免单点概率混淆“无知”与“激烈冲突”。

### D33. 空闲算力复现/形式化排期（含人类队列）优先，论文抽取后置并先在数学基线文献上验证（待实施 · 阶段 3–5）
- **决定**：
  1. 抽象 `IdleVerificationScheduler`，同时消费 **BP 反向 `VoI` 自动队列** 与 **人类显式提交的待核验文献队列**；仅在资源 `Green`/空闲且无高优前台任务时按独立空闲预算调度形式化（ITP）或计算复现（`ProtocolSpec`），通过后经 `replicates` 边纳入 `CORE`，失败则挂 `contradicts` 边回传 BP；
  2. 将 `ai4math-evidence::literature` 溯源门禁与离线漂移测试床排在阶段 1–3 之后，且**阶段 4 先不接入 CompChem / CompBio，而是先针对已有数学基线（Lean 4 / Agda / Rocq）的相关文献完成抽取门禁与实地漂移验证**（重点测试章节前置假设剥离、量词/符号漂移、猜想与已证定理立场混淆及空闲形式化闭环）；
  3. 仅在数学基线文献抽取漂移可控后，再于阶段 5 扩展 CompChem / CompBio 领域卡型（单位换算、实验条件检查表、对照配方卡）并接入首个非 ITP 实战试点。
- **理由**：现有数学基线具备确定性本地形式化内核与反例探测，可作为客观标尺隔离并量化文献抽取的实地漂移，避免在化生领域同时叠加抽取漂移与物理近似不确定性。
