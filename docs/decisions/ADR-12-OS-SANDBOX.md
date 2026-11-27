# ADR-12：基于 Microsoft MXC SDK（`mxc-sdk`）的 OS 级沙箱架构

- 状态：Accepted（2026-11-27 重构落地，取代 2026-11-25 双身份 Win32 FFI / Codex 方案）
- 历史分支：旧版双身份实现已完整保留在 `sandbox` 分支（commit `49fe697`）；`main` 分支不保留旧版兼容层。
- 相关：`docs/decisions/ADR-11-OS-ISOLATION.md`、`AGENTS.md`（2026-11-27 MXC 重构节）
- 代码：`crates/duthub-sandbox/`（`#![forbid(unsafe_code)]`，纯安全 Rust 封装 `mxc-sdk = "1.1.0"`）、
  `apps/desktop/src/agent_host/{sandbox.rs, acp.rs, cli.rs, host.rs}`、`apps/desktop/src/pages/agent_panel.rs`

## 1. 背景与决策摘要

2026-11-25 的旧版沙箱参考 `openai/codex` 的 `windows-sandbox-rs` 实现，维护约 5,000 行 `unsafe` Win32 FFI
（`CreateRestrictedToken` + `CreateAppContainerProfile` + `NtCreateFile` + DPAPI 专用本地账户 + `icacls` 回读）
以及 `duthub-agent` 内 ~1,675 行 PowerShell AST 预检层。其根本瓶颈在于：

1. 非提权桌面进程无法跨账户 `CreateProcessAsUser`（Win32 1314），导致**读隔离在本机始终无法交付**，
   T3（拒读 `.duthub/settings.json`）与 T4（拒读兄弟 DUT 单元）只能 SKIP；
2. 桌面 UI 只能长期硬编码 `SandboxBoundaryProof::Unverified`；
3. 大量手写 `unsafe` FFI 与旧 Windows 兼容分支带来高维护成本。

随着本机及目标运行环境定位为**最新 Windows 11（原生 PSEC 1.1+，无需兼容 Win7/Win10/早期 Win11）**，
并采用微软官方发布的 `mxc-sdk = "1.1.0"`（`mxc_sdk::v1`），上述限制全部消除：

| 维度 | 旧版（`sandbox` 分支） | 新版（`main`，`mxc-sdk 1.1.0`） |
| --- | --- | --- |
| Windows 内核边界 | 受限令牌（仅写）+ AppContainer + 专用账户（非提权 1314） | **原生 Windows 11 ProcessContainer（PSEC 1.1+）**，非提权即可同时强制读/写/网络/UI 隔离 |
| 读隔离（T3/T4） | 非提权不可用（SKIP） | **内核原生 `deniedPaths` + deny-by-default 全绿通过，零 SKIP** |
| 跨平台支持 | 非 Windows 一律 `Unsupported` | Linux `Bubblewrap` / macOS `Seatbelt` 由 `mxc-sdk` 原生提供 |
| `duthub-sandbox` unsafe 代码 | ~5,000 行手写 Win32 `unsafe` | **0 行（`#![forbid(unsafe_code)]`）**，零 `windows-sys` 依赖 |
| 进程树终止 | 手写 Job Object + 回退逻辑 | `mxc-sdk` 内置 `UiJobObject`（`KILL_ON_JOB_CLOSE` + `TerminateJobObject`） |
| 拒绝诊断 | 无 | Windows PSEC Learning Mode `capture_denials` 结构化 JSON/ETL 报告 |
| Codex 参考实现 | `crates/duthub-sandbox/src/windows/*` + `duthub-agent/{policy,ps_ast,sandbox_exec}.rs` + `NOTICE` | **全部彻底移除** |

## 2. 模块边界与唯一安全执行层

```text
crates/duthub-sandbox/            #![forbid(unsafe_code)]，仅依赖 mxc-sdk / base64 / uuid
  src/lib.rs                      对外 API 导出与禁止 unsafe 声明
  src/policy.rs                   SandboxPolicy / CellSpec / NetworkPolicy / UiPolicy /
                                  DenialCaptureConfig / validate_policy / format_command_line /
                                  PowerShell 编码与危险命令预检
  src/status.rs                   SandboxBackendKind / SandboxStatus（基于 mxc_sdk::v1::platform_support
                                  + available_backends + probe，零子进程启动）
  src/runner.rs                   prepare_cell / probe_request / run / spawn_streaming /
                                  run_script / run_agent / SandboxedProcess / StreamCloser /
                                  DenialCaptureSummary
tests/security_invariants.rs      T1–T8 + Agent 读写双隔离 + PSEC 能力 + capture_denials（零 SKIP）
tests/streaming_spawn.rs          长驻流式管道往返、读写边界、Drop 杀整棵进程树
tests/source_contract.rs          钉住 #![forbid(unsafe_code)]、零 windows-sys、无裸 Command 回退
```

### 2.1 策略设计要点

- **文件系统**：
  - `SandboxPolicy::agent_cell(cell, allowed_exe_dir)`：授予 `<cell>` 读写，自动拒读宿主 `.duthub/`（含 `settings.json`/`duts.json`）和兄弟 DUT 目录；
  - **`.duthub/tools/<tool>/` 共存规则**：当 Agent 可执行文件（如下载的 `goose.exe`）位于 `.duthub/tools/<tool>/` 时，`CellSpec::protected_paths_excluding(allowed_exe_dir)` 展开 `.duthub/` 兄弟条目进行精准拒绝，同时授予 `<tool>` 目录只读，既保护凭据配置又允许执行已下载工具；
  - CLI `.cmd`/`.bat` 垫片通过固定 `%SystemRoot%\System32\cmd.exe /D /C call <shim>` 启动时，将 `plan.program.parent()` 加入 `readable`，避免 deny-by-default 容器下 `cmd.exe` 无法读取垫片。
- **网络策略（`NetworkPolicy`）**：
  - `Deny`（脚本默认）：egress 拒绝 + ingress 拒绝 + `host_loopback = Deny`（内核 WFP 拦截回环 TCP/ICMP）；
  - `LoopbackOnly`：仅允许 `127.0.0.1/32` 与 `::1/128` 双向回环；
  - `LoopbackAndOutbound { proxy_url }`（Agent 默认）：允许回环 MCP + 外网 LLM API（或经显式 `network_proxy` 强制转发），所有非回环入站拒绝。
- **UI 隔离（`UiPolicy`）**：
  - 默认 `block_win32k: false`（避免 PowerShell / WinHttp / GUI 链接运行库在 DLL 初始化期触发 `0xC0000142`），同时保持剪贴板读写禁止、输入注入禁止、桌面切换/句柄/全局 Atom/IME/系统设置隔离开启。
- **环境变量隔离**：
  - 统一设 `inherit_default_environment = Some(false)`；
  - Windows PSEC 在禁止默认环境继承时强制要求子进程环境块包含 `SYSTEMROOT` 与 `LOCALAPPDATA`（否则 Win32 203 拒启），因此 `AcpClient` 将 `LOCALAPPDATA` 显式重定向至 `<cell>/home/AppData/Local`，`TEMP`/`TMP` 重定向至 `<cell>/tmp`，绝不继承宿主 `USERPROFILE`/`APPDATA`/`HOME`/`HOMEDRIVE`/`HOMEPATH`。

## 3. 桌面集成与 UI 诚实呈现

1. **统一沙箱准备入口**：`apps/desktop/src/agent_host/sandbox.rs::prepare_agent_sandbox` 供 ACP（`host.rs`）与 CLI（`cli.rs`）共用，fail-closed 检查 `duthub_sandbox::status().supports_agent_isolation()`、建目录、构建并校验策略；删除了 `host.rs` 与 `cli.rs` 中重复的 `prepare_*_sandbox`/`apply_*_isolation` 与 `job.rs` 中 ~260 行废弃的 Win32 Job 启动代码。
2. **真实状态接线**：`Workbench::cached_sandbox_status`（10s TTL）直接查询 `duthub_sandbox::status()`，`agent_ui::sandbox_boundary_proof(&status)` 根据真实能力推导 `VerifiedByBackend` / `Unverified`；`agent_panel.rs` 与 `agent_inspector_panel` 全量走 `crate::i18n` 本地化展示后端名称及已启用能力（文件系统隔离 / 网络策略 / UI 隔离 / 拒绝诊断），彻底移除历史硬编码 `Unverified` 与中文硬编码。

## 4. 安全不变量验收（Windows 10.0.26300 x64，普通用户非提权实测）

| 用例 | 验收内容 | 结果 |
| --- | --- | --- |
| T1 | 单元内写入 `inside.txt` | **通过** |
| T2 | 单元外写入 `%USERPROFILE%` 被内核拒绝 | **通过** |
| T3 | 读取宿主 `.duthub/settings.json` 被 PSEC 1.1 `deniedPaths` 拒绝 | **通过（零 SKIP）** |
| T4 | 读取兄弟 DUT 单元 `secret.txt` 被 PSEC 1.1 拒绝 | **通过（零 SKIP）** |
| T5 | `NetworkPolicy::Deny` 拦截回环 TCP（`curl.exe` 连接失败），对照 `LoopbackOnly` 连接成功 | **通过** |
| T6 | 孙进程继承容器边界（单元外写拒、兄弟 DUT 读拒、单元内写通） | **通过** |
| T7 | 2s 超时触发 `UiJobObject` 终止父+孙整棵进程树，标记文件停止增长 | **通过** |
| T8 | 超长脚本 / 超限超时 / 破坏性命令在落盘前即被 `PolicyRejected` 拒绝；256B 输出截断无死锁 | **通过** |
| Agent 宿主 | 单进程同时满足写隔离 + 兄弟 DUT 读隔离 + 单元内写入 | **通过** |
| `capture_denials` | 拒读触发后产出有效 JSON 诊断报告且 `total_denials > 0` | **通过** |
| `streaming_spawn` | 双向 stdin/stdout 实时往返 + 读写边界 + `Drop` 杀整棵进程树 | **通过** |
