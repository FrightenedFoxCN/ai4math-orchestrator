# ADR-0004：Local Verification Broker 沙箱隔离、静态禁入与洁净室形式化准入决策

- **状态**：已确认（Accepted）
- **日期**：2026-11-24
- **关联文档**：[06-verification-broker.md](../06-verification-broker.md)

## 决策清单

### D16. 四层纵深防御 + Broker 签发 `VerificationReceipt` 为唯一合并凭证
- **决定**：将所有子代理产出视为不可信输入，强制依次通过 **静态禁入过滤器 → OS 级沙箱 → 独立洁净室裁判包装器 → 加锁 Affected-Cone 回归**；主干合并逻辑只认 Broker 签发的哈希绑定 `VerificationReceipt`，不采信子代理自报状态。
- **理由**：防止编译期 RCE、篡改本地状态以及“改弱签名/偷渡公理/走私不安全 pragma”等形式化健全性漏洞。

### D17. `L5` 严格禁入 Lean 4 `native_decide`（`Lean.ofReduceBool` / `Lean.trustCompiler`）
- **决定**：Lean 4 主引擎的 `L5` 公理白名单严格限定为 `{propext, Classical.choice, Quot.sound}`，明确禁止 `sorryAx`、`Lean.ofReduceBool` 与 `Lean.trustCompiler`；有限计算优先使用内核 `decide`、`bv_decide`（LRAT 内核校验）或反射证明；确需使用 `native_decide` 的命题必须降级记为 `L4a` Oracle 债务。
- **理由**：`native_decide` 将可信计算基从内核扩大到整个编译器与 C 运行时，且在编译期执行原生代码，不符合 `L5` 内核保证标准。

### D18. 单机采用分层混合 OS 沙箱（兼顾 macOS APFS CoW 与强隔离）
- **决定**：Fast Lane 热池与常规 CoW Worktree 构建采用本机轻量 OS 沙箱（macOS Seatbelt `sandbox-exec` + APFS `cp -c` / Linux `bubblewrap` + `cgroups v2`），强制 100% 断网、主仓与敏感目录只读隔离及独立进程组整树回收；高风险外部 Oracle 脚本与 T1 洁净室终审启用最严格只读隔离或可选无网容器。
- **理由**：在单机多核（尤其是 macOS）环境下，兼顾 0.2s 级交互延迟、原生 APFS 写时复制缓存隔离与强安全边界。

### D19. T2 周期回归与最终交付挂载第三方独立外部内核校验器
- **决定**：T1 日常合入采用洁净室 Comparator / `--safe` / `Print Assumptions` 裁判包装器保持敏捷；T2 周期回归与最终里程碑交付追加运行独立外部内核校验器（如 Lean 4 的 `lean4checker` / `nanoda`）对导出环境做二次验证。
- **理由**：以极低的周期成本进一步压缩 TCB，防范证明助手自身 elaborator 或缓存状态导致的极端误判。
