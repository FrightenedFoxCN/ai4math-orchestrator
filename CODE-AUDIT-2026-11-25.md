# ai4math-orchestrator 代码审计

- 审计日期：2026-11-25
- 审计基线：`5222f66`（`main`）
- 范围：Rust workspace（core / store / broker / cli / daemon）、DSH Host/Client 插件、Schema、文档与测试。
- 方式：16 个并行子代理按模块审计，主代理交叉验证关键路径；本轮不修改实现代码。

## 1. 执行摘要

当前实现不是“空壳”，而是一个**可信证据与恢复链已经较扎实、产品自动化闭环尚未完成**的系统：

- Rust 核心、SQLite 事件/物化图、Git→SQLite 恢复、Lean/Agda/Rocq T1、T3、Wiki 冷层、Pivot/RFC、daemon UDS、DSH 只读 HUD 均有实现和大量回归。
- 但自动探索→基建→证明→合入→复盘→人类决策的完整闭环、生产 GUI 写操作、外部模型生产运行、全主机资源治理和 Linux 后端仍未交付。
- 主要结构问题不是“没有模块”，而是**策略重复、上帝模块、Store 反向依赖 Broker、CLI 承载过多应用编排、daemon 通过 CLI 子进程协议耦合**。
- 建议下一阶段先修信任/恢复边界，再做“应用服务层 + ProjectContext + Evidence 边界 + CI/架构守卫”，而不是继续堆命令。

## 2. 验证结果

| 检查 | 结果 |
| --- | --- |
| `cargo fmt --all -- --check` | 通过 |
| `cargo test --workspace --locked` | 448 通过、0 失败；70 个原生/集成用例按设计 ignored |
| `npm --prefix plugins/dsh-ai4math test` | 55/55 通过 |
| `node plugins/dsh-ai4math/client/build.mjs --check` | 通过，bundle 与源码一致 |
| `cargo clippy --workspace --all-targets --locked` | 通过，但有警告 |
| `cargo clippy ... -- -D warnings` | 失败，首个阻塞项为 `Trust::Default` 可派生 |

说明：原生 Lean/Agda/Rocq、Seatbelt、Nanoda、双 SMT 与 installed-DSH 用例未在本次标准测试中重跑；它们需要专门环境变量、原生工具链且 daemon 不能处于外层 Seatbelt 中。

## 3. 高优先级发现

### H1. Store 反向依赖 Broker，持久化层被迫链接执行/沙箱层

- 证据：`crates/ai4math-store/Cargo.toml:7`；`state.rs:5,250,262`；`sediment_recheck.rs:7`；`oracle.rs:4`；`probes.rs:3`；`infra_rfc.rs:4`。
- 影响：Store 使用者会被动编译 Broker；证据类型、SQLite 和 OS 执行边界无法独立演进；Store 单测也需要 Broker 类型。
- 建议：抽取无 IO 的 `ai4math-evidence`（或 `ai4math-contracts`）crate，定义 receipt/comparison/witness/oracle/probe 的不可反序列化证据视图；Broker 负责生成密封证据，Store 只接收证据投影或 trait，不依赖 Broker 实现。

### H2. Git、SQLite、原生租约和锁顺序散落在 CLI 各命令

- 证据：`main.rs:101-105` 定义 `managed_native`；`promotion.rs:464` 起实现 Git 锁；`t3.rs:326-334`、`t2_build.rs:388-389`、`infra_rfc.rs:175-180`、`worker_dispatch.rs:354-357`、`wiki_archive/recheck.rs:76-79` 重复“原生租约→Git 锁→StateStore→复核”流程。
- 影响：新增命令容易漏掉前后代次检查或拿错锁顺序；恢复语义和资源门禁难以统一测试。
- 建议：新增 `ai4math-project` / `ai4math-app`，提供 `ManagedProject::{read, write, native_then_git, reserve_generation}`，集中校验 root、runtime、state 路径、repo 绑定、pending journal、锁顺序和代次复查。CLI 仅保留参数解析和输出。

### H3. CLI 承担过多应用编排，核心文件持续膨胀

- 证据：`main.rs` 1654 行、`promotion.rs` 2327 行、`t3.rs` 735 行；Clippy 报告 `verify_promotion`、`promote_managed*`、`worker_dispatch::run` 等 8–9 参数接口。
- 影响：命令层、Git 事务、Broker 调用、Store 事务混在一起，修改一处需要理解多个领域。
- 建议：拆为 `git-ops`（命令执行、ref/CAS、锁、release tag）、`promotion-service`、`t3-service`、`pivot-service`、`rfc-service`、`wiki-service`；用 `PromotionRequest`、`T3PlanContext`、`PivotSelection` 等结构体替代长参数列表。

### H4. Daemon 与 CLI 二进制/命令行协议耦合

- 证据：`t2_audit.rs:301-365`、`t3_discovery.rs:159-186`、`t3_worker.rs:285-303` 直接拼接 CLI 参数并解析 stdout JSON。
- 影响：CLI 输出结构变化会破坏驻留观察；daemon 测试需要构造假 CLI；无法在进程内注入服务做单测。
- 建议：定义版本化 `ResidentCommand` / `ResidentSummary` DTO 和 `ResidentRunner` trait；CLI 作为受信任适配器，daemon 面向服务接口。若保留进程隔离，仍可用同一 DTO 做 NDJSON，而不是依赖人类 CLI 文本格式。

### H5. 关键长参数列表是耦合信号，而非单纯风格问题

- 证据：Clippy 在 `state/pivot.rs:293,313,387,413,436,460` 连续报告 8–9 参数；`refutation.rs:57`、CLI promotion/worker 接口同样超限。
- 影响：调用方必须知道 repo/base/breaker/reason/action/plan/lesson 等隐含组合规则，容易错配。
- 建议：引入上下文对象和命令对象，例如 `PivotContext { repo, base, fact, breaker, reason }`、`RouteArchiveSelection { action, plan_ids, lesson }`，在类型层禁止非法组合。

### H5a. `module_path=NULL` 的合法 L5 Claim 被按模块 T3 漏标

- 证据：`state.rs:1614-1636` 的 T3 模块映射只读取 `c.module_path`；`validate_target`（1745-1758）却允许 `module_path=NULL` 并从 `formal_target` 回退路径。
- 影响：Store API 或旧库中的 `formal_target=Some("Spec.lean#target")`、`module_path=None` Claim 成为 L5 后，`Spec.lean` 变更不会标脏，旧证明可能继续显示 L5。
- 建议：统一有效模块解析器，T3/receipt/binding/refutation 共用；或迁移旧库并强制 `module_path NOT NULL`。

### H5b. `finish_promotion` 历史重放校验不足且遗留 pending intent

- 证据：`state.rs:590-604` 历史分支只比较 claim/base/patch 便返回，早于完整 intent/receipt 校验；`prepare_promotion`（560-579）不拒绝已记录 commit；新分支才清理 intent（618-626）。
- 影响：篡改 file_path/target_decl/proof_body 的重放可能误报成功；成功后重复 prepare→finish 会遗留 pending intent，持续阻断 promotion/T3/refutation。
- 建议：历史分支核对 recorded promotion、managed recipe、receipt 和完整 intent；完全相同重放在同一事务清 intent，冲突则失败并保留现场。

### H5c. 证伪 Markdown 与 SQLite 非原子，缺少跨资源恢复

- 证据：`state.rs:953-978` 在 SQLite 写事务内先 `materialize` Graveyard Markdown；`refutation.rs:108-131` 立即持久化文件；后续 SQL/commit 失败只回滚数据库。`wiki.rs:471-495` 会索引目录中的 Markdown，不要求对应 DB 行。
- 影响：DB 没有 L2b/回执/P0，却可能出现显示“已证伪”的 Graveyard 文件；当前无 durable intent/outbox 对账。
- 建议：pending-refutation/outbox：先记意图→写文件→提交证据/投影→reconcile；不要盲目删除文件，避免误删已提交证据。

### H5d. promotion/T3/Wiki 的 Git 祖先校验仍受 graft/replace 元数据影响

- 证据：`promotion.rs:1295-1318` 的 `published/currently_published/includes_commit` 使用普通 `merge-base --is-ancestor`；`main.rs:578-584,607-614` 的入账/恢复依赖它。T3 已有 `is_ungrafted_ancestor`（646-669），但 discovery、reviewed-spec、CORE projection、breaker/Pivot 仍有普通祖先调用。
- 影响：`info/grafts` 可虚构 prepared commit→无关 main 的边，使未被物理 main 采纳的候选通过恢复/入账；其他入口也会产生不一致的历史依据。现有测试已证明两种 API 在 graft 下结果相反。
- 建议：所有证明/信任/来源决策统一 `physical_ancestry`；prepared commit 父提交用 raw header 校验；补跨 promotion/discovery/spec/CORE/Pivot/breaker 的 graft 一致性回归。

### H5e. CAS 分叉后缺少安全放弃入口

- 证据：`main.rs:607-614` 在非预期分叉时保留 journal/pin；后续合入被 pending intent 阻断；`main.rs:999-1007` 只有 recover，没有 abandon。
- 影响：并发写者赢得 CAS 后项目可能永久卡 pending，只能强改 Git 或手改 SQLite。
- 建议：新增 `abandon-promotion`，仅当物理 main 不包含 prepared commit 时允许；同事务清 intent、写审计事件、清 pin，绝不强推。

### H5f. 直接文件 promotion 绕过公共熔断/Pivot 预检

- 证据：`main.rs:425-451` 的 breaker/Pivot 检查只在 `reviewed_base=Some` 分支；File 输入走 `None`（467-472）。Store 后续会拒绝 Pivot journal，但 breaker 无统一复查。
- 影响：`promote-managed(File)` 可给 TRIPPED/HALF_OPEN/ARCHIVED 模块合入，而 reviewed/task candidate 拒绝；若非有意 override，则策略可绕过。
- 建议：公共 Pivot 检查移出分支；breaker 统一拒绝，或提供显式 operator-override 并审计。

### H5g. 多个 CLI 写入口绕过私有 SQLite 路径门禁

- 证据：`worker_dispatch.rs:103-126` 有 runtime/state/WAL/SHM 私有检查，但 `t3.rs:93-95,332-333`、`wiki_archive/recheck.rs:68-80,221-226`、`wiki_archive.rs:164-188` 等直接 `StateStore::open[_readonly](root.join(".runtime/state.sqlite"))`；`store/src/lib.rs:19-33` 会跟随符号链接并执行 DDL。
- 影响：误配置、符号链接或 ACL 共享时，T3/Wiki 等入口可能读写错误账本；helper 自身还漏检 journal/ACL。
- 建议：统一 `open_managed_state(root, readonly)`，校验规范 root/runtime、全部 SQLite sidecar、ACL 和 inode；所有 CLI 入口使用，最好持目录 fd 后打开。

### H5h. 标准 Lean T1/T3/回归缺少安装模块遮蔽门禁

- 证据：`broker/lib.rs:395-423` 的 Lean `validate_modules` 只检查路径/大小/重名；`850-858,935-1011` 的 verify/check/regress 直接 build；T2/Hammer/Breaker 却调用 `reject_installation_shadows`。
- 影响：`Init.lean`、`Lean.lean` 或同名安装依赖可能进入 `LEAN_PATH`；若 Lean 接受，会在被替换的核心环境上通过公理检查。具体绕过尚未原生复现，但策略缺口确认存在。
- 建议：把 shadow 拦截下沉到所有 Lean build 公共入口，补原生负例。

### H5i. Witness 与标准 T1/T3 缺少独立 kernel replay

- 证据：`lib.rs:858-903,1008-1010` 用普通 check 构建依赖/目标；`witness.rs:212-226` 复用 build；`witness_audit.lean:4-18` 只查 witness 类型和公理，不重放导入闭包。Breaker/Hammer 已用 stock kernel replay。
- 影响：依赖模块若关闭内核检查或 elaborator 产生坏声明，后续 `#print axioms` 仍可能为空，进而签发 VerifiedCandidate/VerifiedWitness；witness 可直接触发反证/P0。利用尚未原生复现，但 replay 策略缺口确认存在。
- 建议：复用 `replayClosure`，对目标及依赖闭包在 stock kernel env 重放；统一不可覆盖内核选项并扫描依赖 option mutation。

### H6. 失败任务永久占据 FIFO 队首

- 证据：`state/tasks.rs:298-307` 按 `queue_seq` 选首个合格任务；`tasks/runs.rs:174-215` 失败只释放租约并记 `TASK_RUN_FAILED`；`task-scheduler.sql:3-15` 没有 retry/cancel/quarantine 状态；`worker_dispatch.rs:359-366` 要求指定任务必须是 next eligible。
- 影响：持续失败、Claim 失效或从未匹配 Claim 的队首任务会卡死同轨；跨轨依赖可进一步触发双轨停摆。
- 建议：增加 retry/backoff、最大尝试数和显式 cancel/quarantine；失败后轮转或隔离，依赖语义单独维护。

### H7. 重复 daemon 在取得单例锁前可能迁移数据库

- 证据：`runtime.rs:716-729` 先执行 `StateStore::open` / `Wiki::open`（含 DDL/迁移），到 `runtime.rs:738` 才调用 `transport::serve`；目录 `flock` 在 `transport.rs:584-595` 才取得。
- 影响：第二个实例可能在最终报 socket 占用前改写正被旧实例使用的 SQLite，破坏单例启动保证。
- 建议：拆出 `SingletonLease`，启动顺序固定为“取目录锁→打开数据库→绑定 socket→启动后台 runtime”。

### H8. 模型任务租约没有提交宽限期

- 证据：`worker_dispatch/model_pipe.rs:142-148` 的执行 deadline 与租约 TTL 同为 `seconds`；回复后还要取 Git 锁、重算并写 SQLite（162-181）；Store 在 `tasks.rs:415-424` 要求租约仍有效。原生 worker 路径则使用 `seconds+30`。
- 影响：临界回复可能因锁竞争或提交延迟丢失；TTL 内旧进程未退出时也可能发生重领竞争。
- 建议：分离执行 deadline 与持久租约 TTL，为确认/落库预留 grace，并保持 fence 与一次性提交。

### H9. InfraRFC 批准重放被消费者漂移错误阻断

- 证据：`state/infra_rfc.rs:786-793` 在 Approved 幂等返回之前重算 consumer CAS。
- 影响：批准已提交但 ACK 丢失后，任一 Claim 事件都会让原批准命令重试失败，而 inspect 显示已批准。
- 建议：先识别 Approved 精确重放并返回，仅 DRAFT→APPROVED 新转换重算 consumer。

## 4. 未完成部分（按产品影响）

| 范围 | 当前状态与证据 | 下一步 |
| --- | --- | --- |
| 自动编排闭环 | 任务领取、一次性模型 Host、T3 autosweep 均为显式/单次；`runtime.rs:360` 的 workers 恒为 `null`；无自动发现→审批→复检→修补派工 | 先做持久任务执行器与受审批状态机，再接模型 |
| 人机共驾 GUI | `remote-contract.mjs` 只有读 RPC 和 agent-only `proofSubmit`；Client 面板明确 read-only；`PATCH_DEFINITION`、完整 `ARCHIVE_AND_PIVOT`、自动 smell 未实现 | 定义 owner-only 命令 RPC、幂等键、预览/确认/恢复协议 |
| 生产模型运行 | Host adapter 已接 DeepSeek Messages，但需显式 SDK/key/route；未验证公网账号、计费、生产 preset 生命周期 | 生产凭据注入、独立 worker 进程、可观测预算与审计 |
| Linux/非 macOS | `sandbox.rs:407-427`、`task_process.rs:23-27`、`resources.rs:256-259` 明确拒绝，无 bwrap/cgroups 后端 | 先定义 `SandboxBackend` trait，再实现 Linux bwrap + cgroups v2 |
| 全主机资源治理 | `resources.rs` 仅进程内车道和 `vm_stat` 启发式，Heavy 还忽略 pressure；`project_native_gate.rs:8-13` 只约束协作进程，不清理孤儿、不做 RAM/CPU 硬配额 | 进程组/会话监管、cgroup/jobobject、租约心跳与孤儿回收 |
| 多引擎对等 | Lean 有 Comparator/Nanoda/heartbeat/T2；Agda/Rocq 主要 T1，Rocq 无独立 T2/Comparator，见证仅 Lean | 先补 Agda/Rocq 正负原生矩阵和独立检查，再扩 T2 |
| Wiki 全生命周期 | 已有冷层归档、唤醒、观察、显式适配/血缘/CORE 投影；自动沉降、自动唤醒适配、自动重升 CORE 未完成 | 显式状态机 + 审批门 + 可恢复任务，不直接自动化信任升级 |
| 事件重放 | Claim/ProofPlan/AND-OR 图已可重放；receipt、task、P0、Pivot、RFC 等全账本重放未完成 | 按聚合分批补 replay/audit，不追求一次性大重写 |
| 发布 | 本地 annotated tag 和 inspect 已有；无签名、远端发布端强制门禁、周期基线策略 | 签名证据 + CI/CD 强制 preflight + 远端 immutable release |
| 外部通知 | P0 只走 UDS/Typert；Webhook、桌面/邮件等未接 | 保持事件唯一来源，新增 outbox worker 和去重投递 |

## 5. 具体可靠性改进

> 本节按主题记录，编号不代表优先级；P0/P0b 的先后顺序以第 7 节为准。

### M1a. 重复 daemon 的单例锁取得过晚（同 H7）

- 证据：`runtime.rs:716-729` 先打开/迁移 StateStore 与 Wiki，`transport.rs:584-595` 的目录锁在 `serve` 内才取得。
- 影响：第二实例可能在拒绝前迁移正被旧实例使用的 DB。
- 建议：先取 `SingletonLease`，再打开数据库和绑定 socket；补“拒绝前后 DB 字节/schema/事件不变”回归。

### M1b. Pretty legacy Fact Pack 可读但不能重复留存

- 证据：`retained.rs:164-190` 按规范化 JSON 验摘要；`facts.rs:121-125` 的 AlreadyExists 分支却用 `matches_retained_file` 比较原始字节（`retained.rs:220-223`）。
- 影响：`briefing.pull` 或 P0 遇到同 ID pretty 旧包会误报 mismatch，告警游标可能反复回滚。
- 建议：JSON 重放按解析后的规范内容/摘要比较；Markdown 才比较原始字节。

### M1c. Wiki 索引及 sidecar 未拒绝跨账号可写权限

- 证据：`runtime.rs:667-677` 的 `ordinary_path` 不检查 group/other 写位；`runtime.rs:707-729` 允许 `0755` Wiki 根和可写索引文件；`store/src/lib.rs:19-33` 打开 DB 时不收紧权限。
- 影响：其他 UID 可篡改可重建索引，甚至注入带 generation 的伪 managed-core 行，污染检索/血缘展示。
- 建议：索引及 WAL/SHM/journal 私有化为 `0600` 并拒绝 ACL 写授权；Markdown 目录权限与索引权限分开。

### M1d. InfraRFC 只读回放不是单快照

- 证据：`state/infra_rfc.rs:703-715` 调用 `fetch`，后者分多次读取 RFC、events、transitions，无统一读事务。
- 影响：并发合法转换可能产生偶发 projection/chain mismatch；CLI Git 锁目前掩盖了它。
- 建议：`fetch` 整体放入 deferred read transaction，并补并发读者回归。

### M1e. InfraRFC 未持久化可复核证据元数据

- 证据：`state/infra_rfc.rs:877-907,966-998` 只保留 commit/module/definitions 或 API lemmas/receipt，未保留 smoke lemmas、sources/compiler 摘要。
- 影响：历史记录无法回答实际 smoke 定理、源码和编译器，receipt 摘要无法独立重算。
- 建议：持久化有界证据元数据并与 receipt 共同绑定。

### M1f. Graveyard 重放接受硬链接并存在路径替换竞态

- 证据：`refutation.rs:112-127` 已存在分支只检查 `is_file` 和长度，随后 `File::open` 跟随路径；未检查 nlink/uid/权限，也没有 dirfd/O_NOFOLLOW。
- 影响：不可变归档可能别名外部文件或被替换，Wiki 随后索引到被篡改内容。
- 建议：复用 Sediment 的 dirfd/openat/O_NOFOLLOW/inode 校验，要求 nlink=1、owner、非共享可写。

### M1g. Sediment 写入侧不执行读取侧大小上限

- 证据：`wiki/sediment.rs:77-160` render 不检查 2 MiB/8 MiB；`publish_sediment`（345-375）也不检查；read/parse 侧则执行。现有测试甚至固定 2 MiB+1 可 publish、read 失败。
- 影响：可能写入 `created=true` 但无法唤醒/适配的归档。
- 建议：render/publish 前拒绝超限源和最终 Markdown；测试夹具走显式 raw/test-only API。

### M1h. 旧合入缺 recipe 时 Sediment 适配门禁失效

- 证据：`state.rs:1480-1485` 的 LEFT JOIN 在最新 recorded promotion 缺 recipe 时返回 None；`sediment_adaptation.rs:201-204` 和 CLI `wiki_archive/adapt.rs:65-68` 把“无 recipe”解释为“未受管合入”。
- 影响：已有 recorded promotion 的旧目标可被再次审批/派发适配，后续 lineage 又失败。
- 建议：目标门禁用 `EXISTS(recorded_promotions)` 判断是否合入；缺 recipe 应 fail closed，不能当作未证明。

### M1i. 历史 Fact Pack 解码复用实时 DTO

- 证据：`retained.rs:3,31-34,82-96` 直接别名/反序列化 `observation` 的实时类型。
- 影响：实时 DTO 增改字段会改变历史留存契约。
- 建议：独立、带 schema 版本的 Retained DTO；FactPack 编译时映射。

### M1j. `CORE-ADAPT` 标识协议双端重复

- 证据：`state/sediment_lineage.rs:98,475-485` 与 `wiki/managed_core.rs:8-41,67-69` 各自拼接/校验。
- 影响：前缀规则变更需双端同步，容易投影漂移。
- 建议：提取共享 `ManagedCoreId` 值对象，集中构造、解析和迁移。

### M0-1. 编译器 Seatbelt 启动未清理继承 FD

- 证据：`broker/sandbox.rs:283-333` 的 `pre_exec` 只 setsid/RLIMIT；对照 `task_process.rs:246-275,348-359` 明确枚举并设置 CLOEXEC。
- 影响：broker/daemon 继承的非 CLOEXEC DB/socket/敏感 FD 可被编译器原生代码复用。
- 建议：共享 Seatbelt launcher，exec 前关闭/封装除 stdio 和 Rust exec-error channel 外全部 FD。

### M0-2. spec comparator 构建丢失 Supervisor 进程组

- 证据：`lean_spec.rs:179-185` 复用 `breaker_probe.rs:205-223` 的构建器，后者固定 `OwnSession`；只有后续 audit 传入 `tool.sandbox_group`。
- 影响：daemon 超时杀 CLI 组时，spec build 的 Lean 子进程可能留在组外，锁释放后仍占 CPU/临时目录。
- 建议：构建器显式接收 `ProcessGroup` 和绝对 deadline，禁止 helper 隐式 OwnSession。

### M0-3. 编译器哈希只在 Toolchain 构造时核对

- 证据：`broker/lib.rs:99-118` 构造时 canonicalize+hash；`sandbox.rs:238-259` 启动前只重新 canonicalize；回执写缓存 `binary_sha256`。
- 影响：工具链替换后可执行另一编译器却签发旧摘要回执；T2 多模块可能混用编译器。
- 建议：每次启动/模块前后复核 inode/长度/SHA，或执行已批准不可变副本。

### M0-4. runtime read grant 的 symlink 目标可漂移

- 证据：`broker/lib.rs:188-209` 校验 canonical 目标后保存 original；`sandbox.rs:245-259` 每次重新解析并同时授权 requested/new real，未重验 Rocq dylib 限制或 prefix。
- 影响：包升级/重定向可把读取授权扩到另一目录，极端情况形成过宽 read/map-executable。
- 建议：钉住批准时 canonical 身份；alias 必须仍指向同一目标，拒绝根/过宽目录并重验文件类型。

### M0a. Lean frozen prefix 在大小预算前被完整词法化

- 证据：`core/proof.rs:548` 先进入 `template_slot`，后者在 130 行对完整 prefix  lex/clone；rendered-size 检查在 526-531，`checked_source` 594 行再次 lex。
- 影响：损坏/恶意 catalogue 可在 2 MiB 限制生效前造成线性时间/内存消耗。
- 建议：任何 lex/template 解析前统一检查 prefix/suffix/body/rendered 预算。

### M0b. 冻结槽协议与 CLI marker 分割强耦合

- 证据：`core/proof.rs:156-164` 拒绝 namespace/多槽；CLI `main.rs:239-245`、`worker_dispatch.rs:201-207` 又用 `split_once("{{PROOF_BODY}}")` 定义协议。
- 影响：常规 namespace 定理、同文件多目标无法接入，core parser 缺口变成全链路不可用。
- 建议：`FrozenSlotCatalog/resolver` 从可信快照和显式 target id 解析槽；core 只校验单槽不可变性。

### M0c. Result/Claim/Plan 契约缺少跨字段不变量

- 证据：`core/contracts.rs:100-115` 允许 `PROVED+L5+evidence:null`、`REFUTED` 无 witness；`204-286` 的 Claim/Plan 派生字段可任意写，与 Store 图模型重复。
- 影响：公共 wire 契约接受语义无效结果；未来消费者可能把 worker 自述当证明。当前 worker adapter 另走 `proof_verified:false`，尚无提权链。
- 建议：validated constructor/`validate()` 或按 status 分支的类型；Claim/Plan 共享不可变图 DTO/command，派生字段只读。

### M0d. T3 用当前渲染规则反推历史 proof recipe

- 证据：`t3.rs:708-726` 从 baseline 重新 split/render/归一化缩进，只与 `recipe.source_sha256` 比较。
- 影响：历史注释 marker、多 marker 或旧空白规范可能产生假 `InvalidProvenance`，本可洗白的 L5 被延后。
- 建议：recipe 持久化原始 prefix/suffix/body 及摘要，不复现历史字节时依赖当前渲染规则。

### M0e. 受保护目录校验只靠前导点拼写

- 证据：`core/preflight.rs:72-80` 拒绝 `.runtime/.git` 仅因首字符点；`runtime/Main.lean`、`git/Main.lean` 可通过。Store 多处用 `validate_source_path(...).is_ok()` 判断模块合法。
- 影响：公共 API 未实现受保护 scope 意图；当前 broker root-level 限制降低直接可利用性。
- 建议：拆 `AdmissionScope` 与 canonical path 检查，显式拒绝隐藏/受保护组件；Store 只接收 scope token。

### M0f. Heavy Lane 忽略内存压力

- 证据：`broker/resources.rs:150-209` Heavy 分支不读取 pressure；测试明确 `acquire(Heavy, Red)` 成功。各入口还硬编码 `Pressure::Red`。
- 影响：高内存/swap 下 T2/T3/Hammer/Witness 仍启动，可能拖垮宿主。
- 建议：`acquire_heavy(priority)` 集中采样并执行黄/红线；调用方传 P0–P3。

### M0h. Rocq T0 证明错误被误归为不可用

- 证据：`broker/lib.rs:672-698` 的 Lean/Agda 编译失败可分类 Rejected，Rocq 分支固定 false，最终 Unavailable。
- 影响：Rocq 类型错误与工具链/沙箱不可用混淆，调用者无法决定修复还是重试。
- 建议：增加明确拒绝分类或文档化保守策略，并补类型错误/工具链缺失对照回归。

### M0g. 单定理 Managed T2 DTO 丢失目标绑定

- 证据：`t2_managed_export.rs:58-64` 的 `ManagedLeanTheoremAudit` 无 target_module/declaration；436-462 丢弃传入目标，CLI 外部补字段。
- 影响：Broker 公共结果单独序列化时无法证明检查对象。
- 建议：单定理 API 返回 batch DTO 或 DTO 内包含并校验目标。

### M1. Wiki 重建缺少文件与总量上限

- 证据：`wiki.rs:221-227` 对每个 Markdown 直接 `fs::read_to_string`；`markdown_files`（471-495）只递归收集，不限制文件数、单文件大小或总字节。
- 影响：误放入超大文件或大量文件可导致内存/磁盘压力，且事务持有时间不可控。
- 建议：事务外流式有界遍历，限制文件数、递归深度、单文件/总字节；用 dirfd/openat(O_NOFOLLOW|O_NONBLOCK)、fstat、nlink/inode 复核，避免收集后替换为 FIFO 或符号链接。

### M2. T3 配置未强制 JSON object

- 证据：`t2_audit.rs:31-49` 已用 `ObjectOnly` 拒绝数组；`t3_worker.rs:47-64` 和 `t3_discovery.rs:47-62` 直接派生结构体，可能接受位置数组。
- 影响：配置形状不一致，错误配置可能以意外字段顺序被接受。
- 建议：把 `ObjectOnly<T>` 提取到 `resident_paths`，所有驻留配置统一使用。

### M1z. Briefing Fact Pack 累计留存无界

- 证据：`daemon/facts.rs:73-92,112-129` 每个新 hash 永久新增 JSON+Markdown，只做同 ID 去重，无总字节/文件数/LRU。
- 影响：长期 P0/人工 pull 可持续占盘并耗尽文件系统。
- 建议：总配额；保留 ADR/待决复盘引用包，未引用包提供可审计 LRU 或显式清理。

### M2a. 原生租约短暂竞争会终态停掉驻留 T3，并消耗 T2 预算

- 证据：CLI `managed_native` 用非阻塞 `try_heavy`；`t3_worker.rs:285-329` 把所有非零退出压成 `StepFailed`，`follow.rs` 随即禁用计划；`t2_audit.rs:212-221` 在子进程启动前增加 attempts，busy 也消耗预算。
- 影响：同时启用 T2/T3 或外部 CLI 短暂持锁时，一次竞争即可让已批准 T3 停到重启，T2 可能未执行真实审计就耗尽次数。
- 建议：daemon 私有协议返回专用 Busy 退出码/typed outcome；Busy 只退避，不禁用、不扣真实审计预算；最终由 daemon 统一仲裁 HeavyJobScheduler。

### M2b. T3 autosweep 停止状态不进入 daemon/HUD

- 证据：`t3_worker.rs:143-150` 只 spawn 并返回 `()`；Stop 仅 stderr；`runtime.rs:355-373` workers 恒为 null，HUD 仍读 `workers.active`。
- 影响：启动失败、超时、计划禁用、完成都不进入 status/tick，只能查 stderr。
- 建议：`ResidentWorkerHandle/WorkerSnapshot` 发布脱敏 running/idle/stopped、固定停止原因和计数，接入现有遥测，不加控制 RPC。

### M2c. DeepSeek adapter 加载绕过 dshRoot 归属校验

- 证据：`model-task-runner.mjs:88-95` 严格限制 SDK 路径，但 `model-task-deepseek.mjs:42-43` 裸用 `createRequire` 加载 adapter，未做同样校验，随后向其传递真实 key。
- 影响：dshRoot 缺少 adapter 时可能向上加载非预期包/版本，造成凭据交给非预期代码。
- 建议：共享 `loadOwnedSdk()`，resolve/realpath 后强制位于 dshRoot 内。

### M2d. Host 重连前丢弃已入队事件

- 证据：`bridge.mjs:92-96` 的 `EventQueue.end` 无条件清空；断线回调随后重连。已解析但未消费的 tick/breaker 事件会丢。
- 建议：可重试断线先排空队列；overflow/protocol 才清空失败。补“发送后立即关闭、延迟消费”回归。

### M2e. `agent/created` 工具同步错误未捕获

- 证据：`index.mjs:151-162` 的 `schedule()` Promise 无 catch；preset 分支有 warn。
- 影响：注册/释放失败可能产生未处理拒绝，终止 Host 或静默丢失角色工具。
- 建议：统一 catch、诊断和失败清理。

### M2f. 模型运行错误码被统一降级为 assignment

- 证据：`model-task-cli.mjs:187-196` 把所有 runner 错误映射为 `assignment`，丢失 timeout/budget/denied/unavailable/cleanup 分类。
- 影响：调用方无法做正确告警和重试决策。
- 建议：白名单保真 runner 自有错误码，仍不暴露 cause/原始 adapter 文本。

### M3. 模型任务清理缺少硬截止

- 证据：`model-task-runner.mjs:198-213` 逐个 `await dispose()`；注释明确无法抢占不合作 JS。`model-task-cli.mjs` 虽能结束外层调用，但不能杀掉同 Host 进程内卡住的 adapter/Agent。
- 影响：生产 Host 可能泄漏线程、连接或内存，壁钟预算名不副实。
- 建议：模型任务放独立 worker 进程；Host 只保留有界 RPC 和取消信号；清理等待也设置上限并记录失败。

### M4. 文档与实现存在平台/异步栈漂移

- 证据：`docs/08-engineering-architecture.md:28,61,70-71` 仍写 bwrap、tokio；实际 Broker 非 macOS 直接拒绝，workspace 无 tokio，daemon 使用 `std::thread`。
- 影响：新贡献者可能按不存在的架构实现，验收口径混乱。
- 建议：把设计目标与当前实现分栏标记；增加 `docs/implementation-status.md` 作为唯一状态源，README/验收文档引用它。
- 另外：README 将 Phase 3“集成验收”勾选完成，但同页承认实际 GUI 未验收；`phase-2-3.md` 的 daemon 参数、RPC、P0 状态和 36/13 测试数量已滞后；`phase-1.md` 仍把 Rocq 列为未交付。应拆分“实现完成/验收完成”并标注历史快照。

### M4a. 健康订阅下 status 失败被静默吞掉

- 证据：`client/live.cjs:150-163` 的 refresh 空 catch；UI 只有 connecting/live/reconnecting。
- 影响：订阅 connected 但 status 持续失败且无 tick 时，HUD 可永久 Connecting，无 degraded/retry。
- 建议：有界重试 + 脱敏 degraded/error + 手动 Retry。

### M4b. 重连用事件序号冒充项目身份

- 证据：`client/live.cjs:117-130` 仅用 event_seq/highWater 判断 reset。
- 影响：同 socket 复用于新项目时，旧 P0/游标可能残留或误重置。
- 建议：status/subscribe 增加 projectId + connectionEpoch/daemonBootId；序号只用于同 epoch 排序。

### M4c. 默认测试不校验 bundle，也不跑 installed-DSH 用例

- 证据：`package.json:11-13` 只跑 `*.test.mjs`；`build.mjs --check` 和 `installed-*.mjs` 均不在默认命令。
- 影响：源码改了忘重建仍可全绿；真实 Typert/Owner/流式链路不在标准验收内。
- 建议：`pretest`/CI 强制 bundle check，新增 `test:installed` 和发布验收矩阵。

### M4d. P0 告警缺少读屏 live region；客户端 pattern 校验不完整

- 证据：`views.cjs` Last P0 动态 div 无 `role/aria-live`；`remote.cjs:87-93` 只在 Wiki 扩展字段存在时校验 pattern，`briefingGet.id` 的十六进制 pattern 被漏掉。
- 建议：P0 使用合适 alert/status live region；所有声明 pattern 的字符串无条件校验。

### M5. CI、架构守卫和工具链固定缺失

- 证据：仓库无 `.github`、`rust-toolchain*`、`Makefile`/`justfile`；70 个 ignored 用例没有分层执行矩阵。
- 影响：标准测试通过不代表原生安全链通过；依赖/编译器升级可能静默改变行为。
- 建议：至少加入 Linux 标准测试 + macOS 原生矩阵 + Node 插件测试 + bundle check + fmt + clippy；用 `cargo-deny`/`cargo-audit` 和依赖方向测试守门。
- 补充：JSON Schema 与 Rust 契约缺少 round-trip/快照测试；调度时间直接依赖 SQLite wall clock，缺可注入 Clock；Git/daemon 测试夹具重复且环境策略分裂。

## 6. 推荐目标依赖方向

```text
plugin client ──data contract──> plugin host ──> daemon RPC
                                           │
CLI binary ───────────────────────────────┤
                                           ▼
                                  ai4math-app / services
                                  │      │       │
                                  ▼      ▼       ▼
                              broker  store   git/project
                                  │      │
                                  └──> core/evidence
```

约束：

1. `store` 不依赖 `broker`。
2. `core/evidence` 无 IO、无 SQLite、无进程、无 Git。
3. `broker` 只负责证据生成、沙箱、资源与执行，不写业务账本。
4. `app/services` 负责锁顺序、Git/SQLite 事务、审批和恢复。
5. `daemon` 负责传输、订阅、驻留监督和脱敏投影，不直接实现命令策略。
6. CLI 与 daemon 共享应用服务/协议，不共享人类可读的命令行格式。

## 7. 分阶段解耦路线

### P0：2–4 天，先修信任与恢复边界

1. 统一所有证据入口的 physical ancestry，修 promotion/T3/Wiki graft 双轨。
2. 修 T3 `module_path=NULL` 漏标、promotion 历史重放/intent 清理。
3. 失败任务 retry/backoff/cancel/quarantine，避免 FIFO 死锁。
4. daemon 单例锁前置；模型租约执行期/提交 grace 分离。
5. InfraRFC Approved 幂等分支前移；直接文件 promotion 的 breaker/Pivot 门禁统一。
6. 为上述每项补失败注入/并发回归，并启用 `clippy -D warnings`。

验收：graft 不入账、T3 不漏标、重复恢复不卡 intent、失败任务不堵队、第二 daemon 不改 DB、临界模型回复不丢、RFC 重放幂等。

### P0b：3–5 天，可靠性与安全加固

1. 统一 `open_managed_state`，覆盖全部 CLI 写入口、SQLite sidecar/ACL/journal。
2. 修 pretty Fact Pack 重留存、Graveyard 安全重放与 refutation outbox。
3. Wiki/Sediment/Fact Pack 写入侧有界，事务外遍历/读取，拒绝 FIFO/别名/共享可写索引。
4. 修 compiler FD 封装、PinnedExecutable、spec 构建进程组和 Heavy 压力策略。
5. 提取 `ObjectOnly`，统一驻留配置；补 Schema/Rust 契约和可注入时钟测试。
6. 修 `Trust::Default` Clippy 警告，建立 CI、bundle check、依赖方向和 installed-DSH 分层门禁。
7. 更新文档状态，把 tokio/bwrap 等标为“目标”或“未实现”。

验收：标准/插件/原生关键测试全绿；超限/别名/ACL/数组配置/路径替换/时钟回退均有失败回归。

### P1：3–5 天，ProjectContext 与应用服务骨架

1. 新增 `ai4math-project`：root/runtime/state/repo 校验、pending journal、锁顺序、代次 snapshot。
2. 将 `managed_native + PromotionRepo + StateStore` 的重复组合替换为 `ManagedProject` 门面。
3. 拆 `promotion.rs` 的 Git 原语和 promotion 用例；长参数列表改为请求结构体。
4. daemon 驻留命令引入 `ResidentCommand/ResidentSummary` DTO，保持外部行为不变。

验收：现有命令行为不变；T3/T2/Pivot/Worker 共用同一门禁；减少重复路径且测试不缩小。

### P2：1–2 周，证据与 Store 解耦

1. 新增 `ai4math-evidence`，迁移 receipt/comparison/witness/oracle/probe 视图。
2. Store API 改为接收证据 trait/投影，移除 `ai4math-broker` 依赖。
3. 将 `StateStore` 内部按 graph/tasks/pivot/rfc/evidence/wiki 查询边界拆分；保留单一 SQLite 写事务，不盲目拆库。
4. 补 receipt/task/Pivot/RFC 的 replay/audit。

验收：Store 可独立编译测试；业务事务仍原子；证据不可由 JSON 直接构造；图/账本审计保持通过。

### P3：2–4 周，生产自动化闭环

1. 持久 scheduler/worker 生命周期：领取、心跳、取消、TTL、孤儿回收、熔断联动。
2. 在线模型任务独立进程化，补生产凭据、预算、审计和限流。
3. owner-only GUI 命令：预览→确认→执行→恢复，先做 `RESUME_AS_IS`/archive，再做 PATCH/Pivot。
4. 自动 Definition Smell 与 T3 触发仍保留人工审批门，不自动提升信任。

验收：真实模型与真实 daemon/GUI 的端到端闭环；断电/强杀/重连/重复确认有回归。

### P4：后续

1. Linux `SandboxBackend`（bwrap + cgroups v2）和全主机配额。
2. Agda/Rocq 独立 T2/Comparator/见证对等。
3. 签名回执、远端发布强制门禁、外部通知 outbox。

## 8. 下一阶段工作目标（建议按此顺序）

1. **修信任边界**：physical ancestry、Lean shadow/replay、T3 漏标、promotion 重放。
2. **修恢复与调度边界**：失败任务公平性、daemon 单例、模型提交 grace、RFC 幂等、refutation outbox。
3. **建立 CI 与架构守卫**：fmt、clippy、448 标准测试、55 插件测试、bundle check、原生/installed 矩阵、依赖方向。
4. **统一项目上下文与锁顺序**：`ManagedProject` + `open_managed_state`，覆盖所有原生/Git/状态命令。
5. **解除 Store→Broker 依赖**：`ai4math-evidence` + Store 证据投影接口。
6. **拆分 CLI 应用服务与 Git 原语**：先 promotion/T3，再 Pivot/RFC/Wiki；驻留命令协议化。
7. **模型任务进程隔离**：硬截止、可回收、生产凭据和预算。
8. **补自动 scheduler 与 GUI owner 写闭环**：从最小受审批状态机开始。
9. **最后扩平台/引擎**：Linux 沙箱、Agda/Rocq T2、全主机资源配额。

## 9. 明确不建议现在做的事

- 不建议为了“解耦”把 SQLite 拆成多个数据库：当前跨 Claim/Plan/Event/Pivot 的原子事务是安全资产。
- 不建议让 daemon 直接调用 Broker 绕过 CLI/应用服务的恢复与锁协议。
- 不建议自动把 Oracle、模型建议或 Wiki 冷层重验升级为证明信任。
- 不建议在原生模型/沙箱硬截止未解决前开放常驻在线模型队列。
- 不建议继续扩展人类可读 CLI 分支而不引入命令对象和应用服务。

## 10. 本轮实施记录（2026-11-25）

- 新增 `ai4math-project`，CLI 生产状态读写统一经过 `ManagedProject` / `open_managed_state`。
- promotion/T3/Wiki 证据入口统一 physical ancestry，新增 graft/replace/commit-graph 回归。
- 修复 T3 旧式 `module_path=NULL` Claim 漏标、promotion 历史重放身份校验与 pending intent 清理。
- 修复失败任务 FIFO 死锁：持久退避、quarantine、显式重试语义；模型任务增加 30 秒提交宽限期。
- 修复 InfraRFC 批准重放、单快照读取和过期租约冻结计数。
- 直接文件 promotion 共享 breaker/Pivot 门禁；T3 统一非阻塞 Git 锁、终检和完成态快速返回。
- 新增基础 CI；插件测试默认检查 bundle 一致性。
- 新增 `ai4math-evidence` 零 IO 证据契约 crate；Broker 保留私有封印类型与原生构造权，Store 只接收 `unsafe` 只读端口，normal/dev 均不再依赖 Broker。
- 三个 macOS 原生 Store 测试已迁至 CLI integration 层，只使用公开 API；Store 源码与 Cargo.lock 均无 Broker 边。
- 保持 receipt/Oracle wire 与历史 hash 不变，新增依赖方向、wire shape、receipt/Observation golden 回归。
- 验证：Rust 480 通过 / 0 失败 / 71 ignored；插件 55 通过；fmt、Clippy（有既有警告）、diff check 通过。
- 原生 graft 恢复用例因外层 Seatbelt 限制未获得原生通过证据；其余未完成项见 [implementation-status.md](docs/implementation-status.md)。

## 11. 可靠性整改

- daemon 单例租约移到 Wiki/briefings 创建与 SQLite 打开之前；双进程测试验证丢锁实例不触及目录、数据库和事件。
- refutation 改为私下暂存 → SQLite 权威提交 → 无覆盖归档发布；CLI 支持查询和恢复。覆盖提交前后故障、旧账本记录采纳、重复提交保留首张回执，以及 macOS ACL 等路径边界。
- Lean 标准 T1/T3 和回归接入安装模块遮蔽检查及独立 kernel replay；macOS Lean 4.35.0 的 4 项原生正负例通过。回执 wire 格式不变。
