# 07. Claim Graph 状态机、标脏传播与持久化事件流（Claim Graph & State Machine）

## 1. 设计目标

Claim Graph 是整个 AI4Math 系统的逻辑骨架，负责把**开放数学探索中的多路线并存**、**对称信任账本（`L0`–`L5`）**、**上游定义变更后的精确回归（Taint Propagation）**以及**单机状态持久化事件流**统一成一张可确定性计算、可审计、零幻觉的机器状态图。

---

## 2. AND-OR 逻辑图结构（Claim × ProofPlan）

在开放数学问题中，同一个命题 $P$ 往往有多种不同的进攻思路（例如：路线 A 依赖谱理论引理，路线 B 依赖组合极值引理）。如果只把 Claim 直接连成普通 DAG，某条中间引理被证伪（`L2b`）时系统无法区分“这条思路死了”还是“主猜想死了”。

因此系统采用 **AND-OR 双节点类型图**：
* **`Claim`（OR 节点）**：表示一个数学命题。只要它的任意一条 `ProofPlan` 达到目标信任度，该命题即得到支撑；
* **`ProofPlan`（AND 节点）**：表示针对某个目标 `Claim` 的一条具体证明方案（例如某个探索分支或 `InfraRFC` 对应的证明计划），依赖一组前置子命题 `{C_1, ..., C_k}`。只有这组前置子命题**全部**成立，该方案才闭合。

```text
                [Claim: Main Conjecture]  (OR)
                  /                  \
     [Plan A: Spectral] (AND)   [Plan B: Combinatorial] (AND)
       /            \                     /          \
 [CLM-101 L5]  [CLM-108 L2b!]      [CLM-201 L5]  [CLM-205 L4a]
        ▲
        └─ CLM-108 被硬证伪 → 仅 Plan A 标为 INVALIDATED_BY_REFUTATION；
           Main Conjecture 不被误判为假，继续沿 Plan B 推进！
```

---

## 3. 双信任度模型：`local_trust_level` vs. `effective_trust_level`

为了支持“探索期先用 `L4a` Oracle 占位把顶层主定理局部证通，后续再逐一消债”的真实研究节奏，每个 `Claim` 节点同时维护两个信任字段：

1. **`local_trust_level`**：该命题**自身**证明达到的信任等级（`L0 | L1 | L2a | L2b | L3 | L4a | L4b | L5`）；
2. **`effective_trust_level`**：沿当前最佳活跃 `ProofPlan` 递归聚合的**端到端真实信任等级**：
   * 对一条 `ProofPlan` $p$：
     $$\text{plan\_effective}(p) = \min\big(\text{local}(u),\; \min_{v \in \text{deps}(p)} \text{effective}(v)\big)$$
   * 对命题 $u$：取所有未失效 `ProofPlan` 中的最高有效等级；
   * 偏序关系：`L5 > L4b > L4a > L3 > L1 > L0`；若存在任意上游依赖为 `stale = true`，则 `effective_stale = true`。

**状态冒泡收益**：
* 主定理可以在 3 条算术引理仍为 `L4a` 时就完成局部 `L5` 形式化（此时 `local = L5, effective = L4a`，复盘简报和生命体征清晰列出 3 笔 Oracle 债）；
* 当 Prover / Tool Subagent 陆续将 3 条 `L4a` 消债升级为 `L5` 时，主定理的 `effective_trust_level` 自动冒泡升级为**端到端 `L5`**，无需重证主定理！

---

## 4. 核心状态流转规则

### 4.1 正常升格流转（Promotion & Debt Discharge）
```text
L0 (Conjecture)
  ├─▶ L1 (Probed)                    [数值/有限测试未见反例]
  ├─▶ L2a (Oracle-Refuted)           [Oracle 经交叉印证给出疑似反例 → 仅软剪枝/挂起]
  │     └─▶ L2b (Kernel-Refuted)     [WITNESS_VERIFY 主引擎核验通过 → 入 GRAVEYARD]
  ├─▶ L3 (Formal-Sketch)             [主引擎编译通过(含sorry) + 盲审语义对齐 + 非空探针]
  │     ├─▶ L4a (Oracle-Backed)      [SMT/ATP/CAS 判真但未重建 → 显式挂 oracle_debt]
  │     │     └─▶ L5 (Kernel-Verified) [Hammer重建 / 反射 / Prover 消债 + Receipt]
  │     ├─▶ L4b (Cross-ITP-Verified) [默认 FORBIDDEN；仅人类 ADR 解锁 + 盲审兼容审计]
  │     │     └─▶ L5 (Kernel-Verified) [主引擎独立重证 + Receipt]
  │     └─▶ L5 (Kernel-Verified)     [Broker 洁净室签发 VerificationReceipt]
```

### 4.2 上游定义变更与「机会主义快速洗白」（Taint Propagation & Fast Re-clean）
系统将**声明级逻辑依赖（Claim Graph）**与**文件/模块级编译依赖（Module Graph / `.olean` 缓存）**联动处理：

1. **分类判定**：
   * **叶证明更新（Proof-body-only Promotion / Golfing）**：未改动定义、签名、`@[simp]` 或 `instance` → **零下游标脏**，Fast Lane 热池零重启，仅触发父节点 `effective_trust_level` 重新聚合；
   * **结构性变更（Definition / Signature / Global Attribute / Instance 变更，触发 T3）**：沿 Claim Graph 的未失效路线向下游 BFS 传播，并合并物理模块的影响范围，将受影响的传递依赖节点置为 `stale = true`，同时保留 `prior_trust_level` 与 `taint_root_commit`。
2. **拓扑序机会主义快速洗白（Opportunistic Clean Re-check）**：
   * Heavy Lane 先按拓扑序重建受影响模块切片的新 Golden `.olean`/`.agdai`/`.vo` 快照；
   * 对标脏前本就为 `L5` 的节点，**绝不立刻派 LLM 重写**——先由 Broker 拿旧证明代码在新 Golden 快照上执行一次零 Token 的洁净室复检：
     * 复检通过（签发新 `VerificationReceipt`）→ 清除 `stale`，保留 `L5`。带已审 Lean 规格的 Claim 还必须用此候选对原规格重新做内核定义等价比较，将比较证据和恢复信任原子入账；单独 T3 回执不能清除标脏。
     * 可归因于目标的确定复检失败（无已审规格绑定）→ 降级为 `L3`，提取精确编译器错误摘要；重排 Prover / Infra 队列属于调度层。若证据不可用，或已审规格的原字节、来源或定义等价无法确认，则保留 `stale`，不把缺失证据当成反例。

### 4.3 硬证伪传播（`L2b` Refutation Propagation）
当某节点 $v$ 经主引擎确认升格为 `L2b`：
1. 永久归档反例 witness 至本地 Wiki `GRAVEYARD`；
2. 直接依赖 $v$ 的 `ProofPlan` 置为 `INVALIDATED_BY_REFUTATION`。若某个上游命题仅仅是路线耗尽（没有被内核证伪），依赖它的方案显示为派生状态 `BLOCKED_BY_REFUTATION`；新增可行替代路线可解除这个阻塞，不改写直接证伪记录或历史告警。
3. 检查每个受影响父 `Claim` $u$：
   * 若 $u$ 仍有其他活跃 `ProofPlan` → 记入普通事件，不中断无人值守；
   * 若 $v$ 本身是主关键路径顶层猜想，或导致顶层猜想的**全部已知方案均失效** → 立刻触发 P0 Push Alert（`CRITICAL_CLAIM_REFUTED`）并写入 `DecisionInbox`。

### 4.4 人类转向（ADR Pivot）的原子语义
当人类确认 `PivotActionPlan`：
1. 优雅停止废弃 `ProofPlan` 的在跑子任务（停前执行 `L5` 独立引理回收进入 Wiki `SEDIMENT`）；
2. 将被放弃的 `ProofPlan` 标为 `ARCHIVED_BY_ADR`（永不硬删除）；
3. 若涉及定义修订，触发第 4.2 节的 T3 标脏传播；
4. 落盘 ADR、下发新 `InfraRFC`、推进 `human_checkpoint_cursor`。

---

## 5. 单文件持久化状态底座（`.runtime/state.sqlite`）

为保证单机零外部运维成本、事务原子性与可追溯性，系统将全部运行时结构化状态统一存入单文件 SQLite（完整可执行 DDL 见 [../schemas/state-store.sql](../schemas/state-store.sql)），开启 `WAL` 模式支持多读单写：

| 核心表 | 职责 |
| :--- | :--- |
| `events`（Append-Only） | 全局单调递增 `event_seq` 不可变事件流——是**复盘简报增量 Fact Pack** 与 **事后重放审计** 的唯一时间轴 |
| `claims` & `proof_plans` & `plan_dependencies` | AND-OR Claim Graph 逻辑结构、`local/effective_trust_level`、`stale` 与 `oracle_debt` |
| `module_dependencies` & `golden_snapshots` | 物理模块依赖与 Broker 认证的 Golden `.olean`/`.agdai`/`.vo` 快照元数据（含 `pinned` 保活标记） |
| `verification_receipts` | Broker 洁净室签发的不可伪造形式化准入凭证存根 |
| `circuit_breakers` | 可恢复 `Definition Smell` 局部熔断器状态与冻结队列快照 |
| `decision_inbox` & `human_checkpoints` | 非阻塞决策收件箱与人类检查点游标（`cursor_id`, `event_seq`, `git_commit`） |

当前已提供 [单 Claim 历史事件重放](architecture-acceptance.md#L458)和 [单 ProofPlan 历史事件重放](architecture-acceptance.md#L466)；也支持 [同一事件截点的 AND-OR 整图重放](architecture-acceptance.md#L474) 与拓扑校验。这些视图不依赖当前的图表数据，也不等同于从事件恢复整库。 另有 [当前图投影对账](architecture-acceptance.md#L482)，在同一 SQLite 读事务内核对账本与物化行，不修复数据。 [账本头 AND-OR 语义检查](architecture-acceptance.md#L490) 另行重算图的派生状态，不将账本/物化表相符等同于推导正确。
