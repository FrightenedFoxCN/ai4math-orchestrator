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
- 新增 graft/replace/commit-graph 回归；原生 graft 恢复用例已编写，但需在非外层 Seatbelt 终端运行。

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

## 尚未完成

- Wiki/Sediment/Fact Pack 写入侧有界读取、ACL/路径替换防护仍需加固。
- 模型任务仍运行在 Host 进程内，无法硬抢占不合作 adapter/Agent。
- Linux bwrap/cgroups、全主机配额、Agda/Rocq 独立 T2、生产 GUI 写闭环仍未交付。
