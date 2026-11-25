# Linux Broker：实现边界与验收门禁

当前 Linux 验证入口仍拒绝启动，不降级为裸进程。已有 `/proc/meminfo` 的有界解析可为快速通道提供宿主压力估计；这不是容器或 cgroup 配额，也不会启用验证后端。macOS 保留 Seatbelt 权限和监督行为。进程监督与平台授权已分开，无法启动 launcher 的错误不进入数学验证拒绝分支。

## 只读配额预检

`ai4math linux-cgroup-preflight ABSOLUTE_LEAF` 接受由操作者预备的真实 cgroup v2 空叶节点，检查 domain、有限内存/PID/CPU 上限以及 `cgroup.kill` 是否存在（缺失时报告 `false`）。路径核验依赖 Linux `openat2`（5.6 起）；接口不可用时拒绝。不会创建 cgroup、写入任何控制文件、移动进程或发信号。输出固定包含 `backend_enabled: false` 和 `delegation_verified: false`。这仅是诊断采样，不证明可写委派、祖先限额、swap 上限、实际进程已入组，也不能保证报告后限额不变。

在已有准备好的 Linux 宿主上，可运行下面的只读集成用例；测试本身不会配置主机：

```sh
AI4MATH_TEST_CGROUP_LEAF=/sys/fs/cgroup/your-delegated-leaf \
  cargo test --locked -p ai4math-cli --test linux_cgroup \
  native_cgroup_preflight_reports_limits_without_enabling_verification -- --ignored --exact
```

## 挂载计划编译器

[Linux mount plan](../crates/ai4math-broker/src/linux_mount_plan.rs) 是纯策略层，既不访问源路径也不启动进程。它将唯一 verifier、逐文件许可的动态库、递归/单文件数据、只读工作目录和唯一可写 scratch 分开。Literal 目录及隐式父节点在空根内合成，不挂载宿主目录。重复、重叠、保留路径和超限输入拒绝；唯一允许的物理重叠是工作目录下独立的直接子目录 scratch。

计划要求所有数据和 scratch 为 `noexec`，所有挂载为 `nosuid,nodev`，只有 scratch 可写。动态库所在挂载允许可执行映射，不等于允许将库作为程序执行。后端尚未接线：未来 launcher 须独立固定和验明源路径/文件类型、排除子挂载和别名、落实最终内核挂载属性及唯一可执行入口；不能仅凭计划输出启动 verifier 或签发回执。

## 启用一个 Linux verifier 前

1. 从空根建立强制隔离的 namespace，固定受管 launcher 与可信工具链清单；不整棵挂载宿主 `/`、`/usr`、`/proc` 或 `/sys`。普通 `ro-bind` 的只读属性不意味着 `noexec`。数据、房间和 scratch 须独立、不可执行，可信动态加载器及库的映射与允许执行程序的授权须分别核验。scratch 还需总容量上限。
2. 递归数据、单文件和 literal 目录授权不得互换。对 literal 目录可先尝试合成节点并仅投影获准的子项；如真实 verifier 依赖宿主目录枚举，保持该 verifier 不可用，不能放开其兄弟路径。禁止将不支持的 runtime 授权静默丢弃。
3. 禁止网络与进程创建；若需要线程，以实测说明允许的 syscall 形态。用独立可信启动状态把策略拒绝、启动阶段错误、资源终止和可归因的 verifier 退出分开，不能依据退出码或诊断文本猜测 launcher 是否已启动目标。
4. 在第一条不可信指令前加入已委派的独立 cgroup v2；验证内存、PID 和 CPU 限制已生效，保留墙钟截止。正常、超时、取消、输出超限及 OOM 均应回收 cgroup，确认为空之前不签发成功。服务层还需能在 Broker 异常退出后清理。限制缺失、确认失败或回收失败均拒绝。

## 验收证据

- 本机 Darwin 可测试压力解析和错误类型，不能证明 Linux 强制隔离。本轮在外层工具沙箱运行原生 Seatbelt 截止时间用例，启动报 `sandbox_apply: Operation not permitted`，未取得该次原生验收；Docker CLI 存在，但所配置引擎的 socket 不存在，未启动 Linux 宿主。
- 需在允许非特权 user namespace、且有 cgroup v2 委派的 Linux 宿主上验证上述隔离，以及数据不能可执行映射、拒绝宿主目录/网络/派生进程、限制/回收的成功和失败路径。应逐 verifier 验证实际 runtime，不能用 shell smoke 替代 Lean/Agda/Rocq。
- 独立模型/任务启动器有自己的平台边界。添加一个 verifier 后端不意味着整个 Broker 或独立任务在 Linux 已可部署。
