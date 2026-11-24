# 03. 本地数学 Wiki 与分层混合检索设计（Local Math Wiki）

## 1. 设计目标

开放数学研究中的知识资产具有独特的生命周期特征：
* **不可删除性**：暂时未被主干引用的引理（`unreferenced`）、废弃分支的局部证明、以及被证伪的猜想与反例，极可能在数周后的路线转向中成为关键拼图，因此**绝不做自动清理删除**。
* **主体低噪声**：日常证明和基建开发必须聚焦于当前主干与活跃路径，避免被历史沉降物污染上下文，也绝不能让历史孤立引理拖累单机 CI 回归算力。
* **必要时精准唤醒**：当 Agent 卡壳、起草新定义、转向新路线或撞上高度相似的历史成果/反例时，沉降知识能以极低延迟浮现。
* **跨引擎可检索**：同一数学事实可能先后出现在 Lean 4、Rocq、Agda 或 SMT 中，查询不能只靠单一语言的字符串匹配。

---

## 2. 三层沉降模型（Stratified Vault）

所有入库条目永久保留，按活跃度与语义角色分布于三个显式层级（Stratum）：

| 层级 | 定位 | 典型内容 | 日常 CI 回归 | 默认搜索可见性 |
| :--- | :--- | :--- | :---: | :---: |
| **`CORE`**（主体记载） | 活跃作战面 | 主干已验证基建（`L5`/`L4`）、当前活跃证明路径节点、高复用核心定理、人工 `pinned` 条目 | **是**（与当前 `HEAD` 同步） | **默认返回** |
| **`SEDIMENT`**（沉降冷储备） | 永不删除的地下室 | 失败任务回收的独立已证引理、废弃分支局部成果、暂未吸收的跨引擎原型、阶段性非形式化思路 | **否（零持续算力）** | **默认屏蔽**（经由 Shadow Hint 或显式触发唤醒） |
| **`GRAVEYARD`**（证伪与避坑墓园） | 独立避坑雷达 | `L2: Refuted` 命题及反例 witness、已证死胡同及根因分析、被否决的劣质定义设计 | **否** | **不进正向引理召回，走独立碰撞预警通道** |

### 核心机制：不可变快照 + 惰性重验（Lazy Re-hydration）
* 条目从 `CORE` 降级或直接回收进入 `SEDIMENT` 时，**移出主工程编译树**，存为不可变快照：保留完整形式化源码、校验通过时的 `verified_at_commit` 以及它所依赖上游定义的签名哈希 `def_fingerprints`。
* 平时消耗 **0 回归 CPU、0 默认上下文 Token**。
* 仅当某条 `SEDIMENT` 条目被唤醒准备复用时，才触发一次 `wiki_rehydrate`：
  1. 比对 `def_fingerprints` 与当前 `HEAD`；
  2. 若上游定义未变，零成本验证后直接升格回 `CORE`；
  3. 若上游定义已演进，派发一个轻量适配任务修补并通过 Comparator 门禁，新版本升入 `CORE`，原快照永久保留并挂接 `superseded_by` 血缘边。

---

## 3. 三级渐进唤醒机制（Escalated Retrieval）

为解决“平时沉底零干扰”与“真需要时能及时想起来查”的矛盾，检索接口内建三级唤醒策略：

### Level 1：默认静默（`scope = "core"`）
常规 `wiki_search` 仅返回 `CORE` 命中（及外部库检索结果），保证 Prover/Builder 上下文极度聚焦。

### Level 2：影子命中提示（Shadow Hit Teaser）
每次执行 `scope = "core"` 查询时，SQLite 顺便在毫秒内对 `SEDIMENT` 与 `GRAVEYARD` 做一次轻量索引扫描。**不返回正文或代码**，仅在满足以下条件时于结果末尾追加一行极短提示：
* `CORE` 高相关命中稀少（如 `< 2` 条），且 `SEDIMENT` 存在高分匹配（如符号签名同构或融合分 `> 0.82`）；
* 或 `GRAVEYARD` 存在高度相似的已证伪命题。

示例输出尾注：
```text
[Shadow Hint] 2 high-match entries in SEDIMENT (top: SED-089 "Haar convolution L2 bound", score=0.91, lean4@a1b2c3); 1 warning in GRAVEYARD (GRV-014). Call wiki_search(scope="sediment"|"graveyard") to inspect.
```

### Level 3：规则强制唤醒（Hard Archaeology Triggers）
在以下 4 个工作流节点，强制触发对 `SEDIMENT` / `GRAVEYARD` 的深度检索：
1. **Explorer 提出新猜想 / Infra Builder 起草新定义前**：强制调用 `wiki_check_graveyard`，拦截重复踩坑；
2. **子任务卡壳升级（`BUDGET_EXHAUSTED` / `BLOCKED_*`）**：Chief Orchestrator 二次重规划前自动跑一次 `scope="sediment"` 考古；
3. **人类介入转向（Pivot）落盘 ADR 时**：用新路线关键词扫描 `SEDIMENT`，批量识别可打捞的伏笔引理；
4. **人类显式考古查询**：随时按主题、分支或符号翻查历史沉降物。

---

## 4. 四维混合数学索引（专为形式化数学优化）

单一的文本向量检索在形式化数学中极易因变量改名、语法差异和逻辑极性翻转失效。每条条目在入库时自动提取 4 个正交检索面：

1. **符号结构面（Symbolic Structure，类 Loogle / Hoogle）**
   * 提取 `conclusion_head`（结论主符号）、`hypothesis_heads`（前提主符号集）、`typeclasses`（类型类约束）、`constants`（引用的关键定义）；
   * 支持按逻辑方向精准过滤（例如：“结论头为 `LE.le` 且前提包含 `IsCompact` 的引理”）。
2. **自然语言数学释义面（Informal Gloss + Taxonomy）**
   * 入库时由廉价模型生成 2–3 句规范数学表述（含标准术语与 LaTeX）和领域标签；
   * **消除多引擎语法鸿沟**：在 Lean 4 探索时也能检索到当年沉降在 Rocq / Agda 中的同一数学事实。
3. **血缘图谱面（Provenance & Dependency Graph）**
   * 记录 `depends_on`、`used_by`、`refutes`、`generalizes`、`superseded_by`、`born_from_claim`，支撑沿依赖邻域扩散检索与反例关联。
4. **多路 RRF 融合排序**
   * 并行执行：**符号结构匹配（SQLite 倒排）+ 词法 BM25（SQLite FTS5）+ 可选语义向量（`sqlite-vec`）**；
   * 支持零外部依赖离线退化（关闭向量表时仅靠符号倒排 + FTS5 仍具备极高召回精度）。

---

## 5. 双层存储架构（Git 源文件 + 可重建单文件 SQLite）

### 5.1 源文件层（唯一事实源，纳入 Git）
```text
.mathwiki/
├── core/                  # 主体记载条目（Markdown + YAML Frontmatter）
├── sediment/              # 沉降条目快照（永不删除，按主题/ID 归档）
├── graveyard/             # 反例与死胡同条目
└── .index.sqlite          # 本地查询索引（加入 .gitignore，可随时全量重建）
```

单条 Markdown 条目约定格式：
```markdown
---
id: SED-0089
stratum: sediment
trust_level: L5
engine: lean4
status: unreferenced
pinned: false
verified_at_commit: a1b2c3d
origin_claim: CLM-072
def_fingerprints:
  "Infra.Haar.LocallyCompactDual": "sha256:9f82..."
symbols:
  conclusion_head: "LE.le"
  hypothesis_heads: ["Integrable", "IsCompact"]
  typeclasses: ["LocallyCompactSpace", "MeasureSpace"]
  constants: ["plancherelMap"]
tags: ["harmonic-analysis", "haar-measure", "l2-bound"]
---

## Informal Statement
局部紧群上紧支集连续函数的卷积 $L^2$ 范数估计...

## Formal Artifact
```lean
lemma haar_conv_l2_bound ... := by ...
```

## Provenance & Notes
来自归档探索分支 `exp/plancherel-direct`，已在 commit `a1b2c3d` 零 sorry 通过 Comparator。
```

### 5.2 索引库层（SQLite）
完整可执行 DDL 见 [../schemas/wiki-index.sql](../schemas/wiki-index.sql)，核心包含：
* `entries`：主元数据、信任等级、沉降层级、引用计数与定义指纹；
* `entry_symbols`：符号结构倒排表；
* `entry_edges`：血缘与依赖图边表；
* `entries_fts`：SQLite FTS5 虚表；
* `entries_vec`（可选）：`sqlite-vec` 语义向量虚表。

---

## 6. 入库去噪与无损沉降闭环

* **入库门槛（防止沉降库堆满垃圾碎片）**：
  1. 回收进入 `SEDIMENT` 的形式化引理必须是**独立闭合声明**（0 `sorry`，不依赖匿名局部上下文）；
  2. **去重归并**：若新引理与已有条目 `conclusion_head` 相同且签名高度等价或严格更弱，则不新建冗余条目，而是作为历史版本合并或跳过。
* **自动沉降规则**：定期扫描 `CORE` 中 `ref_count == 0`、不在任何活跃 Claim 依赖闭包内且 `pinned == false` 的非核心辅助引理，自动移入 `SEDIMENT` 冻结快照；**零删除、零人工整理负担**。
* **暴露给 Agent 的 4 个工具原语**：
  * `wiki_search(query, conclusion_head?, hypothesis_heads?, typeclasses?, engine?, scope="core", include_shadow_hint=true)`
  * `wiki_check_graveyard(informal_statement, formal_signature?)`
  * `wiki_ingest(entry_payload)`
  * `wiki_rehydrate(sediment_id, target_module)`
