# Rocq T2：固定快照的独立对象复核

```text
ai4math rocq-t2-check ROOT BARE_REPO MODULE THEOREM REVIEWED_MAIN NATIVE_ROCQ ROCQ_DATA COMPILER_SHA256 COQCHK CHECKER_SHA256
```

这是操作者主动运行的**只读观察**，不签发 `VerificationReceipt`，不写信任账本、不发布 Golden，也不是发布门禁。它在项目原生重任务租约和 Git 协调锁下读取已经绑定、无待恢复事务的仓库，要求指定提交为当前主干，并在返回前重新核对主干和账本代次。每次在私有目录按已提交顺序从源文件重建对象；不接收外部 `.vo`。独立 `coqchk` 复核新生成的对象；指定定理另外执行封闭假设检查。没有对每个定理做独立的无假设报告。

`COMPILER_SHA256` 固定安装内实际执行的 `rocqworker`，不是其入口驱动文件。`COQCHK` 必须是同一受信 Rocq 安装的独立检查器，另有 SHA-256 固定。工具、系统库与 Rocq 数据是受信任安装的一部分；二进制摘要不认证数据树、证明源码或提交者身份。所有编译和复核均使用 Broker 原生沙箱，没有未隔离的降级路径。受管源码、模块数量、工具输出和墙钟时间均有边界；基础设施或核验失效不产生成功观察。

原生测试入口：

```sh
cargo test --locked -p ai4math-cli --bin ai4math \
  rocq_t2::tests::native_rocq_t2_checks_fresh_objects_without_changing_git_or_ledger \
  -- --ignored --exact
```

设置 `AI4MATH_TEST_ROCQ`、`AI4MATH_TEST_ROCQ_DATA`、`AI4MATH_TEST_ROCQ_SHA256`（worker）、`AI4MATH_TEST_COQCHK` 和 `AI4MATH_TEST_COQCHK_SHA256`。Homebrew 等安装可通过 `AI4MATH_ROCQ_RUNTIME_READS` 为编译器指定必要的绝对动态库路径；独立检查器不继承这些授权。目标宿主必须允许 Broker 自己施加 Seatbelt。

**验收状态：原生成功路径已通过（2026-11-28）。** 在外层无 Seatbelt 的 macOS 会话中，Rocq 9.3.0 编译、独立 `coqchk` 对象复核、目标封闭假设检查、错误 checker 摘要拒绝、Git/ledger 不变性均通过。

本次修复没有扩大数据读取：独立 checker 仍在私有 scratch 当前目录运行；仅额外允许其列出所属安装根目录与 `bin` 目录项（literal read，不递归读取子文件），并允许只读查询 `vfs.disk-space`。源码、对象、Corelib/Stdlib 数据、compiler plugins 与 ambient home/load-path 边界保持不变。该入口仍是操作者主动运行的只读观察，不签发 receipt、不写信任、不发布 Golden，也不是发布门禁。
