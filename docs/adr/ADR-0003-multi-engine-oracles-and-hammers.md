# ADR-0003：多引擎主内核可配置、禁用默认跨 ITP 桥接、动态 Hammer 调度与 Oracle 剪枝防线

- **状态**：已确认（Accepted）
- **日期**：2026-11-24
- **关联文档**：[05-multi-engine-and-hammers.md](../05-multi-engine-and-hammers.md)
- **修订说明**：本 ADR 修订并收紧 [ADR-0001](ADR-0001-core-workflow-decisions.md) 中的 **D4**。

## 决策清单

### D12. Trust Ledger 对称细化为 `L2a / L2b` 与 `L4a / L4b`
- **决定**：将原 `L2` 拆分为 `L2a (Oracle-Refuted)` 与 `L2b (Kernel-Refuted)`，将原 `L4` 拆分为 `L4a (Oracle-Backed)` 与 `L4b (Cross-ITP-Verified)`。
- **理由**：外部 SMT/ATP/CAS 结论在信任度上远低于证明助手内核证明，且可能存在编码失真或符号计算 bug；对称拆分后，系统既能灵活利用 Oracle 加速探索，又能严守内核可信边界。

### D13. 项目级主引擎可配置（Lean 4 / Agda / Rocq），默认禁用跨 ITP 公理桥接
- **决定**：
  1. 修订 ADR-0001 D4：不再把 Lean 4 写死为唯一默认主引擎，允许每个项目在初始化 ADR 中指定 `primary_engine`（`lean4 | agda | rocq`，默认 `lean4`；如构造性/类型论问题可直接选 `agda`）；
  2. 将跨证明助手内核公理桥接（`L4b`）的默认策略收紧为 **`FORBIDDEN`**；辅助 ITP 仅可做无公理洞的探索期原型参考（成果须在主引擎重证至 `L5`）；仅当主引擎面临极端困难并经人类显式 ADR 批准时，才可解锁 `EXCEPTIONAL_ADR_ONLY`。
- **理由**：跨依赖类型论桥接极易在古典公理、UIP/K、宇宙层级和编码一致性上引入难以察觉的语义漏洞。

### D14. Hammer 不写死为 Prover 前置步骤，由 Orchestrator 动态路由至 `tool_automation` 子代理
- **决定**：取消固定 “Hammer-First” 强制前置；新增专职 `tool_automation` 子代理承接 Hammer、SMT 重建、ATP 与 CAS 调用，由 Orchestrator 依据目标形态与单机 Broker 负载按需派发。
- **理由**：重型 Hammer 在部分目标上算力开销极大且命中率低，集中交给可预算控制的工具子代理按需调度更省单机资源。

### D15. 高风险 Oracle（如 Mathematica 符号积分 / SMT）剪枝前强制补充交叉判断
- **决定**：对所有用于剪枝的 Oracle 结果按风险分级（`HIGH / MEDIUM / LOW`）；高风险 CAS/SMT 结果在触发分支剪枝前必须执行**定义域/边界条件显式审计 + 独立双源交叉印证**（如高精度数值/区间采样、双求解器互验或 witness 精确代回），且 `L2a` 仅允许软剪枝（降权/挂起），须经主引擎核验升格为 `L2b` 才可永久写入 `GRAVEYARD` 或触发 P0 证伪推送。
- **理由**：防止符号积分支割线错误、隐式参数假设或 SMT 翻译失真误杀开放问题的正确进攻路线。
