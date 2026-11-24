# Phase 1：Rust 核心与 CLI 验收

- [x] 核心：强类型编排契约、受限证明提交、信任聚合和静态禁入。
- [x] 状态：SQLite WAL 与原子事件；AND-OR 替代路线、T3 标脏及复检；Wiki 重建与 Shadow Hint。
- [x] Broker：有界 Fast/Heavy 租约、macOS Seatbelt、路径规范化、断网、超时回收；沙箱不可用时拒绝执行。
- [x] 洁净室与合入：真实 Lean 4 / Agda 正反例、哈希绑定回执、加锁回归、Git 比较并交换更新 `main`、APFS 黄金产物发布与克隆。
- [x] CLI：初始化、状态、Wiki 重建、安全自检、合入和快照克隆。

## 验收记录

macOS arm64，Lean 4.35.0、Agda 2.8.0.2。工作区测试包含原生测试共 **27 项通过、0 失败**，另通过 Seatbelt 自检及 `cargo fmt --all -- --check`。真实引擎测试覆盖：

- Lean / Agda：无效证明不推进 `main`；有效证明合入并发布可克隆的黄金产物。
- Lean 多模块：目标通过验证不代表可以合入；下游编译失败时，主干和黄金池均不改动。
- T3：定义变更后用旧 Lean 证明复检，T3 回执与状态事件原子落盘；同名但不同文件的合法回执不能清除标脏。复检失败降级、替代路线保留和并发状态变动由状态机测试覆盖。
- Seatbelt：允许写本次产物目录，拒绝越界读写、创建子进程和本机监听；超时任务被回收。嵌套在 DSH 工具沙箱内时拒绝执行，不回退到裸运行。

## 复现

常规测试：`cargo test --workspace --locked`。原生测试须在**没有外层 Seatbelt** 的 macOS 会话启动，Broker 仍负责隔离验证子进程。指定本机可信的真实编译器，不使用 elan 或 Homebrew 启动脚本。按安装位置补齐下列变量；`AI4MATH_RUNTIME_READS` 是可选的本机依赖库路径列表，只应指向必要的可信运行库。

```sh
export AI4MATH_TEST_LEAN="$(lean --print-prefix)/bin/lean"
export AI4MATH_TEST_AGDA=/absolute/path/to/native/agda
export AI4MATH_TEST_AGDA_DATA="$(agda --print-agda-dir)"
# 若 native Agda 动态链接到额外的库，例如 Homebrew GHC、GMP、ICU，给出明确目录：
# export AI4MATH_RUNTIME_READS=/absolute/path/to/ghc/lib:/absolute/path/to/gmp/lib:/absolute/path/to/icu/lib
cargo run --locked -p ai4math-cli -- self-test
cargo test --workspace --locked -- --include-ignored
```

CLI 参数见 `cargo run -p ai4math-cli -- --help`。合入使用操作者拥有的裸 Git 仓库。单模块可以直接使用；多模块用仓库内的 `ai4math-modules.txt` 按依赖优先顺序列出全部顶层同引擎模块。可信基线中目标模块末尾预留一个 `{{PROOF_BODY}}` 槽，worker 只提交表达式或受限证明体；不得提供工具链、模板或白名单。

## 交付边界

这是受限项目配置下的本机第一阶段：用全量重编覆盖受影响集合，不执行通用 Lake/Agda 生态构建；洁净室以冻结源码和引擎检查为准，Lean 审计目标的公理依赖，Agda 以 `--safe --without-K` 检查。当前回执是进程内不可由 JSON 反序列化构造的哈希绑定记录，不是跨进程签名或完整的设计版回执协议。黄金缓存支持显式 APFS 克隆，尚未接入热池或自动使用缓存跳过终审。

Rocq 洁净室、Linux 隔离后端、独立 T2 内核审计、完整 Comparator 和性能基线、常驻调度、热池及 DSH 插件不在本次验收覆盖内。
