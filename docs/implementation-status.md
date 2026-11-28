# 实现状态（2026-11-26）

本文件区分“代码已实现”“标准测试已通过”“原生/真实宿主已验收”三种状态，避免把设计目标或本机夹具误读为生产交付。

## 交接快照

代码验收基线：`67876ec`（在 `e5e5ef4` 之后连续完成跨学科领域包与 `ProtocolSpec` `7648418`、GPU/SLURM 算力抽象 `97a5028`、架构文档 `419aa0a`，以及 ADR-0007 阶段 1–3 信念传播/知识库/空闲复现闭环 `1bd8ef3` / `dac6274` / `67876ec`）。

- **本机标准套件**：Rust 工作区 **70 套件、626 通过、0 失败、77 默认忽略**（截至 `67876ec`，`cargo fmt` 与全部新增/修改模块 Clippy 零警告，架构依赖守卫全通）；DSH 插件 **119 通过 / 0 失败**（含 bundle 一致性、Host 热切桥接、多项目工作台、浏览器 VM 与中英文实时切换校验）。
- **已取得的原生与现用 GUI 证据**：固定工具链的关键 Lean 回归在允许 Broker Seatbelt 的本机宿主跑通；独立安装版 DSH SDK 核对真实 daemon 重启重连、卸载及 Electron Host 托管启停闭环；现用 DSH Desktop GUI 已完成插件安装、全屏工作台与底部 HUD/侧栏面板、断线重连、禁用卸载、多项目创建与中英文跟随验收。
- **优先恢复项**：定位 Rocq 9.3 `coqchk` 初始化所需最小权限并保持项目 Seatbelt；在带 cgroup v2 委派的 Linux 宿主完成 launcher、实际限额和回收的端到端验证；补齐 Guest/登出浏览器实测与独立操作者写确认边界。其余缺口见[交付清单](delivery-roadmap.md)。

## 基线批次改进

### 项目上下文与状态门禁

- 新增 `ai4math-project` crate，提供 `ManagedProject`、`managed_state_path`、`open_managed_state`。
- CLI 生产入口统一校验项目根、`.runtime`（owner、mode `0700`、非符号链接）、`state.sqlite` 及 WAL/SHM/journal（普通文件、单链接、owner、非共享可写），macOS 拒绝授予型 ACL。
- 原生租约通过 `ManagedProject::try_native_heavy` 获取；调用方仍在 operation 内按 native → Git → SQLite 顺序执行。
- 测试夹具保留直接建库，生产代码不再使用 `private_state_path` 兼容入口。

### Git 信任边界

- `PromotionRepo::is_ungrafted_ancestor` 统一使用 physical ancestry：忽略 replace refs、`info/grafts` 和 commit-graph；已删除含歧义的 `is_ancestor` 包装器。
- promotion 发布/恢复、prepared commit 原始父头、T3 discovery、reviewed spec、CORE projection、Pivot、breaker 证据入口统一物理祖先语义。
- 新增 graft/replace/commit-graph 回归；原生 graft 恢复随后已在 Lean 前驱隔离批次通过。

### Store 证明与调度边界

- T3 模块影响统一使用 `effective_claim_module`：`module_path` 优先，旧式 NULL Claim 从 `formal_target=file#decl` 回退。
- `finish_promotion` 历史重放核对 recorded promotion、repo、recipe、receipt 全字段/raw、comparison 和事件身份；仅完全相同 pending intent 可在同事务清理。
- 普通任务失败采用持久指数退避（1s、2s、4s，上限 60s），第三次失败 quarantine；自动调度跳过退避/quarantine，显式 expected task 可绕过 backoff 但不能绕过 quarantine。
- 模型任务执行 deadline 与持久租约 TTL 分离，提交宽限期 30 秒。
- InfraRFC Approved 精确重放先于 consumer CAS；只读 RFC 查询使用单一 read transaction；熔断冻结计数排除过期租约。

### 证据端口与 Store 解耦

- 新增无 IO 的 `ai4math-evidence` crate：`ReceiptTier`、`OracleQuery`/`OracleOutcome`、ordered-source hash，以及 10 个对象安全的 `unsafe` 只读证据端口。
- Broker 保留全部具体封印类型、私有字段、原生验证与独占构造权；只为原生成功路径实现端口。普通 safe Rust 与 JSON 无法实现端口或伪造 fresh evidence；该边界不防御恶意 `unsafe` 依赖。
- Store 生产依赖从 `ai4math-broker` 替换为 `ai4math-evidence`，公开写入口只接收 `&dyn *Evidence`；`VerifiedReceipt`/`ComparedSpec` 仍为 Store 私有投影，SQL/schema/JSON ABI 不变。
- 新增依赖方向守卫、Oracle wire 形状测试、ordered-source hash golden，以及 Broker receipt/Observation 固定 JSON/hash golden。

### CLI 与 T3

- 直接文件 promotion 与 reviewed/candidate 输入共享 breaker/Pivot 门禁。
- T3 All/One/Batch/Sweep 统一非阻塞 Git 锁；All/One 增加与 Batch/Sweep 相同的最终 main 复核。
- 已完成 T3 sweep 在获取原生租约前返回。

### 启动与跨资源恢复

- daemon 在创建 Wiki/briefings 或打开数据库前持有 `.runtime` 单例租约，直至服务结束。双进程回归覆盖竞争失败时目录、数据库文件和事件不变。
- refutation 先持久化配方并私下暂存 Markdown，再原子提交 SQLite 的证据、信任与事件，最后无覆盖地发布 Markdown。用 `ai4math refute-pending ROOT` 查看未完成项，用 `ai4math refute-reconcile ROOT [CLAIM]` 恢复或补建缺失文件；不重发证据事件，不覆盖冲突文件。旧记录需用同一已验证 witness 重新提交一次才能补存配方。
- Lean 标准 T1/T3 及回归入口拒绝安装模块遮蔽；签发及回归时在独立 Seatbelt 进程加载对象、用 stock kernel 重放目标闭包，并核对允许的公理。模块检查和回归下游另重放公开声明。回执 wire 格式未改。这不是对所有 Lean 工具入口的全面原生验收。

### 工程门禁

- 新增 `.github/workflows/ci.yml`：Ubuntu/macOS Rust fmt/test/clippy，Ubuntu Node 插件测试。
- 插件 `npm test` 默认先执行 `client/build.mjs --check`，防止陈旧 bundle 漏过测试。

## 本轮验证

- `cargo fmt --all -- --check`：通过。
- `cargo test --workspace --locked`：500 通过、0 失败；75 个用例默认忽略。另在 macOS 上用 Lean 4.35.0 和 Broker 自身的 Seatbelt 单独运行新增的 4 项原生回归，全部通过。
- `cargo clippy --workspace --all-targets --locked`：通过，仍有既有风格/长参数警告；`ai4math-evidence` 单独 `-D warnings`（仅放行既有 `core::Trust::Default` lint）通过。
- `npm --prefix plugins/dsh-ai4math test`：55 项通过，含 bundle 一致性检查。
- `git diff --check`：通过。

其余默认忽略的原生/宿主用例未在本轮统一重跑，不视为已验收。

## 后续批次：有界 Wiki 重建

- Wiki 在 SQLite 写事务外完成有界扫描、解析和路径漂移复查，再原子替换索引。上限为 16,384 个扫描条目、4,096 个 Markdown 文件、单文件 8 MiB、总计 64 MiB、目录深度 64。扫描拒绝不安全的文件类型、链接、共享写权限及授予写入的 macOS ACL；两次观测不等于跨文件原子快照。
- 受管 CORE 发布阶段持有账本代次保留，账本已换代时不替换旧索引；外部 Git 写入仍由前后检查和读者代次门禁处理。
- 本批 Rust 工作区 509 通过、75 默认忽略，插件 55 通过，格式和 Clippy 检查通过（仍有既有警告）。

## 后续批次：Sediment 与 Fact Pack 留存

- Sediment 在渲染时限制源码 2 MiB、完整归档 8 MiB；在已打开的目录和文件上检查 ACL、临时名与最终名身份，保留无覆盖发布及同字节重试语义。
- Fact Pack 保留紧凑 JSON 768 KiB 和旧版磁盘 JSON 2 MiB 上限。读写固定留存目录、检查身份及 ACL；旧版缩进 JSON 按摘要及内容核对，不改写。权威 JSON 先发布；摘要失败仍可读 JSON，处理冲突后重试可补齐 Markdown。
- macOS Sediment 定向 14 项、Fact Pack 写入 8 项、Store 读取 5 项均通过。Rust 工作区 520 通过、0 失败、75 默认忽略，插件 55 通过，fmt、diff 与 Clippy 通过（仍有既有警告）。父路径仍需由受管部署信任；不承诺同 UID 并发修改下的文件树原子性，未实现长期保留容量策略。

## 后续批次：Lean 前驱隔离

- Lean T1/T3、回归和模块检查每次只把当前模块的 scratch 授权为可写。Broker 将主 `.olean` 和已知对象配套文件收集到只读导入池；下一模块不再看到前驱源码或任意临时产物。Witness 和 breaker 复用这条隔离边界；独立 kernel replay、回执 wire/哈希规则和 Golden 只留存主对象的合同不变。
- [原生 CI](../.github/workflows/ci.yml) 用 SHA-256 固定 [Lean 4.35.0 arm64 发布包](https://github.com/leanprover/lean4/releases/tag/v4.35.0)，通过同一[测试脚本](../.github/native-lean.sh)执行。该脚本在本机 macOS 实跑 11 项全过，含 T3 错误分类、Witness、Golden 克隆、下游失败不发布和 3 项 Git 恢复（含 graft 正负例）；另单独实跑 breaker 和 T2 原生各 1 项，均通过。尚无 GitHub 托管 runner 实跑记录。
- Rust 工作区 523 通过、0 失败、75 默认忽略；插件 55 通过；fmt、diff 与 Clippy 通过（仍有既有警告）。

## 后续批次：独立 DeepSeek 模型任务

- 操作者显式选择的 DeepSeek 任务在单独进程执行；可信 SDK 与 adapter 在其中的 worker 线程运行，由进程主线程监视时限及父生命线。父侧在超时或取消时终止进程组并等待直接子进程回收。父侧中断时由 fd 3 EOF 唤醒仍可运行的 guardian。不是 OS watchdog，也不隔离恶意 SDK；父崩溃后不保证清理逃逸或整个进程组。可注入 adapter 的旧进程内入口仅供夹具和可信本地实验。
- 配置经有界 stdin 帧传入，不进入 argv 或子进程环境；只接受有界单帧结果和成功退出。Rust 仍核对任务/租约并只留存未验证建议。没有接入 GUI、常驻排程或环境中的生产凭据。
- 本机 Electron/已安装 SDK 使用合成凭据和 localhost Messages 流式夹具验证了证明及审阅角色、HTTP 失败不入账与显式重试；未访问远端服务，未验收真实 GUI 或 Linux 宿主。插件测试 78 通过；Rust 工作区 523 通过、75 默认忽略，fmt、diff 与 Clippy 通过（仍有既有警告）。

## 后续批次：持久单次模型派单

- 操作者用 `model-dispatch-approve ROOT BARE_REPO ID CLAIM ROLE REVIEWED_COMMIT MODEL WALL_SECONDS MAX_MODEL_CALLS` 批准一个明确任务。账本不保存凭据或模型上下文；在 Git 锁内检查审核主干，随后通过同一 SQLite 事务消费审批、领取租约并写审计。只有提交后才发送任务上下文。领取仍受容量、依赖、熔断、退避和 Oracle 挂起约束。
- `model-dispatch-status ROOT ID` 查看，`model-dispatch-cancel ROOT ID` 只取消未派发的批准，`model-dispatch-reconcile ROOT ID` 只依据精确匹配的已入账候选推进状态。已进入审批流程的任务 ID 不回旧队列；中断、缺少确认或租约过期不会自动重派。`dispatched` 不证明远端收到请求，也不等同于成功或失败。
- 受信本地 Host 通过 `runApprovedDeepSeekModelTask` 使用私有管道命令；本机 Electron 使用合成凭据和 loopback 夹具通过审批审阅全链路。Store 定向 6 项覆盖迁移、重开、竞争与回滚；CLI 管道覆盖取消与确认丢失。Rust 工作区 531 通过、0 失败、75 默认忽略；插件 78 通过；fmt、diff 与 Clippy 通过（仍有既有警告）。未启用常驻派工、运行中远端取消或生产凭据。

## 后续批次：有界常驻模型调度

- 受信操作者程序可调用 [`runDeepSeekModelResident`](../plugins/dsh-ai4math/host/model-task-resident.mjs)，必须指定 SDK、CLI、项目、仓库、模型、凭据和有限轮数/派发次数/轮询间隔/扫描时限。进程内只运行一个循环，每次至多一个任务；扫描不接收凭据，不挂载到 GUI 或代理工具。无后台自启动。
- 每轮只列出当前仓库、Git 主干和模型下已批准、符合现有门禁的任务，真正派发仍原子重查。重启可跨旧基线确认精确的持久候选；不会重复派发已消费审批。普通扫描/派发失败不可当成已入账。
- `model-dispatch-cancel-running ROOT ID` 与候选提交在 SQLite 中仲裁；运行中取消或熔断作废租约后，保持打开的协调管道也会被主动断开，推动本地 worker 回收。这不是远端 HTTP 服务承诺中止或撤销已开始的调用。乱序、确认丢失、断开与晚到结果不提升信任。
- 本机已安装 Electron 用合成凭据和 localhost 夹具完成驻留审批派发与重启后零额外请求。未验证生产凭据、远端服务、Linux 宿主或系统服务部署。Rust 工作区 538 通过、0 失败、75 默认忽略；插件 111 通过；fmt、diff 与 Clippy 通过（仍有既有警告）。

## 后续批次：DSH Bundle 与安装版 SDK 验收

- 本地包新增默认禁用的 Cordis entry、版本约束和中英文元信息。默认不为任何 Agent 注册项目工具或证明槽。观察工作台可显式留存事实包，不代表批准或推进项目状态；可按[部署与验收步骤](phase-2-3.md)复跑。
- 本机安装版 Electron/Cordis/Typert 实测合成 Owner/Guest 的门禁、实际 daemon 重启重连，以及插件卸载后服务注销和订阅结束。没有安装到现用 profile 或完成浏览器认证、布局和视觉验收。Rust 538 项、插件 113 项通过（Rust 75 项默认忽略）。
- 已核对 [Owner 审批边界](owner-approval-boundary.md)：DSH Owner peer 本身不证明人在场或签署了精确计划。公开预览与状态不授权写入；未建立独立操作者确认前保持无远程 Pivot 写入口。

## 后续批次：Owner 归档签名试点

- 新增本地 `owner-archive-preview`（不创建缺失的 Git 锁、不修改计划状态）和固定消息/Ed25519 离线核验。签名只是相对于已选公钥的诊断结果，不消费确认、不执行写入，也不证明真人在场。持久请求绑定、已确认执行、恢复以及 GUI 接线仍未启用，见 [Owner 审批边界](owner-approval-boundary.md)。
- 本机合成密钥、签名绑定及预览不变性回归通过；Rust 工作区 544 通过、0 失败、75 默认忽略，插件 113 通过，格式、diff 和 Clippy 通过（仍有既有警告）。

## 后续批次：Owner 签署归档的执行与恢复

- 本地操作者可用固定项目公钥验证签署的单熔断归档，完成即时计划比较、短有效期复核、永久请求 ID 和 Pivot 意图的原子记录。恢复沿用原 Git pin/CAS/SQLite 协议，按请求 ID 查询已准备、已执行或已放弃；不重放旧签名，也不为 DSH 提供写入口。
- 合成密钥测试实际执行 Git CAS 后模拟响应丢失，重开项目恢复同一提交；重复应用拒绝，重复恢复读回同一回执。固定信任锚和 Store 迁移、回滚、冲突另有回归。实际人工签署设备、生产项目权限隔离和浏览器接线仍未验收，详见 [Owner 审批边界](owner-approval-boundary.md)。

## 后续批次：Linux 隔离准备

- 将原生验证策略与进程监督分离；launcher 无法启动时不返回 verifier 退出回执。增加 Linux `/proc/meminfo` 有界解析，缺计数、单位不符或值不一致时拒绝采样；这不是 cgroup 配额。
- 非 macOS 验证仍默认拒绝，不存在未隔离回退。本机无运行中的 Linux 容器引擎；外层沙箱拒绝本轮 Seatbelt 原生用例启动。见 [Linux 启用与验收门禁](linux-sandbox-acceptance.md)。

## 后续批次：Linux cgroup v2 只读预检

- 新增 `linux-cgroup-preflight`，仅检查操作者指定的实际 cgroup v2 叶节点：空、非冻结、domain，读取有限内存/PID/CPU 上限，探测 `cgroup.kill` 是否存在。以 Linux `openat2` 防止符号链接和控制文件跨挂载；不启动后端、不写控制文件、不声称委派。普通文件夹不能冒充 cgroup。
- 本机仅验收可移植解析和 CLI 的拒绝路径。真实 Linux 成功路径与控制文件约束需在预置主机上运行 [原生预检用例](linux-sandbox-acceptance.md)；cgroup 实际入组和回收未接入。

## 后续批次：Linux 挂载计划

- 已加入确定性的纯策略编译器，划分主程序、runtime 文件、数据、合成目录和可写 scratch，拒绝冲突及超限。macOS 只验证计划和失败分支；不执行 mount、没有新增可用 Linux verifier。见 [Linux 挂载计划与验收要求](linux-sandbox-acceptance.md)。

## 后续批次：Linux 挂载快照对账

- 新增有界 `mountinfo` 校验，按计划核对根、挂载集合、传播、父节点及有效读写/执行标记。macOS 使用合成快照验证成功与拒绝路径；尚无可信 Linux launcher 采集快照，也没有启用后端。详见 [Linux 验收门禁](linux-sandbox-acceptance.md)。

## 后续批次：Rocq 独立 T2 试验入口

- 增加固定 Git/账本代次的 Rocq T2 手动观察：从源重建，校验编译器和独立 `coqchk` 摘要，复核新对象并检查指定定理是否无未决假设。不会写账本或发回执。
- 可移植回归通过；本机实际隔离编译成功，但独立检查器初始化因 Seatbelt 权限失败。实验入口不参与发布。参见 [Rocq T2 原生验收](rocq-t2.md)。

## 后续批次：DSH 一体化工作台实装验收与中英文支持

- **实装修复（`885eff6`）**：将 4 个 Host 运行时包从 `peerDependencies` 改为 `dependencies` 并锁入 `pnpm-lock.yaml`，修复 `link:` 目录安装后 Host 解析失败；Client 侧按 DSH 0.2.2-rc.5 Cordis 反射门禁改为子 fiber `$mount` + `ctx.inject(['remote.ai4math', ...])`，并在 `vm-fixture.mjs` 中补齐点号属性门禁防回退。
- **UI 呈现重构（`97cf9b6`）**：全面切换为实时 `Theme.listTokens` 中的 `--dsw-alias-*` / `--dsw-specific-sidebar-fill` Token（零硬编码色值）；重构底部 HUD 为三区固定锚点布局、分组遥测与零值降噪；使用 `@container` 容器查询升级四个右侧栏面板的卡片、信任账本磁贴与键盘焦点环。
- **一体化工作台与双语切换（`e5e5ef4`）**：
  - `cordis.patch.yml` 内置 `preset-ai4math-chief`（`id: ai4math-chief`）并默认绑定 `rolePresets.chief`（`dsh-ai4math` 行本身保持默认禁用）。
  - Host 新增热切换 `bridge.retarget/disconnect/wake` 与 Owner-only `workbench.mjs`（多项目注册表 `~/.dsh/ai4math/workbench.json`、自动探测 `ai4math`/`ai4mathd`、`ai4math init`、Unix socket 字节上限检查、Host 托管启停与外部 daemon 只读识别，永不误杀外部进程）。
  - Client 注册左侧导航 `sidebar.panellist:ai4math`、全屏 `main:ai4math` 工作台（总览 / 简报 / 前沿 / 收件箱 / Wiki / 环境与角色、原生目录选择、关联会话与一键 **新建 Chief 主编排会话**）、会话顶栏 `conversation.session.header.utilities` 目录联动切换胶囊，以及熔断/收件箱只读操作者 CLI 命令助手（不越过 [Owner 审批边界](owner-approval-boundary.md)）。
  - 接入 DSH `ctx.locale`，全界面支持中/英文实时无刷新切换，EN/ZH 字典键完全对等。
- **本批验证**：DSH 插件 119 通过 / 0 失败；`installed-bundle-metadata.mjs` 官方 Loader 校验、`installed-dsh.mjs` 真实 SDK+Rust 集成测试及 Electron Host 真实托管闭环均通过；现用 Desktop GUI 已完成安装、UI 视觉、工作台建项与中英文切换验收，并经过两轮独立 UI/UX 审查与全项修复。

## 后续批次：跨学科领域包与反漂移 `ProtocolSpec` 底座（`7648418`）

- 在 `ai4math-core` 抽离 `EpistemicLevel`、`EpistemicDebt`、`EpistemicAssessment`、`ScientificTrust (E0–E5)`、`BackendDescriptor` 正向/证伪双上限与 `DomainPack` trait，将既有 ITP 封装为内置 `ItpDomainPack`，并补充 64 组偏序矩阵全覆盖回归。
- 在 `ai4math-evidence` 新增零 IO 的 `SealedReceipt` / `ProtocolRunEvidence` 端口与 `ProtocolSpec` 单遍槽位编译、物理健全性门禁、正交多验证器分歧判定、无环 Merkle `BlindedControlPack`、`PreRegistrationSpec` 与协议异味（含 `SurrogateGoodhartCollapse`）审计；原有 ITP `VerificationReceipt` 无侵入实现 `SealedReceipt` 且 wire 格式零变化。
- 在 `ai4math-broker` 落地 `Broker::verify_protocol` 与 `SandboxedExecutableRunner`；在 `ai4math-store` 以迁移 7（`schemas/protocol-verification.sql`）落地不可变预注册与只追加 `protocol_trials` 账本。
- **验证**：新增 CompChem / CompBio 端到端反漂移与只读旧库回归，Rust 596 通过 / 0 失败，插件 119 通过 / 0 失败。

## 后续批次：多维 GPU 资源池与统一 SLURM 后端（`97a5028`）

- 在 `ai4math-core` 新增 `ResourceRequest`（CPU/RAM/GPU/scratch/walltime）、`SlurmClusterPolicy`、`ExecutionTargetKind`、`ComputeJobStatus` 与 `ComputePoolSnapshot`；在 `ai4math-evidence` 新增可选 `ProtocolSpec.resource_ceiling`、`StagedJobBundle`、`ClusterMountMap`、匿名 Fisher-Yates `BlindedArrayPlan` 及防注入 `SlurmJobId` / `SlurmFlag`。
- 在 `ai4math-broker` 落地 `LocalComputePool` + RAII `ComputeLease`（逐卡独占/共享显存分配、Apple Silicon 统一内存联动扣减、强制 `CUDA_VISIBLE_DEVICES` 注入与 CPU-only 置空、`Static/NvidiaSmi/AppleUnified` 探测）、上层 `ComputeBackend`（`LocalComputeBackend` / `SlurmComputeBackend<T>`）、下层窄接口 `SlurmTransport`（`LocalSlurmCli` + 固定 `BatchMode`/`StrictHostKeyChecking=yes` + 纯 Rust 有界 ustar tar 的 `SshSlurmTransport`），以及 `Broker` 三阶段异步协议验证（远端 SLURM 自动挂载 `unattested_remote_hpc` 债）。
- 在 `ai4math-store` 以迁移 8（`schemas/compute-jobs.sql`）持久化在途异步作业、私有解盲表、重启配额恢复与按分支标签批量 `scancel`。
- **验证**：本机单测与合成集群测试通过（涵盖本地多卡 GPU 绑定、SLURM 盲测 Job Array 重启恢复、熔断按分支 `scancel` 与 OOM 封印为 `VerifierError`）；Rust **614 通过 / 0 失败 / 77 默认忽略**，插件 119 通过 / 0 失败。尚未在真实生产 SLURM 集群或 Linux CUDA 宿主做原生实跑验收。

## 后续批次：ADR-0007 阶段 1–3 信念传播、四层知识库与空闲复现闭环（`1bd8ef3` / `dac6274` / `67876ec`）

- **阶段 1（`1bd8ef3`）**：`ai4math-core::knowledge`（四层 `KnowledgeStratum`、`KnowledgeSourceKind`、`ReproducibilityClass`、`ExtractionStance`、`CitationStatus`、`DomainKnowledgeProfile`、域限定 `EpistemicTag`）与 `ai4math-core::belief`（双通道 Subjective Logic `(b, d, u)`、交换律标签衰减、菱形共享祖先防重复计票、阻尼因子图 BP、硬撤稿否决与精确反事实 `VoI` 排序）。
- **阶段 2（`dac6274`）**：`ai4math-store` 迁移 9（`schemas/claim-graph-v2.sql`）泛化主图（SQLite 触发器强锁 `itp => L0..L5`、非 `itp => E0..E5`，无缝提升存量 ITP `claims` 入统一 BP 图并持久化快照）；平滑演进 `Wiki` 为四层 `KnowledgeVault`（`literature/` 防洗白、通用 `entry_facets`、三通道 Shadow Hint、冲突簇、可插拔 `EmbeddingProvider`/`SemanticIndex` + SQLite 向量缓存 + 三路 RRF，旧库只读与存量 Wiki 测试全绿）。
- **阶段 3（`67876ec`）**：迁移 10（`schemas/idle-verification.sql`）+ `ai4math-broker::idle_scheduler::IdleVerificationScheduler`，融合 BP 反向 `VoI` 自动队列与人类显式待核验队列、前台 Heavy/内存/GPU/SLURM 零饿死水位线、独立空闲预算与原子 `replicates`（节点升级清标签 + 置信度跃升）/ `contradicts`（自动挂冲突边拉升 `conflict_mass`）闭环。
- **验证**：Rust **70 套件、626 通过 / 0 失败 / 77 默认忽略**，DSH 插件 **119 通过 / 0 失败**，`cargo fmt` 与新增模块 Clippy 零警告。

## 后续规划（[ADR-0007](adr/ADR-0007-scientific-generalization-compute-vault-and-belief.md) 阶段 4–5，尚未代码实施）

- 阶段 4（先数学基线）：`ai4math-evidence::literature` 通用溯源门禁、数学文献卡型与 6 维离线漂移测试床，并先针对已建立的数学基线相关论文做抽取与空闲形式化验证（暂不接 CompChem / CompBio）。
- 阶段 5（非 ITP 拓展）：在数学基线文献抽取验证稳定后，再扩展 CompChem / CompBio 文献卡型（单位归一化、实验条件与对照配方）并接入首个计算化生试点。

## 角色化受控写 RPC 与 Chief 只读工具面（2026-11-28）

- daemon 新增连接级、一次性 `session.bind` Principal；业务写请求不再接受 `role` 字段，审计 actor 由 Principal 派生。未绑定、越权、重复绑定均 fail closed。该机制是 Host 角色绑定，不是同 UID 强认证。
- 新增 `claim.create` / `claim.read` / `plan.create` / `plan.read` / `inbox.push`。Claim 与 ProofPlan 按自然 ID + 不可变创建事件精确重放；Inbox 保持同 ID 同内容幂等，冲突返回稳定 `-32005`。
- 新增 Store `push_decision`：2–8 个决策选项、有界 details、已知 Claim 引用检查、row/event 同事务；不解决 Inbox、不推进 checkpoint、不改变 trust。
- 插件新增递归严格 schema 校验、校验后深拷贝、同角色写连接单飞绑定；Agent-only 写工具不进入 Owner Remote。Chief preset 移除 shell，FS 只读，并增加 Agent scope allowlist/guard。
- 裸 `claim.set_trust` 未暴露：L1/L2a/L3/L4a 需要 typed evidence transition，L2b/L4b/L5 继续由 Broker/ADR 门禁。
- 验证：`cargo test --workspace --locked` 通过；`cargo clippy --workspace --all-targets --locked` 通过（仅有既有警告）；插件 `npm test` 123/123；安装版 Cordis + Typert + Rust UDS 集成测试通过。

## 共享 ManagedGit、RFC 与任务控制面（2026-11-28）

- 新增 `ai4math-git` crate，将 CLI `PromotionRepo` 与 daemon 的 bare-repo 校验、协调锁、直接 main HEAD、精确 tracked-path 检查统一为同一 `ManagedGit` 实现；CLI 原有 CAS/发布/恢复逻辑不变。
- daemon 新增 `rfc.submit` / `rfc.approve` / `rfc.smoke_start` / `rfc.reject` / `rfc.supersede` / `rfc.inspect`：Explorer 提交 DRAFT，Chief 审批/拒绝/替代，Builder 按精确审批快照进入 BUILDING_SMOKE；repo、HEAD、模块跟踪状态来自受管绑定，Agent 不能传 repo 路径或绕过 Git 锁。RFC 事件 payload 绑定协调者 actor，原有回放防伪回归保持通过。
- daemon 新增 `task.register` / `task.status`：Chief 按当前受管 HEAD 注册调度元数据，注册不派发、不授予证明权；Store 按自然 ID 与不可变元数据精确重放，冲突 fail closed。
- 真实 bare Git + UDS 集成测试覆盖 Explorer 提交、Chief 审批/拒绝/替代、Builder smoke-start、RFC 精确重放、任务注册重放与 scheduler ready 状态；共享 Git 锁与 InfraRFC Store 回归分别通过。
- 验证：`cargo test --workspace --locked` 通过；`cargo clippy --workspace --all-targets --locked` 通过（新增 `ai4math-git` 无警告，其余为既有警告）；插件 `npm test` 125/125；安装版 Cordis + Typert + Rust UDS 集成测试通过。

## 持久化 worker attempt 与安全派单（2026-11-28）

- 新增 `worker_attempts` 与事务内 `claim_task_in_tx`：acquire 在有 runnable Claim 的 Infrastructure 任务中选择一个，lease 与 RUNNING attempt 同事务提交；request ID 指纹覆盖 principal、role、base commit 与 wall budget。
- submit/cancel 按 attempt + Principal 校验，在同一 IMMEDIATE 事务内恢复私有 fence、消费精确 lease 并写终态；candidate 与 attempt SUBMITTED 同事务落库，取消不完成任务。旧 attempt 通过 expiry 代次检查不能释放重领后的新 lease。
- Prover 只能提交 proof body，Reviewer 只能提交结构化 review；完整规范化 suggestion 摘要用于幂等，冲突返回 `-32005`，越权/终态使用稳定 typed RPC 错误。
- Host 将 Agent ID 作为连接 Principal binding，同角色不同 Agent 使用独立写连接；worker packet 不含 fence、scope、repo 路径、凭据或 candidate readback。该 binding 不是同 UID 密码学身份，cancel 只撤销后续提交权。
- Store 与真实 bare Git + UDS 测试覆盖 acquire 重放、base 冲突、submit 重放/冲突、cancel、旧 attempt 不劫领新 lease（私有 fence + expiry 双绑定）、同角色不同 Principal 越权，以及 candidate 不授予证明权。
- 验证：`cargo test --workspace --locked -- --test-threads=1` 完整通过；Clippy 通过（仅既有警告）；插件 `npm test` 127/127；安装版 DSH 集成测试通过。

## 脱敏 promotion 控制面（2026-11-28）

- 新增 Store `promotion_preview` / `promotion_status` 与 daemon `promotion.preview` / `promotion.status`，仅 Chief 可调用。
- preview 在同一读事务内从 retained proof-body candidate 生成版本化 candidate/preview SHA-256、字节数、Claim 信任摘要、当前 HEAD、账本代次与 pending 门禁；严格拒绝未知字段与超过 8 KiB 的 body。proof body 只参与哈希，不进入响应。status 额外返回 latest recorded 脱敏摘要，不返回 repo 绝对路径、proof body、receipt 或 fence；daemon 在 Git 锁内核对读取前后 HEAD 不变。
- status 以 `idle/pending/recorded` 表达持久状态，并用 `ManagedGit` 物理祖先查询分类 `prepared/published_unrecorded/main_diverged/git_unobserved`；recorded 的 `on_main` 为可选观察值，Git 查询失败不会被当成 false。
- 两个 RPC 都走 Principal-bound 连接；未注册 execute/recover，不会准备 Git transaction、消费 Owner 授权或授予 trust。
- Store 单测与真实 bare Git + UDS 测试验证响应不含 proof body / repo 路径，pending intent 仅返回脱敏摘要。

## Typed Oracle trust transition（2026-11-28）

- 新增 Chief-only `claim.apply_oracle {claim_id, observation_id}`；调用方不能选择目标信任等级。
- Store 从 retained bounded Oracle observation 派生目标：`NoCounterexampleWithinBounds → L1`、`Counterexample → L2a`；`Disagreement/Unavailable` 返回稳定错误且不改状态。
- transition 在同一写事务内核对 observation approval 与 live Claim 的 repo、HEAD、engine、formal target、informal statement，并按 observation ID 精确重放；事件记录 `proof_verified:false`。
- L3/L4a/L4b/L5 没有对应 typed evidence path 前仍不可写；现有 `set_local_trust` 仅是 Store 低层 API，不是 Agent 授权。
- Store 与真实 bare Git + UDS 测试覆盖 L1/L2a 映射、重放、HEAD/Claim 上下文不匹配、unsupported outcome 与角色越权。

## 原生 RFC freeze/complete 受信适配器（2026-11-28）

- daemon 新增 `--rfc-cli PATH --rfc-lean PATH [--rfc-timeout-secs N]`；两者必须同时配置，路径必须为绝对可执行文件。
- Builder-only `rfc.freeze` / `rfc.complete` 只接受 RFC ID、精确 reviewed commit、draft digest 与 smoke lemma 名称；repo、project root、CLI、Lean executable 均由服务端固定。
- 适配器使用清空环境、`cwd=/`、独立进程组、stdin lifeline、stdout 上限与硬超时调用现有 CLI；CLI 继续负责原生 Broker 证据生产和 Store transition，daemon 不接收或伪造 receipt。
- RPC 在调用前复核 RFC 状态、engine、digest、smoke commit 与 Git HEAD；未配置受信工具时返回 `-32006`，不会回退到 unsandboxed 或调用方指定路径。
- 单元与假 CLI/UDS 夹具验证固定 argv、环境清理、角色/schema 门禁和状态前置检查；另有 ignored 的真实 Lean 4.35.0 端到端验收，覆盖 submit → approve → smoke-start → freeze → complete 与两类 receipt。该测试已在本机无外层 Seatbelt 环境通过，并加入 `.github/native-lean.sh`；其他宿主仍需单独验收。

## Owner-only promotion recover（2026-11-28）

- 新增 Owner-only Typert `promotion.recover`，不注册为任何 Agent 工具；请求不接受 repo、root、proof body、base/new commit 或 executable。
- daemon 先确认存在 pending promotion intent，再通过受信 CLI/Lean 配置调用现有 `recover-promotion ROOT BARE_REPO NATIVE_LEAN`；CLI 继续负责同一 intent 的 Git/ledger 恢复语义。
- Owner bridge 使用独立 Principal-bound 连接；Chief、Builder、Prover、Reviewer 等角色均无法调用。
- 假 CLI/UDS 夹具验证固定 argv、Owner 权限、无参数注入与 pending 前置门禁；真实恢复语义仍由 CLI 既有 native/恢复测试覆盖。

## Owner-signed promotion execute（2026-11-28）

- 新增 Owner-only `promotion.approval` / `promotion.execute`，不注册为 Agent 工具。
- approval 返回 canonical JSON 的 hex 签名消息与脱敏 preview；消息绑定 project root、repo、task/claim/engine/module/target/base、candidate SHA-256、CLI/Lean SHA-256、request ID 和到期 Unix 秒。
- Owner 公钥只从 `.runtime/owner-promotion.pub` 读取，要求单链接普通文件、当前 UID、无 group/world 写权限；签名通过 `/usr/bin/ssh-keygen -Y verify` 离线验证，有效期最多 10 分钟。
- execute 在调用 CLI 前原子消费 request ID；同一消息重放返回 `replayed:true` 且不会再次执行，冲突复用返回错误。CLI 失败后该授权仍保持已消费，必须重新审批。
- 实际执行固定调用 `promote-task-result ROOT BARE_REPO TASK NATIVE_LEAN`，daemon 不读取或序列化 proof body。
- Store、真实 Ed25519 + 假 CLI/UDS 测试覆盖签名、固定 argv、角色隔离、一次性消费与 proof body 不泄露；真实 Lean 4.35.0 的 daemon 端到端验收已在本机无外层 Seatbelt 环境通过，覆盖签名授权、原生验证、Git main 发布、recorded promotion 与一次性重放，并加入 `.github/native-lean.sh`；其他宿主仍需单独验收。

## L3 formal-sketch Store evidence gate（2026-11-28）

- 新增 `formal_sketch_evidence` 不可变证据表与 `StateStore::apply_formal_sketch_evidence`。
- L3 只接受三类同时匹配 live Claim/repo/HEAD 的证据：assessed T3 candidate（axioms 含 `sorryAx`）、sealed reviewed-spec comparison、retained `NoCounterexampleWithinBounds` Oracle probe。
- 任一证据缺失、spec 不匹配、非 T3、HEAD/Claim 上下文变化或 Claim 已 stale/refuted/higher-trust 都会拒绝；proof body 不进入证据表或响应，只保留 source/receipt/probe 哈希。
- receipt、evidence、trust change 与 graph recompute 在同一事务完成，按 sketch ID 精确幂等；`proof_verified:false`，不授予 L4a/L4b/L5。
- Builder-only `claim.apply_formal_sketch {task_id, observation_id}` 通过受信 CLI/Lean 适配器执行；CLI 从 retained candidate 读取 proof body，Agent 不能提交 proof body、repo、executable 或目标 trust level。
- Broker 新增显式 `FormalSketch` 模式：静态准入仅放行 `sorry`/`admit`，axiom 报告与独立 kernel replay 仅额外接受 `sorryAx`；spec comparator 只对 candidate 放行 `sorryAx`，reviewed spec 仍保持严格。
- Store 正反例覆盖三证据齐全、重放、缺失 probe、spec 不匹配与非 T3 receipt；真实 Lean 4.35.0 端到端验收已验证 L3 写入且不发布 Git commit，并加入 `.github/native-lean.sh`。

## L4a Oracle-backed typed transition（2026-11-28）

- 新增 Builder-only `claim.apply_oracle_backed {task_id, observation_id}` 与受信 CLI `oracle-backed-task`；Agent 不能提交 proof body、repo、executable 或目标 trust level。
- Broker 新增显式 `OracleBacked` 模式：静态准入只放行 `native_decide` / `ofReduceBool` / `trustCompiler`，axiom 报告只接受标准白名单、Lean compiler-trust axiom 或目标限定的 `._native.native_decide.ax_` 生成 axiom；拒绝 `sorryAx` 与任意用户 axiom，且不运行独立 kernel replay。
- spec comparator 的 `oracle` 模式只对 candidate 放行上述 compiler-Oracle axiom，reviewed spec 仍严格；comparison 使用独立身份域，既有 T1/T3 身份保持兼容。
- Store 要求 OracleBacked candidate、sealed spec comparison、retained `NoCounterexampleWithinBounds` probe 与 live Claim/repo/HEAD 全部匹配，原子写入 evidence、receipt、L4a 和 `oracle_debt=true`；`proof_verified:false`，不授予 L5。
- Store、假 CLI/UDS 与真实 Lean 4.35.0 `native_decide` 端到端验收已通过，验证不发布 Git commit；测试加入 `.github/native-lean.sh`。L4b 保持默认 `FORBIDDEN`，没有伪造 ADR 解锁或跨 ITP 桥接。

## Rocq / Agda 原生验收（2026-11-28）

- 本机 macOS arm64：原生 Agda 2.8.0.2、Rocq 9.3.0、同安装 `coqchk` 9.3.0；所有验证均由 Broker 自行施加 Seatbelt，外层会话不套沙箱。
- Agda 原生 promotion 正反例通过；Homebrew `bin/agda` 是 wrapper，验收使用其 `libexec` 下 Mach-O 二进制，并只授予 GHC/Agda 运行库、GMP 与 ICU 目录/文件。
- Rocq Broker 3 项、CLI 3 项（含无效证明拒绝、T1 合入/Golden、精确恢复）、T2 `coqchk` 1 项、daemon frozen-slot 1 项全部通过。
- 修复 `coqchk` 初始化失败：显式在私有可写 scratch 作为 cwd，并仅 literal 授予所属安装根目录与 `bin` 目录项及只读 `vfs.disk-space`；没有递归开放安装目录、源码、插件或 ambient load path。
- 复现入口：[`.github/native-rocq-agda.sh`](.github/native-rocq-agda.sh)。T2 仍是只读咨询观察，不签发 receipt、不写信任、不发布 Golden，也不是发布门禁。

## 尚未完成

- Wiki 索引文件自身的完整路径/ACL 防护仍需独立核对，不等于 Markdown 输入已受保护。后续验收见[交付清单](delivery-roadmap.md)。
- 常驻模型循环未配置生产凭据或部署为自动启动的服务。
- DSH 工作台仍待补齐 Guest / 登出态浏览器实测、非 `chief` 多角色独立生产 Preset 生命周期验收，以及托管 daemon 启用 `--proof-targets` 时与 macOS Seatbelt 嵌套的专项核对；Owner 仍仅保留本地签名归档闭环，无远程写入口。
- Linux 仍无可用验证后端；需实现并验收 launcher、cgroup 实际入组与回收、全主机配额。
- Agda 独立 T2 尚未实现；Agda 原生 promotion 与 Rocq T1/T2/daemon 原生验收已于 2026-11-28 通过。Lean 全仓覆盖与强制发布门禁尚未验收。
