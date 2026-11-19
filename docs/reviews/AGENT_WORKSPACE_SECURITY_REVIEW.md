# Agent 与 MCP 工作区权限审核

## 目标权限边界

| 路径 | 桌面 Agent 的 PowerShell | MCP |
| --- | --- | --- |
| 当前 DUT 的工作目录 | 读写 | 按工具声明和 DUT 授权 |
| 其他 DUT 的资料目录 | 只读 | 通用监听器按现有 Read 授权读取，DUT 专用监听器仅访问绑定 DUT |
| 其他宿主目录和凭据 | 拒绝，运行时所需的系统和工具文件单独只读放行 | 无通用文件系统或 PowerShell 工具 |

## 现状与缺口

- **外部 CLI 已接入宿主的受限令牌启动。** `apps/desktop/src/agent_host/cli.rs` 的 `prepare_cli_sandbox` 验证受限令牌和 Job 限额，按当前 DUT 准备单元后通过 `spawn_streaming` 启动。CLI 和托管 goose 共用 `agent_policy`。这一步建立了进程级约束，读取范围仍取决于启动账号和现有 ACL。
- **托管 goose 的只写隔离能力不足以满足读边界。** `apps/desktop/src/agent_host/acp.rs` 和 `host.rs` 使用 `SandboxPolicy::agent_cell`。该策略使用 `WRITE_RESTRICTED`，额外授权同一 DUT 的运行时状态目录，并使用宿主网络。限制 SID 仅参与写入检查，读取范围仍依赖启动账号和现有 ACL。身份初始化、ACL 或能力探测结果需要按实际启动身份核验。
- **脚本沙箱存在独立接口。** `crates/duthub-sandbox/src/policy.rs` 中的 `script_cell` 关闭网络并保护显式列出的目录。生产启动路径没有调用 `run_script`。对少数目录设置拒绝读取 ACL 也不足以证明其余宿主目录都不可读。
- **MCP 资料访问已有独立约束。** `crates/duthub-mcp/src/server.rs` 的 `authorize_scoped_read` 约束 DUT 专用监听器，`crates/duthub-core/src/dut_artifacts.rs` 使用能力目录句柄和 DUT 归属标记打开资料。MCP 的写动作走各自的权限与租约检查。桌面 Agent 直接读取其他 DUT 资料的需求可独立实现，专用监听器继续遵循现有授权。

## 方案取舍

附件将 Rust 内置文件访问与任意命令隔离分开是合理的。继续用 `cap-std` 做结构化文件工具，让 Windows 的进程隔离约束 PowerShell 及其所有子进程。对于附件建议的 AppContainer 或 LPAC 方案，应以实际进程的 ACL 生效范围和工具兼容性为验收依据。Job Object 负责回收进程和约束资源。当前的 Windows 隔离代码已经直接封装系统 API，先验证能力缺口，再决定是否需要引入新封装。Linux 的 namespace 和 Landlock 可在目标平台单独评估。

Windows 优先验证 Copilot CLI 使用的 ProcessContainer 能否在项目支持的 Windows 版本上实现明确的读写允许列表。将当前 DUT 和该会话所需的私有状态目录标记为读写、其他 DUT 的资料目录标记为只读。必需的 Windows 和工具目录单独授权。将任何运行时必须使用的临时目录纳入同一会话的私有写范围。部署前检查其系统更新要求和可用性。Codex 的 Windows 实现可以参考其专用身份、路径授权及会话隔离方式。独立验证同一机制对 PowerShell 和各 Agent 运行时均生效。[Copilot 本地沙箱](https://docs.github.com/en/copilot/concepts/agents/copilot-cli/understanding-local-sandboxing) · [Codex Windows 沙箱代码](https://github.com/openai/codex/tree/main/codex-rs/windows-sandbox-rs)

继续从共用启动入口传入稳定 DUT ID 和宿主生成的权限策略。仅在宿主能够验证完整策略时开放受此限制的 PowerShell。验证至少覆盖当前 DUT 写入、其他 DUT 读取、其他 DUT 写入被拒绝、工作区外读写被拒绝、子进程继承权限，以及 DUT 路径换名、重解析点与会话取消。使用临时测试目录和无敏感信息的夹具。

