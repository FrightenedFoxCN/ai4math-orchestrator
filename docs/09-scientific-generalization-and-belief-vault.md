# 09. 跨学科科学验证泛化、多维算力调度、四层知识库与双通道信念传播（Scientific Generalization, Compute, Vault & Belief Propagation）

> 本文档定义系统从**形式化数学（ITP：Lean 4 / Agda / Rocq）**平滑泛化至**计算化学 / 计算生物学及通用计算科学**的完整架构扩展。
> 本文严格区分 **「已实现并验证的抽象底座与阶段 1–3」**（提交 `7648418`、`97a5028`、`1bd8ef3`、`dac6274`、`67876ec`）与 **「后续阶段 4–5（论文抽取门禁与外围试点）」**（对应 [ADR-0007](adr/ADR-0007-scientific-generalization-compute-vault-and-belief.md) D28–D33）。

---

## 1. 泛化总览：不变的控制面 vs. 可插拔领域包

从 Proof Assistants 迈向计算化学/计算生物学，真正的难点不是替换外部工具，而是跨越**认识论断层**：ITP 拥有小而可判定的二值内核（`defeq` + 公理白名单），而计算科学处于**多尺度物理近似、统计不确定性与条件依赖**的经验世界，子代理验证漂移（指标偷换、参数暗调、`exit 0` 假收敛、代理模型洗白、p-hacking、分析脚本私改与物理语义错位）被成倍放大。

因此，系统把**领域无关的可信编排控制面**与**领域特异的验证/知识规则**彻底分层：

```text
┌────────────────────────────────────────────────────────────────────────────┐
│ Layer 1: 动态编排与异步共驾层（Chief / Regional / Decision Inbox / HITL）   │
│  · 接收双通道信念回传（硬底线 + Subjective Logic (b,d,u) + 反向 VoI）        │
│  · 同时驱动前景探索/基建轨 与 后台空闲算力复现/形式化排期（含人类队列）       │
└───────────────────────────────┬───────────────────────────────┬────────────┘
                                │ 前景任务                       │ 空闲复现/人类队列
                                ▼                               ▼
┌────────────────────────────────────────────────────────────────────────────┐
│ Layer 2: 领域无关反漂移底座与执行承载（Broker + Sealed Evidence 边界）       │
│  · DomainPack + EpistemicLevel + EpistemicDebt + SealedReceipt [已落地]     │
│  · ProtocolSpec 单遍槽位编译 + 物理健全性门禁 + 无环盲测对照    [已落地]     │
│  · LocalComputePool (CPU/RAM/GPU逐卡/统一内存)                 [已落地]     │
│  · ComputeBackend: LocalComputeBackend | SlurmComputeBackend<SlurmTransport>│
│    (LocalSlurmCli / BatchMode SshSlurmTransport + 盲测 Job Array) [已落地]  │
│  · IdleVerificationScheduler (让路前台 + 独立空闲预算 + 闭环回调) [Phase 3] │
└───────────────────────────────┬───────────────────────────────┬────────────┘
                                │ 封印证据写入                   │ 复现/策展桥接
                                ▼                               ▼
┌────────────────────────────────────────────────────────────────────────────┐
│ Layer 3: 泛型持久化状态图与四层知识库（SQLite WAL + Git Markdown）           │
│  · 泛型 AND-OR-Conflict 因子图（claims/proof_plans/conflict_edges）[Phase 2]│
│  · 预注册与只追加试错账本（迁移 7）+ 持久异步作业账本（迁移 8）    [已落地]  │
│  · 四层 KnowledgeVault（CORE / SEDIMENT / GRAVEYARD / LITERATURE） [Phase 2]│
│    + 通用 entry_facets + 三通道 Shadow Hint + 可插拔 SemanticIndex 向量检索 │
└───────────────────────────────▲────────────────────────────────────────────┘
                                │ Membrane 1: 确定性溯源门禁 + 离线漂移测试床
┌───────────────────────────────┴────────────────────────────────────────────┐
│ Layer 4 (后置 · 领域特异): 外围文献摄入与抽取端口 [Phase 4–5，暂不接入外围]  │
│  · DocumentProvider / CitationRegistry / ProposalExtractor / BlindAuditor  │
│  · 原文逐字回查 + 单位换算 + 缺条件打标准入 + 撤稿/立场防火墙               │
└────────────────────────────────────────────────────────────────────────────┘
```

---

## 2. 已落地底座回顾（Commits `7648418` & `97a5028`）

### 2.1 领域包、认识论格与反漂移协议引擎（`7648418`）
* **通用认识论格与债务冒泡**（[domain.rs](../crates/ai4math-core/src/domain.rs)、[pack_itp.rs](../crates/ai4math-core/src/pack_itp.rs)）：
  * `EpistemicLevel`、`EpistemicDebt`、`EpistemicAssessment<E>` 与通用 AND-OR 聚合；
  * 内置科学认识论阶梯 `ScientificTrust`（`E0 / E1 / E2a / E2b / E3 / E4a / E4b / E5`）与 `BackendDescriptor` 正向/证伪双上限护栏；
  * 既有 ITP（Lean 4 / Agda / Rocq / SMT / ATP / CAS / Python）无损封装为内置 `ItpDomainPack`，历史 64 组偏序矩阵与线格式零变化。
* **封印证据端口与 `ProtocolSpec` 引擎**（[sealed.rs](../crates/ai4math-evidence/src/sealed.rs)、[protocol.rs](../crates/ai4math-evidence/src/protocol.rs)、[broker/protocol.rs](../crates/ai4math-broker/src/protocol.rs)）：
  * 无环 Merkle 承诺：`BlindedControlPack -> control_pack_sha256 -> ProtocolSpec -> protocol_sha256 -> CandidateSubmission / PreRegistrationSpec -> VerifiedProtocolRun`；
  * 单遍强类型槽位编译（严格 SMILES / FASTA / Identifier / SafeAscii 与有限数值边界，禁止换行、模板注入与未声明参数）；
  * Broker 自持解析器 + 确定性物理/数值健全性门禁 + 正交多验证器分歧显式化（`Disagreement`）+ 隐藏阳/阴性对照（防泄漏）；
  * 迁移 7（[protocol-verification.sql](../schemas/protocol-verification.sql)）落地不可变预注册与只追加试错账本（Bonferroni `effective_alpha`、选择偏差与 `SurrogateGoodhartCollapse` 等协议异味检测）。

### 2.2 多维本地 GPU 池与统一 SLURM 后端（`97a5028`）
* **多维资源模型与策略护栏**（[core/compute.rs](../crates/ai4math-core/src/compute.rs)）：`ResourceRequest`（CPU / RAM / GPU 卡数与显存 / scratch / 墙钟）、`ProtocolSpec.resource_ceiling` 与 `SlurmClusterPolicy`（partition/account/QOS 白名单、单作业与总在途卡数硬顶、不可变容器 SHA-256 强制校验）。
* **本地逐卡 GPU 与统一内存分配**（[compute/pool.rs](../crates/ai4math-broker/src/compute/pool.rs)）：`LocalComputePool` + RAII `ComputeLease`，支持独占卡/显存切片共享卡、Apple Silicon 统一内存联动扣减主机 RAM，并强制注入 `CUDA_VISIBLE_DEVICES` / `ROCR_VISIBLE_DEVICES`（CPU-only 任务显式置空防盗卡）。
* **双层执行与传输抽象**（[compute/backend.rs](../crates/ai4math-broker/src/compute/backend.rs)、[compute/slurm.rs](../crates/ai4math-broker/src/compute/slurm.rs)、[protocol_async.rs](../crates/ai4math-broker/src/protocol_async.rs)）：
  * 上层 `ComputeBackend` 统一 `LocalComputeBackend` 与 `SlurmComputeBackend<T>`；
  * 下层窄接口 `SlurmTransport`（零任意 shell 方法）同时支撑 `LocalSlurmCli` 与固定 `BatchMode` / `StrictHostKeyChecking=yes` 的 `SshSlurmTransport`（纯 Rust 有界 ustar tar 流式收发，硬拒软链接与路径穿越）；
  * 盲测对照走匿名 SLURM Job Array（`tasks/0..K-1`，解盲表仅留本地），远端运行自动附加 `unattested_remote_hpc` 信任债；
  * 迁移 8（[compute-jobs.sql](../schemas/compute-jobs.sql)）持久化在途作业、重启配额恢复与按分支标签批量 `scancel`。

---

## 3. 知识库平滑演进：四层 `KnowledgeVault` 与可插拔向量检索

在现有 [03-local-math-wiki.md](03-local-math-wiki.md)（Git Markdown 唯一事实源 + 单文件可重建 SQLite + 安全扫描护栏）基础上做向下兼容演进，**不另起平行系统**：

### 3.1 四层 Stratum 与隔离边界
| 层级 | 目录 | 来源属性 | 日常主干回归 | 默认搜索可见性 | 定位与职责 |
| :--- | :--- | :--- | :---: | :---: | :--- |
| **`CORE`** | `core/` | `project_verified` / `human_curated` | 是 | **默认返回** | 本项目已通过内核/协议封印验证的主干知识与人工 `pinned` 基准 |
| **`SEDIMENT`** | `sediment/` | `project_archived` | 否 | Shadow Hint | 本项目历史已验证冷储备（零持续算力，惰性重验） |
| **`GRAVEYARD`** | `graveyard/` | `project_refuted` | 否 | 独立碰撞雷达 | 硬证伪命题、失败分子/突变/收敛陷阱与死胡同教训 |
| **`LITERATURE`** | `literature/` | `literature_extracted` | 否 | Shadow Hint / 显式查询 | **外部论文/文献结构化参考卡**：永久携带 `unreproduced_literature` 债，**绝不冒充 `CORE`**；可选进入空闲算力复现/形式化队列，验证通过后经 `replicates` 边升格/关联入 `CORE` |

> 兼容性：若旧项目不存在 `literature/` 目录，扫描视为空，现有全部 `.mathwiki` 行为与测试零变化。

### 3.2 通用分面倒排（`entry_facets`）与领域知识画像
* 新增领域无关知识本体（`ai4math-core::knowledge`）：`KnowledgeStratum`、`KnowledgeSourceKind`、`ReproducibilityClass`（`FormalizableInItp | ReplicableByProtocol | RequiresWetLab | BackgroundOnly`）、`ExtractionStance` 与 `KnowledgeEdgeKind`（新增 `extracted_from | replicates | contradicts | calibrates_protocol | cites_secondary`）。
* 每个 `DomainPack` 通过 `DomainKnowledgeProfile` 声明本领域合法分面词表：
  * `itp`：`conclusion_head / hypothesis_head / typeclass / const`（与旧 `entry_symbols` 与 `WikiQuery` 保持双写/投影兼容）；
  * `compchem`：`inchikey / smiles / scaffold / reaction_class / level_of_theory / solvent / metric`；
  * `compbio`：`uniprot_id / pdb_id / mutation / assay_type / organism / fold_cluster`；
  * 通用文献面：`doi / arxiv_id / pmid / pub_year / citation_status / card_kind / claim_stance`。
* **三通道 Shadow Hint**：查 `CORE` 时同步静默扫描 `SEDIMENT`、`GRAVEYARD` 与 `LITERATURE`（附撤稿/冲突警报徽章），仅返回极简摘要 ID，零上下文污染。
* **冲突显式建模**：同一 `(entity, target, metric)` 下存在显著数值或极性分歧的条目自动归入冲突簇并暴露 `contradicts` 边，严禁静默覆盖或盲目取均值。

### 3.3 可插拔语义向量检索抽象
为保留“零外部依赖、纯离线确定性重建”的底线，将向量检索抽象为两层可选 trait：
* `EmbeddingProvider`：`model_id()`、`dim()`、`embed_batch(&[&str]) -> Result<Vec<Vec<f32>>>`；
* `SemanticIndex`：按 `(entry_id, content_sha256, model_id, dim)` 键控缓存，支持 `upsert` / `remove_stale` / `search_top_k`。
* 内置两种零外部依赖实现：
  1. `NoopSemanticIndex`（默认，纯 `Facet + FTS5` 离线退化）；
  2. `InMemoryCosineIndex`（纯 Rust 规范化余弦检索 + SQLite blob 缓存，供单测与轻量本机场景使用，后续可无缝替换为 `sqlite-vec`）。
* 检索排序采用 **三路 RRF 融合（精确分面 + FTS5 BM25 + 可选语义向量）**，且单路关闭时行为严格确定。

---

## 4. 泛型主状态图与双通道 Belief Propagation

### 4.1 泛型主状态图（迁移 9）
将 [state-store.sql](../schemas/state-store.sql) 中写死 ITP 的 `claims` / `proof_plans` 平滑升级为多领域通用的 **AND-OR-Conflict 因子图**：
* `claims` 扩展 `domain_id`（旧行默认 `'itp'`）、`epistemic_debts_json`、`source_entry_id`（挂载关联的 `LITERATURE` 条目 ID）与 `belief_json`（缓存的 BP 信念快照）；
* `engine` 列放宽为受控 `backend_id`，同时由 SQLite 触发器强约束领域一致性：**`domain_id = 'itp'` 仅允许 `L0..L5`，非 ITP 领域仅允许 `E0..E5`**，既实现泛化又不削弱原有 ITP 强校验；
* 新增 `claim_conflict_edges` 表，显式表达 `contradicts` 与 `replicates` 因子关系（例如本地计算与某篇论文冲突、或两篇论文互相矛盾）。

### 4.2 准入打标 + 双通道传播代数（`ai4math-core::belief`）
针对论文/外部数据常见的“原文锚点真实存在，但缺少部分实验条件（如未写 pH 或温度）”场景，系统采用 **“准入但打标签 → 在结论推演阶段统一做双通道回传计算”** 的策略：

#### 通道 1：硬认识论底线（不可抵消，保障绝对健全性）
* 严格维持离散 `effective_level`（AND 取 `min`，OR 取 `max`）与全量 `EpistemicDebt` 向上并集传播；
* 任何硬证伪（`L2b` / `E2b`）或上游 `retracted` 撤稿源触发硬否决/警告；
* **不可抵消原则**：通道 2 的连续置信度**绝不能**把大量弱启发（`E1`/`L1`）或未复现文献洗白为 `CORE / L5 / E4b`。

#### 通道 2：Subjective Logic `(b, d, u)` 因子图 Belief Propagation
每个图节点与证据因子维护 Subjective Logic 三元组 **`Opinion { belief: b, disbelief: d, uncertainty: u }`**（满足 `b, d, u >= 0` 且 `b + d + u = 1`）与显式冲突质量 `conflict_mass`：

1. **标签驱动的确定性衰减（`TagAttenuationPolicy`）**：
   * 不允许 LLM 拍脑袋给文献打分；每类准入标签通过可审计的纯规则表确定性压低 `b` 上限、抬升 `u` 下限（或注入 `d`）：
     * `unreproduced_literature` → 限制 `b` 上限，保持显著 `u`；
     * `incomplete_conditions`（缺温度/pH/溶剂/突变体等）→ 按缺失条件等级继续把 `b` 折算回 `u`，并禁止直通高精度对照；
     * `secondary_citation` / `author_speculation` → 强压 `b`，近乎纯不确定；
     * `retracted` → 硬置 `b = 0` 并注入高 `d` 与撤稿债务。
2. **因子图前向传播（带共享祖先去重）**：
   * **AND 因子（`ProofPlan` / 论证链）**：支撑度受限于所有前提的联合（合取衰减），任一前提的高 `d` 向上传播反证压力；
   * **OR 因子（`Claim` 多条进攻路线）**：多条**独立证据路径**做累积/冲突感知融合（同时降低 `u`，若正负证据冲突则同时抬升 `b`、`d` 与 `conflict_mass`）；
   * **共享上游依赖防重复计票**：通过节点血缘祖先集（provenance set）识别菱形共享依赖（例如两条路线共用同一受体 PDB、同一力场或同一篇论文），避免把同一证据重复加权；对含环或密集共享因子图采用**有界迭代的阻尼消息传递（Damped Loopy BP）**，保证确定性收敛与可重放。
3. **反向敏感度回传（Backward Sensitivity / Value of Information, VoI）与归因**：
   * 面向每个关键路径根节点计算各上游叶子/文献节点的 **不确定度敏感度 `VoI(v) = Δb_root / Δu_v`** 及**归因摘要**（例如：“主猜想当前 `u = 0.46`，其中 68% 来自未复现且缺溶剂条件的 `LIT-042`”）；
   * 反向 `VoI` 直接驱动两件事：① `DecisionInbox` 杠杆排序；② **空闲算力复现/形式化队列的自动优先级**。

---

## 5. 空闲算力复现与形式化排期（`IdleVerificationScheduler`）

对于 `LITERATURE` 层中标记为 `FormalizableInItp`（可形式化证明）或 `ReplicableByProtocol`（可计算复现）的条目，在算力空闲窗口自动排期验证，验证通过后纳入 `CORE` 主体知识库：

### 5.1 双来源入队模型
1. **BP 自动推荐队列（VoI-Driven）**：被当前活跃 Claim Graph 引用、`VoI > 0`、未撤稿且尚未本地复现的 `LITERATURE` 条目，按 `VoI` 降序自动进入候选池；
2. **人类显式排期队列（Human Verification Queue）**：人类可通过 CLI / DSH 工作台显式提交单条或批量 `LITERATURE` 条目（可指定目标 `ProtocolSpec` / 主引擎、自定义优先级与截止预算），即便该文献暂未被当前活跃 Claim 引用，也会在算力空闲时按顺序执行复现或形式化。

### 5.2 三条防失控护栏
1. **严格让路前台（Zero-Starvation Gate）**：仅当宿主机 `Pressure::Green`、Heavy 队列为空且 `LocalComputePool` / SLURM 在途配额低于预设空闲水位（如 `< 50%`）时才派发；前台 Track A/B 或 T1/T3 到达时立即暂停新空闲派发并允许抢占/取消。
2. **独立空闲预算天花板（`IdleBudgetPolicy`）**：与前台预算隔离，独立限制每日/每周期最大空闲 GPU-小时、CPU-小时、SLURM 作业数与 LLM 试错轮次。
3. **双向证据闭环**：
   * 复现 / 形式化通过（取得 Broker 封印回执 `L5` 或 `E4a/E4b`）→ 写入 `CORE` 并挂接 `replicates` 边 → 触发 BP 重算，自动清偿上游 `unreproduced_literature` 债务；
   * 复现 / 形式化失败或显著偏离 → 绝不静默丢弃，挂接 `contradicts` 边（附封印诊断与误差）→ 触发 BP 抬升冲突度与 `d`，关键路径推送 `DecisionInbox`。

---

## 6. 后置阶段：论文抽取溯源门禁与实地漂移测试床（先数学基线、后化生领域）

因为论文抽取的卡片字段、前提条件与实地漂移风险具有高度领域特异性，故将其**刻意后置到阶段 1–3 的领域无关底座全部稳固之后**，并遵循 **「先拿已建成的数学基线文献做验证 → 再扩展到 CompChem / CompBio 文献」** 的两段式验证纪律：

* **为什么先测数学基线文献？**
  1. 现有数学基线（Lean 4 / Agda / Rocq 内核 + Oracle 反例探测 + 阶段 3 空闲形式化排期）已经具备**本机可执行的确定性裁决内核**；
  2. 数学论文完整覆盖了最核心的实地漂移形态——**章节前置全局假设剥离**（如“Section 3 默认图有限连通”，等价于化生里的缺 pH/温度）、**量词/不等号/LaTeX 符号漂移**、**立场膨胀**（把猜想/开放问题/他引定理抽成本文已证定理）、**预印本撤回/勘误版本冲突**与**正文提示词注入**；
  3. 用数学文献先跑通「论文抽取 → Membrane 1 溯源门禁 → `LITERATURE` → BP VoI / 人类队列 → 空闲形式化/反例核验 → `CORE`/`contradicts`」全闭环，能用形式化内核客观标定抽取漂移率，避免在化生领域叠加实验条件与物理近似噪声导致归因困难。
* **端口解耦**：外围统一拆为 `DocumentProvider`、`CitationRegistryPort`、`ProposalExtractorPort` 与可选 `BlindGroundingReviewerPort`，保持 Rust 底座 100% 离线确定性；
* **Membrane 1 通用信封 + 分领域卡片插件（`ai4math-evidence::literature`）**：
  * **通用信封（阶段 4a）**：`SourceDocumentRef`（含 arXiv 版本/撤稿/勘误状态）、`VerbatimAnchor`（逐字原文回查）、`ExtractionStance`（`direct_proof / direct_measurement / direct_computation / secondary_citation / author_speculation`）、缺条件准入打标与封印端口 `GroundedLiteratureEvidence`；
  * **数学基线文献卡型（阶段 4a 首批验证）**：定理/引理/定义/猜想/反例卡 + 章节级全局前提（`standing_assumptions`）完整性检查 + 6 维数学文献对抗漂移基准（`LiteratureDriftSuite`）；
  * **化生领域文献卡型（阶段 5 扩展）**：定量实验/计算测量卡、单位双轨规范化（`kJ/mol ↔ kcal/mol`、`uM ↔ nM` 等）、协议与盲测对照配方卡、负结果/PAINS 陷阱卡。

---

## 7. 分阶段实施路线图

1. **阶段 1（领域无关内核）：`ai4math-core::belief` + `ai4math-core::knowledge`**
   * Subjective Logic `(b, d, u)`、标签衰减规则表、AND-OR-Conflict 因子图、带共享祖先去重与反向 VoI 敏感度的确定性 BP 引擎；
   * 四层 `KnowledgeStratum`、`ReproducibilityClass`、`KnowledgeEdgeKind` 与 `DomainKnowledgeProfile`。
2. **阶段 2（存储与检索泛化）：`ai4math-store` 泛型主状态图（迁移 9）+ 平滑演进四层知识库与向量检索抽象**
   * 迁移 9 泛化 `claims`/`proof_plans` + `claim_conflict_edges` + BP 快照（严格保留 ITP 领域约束与全部存量单测零回归）；
   * 平滑演进 `Wiki` 支持 `literature/` 层、`entry_facets`、三通道 Shadow Hint、冲突簇，以及 `EmbeddingProvider` / `SemanticIndex`（`Noop` + 内存余弦 + 三路 RRF）。
3. **阶段 3（空闲算力自加固闭环）：`IdleVerificationScheduler` + 人类待核验队列**
   * 接入 BP 反向 VoI 队列 + 人类显式队列（持久表/迁移 10）+ `LocalComputePool`/`SlurmComputeBackend` 空闲准入护栏 + 独立空闲预算 + `replicates`/`contradicts` 闭环单测。
4. **阶段 4（数学基线文献抽取门禁与漂移实测，不接 CompChem）**：`ai4math-evidence::literature` 通用溯源信封 + 数学文献卡型（定理/定义/猜想/反例 + 章节前置假设检查）+ 6 维离线对抗漂移基准；在离线基准达标后，先针对现有数学基线相关论文做实地抽取与 Lean 4 / Oracle 空闲形式化闭环验证。
5. **阶段 5（拓展非 ITP 领域）：CompChem / CompBio 文献卡型扩展与首个计算化生端到端试点**
   * 在阶段 4 数学文献抽取消除主要实地漂移后，再扩展化生专属卡片（定量观测/单位归一化/协议对照配方/负结果陷阱），并接入首个单机 CompChem / CompBio 计算后端（如 RDKit + xTB + Psi4/ORCA 或 OpenMM）。
