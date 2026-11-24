# ADR-0005：AND-OR Claim Graph、双信任度聚合与黄金 `.olean` 缓存复用决策

- **状态**：已确认（Accepted）
- **日期**：2026-11-24
- **关联文档**：[07-claim-graph-state-machine.md](../07-claim-graph-state-machine.md)、[06-verification-broker.md](../06-verification-broker.md)

## 决策清单

### D20. 采用 AND-OR Claim Graph 与双信任度模型（`local` vs. `effective`）
- **决定**：将逻辑图建模为 `Claim`（OR 节点）与 `ProofPlan`（AND 节点）的二分图，并为每个 `Claim` 分别维护 `local_trust_level` 与递归聚合的 `effective_trust_level`。
- **理由**：防止单一中间引理被 `L2b` 证伪时误杀拥有替代方案的父猜想；同时允许探索期基于 `L4a` Oracle 占位先把主定理局部证到 `L5`，待下游消债后自动冒泡升级为端到端 `L5`。

### D21. T3 标脏传播后优先执行「机会主义快速洗白」
- **决定**：上游定义变动触发下游 `stale = true` 时，保留原 `prior_trust_level`；对原 `L5` 节点先在新 Golden 快照上执行零 LLM Token 的洁净室复检，通过则直接洗白恢复 `L5`，仅失败节点降级回 `L3` 重排 Prover。
- **理由**：大幅削减增量定义调整后的无谓 LLM 重证算力开销。

### D22. 黄金只读 `.olean`/`.agdai`/`.vo` 快照池 + `Defs/Proofs` 物理分层
- **决定**：
  1. 仅 Broker 洁净室验收合入 `main` 的 commit 产物（含完整 `.trace`）可发布进 commit-keyed 黄金快照池，经 APFS `cp -c` / Btrfs `--reflink` 供新 worktree 克隆，严禁不可信子代理产物反哺缓存；
  2. 强制保活熔断器挂起与 ADR 里程碑对应的 Golden 快照；
  3. 规范基建模块分离 `Defs/Spec` 与 `Leaf Proofs`，实现叶证明合入时下游 `.olean` 零失效、Fast Lane 热池零重启。
- **理由**：在严守沙箱零缓存投毒前提下，最大化单机多核构建复用率与回滚恢复速度。

### D23. 单文件 SQLite（WAL）承载 Append-Only Event Log 与全量运行时状态
- **决定**：将 Claim Graph、AND-OR ProofPlan、Golden 快照元数据、VerificationReceipt、熔断器、Decision Inbox、Human Checkpoint Cursor 与单调递增 `events` 流统一落盘至 `.runtime/state.sqlite`。
- **理由**：单机零外部数据库运维负担，同时为常驻生命体征条与复盘简报 Fact Pack 提供严格原子、可增量 Diff 的确定性事实底座。
