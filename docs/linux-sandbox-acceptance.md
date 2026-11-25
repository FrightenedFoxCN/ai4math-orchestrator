# Linux Broker：实现边界与验收门禁

当前 Linux 验证入口仍拒绝启动，不降级为裸进程。已有 `/proc/meminfo` 的有界解析可为快速通道提供宿主压力估计；这不是容器或 cgroup 配额，也不会启用验证后端。macOS 保留 Seatbelt 权限和监督行为。进程监督与平台授权已分开，无法启动 launcher 的错误不进入数学验证拒绝分支。

## 启用一个 Linux verifier 前

1. 从空根建立强制隔离的 namespace，固定受管 launcher 与可信工具链清单；不整棵挂载宿主 `/`、`/usr`、`/proc` 或 `/sys`。普通 `ro-bind` 的只读属性不意味着 `noexec`。数据、房间和 scratch 须独立、不可执行，可信动态加载器及库的映射与允许执行程序的授权须分别核验。scratch 还需总容量上限。
2. 递归数据、单文件和 literal 目录授权不得互换。对 literal 目录可先尝试合成节点并仅投影获准的子项；如真实 verifier 依赖宿主目录枚举，保持该 verifier 不可用，不能放开其兄弟路径。禁止将不支持的 runtime 授权静默丢弃。
3. 禁止网络与进程创建；若需要线程，以实测说明允许的 syscall 形态。用独立可信启动状态把策略拒绝、启动阶段错误、资源终止和可归因的 verifier 退出分开，不能依据退出码或诊断文本猜测 launcher 是否已启动目标。
4. 在第一条不可信指令前加入已委派的独立 cgroup v2；验证内存、PID 和 CPU 限制已生效，保留墙钟截止。正常、超时、取消、输出超限及 OOM 均应回收 cgroup，确认为空之前不签发成功。服务层还需能在 Broker 异常退出后清理。限制缺失、确认失败或回收失败均拒绝。

## 验收证据

- 本机 Darwin 可测试压力解析和错误类型，不能证明 Linux 强制隔离。本轮在外层工具沙箱运行原生 Seatbelt 截止时间用例，启动报 `sandbox_apply: Operation not permitted`，未取得该次原生验收；Docker CLI 存在，但所配置引擎的 socket 不存在，未启动 Linux 宿主。
- 需在允许非特权 user namespace、且有 cgroup v2 委派的 Linux 宿主上验证上述隔离，以及数据不能可执行映射、拒绝宿主目录/网络/派生进程、限制/回收的成功和失败路径。应逐 verifier 验证实际 runtime，不能用 shell smoke 替代 Lean/Agda/Rocq。
- 独立模型/任务启动器有自己的平台边界。添加一个 verifier 后端不意味着整个 Broker 或独立任务在 Linux 已可部署。
