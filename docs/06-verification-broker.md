# 06. 单机本地验证调度器、沙箱与形式化准入保证（Local Verification Broker）

## 1. 威胁模型与设计铁律

所有子代理提交的代码片段、补丁与外部求解器脚本均视为 **Untrusted Input（不可信输入）**。Local Verification Broker 必须同时防御三类威胁：

1. **宿主机编译期代码执行（Compile-Time RCE）**：
   * Lean 4 的 `elab` / `macro` / `run_cmd` / `#eval` / `@[extern]` / `IO.Process.spawn` / `native_decide` / `lakefile.lean`；
   * Agda 的 `--allow-exec`（`execTC`） / `{-# FOREIGN ... #-}` / `{-# COMPILE ... #-}`；
   * Rocq 的 `Declare ML Module` / `Redirect` / `Extraction`；
   * Python / CAS / Mathematica 脚本的文件与网络访问。
2. **形式化健全性破坏（Proof Smuggling / Reward Hacking）**：
   * 偷渡 `sorry` / `admit` / `postulate` / `Admitted` / 未声明公理；
   * Lean 4 利用 `native_decide` 引入 `Lean.ofReduceBool` / `Lean.trustCompiler` 绕过小内核；
   * Agda 漏加 `--safe` 或混入 `TERMINATING` / `NO_POSITIVITY_CHECK` / `--type-in-type` / `--rewriting` / `primTrustMe` 或冲突的 `--cubical`/`--with-K`；
   * 修改目标定理签名、隐式参数、局部 notation 或命名空间遮蔽“狸猫换太子”。
3. **单机资源耗尽与缓存投毒（Single-Host DoS）**：
   * `whnf`/类型推导内存炸弹导致系统 swap 雪崩；Hammer/ATP 超时遗留孤儿进程；并发写坏 `.olean` / `.agdai` / `.vo` 缓存。

> **核心铁律**：Chief Orchestrator 与主干合并逻辑**绝不采信**任何子代理自己在 `ResultContract` 中声称的“已通过”，**唯一准入凭证是由 Broker 洁净室签发的不可伪造 `VerificationReceipt`**。

---

## 2. 四层纵深防御流水线

```text
[Untrusted Patch / Snippet from Subagent]
        │
        ▼ Layer 1: 静态禁入过滤器（Static Pre-Flight Denylist，编译前硬拦截）
        │  - 路径与 Diff 范围锁定（Prover 仅可改冻结前缀之后的 proof body）
        │  - 构建配置文件全员只读（lakefile.* / *.agda-lib / _CoqProject）
        │  - 分引擎危险语法与不安全 pragma 硬拒
        ▼
        │ Layer 2: OS 级分层混合沙箱（Zero-Network & Minimal FS Surface）
        │  - macOS Seatbelt (sandbox-exec) + APFS CoW / Linux bubblewrap + cgroups v2
        │  - 100% 断网；主仓、Trust Ledger、ADR、.git/hooks、$HOME 不可写/不可见
        │  - 独立进程组（PGID）+ RAM/CPU/PID/超时天花板，超时整树 SIGKILL
        ▼
        │ Layer 3: 独立洁净室形式化准入（Clean-Room Kernel Admission Gate）
        │  - 绝不拿 Fast Lane 热池缓存结果直接合入 main
        │  - Broker 生成受信任裁判包装器，拼接冻结规格与候选实现独立编译校验
        │  - 类型定义等价 + 公理白名单 + 零逃逸 pragma 机审 → 签发 VerificationReceipt
        ▼
        │ Layer 4: 加锁串行提升与多级回归（T1 Promotion + T2/T3 Regression）
        │  - 主干互斥锁 → 受影响下游切片（Affected Cone）增量编译 + Heartbeat 预算校验
        │  - T2 周期 / 终审里程碑追加第三方独立外部内核校验（如 lean4checker / nanoda）
```

---

## 3. Layer 1：静态禁入清单（Static Pre-Flight Gate）

在启动任何编译器进程之前，Broker 先执行确定性词法/AST 扫描与 Diff 边界审计：

### 3.1 路径与写范围锁死
* **Prover Worker（`submit_proof_body`）**：仅允许修改目标声明冻结签名之后的 proof body；任何越界改动（含文件头 `import`、上游定义、属性声明）直接拒绝。
* **Infra Builder**：写入范围严格锁定在其获批 `InfraRFC` 授权的模块路径白名单内。
* **全局不可变保护文件**：`lakefile.lean`、`lakefile.toml`、`lean-toolchain`、`*.agda-lib`、`_CoqProject`、`Makefile`、`.git/**`、`.runtime/**`、`docs/adr/**` 对所有执行类子代理一律禁止修改。

### 3.2 分引擎硬禁入清单

| 引擎 | 默认硬禁入构造（直接拦截） | 受控例外 |
| :--- | :--- | :--- |
| **Lean 4** | `unsafe`, `@[implemented_by]`, `@[extern]`, `run_cmd`, `#eval`, `native_decide`, `sorry`, `admit`, `axiom`, `IO.*`, `System.*`, `--load-dynlib`, 以及放宽安全/超时限制的 `set_option` | 仅当 `InfraRFC` 显式授权语法糖扩展且经盲审 Reviewer 审签时，允许 Infra Builder 编写无 `IO` / 无 `unsafe` 的纯 `macro`/`notation` |
| **Agda** | 所有削弱安全性的内联 `OPTIONS`（`--allow-exec`, `--type-in-type`, `--rewriting`, `--allow-unsolved-metas`, `--guardedness` 逃逸）、`{-# TERMINATING #-}`, `{-# NON_TERMINATING #-}`, `{-# NO_POSITIVITY_CHECK #-}`, `{-# INJECTIVE #-}`, `{-# FOREIGN ... #-}`, `{-# COMPILE ... #-}`, `postulate`, `primTrustMe` | Broker 通过 CLI 强制注入 `--safe` 及项目锁定的逻辑选项（`--without-K` 或 `--cubical`），忽略或拒绝冲突的内联 pragma |
| **Rocq** | `Declare ML Module`, `Redirect`, `Extraction`, `Load`, `Admitted`, `Axiom`, `Parameter`, `Conjecture`, 以及 `-type-in-type` / `-bypass-qed` 等不安全编译标志 | 仅在人类 ADR 特批登记 `L4a`/`L4b` 债务时，由 Broker 在受控集中式 `Bridge` 模块声明，Prover 无权自建公理 |

---

## 4. Layer 2：OS 级分层混合沙箱（兼顾 macOS 与 Linux）

采用 **分层混合沙箱策略（Tiered Hybrid Sandbox）**：

* **Fast Lane & 常规 CoW Worktree 构建（低延迟原生沙箱）**：
  * **macOS**：使用原生 Seatbelt（`sandbox-exec` 动态生成最小权限 profile）——强制禁用全部网络（`(deny network*)`）、禁止读写用户 `$HOME` 敏感目录与主仓 `.git`/`.runtime/ledger`、仅授权读写当前专属 `scratch` 或 APFS CoW worktree，上游只读挂载 Mathlib / Agda stdlib / opam；配合原生 APFS `cp -c` 克隆缓存，保持 ~0.2s 热检查延迟。
  * **Linux**：使用 `bubblewrap (bwrap --unshare-net --ro-bind ...)` + `cgroups v2` 强内存/CPU 限额。
* **高风险外部 Oracle（Python / CAS）与 T1 洁净室终审**：
  * 启用最严格只读沙箱 profile（或可选无网轻量容器），彻底隔离环境变量与宿主机文件系统；商业本地 CAS（如 Mathematica）若需宿主机调用，则通过受控白名单只读封装脚本执行，禁止 `Run` / `Import` / `Export` / 网络函数。
* **进程树全回收（Anti-Zombie / Anti-Fork-Bomb）**：
  * 每次调用在独立会话/进程组（`setsid` / `setpgid`）启动并设 `RLIMIT_NPROC` 与 `RLIMIT_FSIZE`；超时、超内存或被取消时，向整个 `-PGID` 发送 `SIGTERM` → 2 秒宽限 → `SIGKILL`，彻底清理 Lean / cvc5 / Vampire / Z3 子进程。

---

## 5. Layer 3：洁净室形式化准入保证（Clean-Room Kernel Gate）

Fast Lane 热池仅服务于探索期快速试错，**绝不可用于签发 `L5` 合并回执**。任何候选补丁合入 `main` 前（T1 Promotion），必须进入独立洁净室由 Broker 生成的**受信任裁判包装器**做形式化验收：

### 5.1 Lean 4 形式化准入
1. **前缀与规格分离校验（Comparator 范式）**：将来自主干 `HEAD` 的受信任冻结规格文件（`TrustedSpec.lean`）与候选证明分离加载，核验目标定理类型与冻结规格在内核下**定义等价（`defeq`）**；
2. **公理白名单强校验**：对导出环境递归执行公理收集（`Environment.collectAxioms`），`L5` 严格白名单仅允许：
   * `propext`
   * `Classical.choice`
   * `Quot.sound`
   * **硬禁入**：`sorryAx`、`Lean.ofReduceBool`、`Lean.trustCompiler`（即彻底禁绝 `native_decide` 进入 `L5`；有限计算须改用内核 `decide`、`bv_decide` LRAT 内核校验或反射证明；确需使用 `native_decide` 时须降级登记为 `L4a` Oracle 债务）。
3. **T2 与终审第三方独立内核复核**：在周期性 T2 回归与最终里程碑交付前，对导出的 `.olean` 环境追加运行独立外部内核校验器（如 `lean4checker` 或 `nanoda`），将可信计算基（TCB）压至最小。

### 5.2 Agda 形式化准入（Broker 构造 `--safe` 裁判包装器）
Broker 在洁净目录自动生成只读裁判模块 `VerifyTarget.agda`：
```agda
open import TrustedSpec using (TargetType)
import CandidateModule as Cand

check : TargetType
check = Cand.targetProof
```
随后以命令行强制加锁 `agda --safe`（外加项目锁定的 `--without-K` 或 `--cubical`）从头编译。编译通过即由 Agda 内核从形式上保证：
* 候选证明与受信任目标类型严格一致；
* 整条传递依赖链全部满足 `--safe`（零 `postulate`、严格终止性、严格正性、零外部执行与不安全重写）。

### 5.3 Rocq 形式化准入（Broker 构造 `CheckTarget.v` 裁判包装器）
Broker 生成只读验证脚本：
```coq
Require Import TrustedSpec.
Require CandidateModule.
Definition check : TrustedSpec.TargetType := CandidateModule.target_proof.
Print Assumptions check.
```
使用干净 `coqc` 编译并确定性解析 `Print Assumptions check.` 输出，强制确认其为 `Closed under the global context` 或严格落入项目白名单公理集（零 `Admitted`）。

### 5.4 不可伪造的 `VerificationReceipt`
通过 Layer 1–3 后，Broker 签发结构化 `VerificationReceipt`（Schema 见 [../schemas/verification-broker.schema.json](../schemas/verification-broker.schema.json)），包含：
* `base_commit` 与候选补丁的 `patch_sha256`；
* 洁净室命令、引擎版本与工具链哈希；
* 完整 `axioms_used` 清单与 `allowlist_passed = true`；
* `compile_seconds`、`max_heartbeats_observed` 与 `external_checker_passed`（T2/交付阶段）；
* 若缺少有效且哈希匹配的 `VerificationReceipt`，Promotion 门禁直接拒绝合并。

---

## 6. Layer 4：单机多核资源治理、CoW 缓存与四级回归

### 6.1 双车道优先级调度
* **Fast Lane（高并发、短租约）**：
  * 常驻预热主引擎 REPL 池（2–4 worker，预加载核心上游库，LRU 复用）；
  * 有界 `tool_automation` 槽位（SMT / ATP / Hammer / CAS，独立配额）；
  * 单次调用硬超时（tactic/SMT 2–15s，Hammer 15–60s）。
* **Heavy Lane（严格限流，并发度 1–2）**：
  * 优先级队列：`P0: T1 Promotion` > `P1: T3 标脏重检` > `P2: Infra Builder 烟雾测试` > `P3: T2 全仓周期回归`。

### 6.2 内存水位线准入控制（防单机 Swap 雪崩）
Broker 每 2 秒采样宿主机可用内存，执行三级背压（并实时输出到常驻生命体征条）：
* **`< 70%`（GREEN）**：正常受理 Fast 与 Heavy 租约；
* **`70%–85%`（YELLOW）**：暂停受理新 Heavy 与重型 Hammer 租约；按 LRU 回收闲置辅助引擎与多余 REPL worker；
* **`> 85%`（RED）**：拒绝新探索期租约；按优先级从低到高优雅中止最低优先级探索子任务（回收局部引理后退出），全力保障主干 T1 回归不 OOM；连续触发 RED 超过阈值时向人类推送 P0 `BROKER_REGRESSION_MELTDOWN` Alert。

### 6.3 安全的 `.olean` / `.agdai` / `.vo` 黄金快照缓存复用与 CoW 隔离
为同时解决**“不可信缓存投毒”**、**“`.trace` 失配触发全量重编”**与**“叶证明修改引发虚假下游失效”**三大痛点，采用三级缓存复用体系：

1. **全局只读上游大库（`upstream-ro/`）**：Mathlib 预编译 `.olean`、Agda stdlib、Rocq opam switch 跨所有任务只读共享；
2. **提交绑定的黄金快照池（`.runtime/artifact-cache/golden/<commit_sha>/`）**：
   * **防投毒铁律**：**只有通过 Broker 洁净室验收并合入 `main` 的 commit**，其完整构建产物（含 `.olean` + `.ilean` + `.trace` / `.agdai` / `.vo`）才允许发布进黄金快照池；任何不可信子代理 worktree 的临时产物用完即弃，**绝不反哺共享缓存**；
   * **CoW 瞬时克隆**：新开 `InfraRFC` worktree 或 T1 洁净室时，从 `golden/<base_commit>/` 通过文件系统写时复制（macOS APFS `cp -c` / Linux Btrfs/XFS `cp --reflink=auto`）瞬时克隆完整缓存及 `.trace`——零共享写锁冲突、零误判全量重编、近乎零额外磁盘；
   * **保活与 GC（`pinned`）**：自动保活最近 `N` 个 `main` commit、所有处于 `TRIPPED` 熔断挂起状态的 `base_commit`、以及人类 ADR 里程碑 commit（确保熔断恢复或策略回退时零冷启动）；其余陈旧快照按 LRU 回收。
3. **物理模块分层减少重编半径（`Defs/Spec` 与 `Leaf Proofs` 分离）**：
   * 规范要求 `Infra Builder` 将核心定义、冻结签名与烟雾引理放在 `Module/Defs.*`，将可并行消解的常规证明放在叶模块 `Module/Proofs/*`（不被兄弟分支 `import`）；
   * **直接收益**：叶证明合入 `main` 时下游兄弟模块 `.olean` **零失效**；Fast Lane 热池 REPL 只预加载 `upstream-ro + CORE Defs 层`，叶证明合入时热池**零重启**，仅在 `Defs` 层或全局 `@[simp]`/`instance` 变更时执行滚动重载（Rolling Reload）。

### 6.4 T0–T3 回归闭环
* **T0（秒级）**：Fast Lane 交互试错与 SMT 探针；
* **T1（加锁串行 Promotion）**：Layer 1 静态禁入 → Layer 3 洁净室签发 Receipt → 加主干互斥锁 → 受影响下游切片（Affected Cone）增量编译 + `maxHeartbeats` 退化检查 → 原子快进 `main`（失败自动回滚）；
* **T2（周期后台）**：全仓增量构建 + 编译耗时/Heartbeat 基线回归 + 第三方外部内核校验（如 `lean4checker` / `nanoda`）+ `L4a`/`L4b` 债务扫描；
* **T3（定义/ADR 转向后）**：沿 Claim Graph 标脏受影响下游子树并按拓扑序重检。

现有 T2 切片可手动采集 Lean 冷构建样本并 [离线比较墙钟中位数](architecture-acceptance.md#L514)，也可 [只读扫描经校验的 L4 信任债](architecture-acceptance.md#L522)，并可在 [同代次托管 T2 审计](architecture-acceptance.md#L530) 中合并冷构建和债务结果；同代次审计另有 [固定审核提交的驻留限次采样](architecture-acceptance.md#L538)；可 [显式采样指定 Lean 模块的 heartbeat](architecture-acceptance.md#L610)，尚无自动基线或周期 heartbeat。独立内核现可 [复核明确固定的 Lean 导出](architecture-acceptance.md#L554)，也可 [从固定托管快照复核指定定理](architecture-acceptance.md#L562)，也可 [在一次构建中复核显式定理批次](architecture-acceptance.md#L570)，并提供 [固定 Git 清单的人工里程碑预检](architecture-acceptance.md#L578)，亦可 [按固定清单限次周期复核](architecture-acceptance.md#L586)，亦有 [强制预检的本地里程碑标签](architecture-acceptance.md#L594)，可 [只读检查其留存证据和代次](architecture-acceptance.md#L602)；尚未接到全仓声明覆盖或远端发布强制门禁。
