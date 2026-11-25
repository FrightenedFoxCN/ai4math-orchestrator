# 实现状态（2026-11-25）

本文件区分“代码已实现”“标准测试已通过”“原生/真实宿主已验收”三种状态，避免把设计目标或本机夹具误读为生产交付。

## 本轮已完成

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

## 尚未完成

- Wiki 索引文件自身的完整路径/ACL 防护仍需独立核对，不等于 Markdown 输入已受保护。后续验收见[交付清单](delivery-roadmap.md)。
- 显式 DeepSeek 任务已有可回收进程；基于持久租约的常驻调度、跨进程重启恢复与生产凭据接线仍待交付。
- Linux bwrap/cgroups、全主机配额、Agda/Rocq 独立 T2、生产 GUI 写闭环仍未交付。
