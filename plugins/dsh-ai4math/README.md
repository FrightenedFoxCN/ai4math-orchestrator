# AI4Math Workbench / AI4Math 工作台

## English

This local bundle targets **DeepSeek Harness 0.2.2-rc.5** on the Desktop/Web surface, reusing the existing Typert registry, gateway, layout, sidebar, workspace, and locale services. It delivers an integrated AI4Math Workbench, a bottom Live HUD, four right-sidebar companion tabs, and a built-in **AI4Math Chief** Agent Preset (`ai4math-chief`).

- **Install & enable**: Run `pnpm install` once inside this directory before adding it via **Plugins → Add plugin** (the four Host packages `@deepseek-ai/dsh-scope`, `@deepseek-ai/dsh-typert-protocol`, `@deepseek-ai/schemastery`, and `zod` are declared in `dependencies`; packed tarballs carry the same declaration). Adding the bundle registers the `preset-ai4math-chief` preset row and inserts `dsh-ai4math` **disabled by default** with `rolePresets: { chief: 'ai4math-chief' }` and empty `proofSlots`.
- **One-click Workbench (`AI4Math` in left navigation)**: After enabling `dsh-ai4math`, open **AI4Math** from the left sidebar (or click `AI4Math` in the bottom HUD). From the Workbench you can:
  - Pick or create a local math project folder (`lean4` / `agda` / `rocq`), with live Unix socket byte-length checks (`≤ 103` bytes on macOS/BSD, `≤ 107` on Linux) and automatic `ai4math init` or adoption of existing projects.
  - Manage multiple projects (`~/.dsh/ai4math/workbench.json`), hot-switch active projects without restarting DSH, and either supervise a Host-managed `ai4mathd` or attach read-only to an externally started `ai4mathd` (external daemons are never killed by DSH).
  - Launch a new conversation pre-bound to the project directory and the `ai4math-chief` preset in one click (**New Chief Session**), or jump back to linked conversations.
- **In-conversation companion & language**: The bottom Live HUD and right-sidebar **Briefing**, **Frontier**, **Inbox**, and **Wiki** tabs follow DSH's active language (`en` / `zh`) reactively. When the active conversation directory matches another registered AI4Math project, a compact switch chip appears in the session header and HUD.
- **Trust boundary**: Only authenticated Owners can access the Workbench and remote endpoints. No Agent role receives Workbench control endpoints; unmapped presets receive no `ai4math_*` tools. The UI exposes no proof promotion, pivot/archive signing, or human checkpoint advancement—Breaker and Inbox cards provide copyable read-only operator CLI templates only. **Peek** previews a Fact Pack; **Pull and retain** saves a local copy without advancing the human review checkpoint.
- **Stop / uninstall**: Disabling `dsh-ai4math` cleanly stops any Host-managed `ai4mathd`, removes all UI slots/subscriptions, and never stops external daemons or deletes project files/ledgers.

From this directory, `npm test` verifies bundle consistency (`client/build.mjs --check`), Host/Client unit & VM tests, and package manifest rules. From a DSH Desktop session shell, the command below validates the bundle patch and metadata against the installed Loader without mutating any profile:

```bash
ELECTRON_RUN_AS_NODE=1 "$DSH_NODE_EXECUTABLE" test/installed-bundle-metadata.mjs "$DSH_INSTALL_DIR"
```

## 中文

此本地 bundle 面向 **DeepSeek Harness 0.2.2-rc.5** 的 Desktop/Web，复用现有的 Typert registry、gateway、布局、侧栏、工作区与语言服务，提供一体化的 **AI4Math 工作台**、底部实时状态栏（Live HUD）、四个右侧栏伴随面板，以及内置的 **AI4Math Chief** Agent Preset（`ai4math-chief`）。

- **安装与启用**：按目录安装前，请先在本目录执行一次 `pnpm install`（Host 依赖 `@deepseek-ai/dsh-scope`、`@deepseek-ai/dsh-typert-protocol`、`@deepseek-ai/schemastery`、`zod` 均声明在 `dependencies` 中；打包 tarball 带有相同声明），然后在 DSH **插件 → 添加插件** 中选择此目录或 tarball。添加后会自动注册 `preset-ai4math-chief` 预设，并插入**默认禁用**的 `dsh-ai4math` 插件行（默认配置 `rolePresets: { chief: 'ai4math-chief' }`、`proofSlots: {}`）。
- **一键工作台（左侧导航栏 `AI4Math`）**：启用 `dsh-ai4math` 后，从左侧栏点击 **AI4Math**（或点击底部 HUD 左侧的 `AI4Math`）即可打开全屏工作台：
  - 通过原生文件夹选择器新建或接管本地数学项目（支持 `Lean 4` / `Agda` / `Rocq`），实时校验 Unix socket 路径字节上限（macOS/BSD ≤ 103 字节，Linux ≤ 107 字节），空目录自动执行 `ai4math init`，已有目录直接接管。
  - 多项目持久化在 `~/.dsh/ai4math/workbench.json`，支持热切换活跃项目（无需重启 DSH），既可由 Host 托管启停 `ai4mathd`，也可自动识别并只读挂载外部终端启动的 `ai4mathd`（DSH 绝不终止外部进程）。
  - 点击 **新建 Chief 主编排会话**，一键创建并进入已绑定项目目录与 `ai4math-chief` 预设的新对话，或在工作台总览中直达该项目的关联历史会话。
- **对话内伴随视图与多语言**：底部 Live HUD 与右侧栏 **简报 / 前沿 / 收件箱 / Wiki** 实时跟随 DSH 语言设置（中文 / 英文）无刷新切换。当当前会话目录命中另一个已注册的数学项目时，会话顶栏与 HUD 会自动提示一键切换。
- **信任边界**：工作台与远端端点仅向已认证 Owner 开放；任何 Agent 角色均不可调用工作台控制端点，未映射的 Preset 不会获得任何 `ai4math_*` 工具。UI 不提供证明合入、Pivot/归档签名或推进人工检查点的写接口——熔断与收件箱卡片仅提供可复制的只读操作者 CLI 命令模板。**预览**不落盘，**拉取并留存**仅保存本地副本而不推进检查点。
- **停用与卸载**：禁用 `dsh-ai4math` 会干净回收 Host 托管的 `ai4mathd` 并移除所有面板与订阅，绝不终止外部 daemon，也不删除磁盘上的项目数据或账本。

在源码 checkout 本目录执行 `npm test` 可运行 bundle 一致性校验及全部 Host/Client/打包测试；在 DSH Desktop 会话终端运行英文部分的命令可用已安装 Loader 校验元数据。
