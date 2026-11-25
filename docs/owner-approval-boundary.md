# Owner 审批写闭环：信任边界

本页是实现约束，不是已部署的写接口。现有 [Pivot 操作者命令](../crates/ai4math-cli/src/pivot.rs)继续工作；现有 [Typert 服务](../plugins/dsh-ai4math/host/index.mjs)没有暴露 Pivot 写入。

## 本地签名协议试点

`owner-archive-preview ROOT BARE_REPO FACT_SHA256 BREAKER REASON` 仅生成归档单个熔断的预览（含 `adr_sha256`）。它要求仓库已有协调文件，不创建锁文件、不写 Git 主干或追加账本；SQLite 只读连接仍可能使用 WAL 同步文件。不会从 Remote 执行该命令。

`owner-archive-signing-message ROOT BARE_REPO BASE ADR_SHA256 REQUEST_ID EXPIRES_UNIX` 输出精确签名字节。消息绑定规范化项目根、裸仓库路径、Git 基点、ADR 摘要、请求 ID、有效期和固定的 `archive_breaker` 操作。部署者在独立操作者通道审核并签署字节，OpenSSH 签名使用 `ai4math-owner-archive-v1` 命名空间。

`owner-archive-verify ROOT BARE_REPO BASE ADR_SHA256 REQUEST_ID EXPIRES_UNIX TRUSTED_PUBLIC_KEY_FILE SIGNATURE_FILE` 是**离线诊断**：仅验证相对于操作者已核对的 Ed25519 公钥和消息的签名，不消费请求、不查询计划是否仍有效、也不执行归档。这里的公钥路径由本地操作者选取，绝不能作为未来 Remote 请求的信任输入。签名证明密钥控制，不自动证明是谁在场或硬件确认。部署须独立配置固定信任锚及人类操作边界；仍未启用 GUI 写入。

## 已核对的宿主边界

DSH 0.2.2-rc.5 的 Typert invocation 可检查认证后、宿主注入的 `peer.principal.kind === 'owner'`；请求 JSON 不提供身份。本版本的 Client Connection 为已认证浏览器复用 Owner peer，也可给准入 API peer 注入同一 Owner 角色；peer ID 和角色均不能证明某个自然人在这次操作上刚作确认。通用 DSH Approval/Questions/Card 接线也不天然绑定本项目的 Git 快照和变更内容。不能把点击、拥有 Owner 会话、请求正文中的角色字段、TTY，或可公开计算的 ADR 摘要当作本项目写授权。

## 最小实现顺序

1. **预览**：Owner 可取有界预览。由当前本地 CLI/Git/账本生成，显示仓库、当前主干、目标、影响范围和 ADR 摘要。预览没有写权限，也不保留某个仍可写的 Git 锁。
2. **确认**：独立受保护的操作者通道审批这份精确预览；必须能区分 Owner 会话和未获授权的 Agent。候选方案是独立持有且需用户交互的签名凭据，绑定项目、操作类型、当前主干、ADR 摘要、一次性 ID 和截止时间；凭据注册及实际交互方式需单独验收。若目标环境不能建立此边界，仅允许操作者通过原 CLI 执行，不启用 GUI 写入口。
3. **执行**：先用唯一请求 ID 持久记录消费和对应现有 Pivot 意图，再在受管 Git 锁内重新核对主干、账本、完整计划和门禁；调用同一恢复协议，不实现第二条 Git 写通道。过期、重复、错仓库、主干变化、被篡改的计划均拒绝。不提供接受任意命令、路径或参数的代理。
4. **恢复**：断线或确认丢失后首先查询同一请求 ID 的持久状态。只恢复已开始的同一意图；不重新批准或生成另一个 ADR。不因网络失败自动重发执行。`recover` 可能改变状态，它与 `apply` 一样属于受保护通道；纯状态查询不替它执行。

## 执行端仍需实现的持久约束

- 信任锚由部署者配置，调用者不能提交或替换公钥。应支持明确吊销；首次开始执行前同时核验当前信任锚、有效期、消息和完整实时预览。
- `request_id` 去重应与现有 `PivotIntent` 一起提交并绑定 `adr_id`，而不是先独立消费确认再尝试创建意图。Git 本身仍通过既有 pin 和 CAS 恢复：准备 Git 对象未入账只会留下待清理对象；已写入意图则只恢复同一对象。过期前从未消费的确认不得启动；已消费且合法开始的意图可在过期后恢复，不重新把旧签名当新授权。
- 直接 CLI 路径与未来受确认路径共用业务核验，但不是同一威胁模型。拥有同 UID 直接修改仓库、账本、工具执行环境或信任锚的进程不由此协议隔离。验收需验证部署上的身份与文件边界，而不只测试签名数学正确。

## 验收门禁

- 从 Guest、未映射 Agent 和普通工具面均无法调用确认或执行；未配置独立操作者边界时不注册写接口。
- 并发重复确认最多落一条 ADR；已执行但响应丢失后只读找回结果；Git/SQLite 中间中断按现有意图恢复。
- 真正的同一 Host 页面另验登录/登出、布局、Owner 再认证或独立确认、取消、断线、禁用及卸载；安装版 SDK 夹具或 VM 不能代替这些验收。
