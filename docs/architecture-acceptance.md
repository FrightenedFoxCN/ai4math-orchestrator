# 全架构验收矩阵

目标是按设计实现可无人值守的数学探索与形式化验证闭环。Phase 1–3 的最小实现通过本机测试，但不等于完整能力已交付。下面各行需分别提供实现、回归和运行证据。状态从「待补齐」推进到「已验收」，不因相邻行通过自动完成。

| 批次 | 工程范围 | 完成所需证据 | 当前状态 |
| --- | --- | --- | --- |
| A：可信合入 | 主引擎单一授权；冻结规格、洁净室和不可由调用者构造的证据；Git 到 SQLite/HUD 的恢复一致性 | 真实 Lean 合入、同一回执仅一次入账、CAS 失败不入账、Git 已推进后的跨进程重启恢复、状态/推流可见 | 已有托管合入与恢复实现，验收继续 |
| B：研究状态 | AND-OR 路线耗尽传播，T3 标脏与旧证明实际重验，事件日志与可重建视图，Wiki 状态/血缘/考古 | 多跳反例、替代路线、归档路线、状态重放；真实定义变更后的复检；证伪条目只作为警告与证人证据 | 图传播、旧库迁移、操作者 Git T3 复检（含已审 Lean 规格重比）已验证；已批准 T3 有可重启检查点和显式单/多计划 Lean 驻留续跑；Wiki 支持受管冷层快照、唤醒重验、非授权观察留存、审批派发和经 T1 验证的替代血缘、操作者 CORE 投影及封存 T3 后恢复、受门禁的 Wiki RPC/HUD 源码接线；已有显式基线的一次性及可选常驻结构发现；已有基于指定 Lean Claim 的精确 T1/封存 T3 基线选择及可选常驻调用；已有单 Claim、单 ProofPlan 和 AND-OR 整图在统一截点的历史重放与拓扑校验；已有当前图投影同快照只读对账及账本头 AND-OR 语义重算；已有显式 opt-in 的已应用计划跨快进提交驻留调度；其余账本重放及 Wiki 生命周期待补齐 |
| C：验证底座 | Lean/Agda/Rocq 主引擎门禁，比较器与独立 T2，可信反例核验，Hammer/Oracle 债务，缓存和资源隔离 | 每个受支持引擎正负样例；Oracle 与内核结论不混用；配额、超时、跨进程租约、高水位下回归行为 | Lean/Agda/Rocq 受限 T1 和 Lean 单独规格比较已通过原生验收；双 SMT 有界整数探针可留存建议并人工限时挂起探索任务；Lean 有固定策略的有界本机前提重建，托管 CLI 和 daemon T0 已有项目级跨进程门禁；已有 T2 的手动 Lean 快照冷构建、离线中位数比较及经双校验的账本头信任债清单，已能合并报告同代次的冷构建和债务，并可在固定审核主干上显式驻留限次采样和查看脱敏 HUD；已有双摘要固定的独立 Nanoda 导出校验和托管快照指定定理的单项及有界批量复核；已有人工固定 Git 清单和零 L4 预算的里程碑预检；已有显式固定清单、限次运行的周期独立复核；已支持强制预检的本地里程碑标签及留存证据只读检查；已有固定快照的单模块手动 heartbeat 采样、离线中位数比较及固定祖先/主干的同宿主成对复采；固定基线的驻留 heartbeat 已接入脱敏 HUD，也可显式从已核对的本地里程碑标签复采；自动全仓覆盖、远端发布端强制接线、自动滚动基线、其他引擎规格比较、外部 Hammer、Oracle 债务与主机级资源配额待补齐 |
| D：编排与权限 | 探索/基建双轨、RFC/冻结/烟雾验证、任务预算/能力、盲审上下文、DAG 调度与宿主工具隔离 | 从探索派发到合入；多个任务公平运行；任务跨界、过期、缺预算皆拒绝；直接 UDS/SQLite 不在 worker 信任面 | 双轨 DAG、租约与受限本地 worker 派发已验证；有任务专用 Agent 工具且离线实装循环已桥接持久租约、写回未验证候选；在线模型、RFC、多会话隔离和自动调度待补齐 |
| E：人机共驾 | 确定性简报与告警可读、P0 四类生产信号、局部熔断恢复、Pivot Dry-Run/ADR/检查点 | 四类 P0 真正来源、去重/重连；风险与历史可见；人类确认前无副作用、确认后重启可恢复 | 操作者 `RESUME_AS_IS` 与有界 Lean 探针已接通；已验证关键内核反例、带 AND/OR 路线判定的操作者关键熔断和双轨停摆来源；已验证有界的 Lean 下游回归异常来源；自动 Definition Smell、热池持续驱逐、其他 Pivot 路径及实际 GUI 确认待补齐 |
| F：实际部署 | 官方 Typert/DSH 宿主集成、Web/Electron GUI、角色工具可见性与项目生命周期 | 真机宿主配置、角色变更与重启、认证、资源/布局冲突；记载平台约束与不适用项 | 已通过独立进程的实装 DSH SDK/Typert 告警推流、留存读取及固定审核主干 T2 采样传输；实际 GUI 挂载、认证与生产 preset 生命周期仍待补齐 |

明确区分「实现且单测通过」「本机集成验证」「真实宿主和生产工作流验收」。部署到用户现用 DSH profile 不属于自动默认操作。

## 批次 A：托管合入操作

在受信任操作者的宿主终端执行。先 `ai4math init ROOT lean4`，再用 `ai4math declare-claim ROOT CLAIM MODULE TARGET TITLE STATEMENT` 绑定由操作者冻结的目标声明。随后使用 `ai4math promote-managed ROOT BARE_REPO CLAIM MODULE TARGET NATIVE_ENGINE PROOF_BODY_FILE [AGDA_DATA]`，不要把这些操作暴露给普通 worker。旧的 `promote` 是独立 CLI 核心验收路径，不记状态账，不能替代托管合入。

托管路径验证主引擎、证明与全项目回归；先持久化待处理意图，再使用 Git CAS 推进 `main`。SQLite 的回执、Claim、派生状态、事件与意图清除在同一事务中提交。中断后运行 `ai4math recover-promotion ROOT BARE_REPO NATIVE_ENGINE [AGDA_DATA]`：重新从原快照验证证明，检查准备好的 Git 树和父提交；若主干发生非预期分叉则停止，不强制推送。请把 bare repo 和项目运行目录留在受信任宿主，只允许托管协调者写主干；不对其他人为强制改写 Git 提供账本一致性保证。

真实 Lean 测试已验证 Git 已推进但 SQLite 尚未入账时可恢复，重复恢复不重复入账，简报投影可读到正确提交上的里程碑。不等于已测试进程被强杀或主机断电，也不等于实际 GUI 验收。缓存发布不是入账门禁，报告会单独标识缓存状态。

## 批次 B：状态闭包与 T3 接入

多跳路线耗尽和直接内核证伪在状态上分离；替代路线可解除间接阻塞，旧 P0 与历史 Inbox 不删除。旧库迁移补齐路线投影，已发的同源 P0 不重复。T3 逻辑遍历排除失效和归档路线，物理模块闭包独立合并。预览读取事件版本与同一套影响范围，不改写信任状态。

新托管合入同时保留面向操作者的重验材料（原 Git 基点、合入点、目标、证明体和源码摘要），不经 daemon 或浏览器传输，不将材料本身当作内核证据。操作者协调器从 Git 重建并核对原始已合入字节，再核对被审快照含有完全相同的目标源码。没有资料的旧合入不能自动补记可信证明。

T3 操作（当前适用于受信任的 bare repo、根级模块和严格模块顺序；ANCHOR_MODULE 必须同时存在于两个快照）：

1. `ai4math t3-preview ROOT BARE_REPO ANCHOR_MODULE OLD_COMMIT REVIEW_COMMIT`。使用完整 Git OID，要求同仓祖先→后代，输出变更文件、保守的全项目影响范围与摘要。
2. 确认项目主干已由受信任操作者推进到审核提交后，运行 `ai4math t3-apply ROOT BARE_REPO ANCHOR_MODULE OLD_COMMIT REVIEW_COMMIT PLAN_SHA256`。摘要或账本版本过时会拒绝；授权与标脏同事务。此命令不发布 Git，不自动批准定义变更。
3. `ai4math t3-recheck ROOT BARE_REPO PLAN_SHA256 NATIVE_ENGINE [AGDA_DATA]`。主干变化则停；内核通过清除旧证明 stale，明确且可归因的 Lean 类型/定义等价失败转 L3。依赖、策略或环境不可用，以及目标本身源码改动，仍保持 stale；可重试，不会重复执行已处理的结论。

原生验收覆盖 Git 托管合入、GC 后 CAS 冲突恢复、实际 Lean 定义变化时的通过与失败、主干改写拒绝及幂等。尚未实现 T3 自动触发、自动修补任务派发、Golden 重建复用，也没有让 daemon 在任意外部 Git 改写后立刻冻结旧 HUD 读数；需要统一结构变更发布协议。

Wiki 的来源边浏览和反例提示已通过 daemon、实际安装的 DSH SDK、角色授权测试与浏览器测试夹具。它们仅提供索引中的证据，不自动允许、否决或重新入库。最近验收：macOS Lean/Agda 含原生项的 Rust 全工作区 68 项，Host/Client 18 项；未安装现用 GUI 插件。

## 批次 C：可信 Lean 反例

登记可证伪命题为已提交、公开、闭合的 Lean `def target : Prop := ...`。操作者用 `declare-claim ROOT CLAIM MODULE PROPOSITION TITLE STATEMENT [--critical]` 注册，再用 `bind-witness-spec ROOT BARE_REPO CLAIM MODULE PROPOSITION APPROVED_COMMIT` 同时锁定仓库、主干和完整提交；旧 Claim 不允许悄然改绑到同名的新命题。然后运行 `refute-managed ROOT BARE_REPO CLAIM MODULE PROPOSITION NATIVE_LEAN PROOF_BODY_FILE`，仅提交求证否定命题的证明体。这个保守版本要求当前主干仍等于批准的快照；其他提交（即使未改该文件）需作为新 Claim 经人工确认，暂不做依赖闭包等价判定。

Broker 在独立 Seatbelt 中构造内核否定类型并审计公理；成功后先持久化可回放的 Graveyard Markdown，再原子记录回执、L2b、依赖路线变化和关键 P0。磁盘失败不升级，不产生告警；同一见证重试不重复记账。Wiki 可通过 `wiki-rebuild` 再检索。当前未自动提交 Wiki Git 归档，也未实现 Agda/Rocq 见证、自动派发，或从 Oracle 输出自动转交内核见证。Broker 内部 Heavy Lane 有单进程 FIFO（最多 8 个等待、单次排队上限 30 秒）；后文接入的项目级跨进程门禁不排队，主机总体内存约束仍待接入。

截至本批，实际 macOS Lean/Agda 主机原生测试共 74 项，Host/Client 仍为 18 项。

## 批次 E：操作者批准 `RESUME_AS_IS`

1. `ai4math trip-breaker ROOT BARE_REPO BREAKER MODULE DEFINITION REASON [--critical]` 登记已报告的局部熔断，产生对应的待决 Inbox；`--critical` 由操作者指定，生成 P0，并绑定因果事件与留存简报。这不会暂停 worker，也不会推断路线是否耗尽。
2. 从 daemon 的 `briefing.pull` 获取 `retained_id`，再执行 `ai4math pivot-preview ROOT BARE_REPO FACT_SHA256 BREAKER REASON`。预览不修改主干、决策与检查点；只有所选熔断和关联 Inbox 都显示在该简报中才纳入计划。选中条目之外若有所截断，预览与 ADR 会保留该提示；预览不是对所有历史条目的逐条确认。
3. 审阅精确 ADR 内容后，执行 `ai4math pivot-apply ROOT BARE_REPO FACT_SHA256 BREAKER REASON APPROVED_ADR_ID`。ID 绑定简报、理由、Git 主干、账本事件版本、上一检查点和选中熔断；任一改变需要重新复盘。只向托管数学仓库写入新增的 `docs/adr/ADR-PIVOT-<sha256>.md`，不变更数学源码。
4. 中断后执行 `ai4math pivot-recover ROOT BARE_REPO`。先用 Git ref 固定准备好的提交、再写入 SQLite journal，之后 CAS 更新主干；SQLite 同事务记录 ADR、将熔断改为 `HALF_OPEN`、解决关联 Inbox 并推进检查点。游标记录所审事件的上界，不吞掉 CAS 后、恢复前的新事件。过期且未被当前主干采纳的 journal 可用 `ai4math pivot-abandon ROOT BARE_REPO APPROVED_ADR_ID` 放弃；不得用强制改写 Git 恢复。主干若已移到不同提交（即使是 ADR 的后代），保守停止，保留 journal，等待操作者检查。其他主干写入者必须遵守项目锁与未决事务门禁。

验收证据：CLI 回归覆盖 Git GC 后找回准备好的 ADR、模拟 CAS 后未入账再恢复、重复执行不重复记账、主干或账本变化拒绝落地，以及不改动其他熔断。端到端测试以独立的真实 daemon + CLI 进程，经 UDS 验证 P0、留存简报、熔断通知与检查点，并确认 daemon 不暴露 Pivot 写 RPC：

```bash
cargo build --locked -p ai4math-cli
AI4MATH_TEST_CLI="$PWD/target/debug/ai4math" cargo test --locked -p ai4math-daemon --test workflow operator_pivot_observed_by_real_daemon -- --ignored
```

本轮通过 86 项 Rust 全工作区测试（含宿主原生 Lean/Agda 和上述真实 daemon 流程），以及 18 项 DSH 插件测试。

尚未实现 `PATCH_DEFINITION`、`ARCHIVE_AND_PIVOT`、自动生产熔断或 GUI owner 确认控件。当前不改变用户现用 DSH GUI。

## 批次 D/E：双轨租约与半开探针

- 操作者可用 `ai4math task-register ROOT BARE_REPO ID exploration|infrastructure MODULE TARGET [DEP_ID ...]` 登记任务，依赖必须已登记；用 `ai4math tasks-status ROOT` 查看计数。状态库以轨内 FIFO 和跨轨交替发放租约，上限为总计 6 个、单轨 4 个。租约过期可重领，旧代无法完成新代任务；完成仅是调度元数据，不授予证明或合入权限。daemon `status` 仅暴露汇总计数，不暴露租约或派发接口。
- 熔断登记原子作废同引擎、同模块的现有普通租约；`TRIPPED`、`HALF_OPEN`、`ARCHIVED` 都不发新租约。不跨模块推断物理依赖，不杀进程；自动派发与暂停队列转探针尚未接入。本地 worker 的独立能力边界见本页下一节。并列熔断必须全部关闭才使该模块普通任务重新具备资格，仍受依赖和容量限制。
- `ai4math breaker-probes-preview ROOT BARE_REPO BREAKER THEOREM [THEOREM]` 必须在 Pivot 后执行。检查当前 Git 主干包含已落地的 ADR，且目标模块及按清单顺序在它之前的源码与 ADR 快照逐字相同。审阅输出中的完整提交、源码摘要、定义和 1–2 个已提交的 Lean 定理名；预览不会运行 Lean。
- `ai4math breaker-probes-arm ROOT BARE_REPO BREAKER APPROVAL_ID THEOREM [THEOREM]` 复算计划并要求精确批准 ID。Git 或事件版本变动需重审；单纯登记并不解冻。`ai4math breaker-probes-run ROOT BARE_REPO BREAKER APPROVAL_ID NATIVE_LEAN` 使用受信任操作者安装的原生 Lean 和 Seatbelt。90 秒批次上限，留存对象每件不超过 16 MiB、累计不超过 64 MiB；不是硬性全机内存、磁盘或多进程配额。
- 每条探针必须是在已审核模块定义的公开定理，且类型直接引用精确的目标定义。独立的受信任 Lean 检查器检查公理并重放内核依赖闭包；只有全批通过的密封回执可关闭熔断。失败不计部分通过；环境不可用、Git 变动或中断不会释放普通任务。中断后以同一批准 ID 重跑完整批次（最多 8 次）；也可重新审阅，新的批准取代旧批次。已完成批次的重跑仅返回历史回执，不重复写账。不改变 Claim 的信任等级，也不推进 Pivot 已审事件游标。

最终验收：99 项 Rust 全工作区测试（含 macOS Seatbelt 中的 Lean/Agda）和 19 项 DSH Host/Client 测试通过。真实 daemon/CLI 回归覆盖探针状态经 UDS 推流、双轨阻塞汇总、缺失编译器失败不解冻，以及拒绝普通 RPC 写入。原生 Lean 联合回归 `native_half_open_gate_opens_only_after_exact_committed_lean_probes` 验证无关定理拒绝、双探针全部通过才关闭，以及两轨随后才可领取租约。



## 批次 D：受限本地 worker 的建议与可信合入

1. 用 `ai4math task-bind ROOT BARE_REPO ANCHOR_MODULE REVIEWED_COMMIT` 显式绑定首次使用的仓库（已经绑定的项目可跳过）。需要本操作者拥有的 `.runtime`（0700，不能是符号链接），SQLite 文件不可共享硬链接。新项目初始化即按此创建；旧项目请先检查路径所有权并收紧权限。该目录只是跨账号存储边界，不是同 UID 对 worker 的隔离边界。
2. 登记具有模块和正式声明的 Claim，再用 `task-register` 登记相同引擎、模块和目标声明的任务。执行 `ai4math task-run ROOT BARE_REPO ID CLAIM ROLE WORKER_EXE SHA256 WALL_SECONDS`：仅领取指定轨道的下一个合格任务，不跳过排在前面的任务。`explorer` 属于探索轨，只接收非形式化命题；`prover_worker` 接收受限的 Lean 冻结模板；`independent_reviewer` 仅接收非形式化命题和经过检查的单行 Lean 签名，不接收前文源码、Builder/Writer 历史、证明体或审签权限。暂不装配定义依赖，签名比对不能替代正式盲审。
3. 操作者选择绝对路径和 SHA-256 固定的原生 Mach-O。需兼容最小动态加载器配置，没有网络、外部服务或非系统运行时文件授权。进程经 Seatbelt 获得结构化 stdin/stdout；无 shell、数据库路径、会话历史、租约令牌或 daemon RPC。只接受 v1 角色对应的 JSON 建议：`{"kind":"findings","summary":"..."}`、`{"kind":"review","verdict":"insufficient_context","rationale":"..."}` 或 `{"kind":"proof_body","proof_body":"..."}`。不接受信任等级、内核回执、审签票据或其他字段。单次 1–60 秒监督超时、CPU/输出限额、1 MiB stdin/stdout；解析后的建议最多 32 KiB。stderr 不进入候选、事件或 HUD。
4. 建议入账时重新核对引擎、模块、目标、Git 版本、Claim 和租约。入账后可用 `ai4math task-result ROOT ID` 读取历史建议；读取和入账都不改变证明信任。显式失败可重试；协调者异常退出后租约在 TTL 到期恢复，但尚无独立守护进程回收仍在睡眠的孤儿 worker。熔断只废弃租约、拒绝后续回报，不向 worker 强制发信号。
5. 用户可在审阅候选后执行 `ai4math promote-task-result ROOT BARE_REPO ID NATIVE_LEAN`。只有 Prover 的 proof body 可进入此门；要求绑定仓库、未熔断模块和同一 Git 主干快照，并走既有洁净室原生 Lean、全项目回归及 Git/SQLite journal 路径。候选存档不是证明，非形式化建议不是审签。本地测试 worker 是原生协议夹具，不是模型服务或现用 DSH 代理。

验收：110 项 Rust 全工作区测试和 19 项 DSH Host/Client 测试通过。原生隔离测试覆盖文件读写、继承 FD、UDS、网络绑定、创建子进程及外部执行的拒绝、超时与输出边界；端到端覆盖双轨三种角色、无审签权限、运行中熔断拒绝留存，以及候选经操作员明确触发原生 Lean 才从 L0 升至 L5。尚无常驻自动派发、模型中转和工具调用配额、真实 DSH 角色安装、系统级总内存或独立进程回收验收。

## 批次 C：Rocq 冻结证明体与托管合入

Rocq 主引擎使用同一 Git/SQLite 回执流程，但不接受通用 Rocq 项目或任意 tactic 脚本。初始化 `ai4math init ROOT rocq`，提交根级 `.v` 文件；多模块用 `ai4math-modules.txt` 逐项列出，按导入顺序，不能与 Lean 或 Agda 混编。批准的文件使用严格命令子集：本地/受信任库导入、定义和受限书写的闭合定理。核心槽示例：

```coq
Theorem target : True.
Proof.
  exact ({{PROOF_BODY}}).
Qed.
```

用 `ai4math declare-claim ROOT CLAIM Goal.v target TITLE STATEMENT` 绑定目标，再执行 `ai4math promote-managed ROOT BARE_REPO CLAIM Goal.v target ROCQ_NATIVE PROOF_BODY_FILE ROCQ_DATA`。只填 proof term（示例为 `I`）。允许的规范形不包括 `Admitted`、自建公理、切换守卫/宇宙检查、任意扩展插件、外部工具或 tactic 注入；不会把解释器快检当成合入凭据。

ROCQ_DATA 是操作者指定的绝对安装目录（可用 `rocq c -where` 确认）。Homebrew Rocq 9.3.0 的入口应为原生目标，不用 shell shim；Broker 实际调用并记录同一安装的编译器，额外动态库只授予明确的实际文件。暂不承诺任意 opam switch 兼容。T1 按精确冻结源检查内核，再在单独生成的模块导入编译产物并对目标执行 `Print Assumptions`，仅接受闭合的全局上下文。随后重建同一 Git 快照的完整模块清单，并按既有 Git CAS、SQLite journal 合入；可按普通恢复命令带相同 Rocq 数据目录恢复。此路径没有独立重建手写规格的 Comparator 或第二方 T2 内核验收，也不支持跨引擎债务或 Rocq L2b 反例路径。

在该 Homebrew 安装上，明确设置 `AI4MATH_ROCQ_RUNTIME_READS=/opt/homebrew/opt/gmp/lib/libgmp.10.dylib`（实际路径由本地链接信息确认）；不允许把整个 Homebrew 前缀交给编译器。复跑新增原生回归：

```bash
export AI4MATH_TEST_ROCQ="$(command -v rocq)"
export AI4MATH_TEST_ROCQ_DATA="$(rocq c -where)"
export AI4MATH_TEST_ROCQ_READS=/opt/homebrew/opt/gmp/lib/libgmp.10.dylib
export AI4MATH_ROCQ_RUNTIME_READS="$AI4MATH_TEST_ROCQ_READS"
cargo test --locked -p ai4math-broker --test rocq -- --ignored
cargo test --locked -p ai4math-cli rocq_tests -- --include-ignored
cargo test --locked -p ai4math-daemon --test workflow native_daemon_rocq_slot_checks_without_promotion_authority -- --ignored
```

验收：125 项 Rust 全工作区测试（含三种原生引擎）和 19 项 DSH Host/Client 测试通过。覆盖：错误类型、逃出证明槽、跨主引擎和间接使用已安装库公理均不能获得 T1；闭合项在多模块项目中合入并发布可克隆的 `.vo`；模拟 Git 已更新但 SQLite 未提交的中断可重复恢复且只记账一次。daemon 的操作员冻结 Rocq 槽可做 T0 检查且不发回执；Agent 不能覆盖工具链目录。交互 MCP 使用独立环境，不参与本批的原生合入验收。

## 批次 C：Lean 已审规格比较器（托管 T1）

1. 在裸仓库当前主干提交单独的根级规格模块，例如 `TrustedSpec.lean` 中的 `def approved : Prop := Nat.succ 0 = 1`；在模块清单中把它排在候选模块之前。另登记候选 Claim，例如 `Goal.lean#target`。规格必须是其模块自己公开的安全、无宇宙参数 `Prop` 定义，不能是导入别名或候选所在模块；绑定命令只登记精确审阅快照，不预先认定规格通过内核检查。
2. 人工审阅 `SPEC_MODULE` 及其按清单排序的全部前序模块后，运行 `ai4math bind-lean-spec ROOT BARE_REPO CLAIM SPEC_MODULE SPEC_DECL APPROVED_COMMIT`。批准提交必须是当时的 `main`。绑定不提升 Claim 信任；只能重复同一绑定，修改规格需要另行登记目标和 Claim；不允许用同一物理定理的另一个 Claim ID 绕开托管门禁。
3. 后续 `promote-managed` 或 `promote-task-result` 要求候选基线是该批准提交或其后代，并且规格与前序模块的路径、次序和字节散列相同。候选证明先单独验证；比较器随后重编译，并在独立 Seatbelt 进程中以只读数据导入规格前缀和候选产物，不加载项目扩展，重放两者的依赖闭包。它让 Lean 内核用候选定理常量对照已审定义的值（无额外实参、强制转换、tactic 或蕴含证明）。因此可接受 `Nat.succ 0 = 1` 与 `1 = 1`，拒绝即使已经证明但不同的 `True`。
4. 只有同一候选的封存比较证据加全项目回归才能完成受约束的入账。Git 已推进但 SQLite 尚未完成时，`recover-promotion` 根据原候选基线重新比较，最终只记录一次。`ai4math lean-spec-comparison ROOT PROMOTED_COMMIT` 只读显示留存证据；它不能构造准入回执。比较证据在 SQLite 保留，Git commit 的原有内核回执不代替比较证据。

范围：现有未绑定 Claim 保留原冻结模板流程；`promote` 等未托管 Git 操作和外部 Git 写入不在 Claim 升级边界内。受绑定的 Claim 标脏后，普通 T3 单证明复检不会清除它；经已应用计划的规格重比路径见本页下一节。当前只有 Lean 接入单独规格比较，使用同一原生 Lean 内核的独立进程，不是独立实现的 T2 检查器，也未实现跨引擎规格桥接。

运行本批原生测试（需允许项目创建自己的 Seatbelt）：

```bash
export AI4MATH_TEST_LEAN="$(lean --print-prefix)/bin/lean"
cargo test --locked -p ai4math-broker --test lean_spec -- --ignored
cargo test --locked -p ai4math-cli reviewed_spec::tests -- --include-ignored
```

验收：135 项 Rust 全工作区测试（含原生引擎和 Seatbelt）和 19 项 DSH Host/Client 测试通过。新增覆盖非等价但可证明、隐式实参误插入、私有与外模块规格、非许可公理、日志伪造、规格前缀变动、Claim 别名、同候选回执绑定和合入恢复。

## 批次 B：已审 Lean 规格的 T3 重比

沿用 `t3-preview` → `t3-apply` → `t3-recheck`。预览显示原托管证明的 Git 来源；绑定规格仍吻合时显示已审前缀散列，规格缺失、顺序变更或字节不同则显示 `reviewed_spec_changed_or_missing`。判为可复检只代表静态前提满足，不承诺内核通过。

- 复检要求已批准并应用的计划仍对应主干、Claim 在该计划的标脏范围内，原 T1 比较证据可追溯，目标证明 blob 与首次合入相同。编译旧证明，再用同一个 T3 候选回执与已审规格做内核定义等价比较。未受绑定的 Claim 保持原复检规则。
- 证明依赖可在已审规格前缀之后变化。若旧证明仍可证明一个**不等价**的命题，或编译/比较不可用，保留 stale，不写成功证据，也不宣告证伪或降级。即使公式等价，只要已审规格前缀字节改变也要保留 stale；恢复批准的原字节后可用新计划复检，或为新规格登记独立目标。
- `ai4math lean-spec-recheck ROOT CLAIM PLAN_SHA256` 只读查询成功比较记录，绑定 T3 计划、复检提交、原合入提交、候选回执和比较散列。新回执、比较记录、清除标脏及重算有效信任在同一 SQLite 事务提交，重跑不重复入账，不改写 Git 主干。仅凭 T1 证据、普通 T3 回执或读回 JSON 都不能恢复受绑定的 Claim。

原生复跑（需允许项目自身的 Seatbelt）：

```bash
export AI4MATH_TEST_LEAN="$(lean --print-prefix)/bin/lean"
cargo test --locked -p ai4math-broker --test lean_spec -- --ignored
cargo test --locked -p ai4math-cli native_reviewed_spec_t3_restores_only_the_same_reviewed_proposition -- --ignored
```

验收：141 项 Rust 全工作区测试（含三种原生引擎和 Seatbelt）及 19 项 DSH Host/Client 测试通过。覆盖等价依赖变化后的恢复、仍然可证但不同的命题、仅改规格字节的拒绝，以及计划范围、并发与事务回滚。尚未实现结构变更自动发布、T3 自动触发或比较证据的独立 T2 复核。

## 批次 C：双 SMT 有界整数探针（仅建议）

操作者可运行 `ai4math oracle-probe QUERY_JSON NATIVE_Z3 NATIVE_CVC5`。它只输出结构化 JSON，不打开项目库、不修改 Claim，不创建债务、归档、剪枝或 P0，也不接受 Agent RPC。求解器为操作者指定的两个不同原生 Mach-O 安装；装机来源和角色由操作者核对，输出包含可复核的可执行文件 SHA-256，不是签名，也不证明求解器归属。两者独立于项目在两个 Seatbelt 进程中运行，每次求解有 5 秒运行上限；只接受一个干净的 `sat`、`unsat` 或 `unknown` 状态，不输出求解器日志。

输入是有版本的有界线性整数编码，不接收原始 SMT-LIB、外部文件引用、命令行参数、量词或实数。最多 8 个变量、16 条前提、65,536 个整数赋值；系数、常数和端点均限制在 ±10⁹。下面的测试编码表示 `0 ≤ x ≤ 3`，检查 `x < 3`：

```json
{"version":1,"variables":[{"name":"x","lower":0,"upper":3}],"premises":[],"conclusion":{"terms":[{"var":"x","coeff":1}],"relation":"lt","constant":3}}
```

Broker 将**前提与结论的否定**交给两套求解器，并以独立的精确整数枚举复核每个赋值，报告满足前提的数量（零意味着结论在该域内空真）。只有两个求解器和精确枚举一致时，才标记找到反例或域内找不到反例。确切 verdict 相互冲突为 `disagreement`，求解失败、未知或异常输出为 `unavailable`；二者都不允许使用求解结果支持剪枝。输出保留原输入及其散列，只说明该数学编码，不审查自然语言翻译的保真度，也不证明无界情形。可按后续章节的人工批准流程保留建议，操作员还可按专门门禁限时挂起单个探索任务。Oracle 债务与其他剪枝策略尚未接入。

原生复跑（需允许项目创建自己的 Seatbelt）：

```bash
export AI4MATH_TEST_Z3="$(command -v z3)"
export AI4MATH_TEST_CVC5="$(command -v cvc5)"
cargo test --locked -p ai4math-broker native_bounded_oracle_corroborates_exact_results -- --ignored
cargo test --locked -p ai4math-cli --test oracle -- --include-ignored
```

验收：117 项标准 Rust 测试、2 项新增原生 Oracle 测试及 19 项 DSH 插件测试通过。实测 Z3 5.4.1 与 cvc5 1.4.2 对有限域正反例一致；CLI 以无有效结果的本地占位程序替代一方时返回 `unavailable`，且两种运行都不创建项目文件或状态库。求解器互相冲突及其一致但与精确枚举冲突，均由单元测试覆盖。

## 批次 C：人工确认后留存有界 Oracle 观察

1. 使用已绑定的托管裸仓库与已登记正式目标的 Claim；首次绑定可沿用 `task-bind ROOT BARE_REPO ANCHOR_MODULE REVIEWED_COMMIT`。运行 `ai4math oracle-preview ROOT BARE_REPO CLAIM QUERY_JSON NATIVE_Z3 NATIVE_CVC5`。预览不运行求解器、不改变账本或 Git ref；它展示待确认的精确有限整数编码、Claim 非形式化陈述、正式目标、当前 Git commit 和两个求解器的二进制散列。
2. 操作者核对编码保真度及有限域，确认后执行 `ai4math oracle-retain ROOT BARE_REPO CLAIM APPROVAL_SHA256 QUERY_JSON NATIVE_Z3 NATIVE_CVC5`。散列绑定这些精确信息，不是审批签名，也不独立证明翻译正确。协调者在运行前和留存前重检主干与 Claim；求解时不占 Git 锁，发现上下文发生变化就不入账。历史记录绑定该提交，不声称记录之后的 `main` 仍未改变。
3. `ai4math oracle-result ROOT RECORD_SHA256` 只读查询历史记录。相同审批和相同观测只记一次；同一审批下不同求解状态独立保留。事件只包含 ID、散列及状态，不含查询、Claim 陈述或求解器日志。记录和事件原子提交；即使一对求解器与枚举都找到反例，仍然只是编码的证据，不改变 Claim/路线、不创建 Oracle 债务、不写 Graveyard 或触发 P0。观察/审批不经 daemon RPC 或 DSH Agent 对外开放。

复跑原生确认路径：

```bash
export AI4MATH_TEST_Z3="$(command -v z3)"
export AI4MATH_TEST_CVC5="$(command -v cvc5)"
cargo test --locked -p ai4math-cli --bin ai4math oracle::tests -- --include-ignored
```

验收：154 项 Rust 全工作区测试（含原生引擎、双 SMT 和 Seatbelt）及 19 项 DSH 插件测试通过。真实双 SMT 反例对关键 Claim 仅追加一条观察事件；复跑不重记，篡改输入或探针后主干前进不入账。状态测试覆盖待恢复 Git 事务、旧库只读、不可替换记录和禁止通过通用事件接口伪造。

## 批次 C/D：Oracle 反例的限时探索任务挂起

- 操作者检查 `oracle-result` 后，可以用 `ai4math oracle-hold ROOT BARE_REPO TASK RECORD_SHA256 SECONDS` 对**单个**未领过租约的探索任务设置 1–3600 秒调度挂起。只接受当前托管仓库与主干对应的人工确认观察、两套已批准求解器均为 SAT、精确代回亦为反例；结论不一致、不可用或域内无反例都不合格。任务的引擎、模块与声明必须与该 Claim 一致，Claim 已证明、证伪或标脏则不准挂起。
- 挂起的任务暂不参与 FIFO/双轨领取；不撤回已有租约、不直接挂起基建任务，也不修改 ProofPlan、Claim 等级或 P0。已有任务 DAG 的依赖仍然有效，等待该任务完成的下游任务不会被视作已满足依赖。
- `ai4math oracle-hold-status ROOT TASK` 查询该任务的最近一次挂起。按调度器壁钟到期后恢复正常资格（仍须通过依赖、容量与熔断条件）；也可先用 `ai4math oracle-release ROOT TASK HOLD_SHA256` 释放指定记录。同一任务和观察的重试不续期，旧记录无法释放新挂起。启动挂起要求观察对应当时的主干；主干之后变化不会自动撤销已启动的有限调度挂起，可人工解除。
- daemon 的实时遥测与 HUD 显示正数 `Oracle-held exploration`，不能从该数推断 Claim 已被证伪。留存 Fact Pack 不新增随壁钟改变的计数。操作接口仍不经 daemon RPC 提供；本轮不自动发 `WITNESS_VERIFY`、不赋予 `L2a`/L4a，也不做 CAS/Hammer 或修改 DSH 部署配置。

原生复跑：设置 `AI4MATH_TEST_Z3`、`AI4MATH_TEST_CVC5` 后，运行 `cargo test --locked -p ai4math-cli --bin ai4math oracle::tests -- --include-ignored`；另用 `npm --prefix plugins/dsh-ai4math test` 验证 HUD 的只读计数。

验收：159 项全工作区原生 Rust 测试和 20 项 DSH 插件测试通过。双 SMT 真实反例下，挂起的探索任务无法领取，另一探索和独立基建任务仍可领取；人工释放后先前任务恢复。其余测试覆盖到期、旧 ID、并发领取与错证据拒绝。

## 批次 C：有界 Lean 前提重建（本机 Hammer 子集）

1. 在已绑定的托管 Lean 仓库登记开放的 Claim，候选模块保留冻结证明槽。操作者在该 Git 主干提交中选择 1–8 个声明，写入 JSON，例如 `{"version":1,"premises":["Hints.seed","Hints.toMiddle","Hints.toLast"]}`。只接收普通声明名，不接收调用方的 Lean tactic、外部脚本、编译参数或自定义公理。
2. 运行 `ai4math hammer-reconstruct ROOT BARE_REPO CLAIM REVIEWED_COMMIT PREMISES_JSON NATIVE_LEAN`。Broker 用固定的 `solve_by_elim`（深度 4，关闭构造子、引入、对称、反证与前提示自动补充，只给指定的声明名；仍遵循 Lean 自身的 elaboration）尝试关闭原目标，并在另一个隔离进程里做内核回放与公理审计。最多 8 个前序模块，原生阶段总预算 45 秒。结果只返回证明体和精确基线/目标，不会改写 Git、写入 SQLite 或赋予 `L5`，也不会把失败记作债务或反例。主干在运行期间推进、Claim 被标脏、目标被熔断时，返回前重新检查上下文。
3. 人工审阅重建结果后，将 `candidate.proof_body` 原样保存，执行 `ai4math promote-reviewed-body ROOT BARE_REPO CLAIM MODULE TARGET REVIEWED_COMMIT NATIVE_ENGINE PROOF_BODY_FILE`。该命令不信任 Hammer 输出 JSON，重新做 T1、（如绑定）已审规格比较、全项目回归和 Git→SQLite 合入。旧基线拒绝，不以当前快照替代。现用 DSH 不开放 Hammer 操作。

这是固定策略的本机 Lean 重建，不包含 LeanHammer/CoqHammer 外部 ATP、第三方证书格式、自动前提示检索、Tool/Automation agent 调度或 Oracle 债务。可复跑：

```bash
export AI4MATH_TEST_LEAN="$(lean --print-prefix)/bin/lean"
cargo test --locked -p ai4math-broker --test lean_hammer -- --ignored
cargo test --locked -p ai4math-cli --bin ai4math hammer::tests -- --include-ignored
```

验收：166 项全工作区原生 Rust 测试、20 项 DSH 插件测试通过。原生样例验证：多段前提示闭合后可独立回放；前提示不足、未知声明、自定义公理或目标不符均不产生候选；旧基线和熔断在编译前拒绝。受控 CLI 实测候选不写账本不推进 Git，另行审阅合入才赋予 L5。

## 批次 C/D：项目级跨进程原生执行配额

- 同一 macOS 本地文件系统的受信任项目运行目录下，托管 CLI 的原生计算（T1 合入/恢复、T3、见证核验、已批准的熔断复探、Hammer、留存 Oracle 和本地 worker）使用独占租约。daemon 的 T0 探针使用共享租约，仍受 daemon 原有 Fast Lane 容量和内存准入限制。独占租约与其他独占租约、共享租约互斥；多个 T0 共享租约可并行。CLI 不跨进程排队，忙时直接退出，可稍后重试。daemon 忙时返回 `unavailable`，不把忙当证明失败或可合入证据。调度元数据、状态查询和人类熔断等不需要该租约，已登记的任务不会因此丢失。
- 协作进程先取得租约，再取 Git 协调锁（如需）并写入状态；忙时不会启动编译器、领取任务或追加失败事件。内核用项目运行目录下的同一受限锁文件协调；锁文件不因作业结束或进程退出而删除。不同项目的配额独立。不要把同一托管仓库跨多个项目运行目录当成一个资源域。
- 此锁只约束走这些入口的任务。不覆盖无项目上下文的 `oracle-probe`、旧 `promote`、直接启动的编译器、其他项目或其他工具。同 UID 应信任操作者不删换运行目录/锁文件。协调进程被强杀后 OS 会释放它的 FD，但**不等于**证明其子进程已退出；完整孤儿回收、主机 RAM/CPU 硬配额、跨进程优先队列以及热池仍待验收。

复跑：`cargo test --locked -p ai4math-broker --test project_native_gate`、`cargo test --locked -p ai4math-cli --test native_gate`、`cargo test --locked -p ai4math-daemon --test workflow project_heavy_lease`。

验收：175 项全工作区原生 Rust 测试和 20 项 DSH 插件测试通过。跨 PID 实测共享/独占争抢、跨项目隔离、结束/强杀释放和异常锁文件拒绝；正常释放显式解锁，即使并行 fork 暂时继承打开文件描述也不会错误阻挡下一次执行。CLI 忙时未写账本，daemon 忙时 T0 不可用但状态读取正常。

## 批次 E/F：双轨结构性停摆的生产 P0

- Daemon 观察已登记的**普通调度队列**。探索、基建两个轨道各至少有一个未完成且被熔断或依赖卡住的任务，两边都没有可领取任务或有效租约，才开始去抖。空闲/单轨、不足一轮确认、资源争抢、活跃 Oracle 软挂起和待恢复 Git 操作都不触发。不会从此信号推断整个主机已停止工作。
- 按 SQLite 调度时钟至少经过 3 秒，再次观测到相同账本版本和阻塞条件，才原子追加一次 `ALL_TRACKS_BLOCKED`。待确认期发生其他账本事件或 daemon 重启会重新去抖；已发事件跨重启保留，需先观察到队列恢复（不再满足双轨阻塞）才重新武装。若期间只遇到被排除的观测，不视为恢复。通用事件 API 不得伪造新事件；既有旧事件仍可查。告警仍按既有同源 300 秒冷却策略投递；不自动改 Claim、开启 Pivot 或创建新的审阅待办。
- 事件仅携带两个轨道的阻塞任务数，复用现有 `alert.p0` 和不可变 Fact Pack；浏览器夹具验证打开留存简报可直接看到两轨阻塞数，旧事件及非法计数不冒充结构化读数。原生 UDS 测试验证真实调度依赖产生该事件，读取已留存 JSON/Markdown，重启后不重复。独立的 installed DSH fixture 验证本机安装版 Electron/DSH Cordis + Typert 从临时项目的 Rust UDS 接收该告警，并能取回同一 Fact Pack。未修改当前用户 DSH profile，也没有部署到当前 GUI。

复跑：`cargo test --locked -p ai4math-daemon --test workflow scheduler_starvation`；[独立 DSH fixture 的启动命令](<docs/phase-2-3.md#L24-L32>)传入 `ai4math` 后启用真实双轨场景。

验收：183 项全工作区原生 Rust 测试、20 项 DSH 插件测试通过；独立 installed DSH fixture 的真实推流与留存读取通过。覆盖有就绪/运行任务、仅一轨、Oracle 排除、Git 恢复意图、时钟回退和重启去重。

## 批次 E：受管 Lean 下游回归异常的生产 P0

- 托管合入已通过候选 T1（以及适用时的规格比较）后，若完整回归在**目标之后的已跟踪 Lean 模块**失败，Broker 才尝试归因。只认可精确文件中 Lean JSON 报告的已知类型/定义等价不匹配；目标验证失败、工具链缺失、超时、内存/沙箱错误、未知诊断、Git 冲突和恢复流程均不进入计数。本规则说明下游回归确实失败，**不**断定失败由某个证明体引入。
- 同一托管 Git 基点、Claim、目标、编译器和失败模块，按 SQLite 时钟在固定 15 分钟内累积 **3 个不同的候选源码摘要**，才在事务内发一条 `BROKER_REGRESSION_MELTDOWN`。相同摘要重试不增加次数，也不续期；发出后同一上下文在该基点不重复。新的受管基点或成功合入重新建立计数域；告警仍经过既有同源冷却。该生产入口不接受任意事件注入。
- 告警留存的证据字段只包含失败模块、不同候选数量和窗口；简报显示失败模块与次数，不包含编译日志或证明体，不降低 Claim 等级、不写合入意图，也不阻止修复后的合入。暂不覆盖 Agda/Rocq 自动归因、热池持续驱逐或所有回滚类别。

复跑：构建 `ai4math`，设置 `AI4MATH_TEST_CLI` 指向该二进制，`AI4MATH_TEST_LEAN` 指向原生 Lean，运行 `cargo test --locked -p ai4math-daemon --test workflow native_managed_regressions -- --ignored`。

验收：191 项全工作区原生 Rust 测试、20 项 DSH 插件测试通过。真实 Lean/CLI/daemon 样例核对普通证明失败不计、同一候选重试不计、第三个不同候选只发一次 P0 且不推进 Git，修复下游后正常合入达 L5；另复跑上一轮独立安装版 Typert 的停摆告警。

## 批次 E：关键定义熔断须耗尽已登记的备选路线

- `ai4math trip-breaker … --critical` 是操作者请求评估 P0，不是直接发告警。熔断与原队列隔离继续生效；只有精确登记的引擎与 `MODULE#DEFINITION`，经 SQLite 的 AND/OR 路线判断，确实让至少一个开放的关键 Claim 从有潜在路线变成无已知可用路线，才生成 P0。没有映射、无关键目标、已有阻断或存在仍可能的替代路线时，只写 P1 审阅待办。单纯 Oracle 暂停不是路线淘汰证据；仍有效的 L5 证明不因局部调度熔断失效。
- 决策、局部熔断、租约失效和 P0 事件在同一事务；不改变 Claim 信任、不将 ProofPlan 记为证伪，也不尝试证明不存在数学上的其他路线。命令回报实际 Inbox 优先级和受影响关键 Claim 数。留存简报包含首个受影响关键 Claim、数量及对应熔断序号；旧告警仍可回看，通用事件 API 不能注入新告警。
- 本轮仍为人工报告入口，不包含自动 Definition Smell 阈值、运行模型的温和停止/工作树保留或其他 Pivot 路径。可复跑 `cargo test --locked -p ai4math-daemon --test workflow critical_breaker_alert`；此测试同时覆盖保留替代路线的 P1 与转接依赖上的最后一条路线被阻断后的 P0。

验收：163 项标准 Rust 工作区测试、20 项 DSH 插件测试通过；另单独通过真实 CLI/daemon Pivot 测试并复跑安装版 Typert 推流 fixture。未在本轮重跑整个原生多引擎矩阵。

## 批次 D/F：任务专用 DSH Agent 能力（首个隔离验证）

- 受信任 Host 可为一个**新建、用后释放**的 DSH Agent 装配一份已切片的 Lean 任务。任务绑定 Agent 对象和 Git 基点，只有读当前上下文、交一次建议两个工具；输入不能传另一任务 ID，输出不能携带回执、替换上下文或授权合入。取消、过期、不同 Agent 对象、重复提交均拒绝。Reviewer 只接收任务标识和非形式化陈述/形式头，不接收 Writer 历史、作者身份或候选证明体；受信任上游仍负责生成干净的声明切片。
- 安装器屏蔽继承工具、使用限定执行守卫；启动回合前应核对 Agent 可见工具严格等于这两项，并选用 Native Tool Executor。安装版验证通过后仍只能证明任务工具作用域，不等于同 UID 操作系统隔离，不授权任务从 SDK 以外直接访问进程、UDS 或 SQLite。
- 该模块是供可信调度器集成的内部件，默认不注册到现用插件、Remote 或浏览器；不使用现用用户 profile 或模型凭据。内存中的截止时钟和一次性结果不是 SQLite 任务租约。本轮不声称真实模型已经派发；下一步是新建任务 Agent、从持久租约生成切片、用实际模型 adapter 执行，再按精确 Git 基点和租约把候选写回协调器。

验收：163 项标准 Rust 工作区测试、24 项 DSH 插件测试通过。本机安装版 Electron/DSH 使用无凭据的确定性测试 adapter 驱动真实 Agent 工具循环：每次模型请求恰好暴露两个任务工具，伪造的 bash 和 run_code 调用均被拒绝，建议只可提取一次；未运行真实模型、未部署当前 GUI。复跑见[独立实装夹具启动命令](<plugins/dsh-ai4math/host/test/installed-model-task.mjs#L1-L5>)。截止时间仅限制任务工具和结果提取，尚不负责终止真实模型请求。

## 批次 D/F：DSH 单任务建议与 SQLite 持久租约

- 受信任的独立 Host 通过私有 stdin/stdout 管道一次性调用 `task-run-model ROOT BARE_REPO TASK CLAIM ROLE WALL_SECONDS`。目前只接 Lean 证明者与盲审员，按现有调度资格领取任务；SQLite 租约和 fence 留在 Rust 进程。只发送绑定任务、Claim、角色和 Git 基点的冻结声明与陈述，不传路径、凭据、源码上下文或 fence（模块名是逻辑身份）。
- 返回一条有界结果并关 stdin 后，Rust 重新检查同一任务、Claim、角色、目标、Git 主干与声明快照，并在 SQLite 消费仍有效且未被熔断的租约。超时、错配、主干移动或熔断不留候选。无可用建议时，Host 先发送空 EOF，让协调器在租约仍有效时记录失败并释放；超出清理时限或进程掉线时按 TTL 恢复。协议错误、不完整输出均不自动重试，确认帧丢失应先用 `task-result` 查询，不重放提交。
- `task-result` 标注 `source: "dsh_model"`，不伪造原生可执行文件摘要；保留内容始终是未验证建议。Reviewer 输出没有审签或合入权，证明体仍需操作者另行通过现有内核合入流程。未向 daemon、浏览器或 Agent 注册调度入口。
- 复跑：`cargo test --locked -p ai4math-cli --test model_pipe`。构建 `ai4math` 后，按[独立安装版夹具命令](<plugins/dsh-ai4math/host/test/installed-model-task.mjs#L1-L5>)传入 CLI 的绝对路径，即可测试真实 DSH SDK 循环 → 私有管道 → Git/SQLite 候选回写（使用离线测试 adapter，不访问现用 GUI、profile 或在线模型）。

验收：169 项标准 Rust 工作区测试、41 项 DSH 插件测试通过。本机安装版 DSH 实测证明者和盲审员均可消费 SQLite 租约并留存对应的未验证建议，主干未改变。另实测错误基点、主干移动、模块熔断、stdin 不关闭、重复领取均不产生越权候选。未重跑全量原生多引擎矩阵。

## 批次 D/F：一次性任务执行与独立内核合入

- 私有管道写回的 `dsh_model` 结果仍视为未验证建议。已用原生 Lean 和项目自带的 Seatbelt 检查：同一个 `True.intro` 候选可在 `True` 声明下显式合入，仅此时 Git 推进、账本追加一次回执与合入事件，Claim 达到 L5；候选用于 `False` 时由内核拒绝，不推进 Git、不写回执、不改变信任。成功后的旧快照候选拒绝再次合入。盲审员的正面建议在进入验证器前即因角色被拒绝。
- 复跑：在允许项目自身启动 Seatbelt 的宿主机上，设置 `AI4MATH_TEST_LEAN` 为原生 Lean 的绝对路径，运行 `cargo test --locked -p ai4math-cli --test model_pipe native_dsh_model_candidate_requires_kernel_promotion -- --ignored`。普通 `model_pipe` 回归覆盖盲审角色、已写回候选遇到新基点时的拒绝，以及结果回写本身，默认不启动原生验证器。
- 原生验收在本机真实 Lean + Seatbelt 下通过。外层工具默认沙箱无法嵌套 Seatbelt，已按工具批准流程仅对该临时宿主测试复跑。

### 受信任的一次性 Host 入口

由独立操作者程序调用 [Host 入口](<../plugins/dsh-ai4math/host/model-task-host.mjs>)的 `runDshModelTask`：显式提供 CLI/SDK 路径、项目/裸仓库、任务/Claim/角色、模型 provider/model、时限，以及唯一的 adapter 实例或工厂。推荐工厂为每个任务创建新 adapter；**工厂与 adapter 是受信任的宿主代码，不是可由任务正文指定的插件**。

入口不安装插件、不查找凭据、不读取用户 profile、不自动挑选任务或失败重试。单次完成只返回已有 CLI 的候选写回确认；在线模型、生产凭据注入和当前 GUI 挂载另行验收。

每个任务建立独立的内存 Agent 和空 preset；不装载历史、持久化或通用工具。使用 Native Tool Executor，在实际 provider 请求处核对只有两项任务工具；最多 8 次工具调用，默认最多 4 次模型请求（仅可设 1–8）。完整回合与一次性结果缺一不可。取消或截止时先撤销能力，向 SDK Agent 发取消并等待 adapter 退出；不保证能抢占不理会信号的第三方 JavaScript，也不等同于操作系统隔离。

验收：171 项标准 Rust 测试、44 项插件测试通过。按[实装夹具启动命令](<../plugins/dsh-ai4math/host/test/installed-model-task.mjs#L1-L5>)传入构建好的 CLI，实测证明与盲审落账、无建议释放租约、拒绝未完整回合、模型与工具次数上限、主动取消和到期时 adapter 清理。本机安装版 DSH 使用离线 adapter，未发真实模型请求。

### DeepSeek Messages 原生 adapter 接入（独立 Host）

- [固定供应商入口](<../plugins/dsh-ai4math/host/model-task-deepseek.mjs>)从明确指定的已安装 DSH SDK 装配真实 `DeepSeekAdapter`，而不是载入带账号流程、模型发现或默认凭据的 DSH preset。路由、模型、API key 均显式；不读取当前 profile、DSH 凭据、环境默认 key，不把凭据传给 CLI、Agent 工具或结果。文本任务不接 SDK 上传索引、附件、文件工具或自定义请求扩展。
- 远程调用须由操作者显式传入 `allowRemote: true`，目标固定为 `https://api.deepseek.com/anthropic`。测试专用覆盖只接受显式的 `http://127.0.0.1:PORT` 和 `fixture-*` 假凭据；真实形状的 key 不能用该分支。不要把 API key 写进命令行参数、任务或日志。调用有原 runner 的次数/时限约束；远程接入不改变候选信任。

验收：171 项标准 Rust 测试、46 项 DSH 插件测试通过。用[实装 Messages 夹具](<../plugins/dsh-ai4math/host/test/installed-deepseek-task.mjs#L1-L5>)在临时 loopback HTTP 服务上运行真实安装版 SDK：证明与盲审都以精确角色工具 schema 完成 SSE 工具往返，建议落账而 Git 主干不变；401、307（不跟随）和工具已接受建议后返回的 503 均不留候选，同任务可显式重跑。只用了假凭据和本机端口，未验证公网账号、在线推理、计费或当前 DSH GUI。

## 批次 B：已批准 T3 工作清单与单项复检

- `t3-worklist ROOT BARE_REPO PLAN_SHA256 LIMIT [AFTER_CLAIM]` 只读 SQLite，在同一快照中恢复该计划实际批准的 Claim 集并按依赖序分页（每页 1–64 项）。页内只含 Claim 标识、状态和账本事件序，不暴露证明体、日志或凭据。已解决或不再符合复检条件的 Claim 仍保留在清单中，复检后原游标仍可使用。若并发改动可能调整顺序，用 `last_event_seq` 判断并从首页重读。
- `t3-recheck-one ROOT BARE_REPO PLAN_SHA256 CLAIM NATIVE_ENGINE [ENGINE_DATA]` 仍使用现有原生 Broker、仓库锁与项目门禁，只重验一个已批准 Claim；计划必须仍匹配 Git 主干。越界拒绝，已处理的 Claim 重跑不重复入账。完整计划复检也不再接收同提交但不在批准清单的普通 Claim。没有自动批准 Git 变更、常驻派发器或新增 daemon/模型写接口。

验收：174 项标准 Rust、46 项 DSH 插件测试和两项原生 Lean T3 测试通过。实测未批准计划和旧主干读不到工作清单；越界 Claim 不写账，单项成功只追加一次回执，重跑不重复；已审规格仍走同一原生比较门禁。

## 批次 B：有上限、可续跑的 T3 批次复检

- 操作者显式执行 `t3-recheck-batch ROOT BARE_REPO PLAN_SHA256 LIMIT NATIVE_ENGINE [ENGINE_DATA] [--after CLAIM EVENT_SEQ]`。每批从已经应用的计划中按依赖序选连续 1–8 个 Claim；已解决、不可复检和本轮延期的 Claim 也计入页长，避免卡在首个延期项。复检仍按 Claim 单独原子入账，且只允许当前批准的 Git 主干；不会为未批准计划或越界 Claim 启动 Broker。
- 下一批使用同一计划与上一批返回的 `next_after_claim` + `last_event_seq`。账本代次已变则拒绝旧游标，需从首页读；若批次失败或退出，已完成的回执仍在，从首页重跑不会重复写入已解决项。延期不等于反例、不自动降信任；走完整轮后可以显式从头重试延期项。
- 这是操作者命令，不会自动批准结构变更、启动常驻调度、调用模型或配置当前 DSH。原 `t3-recheck` 和单项命令仍可用。

验收：179 项标准 Rust 测试、46 项 DSH 插件测试以及两项原生 Lean T3 测试通过。实测上限和依赖序、旧游标拒绝、新游标续跑、批次中途错误后重开 SQLite 保留先前回执，以及已审规格继续走原生比较门禁。

## 批次 B：T3 批次持久检查点

- 操作者运行 `t3-sweep-step ROOT BARE_REPO PLAN_SHA256 LIMIT NATIVE_ENGINE [ENGINE_DATA] [--restart]`，每次最多处理 8 个已批准 Claim。成功页的游标和账本代次由 SQLite 持久保存；下一次运行无需手抄游标。崩溃或其他写入导致未完成检查点过期时，从首页保守重读，已入账回执不会重复执行。并发推进不得覆盖新检查点。
- `t3-sweep-status ROOT BARE_REPO PLAN_SHA256` 只读，不需要验证器。`complete` 表示已迭代完一遍工作清单，不意味着每项都验证成功；延期项仍可能标脏。结束状态持久锁存；只有显式 `--restart` 才发起新一遍。检查点不写证明事件，不能作为内核回执或提升信任。
- 仍只接受已应用且与当前 Git 主干相符的计划，不自动批准、不暴露 daemon 写入口，也尚未接入常驻自动派发。频繁并发写入可让未结束的 sweep 多次回到首页；本轮不宣称有自动调度公平性。

验收：184 项标准 Rust、46 项 DSH 插件测试和两项原生 Lean T3 测试通过。独立 CLI 进程实测读回游标、代次变化重读首页、新增但未批准的 Claim 不入批次，以及完成后无需验证器；SQLite 回归另外覆盖中途失败后保留早先回执和并发检查点拒绝。

## 批次 B：驻留 daemon 复检（显式单计划、Lean）

- `ai4mathd --project ROOT --t3-autosweep CATALOG` 是独立的操作者启动选项，默认不启用。私有 JSON 写明已经应用的计划摘要、裸仓库、受信任 CLI、原生 Lean、每步页长和间隔；daemon 不自动批准或发现计划，不接受 RPC/模型输入设置这些字段。每步仍由 CLI 重新核对仓库、批准 Git 主干、项目门禁和 SQLite 检查点。
- 目录与二进制使用可信规范绝对路径；配置文件须仅所有者可读写（最大 16 KiB）。字段：`version:1`、`cli_executable`、`bare_repo`、`plan_sha256`、`native_executable`、`page_limit`（1–8）、`interval_ms` 和可选 `step_timeout_ms`（单位毫秒、均为 100–3,600,000，后者默认 120,000）。每次启动最多推进 256 页，防止账本不断变化时无限回退。
- 单独 worker 一次只运行一页，等待子进程时不占用 RPC 线程或 daemon 内存中的存储互斥锁；失败、超时或完成会停止该 worker，不停止 RPC。启动后直接续跑未完成检查点；完成不会自动开启下一遍。CLI 子进程使用专用进程组，守护进程失去 stdin 寿命管道时终止该组；Lean 构建、复检及规格比较维持原 Seatbelt、按调用的资源限额，并在该模式继承这一组。超时只记录固定分类，不向 HUD/RPC 泄露原生输出。
- 不修改当前 DSH profile，不配置 DSH 定时任务；当前不覆盖自动发现结构变更、多计划竞争、Agda/Rocq 常驻复检或跨主机调度。

验收：186 项标准 Rust、46 项插件测试通过。额外通过真实 Lean + Seatbelt + daemon 夹具：默认不派发，未批准或旧主干计划无回执；中途重启后续跑，恰好新增一份原生 T3 回执并恢复 L5，完成态重启不重放，状态 RPC 持续可用。进程寿命管道断开和非法目录清单也有单独回归。

## 批次 B：受管 L5 证明的 Wiki 沉降快照

- 操作者先运行 `wiki-sediment-preview ROOT BARE_REPO CLAIM REVIEWED_MAIN`，核对原合入提交、源码 SHA-256、历史前置模块哈希和顺序，再以 `wiki-sediment-archive ROOT BARE_REPO CLAIM REVIEWED_MAIN APPROVAL_SHA256` 确认。只接受未标脏且本地与有效信任均为 L5、具备受管合入配方的 Claim；从冻结模板重建并逐字节校验原合入 Git blob，而非读取当前工作区源码。审批摘要绑定项目、裸仓库、当前批准主干与完整产物。
- 写入一个按内容标识的冷层 Markdown 文件。同字节重复操作不改写；同名不同内容或不安全目录/文件拒绝。归档不推进 Git、不删模块、不改变任何 Claim 信任，也不触发重新验证；文档中的 L5 是历史提交的事实。定义指纹目前是保守的整模块字节哈希，不是定义级等价证据。
- 由操作者审阅并将 Markdown 纳入项目 Wiki 的版本管理；用 `wiki-rebuild` 更新本地可重建索引后，条目只出现在冷层正向召回，并保留 `born_from_claim` 血缘。尚未实现从 CORE 移出、自动沉降、唤醒后适配或重升 CORE；无 daemon、模型或当前 GUI 写入口。

验收：193 项标准 Rust、46 项 DSH 插件测试通过，另通过真实 Lean + Seatbelt 的受管合入→预览→沉降→索引重建测试。实测错误审批、旧主干、标脏、同名异字节、文件/目录别名拒绝；并发同内容只发布一次，默认 CORE 不召回快照。

## 批次 B：Wiki 冷层的只读唤醒预览

- `wiki-sediment-wake-preview ROOT BARE_REPO ARCHIVE_ID REVIEWED_MAIN` 从项目 Wiki 冷层回读规范快照（只接受内容寻址 ID、拒绝别名、限制文件和源码大小），校验全字节与 ID。自洽的文件不构成证明：还须匹配同项目同 Claim 同 Git 提交的历史受管合入配方、原回执的源码哈希和 Git 中可重建的内容，不回退到较新回执。
- 对操作者明确指定且仍等于当前主干 HEAD 的 Git 提交，比较目标模块和顺序敏感的历史前置模块；返回字节一致、目标变化、前缀变化、二者变化或目标缺失。前缀差异列出新增、移除、修改及顺序是否相同。字节一致不等于工具链、依赖语义或 Lean 规格等价，也不授予当前信任。
- 不调用证明器、不更改 SQLite 账本、Git、Wiki 文件或索引，不暴露 daemon/模型入口。可查看已标脏 Claim 的历史归档；后续仍需操作者审阅和真实验证。惰性重验写回、直接重升及派发适配任务均未在本轮实现。

验收：199 项标准 Rust、46 项插件测试通过，另通过真实 Lean 原生夹具；覆盖同字节、只改目标、只改前缀、二者改变及模块缺失，且读取标脏归档不改变状态。伪造快照即使重新计算摘要并借用真实提交，也无法冒充该提交中的受管证明。

## 批次 B：操作者发起的 Lean 冷层惰性重验

- `wiki-sediment-wake-recheck ROOT BARE_REPO ARCHIVE_ID REVIEWED_MAIN NATIVE_LEAN` 在项目原生资源门与 Git 锁内复用唤醒预览，只运行当前主干中目标源码逐字节相同、且仍匹配 Claim 正式目标的历史证明。无已审规格时必须保持前置模块内容和顺序；有规格时，历史 Comparator 必须绑定原合入 T1 回执，且当下规格及其前缀仍符合审批。通过后用当前主干依赖在 Seatbelt 中重放，并在需要时执行原生规格比较。
- 返回已重验、延期、证明拒绝或验证不可用等结果和固定原因；只有核验通过才返回 SHA-256 摘要（不是持久回执）。数学不匹配或比较器不确定不能成为已审规格的降级证据。Git 主干和账本序号在原生执行前后核对，变化即丢弃结果。
- 不写入证明账本、Git 对象/引用、Wiki 快照或索引，不恢复信任，不进 CORE，不派发适配，不打开 RPC/模型入口。锁和验证临时文件不是证明状态；Agda/Rocq 冷层原生重验另待验证。

验收：200 项标准 Rust、46 项插件测试通过；另通过两项真实 Lean + Seatbelt 测试，覆盖不变依赖的原生重放、规格前缀内外的变化，以及证明仍能编译但规格已不匹配时不给摘要。测试核对账本、Claim、Git 引用与 Wiki 内容均未改变。

## 批次 B：保留冷层重验观察（不授予证明权限）

- 显式运行 `wiki-sediment-wake-retain ROOT BARE_REPO ARCHIVE_ID REVIEWED_MAIN NATIVE_LEAN` 才尝试留存；它重新执行原生检查，不接受外部 JSON、先前摘要或用户构造的回执。旧 `wake-recheck` 仍为只读；延期、拒绝和不可用都不会留存。
- 只将成功的密封 Lean 重验（若有已审规格，还需本次密封比较）作为独立观察入库。写入事务比对已读账本序号、项目绑定、历史配方、当前 Claim 目标和已审规格，原生与 Git 锁持有到追加完成。观察与单条审计事件在同一个 SQLite 事务；读取用 `wiki-sediment-wake-result ROOT OBSERVATION_SHA256`，不会触发重验。
- 留存的摘要和审计事件不进入验证回执表、不能改变 Claim 信任或解除标脏，不修改 Git、Wiki 文件或索引，不新增 RPC/模型入口。观察标识钉住具体运行证据，而不是为未来主干承诺等价；不同原生运行可产生不同观察。惰性适配派发、恢复 CORE 或复用进入合入流程仍待实现。

验收：204 项标准 Rust、46 项插件测试以及 4 项真实 Lean 测试通过。真实用例覆盖有/无已审规格的留存、规格不匹配无写入、旧式 T3 及改变无规格前缀的拒绝；事务回归确认失效代次、插入失败、相同密封值重试与账本/信任隔离。

## 批次 B：冷层适配的操作者审批交接

- `wiki-sediment-adapt-preview ROOT BARE_REPO ARCHIVE_ID REVIEWED_MAIN TARGET_CLAIM` 只为当前主干已变化的真实 Lean 冷层快照准备交接；必须另有一个未标脏、未完成受管合入的目标 Claim，其 Git 模块包含严格的冻结证明槽，不能覆盖或别名复用历史目标。
- 原归档必须有对应 T1 合入和 Comparator；新旧 Claim 必须绑定同一份已审规格（同文件、声明及有序前缀摘要），并核验当前 Git 中的规格前缀。审批摘要绑定历史快照、当前主干、目标完整源码、规格及账本序号。摘要不匹配或账本/主干变更需要重新预览。
- 操作者用 `wiki-sediment-adapt-approve ROOT BARE_REPO ARCHIVE_ID REVIEWED_MAIN TARGET_CLAIM APPROVAL_SHA256` 显式留下不可改的审批和单条审计事件；`wiki-sediment-adapt-result ROOT APPROVAL_SHA256` 只读查回。交接不是工作队列、租约或候选证明；本轮不启动模型、不排队、不改变 Git、Wiki 或 Claim 信任，也不开放 daemon RPC。适配任务实际调度和验证合入仍待实现。

验收：208 项标准 Rust、46 项插件测试及 1 项真实 Lean + Seatbelt 测试通过。真实夹具验证历史 T1 归档、定义变更后的新槽位、过期主干及错误审批拒绝；审批只追加一条事件，任务、Claim、裸 Git 仓库和 Wiki 均保持不变。事务回归另覆盖账本失效、回滚、去重和不可改写。

## 批次 B：已批准冷层适配的显式本地派发

- 操作者调用 `wiki-sediment-adapt-run ROOT BARE_REPO APPROVAL_SHA256 WORKER_EXE WORKER_SHA256 WALL_SECONDS`，就地核对审核主干、冻结槽和两端 Claim 规格；只有这条入口能按审批租用专属基建任务。审批摘要映射固定任务 ID，普通 FIFO 或模型入口不会捡走该任务，过期/失败停放不挡普通任务。派发与普通任务共享项目原生资源门、并发上限及熔断规则。
- 固定为本地 `ProverWorker`（1–60 秒、显式绝对二进制与 SHA-256），沿用工作进程隔离。只从已核验的历史配方取证明体作为参考；引用不是当前证明，结果仍受冻结槽、大小及租约代次检查。主干、目标或审批输入变化后不接受候选。
- `task-result` 可读未验证建议；必须另走受管原生验证、规格比较及合入门禁，不能由任务完成或引用恢复信任。不启动 DSH 在线模型，不新增 daemon RPC，不写 Wiki CORE 或原快照。

验收：214 项标准 Rust、46 项插件测试通过；另通过真实 Lean + Seatbelt 与哈希固定的 C worker 夹具。过期 HEAD 拒绝启动；有效审批只留存一个未验证候选，普通任务不变；另行执行真实比较器和受管合入后，新 Claim 才获得 L5，旧归档不改。六项调度回归覆盖并发 CAS、容量、熔断、停放租约和原生候选绑定。

## 批次 B：冷层适配成功后的可恢复替代血缘

- 新版本已经独立通过受管 Lean T1 合入后，操作者可用 `wiki-sediment-adapt-lineage-retain ROOT BARE_REPO APPROVAL_SHA256 PROMOTED_HEAD` 补录历史替代关系。该命令只读取现有 Git 对象：重验冷层归档身份、已批准冻结模块和规格前缀，核对新提交只有原批准提交一个父节点且只改批准的模块，模块字节须能由原冻结槽和入账配方重建。
- SQLite 在一个事务中核对审批、专属 native worker 候选、候选在合入前完成、候选证明体与最新 L5 合入配方、原/新 T1 与规格比较摘要，以及账本代次，再追加一个只读历史凭据与审计事件。失败不写入；同一历史记录可查回，不重复入账。一个新 T1 合入只接纳一条显式替代血缘。
- `wiki-sediment-adapt-lineage-result ROOT APPROVAL_SHA256` 返回 `superseded_by` 的两端标识；目标 `CORE-ADAPT-…` 为后续投影预留。当前不创建 CORE 文件或索引条目，不更改原归档或 Claim 信任，不增加 daemon 写入口。

验收：220 项标准 Rust、46 项插件测试及 1 项真实 Lean + Seatbelt 端到端测试通过。候选已产出但还未合入时拒绝，完成 T1 后只追加一条历史血缘事件；重试、错主干和错摘要无副作用。回归另验证只读 Git 树、候选先于合入、回执身份及单个 T1 合入不接受第二条血缘。

## 批次 B：可重建的适配 CORE 投影（操作者快照）

- `wiki-core-rebuild ROOT BARE_REPO REVIEWED_MAIN` 从项目 Markdown 与 SQLite 已留存替代血缘原子重建本地索引。初次投影只接纳仍持有原 T1 回执且当前有效、未标脏的 L5 目标；Git 主干必须包含该合入，目标内容和有序前置模块必须逐字节匹配。单次最多考察 64 条符合账本条件的血缘，超出拒绝重建。
- 派生 CORE 行只含声明和简要说明；`.managed-core/<ID>` 是索引虚拟路径，不是 Markdown 文件。索引连上原始冷层 `superseded_by` 边，不改原归档，不写 Git，不恢复信任。普通 Markdown 重建会清除派生投影；Markdown 不能冒用其 CORE 标识。
- `wiki-core-search` 与 `wiki-core-provenance` 仅在当前受管 Git HEAD 和 SQLite 事件序号同时匹配时返回派生条目或边。任一变化都要求重建；即使无关事件也保守隐藏。普通 Wiki 索引读法仍隐藏派生内容；daemon/HUD 门禁和 T3 后恢复见后续批次。

验收：226 项标准 Rust、46 项插件测试、1 项真实 Lean + Seatbelt 端到端测试通过。端到端覆盖受管合入→血缘→CORE 查找；前置 Git 模块变化、尚无 T3 账本事件时重建也不会显示该条目；另独立验证同一 HEAD 上 T3 标脏隐藏条目及边。归档、证明和历史血缘保持不变。

## 批次 B：现有 Wiki RPC 与 HUD 消费派生 CORE

- `wiki.search`（仅 CORE scope）与 `wiki.provenance` 使用 daemon 管理的仓库与账本，不接受调用方指定 HEAD、目录或代次。不修改原 RPC 参数和权限。读请求前后核对直接的受管 main（包括 packed refs）、SQLite 最后事件序号和两类待恢复日志；出现漂移、Git 无法观察或索引不匹配时，只返回普通 Wiki 项和边。SEDIMENT 与 GRAVEYARD 检索不进入此门禁。
- CORE 搜索与血缘响应增加 `managed_core`，表明本次响应启用了匹配快照，不承诺已有命中或替代证明审核。HUD 区分派生项与手写 Markdown，将虚拟索引路径展示为非文件位置，并标识仅普通条目的降级。展示是请求时快照，不是实时订阅；重新查询才会重新采样。无新写入方法；重建仍为操作者显式动作。
- 本轮验收以集成运行 `ai4mathd` 与插件契约/渲染测试为准；未变更 live DSH 配置，未宣称已在当前 GUI 部署。

验收：229 项标准 Rust、49 项插件测试及 1 项真实 Lean + Seatbelt 回归通过。运行真实 daemon 的独立夹具验证 loose/packed 主干漂移、同 OID 符号引用拒绝、恢复直接引用、仅事件序号漂移、客户端不能指定代次或写入；另验挂起的 Git 日志不增加事件也会隐藏派生内容。HUD 仿真验收覆盖派生标签、虚拟路径、边和静态降级。

## 批次 B：封存 Lean T3 复检后恢复派生 CORE

- 操作者完成已批准的 Lean T3 复检及规格比较后，同一 `wiki-core-rebuild` 可以恢复原派生条目。只有目标仍为本地和有效 L5、未标脏、当前回执是该条血缘的精确 T1（初次）或绑定该 T1 的封存同规格 T3，才成为投影候选；普通 T3 回执和历史旧回执不够。
- 复检后以 T3 审批的 Git 提交为验证锚点：原 T1 必须在其祖先历史内、当前 HEAD 必须包含该锚点，冻结证明源和受审规格必须匹配，并逐字节比较自验证锚点以来的有序前置模块。后续依赖再漂移即使未标脏也不能恢复显示。
- 复用原 `CORE-ADAPT-…` 条目标识和 `superseded_by` 边；公开 `verified_at_commit` 前移到 T3 验证锚点，HUD 将其标作验证提交；原 T1、冷层快照和不可变血缘保持不变。只改索引；没有自动重建动作，也不增加可写 RPC。

验收：232 项标准 Rust、49 项插件及 1 项真实 Lean + Seatbelt 端到端测试通过。真实测试改变前置模块字节但保留定理，在 T3 审批、标脏、Lean 复检和规格比较后恢复同一 ID 与同一血缘；之后再次变动依赖即拒绝投影。账本回归覆盖重复 T3、普通回执、被替代的 T1 与证据不匹配。

## 批次 B：从显式基线只读发现 T3 结构变化

- `t3-discover ROOT BARE_REPO ANCHOR_MODULE BASELINE_COMMIT` 自动读取绑定裸仓库当前直接 main，并与操作者指定的完整 Git 基线比较，不需要人工抄写新 HEAD。它以受管模块内容及模块清单的顺序/成员为准：仅提交元数据或 ADR 变化为 `unchanged`；非后代主干为 `diverged`，不猜测基线也不生成计划。基线必须是有效的受管项目快照；只有主干是其后代时才比较两端，且锚模块必须仍存在。
- 结构变化返回 `structural_change`、模块差异、观察到的 HEAD 与账本代次，以及与同一快照的 `t3-preview` 等价的计划摘要。持有 Git 协调锁，读前后校验 Git、账本和待恢复事务；不自动批准、标脏、运行引擎、改 Git 主干或派发任务。SQLite 仍可能创建或更新只读连接所需的 WAL 同步边车，不追加账本记录。只有另行调用原 `t3-apply` 才能应用，主干或计划快照变化会拒绝旧审批。
- 此命令本身是一次性操作者入口，不接进 RPC；可选常驻调用见下一批次。按 Claim 选基线和显式同主干多计划续跑见后续批次。比较的是端点快照，不声称检测已撤回的中间提交。

验收：238 项标准 Rust、49 项插件及 1 项真实 Lean + Seatbelt 复检回归通过。五项独立 CLI 测试覆盖无变化/仅元数据或 ADR、源码变更与显式批准、只调整模块顺序、同树异分支、错仓库与非法基线；核对发现前后 Git 和 SQLite 持久内容不变，并验证计划完整等同于原预览。

## 批次 B：显式启用的常驻 T3 发现

- `ai4mathd --project ROOT --t3-discovery PATH` 选择启用一个常驻只读观察器；默认不启用。配置文件仅归操作者所有，须是绝对规范路径、不可经符号链接或有额外 ACL 授权。版本 1 字段：`version`、`cli_executable`、`bare_repo`、`anchor_module`、`baseline_commit`、`interval_ms`（100–3600000）及可选 `probe_timeout_ms`（100–60000，默认 10000）。基线不会自动推进。
- 单个后台工作线程逐次运行固定的只读发现命令，清理环境，限制单次进程的时长与输出，使用独立进程组与父进程生命线，只拿既有受保护的 Git 协调锁，不会重建丢失的锁文件。保留最新摘要，不留存计划正文；失败后显示不可用，清空上次摘要。不会自动标脏、批准、复检、派发或改变 Git；显式单计划 autosweep 维持原边界。
- 现有 `status` 和 `vital_signs.tick` 在启用时附送 `t3_discovery`；HUD 只显示上次观察和账本代次陈旧提示，无批准操作。未挂载当前 DSH，也未创建 DSH 定时任务。

验收：245 项标准 Rust、50 项插件测试及 1 项独立双进程实测通过。实测由真实 daemon 驱动真实 CLI：默认不运行、无 main 时降级、恢复后摘要与显式预览一致、分叉清除计划摘要、丢失锁不创建；拒绝 RPC 启动/批准和脱离父进程的私有模式。单元测试另外覆盖结构化摘要拒绝、输出上限与超时回收。账本、Claim 与 Git 主干没有被观察器修改。

## 批次 B：同一主干的显式多计划 T3 续跑

- `--t3-autosweep` 保持原版单计划配置。版本 2 用 `plan_sha256s` 替代单个 `plan_sha256`，接收 1–8 个互不重复的审批摘要；其余资源、可执行程序与私有配置规则保持不变。
- 每次 daemon 启动只选定一个可验证的直接 main，锁定启动时已应用且匹配该提交的计划集合；按配置顺序每轮给各未完成计划一页机会。后续补批需要下次启动。一个计划失败或未精确推进自身修订号时，本次运行不再调度它，其他计划仍可推进；只要某计划被禁用，收尾便不报告全部完成。主干漂移停止该次运行；最多 256 次子进程尝试，不并发运行原生页。
- 每个子进程仍由原 CLI 门禁复核审批、Git 和原生验证。SQLite 迁移从主干唯一改为按计划摘要区分，保留历史审批、回执、事件和页检查点。重启从各自检查点续跑，不自动重置、不重复留存已恢复 Claim 的证明回执。计划列表不构成审批；仍无自动发现→批准的写路径，也不接入当前 DSH 设置。

验收：253 项标准 Rust、50 项插件测试、2 项真实 Lean + Seatbelt 的单/多计划 daemon 测试通过。独立 daemon 测试覆盖无进展和跳号隔离、主干漂移停止、重启按原检查点续跑。实测两个同主干的重叠审批完成时只新增一份 T3 证明回执。旧库迁移回归确认事件序号、审批、检查点和已有规格比较回执不被迁移改写。

## 批次 B：由指定 Lean Claim 的当前 T1 回执选择发现基线

- `t3-discover-claim ROOT BARE_REPO CLAIM_ID` 从指定 Claim 当前的受管 Lean T1 回执选基线，不需传主干或基线 OID。目标必须本地/有效 L5、未标脏，当前回执精确匹配该 Claim 最新受管合入和目标；有受审规格时必须与封存比较匹配。该批次不回退到较老证明、普通 L5 行或 T3 回执；封存 T3 的扩展见后续批次。
- 在 Git 协调锁下复核冻结证明体、合入的唯一父提交、只改批准模块以及验证时有序前置模块的摘要；受审规格从实际 Git 快照核对。在一致的 Git/账本代次上复用 `t3-discover` 逻辑，结果标明 `baseline_scope: claim_t1`、Claim 和回执摘要。基线只对该 Claim 有证据，**不是整个主干已通过验证**；计划仍是只读建议，应用必须另行审签。
- 常驻需显式配置，见下一批次；不改变其他引擎或 T3 复检的基线政策。未接线到 RPC、当前 DSH 或模型。

验收：258 项标准 Rust、50 项插件测试和 1 项真实 Lean + Seatbelt 端到端测试通过。实测复核适配后的精确 T1 基线，前置模块漂移后生成与原预览一致的计划；标脏和当前 T3 回执均拒绝，Git/账本不受发现操作修改。另覆盖缺失、普通未验证、非当前回执、原始摘要或封存比较不匹配。

## 批次 B：常驻使用指定 Lean Claim 的当前 T1 基线

- 同一 `--t3-discovery` 接受私有版本 2 配置：用 `claim_id` 代替版本 1 的 `anchor_module`/`baseline_commit`，不可混用。每次探测重新读该 Claim 的当前精确 T1，复用受管 Git、规格和依赖校验；不会沿用失效基线或退回历史记忆。原固定基线模式不变。
- daemon 仍只接受有界、闭合、受父进程生命线约束的子进程协议。成功摘要只新增 `baseline_scope: claim_t1`，不向 status、tick 或 HUD 传 Claim ID、回执摘要、源码或路径。下一次探测发现 T1 失去资格时会降级为 `unavailable`，清除上次摘要；HUD 标注该基线只由一条 Claim 的 T1 支持。
- v2 是特定 Claim 的显式配置，不是自动挑选最强 Claim；这一批次未使用 T3 回执，封存扩展见下一节。不会审批、标脏或复检。本轮没有配置当前 DSH。

验收：261 项标准 Rust、50 项插件、2 项额外真实 daemon→CLI 测试，以及 1 项真实 Lean + Seatbelt 端到端测试通过。原生实测从当前 T1 选出基线，Git 依赖变更后摘要与显式预览一致；只有随后由操作者显式应用计划才标脏，下一次探测清除摘要。协议回归覆盖版本混用、空值、未知/未验证 Claim、私有字段与失败后恢复。

## 批次 B：沿封存的 Lean T3 复检更新 Claim 发现基线

- 指定 Claim 仍须当前本地/有效 L5 且未标脏。当前 T3 只在最新受管 T1 合入、受审规格与原封存比较、已应用计划、精确复检回执、最新恢复事件及 Claim 工作范围相互吻合时才可作为基线。不接受普通 T3、已过时封存或回退到 T1。
- Git 核对保留了对原 T1 唯一父提交、单模块改动和冻结证明体的验证，另外对 T3 的受审提交验证目标原字节、有序依赖和受审规格。实际比较从该受审提交起算，不把此前已重验的变更再次报成新漂移。
- 一次性命令和常驻 v2 可输出 `claim_t3`，HUD 显示 Claim T3 基线；常驻仍只传有界摘要，不能审批、应用、标脏或复检，也不代表整个主干已验证。未更改当前 DSH 配置。

验收：264 项标准 Rust、50 项插件、2 项额外 daemon→CLI 和 2 项真实 Lean + Seatbelt 测试通过。原生 daemon 实测 T1 → 标脏不可用 → 显式复检后变为封存 T3；另一次原生测试在 T3 之后改动前置模块，发现计划与从 T3 受审提交生成的显式预览一致。便携回归覆盖普通 T3、过期/重挂回执、伪造恢复或审批数据均不提供基线。

## 批次 B：单 Claim 的离线事件重放

- `ai4math claim-replay ROOT CLAIM [THROUGH_EVENT_SEQ]` 从业主控制的已有 SQLite 以只读方式获取同一事务中的全局高水位，按序重建该 Claim 在指定全局事件序号（含边界）时的投影；省略序号表示本次读取的高水位，序号 0 代表事件前。尚未创建或未知 Claim 返回 `claim: null`，不把现在的数据库行补到历史中。按事件序号截断的中间视图可以位于同一写事务的本地与有效更新之间，不表示 SQLite 曾单独提交该中间状态。
- 仅重放创建元数据、本地/有效信任、T3 标脏及其根提交、路线耗尽与重开。核对已知前态，拒绝不支持的 Claim 事件、损坏输入或超出上限（每 Claim 至多接受 8192 条源事件，解析单项最多 1 MiB、合计最多 8 MiB），不返回部分投影。读时不打开 Git、不运行引擎、不改写事件、Claim、回执或 Wiki，无 RPC/HUD 接线。
- 这是审计视图，不是全库恢复，也不重新认证历史回执：转换以保存的事件为准，不声称检测绕过存储保护的所有历史删改。计划、依赖图、任务、P0/Pivot 和回执的独立重放仍未完成。

验收：272 项标准 Rust、50 项插件测试通过。回归覆盖事件前、创建和本地/有效更新之间的截点，重复 T3 标脏、依赖有效状态、路线耗尽后新增替代方案重开；关闭重开后可复现，故意改动当前 Claim 行不影响历史。损坏或未知状态事件、无前态、越界序号和超过源行上限均拒绝，CLI 不创建或修改账本，daemon 无此 RPC。

## 批次 B：单 ProofPlan 及依赖边的离线事件重放

- `ai4math plan-replay ROOT PLAN [THROUGH_EVENT_SEQ]` 与 `claim-replay` 共用只读账本快照、包含式序号边界及同一资源上限。可以重建该计划创建时的目标、路线和有类型依赖边，以及各时点的有效信任、直接失效和传递阻塞。未知或尚未创建的计划返回 `plan: null`。
- 直接被依赖证伪时，持久状态设为 `INVALIDATED_BY_REFUTATION` 并将计划有效信任降至 L0；传递性 `BLOCKED_BY_REFUTATION` 是可解除的显示状态。若同一事务同时写下失效和解除阻塞，中间事件截点保留两个独立状态，以事件序列为准。
- 重放只读已记录的计划事件，不从当前图补齐或重新计算 AND-OR，也不把历史阻塞原因当成新的见证。未写入计划事件的 ADR 归档、软剪枝等不在本批次覆盖内；完整一致性审计、其余账本和真实 DSH 接线仍待完成。

验收：280 项标准 Rust、50 项插件及 1 项真实 Lean + Seatbelt 证伪测试通过。原生复验核对历史路线在证伪前仍活跃、内核见证留存后只直接失效依赖证伪项的路线，备选路线仍有效。便携用例覆盖传递阻塞原因变更、替代路线重开、同事务中间边界、非法和未知事件、行数与字节上限，以及只读重开后与当前投影核对。

## 批次 B：同一账本截点的 AND-OR 整图重放

- `ai4math graph-replay ROOT [THROUGH_EVENT_SEQ]` 使用单一只读 SQLite 快照，从全局事件前缀依序重放 Claim 与 ProofPlan。结果包含统一截点、高水位、两类应用的事件数、依赖边数，并按 ID 确定性输出。按 0 查询得到空图；不从当前图表补齐，不调用 Git 或数学引擎。
- 创建计划时校验目标和依赖 Claim 已存在；校验记录中的阻塞/证伪原因引用，并在发布前检查完整依赖图无环（包括失效路线）。缺失、前向引用、已知事件与实体类型错配、不支持的状态事件、非法截点、超限和环路均拒绝输出部分图。
- 上限：全局前缀最多 32768 条源事件（含被略过的事件），最多 1024 个 Claim/计划节点、8192 条边，单个状态事件体最多 1 MiB，事件体与标脏提交字段合计最多 16 MiB，相关事件类型和实体 ID 各限 4096 字节。这是快照审计，不从证据重新聚合 AND-OR 信任，也不和当前物化图做自动比对或修复；不重放回执、计划归档及其他账本，不增加 RPC/HUD 或改动当前 DSH。

验收：293 项标准 Rust、50 项插件测试通过。两个 SQLite 连接的 WAL 回归验证：重放期间追加事件不会改变本次高水位或输出，下次重放才可见。历史图与同序号单项投影逐项相符；缺失、前向和错误类型引用、含失效边的环路、行数/边数/节点数/字节上限均有拒绝回归。

## 批次 B：同一账本快照的当前图投影对账

- 操作者命令 `ai4math graph-audit ROOT` 在同一只读 SQLite 事务中，从最新事件高水位重放整图，再比较当前物化 Claim/ProofPlan、依赖边及可对账的路线状态。覆盖元数据、信任与标脏、持久计划状态、阻塞原因、缺行和额外行。依赖按边集合比较，不把账本中的创建顺序误当成 SQL 行顺序。
- 一致时退出 0；可比较但不一致时输出完整的诊断 JSON，退出非零。只列实体类型、ID、字段和差异类别，不列字段值。`missing_projection` 表示账本有而物化表无，`unexpected_projection` 表示物化表有而账本无；按实体类型、ID 和字段排序，最多展示 128 条并保留总数与截断标记。账本不完整、查询超过上限或无法比较时失败，不返回部分对账报告，也不迁移缺表的旧库。物化侧同样最多 1024 个 Claim/计划节点、8192 条边，另有路线表行数、实体 ID 和合计 16 MiB 文本读取预算。
- 不自动修复或改写账本，不从见证重新计算信任，不核验证据、Git 或当前 DSH 部署。只比较双方共同可表示的字段：Claim 路线耗尽只对账布尔状态，历史直接证伪原因不要求存在于物化 ProofPlan；不增加 RPC 或 HUD。

验收：300 项标准 Rust、50 项插件测试通过。WAL 回归在第二连接提交后证明重放与物化读取仍使用原快照；对账夹具验证信任、标脏、依赖和路线状态的正常/漂移行为，报告不泄漏字段值也不修复数据。旧账本拒绝、130 条差异保留总数仅展示前 128、物化侧 1024/1025 条边界，以及 CLI 退出规则均通过。

## 批次 B：账本整图的 AND-OR 语义检查

- `ai4math graph-check ROOT` 只在同一账本快照的最新已提交事件边界检查，不拿任意历史截点冒充完整写事务。用重放记录的本地信任、标脏、路线和依赖边，在内存拓扑顺序中重算 AND/OR 有效信任、标脏传播、路线耗尽和传递阻塞；检查直接失效是否有已记录的直接硬证伪依赖，也检查该失效是否被遗漏。
- 重算不把已记录的有效信任、阻塞或耗尽结果用作输入。即使账本和物化图以同样方式写错、`graph-audit` 显示相符，也能发现推导矛盾。与对账一样，语义矛盾时输出最多 128 条确定排序的诊断并返回非零，完整保留总数和截断标记，不公开字段值。账本不合法或超出整图重放上限时不输出部分结果。
- 不读物化投影、不改状态、不执行 Lean/Git、也不认证账本中的本地证据或见证。这是针对已有图规则的只读一致性诊断，不提供历史事务边界恢复、不覆盖其他账本；无 RPC/HUD 接线或当前 DSH 变更。

验收：305 项标准 Rust、50 项插件及 1 项真实 Lean + Seatbelt 证伪测试通过。便携夹具故意令账本和 SQL 同时写入错误的 AND/OR 信任、标脏和阻塞；`graph-audit` 相符但 `graph-check` 拒绝。真实 Lean 见证产生的直接失效与备选路线，检查前后均一致且不改账本；另覆盖软证伪失分仍传播标脏、遗漏/伪造直接失效与有界诊断。

## 批次 B：沿快进主干续跑已应用的 T3 计划

- 私有 `--t3-autosweep` 版本 3 需显式选择 `"selection":"applied_current_main"`，不得混入版本 1/2 的计划摘要字段。监督器按配置间隔读取受管裸库的直接 `main`，并从 SQLite 选择其**已经应用**的计划（每代最多 8 个，含完成项；超过上限拒绝继续而不派发部分列表）。空闲或某代完成后继续等待，晚到的审批可被发现；不把 `t3-discovery` 建议当审批。
- 仅跟随实际 Git 父链中的快进，拒绝观察到的回退、分叉和不可用主干；不用本地 graft/replace 或提交图缓存来认可祖先关系。不声称检测两次观测之间又被撤回的短暂变化。派发前再次确认主干，现有 CLI 子进程仍在受管门禁内复核精确提交、审批与工作范围；有挂起合入/Pivot 日志时暂停派发。
- 同代按摘要轮转，一次只跑一页、最多一个原生验证子进程。全部代共享每进程 256 次派发、最多 32 次观察到的快进；失败或未精确推进的摘要本次运行不重试。沿用持久检查点，不覆盖审批、不重置、不重新申请，不增加 RPC 或改动当前 DSH 配置。版本 1/2 原语义保留。

验收：315 项标准 Rust、50 项插件及 4 项真实 Lean + Seatbelt 测试通过。原生 v3 在同一 daemon 中对两个快进代分别等待人工应用，恰好各新增一次 T3 回执；回退后停止派发，RPC 仍可读。便携夹具覆盖晚到审批、单次故障隔离、在旧页结束后因挂起日志暂停、清理日志后的快进续跑、旧未完成检查点保留和分叉拒绝；Git 回归验证本地伪祖先不能突破快进检查。

## 批次 C：固定受管 Lean 主干的 T2 冷构建观察

- 操作者 `ai4math t2-build ROOT BARE_REPO ANCHOR_MODULE REVIEWED_MAIN NATIVE_LEAN` 只检查明确审核且仍是当前主干的受管 Git 提交。按已有锁顺序取得项目原生租约、再取 Git 协调锁；要求账本绑定、无挂起合入/Pivot，并在返回前重查 Git 与 SQLite 代次。任务忙、配置不可信或输入漂移即拒绝结果。
- 按 Git 清单顺序在新的 Seatbelt 洁净室编译该快照的全部 Lean 模块。报告全量有序源码摘要、编译器摘要、各模块路径及源码摘要和墙钟耗时。每个模块有独立临时目录，仅可只读导入已构建的前置产物；总构建预算 120 秒、单模块至多 30 秒，接纳的产物限每模块 32 MiB、合计 128 MiB（不是整个临时盘配额）。无增量缓存，不保留产物或写 Golden cache。项目门禁及临时工作目录不代表新证明授权。
- 这一切片是同步、只读的性能基线采样，`advisory_only: true`、`external_checker_passed: null`；不读取/提升 Claim 信任，不追加验证回执，不把编译成功等同于内核证据。不测 heartbeat、不判定性能回归、不接独立内核、周期 daemon、RPC 或当前 DSH。

验收：320 项标准 Rust、50 项插件及 2 项真实 Lean + Seatbelt 测试通过。原生覆盖有序前置产物可导入、逆序/非法下游证明失败、编译器与模块摘要核对；成功与失败均不修改 Git 对象/引用、SQLite 主库/WAL 或 Claim 权威。另覆盖忙租约、仓库和审核提交错配、产物类型及预算。

## 批次 C：离线 T2 耗时基线与候选比较

- `ai4math t2-compare BASELINE_JSON CANDIDATE_JSON MIN_DELTA_MS MIN_INCREASE_PERCENT` 读取两个常规 JSON 文件（各至多 1 MiB），每组收集同一审核提交、编译器与有序源码快照的 3、5、7 或 9 次 `t2-build` 报告，写成 JSON 数组。两组须使用同一编译器摘要、同宽 Git 提交 ID 及同名、同序模块；不同提交允许源码变更，并逐项标注。拒绝不同模块布局、不一致摘要、不完整或带授权字段的报告。
- 逐模块和全仓比较墙钟中位数；候选确实变慢且同时达到调用者给定的毫秒（1–120000）和相对百分比（1–1000）阈值才标记，等于阈值也算。基线中位数为 0 的项明确标记 `baseline_zero`，不用不定义的比例判定；其他项仍可正常比较。发现达到双阈值的变慢时完整输出建议 JSON 并退出非零，输入不可比或无效则失败且无结果。
- 样本是操作者提交的普通、非签名观测；检查形状与一致性，不证明独立采样或同机。实际基线应在同主机、固定工具链安装、相近负载下复采，不把计时变化归因于特定源码或视为形式化证据。命令不运行引擎，不打开项目、不写事件或信任；独立内核、heartbeat 和周期调度仍未接入。

验收：325 项标准 Rust、50 项插件及 1 项真实 Lean + Seatbelt 工作流通过。原生夹具从两个真实提交分别采集三次构建，确认源码差异标记与无 Git/SQLite 权威变更，不以耗时抖动断言性能优劣。确定性样例另验证离群值、模块/全仓双阈值、零基线、不兼容样本和只读输入上限。

## 批次 C：账本头 T2 信任债扫描

- 操作者运行 `ai4math t2-trust-debt ROOT`。扫描只读打开项目 SQLite，在**同一读取事务**的账本头回放 Claim/ProofPlan，重算 AND–OR 语义并与当前图投影对账。任一审计不一致、账本残缺或既有回放预算超限时退出非零，不输出部分债务清单。不需 Git 或原生编译器。
- 区分 `oracle`（L4a）和 `cross_engine`（L4b），各按三类角色计数：`direct` 是当前 local 标签，`prior_tainted` 是已标脏且保留的旧标签，`effective_ceiling` 是当前 effective 标签恰为 L4。T3 不一定改写 local 标签，因此同一 Claim 可有多行；各行显示本地/有效标脏，不能把 `direct` 当成仍新鲜的证据。有效标签不代表指出了债务根源，也不会枚举所有可能依赖。
- 结果只含账本序号、计数及按 Claim ID 和固定角色顺序排的至多 128 项；超限注明截断，计数仍精确，不输出私有陈述或证明。债务是待审提示，不验证 Oracle 或 Bridge 票据，不读取旧版 `oracle_debt` 统计位，也不自动登记、清偿、改写信任或产生回执；不接 daemon、HUD 或现用 DSH。

验收：329 项标准 Rust 和 50 项插件测试通过。状态机夹具验证 T3 标脏后角色重叠、替代路线使有效等级从 L4a 经 L4b 到 L5、两种图漂移拒绝及截断时计数精确；L4b、L5 标签只在该测试中作为留存记录构造，不是新授权。真实 CLI 夹具验证拒绝共享运行目录、隐藏声明正文且不修改 SQLite 主库/WAL。本轮不调用原生引擎。

## 批次 C：同代次托管 T2 审计

- 新增操作者命令 `ai4math t2-audit ROOT BARE_REPO ANCHOR_MODULE REVIEWED_MAIN NATIVE_LEAN`。在既有原生执行门禁和 Git 协调锁下，先扫描通过双校验的账本头信任债，再对固定审核主干做 Lean 冷构建；任一步失败不输出半份报告。
- 仅当扫描序号匹配初始账本代次、构建前后主干和账本代次复查通过、且无挂起合入或 Pivot 时，输出 `ai4math.t2.audit.v1`，其中 `clean_build` 保持原 `t2-build` 样本结构（可用于离线比较），`trust_debt` 保持原扫描结构。原两个命令仍可独立运行，不给 `t2-build` 加债务前置条件。
- 这是单次、只读建议；不比较耗时基线，不启动驻留或计划任务，不创建证据、回执或授权，不更新现用 DSH。跨 Git 与 SQLite 使用协作锁和前后读检，不声称能检测未观测到的外部短暂改写，或在报告发出后维持主干不变。

验收：332 项标准 Rust、50 项插件及 1 项真实 Lean + Seatbelt 工作流通过。原生成功报告中冷构建、债务、外层共用账本代次，保留原 L4a 标签且不写 Git/SQLite；下游证明错误不回传半份报告。确定性夹具分别移动扫描后的账本、构建中的 Git 或账本，验证阻止启动或丢弃结果；另验证坏账本在编译器解析前被拒绝，而独立冷构建仍保留原语义。

## 批次 C：固定审核主干的驻留 T2 采样

- `ai4mathd --project ROOT --t2-audit PATH` 是私有显式启动选项；不带选项不运行或发布 T2。配置版本 1，固定 `reviewed_main` 的完整 Git 对象 ID，并指定 `cli_executable`、`native_executable`、`bare_repo`、`anchor_module`、`interval_ms`（1000–3600000）、`audit_timeout_ms`（100–180000）和 `max_runs`（1–64）。没有自动改跟或批准新主干。
- 同进程最多一个 T2 子进程，每次尝试结束后再等一个间隔，计满后停止。待处理合入/Pivot 时不派发；观察到主干不是经审的直接引用或不可用时停止，不沿用旧结果。子进程仍使用项目原生租约、Git 协调锁和同代次双校验，且在专用进程组中受超时和 daemon 生命线约束；配置和诊断细节不经 RPC 公开。
- `status` 与 `vital_signs.tick` 中的 `t2_audit` 只给尝试状态/次数，成功状态另附通过复核的脱敏建议摘要；包含审核提交、账本序号、耗时、模块数与债务条数，不含路径、Claim、源码或编译日志。它是带世代戳的观测而非实时权威，达到上限后仍可读。没有写事件、回执、周期基线策略、heartbeat 或独立内核；未部署到现用 DSH。

验收：344 项标准 Rust、50 项插件及 1 项真实 Lean + Seatbelt 工作流通过。真实 daemon 在固定主干上两次串行运行并按预算停止，RPC 仅返回 7 字段摘要，Git、SQLite 主库/WAL 和回执不变；便携夹具验证未启用、非法配置/摘要、等待挂起日志且不扣预算、主干和账本漂移拒绝、超时回收，以及停止后的可读状态。

## 批次 C/F：驻留 T2 脱敏 HUD 与已安装 DSH 传输

- HUD 仅在与 daemon 的连接处于 live 且启用驻留 T2 时展示该项。只接受固定状态、精确摘要字段、有界计数与规范 Git ID；不采信或展示子进程诊断。只有摘要的账本序号与状态快照匹配时才展示耗时、模块和债务条数。旧/超前摘要不展示这些数字，重连时不沿用旧项目状态。
- 起跑、执行、等待、不可用、审核主干变更与次数耗尽是运行状态，不是证明结论；成功摘要也始终标为建议。HUD 不发起审计，不添加 RPC，不揭示配置、工作路径、Claim 或证据正文。
- 独立安装版验收入口见 [Phase 2–3 复跑命令](phase-2-3.md#L35)。该夹具在临时工作目录启动真实 daemon 和 native Lean，经过本机安装版 Cordis/Typert Gateway 检查 Owner 状态、流式转发和 Guest 拒绝。浏览器渲染、乱序和重连由独立 VM 夹具覆盖；没有把插件挂到当前 Web/Electron GUI，也未修改 DSH 设置。

验收：344 项标准 Rust 和 51 项插件测试通过，并通过两个安装版 SDK 夹具。新增夹具实测真实 Lean 摘要经 Rust UDS 到达 Owner 单次状态及推流，两者一致；Guest 被拒绝，没有启动审计的 RPC，Git 主干和账本序号不变。旧夹具同时复验 P0 推流、留存 Fact Pack 和非托管 Wiki 血缘响应。

## 批次 C：固定 Lean 导出的独立 Nanoda 校验

- 操作者运行 `ai4math t2-check-export EXPORT_NDJSON EXPORT_SHA256 NATIVE_NANODA CHECKER_SHA256`。两项 SHA-256 需分别和导出文件及受信任原生 Nanoda 一致；应从可信分发核对 checker 摘要，不能把自行计算的摘要当作来源证明。支持含定理记录的 Lean NDJSON 3.1.0；报告里的 Lean Git 标识来自输入元数据，并非另行证实的编译器来源。格式参见 [Lean 外部内核参考](https://lean-lang.org/doc/reference/latest/External-Kernels/)。
- Broker 只把固定字节及其生成的只读配置交给独立 Rust 内核，经 Seatbelt 运行，导出上限 16 MiB、单行上限 1 MiB、checker 文件上限 64 MiB、执行预算 30 秒；诊断同样有界。只允许 `propext`、`Classical.choice`、`Quot.sound`，出现其他公理即拒绝；工具、格式不匹配或超限不回传成功报告；未能完成检查不等于出现数学反例。
- 结果只表示 **Nanoda 接受该摘要标识的导出**。这是离线建议，不验证导出是否涵盖整个项目、是否确来自 Git 或声明冻结边界，不读写项目账本、回执、信任或当前 DSH。尚未把新生成导出接到托管冷构建、周期采样及发布门禁。

验收：351 项标准 Rust、51 项插件及 1 项真实 Lean 4.35 + Nanoda + Seatbelt 工作流通过。[原生夹具](<../crates/ai4math-cli/tests/t2_export_native.rs>) 现场生成正确导出，验证改坏证据项与非许可公理都被独立 checker 拒绝；还验证双摘要错配、无成功半报及不初始化项目。

## 批次 C：从托管 Lean 快照复核指定定理

- 操作者运行 `ai4math t2-check-managed ROOT BARE_REPO TARGET_MODULE DECLARATION REVIEWED_MAIN NATIVE_LEAN EXPORTER_SHA256 CHECKER_SHA256`。指定跟踪的根级 `.lean` 文件及带命名空间的 ASCII 定理名；复用项目原生租约、Git 协调锁、受管绑定和构建前后的主干/账本门禁。拒绝挂起事务、错误提交或输入漂移。
- 按清单洁净编译完整快照；只从目标及已构建前置模块的临时对象导出指定定理及其依赖。先核对声明确为目标模块自己定义的定理，再以原生 Lean 同安装的、显式固定摘要的 `leanexport`/`nanoda_bin` 做独立复核。导出 stdout 独立有界采集，不与诊断混合，不向调用者发布原始导出或缓存。
- 建议报告包含审核提交、账本序号、目标模块/定理、有序全仓构建数据及独立 checker 结果。全仓编译不是逐声明独立复核，也不拒绝所有无关公理或编译警告；Nanoda 仅覆盖选中定理的导出闭包。失败不输出部分报告，不修改 Git/SQLite 或验证回执，不接 daemon、发布授信和现用 DSH。

验收：356 项标准 Rust、51 项插件及 1 项真实 Lean 4.35 → leanexport → Nanoda + Seatbelt 工作流通过。真实多模块夹具核对有序源码摘要、目标定理与导出闭包，拒绝误用导入定理、普通定义、非许可公理、摘要错配及下游类型错误。成功失败均不修改 Git/SQLite 或 Claim 权威；便携测试另覆盖错仓库、旧提交、忙租约、输出上限及旧 CLI 合同。

## 批次 C：一次洁净构建的受管定理批量复核

- `ai4math t2-check-batch ROOT BARE_REPO ANCHOR_MODULE REVIEWED_MAIN NATIVE_LEAN EXPORTER_SHA256 CHECKER_SHA256 TARGETS_JSON` 接受不超过 8 KiB 的普通 JSON 文件，内容是 1–8 个精确对象，每项指定 `target_module` 与 `declaration`（如 `[{"target_module":"Proof.lean","declaration":"Example.identity"}]`）。拒绝重复声明、未知模块、无效清单；不改变旧单定理命令。
- 一次按 Git 清单编译被审核的固定快照。每项仍在其目标模块刚形成的对象前缀上核查本地公开声明、独立导出并用固定摘要的 Nanoda 复核；建议报告按操作者清单顺序返回，不因内部编译顺序改写。独立复核使用共享 120 秒预算，每进程最多 30 秒且不超过剩余独立预算；构建计时不含独立复核。
- 任何目标、下游编译或最后的 Git/SQLite 代次复查失败都不输出部分结果。既不枚举全项目声明也不安装发布门禁；不写证明信任、回执或现用 DSH，不增加 daemon RPC。

验收：361 项标准 Rust、51 项插件及 2 项真实 Lean → Nanoda + Seatbelt 工作流通过。双模块夹具验证构建顺序与报告顺序不同、逐项导出摘要与直接导出一致；即使第一项已通过，后续归属错误或非许可公理也不会产生部分结果。成功与拒绝均不改变 Git、SQLite、信任及回执，单项命令兼容。

## 批次 C：已审核 Lean 里程碑放行预检

- 在受信任终端运行 `ai4math t2-release-check ROOT BARE_REPO ANCHOR_MODULE REVIEWED_MAIN NATIVE_LEAN POLICY_SHA256 COMPILER_SHA256 EXPORTER_SHA256 CHECKER_SHA256`。`REVIEWED_MAIN` 必须等于绑定 bare repo 的当前 `main`；发布清单是该提交根目录内固定的 `ai4math-t2-release.json`，采用上节同一 1–8 项数组、8 KiB 上限，只接受常规非可执行 Git blob。工作树同名文件和外部临时清单不会被读取。运维端从独立审核记录获得提交、清单原始字节及三件原生工具摘要，不能只接受被审核输入自报的值。
- 在同一项目原生租约、Git 协调锁及账本代次内，复核经语义校验的 L4a/L4b 信任债计数必须为零，随后冷构建完整快照并独立复核所有清单条目。最后重查 Git/SQLite 代次；只有全部满足才返回 `gate_passed: true` 与提交、策略摘要、账本事件序号和逐项复核结果。普通证明合入和 Pivot ADR 提交不能顺带改写发布策略。
- 这是手动、只读的里程碑预检，不创建发布、标签、授信、签名或证明回执。零 L4 发现不是零未证明 Claim，也不审计历史回执或旧 Oracle 标志；仍需审核人确认清单覆盖发布要求。周期接入、全仓声明覆盖、发布时的重新确认及实际发布端强制门禁均未接线。

验收：367 项标准 Rust、51 项插件及 3 项真实 Lean → Nanoda + Seatbelt 工作流通过。验证真实预检放行与错误摘要、缺少 Git 清单、L4a 信任债、旧提交及非许可公理拒绝；Git 回归验证非普通文件/超限清单和证明或 ADR 提交夹带改写均被拒绝。拒绝不输出部分通过结果；正负原生用例均保持 Git、SQLite、Claim 信任与回执不变。

## 批次 C/F：固定发行清单的驻留独立复核

- `--t2-audit PATH` 配置版本 2 沿用版本 1 的固定审核提交、路径、间隔和 1–64 次预算，并要求 `policy_sha256`、`compiler_sha256`、`exporter_sha256`、`checker_sha256` 四个小写 SHA-256，整次超时可配置为 100–270000 ms。版本 1 保留兼容，不会隐式启用独立检查。无配置依然不派发；待处理事务、主干/账本漂移和串行限制沿用既有门禁。
- 每轮运行同一只读里程碑预检；Lean 构建、声明归属检查、导出和 Nanoda 均受专用进程组、daemon 生命线和超时约束。清单、工具、L4 或独立 checker 任一拒绝只显示通用不可用；不回退到版本 1 的冷构建结果。后台任务不发布、不写账本/信任和回执，也不增加 RPC。
- 成功的 `ai4math.t2.resident-sample.v2` 只在原摘要上增加 1–8 个独立检查的数量 `independent_checks`；守护进程校验私有策略摘要后在对外发布前删除它。HUD 只在账本代次匹配时显示数量，并标为建议。仍是显式清单覆盖，非全仓声明扫描，不自动跟踪新主干或改变发布端。

验收：371 项标准 Rust、51 项插件、2 项真实 resident Lean/Nanoda 工作流及安装版 DSH SDK 的 v1/v2 传输各 1 项通过。v2 实测正确策略经状态与推流发布相同计数，错误策略只返回不可用，Git、SQLite/WAL、Claim 和回执不变；便携测试拒绝跨版或含私密字段的样本，HUD 仅展示同代次计数。安装版夹具验证 Owner/Guest 和无启动 RPC；未挂载现用 GUI。

## 批次 C：强制预检的托管本地里程碑标签

- `ai4math t2-release-tag ROOT BARE_REPO ANCHOR_MODULE REVIEWED_MAIN NATIVE_LEAN POLICY_SHA256 COMPILER_SHA256 EXPORTER_SHA256 CHECKER_SHA256 LABEL` 在绑定的 bare repo 中创建 `refs/tags/ai4math-release/LABEL`。该命令重新运行发布预检（固定 Git 清单和工具、零审计 L4、Nanoda 独立复核），不会接收先前 JSON 或驻留 HUD 作为通行证。仅接受受限小写标签名；已有名称不能覆写。
- 完成全量清单复核后仍持有原生和 Git 协调锁；仅在最后写标签时短暂占用 SQLite 写者槽、确认账本代次和无挂起事务。Git 同一引用事务核对直接 `main` 和清单审核提交并仅在目标标签缺失时创建。不会移动主干、回写账本、增加证明信任或回执。标签对象保留提交、账本代次、清单/源码集/工具和逐项目标导出摘要，不记录编译计时。
- 这是本地受管发布入口，不签名、不推送、不部署，也不保护有直接 Git 写权限的人绕过入口。应保留仓库写权限边界。进程若在 Git 提交阶段或打印结果前异常中断，先读取固定命名空间里的标签对象，复核记录代次，不盲目重试。这里没有持久化的跨系统意图日志，不能保证对父进程强杀或主机断电自动恢复；未挂到远端发布端或全仓声明覆盖。

验收：382 项标准 Rust、51 项插件及 4 项真实 Lean → Nanoda + Seatbelt 工作流通过。原生夹具验证创建并解析带摘要的附注标签；重复标签、错误工具摘要、非许可公理不能产生新引用。便携测试覆盖直接/符号引用、最新主干 CAS、账本代次漂移、挂起事务和并发写者；成功不改主干、账本事件、Claim/回执。

## 批次 C：本地里程碑中断后的只读检查

- `ai4math t2-release-inspect ROOT BARE_REPO LABEL [EXPECTED_TAG_OID]` 只从绑定 bare repo 的发布命名空间读取直接附注标签；若之前已记录对象 ID，可再固定该 ID。拒绝缺失、轻量/符号引用、无效对象头和非规范证据。不会自动重试发布或修补标签；无标签也不等于后台发布进程已退出。
- 重新读取标签指向提交的固定清单、按 Git 清单排序的源码集，验证摘要及逐项选择严格匹配。完毕重查标签、主干和 SQLite 代次；账本有挂起 Git 工作、代次漂移或标签记载未来序号时不返回报告。历史标签仍可读，报告分别以 `heads_match` 和 `ledger_matches` 表示当前主干和账本序号是否匹配。
- 只检查留存数据的一致性，不运行 Lean/Nanoda、不需要原生租约，也不会写 Git/账本或创建回执。对象哈希、固定 tagger 字符串和代次相等均不是签名、远端授权、当前证明复核或强杀/断电恢复证明；保有直接 Git 写权限者仍能伪造结构一致的标签。

验收：387 项标准 Rust、51 项插件及 4 项真实 Lean → Nanoda + Seatbelt 工作流通过。原生标签在占用原生租约时仍可检查；固定对象 ID 与无固定值读回一致，错误 ID 拒绝，主干移动后按历史记录返回，均不写 Git/SQLite。便携测试额外覆盖非规范和超限对象、轻量/符号引用、策略/源码摘要错配、顺序变化及账本过去/未来序号。

## 批次 C：Lean 指定模块的原生 heartbeat 采样

- `ai4math t2-heartbeats ROOT BARE_REPO TARGET_MODULE REVIEWED_MAIN NATIVE_LEAN COMPILER_SHA256` 固定绑定仓库当前审核主干和编译器 SHA-256。沿用 Git 协调锁、项目原生租约和 SQLite 代次校验；完整冷构建按清单顺序进行，目标模块刚构建后在同一有界对象前缀上另做计数观测。下游编译失败或代次漂移不回传半份报告。
- 使用 Lean 前端在单独 Seatbelt 进程中观测当前线程对非终止顶层命令的解析和 elaboration，以 `IO.getNumHeartbeats` 差值记录命令数、每命令最大值和合计（原始分配计数；`maxHeartbeats` 选项的单位是千次分配）。计数不含 import header、单独的语法预检、异步任务或原始产物写出，也不混入原冷构建耗时。应在同安装、同范围和相近选项下复采，不把值解释为墙钟或所有线程的用量。
- 这是人工触发的建议观测，不是强制 heartbeat 阈值、全仓累计、回归判决或内核证明。被审核源的元程序仍可影响运行时计数；不把它视为防伪测量。不写 Git、账本或回执，未接入周期调度、自动基线或 HUD。

验收：391 项标准 Rust、51 项插件及 1 项真实 Lean 4.35 + Seatbelt 工作流通过。原生三模块夹具验证非字典序 import、同输入重复计数相同、较重证明使最大值和合计均增长超过 10 倍；错误编译器摘要、失效提交及下游类型错误均无输出。实测 Git/SQLite、Claim 信任与回执不变；独立脚手架探针还拒绝异常计数、提前终止、非法语法及显式启用异步。

## 批次 C：离线 Lean heartbeat 基线比较

- `ai4math t2-compare-heartbeats BASELINE_JSON CANDIDATE_JSON MIN_DELTA_HEARTBEATS_RAW MIN_INCREASE_PERCENT` 各读一个不超过 1 MiB 的常规文件，内容为同一组 3、5、7 或 9 份 `t2-heartbeats` 样本。组内固定提交、编译器、模块顺序/摘要、目标及命令数；两组必须使用同一编译器和目标模块，模块路径及顺序相同，若目标前缀源码摘要完全相同则命令数也须匹配。报告显式标出目标本身及其构建前缀是否变化，并分别给出两组命令数。
- 分别比较每命令最大值和全命令合计的中位数。任一指标增加且同时达到正整数绝对增加门限和百分比门限即标记回归；绝对阈值上限为 2^53−1，百分比上限为 1000；两门限都含等号，计算使用原始计数，不四舍五入。零基线单列为 `baseline_zero`，不作为相对回归；回归返回 JSON 并非零退出，格式/范围/可比性错误无 JSON。
- 仅对操作者提交的未签名样本做形状、内部一致性和阈值计算，不证明采样来自同一安装依赖、同一宿主，也不验证样本不是复制，也不把样本中的账本序号当作已查验状态。命令数变化时的合计增长不等于单命令退化。未打开项目、运行引擎或修改账本；未接入自动采集基线、周期接线、HUD 或 P0。

验收：399 项标准 Rust、51 项插件及 1 项真实 Lean + Seatbelt 工作流通过。原生夹具从两个审核提交各独立采集三份报告；实际最大值/合计中位数在较重证明中上升，默认夹具门限两项触发，提高绝对门限后均不触发，Git、SQLite 和样本文件不变。便携测试覆盖等号、零基线、极值精确计算、错误路径/版本/顺序、输入上限以及既有墙钟比较兼容性。

## 批次 C：同代次的托管 Lean heartbeat 基线复采

- `ai4math t2-compare-managed-heartbeats ROOT BARE_REPO TARGET_MODULE BASELINE_COMMIT REVIEWED_MAIN NATIVE_LEAN COMPILER_SHA256 MIN_DELTA_HEARTBEATS_RAW MIN_INCREASE_PERCENT` 从固定的 Git 对象读取两个快照。基线必须是当前审核 `main` 的不同祖先；不接受替换引用或 graft 制造的祖先关系，也不接受两边模块顺序不同。两快照源码合计不超过 32 MiB。
- 同一个宿主、编译器和项目租约中，基线与候选交替各冷构建/采样三次，将样本直接交给同一严格中位数比较器；不接收外部样本。原生采样共用 240 秒墙钟截止，每次编译/探针保留原各自上限；不含前置 Git 快照读取。每次采样前及结果返回前重查主干与账本代次；漂移、预算用尽或单次构建失败没有部分报告。
- 有效报告嵌套既有比较结果，超过两个门限仍返回 JSON 和非零退出。只读，不更改 Git、SQLite、回执或信任。基线仍由操作者显式固定，不自动选取或滚动更新；没有接入周期采样、HUD、P0 和发布门禁。

验收：405 项标准 Rust、51 项插件与 2 项真实 Seatbelt 用例通过。Lean 原生夹具在旧基线无法单项复采时仍可读取其固定提交，托管六次采样的中位数和两档门限结果与离线原生数据一致。运行中子进程实测受绝对截止限制；便携夹具验证相同截止、交替顺序，以及 Git 或账本漂移后不再采样。正负用例保持项目和账本不变。

## 批次 C/F：固定基线的驻留 Lean heartbeat 比较

- 显式启用 `ai4mathd --project ROOT --t2-audit PATH` 的配置版本 3。沿用固定审核主干、可信 CLI/Lean 路径、受管 Git 库、根级模块、间隔及限次设置（1–64 次）；要求额外固定 `baseline_commit`、`compiler_sha256`、`min_delta_heartbeats_raw` 和 `min_increase_percent`，配置超时 100–270000 ms。基线必须是审核主干的不同祖先；不会跟随新主干或滚动批准基线。原 v1/v2 参数、调度及摘要仍兼容。
- 每次任务仍做三组成对冷构建/采样，受同一采样截止、项目原生租约和 daemon 专用进程组/生命线约束。漂移、待处理 Git 工作、超时和无效样本不发布比较结论。真实比较命中门限是合法观测，不与子进程失败混淆，也不改变账本与信任。
- 部署端对私有摘要逐字段核对固定对象与代次后，`status` / `vital_signs.tick` 只暴露候选审核提交、代次与最大单命令/合计的阈值布尔值及其 OR；不发布基线对象、编译器指纹、路径、阈值、原始计数或日志。HUD 仅在匹配账本代次时显示命中与否，始终标为建议。不触发 P0、发布门禁或证明动作。

验收：409 项标准 Rust、51 项插件、3 项真实驻留 Lean 工作流和安装版 DSH SDK v1/v2/v3 各 1 项通过。v3 原生夹具实测较重证明命中门限仍正常退出、错编译器固定摘要拒绝；状态和推流严格去除私有字段，Git 与 SQLite/WAL 不变。浏览器夹具实测三种命中组合、未命中、旧/超前代次和畸形数据；当前 GUI 未挂载。

## 批次 C：用本地里程碑标签固定 heartbeat 基线

- `ai4math t2-compare-release-heartbeats ROOT BARE_REPO TARGET_MODULE BASELINE_LABEL BASELINE_TAG_OID REVIEWED_MAIN NATIVE_LEAN COMPILER_SHA256 MIN_DELTA_HEARTBEATS_RAW MIN_INCREASE_PERCENT` 只接受命名空间内的直接附注标签及完整标签对象 ID，不能用剥离后的 commit 代替。复核留存证据的规范编码、历史账本序号、该提交的旧策略和目标顺序、源码集合摘要；其 Lean 编译器指纹须与当前显式固定指纹一致。标签须指向审核主干的不同祖先，比较结束时再次核对标签、审核主干和账本代次。
- 沿用同一项目租约下交替三组成对复采和 240 秒采样截止。报告列出操作者固定的标签对象和历史发布账本序号，再嵌套同一比较结果；命中门限有 JSON 和非零退出，命令失败或对象漂移无 JSON。被测模块不必在历史独立校验选择中；标签核对不重新验证历史证明、不证明标签作者身份，也不授予任何信任。
- 只读，不改标签、主干、账本或回执；不在 daemon 或 DSH RPC 暴露该动作。仍需人工提供标签和对象，不自动发现/滚动，也未将观察用作 P0 或门禁。

验收：410 项标准 Rust、51 项插件、2 项真实 Lean + Seatbelt 工作流通过。原生测试先发布经 Lean/Nanoda 审查的标签，再从未纳入历史独立校验选择的模块复采，验证提高门限可清除回归判定，且 Git、SQLite/WAL 不变；错标签/编译器和失效主干无输出。便携夹具在第六次采样时从锁外删除标签，确认不返回部分结论。既有驻留 v3 明确提交路径也通过了真实回归测试。
