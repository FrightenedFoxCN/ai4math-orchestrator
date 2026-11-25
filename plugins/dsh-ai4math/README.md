# AI4Math workspace / AI4Math 工作台

## English

This local bundle targets **DeepSeek Harness 0.2.2-rc.5** on the Desktop/Web surface with the existing Typert registry, gateway, and browser layout/sidebar services. It connects to an already-running `ai4mathd` through a local Unix socket. No daemon, model task runner, or agent preset is started by the bundle, and it does not grant extra access to other principals.

- Adding the bundle makes the `dsh-ai4math` row available, but that row is **disabled by default**. For a future installation, use **Plugins → Add plugin** with this package directory or a packed tarball. Before explicitly enabling the `dsh-ai4math` row, configure `socketPath` as the absolute path to the local `ai4mathd` Unix socket (or provide `AI4MATH_SOCKET` in the DSH Host environment). Keep `rolePresets` and `proofSlots` empty for UI-only use. No agent tooling or proof slots are added by default.
- After enabling, authenticated Owners can use the bottom status bar and **Briefing**, **Frontier**, **Inbox**, and **Wiki** in the right sidebar. The UI exposes no Owner approval or project-changing workflow (proof submission, promotion, pivot/archive, or review checkpoint advancement). **Peek** previews a Fact Pack; **Pull and retain** explicitly saves a local copy without advancing the human review checkpoint.
- To stop the integration, disable the `dsh-ai4math` row. This does not stop `ai4mathd` or remove data already retained by it.

From this directory in the source checkout, `npm run test:package` checks metadata and the tarball file list via an offline dry run. From a DSH Desktop session shell, the command below also checks the metadata with the installed Loader, without loading the plugin or profile. Neither check installs the bundle or verifies the live UI.

```bash
ELECTRON_RUN_AS_NODE=1 "$DSH_NODE_EXECUTABLE" test/installed-bundle-metadata.mjs "$DSH_INSTALL_DIR"
```

## 中文

此本地 bundle 面向 **DeepSeek Harness 0.2.2-rc.5** 的 Desktop/Web，复用现有的 Typert registry、gateway 以及浏览器布局和侧栏服务。它通过本地 Unix socket 连接已经运行的 `ai4mathd`。bundle 不会启动 daemon、模型任务运行器或 Agent preset，也不会向其他身份授予额外访问权限。

- 添加 bundle 后，可以看到 `dsh-ai4math` 插件行，但该行**默认禁用**。日后可在 **Plugins（插件）→ Add plugin（添加插件）** 中选择此包目录或打包后的 tarball。在显式启用 `dsh-ai4math` 前，请将 `socketPath` 配置为本机 `ai4mathd` Unix socket 的绝对路径，或在 DSH Host 进程环境中提供 `AI4MATH_SOCKET`。仅使用 UI 时，请保持 `rolePresets` 和 `proofSlots` 为空；默认不会为 Agent 挂载工具或开放证明槽位。
- 启用后，通过认证的 Owner 可使用底部状态栏和右侧栏的 **Briefing（简报）**、**Frontier（前沿）**、**Inbox（收件箱）** 和 **Wiki**。当前 UI 不提供 Owner 审批或修改项目状态的流程，例如提交证明、提升、转向/归档或推进人工审阅检查点。**Peek** 只预览 Fact Pack；**Pull and retain** 会按用户操作在本地留存一份副本，但不会推进人工审阅检查点。
- 禁用 `dsh-ai4math` 插件行即可停止集成；禁用不会停止 `ai4mathd`，也不会删除此前留存的数据。

在源码 checkout 的本目录运行 `npm run test:package`，可离线检查元数据及 dry-run 打包文件清单。也可在 DSH Desktop 会话的终端运行英文部分的命令，使用已安装 Loader 校验元数据。这两项检查均不会安装插件或验证实际 UI。
