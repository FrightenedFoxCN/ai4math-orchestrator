# 04. 常驻生命体征监控与复盘简报架构（Live Telemetry, Review Briefing & HITL）

## 1. 核心分层：观测态与复盘态彻底解耦

在无人值守 + 人类不定期介入的模式下，必须把**“随时扫一眼系统是否在健康运转”**和**“坐下来花 5 分钟做数学方向复盘与拍板”**拆成两个独立子系统：

1. **常驻生命体征面（Always-On Live Vital Signs）**
   * **零 LLM 消耗、零简报触发、不移动人类游标**；
   * 由确定性后端持续刷新到本地实时状态文件（`.runtime/status/vital-signs.json`）并驱动终端 HUD / 本地 Web 顶部状态条；
   * 让人随时一眼看到：当前活跃子任务数、单机内存/Broker 双车道负载、Trust Ledger 实时存量、熔断器状态与待处理 Inbox 计数。
2. **事件驱动的复盘简报（On-Demand / Push-Triggered Review Briefing）**
   * **只在两种时刻生成并留存**：① 触发 **P0 Push Alert** 主动唤起人类时；② 人类主动拉取查看时；
   * 严格遵循 **“确定性事实包（Fact Pack）+ 有界双语数学叙事（Bilingual Narrative）”** 两段式流水线，杜绝 LLM 报喜不报忧的乐观幻觉。

```text
[Event Log + Claim Graph + Trust Ledger + Broker Metrics + Circuit Breakers + Wiki]
           │                                             │
           ▼ (持续刷新，纯代码/SQL，0 LLM)                 ▼ (仅 Push Alert 或人类拉取时触发)
┌─────────────────────────────┐             ┌────────────────────────────────────┐
│  常驻生命体征条 (Live HUD)   │             │  Deterministic Fact Pack Compiler  │
│  - 内存/Broker/活跃 Worker   │             │  - (last_human_cursor, NOW] 净增量  │
│  - L0-L5 实时计数 / stale   │             │  - Active Frontier DAG 切片         │
│  - 熔断器数量 / Inbox 数量   │             │  - Definition Smell & 信任债统计    │
└─────────────────────────────┘             └─────────────────┬──────────────────┘
                                                              ▼ (有界合成，强锚定实体 ID)
                                            ┌────────────────────────────────────┐
                                            │  双语并排复盘简报 (Informal + Formal)│
                                            │  - 仅在触发人类或查看时落盘留存       │
                                            └─────────────────┬──────────────────┘
                                                              ▼ (讨论 + Pivot Dry-Run 预览)
                                            ┌────────────────────────────────────┐
                                            │  原子执行 + ADR 落盘 + 推进 Cursor  │
                                            └────────────────────────────────────┘
```

---

## 2. 常驻生命体征条（Always-On Vital Signs Surface）

生命体征条**不纳入复盘简报正文**，而是作为长期存在的实时看板，刷新间隔 2–5 秒（或事件驱动更新）：

| 维度 | 常驻展示字段 | 目的 |
| :--- | :--- | :--- |
| **系统与资源** | `broker_fast_pool`（可用/忙碌）、`broker_heavy_queue`（运行中/排队）、单机 RAM 水位（%）、活跃子代理数 | 一眼判断单机是否接近内存墙或出现编译堵塞 |
| **双轨脉搏** | 探索轨状态（active / suspended / idle）、基建轨状态（活跃 RFC 列表与完成比） | 判断算力是否在有效产出 |
| **Trust Ledger 实时盘口** | `L5` / `L4b` / `L4a(债)` / `L3` / `L2b` / `L2a` / `stale` 实时计数，以及距上次检查点的净增量徽章（如 `+6 L5`） | 无需打开简报即可感知整体进度与信任债 |
| **告警与待办指示灯** | 当前处于 `TRIPPED` 状态的熔断器数、待处理 `DecisionInbox` 条数（按 P0/P1 分色）、最近一次 P0 Push Alert 摘要 | 决定此刻是否值得拉取完整简报介入 |

---

## 3. 触发机制：按需拉取（Pull）+ 紧急主动推送（Push Alert）

### 3.1 按需拉取（Pull）
* `briefing pull`：以 `last_human_cursor` 为起点编译 Fact Pack 并生成双语简报；支持 `--peek` 只读浏览模式（落盘查看记录，但不推进检查点游标）。
* 同样支持指定任意历史窗口（例如“自上次 ADR 以来”或“过去 24 小时”）做回溯对比。

### 3.2 主动推送（P0 Push Alert）
无人值守期间，规则引擎仅在命中以下 **P0 级高价值中断条件** 时发送轻量推送（本地桌面通知 / Webhook / 终端响铃），并自动预生成对应简报留存待查：
1. **顶层或关键路径核心猜想被内核硬证伪（`L2b` Kernel-Refuted on Critical Path）**：原主路线逻辑崩塌，急需人类决策转向（`L2a` 须先经 `WITNESS_VERIFY` 核验升格为 `L2b` 后才推 P0，防 Oracle 误报）；
2. **全轨道停摆（Global Starvation / All Tracks Blocked）**：所有活跃探索与基建轨道全部挂起或等待审批，机器已无有意义任务可跑；
3. **关键路径触发 Definition Smell 熔断且无替代轨道**：主基建定义被局部熔断，亟需人类判断恢复或重构；
4. **验证底座严重异常（Broker / Regression Meltdown）**：例如连续多次 T1 Promotion 失败回滚、或内存保护连续驱逐主引擎热池。

> **防骚扰约束**：同源 P0 事件内置去重与冷却窗口（Cooldown），绝不反复轰炸；常规进展和普通 `L5` 增量绝不触发 Push Alert（通过常驻生命体征条静默可见）。

---

## 4. 复盘简报结构（双语并排呈现）

剥离生命体征条后，主简报聚焦于**数学内容、前沿瓶颈、风险与决策**，压缩为 **5 个高密度版块**。

### 双语并排规范（Bilingual Side-by-Side）
凡涉及**关键里程碑、前沿瓶颈、定义异味与决策收件箱**的数学条目，默认采用 **非形式化数学（自然语言 + LaTeX）与精确形式化签名（Lean 4 / Rocq / Agda / SMT）并排展示**：
* 只展示**声明/定义签名**（含隐式参数、typeclass 约束与关键前提），**绝不在主简报里贴大段 proof body**；
* 价值：数学家在 5 分钟内既能看懂直觉意义，又能一眼识破隐藏在隐式参数、宇宙层级或 typeclass 里的形式化陷阱。

示例双语卡片格式：
```markdown
#### [CLM-108] 谱隙正性引理（Status: L2b Kernel-Refuted · Lean 4）
| 非形式化数学表述 | 精确形式化签名（Frozen Prefix） |
| :--- | :--- |
| 对任意有限简单图 $G$，其归一化拉普拉斯算子的第二特征值 $\lambda_2(G) > 0$。 | `lemma spectral_gap_pos {V : Type*} [Fintype V] [DecidableEq V] (G : SimpleGraph V) : 0 < normalizedLaplacianEigenvalue₂ G` |
- **证伪证据（`GRV-014`）**：缺少连通性假设 `G.Connected` 与非平凡顶点数 `2 ≤ Fintype.card V`；SMT/Lean 给出反例 `V = Fin 2, G = ⊥`（`λ₂ = 0`）。
```

### 5 个固定版块
1. **数学前沿增量（Breakthroughs & High-Impact Refutations）**：仅列出改变关键路径的 `L5`（及带债标注的 `L4a`/`L4b`）里程碑、推翻原路线的 `L2b` 硬证伪反例及整包交付的 `InfraRFC`；常规辅助引理仅报聚合计数。
2. **活跃证明前沿与瓶颈地图（Active Frontier Slice）**：仅裁剪 Claim Graph 中距离主目标最近的未证边界子图（ASCII/Mermaid），每个瓶颈带类型标签（`[MATH_IDEA]` / `[MISSING_INFRA]` / `[DEF_FRICTION]` / `[ENGINE_WALL]`）及双语签名。
3. **早期风险雷达（Early Warning Radar）**：
   * **Definition Smell 熔断报告**（见第 5 节）；
   * **信任债清单**：存量/新增 `L4a` Oracle 债务（附风险等级与交叉印证状态）、特批 `L4b` 跨 ITP 桥接（若有）及 `stale` 子树规模；
   * **回归与算力异常**：`maxHeartbeats` 飙升文件、零产出高消耗探索分支。
4. **高杠杆决策收件箱（Decision Inbox Top-3）**：按杠杆分（解阻塞下游节点数 × 算力释放量）排序，每条卡片包含：双语问题与证据、2–3 个备选方案（含代价与推荐项）、以及“暂缓拍板的非阻塞影响”。
5. **Wiki 与考古摘录（Wiki & Archaeology Digest）**：本周期沉降入 `SEDIMENT` 的独立引理数（最多 2 条高亮）、新增 `GRAVEYARD` 计数、以及卡壳时由 **Shadow Hit** 浮现的历史伏笔线索。

---

## 5. Definition Smell 局部熔断器（可恢复状态机）

为防止劣质定义整夜空烧单机算力，同时避免误报导致前功尽弃，围绕每个稳定定义模块引入**可恢复的局部熔断器（Resumable Local Circuit Breaker）**。

### 5.1 触发条件（确定性启发式，满足任一即熔断）
* 同一上游定义在滑动窗口内，有 **`≥ 2` 个独立 Prover Worker** 返回 `BLOCKED_SPEC_DEFECT`；
* 围绕该定义的同构 elaboration / typeclass 推导失败连续超过阈值（如 `≥ 6` 次且零进展）；
* 该定义引发连续 T1 回归失败或下游编译 `maxHeartbeats` 较基线暴涨（如 `> 2x`）。

### 5.2 熔断动作（止血但不破坏现场）
1. 状态切换为 `TRIPPED`，**仅隔离该定义的受影响下游队列**，将其余无关探索/基建轨道继续运行，单机算力立即让渡；
2. **完整保留可恢复现场（Zero-Destruction Freeze）**：
   * 尚未启动的下游任务转入 `PAUSED_BY_BREAKER` 冻结队列（保留优先级与上下文切片）；
   * 正在运行的关联子任务收到温和停止信号，先完成已证局部引理的回收（`SEDIMENT`），再落盘检查点退出；
   * 对应 CoW worktree 与编译缓存按租约保留（打上 `breaker_pinned` 标记免于 GC 清理）；
3. 自动向 `DecisionInbox` 写入一条高优先级条目（若该定义位于主关键路径且无替代轨道，则同时触发 P0 Push Alert）。

### 5.3 三种显式恢复路径（人类拍板后执行）
```text
                     ┌────────────────────────┐
                     │        CLOSED          │ (正常运行)
                     └───────────┬────────────┘
                                 │ 触发 Definition Smell 阈值
                                 ▼
                     ┌────────────────────────┐
                     │        TRIPPED         │ (局部冻结队列 + 保留 worktree/状态)
                     └──┬────────┼─────────┬──┘
       1. 误报/补提示    │        │         │  3. 放弃该路线
     (RESUME_AS_IS)     │        │         │ (ARCHIVE_AND_PIVOT)
                        ▼        │         ▼
          ┌──────────────────┐   │   ┌───────────────────────────┐
          │    HALF_OPEN     │   │   │  归档旧分支 + 回收引理入    │
          │ (先放行1-2探针任务)│   │   │  SEDIMENT + 释放 worktree  │
          └────────┬─────────┘   │   └───────────────────────────┘
                   │ 探针通过     │ 2. 修订定义 (PATCH_DEFINITION)
                   ▼             ▼
                CLOSED    回到 Infra Builder 修订定义
                          → 触发下游 T3 标脏重检
                          → 新烟雾测试通过后解冻队列恢复运行
```

* **路径 1（`RESUME_AS_IS`）**：人类判定定义本身没问题（例如只需给 Prover 补一条 tactic 提示或补充某个 Mathlib 引理线索），熔断器切入 `HALF_OPEN`，从冻结队列优先放行 1–2 个探针任务验证通过后，无缝恢复全部原冻结队列；
* **路径 2（`PATCH_DEFINITION`）**：人类批准修正定义，Infra Builder 在保留的 worktree 上改定义、跑烟雾测试、触发下游受影响切片 T3 标脏，再恢复证明队列；
* **路径 3（`ARCHIVE_AND_PIVOT`）**：确认抽象路线彻底走不通，将已有独立引理无损沉降进 `SEDIMENT`，记录教训进 `GRAVEYARD`，释放 worktree 资源并转向新 RFC。

当前可用的是 [独立审批、仅归档指定熔断的 `ARCHIVE_BREAKER`](architecture-acceptance.md#L650)。它保留模块门禁和所有现场，不替代完整的归档转向。 还可 [明确选中并审批实际证明路线](architecture-acceptance.md#L658)，归档不删除依赖边，也不产生替代任务。

---

## 6. 留存策略、讨论预览与游标推进

### 6.1 简报留存策略（零后台垃圾文件）
* **禁止**后台无人查看时按小时定时批量生成空转简报文件；
* **仅在两类事件发生时落盘留存**：
  1. 触发 **P0 Push Alert**（写入 `.runtime/briefings/<id>.json` 与 `.md`，标记 `trigger = "push_alert"`）；
  2. 人类主动执行拉取查看（标记 `trigger = "human_pull"`）；
* **Git 清洁边界**：所有常规留存简报默认存放在 `.runtime/briefings/`（已被 `.gitignore` 忽略）；只有当某次复盘讨论最终产出了正式的 **`ADR`** 时，才将该期简报的精简摘要与 Fact Pack 哈希绑定写入对应 ADR 文件纳入 Git，确保长期可追溯且不污染仓库历史。

### 6.2 讨论后的原子化落地：`PivotActionPlan` Dry-Run
人类结合简报与 Chief Orchestrator 讨论完毕并拍板后，系统先生成一份结构化的 **`PivotActionPlan`** 预览给人类确认：
1. 将新写入的 `ADR` 编号与核心内容；
2. 将要恢复（`RESUME_AS_IS` / `PATCH_DEFINITION`）或归档的熔断器列表；
3. 将要优雅停止的在跑任务（以及停前是否强制执行引理回收）；
4. Claim Graph 中被标记为 `stale` / `archived` 的节点清单；
5. 新下发的 `InfraRFC` 与探索任务。

人类确认后，系统原子化执行上述动作，并正式推进 `human_checkpoint_cursor` 至本次提交点。
