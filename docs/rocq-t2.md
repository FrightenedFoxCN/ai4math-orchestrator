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

**验收状态：原生成功路径未通过。** 本机外层沙箱拒绝嵌套 Seatbelt；外层放行后，Broker 自身 Seatbelt 下的 Rocq 9.3 编译已走到独立检查阶段，但 `coqchk` 初始化返回 `Sys_error: Operation not permitted`。没有放松隔离或输出成功观察。此入口保持实验性、手动且不参与发布门禁。
