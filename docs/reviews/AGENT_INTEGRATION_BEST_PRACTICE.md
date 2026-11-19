# duthub Agent 集成最佳实践

> 历史设计评审。当前运行时与验收入口见[接入指南](../guides/AGENT_INTEGRATION_HANDOFF.md)。

> 面向 duthub 维护者。本文给出 v5 的 Agent 集成口径：协议分工、宿主架构、进程生命周期、工作路径、长时任务、跨机 A2A、接入方式与安全边界。
>
> 本文版本：2026-11-21（v5 口径）。本文所称 **v5** 指本方案覆盖的产品阶段（ADR-0 的范围），不是 crate 版本号。
>
> 基线：本文编写时 `main` 分支 `4c5e43a`；2026-11-21 的实施在 `feat/agent-integration`（HEAD `fe24fc6`）上进行，W4 实测见 `docs/reviews/AGENT_IMPLEMENTATION_REPORT.md`。行号以素材读取时点为准，引用前重新 `grep`。
>
> **2026-11-21 W4 实施口径（以实测为准）**：本轮 W1–W4 已完成内嵌 goose-acp v1.60.2
> （`duthub.exe` 内嵌解压后的 exe + LICENSE/NOTICE/manifest，zip 仅来源记录）、自研 ACP
> 客户端、每 DUT `HostManager`/sandbox/模型环境、桌面 Agent 会话面板与每会话只读 listener；
> 全量门禁实测通过（见 `docs/reviews/AGENT_IMPLEMENTATION_REPORT.md`、`docs/decisions/ADR_SIGNOFF.md`）。
> 本文的 v5 长周期波次（P0–P7 / W1–W7 / T10–T50）已被 `43c58dc:docs/AGENT_IMPLEMENTATION_TASKS.md`
> 顶部的 supersession 块取代；本文保留作设计与边界参考，与实现冲突时以 ADR-3、ADR-WORKDIR
> 与实测报告为准。
>
> **2026-11-21 收口轮更新（详见 `docs/reviews/AGENT_IMPLEMENTATION_REPORT.md` §11）**：
> Job Object（`agent_host/job.rs`）、外部 goose 接入启动链（`agents.json` + `agent_host/registry.rs`
> + `host.rs` 经 `registry::resolve`）、NotSigned 应用内徽标、应用内第三方许可页
> （`ConfigPage::Licenses`）、`GOOSE_MODE=approve`、Anthropic Messages 支持、backon 1s/2s/4s
> 自动重启退避**已实现**；workspace 现为 **11 成员**（新增 `duthub-a2a` 与 `duthub-transfer`）。
> **仍未完成**：A2A LAN/持久化/peer 认证/TLS（A2A 仅回环实验面，ADR-2/8/9 为 Proposed，
> 未经产品签字不得进产品验收）、OS 受限令牌 spawn、CLI 真实验证（rmcp 3.10.0 非对象
> tool `outputSchema` 问题）、SBOM/签名（release gate 按设计失败，对外发布 blocked）。
> `agents/goose/` 旁挂、T24 统一仍 deferred。

> 阅读约定：
>
> - **事实**：给出 `path:line` 或来源 URL；本轮实际读取/执行的条目在「事实核实记录」汇总。
> - **设计裁决**：写明「必须 / 应当 / 默认」的内容是 v5 规格，不是现状。
> - **未运行**：本文设计调研时点未执行 `cargo build/test/clippy/fmt`、`bash scripts/regression`、`--self-test`，未实连 goose、ACP、A2A 或真实 peer。2026-11-21 W4 已实测全量门禁并启动真实 goose-acp（假模型服务），结果见 `docs/reviews/AGENT_IMPLEMENTATION_REPORT.md`；其余条目的「通过」仍以对应命令输出为准，静态阅读不算验收。
> - 本文只写设计与任务，不含实现代码。
>
> **v5 硬边界（ADR-0）**：Agent 只在 duthub GUI 进程存活期间运行；关闭 duthub 即结束 Agent 子进程并停止 A2A。v5 **不保留** headless、无人值守、计划任务、Session 0、24/7 形态；duthub 关闭期间 A2A 不可用。若产品要求常驻，必须另立 ADR 并明确推翻本边界，不能默认实现。
>
> **本版修订（2026-11-21）**：吸收两轮独立评审。工具口径统一为「27 个 `#[tool]` 标注 / 13 个 Plan 工具（对外承诺）/ 15 个只读能力（含 2 个非 Plan）/ 8 个 A2A 证据工具」；GUI 租约修复作为 P1 伴随 PR；会话 listener 走 loopback TCP + internal auth；A2A 内部 Paused/NeedsInspection 映射为 `Working` + metadata；补齐路径、JSON 生命周期、保留/删除、定时补做与关闭语义。
>
> **2026-11-24 通知工具追补（当前口径）**：新增 `notify_user` 后 `#[tool]` 标注为 **28**、路由为 **14**，但「13 个 Plan 工具」口径不变——`notify_user` 只出现在**用户配置的通用/DUT MCP 监听器**（`PLAN_ROUTER_WITH_NOTIFY`，14 条）；桌面发起的 Agent 会话（内嵌 goose 与 claude/codex CLI profile）、stdio 快照端与 A2A 证据端仍是 13 条，且调用 `notify_user` 得 `PLAN_MODE_ONLY` / `EVIDENCE_SCOPE_ONLY`。对外统一表述「**13 个只读 Plan 工具 + 1 个通知工具（仅用户配置的 MCP 监听器可用）**」，不得写成「14 个 Plan 工具」。`notify_user` 不是设备写动作（无审批、无租约），但能唤起用户界面，受宿主告警开关、抑制与限流约束；正文不进审计。

---

## 结论

1. **duthub 做 Host，不做 Agent 内核。** v5 不建 `duthub-runtime`，不填 `crates/duthub-agent`，不内置 LLM 循环；旧计划把 workspace 从 9 个成员增加到 11 个（新增 `duthub-acp`、`duthub-a2a`）——**收口轮现状（2026-11-21）**：workspace 已是 11 成员（新增 `duthub-a2a` 每 DUT 回环 A2A 端点与 `duthub-transfer` 板卡镜像 tus 接收端点；`duthub-acp` 未建），ACP 客户端继续落在 `apps/desktop/src/agent_host/acp.rs`（见 TASKS supersession）。duthub 提供的是 ACP client/Host、MCP server、进程/凭据/审计基础设施，模型、上下文、会话存储与工具决策由外部 Agent 负责。

2. **Agent 与 duthub 同生共死。** Agent 子进程由 duthub 以 `CREATE_NO_WINDOW` 启动；关闭窗口时先 cancel、关 stdin、短等待，再 `kill`（Workbench `HostManager::drop` 收尾）。**收口轮更新**：Job Object 已实现（`agent_host/job.rs`：CREATE_SUSPENDED → Assign → Resume，`KILL_ON_JOB_CLOSE`/16 进程/4 GiB，失败 fail closed）。v5 **不保留 headless、无人值守、计划任务、Session 0、24/7**；duthub 关闭期间 A2A 不可用。这是 ADR-0 要求产品签字的边界，不是实现细节。

3. **协议三角色固定。** ACP 只做 Client/Host（按行 JSON-RPC over stdio，不声明 `fs/*`、`terminal/*`）；MCP 只做 Plan 工具面（对外 13 个；DUT 绑定 listener 再收窄到单 DUT）；A2A 只做入站 server（Card、SendMessage/GetTask/CancelTask、8 个证据工具，`task.write` 硬关闭）。三者的承载与不承载见「推荐架构」。

4. **默认 Agent 内嵌 goose-acp v1.60.2（ADR-3 修订）。** 用户已拍板：`duthub.exe` 内嵌解压后的官方 lean `goose-acp.exe`，首次使用时校验 SHA-256 并落到版本化目录；zip 仅来源记录。外部路径覆盖**已接入启动链**（收口轮：`agents.json` + `agent_host/registry.rs`，Host 按 DUT `agent_id` 经 `registry::resolve` 启动，外部 goose 版本 ≥ 1.60.2），便于独立升级。旧「引导安装 + 用户自装」口径已被 ADR-3 取代；codex、Claude Code、DeepSeek harness 仍仅作可选外部 Agent（claude-code 适配器已存在但真实 CLI 端到端未通过，见 §11.5 of 实施报告）。

5. **对外只承诺 13 个只读 Plan 工具（用户配置的 MCP 监听器另加 1 个通知工具 `notify_user`）；内部只读能力 15 个；A2A 证据工具 8 个。** `plan_tool_allowed` 放行 13 个名字，`PLAN_ROUTER` 与 `call_tool` 双重拒绝非白名单；这 13 个是 Agent 会话与 stdio/A2A 端点唯一可见的集合。用户配置的通用/DUT 监听器用 `PLAN_ROUTER_WITH_NOTIFY`（13 + `notify_user`），因此 Plan 模式也能唤起用户界面——它不是设备写动作，受宿主限流/抑制约束。`relay_status`/`flash_test` 只读但不在 Plan 路由，故「只读能力」=15；A2A 只暴露 13 的子集 8 个证据工具。28 个 `#[tool]` 标注中，除 13 个只读 Plan 工具与 1 个通知工具外，其余 14 个（2 个只读 + 12 个写/租约/审批）在任何 Plan 路由中都不可达。v5 不做内部 26 工具写面（ADR-8 取代旧草案 §4.5）。

6. **认证/授权用共享 `AgentAccessRuntime` + runtime overlay。** 持久设置与运行时临时凭据分开：`authenticate_http` 只查持久表；`authenticate_internal` 只查 overlay；`authorize`（`McpServerCore::authorize_any_read` 的 any-read 判定也走此路径）/`lease_credential_enabled` 合并；`update(settings)` 重建持久表但保留 overlay；`enabled()` 只看持久设置。租约唯一实例是 `device.lease_manager()`，`set_agent_access` 只成功一次，构造链必须带 `.with_gui_session_lifetime()`。

7. **DUT 绑定是协议级隔离，不是安全边界。** 每会话 listener 用 `session-<uuid>`，`listener_dut=Some(dut)`，全部 13 个 Plan 工具按绑定 DUT 收窄；但通用 MCP token 仍以明文存在 `settings.json`，同用户 Agent 读到后可以调用通用 listener 读取全部 DUT 资料。v5 不静默把 token 迁到 keyring/DPAPI（ADR-10），UI 与文档必须如实写「协议级 DUT 绑定，不是安全边界」。

8. **长时任务靠账本，不靠内存。** A2A 任务使用 append-only journal + 每步 fsync、checkpoint（保留 20 个 + 每日 1 个）、证据 pin（copy + SHA-256，每任务 512 MiB）、`step_key`/`tool_key`/`messageId` 幂等键；Agent turn 用 `prompt_id` 幂等。恢复只做 journal 重放、Succeeded 取记录、Unknown 对账，不承诺「确定性重跑」。

9. **A2A 先裁决后实现。** 需要 ADR-2（LAN 入站）、ADR-8（Card 1.0.1 + supersede 旧 §4.5/§4.7/§5.5/§6.5）、ADR-9（StateDb 上移 core）与产品签字；入站 peer 只用 `<data_dir>/peers.json`，`settings.a2a` 只保留全局默认/旧兼容，`host` 废弃；每 DUT listener 的 `bind_addr` 默认 `None`，升级时 `NeedsBindAddr`/`PortConflict` 阻塞，不静默绑 `0.0.0.0`。

10. **诚实口径。** 本文其余章节保留设计调研时点的三种状态：**设计裁决**、**计划验证**、**未运行**。2026-11-21 W4 的实测状态以 `docs/reviews/AGENT_IMPLEMENTATION_REPORT.md` 为准：全量门禁通过，真实 goose-acp 已由测试启动（假模型服务）；真实在线模型、真机硬件与 A2A peer 未运行。

---

## 现状基线

### 2.1 工作区、依赖与交付形态

| 项 | 现状 | 依据 |
|---|---|---|
| workspace 成员 | 收口轮现状：**11 个**（新增 `duthub-a2a`、`duthub-transfer`；`duthub-acp` 未建，见 TASKS 收口轮 supersession） | 根 `Cargo.toml:3-15`（本轮读取） |
| MCP SDK | `rmcp = 3.10.0` | 本轮 `Cargo.lock:5861-5862` |
| HTTP | `axum = 0.8.9` | 本轮 `Cargo.lock:553-554` |
| UI | `gpui-kit = 0.9.1` | 本轮 `Cargo.lock:2367-2368` |
| `duthub-agent` | 空占位：`lib.rs` 仅 1 行注释，`Cargo.toml` 的 `[dependencies]` 为空 | 本轮 `wc -l` = 1/7；`crates/duthub-agent/src/lib.rs:1` |
| 桌面 Agent | 本轮已接真实 Host：每 DUT 一个 `goose-acp` 会话（`agent_host/host.rs`），演示状态机保留为无 live 时的降级展示 | `apps/desktop/src/agent_host/host.rs:130`、`:369-397`；`pages.rs:1238` |
| 桌面依赖 | `apps/desktop` 直接依赖 `duthub-mcp` | 本轮 `apps/desktop/Cargo.toml:23` |
| 发布 | release 构建并上传 `duthub.exe`（`+crt-static`），另旁附 `dist/licenses/goose/{LICENSE,NOTICE,manifest.json}`；`duthub.exe` 仍单文件 | `.github/workflows/release.yml`（W4 修订）、`:43-47` |
| 外部工具 | ffmpeg、ADB/HDC、烧录均为外部可执行文件，不内置 | `media.rs:120`、`:157-170`；`duthub-adb/src/lib.rs:3`、`:101`、`:400-416`；`duthub-flash/src/lib.rs:50-58` |
| ACP | 本轮已实现自研按行 JSON-RPC ACP 客户端（`apps/desktop/src/agent_host/acp.rs`），未引入 `agent-client-protocol` crate | `apps/desktop/src/agent_host/acp.rs`；TASKS supersession |
| A2A | 收口轮现状：`crates/duthub-a2a` 最小实验面已落地（仅回环、3 方法、8 证据工具、`-32601`、任务仅内存）；LAN/持久化/peer 认证/TLS 仍 deferred | `crates/duthub-a2a/src/{listener,task,evidence}.rs`（本轮读取）；`apps/desktop/src/a2a_runtime.rs` |

### 2.2 MCP 工具口径：28 个标注 / 13 个只读 Plan + 1 个通知 / 15 个只读能力 / 8 个 A2A 证据 / 15 个非白名单（含通知）

先给结论，避免把几套集合混用：

| 口径 | 数量 | 含义 | 对外承诺 |
|---|---|---|---|
| `#[tool]` 标注 | 28 | `server.rs` 中 28 个宏标注（2026-11-24 `grep` 计数）；不是「用户可见工具」的同义词 | 否 |
| Plan 工具 | 13 | `plan_tool_allowed` 放行、`PLAN_ROUTER` 暴露的只读集合 | **是，唯一对外可见的只读集合** |
| 通知工具 | 1 | `notify_user`：仅在用户配置的通用/DUT 监听器（`PLAN_ROUTER_WITH_NOTIFY`）出现，Plan 模式可调用；不触设备、无审批/租约 | **是（限用户配置监听器）** |
| 只读能力 | 15 | 13 个 Plan + `relay_status`/`flash_test`（只读但不在 Plan 路由） | 否 |
| A2A 证据工具 | 8 | 13 的子集，仅 A2A 内部 per-task server 使用 | 否 |
| 非 Plan 白名单工具 | 15 | 28 − 13（`plan_tool_allowed` 不含的 15 个）；其中 2 个只读、12 个写/租约/审批、1 个通知 | 否 |

- **28 个标注**：2026-11-24 在原有的 27 个之外新增 `notify_user`。锚点（2026-11-24 实测）：`NOTIFY_TOOL_NAME` 在 `server.rs:49`，`#[tool_router]` 在 `:799`，`plan_tool_allowed` 在 `:1681`，`PLAN_ROUTER` 在 `:1834`、`PLAN_ROUTER_WITH_NOTIFY` 在 `:1847`，`#[tool_handler(router = PLAN_ROUTER)]` + 手写的 `list_tools`/`get_tool`/`call_tool` 在 `:1857-1858` 起的 `ServerHandler` impl。
- **13 个 Plan 白名单**：`plan_tool_allowed` 放行 `dut_list`、`dut_info`、`dut_control_status`、`serial_read`、`analyze_serial`、`log_segments`、`log_segment_read`、`dut_files`、`dut_file_read`、`heartbeat_get`、`media_streams`、`adb_list_devices`、`skill_export`。
- **15 个只读能力**：13 个 Plan + `relay_status`（描述明确写「只读，无需审批」）+ `flash_test`（描述与注释写「只读测试」）。这两个不对外暴露，不能写成「13 个之外还有 2 个外部可见只读工具」。
- **15 个 `plan_tool_allowed` 之外**：28 − 13。其中 2 个只读、12 个写/租约/审批（`dut_acquire_control`、`dut_renew_control`、`dut_release_control`、`serial_write`、`adb_shell`、`control_power`、`control_power_key`、`control_reset`、`relay_test`、`heartbeat_set`、`log_mark`、`approval_request`），另有 1 个通知工具 `notify_user`——它不在 `plan_tool_allowed` 里，但由 `PLAN_ROUTER_WITH_NOTIFY` 在用户配置监听器的 Plan 面暴露，因此不能与 12 个写工具并列称为「不可达」。
- **双重拒绝**：`PLAN_ROUTER` / `PLAN_ROUTER_WITH_NOTIFY` 都在构建时 `remove_route` 非白名单工具；`call_tool` 入口再按所选路由表 `has_route` 复核，失败返回 `-32000`、`kind=execution_policy`、`code=PLAN_MODE_ONLY`。没有通知器的端点（stdio、A2A 证据端、桌面 Agent 会话）连 `tools/list` 都看不到 `notify_user`。
- **自述**：`get_info` 只声明 `tools` 能力，instructions 自述「只支持 Plan 模式」「不会运行 Shell、写串口、烧录、控制继电器或改动配置」，并追加「用户配置的 MCP 监听器额外提供 `notify_user`……桌面发起的 Agent 会话没有该工具」。
- **写作用域**：设备写工具对外一律不可达；`notify_user` 只唤起 UI、不写设备。15/8 是内部口径。任何文档/UI 不得把 15 或 8 写成外部可见，也不得把 `notify_user` 写成「14 个 Plan 工具」之一。

### 2.3 传输、认证与授权

- **stdio**：`serve_stdio` 用 `McpServerCore::new(state, None)` 提供（`transport.rs:28-35`）；stdio 无 HTTP parts，认证回退 `LocalAdmin`（`auth_http.rs:170-184`）。
- **HTTP**：只暴露 `/mcp` Streamable HTTP 路由，由 `AuthSvc` 包裹（`transport.rs:128-155`）；默认端口 8797（`agent_access.rs:29-32`）；非回环绑定在认证关闭时 fail closed（`transport.rs:213-221`）。
- **认证**：只读 `Authorization: Bearer` 头（`auth_http.rs:154-164`），失败统一 401 `{"error":"unauthorized"}`（`auth_http.rs:137-144`）；`authenticate_http` 要求 `settings.enabled`（`agent_access.rs:315-331`），默认 auth 关闭时桌面 HTTP `/mcp` 一律 401。
- **凭据作用域**：`authenticate_with_policy` 要求 `cred.listener_id == listener_id`（`agent_access.rs:333-354`）；DUT 专用 listener 把 `listener.dut_id` 传入 `McpServerCore`（`apps/desktop/src/mcp_runtime.rs:502-529`、`transport.rs:128-137`），跨 DUT 调用返回 `LeaseRequired/UNAUTHORIZED`（`server.rs:344-349`、`:381-397`）。
- **读授权**：`authorize_read` 按 DUT/动作 deny-by-default（`server.rs:371-379`、`agent_access.rs:363-396`）。`dut_list` 过滤 listener DUT 与 read grant（`server.rs:564-591`）；`adb_list_devices` 只返回可读 DUT 的 ADB serial（`server.rs:313-333`、`:955-990`）。
- **通用 listener 的全局读**：`authorize_scoped_read` 只在 `listener_dut.is_some()` 时校验（本轮读取 `server.rs:386-395`）；因此通用 listener 的 `log_segments`/`log_segment_read`/`dut_file_read` 不做逐 DUT grant 校验（`server.rs:638-746`）；`dut_files` 列表只要求任一 read grant 并过滤（`server.rs:710-725`），省略 `dutId` 时列出全部 DUT（本轮读取 `server.rs:718-730`）。DUT 专用 listener 全部强制 scoped（`server.rs:383-392`）。
- **`skill_export` 授权（本轮已修）**：`server.rs:1281-1290` 已调用 `authorize_any_read`；零 grant 的 HTTP 凭据被拒，stdio LocalAdmin 保持可用（`http_auth_e2e` 覆盖）。

### 2.4 租约、审批、审计

- **租约**：每 DUT 单实例、FIFO 上限 8（`lease.rs:24-28`）；授予时 generation+1（`lease.rs:1043-1066`）；renew 不递增（`lease.rs:593-640`）；release/Protected/NeedsInspection 在 `lease.rs:643-673`、`:841-914`；Agent 写入用 `validate_agent_lease` 校验持有凭据与 generation（`lease.rs:678-715`）。
- **GUI 会话租约（本轮已修）**：`apps/desktop/src/main.rs` 的 `ControlLeaseManager` 已加 `.with_gui_session_lifetime()`；`lease_gui` 测试覆盖 GUI 会话租约不被 TTL 提前释放。旧基线缺口已关闭。
- **审批**：`apr-` 前缀、TTL 300 秒（`approval.rs:15`、`:87-96`），绑定 `dut+action+args` 的 SHA-256（`approval.rs:53-62`、`:136-159`）；peek 不消费、consume 单次移除（`approval.rs:121-175`）。
- **审批审计缺口**：`ApprovalManager` 只写自己的内存 Vec（`approval.rs:68`、`:106-108`、`:170-173`），`audit_tail` 无调用者（`approval.rs:178`）；`AuditSink` 的 `approval.issue`/`approval.consume` 事件声明（`audit.rs:5`）没有实际写入路径。GUI 审计列表读 `AuditSink`，所以审批链在权威审计中不可见。该缺口同样拆独立 PR。
- **审计落盘**：`AuditSink` 写 `<dir>/agent-access-YYYYMMDD.jsonl`（`audit.rs:192-217`、`:258-290`），落盘前对 `token`/`approval_token`/`authorization`/`bearer` 等键脱敏（`audit.rs:75-133`）；桌面审计目录为 `<work_dir>/audit`（本轮读取 `main.rs:170`）。

### 2.5 A2A 与 Agent 面板现状

- **配置**：`A2aConfig`（`agentName`/`port`/`host`/`peers`）在 `settings.a2a`（`model.rs:91-104`）；`A2aPeer` 只有 `id`/`name`/`cardUrl`（`:127-133`）；每 DUT `DutA2aConfig` 只有 `enabled`/`port`/`agentName`（本轮读取 `model.rs:135-146`），没有 `bind_addr`；默认按默认值跳过序列化（`model.rs:158-160` 的 `dut_a2a_is_default`；属性在 `model.rs:628`）。
- **Token**：A2A Token 存 OS keyring service `duthub`、条目 `a2a/<dut_id>`（`keys.rs:43-74`），不落 settings/duts。
- **UI**：Agent 页 `a2a_panel`（`pages.rs:3490` 起）编辑 `agentName`/`port`/`peers` 并导出/检查；检查按钮只弹「A2A 服务未运行」。连接管理 `a2a_dut_panel`（`pages.rs:1235` 起）可启用每 DUT 预配置、分配端口、生成/复制/重生成 Token，并展示本机/局域网 Agent Card URL；页面明说运行时尚未接入。
- **运行时（收口轮更新，2026-11-21）**：~~运行时为零~~ `crates/duthub-a2a` 已落地**最小实验面**：
  每 DUT listener 只绑 `127.0.0.1`（`listener.rs:43-47`，非回环一律 `NeedsProductSignoff`）；
  JSON-RPC 仅 SendMessage/GetTask/CancelTask 3 方法（`task.rs:85`），其余一律 `-32601`
  （`task.rs:74`）；执行体白名单 8 个证据工具（`evidence.rs:23`，绑死本端点 DUT）；请求体
  上限 256 KiB（`listener.rs:36`）；Bearer fail-closed；任务状态仅内存（`lib.rs:8`）。
  桌面包装在 `apps/desktop/src/a2a_runtime.rs`（专属 tokio 运行时 + PortRegistry 预登记）。
  **仍为零/deferred**：LAN 监听、任务持久化（ADR-9 StateDb）、peer 认证、TLS。
  A2A 为实验性，ADR-2/8/9 为 Proposed，未经产品签字不得进产品验收。
- **导出**：`export_a2a_config` 运行时写 `<work_dir>/exports/a2a_server.json`，内容只有 `agentName`/`port`/`peers[].agentCardUrl`（`workspace.rs:2678` 起）；仓库没有检入的 `exports/a2a_server.json`。

### 2.6 桌面运行时与 Agent 面板

- 桌面 `spawn_mcp_runtime` 在进程内为主端口/附加监听器起线程，并每 2 秒写 `<work_dir>/runtime/mcp-state.json`（`apps/desktop/src/mcp_runtime.rs:480-547`）；监听器拓扑只在启动时绑定，端口/绑定变更重启生效。
- Agent 页由 `provider_models.rs:573-587` 把 `agent_access_panel` 与 A2A 面板组合；`agent_access_panel`（`pages.rs:1855-2172`）含 auth 开关、`lanAccess`、端口/TTL、附加监听器、凭据/Token、DUT 控制权与最近 100 条审计（`pages.rs:2147`）。
- **Agent 面板（本轮已接 live 会话）**：`pages.rs` 的 `agent_session_panel` 读取当前 DUT 的 `HostManager` 视图（状态/端口/transcript/停止/取消/发送），每 DUT 独立草稿；无 live 会话时保留演示面板并标注。旧 `agent.rs` 的 900ms 演示状态机仍作降级展示，不再是唯一形态。

### 2.7 文档漂移（引用前必须修正）

| 文档声明 | 实际 | 处置 |
|---|---|---|
| `docs/ARCHITECTURE.md:11` 说 `duthub-agent` 已有「生成分析上下文、调用 MCP 工具、任务与证据状态机」 | `crates/duthub-agent/src/lib.rs:1` 只有一行注释，`Cargo.toml` 依赖为空，桌面也不依赖它 | 改为「空占位，v5 不填」 |
| `docs/ARCHITECTURE.md:12-13`、`:51` 说桌面是 Tauri 2 + HTML/JS、TUI 是 Ratatui | 实际桌面是 GPUI（`main.rs:1`、`gpui-kit 0.9.1`），TUI 只打印占位 | 改为 GPUI；TUI 标占位 |
| `43c58dc:docs/GOOSE_AGENT_FEASIBILITY.md:201` 把「仅绑 127.0.0.1（`assert_loopback`）」列为必须保真契约 | 当前用 `assert_bind_allowed`（`transport.rs:213-221`），认证开启时允许 `0.0.0.0`；`lanAccess` 已是正式配置；不存在 `assert_loopback` | 由 ADR-2 取代；删除旧契约 |
| `43c58dc:docs/GOOSE_AGENT_FEASIBILITY.md:61-62`、`:225`、`docs/design/CONFIG_DIR_DESIGN.md:42`、`.agents/skills/duthub-a2a-integration/SKILL.md:9` 引用 `interop_a2a_server.py`、`docs/A2A_MCP_INTEROP.md` | 两文件均不存在 | 删除引用或补资产；不能把不存在的互操作脚本当验收依据 |
| `43c58dc:docs/AGENT_IMPLEMENTATION_TASKS.md:1025` 的 T39 只有 `preferred_transport` | A2A 1.0.1 用 `supportedInterfaces[].protocolBinding`、`securityRequirements`；旧字段由 ADR-8 取代 | 重写 T39 契约测试 |
| `43c58dc:docs/agent/TASKS.md:181-187` 把 M5 与 5.4/5.5 勾为完成 | 代码没有 Submitted/Working/InputRequired 状态机，也没有审批 UI | 取消勾选，按本文 P3/P4 重排 |

### 2.8 本轮本地核对（已执行）

以下是本轮实际运行并看到输出的命令，不是素材转述：

- `grep -c '#\[tool(' crates/duthub-mcp/src/server.rs` → `27`。
- `grep -n 'fn plan_tool_allowed' -A20 crates/duthub-mcp/src/server.rs` → 函数在 `server.rs:1296-1313`，恰好 13 个名字。
- `grep -n 'fn skill_export' -A12 crates/duthub-mcp/src/server.rs` → `server.rs:1280-1292` 只接收 `_principal`，无授权调用。
- `grep -n 'fn authorize_scoped_read' -A16 crates/duthub-mcp/src/server.rs` → `server.rs:386-395` 仅在 `listener_dut.is_some()` 时校验。
- `grep -n 'a2a_running' apps/desktop/src/workspace.rs apps/desktop/src/pages.rs` → `workspace.rs:631`、`:940`、`pages.rs:3621`。
- `grep -n 'lease_manager' apps/desktop/src/workspace.rs` → 首次出现在 `workspace.rs:801`，并在 `:819` 等处以 `device.lease_manager()` 取用。
- `grep -n 'with_gui_session_lifetime' crates/duthub-device/src/lib.rs apps/desktop/src/main.rs` → 只命中 `crates/duthub-device/src/lib.rs:93`；`main.rs` 无命中。
- 分别执行 `grep -n 'name = "rmcp"' -A1 Cargo.lock`、`grep -n 'name = "axum"' -A1 Cargo.lock`、`grep -n 'name = "gpui-kit"' -A1 Cargo.lock` → `3.10.0`、`0.8.9`、`0.9.1`。
- 分别执行 `grep -c 'name = "rusqlite"' Cargo.lock`、`grep -c 'name = "redb"' Cargo.lock`、`grep -c 'name = "x509-parser"' Cargo.lock` → 均为 `0`。
- `grep -n 'name = "rustls-webpki"' -A1 Cargo.lock` → `0.103.17`（`Cargo.lock:6152-6153`）。
- `grep -rniE '\bacp\b|agent-client-protocol' --include=*.rs --include=*.toml .` → `0` 行。
- `wc -l crates/duthub-agent/src/lib.rs crates/duthub-agent/Cargo.toml` → `1`、`7`。
- 读取 `crates/duthub-core/src/store.rs:77-103` 与 `:137-157`：`ensure_portable_work_dir` 的优先级是 CLI `--workdir` > `settings.workspaceDir` > exe 目录；同一 crate 的 `resolve_work_dir_in` 却是 settings > CLI。这是现有歧义，见「工作路径与 Plan 模式」。
- 读取 `apps/desktop/src/main.rs:82-94`：`artifact_root` 经 `ensure_portable_work_dir` 解析，`work_dir = artifact_root.join("duthub-data")`。
- 读取 `apps/desktop/src/main.rs:171-177`：桌面自建租约只 `.with_ttl(...)`，没有 `.with_gui_session_lifetime()`；审计目录在 `:170`。
- 读取 `crates/duthub-core/src/model.rs:91-146`：`A2aConfig.host` 默认 `127.0.0.1` 且注释「永远 loopback」；`DutA2aConfig` 只有 `enabled`/`port`/`agent_name`，没有 `bind_addr`。

### 2.9 术语、路径与配置键

| 术语/路径 | 定义 | 落点 |
|---|---|---|
| `<exe_dir>` | `duthub.exe` 所在目录 | — |
| `<data_dir>` | `<exe_dir>/.duthub`；settings/duts/peers/agents 等启动配置 | 不随 `workspaceDir` 移动 |
| `<artifact_root>` | `ensure_portable_work_dir` 解析出的根：CLI `--workdir` > `settings.workspaceDir` > `<exe_dir>` | main.rs:82-93 |
| `<work_dir>` | `<artifact_root>/duthub-data`；audit/logs/state.db/agent/exports/runtime | main.rs:94 |
| `peers.json` | 入站 peer 白名单与 token 指纹 | `<data_dir>/peers.json` |
| `agents.json` | 外部 Agent 定义（不含 token） | `<data_dir>/agents.json` |
| `sessions.json` | 每会话 `config_dir`/`cwd`/resume 元数据 | `<work_dir>/agent/sessions.json` |
| `state.db` | A2A TaskStore/幂等/迁移 | `<work_dir>/agent/state.db` |
| `audit/` | `agent-access-YYYYMMDD.jsonl` | `<work_dir>/audit/` |
| session `config_dir` | CLI/Agent 会话配置，保留给 resume | `<work_dir>/agent/<session>/home/` |
| keyring | service 统一 `duthub`；条目 `model/<id>`、`a2a/<dut_id>`、`a2a/<dut>/peer/<peer>`、`acp/<id>/env/<KEY>` | OS keyring |
| `dutKey` | `HMAC-SHA256(per-install salt, dut_id)`，用于 Card 去敏；salt 存 keyring/受 ACL 文件，轮换审计 | A2A Card |
| `AsyncRwTransport` | rmcp 的进程内异步读写 transport，基于 `tokio::io::duplex`，不占 TCP 端口 | duthub-mcp task server |
| `ReadScope=Evidence` | A2A 内部 per-task server 的读 scope，只允许 8 个证据工具且绑定目标 DUT | A2A task |
| `task_credentials` | 每任务重签的 runtime listener/token 映射，供重启恢复 | StateDb |

`.duthub/` 是 `<data_dir>`，不是 `<work_dir>`；两者不能混用。`<data_dir>` 不随 `workspaceDir` 改变，`<work_dir>` 会随 `--workdir`/`workspaceDir` 改变。

---

## 选型矩阵

口径：

- **维护度**：以 2026-11-20/21 可见的 release 与仓库活跃度为依据；带「未核实」的发布节奏不作为承诺。
- **纯 Rust**：核心实现是否为 Rust；CLI/桌面壳的 Node/Electron 不计为纯 Rust。
- **单 exe**：能否在不增加第二个进程/安装包的前提下随 `duthub.exe` 分发；「可（库）」表示可作为依赖链进主程序，但不等于推荐。
- **长时任务**：候选自身是否带持久会话、恢复、幂等；「宿主自写」表示必须由 duthub 补。
- **嵌入成本**：把候选接进 duthub 并满足本文安全/生命周期要求的工程量，不是安装体积。

| 候选 | 维护度 | 许可证 | 纯 Rust | 单 exe | 长时任务 | 嵌入成本 | 结论 |
|---|---|---|---|---|---|---|---|
| duthub 自研 Host + 薄运行时 | 自控 | 自有 | 是 | 是（Host 层） | 需自建 journal/checkpoint（§7） | 高（自建） | **推荐为 Host 层**；不实现完整 Agent 循环 |
| goose（外部 ACP 进程） | 极高：v1.60.2（2026-11-20）；发布节奏未核实 | Apache-2.0 | 是 | 否（sidecar；ACP zip 9,832,133 B，内嵌解压后的 exe 20,541,440 B） | SQLite `sessions.db` + 可重入状态机（未核实） | 中（stdio + 进程托管） | **本轮已内嵌**（ADR-3 修订）；外部路径覆盖待接入启动链 |
| goose GDK / oi-kit | 低：0.1.0-alpha.15（2026-11-19） | Apache-2.0 | 是 | 可（库） | 宿主自写 `SessionLoader`/`EffectHandler` | 高（alpha、改名） | 不内嵌；等 beta |
| Zed 自有 Agent | 高 | GPL-3.0-or-later | 是 | 否（进程内库、`publish=false`） | 绑定 Zed `threads.db` | 不可行 | **排除** |
| GitHub Copilot CLI | 高：1.0.114（2026-11-18） | 专有（NOASSERTION） | 运行时闭源（Rust 重写未核实） | 否（196,113,184 B） | 受订阅/premium 限制 | 不可行 | **排除内置**；仅用户自装 |
| Claude Code | 高：2.1.329（2026-11-19） | 商业条款 | 否（Node） | 否 | `--resume`；后台上限 10 分钟（未核实） | 低-中（CLI profile） | 可选外部 CLI |
| DeepSeek harness（dsh） | 预览：0.2.2-rc.5（2026-11-18） | MIT | 否（Node/Electron） | 否（约 299,593,552 B） | ACP automation-only；无 load/modes | 中 | 可选实验性；默认不可用降级 |
| openai/codex | 高：rust-v0.171.0（2026-11-20） | Apache-2.0（未核实） | 是（monorepo） | 否（平台包 + ACP 适配器） | rollout + resume + SQLite | 高 | 可选外部；不内嵌 |
| a2a-server-lf | 0.5.3（2026-10-16） | Apache-2.0 | 是 | 可（库） | 仅 `InMemoryTaskStore`（未核实） | 中 | 不作运行时依赖；自研 A2A |
| agent-client-protocol | 5.4.0（2026-11-18）；5 个月 4 个 major | Apache-2.0 | 是 | 是（库） | 能力位 load/resume | 低 | 仅参考；T35 自研，不引依赖 |

矩阵读法：

- **本轮起 goose 内嵌**：`duthub.exe` 仍是单文件，但内含只读 sidecar 资产（goose-acp v1.60.2）；「单 exe = 是」对 Host 层与 sidecar 资产均成立。其他 Agent 不内嵌。
- **goose 是唯一同时满足「专用 ACP 产物 + Apache-2.0 + Rust + 原生 Windows」的候选**，但它的只读能力不足，默认只能标 `mcp_only`。
- **codex 的许可证（Apache-2.0，未核实）与长时任务最完整**，但它不以 crates.io 库形式发布，且 ACP 适配器是额外进程；v5 只把它列为可选 CLI/外部 Agent。

---

## 候选 Agent 对比

### Zed 自有 Agent

- **形态**：Zed 自带 Agent 通过进程内 `NativeAgentServer` 接入；`connect` 直接接收 `&mut App` 与 `Entity<Project>`，在 Zed 进程内创建 `NativeAgent`，不是 spawn 子进程或 stdio ACP server。
- **许可证**：`crates/agent/Cargo.toml` 标 `GPL-3.0-or-later`；仓库 README 写「primarily under GPL-3.0-or-later, with Apache-2.0 components where marked」。
- **不可打包**：`publish.workspace = true` 继承 workspace 的 `publish = false`；依赖 `gpui`、`project`、`client`、`db`、`language_model` 等 Zed 内部 crate，不能作为普通 crates.io 依赖被第三方宿主引用。
- **结论**：**排除**。把 `agent` 链进闭源 `duthub.exe` 通常触发 GPL copyleft；也没有可独立启动的 ACP server/可执行文件。Zed 的 ACP 协议与 SDK 可以借鉴，但 Agent 本体不可复用。A2A 支持未证实。

### goose（aaif-goose/goose，原 block/goose）

- **形态**：`goose acp` 走 stdio（`cli.rs:848` → `goose::acp::server::run`）；`goose serve` 提供 HTTP/WebSocket；服务端实现 `session/load`（`load_session.rs:367` 起，从 `SessionManager` 取会话并回放给 client）；Zed 的 ACP Registry 直接以 goose 为 ACP agent。
- **维护度**：v1.60.2（2026-11-20T06:18:55Z），Apache-2.0、Rust、原生 Windows；Windows 另有专用 `goose-acp-x86_64-pc-windows-msvc.zip`，9,832,133 B（同版本 CLI zip 54,449,330 B、桌面 zip 237,837,415 B）。「11 天 6 个 release」等节奏数据未核实。
- **长时任务**：会话落盘为 SQLite `sessions.db`（sqlx、WAL、`CURRENT_SCHEMA_VERSION=18`，均未核实），Agent 内核自述为可重入状态机（未核实）；官方同时承认「工具已成功但进程在持久化响应前崩溃时，重建状态机并不能让该工具安全重放」。
- **只读能力**：`GooseMode` 只有 `auto`/`approve`/`smart_approve` 三个变体，`Auto` 是默认；没有只读/plan 模式。配置在 `%APPDATA%\Block\goose\config\config.yaml`，密钥默认进系统 keyring，官方文档警告 keyring 访问可能无限阻塞，可用 `GOOSE_DISABLE_KEYRING=1` 关闭。
- **未核实**：goose 是否消费 ACP `session/new` 的 `mcpServers`（W2 测试已覆盖假 MCP 注入）；如何关闭其 developer/本地工具扩展（W2 managed config 已覆盖）；Windows 资产 Authenticode（2026-11-21 已核实为 `NotSigned`；应用内展示 deferred）；冷启动与内存占用。
- **结论**：**首推的外部 Agent**；本轮按 ADR-3 改为内嵌解压后的 exe（zip 仅来源记录），默认 `trust_level=mcp_only`。UI 不得写「Plan 只读」。

### GitHub Copilot CLI

- **形态**：自带 ACP server；v0.0.397（2026-01-28）起提供 `copilot --acp`，默认 stdio，`--port` 走 TCP/NDJSON；官方文档标注 ACP 支持为 public preview。ACP Registry 条目 `github-copilot-cli` 以 `npx @github/copilot@1.0.114 --acp` 分发，license 字段标 `proprietary`。
- **许可证**：npm 元数据 license 为「SEE LICENSE IN LICENSE.md」；`LICENSE.md` 只授予把「未修改副本」随功能更丰富的应用再分发的权利（须附许可证、保留声明、不得独立分发），并明确禁止修改/翻译/衍生作品与商标使用。GitHub 元数据 SPDX 为 `NOASSERTION`。
- **运行条件**：需要有效 Copilot 订阅并 GitHub 登录（`/login` 或带 Copilot Requests 权限的 fine-grained PAT；Classic `ghp_` 不支持）；每条提示扣减一次 premium request；Windows 由 `@github/copilot-win32-x64` 提供原生 `copilot.exe`（196,113,184 B），README 要求 PowerShell 6+。
- **未核实**：ACP 与 duthub 的 stdio 握手/会话恢复能否互通；运行时是否已全部 Rust 重写（官方博客称逾 80 万行 Rust，源码在私有仓库）；A2A 支持（官方 ACP 文档与 changelog 零命中）。
- **结论**：**排除内置**。许可证禁止修改，且只允许未修改随附、不得单独分发；订阅与登录也不适合作为 duthub 默认能力。只允许用户自行安装登录后作为可选外部 Agent。

### Claude Code

- **形态**：headless 契约是 `claude -p` 子进程；`--output-format` 支持 `text`（默认）/`json`/`stream-json`，`stream-json` 在 stdout 输出逐行 JSON 事件；`--input-format stream-json` 支持流式输入；`--resume <session_id>` 或 `--continue` 恢复。
- **只读/Plan**：`--permission-mode plan` 允许读取与探索、写计划，但在计划被批准前阻止编辑；Claude Code ≥2.1.302 的 `claude -p` 在未设置权限模式时以 Manual 启动，需要批准的调用会被直接拒绝（除非 allow 规则、hook 或 `--permission-prompt-tool`）。
- **许可证与运行时**：npm 最新 2.1.329（2026-11-19），许可为 © Anthropic PBC 商业条款（非开源）；Node 运行时。
- **长时任务限制**：`claude -p` 的后台工作默认有 10 分钟空闲上限（`CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS` 可调；未核实）；SIGTERM 退出码 143，且中断回合默认不续跑（需 `CLAUDE_CODE_RESUME_INTERRUPTED_TURN=1`）。因此数月级任务必须由 duthub 外部守护/续跑，不能依赖单个 CLI 进程常驻。
- **Windows 只读**：Claude Code 文档中的内置 Bash sandbox 只列 macOS/Linux/WSL2，Windows 上未见等价 OS 沙箱说明（未证实）。
- **结论**：**可选外部 CLI**。通过 CLI profile 接入，不内置；认证支持 `inherit`（复制/只读挂载用户凭据到持久配置目录）或 `api_key`，复制失败必须要求 API key，不能静默丢认证。

### DeepSeek harness（dsh）

- **存在性/形态**：官方 Windows x64 安装包存在，当前 win-x64 更新清单指向 `deepseek-harness-0.2.2-rc.5-win-x64.exe`（299,593,552 B，releaseDate 2026-11-18T11:35:49Z）；npm `@deepseek-ai/dsh` 0.2.2-rc.5，MIT，Node >=24.9。
- **接入面**：首选 ACP：`dsh --profile acp` 走 ACP stdio；调研方在本机 Windows/Node 24 跑通 `initialize`（agentInfo `deepseek-harness-acp` 0.0.1）与 `session/new`，但未跑真实 prompt，`headless --json` 未实测。ACP 面是 automation-only 子集：stdout 只走协议、单主工作区、绝对 cwd、可挂 stdio/HTTP MCP server；`modes`、`commands`、`plans` 被明确省略/拒绝；`authMethods` 为空。
- **只读/cwd**：cwd 由 `session/new` 的绝对路径给定；只读靠 dsh 的 sandbox 配置，Windows ACL 后端对 read-only 只报 partial enforcement，出厂 preset 只有 `workspace-write`/`danger-full-access`，read-only 需用户自行配置；ACP 面不暴露 sandbox，duthub 无法从协议侧证明只读。
- **打包**：npm 包 bin 只有 `lib/bin.js`（Node >=24.9）；Windows 安装版是 `dsh.cmd` 包装（Electron/Node），该 release 的 GitHub 资产只有源码包与 attestation，没有独立 `dsh.exe`。
- **结论**：**可选实验性外部 Agent**，默认 `mcp_only`，不标「Plan 只读」；未安装/Node <24.9/无 `DEEPSEEK_API_KEY`/initialize 失败/read-only 未验证 → 面板显示「DeepSeek harness 不可用：原因」，不自动换模型、不自动装 Node、不自动改用其他 Agent。

### openai/codex（许可证候选，待核实）

- **不可作为常规 crates.io 依赖**：`codex-core` 在 crates.io 返回「crate does not exist」；同名 `codex-protocol` 0.63.0 实际是第三方 `namastexlabs` 的 fork；`codex-rs` 的核心 crate 只在 workspace 内以 path 依赖存在，`codex-core` 一个 crate 就列出 90 个 `codex-*` 依赖（含 dev），`codex-windows-sandbox` 还定义了 3 个 helper bin 目标。
- **长时任务**：会话按 JSONL rollout 落盘（`rollout/src/recorder.rs:77`，形如 `~/.codex/sessions/rollout-<ts>-<id>.jsonl`），由 `RolloutRecorder::resume` 恢复；索引走 SQLite state DB；上下文默认自动压缩，可用 `AutoCompactionDisabled` 关闭。
- **CLI/ACP**：`codex exec --json` 输出逐行 JSON 事件；`--sandbox read-only`、`-C` 指定工作根、`resume` 恢复；ACP 需 `agentclientprotocol/codex-acp` 适配器（旧 `zed-industries/codex-acp` 0.16.0 已被取代，兼容性未验证）。
- **结论**：**Apache-2.0 许可证（未核实）相对宽松，是原生 CLI 候选**，但只作为可选外部 Agent；不内嵌，不随 duthub 分发。

### 自研薄运行时（Host 层）

v5 自研的是 **Host**，不是完整 Agent：进程/会话/凭据/审计/A2A 状态机由 duthub 负责；模型、上下文、工具决策由外部 Agent 负责。这样做的理由：

- 现有模型 provider 能力只到连通性探测（`llm.rs:12-19`、`:125-157`、`:160-267`），`settings.agent` 的 `fast`/`secondary`/`turns`/`parallel`/`compare`/`approve` 全仓无读取，`run_agent_task` 不存在。
- 内置完整 Agent 需要自建多轮 LLM 调用、工具编排、上下文压缩与证据绑定，工程量与风险都大于「Host + 外部 Agent」。
- 外部 Agent 的许可证与版本漂移风险可以通过「用户自装 + 进程边界 + 能力位降级」隔离；把 Agent 链进主程序反而把风险带入单 exe。

---

## 推荐架构

### 4.1 v5 范围与 crate 边界

v5 的 workspace 目标为 11 个成员：现有 9 个 + `duthub-acp` + `duthub-a2a`。`crates/duthub-agent` 保持空占位，不填实现；不新建 `duthub-runtime`。

| 位置 | 职责 | 禁止 |
|---|---|---|
| `apps/desktop` | GPUI、`agent_host`（HostManager/SessionRegistry/ListenerRegistry/TokenIssuer）、`AlertCenter`、Workbench 接线 | 不在 UI 层实现协议细节；不把 `AlertCenter` 暴露给库 crate |
| `crates/duthub-acp` | ACP client/protocol/process/config：按行 JSON-RPC、能力位、进程 spawn/supervisor | 不依赖 `duthub-desktop`；不实现 `fs/*`、`terminal/*`；不做 ACP server |
| `crates/duthub-a2a` | Agent Card、JSON-RPC 3 方法、任务状态机、peer 认证、TLS、限流 | 不直接依赖 `rusqlite`；持久化统一走 core `StateDb` |
| `crates/duthub-mcp` | 13 个只读 Plan 工具（用户配置的监听器另加 `notify_user` 通知工具）、stdio/HTTP 传输、Bearer 认证、Plan 路由 | 不做 Agent 托管；不开放设备写工具 |
| `crates/duthub-core` | `StateDb`/`open_state_db`/`migrate`、`AgentAccessRuntime`、peer 配置类型、审计/策略类型 | 不依赖 UI/协议 crate |
| `crates/duthub-device` / `control` / `flash` / `adb` | 硬件与设备 I/O | 不感知 Agent 会话 |
| `crates/duthub-agent` | 空占位 | v5 不填 |

**编译边界（计划检查，未运行）**：

- `cargo tree -p duthub-acp | grep -c duthub-desktop` 必须为 `0`。
- `cargo tree -p duthub-a2a -i libsqlite3-sys` 必须只有一个 bundled 来源（防止直接依赖与 core 依赖各带一份）。
- `cargo tree -p duthub-mcp | grep duthub-acp` 必须为 `0`（反向边界）。

**HostManager 归属**：放在 `apps/desktop/src/agent_host/`，因为 `AlertCenter` 是 `apps/desktop/src/alerts.rs:91` 的私有模块（`main.rs:10` 为私有 `mod alerts`），库 crate 不能命名它。`duthub-acp` 只保留 `client/protocol/process/config`，通过 `HostEvent` 回调把 spawn/exit/restart/hang 交给 desktop 映射到 `AlertSource::Agent`。若未来要把 HostManager 移入库，必须先给 core 加 `AlertSink` trait/回调，不能直接引用 desktop 私有类型。

### 4.2 协议角色与版本策略

| 协议 | 连接 | duthub 角色 | 承载 | 不承载 |
|---|---|---|---|---|
| ACP（wire v1） | duthub ↔ 外部 Agent，按行 JSON-RPC over stdio | client/Host（自研，T35；不引 `agent-client-protocol`） | `initialize`（不声明 `fs`/`terminal`）、`session/new`（cwd + mcpServers）、`prompt`、`update`、`request_permission`、`cancel`、`load`/`resume`、`authenticate`/`set_mode`/`set_model`/`set_config_option`；`fs/*`、`terminal/*` 回 `MethodNotFound` | 工具调用（走 MCP）；duthub 永不做 ACP server |
| MCP（2026-07-28 无状态 + 2025-11-25 legacy） | Agent → duthub，Streamable HTTP + Bearer | server | 13 个只读 Plan 工具；DUT 绑定 listener 上全部 13 个按绑定 DUT 收窄；外部通用 listener 保留全局读（ADR-7）；用户配置的 listener 额外提供 `notify_user`（唤起 UI，非设备动作） | Agent 会话、模型调用、进程生命周期不在 MCP |
| A2A（wire 1.0，Card 按 1.0.1 补丁） | 远端 peer → duthub，HTTPS（LAN 入站） | server | Card、SendMessage/GetTask/CancelTask、只读证据任务（8 工具，Evidence scope）；v1 无 push、无 `task.write`、无 ListTasks/GetExtendedAgentCard（返回 `-32601`） | 不要求本地 Agent 支持 A2A；不是 MCP 替代；`INPUT_REQUIRED` 只是中断态，不是授权 |

**MCP 版本策略**：duthub 作为 MCP **server** 必须同时支持 2026-07-28 的 `server/discover` 与 legacy `initialize`；接入 duthub 的外部 Agent 作为 **client** 必须处理 modern/legacy 双 era。rmcp 3.10.0 已有 `DiscoverRequestMethod`（`model.rs:1441`）与 handler 分支（`handler/server.rs:139-140`）；P1 必须同时测两条路径，不能只测 legacy。2026-07-28 把协议改为无状态：删除 `initialize`/`notifications/initialized` 握手、`Mcp-Session-Id` 与 SSE 断点续传，协议版本与客户端能力随每个请求的 `_meta` 传递；服务端必须实现 `server/discover`。这正好适合 duthub 的短请求工具面。

**ACP 只把 MCP 连接信息放进 `session/new`**：`mcpServers` 列表由 client 提供，Agent 应当连接 client 指定的全部 server。duthub 的 MCP 工具清单没有「启动 Agent」，也不把 A2A 当成 Agent 启动器。

**A2A 只接受结构化 skill（DataPart）**：不把自由文本映射到任意工具；未知 skill → `Failed(policy)`。

### 4.3 共享 AgentAccessRuntime 与 runtime overlay

终审裁决：**不采用「每会话独立 AgentAccessRuntime」**。每会话独立 runtime 会与租约管理器分裂：`ControlLeaseManager::set_agent_access` 是 `OnceLock`，只成功一次，`holder_enabled` 读的是主 runtime；临时凭据在租约路径会被误判为停用。正确做法是**一个共享 runtime + overlay**。

| 操作 | 持久 settings | runtime overlay | 说明 |
|---|---|---|---|
| `authenticate_http` | 只查 | 不查 | 持久 listener 只认持久凭据；主 auth 关闭时仍 401，不被临时 token 打开 |
| `authenticate_internal` | 不查 | 只查 | 只查 `runtime_by_fingerprint` + `runtime_listeners`；`AuthSvc` 对 `session-<uuid>` listener 走此入口 |
| `authorize`（含 any-read 判定） | 合并 | 合并 | 临时凭据与持久凭据都可见；`McpServerCore::authorize_any_read`（`server.rs:404`）是 MCP 方法，不是 `AgentAccessRuntime` 的方法 |
| `lease_credential_enabled` | 合并 | 合并 | overlay 凭据不会被误判为停用 |
| `update(settings)` | 重建 | 保留 | 只重建持久表，不清 overlay |
| `enabled()` | 只查 | 不查 | 只看持久 settings，保持既有语义 |
| `set_runtime_listener(listener, cred)` | — | 写入 | 要求 `cred.listener_id == listener.id` |
| `remove_runtime_listener(id)` | — | 删除 | 会话结束 → 该 listener 的 token 立刻 401 |

**租约唯一实例**：`device.lease_manager()`（本轮 `workspace.rs:801`、`:819`）。`lease.set_agent_access(access_runtime.clone())` 只成功一次（`lease.rs:288` 的 `OnceLock`、`:330` 的 set、`:334` 的 `holder_enabled`）。Host 不得自建 runtime，也不得使用 `main.rs:171-177` 缺 `.with_gui_session_lifetime()` 的实例；构造链统一补该方法并加回归（`lease.rs:318` 定义、`duthub-device/src/lib.rs:93` 已用）。

**每会话 listener**：ID 用 `session-<uuid>`，写入 overlay 的 `runtime_listeners`；`AuthSvc` 按 listener_id 是否属于 `runtime_listeners` 选择入口：命中走 `authenticate_internal`，否则走 `authenticate_http`。主 token 的 `listener_id=None` 不在 `runtime_listeners` 中，按 listener scope 被拒，而不是只靠「另一个 runtime 实例」隔离。若未来有写/租约动作，`lease_credential_required` 从 runtime cred 的 `lease_required` 读取。

**必测（计划，未运行）**：

- `settings.enabled=false` + `listeners=[]` + overlay：internal 认证/acquire/validate 成功；`authenticate_http` 401；`enabled()` 仍 false；`lease.set_auth_enabled(false)` 不被改写。
- auth-off + overlay + 无租约 `serial_write` 必须拒绝，不能走 `server.rs:518-521` 的放行。
- 临时 listener 无 token/错 token/吊销后均 401；主 listener 的认证开关不被改变；线程/端口回到基线。

### 4.4 Listener 生命周期

现有 `serve_http_listener_blocking`（`transport.rs:177-215`）的 `axum::serve` 没有 graceful shutdown，线程是 detached；会话结束无法回收端口与线程。v5 必须新增：

- `ListenerHandle { shutdown: oneshot::Sender<()>, join: JoinHandle<()> }`。
- axum `with_graceful_shutdown` + 线程 join。
- Host 与 A2A 都用注册表管理；会话结束/关闭时 `shutdown` + `join`。
- 测试断言：吊销后 401，且线程/端口回到基线（不增长）。

**会话 listener 的传输与认证**：每会话新建 loopback TCP listener（`127.0.0.1:0`，临时端口），`listener_id=session-<uuid>`，`listener_dut=Some(dut)`。`set_runtime_listener` 先写 overlay，再启动 listener，`ListenerHandle` 存入 ListenerRegistry。`AuthSvc` 按 listener_id 是否属于 `runtime_listeners` 选择入口：命中走 `authenticate_internal`，否则走 `authenticate_http`。外部 Agent 用 `http://127.0.0.1:<port>/mcp` + session token 连接；这是 TCP，但认证入口是 internal。撤销时先从 overlay 删除（立即 401），再 shutdown/join 释放端口。端口不得与 `agentAccess.listeners[]`/A2A 端口冲突；绑定失败最多重试 2 次，仍失败则会话启动失败。实际调用点在 `apps/desktop/src/mcp_runtime.rs:528`；`duthub-mcp` 没有 `mcp_runtime.rs`。

**A2A 每任务 MCP 不再起 TCP listener**：改用进程内 `AsyncRwTransport` over `tokio::io::duplex`（进程内 transport 设计）。A2A 的公共 listener 仍用同一个 `ListenerHandle`。

### 4.5 StateDb 上移 core

终审裁决：**StateDb 放在 `crates/duthub-core/src/statedb.rs`**，ADR-9 取代 T23/T42 的归属争议。原因：T42 要求「通过 duthub-core 的 StateDb 访问同一 state.db」，而 T23 的 StateDb 在 `crates/duthub-agent/src/store`；ADR-0 又明确不填 `duthub-agent`。两者不能同时成立，必须把 StateDb 上移。

- API：`open_state_db`、`migrate`、`CURRENT_SCHEMA_VERSION`。
- DB：`<work_dir>/agent/state.db`，WAL + `synchronous=FULL` + `busy_timeout` + `integrity_check` + 512 MiB 上限。
- 依赖：`rusqlite`（bundled）加入 `duthub-core` 的 Cargo.toml/lock；版本由 P0 选定（本次未核实）。
- `duthub-a2a` 只依赖 `duthub-core`，不直接依赖 `rusqlite`；`a2a_tasks`/`a2a_messages`/`a2a_dedupe` 由 core migration 注册。
- 不选 redb：需要 SQLite WAL/`synchronous=FULL` 语义与现成迁移/查询能力。
- P0 必须把 `rusqlite` bundled 与 `+crt-static` 的构建门写进验收；当前 `Cargo.lock` 中 `rusqlite`/`redb`/`x509-parser` 计数均为 0（本轮实测）。

### 4.6 配置单一来源

| 配置 | 落点 | 谁写 | 谁读 |
|---|---|---|---|
| 入站 peer | `<data_dir>/peers.json` | 只在 GUI 写 `peers.json` + secret 后端 | A2A listener |
| peer token | keyring service `duthub`、条目 `a2a/<dut>/peer/<peer>`（interactive）或后续 DPAPI | GUI 注册 | A2A 认证 |
| 全局 A2A 默认/旧配置 | `settings.a2a` | 设置页（标注「仅默认值」） | 兼容读取；`host` 废弃、不参与绑定 |
| 出站 peer 目录 | `settings.a2a.peers` | 设置页 | 只做出站展示；入站永不读取 |
| 每 DUT A2A 预配置 | `duts.json` 的 `dut.a2a` | 连接管理 | 启动时解析为 listener 配置 |
| Agent 定义 | `<data_dir>/agents.json` | GUI/文件选择器 | HostManager |

**裁决**：

- 不新增 `settings.a2a.inboundPeers`；入站 peer 只认 `peers.json`。
- `settings.a2a` 只保留全局默认/旧配置兼容；`host` 废弃，不参与绑定。
- `A2aConfig.peers` 保留为**出站** peer 目录，入站路径不读；UI 必须标注「出站」。这一条在 ADR-6/ADR-8 里再确认，避免 P6 后出现两套 UI 真相。

**路径与配置落点**：`<data_dir>` = `<exe_dir>/.duthub`（启动配置，不随 `workspaceDir` 移动）；`<artifact_root>` = CLI `--workdir` > `settings.workspaceDir` > `<exe_dir>`；`<work_dir>` = `<artifact_root>/duthub-data`。`peers.json`/`agents.json` 在 `<data_dir>`；`sessions.json`/`state.db`/`audit/`/`logs/`/`agent/<session>/home/` 在 `<work_dir>`。`.duthub/` 不等于 `<work_dir>`，见 §2.9。

**JSON 生命周期**：`peers.json`/`agents.json`/`sessions.json` 各带 `version`；写入用 temp+rename 原子替换；损坏文件备份为 `.bak` 并 fail closed（不静默回退空配置）；GUI 是唯一写者；迁移失败不覆盖旧文件，启动时报错并保留旧值。

### 4.7 ADR 与 supersession

| ADR | 裁决 | 取代/影响 |
|---|---|---|
| ADR-0 | v5 范围：不建 `duthub-runtime`、不填 `duthub-agent`、11 成员、Agent 与 duthub 同生共死、不保留 headless/无人值守 | 必须产品签字；否则 A2A 标 experimental |
| ADR-1 | ACP 自研 vs SDK：不引 `agent-client-protocol`，按 wire v1 自研薄 client；并入 ADR-5 | 旧 ADR-1 不单独存在，避免两套 ACP 决策 |
| ADR-2 | A2A LAN 入站；每 DUT listener、TLS、peer allowlist | 取代旧「仅 loopback」契约与 `A2aConfig.host` 注释 |
| ADR-3 | **内嵌** goose-acp v1.60.2：`duthub.exe` 内嵌解压后的 exe + LICENSE/NOTICE/manifest；zip 仅来源记录；外部路径可选覆盖（收口轮已接入启动链） | 必须产品签字（2026-11-21 修订重签）；发布门禁见 ADR-3 §6 |
| ADR-4 | 协议分工/版本：MCP 2026-07-28 + legacy、ACP v1、A2A 1.0；并入 ADR-8 | 旧 ADR-4 不单独存在 |
| ADR-5 | ACP 不声明 `fs/*`、`terminal/*`，一律 `MethodNotFound` | 写入 `duthub-acp` 协议层 |
| ADR-6 | `agents.json`/`peers.json` 单写者；GUI 是唯一写入口 | 禁止 CLI/后台进程直接写配置 |
| ADR-7 | 通用 listener 保留全局读；DUT 专用 listener 收紧 | 不改既有 e2e；UI 明示「通用 token 不是 DUT 隔离」 |
| ADR-8 | Card 1.0.1（`supportedInterfaces`/`protocolBinding`/`securityRequirements`）；取代旧 best-practice §4.5（内部 26 工具）、§4.7、§5.5、§6.5；取代 T39 的 `preferred_transport` | 实现者不得按旧文档开工 |
| ADR-9 | StateDb 上移 `duthub-core`；取代 T23/T42 的归属 | `duthub-a2a` 不直接依赖 `rusqlite` |
| ADR-10 | token 移出 `settings.json` 的可选迁移（keyring/DPAPI）；v5 不实施 | UI 明示风险；后续单独评估 |

**supersession 标注**：`43c58dc:docs/AGENT_IMPLEMENTATION_TASKS.md`、`docs/API_CONTRACT.md`、`docs/SECURITY.md`、`docs/ARCHITECTURE.md` 必须在 P0 同步标注：旧 §4.5/§4.7/§5.5/§6.5 与 T39 旧字段不再有效；否则实现者会按旧文档开工。

**ADR 流程**：ADR 文本放 `docs/decisions/`；签字人 = 产品负责人 + 维护者；ADR-0/2/5/6/7/8/9/10 在 P0 出口前签字，ADR-3 在 P7 前签字；签字记录写 `docs/decisions/ADR_SIGNOFF.md`；未签字不得进入对应阶段。ADR-1/4 不单独存在，已并入 ADR-5/8。

### 4.8 单 exe 与 stdio 兜底

- **v1**：`duthub.exe` 保持单文件（release 已 `+crt-static`）；本轮按 ADR-3 内嵌解压后的 `goose-acp.exe` 与声明文件，首次使用时落到 `<work_dir>/agent/bin/goose/<version>/<sha256>/`，**不**解包到 `%TEMP%`，也不做 `agents/goose/` 旁挂。外部 Agent 默认内嵌；其他 CLI Agent 仍由用户自装。
- **stdio 兜底复用**：不在 `duthub.exe` 重写快照发现。把 `crates/duthub-mcp/src/main.rs` 的 `discover_data_dir`、`configured_data_dir`、`--bridge` 分支抽成 duthub-mcp 的库函数（例如 `bin_support`），duthub-mcp bin 与 `duthub.exe --mcp-stdio/--bridge` 共用。
- **早退顺序**：`duthub.exe` 在 tracing 初始化（`main.rs:116`/`:128`）和 GPUI 初始化之前早退；mcp-stdio 不初始化任何 stdout fmt layer，stdout 只输出 MCP 协议，stderr 可日志；bridge 的 token 从 `DUTHUB_MCP_TOKEN` 读取，不进 argv。
- **验收**：e2e 抓 stdout 断言每行 JSON-RPC、无 `println!`/fmt 日志；覆盖 `--trace` 分支与 `--self-test` 的 `println!`（`main.rs:116`、`:240`）。
- **内嵌资产**：goose-acp v1.60.2（Apache-2.0）随 `duthub.exe` 内嵌（解压后的 exe + LICENSE/NOTICE/manifest），zip 仅来源记录；首次解包到工作根下的版本化目录并校验 SHA-256；不自动下载/更新。`agents/goose/` 旁挂形态 deferred。资产大小与 Defender 行为仍未实测，不写进验收。

---

## Agent 托管与生命周期

对应终审字段 `agentHosting`。核心原则：**duthub 负责进程、会话、凭据、审计；Agent 负责模型、上下文、会话存储、工具决策、自身鉴权**。

### 5.1 启动与会话

1. 面板首次打开某 Agent 的某 DUT 会话 → desktop `HostManager.ensure_started(serverId)`：ACP 进程不在则 spawn。
2. `initialize`：不声明 `fs`/`terminal` 能力；读取 `loadSession`/`sessionCapabilities.resume` 能力位。
3. `session/new`：cwd 为 `<work_dir>/agent/<session>/sandbox`；`mcpServers` 指向 `http://127.0.0.1:<port>/mcp`，带 `Authorization: Bearer <session-token>`。该 listener 是每会话 loopback TCP（临时端口），`AuthSvc` 按 `session-<uuid>` 走 `authenticate_internal`；见 §4.4。
4. CLI 每 turn spawn（Claude `-p --resume`、codex `exec resume`）；turn 结束进程退出，session id 由 duthub 保存。

**多会话 UI**：左 Agent 列表（per-DUT/进程状态）→ 中会话 tab（每会话绑定一个 DUT；ACP 一进程多 session，CLI 每 turn 一个）→ 右 Inspector（工具卡、证据 seq、权限请求、token 状态、transcript、能力位）。同一 DUT 可多只读会话；默认不提供通用 listener 会话（ADR-7）。

### 5.2 进程与退出（同生共死）

- **spawn**：`windows-sys` `CreateProcessW`，`CREATE_SUSPENDED | CREATE_NO_WINDOW`；`STARTUPINFO` 只继承 stdin/stdout/stderr（`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`）。
- **env 白名单**：`PATH`、`SystemRoot`、`SystemDrive`、`windir`、`COMSPEC`、`PATHEXT`、`APPDATA`、`LOCALAPPDATA`、`PROGRAMDATA`、`USERPROFILE`、`HOMEDRIVE`、`HOMEPATH`、`TEMP`、`TMP`、`HOME`、`USERNAME`、`PROCESSOR_ARCHITECTURE`、`NUMBER_OF_PROCESSORS`，再追加 `DUTHUB_AGENT_MCP_URL`/`TOKEN` 与 `agents.json` 的 `env_keys`（值来自 keyring）；不注入 duthub settings/keyring。goose 的 `%APPDATA%\Block\goose`、Node/npm 的 `LOCALAPPDATA`/`TEMP` 因此可见（集成测试）。
- **Job Object**：`CreateJobObject`（`KILL_ON_JOB_CLOSE`、`ProcessMemoryLimit=4 GiB`、`ActiveProcessLimit=16`）→ `AssignProcessToJobObject` → `ResumeThread`；job 句柄不可继承。**收口轮已实现（2026-11-21）**：`apps/desktop/src/agent_host/job.rs`（`windows 0.62.2`）CREATE_SUSPENDED spawn → assign → 枚举线程 Resume，任一步失败终止挂起子进程（fail closed）；嵌套 Job（duthub 自身已在父 Job 中）场景未实测。
- **失败路径**：`CreateProcessW` 失败 → `SpawnFailed`；`Assign` 失败 → `TerminateProcess` 挂起子进程 + `CloseHandle` + 告警（**绝不放行未进 job 的子进程**）；`ResumeThread` 失败 → `TerminateProcess` + 告警；job 句柄提前关闭/继承 → 重建并告警。
- **退出**（设计目标；Job Object 已由收口轮实现）：Job Object 是唯一保证；`on_window_should_close`（gpui-pre `window.rs:7162`，同步 bool）返回 false，启动 `cx.spawn` 清理：`session/cancel` → 关 stdin → 等 5 秒 → `TerminateJobObject` → 置 `shutdown_done` 再关窗；不在 UI 线程阻塞；`on_app_quit` 只有 200ms（`app.rs:80`/`:1310`），不作为保证；`cx.quit()`/强杀/崩溃都靠 job 关闭（`job.rs` 的 `KILL_ON_JOB_CLOSE` 兜底）。**本轮实际**：`HostManager::drop` 停止所有会话并 `kill` 子进程，Workbench `Drop` 再关闭 MCP 监听器。
- **不保留 headless/无人值守**：v5 没有 `--runtime`/`--unattended`/计划任务/Session 0/24/7。GPUI 进程关闭后 Agent 不存活；A2A listener 同步停止。产品若要求常驻，必须另立 ADR，不能在本方案里加回。

### 5.3 崩溃、卡死与重启

- **ACP 常驻**：非预期退出 → 仍有会话则重启（3 次/10 分钟，退避 1s/2s/4s）；成功 `load`/`resume`，失败新会话 + 常驻「已降级为新会话」横幅；超预算 `Failed` + `AlertCenter`。
- **CLI**：退出码 0 正常；非 0 同一 turn 重试 1 次。
- **watchdog**：看 ACP/MCP/stderr；10 分钟无活动 → cancel → 30 秒 → 杀树；有 in-flight MCP 调用延长到 20 分钟且不自动杀。10 分钟是设计阈值，不是实测结论；纯模型长思考可能长时间无 ACP/MCP 事件，P4 必须用长思考 fixture 验证，误杀则提高阈值。
- **重试副作用**：CLI 非 0 重试只用于无本地工具副作用或已幂等的 turn；有本地工具副作用时不得盲目重试，需记录并转人工。
- **全局上限**：Agent 进程/会话 listener/A2A 任务的全局上限未定，P1/P6 压测定值；未定前按 per-DUT 1 个分析任务、每 peer 4 并发、Job 16 进程执行。端口由 ListenerRegistry 全局登记，禁止与 `agentAccess.listeners[]`/A2A 端口冲突。
- **transcript**：8 MiB×10（80 MiB 上限）；面板 2000 条；load 重放 50k/32 MiB 以上只渲染尾部；Job 内存 4 GiB；Host 不做上下文压缩（Agent 负责）。transcript 是环形缓冲；`config_dir` 为 resume 保留但提供用户清除；审计/A2A/evidence 按各自保留策略，不做隐式备份。

### 5.4 流解析与 UI

- stdout 只喂协议（ACP 行 JSON-RPC / CLI 行 NDJSON）；行缓冲兼容 `\r\n`、UTF-8 lossy、单行 4 MiB 超限杀进程。
- stderr 独立线程环缓冲；有界通道背压；Host 生成 UI seq（不依赖 ACP event id），供 Inspector 的工具卡/证据 seq 使用。

### 5.5 会话 listener 与 token

- 每会话 listener ID `session-<uuid>`，`listener_dut=Some(dut)`；`set_runtime_listener(listener, cred)` 写入 overlay，`cred.listener_id == listener.id`。
- `AuthSvc` 按 listener_id 是否属于 `runtime_listeners` 选择入口：命中走 `authenticate_internal`，否则走 `authenticate_http`；`authenticate_http` 不认临时 token。
- 会话结束 `remove_runtime_listener(id)` → 该 token 立刻 401；再 `shutdown` + `join` listener。
- 主 token 的 `listener_id=None` 不在 `runtime_listeners`，按 listener scope 被拒；不能只靠「另一个 runtime 实例」隔离。

### 5.6 self-test 与 demo 迁移

- **self-test**：`--self-test` 时 HostManager/A2A 不启动、StateDb 用临时目录、不绑端口、不写真实 `work_dir`；测试断言 self-test 后无 `agent/`/`a2a` 文件与 listener。`scripts/regression` 第 4 步会跑该模式；P0/P2 验收必须包含 `bash scripts/regression`。
- **保留**：provider catalog 与 `test_model_connection`（迁移到 `provider_models.rs` 的 `ProbeState`，删除 `agent.rs` 的重复 `MODEL_TEST_RESULT`/`start_model_test`）；`AlertSource::Agent` 通道（`workspace.rs:4365-4379`）保留，生产改为 Host 的 crash/restart_failed/hang；`settings.agent.provider`/`slow` 与 `workspace.rs:613-615`/`:763-776` 保留给 provider catalog。
- **删除**：`AgentState`/`AgentSession`/`RunStatus`/`Workflow`/findings、900ms STEP、OFFLINE DEMO 报告（`agent.rs:492` 删除旧报告）；transcript 导出由 P4 的 T37 接管，W2 不依赖 W4 文件。
- **回归**：provider 探针仍可用、crash 仍走 `AlertCenter`、`--self-test` 仍只断言帧数。

---

## 工作路径与 Plan 模式

对应终审字段 `agentSandbox`。

### 6.1 工作目录解析顺序

**唯一权威顺序（本轮读取 `store.rs:77-103`）**：

1. CLI `--workdir`（`cli_dir`）
2. 保存的 `settings.workspaceDir`（`settings_dir`）
3. exe 所在目录

`ensure_portable_work_dir` 的实现是 `[cli_dir, settings_dir]` 取第一个非空，再 `exe_dir.join(...)`；目录不可写时回退 exe 目录并返回提示（`store.rs:89-102`）。桌面在 `main.rs:82-94` 调用它得到 `artifact_root`，再定义 `work_dir = artifact_root/duthub-data`。

**Agent cwd 收窄**：默认 cwd = `<work_dir>/agent/<session>/sandbox`。不把 `.duthub`、`duts.json`、DUT 资料目录作为 cwd。这是同一解析器下的收窄，不新增第二套规则。若产品坚持 `cwd=artifact_root`，必须 ADR 记录并接受同用户可读 `.duthub` 的风险。

**现有歧义（必须收口）**：同一 crate 的 `resolve_work_dir_in`（`store.rs:133-157`）优先级是 settings > CLI，与 `ensure_portable_work_dir` 相反。桌面 `main.rs` 用的是 `ensure_portable_work_dir`；其他调用点若用 `resolve_work_dir_in` 会得到不同根。Agent 的 cwd/写边界不能建立在这套歧义上；P0/P2 必须先统一解析器，再接线 Host。

**未核实**：`StartupContext.artifact_root` 的实际注入与所有调用点是否都走 `ensure_portable_work_dir`；本轮只读了 `main.rs:82-94` 与 `store.rs` 两段。

### 6.2 默认 Plan 只读边界

Plan 不是单一开关，而是三层：

| 层 | 机制 | 强度 | UI 文案 |
|---|---|---|---|
| 工具面（硬） | Agent 只经 MCP 访问 duthub；`PLAN_ROUTER` 固定 13 个只读 Plan 工具（用户配置的 listener 另挂 `notify_user`，只唤起 UI、不写设备）；DUT 绑定 listener 再收窄到单 DUT | 硬边界：非白名单工具在路由层不可达；`notify_user` 不能触设备 | 「工具面：13 个 Plan 工具（只读）」 |
| Agent 侧（软） | ACP 不声明 `fs`/`terminal`，`request_permission` 默认 deny；CLI 用 `--permission-mode plan`/`--sandbox read-only`；Claude `--strict-mcp-config`、codex 持久化 `CODEX_HOME`；goose 无只读模式，默认 `mcp_only`；dsh 未验证 read-only 前不称只读 | 依赖 Agent 遵守；不是 OS 强制 | 「本机工具：未限制 / 受限令牌（仅挡写）/ 专用账户（读写受限）」 |
| OS 级（可选） | 受限令牌（`WRITE_RESTRICTED`，只挡写）；专用账户 + NTFS ACL（读写都限制，需管理员一次性配置）；AppContainer（二期） | 真正的读隔离需要专用账户/ACL 或 AppContainer | 按实际配置显示，不夸大 |

**两条 UI 文案必须同时显示**：

1. 「工具面：13 个 Plan 工具（只读）」
2. 「本机工具：未限制 / 受限令牌（仅挡写）/ 专用账户（读写受限）」

**默认配置下不得宣称**「DUT 隔离」或「Plan 只读」。原因：

- 协议级 DUT 绑定只约束工具参数与授权路径；`settings.json` 里的通用 MCP token 是明文（`agent_access.rs:159-162`、`crates/duthub-core/src/agent_access.rs:664` 的测试），同用户 Agent 读到后可调通用 listener，读到全部 DUT 资料（`server.rs:718` 通用 listener 全局读）。
- `cwd` 只是协议约定，不是 OS 强制；ACP 规范明说 root set 是 SHOULD，shell 只有在沙箱执行时才受约束。
- 受限令牌只挡写，不能替代读隔离；AppContainer 需要按 capability 重新授权文件访问。

**越界拦截**：经 MCP 的读取走 `dut_artifacts`（cap-std，拒绝 `..`、绝对路径、junction/symlink）与 `log_archive`，不接受任意路径；Agent 本地工具无法在用户态拦截，只能靠 OS 限制。写模式开关只影响本地工具，且必须 OS 限制或用户二次确认；duthub MCP 写链仍不可达（`PLAN_ROUTER` 传输层固定），要让 Agent 经 MCP 写硬件需独立写链 ADR（v5 不做）。

**失败降级**：Agent 不支持只读且无法禁用本地工具 → 默认不推荐启用；启用需确认风险，cwd 为空 sandbox，工具面仍只有 Plan 13；Agent 不消费 `mcpServers` → 标「未接入 duthub 工具」，不允许读 DUT。

**外部 Agent 的本地工具不是 duthub 能约束的面**：默认 `mcp_only` 只约束 MCP 工具；Agent 若在本机有 shell/文件权限，可直接打开 COM、驱动继电器或调用烧录工具，绕过 duthub 的租约/审批。v5 的裁决：默认把外部 Agent 视为不可信；没有专用账户/ACL/受限令牌等 OS 隔离时，外部 Agent 标 experimental，UI 明示；`mcp_only` 不是安全边界。

---

## 长时任务

对应终审字段 `longRunning`。目标不是「进程永不死」，而是「进程死了状态不丢、重启后知道做到哪一步」。

### 7.1 Agent 会话连续性

- **延续条件**：duthub 开着 + Agent 子进程活着 + 会话在 Agent 自身存储（goose `sessions.db`、Claude `--resume`、codex rollout + resume；dsh ACP 省略 load → 新会话）。
- **能力位**：ACP `initialize` 后读 `loadSession`/`sessionCapabilities.resume`；无则新会话 + 常驻「已降级为新会话」横幅。
- **duthub 重启**：按 `sessions.json` 恢复；Agent 进程需重新 spawn，再用 Agent 自己的 resume/load；失败则新会话 + 横幅，不假装恢复。

### 7.2 sessions.json 与 config_dir

每会话持久配置（`sessions.json`）：

`sessions.json` 记录 `sessionId`/`serverId`/`dutId`/`runtimeListenerId`/`agentSessionId`/`resumeId`/`cwd`/**`config_dir`**/`auth_mode`/`mcp_config_path`/`createdAt`/`lastPromptAt`/`restartCount`。

- `config_dir` 放 `<work_dir>/agent/<session>/home/`；会话结束不立即删除（保留给 resume）；duthub 重启后按记录恢复。
- CLI 的 `CLAUDE_CONFIG_DIR`/`CODEX_HOME` 必须持久化，否则 resume 找不到 session/rollout。
- **token 交付**：Claude `--mcp-config` 用 env 展开 `${DUTHUB_AGENT_TOKEN}`（若锁定版本不支持则用 ACL 文件）；codex 用 `bearer_token_env_var`；token 不进 argv、不落盘。
- **测试**：duthub 重启后 resume（Claude/Codex 各一）。
- **JSON 生命周期**：`sessions.json` 带 `version`、原子写、损坏备份、fail closed；GUI 唯一写者；config_dir 保留给 resume，但提供用户清除入口。
- **保留/删除**：transcript 80 MiB 环形；config_dir 保留给 resume 但 GUI 可清除；A2A 终态 30 天；evidence 512 MiB/任务；audit 按日 JSONL，提供手动清理；默认不做隐式备份。

### 7.3 Journal、checkpoint、证据 pin 与幂等

- **A2A 任务**：`agent/tasks/<task_id>/journal.jsonl` append-only + 每事件/每步 fsync；`checkpoints` 原子写保留 20 个 + 每日 1 个；`evidence_ref` 生成时把引用区间 copy 到 `agent/tasks/<id>/evidence/` 并记 SHA-256，防 `logs.rs` 的 2 MB×20 轮转丢证据；每任务 evidence 512 MiB 配额。
- **定时不是常驻**：每日 checkpoint、30 天保留清理、证书每日检查都不是常驻定时器；duthub 启动时按 `last_run` 补做（catch-up），关闭期间错过；A2A 任务在关闭期间不推进。
- **幂等键**：`step_key=sha256(task_id|run_id|step_index|kind|canonical_args)`；`tool_key=sha256(task_id|step_key|tool|args_hash)`；A2A `messageId` 去重；已 Succeeded 的步骤取记录结果，不无条件重跑。
- **Agent turn**：Host 记 `prompt_id` 与 journal；重复 `prompt_id` 返回记录/不重复发送；duthub 崩溃时把在飞 turn 标 `Interrupted`，重启后先 resume Agent 会话再决定继续/新会话。

### 7.4 A2A 状态机

| 内部状态 | 含义 | 允许的下一步 | 对外 wire 映射 |
|---|---|---|---|
| `Submitted` | 已接受，尚未执行 | `Working` / `Canceled` | `Submitted` |
| `Working` | 执行中 | `InputRequired` / `Paused{reason}` / `NeedsInspection` / `Completed` / `Failed` / `Canceled` | `Working` |
| `InputRequired` | 等待远端 peer 输入（不是本机操作员） | `Working` / `Canceled` | `INPUT_REQUIRED` |
| `Paused{NeedsHuman}` | 需要人工介入 | 人工恢复 / `Canceled` | `Working` + `metadata.duthubState="paused"`/`requiresHuman=true` |
| `NeedsInspection` | 状态不明/证据缺失 | 人工检查 / `Canceled` | `Working` + `metadata.duthubState="needs_inspection"` |
| `Interrupted` | 进程关闭/崩溃打断 | 恢复 / `NeedsInspection` / `Canceled` | `Working` + `metadata.duthubState="interrupted"` |
| `Completed` | 成功终态 | 保留 30 天 | `COMPLETED` |
| `Failed` | 失败终态 | 保留 30 天 | `FAILED` |
| `Canceled` | 取消终态 | 保留 30 天 | `CANCELED` |

规则：

- `INPUT_REQUIRED` 是远端 peer 输入，不是本机操作员；`Paused{NeedsHuman}`、`NeedsInspection` 禁止自动重试。
- `CancelTask` 在 `Protected` 时返回 `working` + `metadata.cancel_pending=true`，收尾后 `GetTask` 返回 `canceled`。
- `Completed`/`Failed` 保留 30 天；`return_immediately` 用于长任务；`pollIntervalMs=5000`、长轮询 25 秒、请求体 ≤256 KiB、message ≤64 KiB。
- `AUTH_REQUIRED`/`INPUT_REQUIRED` 只是中断态，不是对任何操作的授权。

**对外映射**：A2A wire 只承认 `Submitted`/`Working`/`InputRequired`/`Completed`/`Failed`/`Canceled`/`Rejected`。内部 `Paused{NeedsHuman}`/`NeedsInspection`/`Interrupted` 不新增 wire 状态，统一映射为 `Working` + `metadata.duthubState`（`paused`/`needs_inspection`/`interrupted`）+ `metadata.reason`/`requiresHuman`；`GetTask` 返回标准状态，内部恢复靠 metadata 判断。远端 peer 永远不会看到非标准状态。

### 7.5 恢复语义（不夸大）

duthub 重启：

1. 开 DB + `integrity_check`。
2. 扫 `Working`。
3. 加载 checkpoint。
4. 按 `task_credentials` 重签 token/重绑 listener。
5. 重放 journal 中 Succeeded 步骤。
6. 对 `Intent`/`Dispatched`/`Unknown` 做对账；无法确认 → `NeedsInspection` + 暂停。
7. evidence 缺失 → `Paused(NeedsHuman)`。

**不写「确定性重跑」**：没有真实 fixture/证据 pin 前不承诺。Working 恢复只做 journal 重放 + Succeeded 取记录 + Unknown 对账 + NeedsInspection，宁可不自动继续。A2A 在真实 fixture 通过前标 experimental。

**正常关闭**：停止接受新 `SendMessage` → 对在飞 `Working` 写 checkpoint + journal `Interrupted` → 关闭 listener/DB；远端看到连接拒绝；重启后按恢复流程决定继续/NeedsInspection，不自动 Cancel（除非 peer 明确 `CancelTask`）。

### 7.6 资源上限与看护

- transcript 8 MiB×10（80 MiB 上限）；面板 2000 条；load 重放 50k/32 MiB 以上只渲染尾部；Job 内存 4 GiB。
- Host 不做上下文压缩（Agent 负责）；上下文压缩策略由 Agent 自身实现。
- 证据 pin 只覆盖已取到的区间：`serial_read` 需 open session（`server.rs:629-639`）、`analyze_serial` 取内存尾部（`:1265-1276`）、日志 2 MB×20 轮转；缺失 evidence 时只能 Paused/NeedsInspection，不能假装恢复。
- P4 前用压力脚本测量 transcript 写入吞吐、GUI 内存、`session/load` 首屏、MCP 大响应分页；本文的 8 MiB×10、2000 条、50k/32 MiB 是设计阈值，不是实测结果。

### 7.7 与 Temporal/MCP Tasks 的关系

- Temporal 的 Event History + replay 是设计参照：Activity 结果、Timer、消息写入历史，崩溃后另一个 Worker 重放历史重建状态，已完成步骤取记录结果不再执行；Continue-As-New 用同一 Workflow Id、新 Run Id 开启新历史并把最新状态传入，从而「永远运行」。duthub 不引入 Temporal 服务端，只借 journal/checkpoint/幂等思想。
- MCP Tasks 扩展（`io.modelcontextprotocol/tasks`）提供 `CreateTaskResult`、`tasks/get`、`tasks/update` 与 `working/input_required/completed/failed/cancelled`，并建议客户端持久化 taskId 以便崩溃后续 poll。duthub 不把 MCP taskId 当作唯一状态源；A2A 任务状态与证据在 duthub 自己的 StateDb/journal 中。

---

## 跨机 A2A

对应终审字段 `crossMachine`。

### 8.1 前置裁决与可用性

- **先决**：ADR-2（A2A LAN）、ADR-8（Card 1.0.1 + supersede 旧 §4.5/§4.7/§5.5/§6.5）、ADR-9（StateDb 上移 core）+ 产品签字。`43c58dc:docs/IMPLEMENTATION_PLAN.md:45`、`model.rs:94-104`、`docs/SECURITY.md:9` 的冲突必须裁决。
- **可用性**：A2A 只在 duthub GUI 进程存活时可用；v5 不做 headless/服务/计划任务，TaskStore 持久化只解决状态丢失，不解决关机期间不可达。ADR-0 必须由产品签字接受；否则 A2A 标 experimental。
- **未签字前的口径**：A2A 不进入产品验收；没有真实 A2A 1.0 peer 或锁定 wire 录包时，只作 experimental。

### 8.2 Agent Card 1.0.1

按 A2A 1.0.1：

- `supportedInterfaces[{url, protocolVersion:"1.0", protocolBinding:"JSONRPC"}]`（首项 preferred）。
- `securityRequirements`（不是 `security`）与 `securitySchemes`。
- `A2A-Version: 1.0`（Major.Minor）。
- 缺失/不支持版本 → `-32009`；响应加 `Vary: A2A-Version`。
- 顶层不再有 `url`/`preferredTransport`/`security`。

**发现端点**：每 DUT listener 的 Card 固定 `https://<bind_addr>:<port>/.well-known/agent-card.json`；`supportedInterfaces[].url` 用同一 base（不含路径前缀）；SAN 必须含 `bind_addr`；防火墙放行该端口；IP/DHCP 变化 → `CertMismatch`。不做通配符，不把多个 DUT 混在一个 Card。

**取代**：T39 与旧 best-practice §6.5 的 `url`/`preferred_transport`/`security` 由 ADR-8 取代；实现者不得按旧字段写 Card。

**契约测试（计划，未运行）**：

- 字段名：`supportedInterfaces`/`protocolVersion`/`protocolBinding`/`securityRequirements`/`securitySchemes`。
- 版本错误码：缺失/不支持 → `-32009`；`Vary: A2A-Version`。
- Card 不含 DUT 名、COM、路径、hostname、token；≤16 KiB。
- `ListTasks`/`GetExtendedAgentCard`/push → `-32601`；Card capabilities 不承诺这些能力。

### 8.3 peers.json 与配置单一来源

- 入站 peer 只用 `<data_dir>/peers.json`（单一配置源原则）。
- 字段：`peer_id`/显示名/`allowedDuts`/`allowedSkills`/`allowedActions`/`source_cidrs`/`cert_sha256`/token 指纹/`enabled`/配额/创建/吊销时间。
- token 明文进 keyring service `duthub`、条目 `a2a/<dut>/peer/<peer>`（interactive）或 DPAPI（后续）；`peers.json` 不存明文 token。
- 旧 `a2a/<dut_id>` 迁移为 `local-operator` peer（`allowedDuts=[dut]`，`task.read`），GUI 显式确认，失败 fail closed。
- `settings.a2a` 只保留全局默认/旧兼容；`host` 废弃、不参与绑定；不新增 `settings.a2a.inboundPeers`；`A2aConfig.peers` 保留为出站目录（见 §4.6）。

### 8.4 拓扑、bind_addr 与升级

- 每 DUT 一个 listener。
- `DutA2aConfig.bind_addr: Option<String>` + `#[serde(default, skip_serializing_if = "Option::is_none")]`；默认 `None`。
- 启动校验：必填、本机具体网卡 IP、拒绝 `0.0.0.0`/`::`。
- 升级：旧 `duts.json` 中 `a2a.enabled=true` 但 `bind_addr=None` → 状态 `NeedsBindAddr`，不启动，GUI 引导选择网卡；多个 DUT 默认 8790 → `PortConflict`，阻塞并提示改端口。
- 旧配置解析保持兼容，追加 `dut_a2a_bind_addr_roundtrip` 用例，证明默认省略不破坏 `docs/examples` 往返（AGENTS #19/#24）。
- `a2a_running: bool` 改为 `BTreeMap<dutId, A2aListenerState>`（`Stopped`/`Starting`/`Running`/`Failed{reason}`/`CertMismatch`/`PortConflict`/`NeedsBindAddr`）；`a2a_running` 仅作聚合兼容，UI 用 per-DUT 状态。当前实际位置 `workspace.rs:631`/`:940`、`pages.rs:3621`；修改前重新 `grep`。
- `port_is_free` 必须按 A2A 的 LAN 开关探测（`workspace.rs:1636-1643` 只看 `agent_access.lan_access`）；LAN 开启时按 `0.0.0.0` 探测。
- **端口与全局上限**：端口由 ListenerRegistry 全局登记，禁止与 `agentAccess.listeners[]`/会话 listener/A2A 端口冲突；释放后回收。Agent 进程/会话 listener/A2A 任务的全局上限未定，P1/P6 压测定值；未定前按 per-DUT 1 个分析任务、每 peer 4 并发、Job 16 进程执行。

### 8.5 TLS

- 进程内 rustls：`rustls` + `rustls-pemfile` + `tokio-rustls`。
- 证书链用 `rustls-webpki`（`Cargo.lock` 已有 0.103.17）；`x509-parser` 只做 SAN/notAfter/SPKI。
- PEM 证书 + 私钥导入是必需路径；私钥文件 ACL 限当前用户（导入时 `icacls`）。
- SAN 必须匹配 `bind_addr`；到期/不匹配 fail closed；每日检查 `notAfter`，30/14/7/1 天审计告警。每日检查不是常驻定时器：启动时按 `last_check` 补做，关闭期间错过。
- IP/DHCP 变化 → listener 不启动、health `cert_san_mismatch`、GUI 提示重签；轮换 = 重新导入 + 重启 listener + 审计。
- **v1 移出 mTLS/DPAPI**：避免未核实 API 与依赖；mTLS 后续单独评估。

### 8.6 认证、peer 与限流

- 每 peer 每 DUT Bearer；运行时只存 SHA-256 指纹 + 常量时间比较。
- `PeerPolicy`：`allowed_duts`/`allowed_skills`/`allowed_actions`/`source_cidrs`/`cert_sha256`；v1 `allowedActions` 只允许 `task.read`。
- 限流：180/min 突发 40、每 IP 240/min、全局 1200/min；并发每 peer 4、每 DUT 1 个分析任务。全局 listener/任务上限未定，P6 压测定值；端口冲突由 §8.4 的 ListenerRegistry 处理。
- 吊销 epoch + 取消 `Working` + 清理 `a2a:<peer>:*`；注册只在 GUI 写 `peers.json` + secret 后端。
- `AUTH_REQUIRED` 不是授权；`task.write` 硬关闭。

### 8.7 读 scope 与方法语义

**8 个证据工具**：

`dut_list`（只列绑定 DUT）、`dut_info`、`dut_control_status`、`serial_read`、`analyze_serial`、`log_segments`、`log_segment_read`、`heartbeat_get`。

**拒绝**：`dut_files`、`dut_file_read`、`media_streams`、`adb_list_devices`、`skill_export`。

- 内部 per-task server 用进程内 transport + `listener_dut=Some(target)` + `ReadScope=Evidence`。
- `task.write` 拒绝注册；SendMessage 只接受结构化 DataPart skill（固定 schema），未知 skill → `Failed(policy)`。
- 3 方法：`SendMessage`、`GetTask`、`CancelTask`；`ListTasks`/`GetExtendedAgentCard`/push 返回 `-32601`。

### 8.8 持久化与启动顺序

- SQLite TaskStore 经 core `StateDb`（见 §4.5）。
- `Workbench::new` 中若任一 `dut.a2a.enabled`：open StateDb + migrate → 启动各 DUT listener（`bind_addr`）→ `Working` 恢复（重签 token/重绑/listener）→ 置 per-DUT 状态。
- 未启用不启动；`Working` 不执行。
- A2A 不依赖用户打开面板（与 MCP runtime 同层）；但 v5 不做 headless，duthub 关闭期间 A2A 不可用；ADR-0 必须由产品签字接受；否则 A2A 标 experimental。
- A2A DB 关闭时 checkpoint + close；正常关闭时停止接受新任务，在飞 `Working` 写 `Interrupted` 并 checkpoint，远端看到连接拒绝；重启后按 §7.5 恢复，不自动 Cancel。

### 8.9 防火墙与审计

- GUI 生成 `netsh advfirewall firewall add rule ... remoteip=LocalSubnet profile=private,domain`；不自动执行、不加 Public、不做 UPnP。
- 审计：`a2a.request`/`auth.fail`/`ratelimit`/`task.*`/`peer.revoke`；不记 token，payload 只记 SHA-256。

---

## 接入与替换

对应终审字段 `agentAdapters`。

### 9.1 agents.json 与凭据

`agents.json` 字段：`id`/`name`/`kind(acp|cli)`/`program`/`args`/`env_keys`/`cliProfile`/`enabled`/`auth_mode`/`trust_level`/`expected_sha256`；落点 `<data_dir>/agents.json`，带 `version`，原子写、损坏备份、fail closed。

- `env_keys` 只存键名，值存 keyring service `duthub`、条目 `acp/<id>/env/<KEY>`；token/模型 key 不进 settings。
- `kind=acp` 的 `program` 必须是可执行文件或原生 exe；npm `.cmd` shim 需额外校验包版本/integrity。
- `kind=cli` 的 `cliProfile` 决定参数模板与事件解析器。

### 9.2 ACP 适配（自研，T35）

- duthub 负责进程、`initialize`、`session/new(cwd+mcpServers)`、`prompt`、`update`、`request_permission`、`cancel`、`load`/`resume`、能力位。
- Agent 负责模型、上下文、会话存储、工具决策、自身鉴权。
- 方法面补齐 `authenticate`/`set_mode`/`set_model`/`set_config_option`；`fs/*`、`terminal/*` 一律 `MethodNotFound`（ADR-5）；未知 `update` 变体安全渲染。
- `load`/`resume` 前重建 runtime listener/token 并重注入 `mcpServers`；能力位缺失则新会话 + 横幅，不重试轰炸。
- **不引 `agent-client-protocol`**：版本演进快（5 个月 4 个 major，5.4.0 发布仅数日），且 duthub 只需要 client 侧薄层；按 wire v1 自研行 JSON-RPC，锁版本 + fixture 测试。

### 9.3 CLI 适配

| Profile | 启动参数（设计） | 认证/配置 | 恢复 | 备注 |
|---|---|---|---|---|
| `claude-code` | `claude -p --output-format stream-json --verbose [--resume <id>] --permission-mode plan --mcp-config <path 或 json> --strict-mcp-config` | `auth_mode=inherit` 复制/只读挂载用户凭据到持久 `CLAUDE_CONFIG_DIR`；失败要求 `api_key`；token 用 env 展开 `${DUTHUB_AGENT_TOKEN}` 或 ACL 文件 | `--resume`/`--continue`；duthub 重启后同一 config_dir + session id | `--verbose` 是 `stream-json` 必需（评审核对，实现前按锁定版本复验）；`--strict-mcp-config` 屏蔽用户 `.mcp.json` |
| `codex` | `codex exec --json -C <sandbox> --sandbox read-only`；MCP 用 `--config mcp_servers.<id>.url=...` + `--config mcp_servers.<id>.bearer_token_env_var=DUTHUB_AGENT_TOKEN` | `auth_mode=inherit` 用持久化 `CODEX_HOME`，只复制 `auth.json` 与必要的 model provider 配置（不复制含 `mcp_servers` 的 `config.toml`）；失败要求 `api_key` | `codex exec resume` 按锁定版本复核；不存在则降级新会话；重启 duthub 后用同一 `CODEX_HOME` + session id resume | 需平台包 + 适配器；`--json` 字段不是稳定 ABI |
| `deepseek` | ACP 优先；`--profile headless --json` 标实验性 | `DEEPSEEK_API_KEY`/凭据；Node >=24.9 | ACP 面省略 load → 新会话 | 未实测真实 prompt；默认 `mcp_only` |
| `generic-ndjson` | 用户自定义命令，stdout 逐行 JSON | 用户负责 | 不承诺 | 高级用户；不承诺字段语义 |

**区分 ACP 与 CLI**：

- **ACP**：长驻进程，`initialize` 后多 session；`session/new` 注入 `mcpServers`；能力位决定 load/resume；duthub 不解析模型输出，只解析 ACP `update`。
- **CLI**：每 turn 一个短进程；参数模板与 stdout 事件流由 profile 解析；token 用 env 或 ACL 文件；resume 依赖 CLI 自身存储。
- 两者都不给 Agent 通用 token；只给 `session-<uuid>` 的 DUT 绑定 token。

**fixture 要求**：Claude（system/assistant/result）、codex（thread/turn/item/error）、ACP mock（authenticate/set_mode/fs/terminal/load mcpServers）；每个 profile 配「MCP 注入后能看到 13 个 Plan 工具」的集成测试。

### 9.4 DeepSeek harness（dsh）的接入口径

对应终审字段 `deepSeekHarness`。

- **存在性/形态**：官方 Windows x64 安装包 0.2.2-rc.5（299,593,552 B，2026-11-18）；npm `@deepseek-ai/dsh` 0.2.2-rc.5，MIT，Node >=24.9；Windows 安装版是 `dsh.cmd` 包装（Electron/Node），没有独立 `dsh.exe`。
- **接入**：首选 ACP：`kind=acp`、`program=dsh`、`args=[--profile,acp]`；ACP 面是 automation-only 子集（无 modes/commands/plans、`authMethods` 为空）。
- **备选**：仅实验性 `--profile headless --json`（`cliProfile=deepseek`）；`--profile sdk` 不接。
- **只读/cwd**：cwd 由 `session/new` 的绝对路径给定（duthub 的会话沙箱）；只读靠 dsh 的 sandbox 配置，Windows ACL 后端对 read-only 只报 partial enforcement，出厂 preset 只有 `workspace-write`/`danger-full-access`，read-only 需用户自行配置；ACP 面不暴露 sandbox，duthub 无法从协议侧证明只读。因此默认 `trust_level=mcp_only`，不标「Plan 只读」；只有用户提供配置且 duthub 能读到/探针验证后才升级。
- **不可用降级**：未安装/Node <24.9/无 `DEEPSEEK_API_KEY`/initialize 失败/read-only 未验证 → 面板显示「DeepSeek harness 不可用：原因」，不自动换模型、不自动装 Node、不自动改用其他 Agent；用户可切到其他已配置 Agent。dsh 是 developer preview，版本锁定，升级需用户确认。

### 9.5 手动安装、检测与信任

- **默认内嵌**：v1 按 ADR-3 内嵌 goose-acp；其他 Agent 仍不捆绑、不自动下载。duthub 只做检测与托管。
- **检测**：只允许文件选择器显式添加；PATH 候选只展示不自动执行；展示完整路径/版本/SHA-256/Authenticode，确认后 hash 固化。**当前实现（收口轮）**：外部 goose 路径**已接入启动链**（`agents.json` + `agent_host/registry.rs`，Host 经 `registry::resolve` 启动）；Authenticode `NotSigned` 的应用内徽标已实现（「未签名（仅 SHA-256 校验）」，`agent_ui.rs:846`）；签名链验证仍 deferred。
- **npm `.cmd` shim**：不能只钉 shim 的 SHA-256；需校验包版本/integrity 或要求原生 exe。shim 的 argv 启动与包代码不是同一个对象，TOFU 可被绕过。
- **dsh**：检查 Node >=24.9 与 `DEEPSEEK_API_KEY`；不满足则显示不可用原因。
- **trust_level**：`mcp_only`/`verified_readonly`/`os_sandboxed`。goose/dsh 默认 `mcp_only`（工具面只读，本机工具未限制），不得在 UI 写「Plan 只读」；`verified_readonly` 需要文档化配置 + 探针（goose 关闭 developer/本地工具扩展的机制未核实；dsh 的 ACP 面不暴露 sandbox，需读配置/探针），未验证前不授予。
- **更新**：manifest 锁版本 + SHA-256；duthub 只做 `--version` 探测并显示，不自动下载/自动更新；从安装目录执行第三方 exe 的 AV 影响未实测；企业分发建议签名/哈希白名单。

### 9.6 替换、降级与可选捆绑

- 无 ACP 但有 CLI profile → 显式切 CLI 并标能力子集。
- ACP 握手失败/版本不符 → 不可用，不重试轰炸。
- 无 load/resume → 新会话 + 横幅。
- 无流式 → 一次性呈现并标注。
- 不消费 `mcpServers`（goose 待核实）→ 生成扩展配置指引并明示工具面未注入。
- **内嵌**：按 ADR-3 内嵌 goose-acp（解压后的 exe + LICENSE/NOTICE/manifest；zip 仅来源记录），必须产品签字；`agents/goose/` 旁挂 deferred。Copilot 只允许用户自装（196,113,184 B、专有许可、只能未修改随附且不得单独分发）；Claude Code（Node + 商业条款）、dsh（约 299,593,552 B、预览）同样只允许用户自装；codex 需平台包 + ACP 适配器，不捆绑。

---

## 安全

对应终审字段 `security`。

### 10.1 认证/授权不变量

- **共享 runtime/overlay**：见 §4.3。必测 auth-off + listeners=[] + overlay 三连：internal 认证/acquire/validate 成功、`authenticate_http` 401、`enabled()` false、`lease.set_auth_enabled(false)` 不被改写；auth-off + overlay + 无租约 `serial_write` 拒绝。
- **租约唯一实例**：`device.lease_manager()`；`set_agent_access` 只成功一次；构造链补 `.with_gui_session_lifetime()`。Host 不得自建 runtime，也不得用 `main.rs:171-177` 的缺方法实例。
- **DUT 绑定**：每会话 listener 用 `session-<uuid>` runtime listener，`listener_dut=Some(dut)`；主 token `listener_id=None` 不在 `runtime_listeners`，按 listener scope 被拒。
- **跨 DUT 回归覆盖 13 个 Plan 工具**：`dut_list` 只列绑定 DUT；`dut_info`/`serial_read`/`analyze_serial`/`log_segments`/`log_segment_read`/`dut_files`/`dut_file_read`/`heartbeat_get`/`media_streams`/`dut_control_status` 对 DUT-B 全部 `UNAUTHORIZED`；`adb_list_devices` 不返回 DUT-B serial；`skill_export` 补 `authorize_any_read`（当前 `server.rs:1280-1292` 只取 `_principal`）。
- **token 被盗 canary**：同用户读 `settings.json` 通用 token 调通用 listener 成功，证明「协议级 DUT 绑定不是安全边界」。这不是要修掉的行为，而是要如实承认的事实。

### 10.2 Token 与审计

- **会话 token**：32 字节 CSPRNG；只存内存；不进 settings/argv/transcript/审计；会话结束 `remove_runtime_listener` → 401 + listener shutdown/join。
- **通用 token 仍在 `settings.json` 明文**（`agent_access.rs:159-162`，用户既有要求）；v5 不静默迁移，ADR-10 评估 keyring/DPAPI；在此之前 UI 明示风险。
- **审计 tap**：`call_tool` 外层记 `tool.call`：tool 名、`dutId`、`credential_id`、耗时、`Ok`/`Denied`/`Error`、结果摘要（长度/条数，不记正文）；`serial_read` 额外记 `last_seq`/`session_id`/`line_count`，`log_segment_read` 记 segment/offset/nextOffset，`dut_files` 记 count；写 `AuditSink`（`audit.rs:75-133` 脱敏）。Inspector 的证据 seq 由这些元数据生成，不能只记长度/条数。
- **审批审计缺口**：`approval.rs:106-108`/`:170-173` 拆独立 PR + 独立回归，不并入本方案。**GUI 租约缺口不同**：修复作为 P1 伴随独立 PR，P1 门必须包含回归（见 §2.4/§4.3）。

### 10.3 DUT 绑定与跨 DUT 回归

- 13 个 Plan 工具跨 DUT 回归 + token 被盗 canary（见 §10.1）；`relay_status`/`flash_test` 是内部只读能力，不对外暴露，另加内部回归。
- `skill_export` 加授权会改变「零 grant 的 HTTP 客户端也能读 SKILL.md」的既有行为；P0 起对 HTTP 零 grant 拒绝（stdio LocalAdmin 保持），T17 完成正式测试/文档。需更新 `docs/API_CONTRACT.md`/`docs/SECURITY.md` 并加「零 grant 的 HTTP 客户端」回归。若产品不接受，保留 `LocalAdmin`-only 并记录例外。
- 通用 listener 保留全局读（ADR-7）；DUT 专用 listener 严格 scoped。UI 必须提示：对通用 Token 勾选 DUT-1 的 read Grant **不会**限制 DUT-B 的文件/日志读取。

### 10.4 Origin/Host 与传输

- 当前 `transport.rs:152` `.enforce_origin_validation()` 且 allowed_origins 为空；rmcp tower 在空 allowlist 时对带 Origin 请求拒绝（`tower.rs:1361-1373`）。
- P2 前用真实客户端探测 Origin/Host，显式 `with_allowed_origins(["http://127.0.0.1:<port>"])` 或仅对 loopback 内部 listener 关闭空 allowlist 校验；不盲目 `disable_allowed_origins`，不放开 Host 校验；加 e2e（带 Origin 不 403）。

### 10.5 A2A 安全

- 默认关闭；LAN 必须 TLS + Bearer + `peers.json` allowlist + 限流 + 审计；Card 不含秘密；push 默认关；`AUTH_REQUIRED` 不是授权；`task.write` 硬关闭；只给 8 证据工具与 `allowed_duts`。
- TLS/SAN/私钥 ACL 见 §8.5；peer 认证见 §8.6；读 scope 见 §8.7。

### 10.6 既有审批/租约与不破坏契约

- 审批令牌与租约语义不变；审批审计缺口（`approval.rs:106-108`/`:170-173`）拆独立 PR，不并入本方案；GUI 租约修复作为 P1 伴随 PR，P1 门必须包含回归（见 §2.4/§10.2）。
- `http_auth_e2e`/`stdio_e2e`/`roundtrip` 一条不改；新增字段全部 `skip_serializing_if` 默认（AGENTS #19/#24）；`skill_export` 加授权是行为变化，需在 `API_CONTRACT`/`SECURITY` 更新并加回归。

### 10.7 不可信输入与提示注入

- MCP 规范把工具安全责任放在宿主：调用任何工具前取得显式用户同意，工具描述/annotations 视为不可信（除非来自可信 server）。
- Agent 输出与工具结果都按不可信输入处理：证据引用只做展示/引用，不直接映射为控制动作；自由文本不映射到任意工具（A2A 只接受结构化 DataPart skill）。
- 写模式开关只影响本地工具；duthub MCP 写链不可达。任何写硬件的能力都需要独立 ADR 与审批/租约链，不因 Agent 接入而放宽。

---

## 分期

> **已被取代**：本节 P0–P7 是旧 v5 长周期计划，已被 `43c58dc:docs/AGENT_IMPLEMENTATION_TASKS.md`
> 顶部的 2026-11-21 W4 supersession 块取代。P1 的 Job Object、P2 的 ListenerRegistry、
> P3–P6 的 CLI/A2A/沙箱均不在本轮；P7 的交付形态已按 ADR-3 修订改为内嵌（非旁挂）。

所有阶段的验收命令均为**计划验证，未运行**。P0/P2/P6 必须包含 `bash scripts/regression` 与 `cargo run -p duthub-desktop -- --self-test`。

| 阶段 | 周期 | 范围 | 验收（未运行） |
|---|---|---|---|
| P0 契约/StateDb/依赖/构建门 | 2 周 | ADR-0/2/5/6/7/8/9/10；core `StateDb`（rusqlite bundled，P0 选版本）；`x509-parser`；`peers.json` 类型；`bind_addr` 字段；`skill_export` HTTP 零 grant 临时拒绝；更新 `Cargo.lock` | `cargo metadata --no-deps` 11 成员；`cargo tree -p duthub-a2a -i libsqlite3-sys` 单 bundled；零 grant HTTP 读 SKILL.md 被拒；`bash scripts/regression`（4 步，含 `--self-test`）；`cargo build --release -p duthub-desktop`（`+crt-static`） |
| P1 共享 runtime overlay + 进程/ACP 核心 | 2 周 | `AgentAccessRuntime` 加 `runtime_by_fingerprint`/`runtime_listeners`/`set_remove`/`authenticate_internal`/`authorize` 合并/`lease_credential_enabled` 合并/`lease_credential_required`；租约统一 `device.lease_manager` 并补 `with_gui_session_lifetime`；`duthub-acp` 自研行 JSON-RPC、T35 方法面、T36 suspended spawn + Job Object + 5 秒关闭 + 失败三路径 | auth-off + overlay 三连测；`cargo test -p duthub-acp`；真实孙进程杀树；`cargo tree -p duthub-acp` 的 `duthub-desktop` 计数为 0 |
| P2 Host 接线 + Listener 生命周期 + DUT 绑定 + 审计 + demo 迁移 | 2 周 | desktop `agent_host`（HostManager/SessionRegistry/ListenerRegistry/TokenIssuer）；每会话 `session-<uuid>` runtime listener（`listener_dut=Some`）；`ListenerHandle` shutdown+join；`skill_export` 加 `authorize_any_read`；`tool.call` + `serial_read` 元数据；demo 迁移；`--mcp-stdio/--bridge` 复用 duthub-mcp 的 `discover_data_dir`/`bridge_stdio` 并在 tracing 前早退 | 13 个 Plan 工具跨 DUT 回归 + token 被盗 canary；吊销后 401 且线程/端口回基线；stdio e2e stdout 纯 JSON-RPC；Origin/Host 实测不 403；`bash scripts/regression` + `--self-test` 无副作用 |
| P3 CLI 适配/认证/持久配置 | 2 周 | `claude-code`（`--verbose`/`--mcp-config`/`--strict-mcp-config`/`--permission-mode plan`）与 `codex`（`mcp_servers`/`bearer_token_env_var`/`CODEX_HOME`）profile + fixture + 锁定版本 resume 复核；`auth_mode inherit/api_key`；持久化 `config_dir`/`cwd`/`mcp_config_path`；OAuth 与 API key 各一次；npm `.cmd` 校验包版本/integrity；goose/dsh 默认 `mcp_only`/experimental | fixture；真实锁定版本列 13 个 Plan 工具；duthub 重启后 resume；不读用户 `.mcp.json`/`.codex/config.toml`；restart resume 测试 |
| P4 Journal/证据 pin/幂等/恢复 | 2 周 | A2A 任务 journal（fsync）、checkpoint 20+每日、evidence copy+SHA-256（512 MiB/任务）、`step_key`/`tool_key`/`messageId`、`Interrupted`/`NeedsInspection`；Agent turn `prompt_id` 幂等；`sessions.json` 持久化 `config_dir`/`cwd`；transcript 8 MiB×10；load 50k/32 MiB；watchdog 含 MCP 活动 | 重启后 Working 恢复（Succeeded 取记录、Unknown 对账、evidence 缺失 Paused）；重复 `messageId`/`prompt_id` 不重放；压力脚本测量 |
| P5 信任/沙箱/OS 隔离 | 2 周 | `trust_level`、受限令牌（`WRITE_RESTRICTED`）、专用账户/ACL 指引、UI 双层徽标、写模式开关；canary 证明同用户可读 `settings.json` 通用 token；未上 OS 隔离时明确「协议级绑定不是安全边界」 | 受限令牌根外写失败；UI 文案与矩阵一致；goose/dsh 未被验证前不显示 Plan 只读 |
| P6 A2A 入站 | 4 周 | `peers.json`（单一来源）、Card 1.0.1（`supportedInterfaces`/`protocolBinding`/`securityRequirements`、`-32009`、`Vary`）、3 方法 + 8 工具 + 结构化 skill、StateDb TaskStore（Working 恢复/Completed 30 天/`messageId` 去重）、per-DUT listener（`bind_addr` Option、拒绝 `0.0.0.0`）、升级迁移（`NeedsBindAddr`/`PortConflict`）、per-DUT 状态、TLS（`rustls-webpki` 链 + `x509-parser` SAN/到期 + PEM 导入 + ACL）、Bearer/限流、启动顺序、`a2a_running`（`workspace.rs:940`/`pages.rs:3621`）、https Card、防火墙/审计 | 同网段 peer 只读任务；401/403/429；跨 DUT 拒绝；Card schema/版本错误码/Vary；重启后 Working 恢复；duthub 关闭连接拒绝；interop fixture 前 experimental |
| P7 内嵌与发布（本轮已实施，发布门禁 conditional） | 1 周 | 按 ADR-3 修订把 goose 解压后的 exe + LICENSE/NOTICE/manifest 内嵌进 `duthub.exe`，首次解包到工作根并校验 SHA-256；release 旁附 `dist/licenses/goose/`；SBOM / 签名链 / 应用内许可页 deferred | 离线可启动；hash 与 manifest 一致；`--version` 由 bundle 测试覆盖；`duthub.exe` 单文件分发不受影响；deferred 项未完成前不对外发布 |

**阶段依赖**：P0 → P1 → P2 串行；P3 可与 P2 部分并行，但依赖 P2 的 listener/token；P4 依赖 P2/P6 的任务模型；P5 依赖 P2 的进程与 UI；P6 依赖 P0 的 StateDb、P1 的 listener 生命周期、P4 的 journal；P7 只在 ADR-3 签字后启动。

**估算假设**：P0–P7 约 17 周按「2 名 Rust 工程师 + 1 名产品/测试」估算，允许 P0/P3/P6 部分并行；外部依赖为产品签字、真实 A2A peer、goose 锁定版本；这是规划估算，不是交付承诺。1 人执行约翻倍。P0 起必须处理 `skill_export` 的 HTTP 零 grant 临时拒绝；GUI 租约修复作为 P1 伴随 PR，两项都不等到 P2/P4。

---

## 风险

### 11.1 设计修订（终审 mustFix 的主题归并）

> 评审记录来自本轮输入的两份独立评审（bookReview/taskReview）与最终方案；F1-F17、R2-1..R2-15 的编号沿用评审原文。mustFix 共 17 项（F1-F17）与 15 项（R2-1..R2-15）两组；下表按 12 个设计主题归并，逐项缺口与状态见 §11.3。二者不是一一对应；StateDb（F1）、HostManager 边界（F3）等条目在 §11.3 单列，不因表中未单列而视为遗漏。

| # | 修订 | 落点 | 残余风险 | 状态 |
|---|---|---|---|---|
| R1 | 共享 `AgentAccessRuntime` + runtime overlay，替代每会话独立 runtime | §4.3 | `set_agent_access` 是 `OnceLock`；若 Host/测试另建 runtime，`holder_enabled` 仍会误判 | 设计裁决；P1 验证 |
| R2 | `ListenerHandle` + graceful shutdown + join；A2A 每任务改用进程内 duplex | §4.4 | 现有 API 无 shutdown，改造要同时改 `apps/desktop/src/mcp_runtime.rs` 的 spawn 循环 | 设计裁决；P1/P2 验证 |
| R3 | token 不静默迁移；改为「文案 + canary + ADR-10」 | §10.2 | 同用户 Agent 可读 `settings.json` 通用 token；协议级绑定不是安全边界 | 设计裁决；P5 验证 |
| R4 | journal/checkpoint/evidence pin/幂等/中断态 | §7.3-§7.5 | `serial_read` 需 open session、`analyze_serial` 取内存尾部、日志 2 MB×20 轮转；evidence 只能覆盖已取区间；不写「确定性重跑」 | 设计裁决；P4 验证 |
| R5 | CLI resume 与持久 `config_dir` | §7.2、§9.3 | Claude/Codex 凭据文件路径与 codex `config.toml` 复制规则未核实；复制失败必须要求 API key | 设计裁决；P3 验证 |
| R6 | A2A 单一配置源 `peers.json`；`settings.a2a` 只作默认/兼容 | §4.6、§8.3 | 旧 `a2a_panel`/`save_a2a_settings` 仍保存 `settings.a2a`，需 UI 标注「仅默认值」；迁移失败 fail closed | 设计裁决；P6 验证 |
| R7 | `a2a_running` 改 per-DUT 状态 + `bind_addr` 升级 | §8.4 | 旧 `duts.json` 解析兼容；`dut_a2a_bind_addr_roundtrip` 必须证明默认省略不破坏 `docs/examples` 往返 | 设计裁决；P6 验证 |
| R8 | TLS 链用 `rustls-webpki`，`x509-parser` 只做 SAN/到期；mTLS/DPAPI 移出 v1 | §8.5 | `x509-parser` 不是链验证器；私钥 ACL 的 `icacls` 实现与轮换流程要在 P6 实测 | 设计裁决；P6 验证 |
| R9 | `--mcp-stdio/--bridge` 复用 duthub-mcp 的 `discover_data_dir`/`bridge_stdio` | §4.8 | `duthub.exe` 是 `windows_subsystem=windows`，仍需验证子进程管道下 stdout 可用；不初始化 fmt layer 要覆盖 `--trace` 与 `--self-test` 的 `println!` | 设计裁决；P2 验证 |
| R10 | P0/P2 验收加 `bash scripts/regression` 与 `--self-test` | §5.6 与「分期」 | `Workbench::new` 启动 A2A/Host 会与 self-test 冲突，必须把 `self_test` 标志传进 `StartupContext` | 设计裁决；P0/P2 验证 |
| R11 | 补 2026-07-28 `server/discover` 测试与 Origin/Host 实测 | §4.2、§10.4 | 当前 `transport.rs:152` enforce 且 allowlist 空，带 Origin 客户端会 403；修复前 P2 不能用真实浏览器型客户端验收 | 设计裁决；P1/P2 验证 |
| R12 | 13 个 Plan 工具跨 DUT 回归 + token 被盗 canary；`skill_export` 加授权 | §10.1、§10.3 | `skill_export` 加授权会改变「零 grant 的 HTTP 客户端也能读 SKILL.md」的既有行为；需更新 API_CONTRACT/SECURITY 并加回归；若产品不接受，保留 `LocalAdmin`-only 并记录例外 | 设计裁决；P2 验证 |

### 11.2 明确不采纳与弱化处理

| 项 | 裁决 | 理由 | 残余风险 |
|---|---|---|---|
| token 强制移出 `settings.json`（keyring/DPAPI） | **v5 不实施**；只用「改文案 + canary + ADR-10」 | 明文 token 是用户既有要求，且既有 e2e/往返测试依赖；静默迁移会破坏契约与可见性 | 同用户 Agent 可读通用 token；要真正隔离需专用账户/AppContainer 或把外部 Agent 标 experimental |
| A2A 24/7/headless | **不采纳**；A2A 只在 duthub GUI 存活时可用 | v5 不建 `duthub-runtime`，不保留 headless/无人值守/计划任务；持久化 TaskStore 只解决状态丢失，不解决关机期间不可达 | 如果产品要求常驻，必须另立 ADR 加 headless，或把 A2A 标 experimental |
| 内部 26 工具写面 | **不采纳**；v5 只做 8 个证据工具的 A2A 读面 | 内部写面属于无人值守/内置任务计划；ADR-8 明确 supersede 旧 §4.5 | 实现者若按旧文档开工会做出无法验收的写链；P0 必须做 supersession 标注 |
| mTLS/DPAPI 进入 v1 | **移出 v1**；只保留 Bearer + TLS + PEM + ACL | 避免未核实 API 与额外依赖；mTLS/DPAPI 的 Windows 行为未实测 | 企业级 peer 身份认证要等后续评估；v1 的 `source_cidrs`/证书指纹只做辅助 |
| 「确定性重跑」话术 | **不采纳**；只写 journal 重放/Succeeded 取记录/Unknown 对账/NeedsInspection | 没有真实 fixture/证据 pin 前无法保证 | Working 恢复可能停在 NeedsInspection；A2A 在 fixture 前标 experimental |
| 每会话独立 `AgentAccessRuntime` | **不采纳**；改为共享 runtime + overlay | 每会话独立 runtime 与租约 `OnceLock` 分裂，临时凭据会在租约路径被误判为停用 | overlay 必须挂在同一个 Arc 上；Host/测试不得另建 runtime |
| 外部 Agent 的本地工具可直接操作硬件 | **不采纳「仅靠 mcp_only 就安全」**；默认按不可信处理 | Agent 有本机 shell 时可绕过 MCP 直接开 COM/继电器/烧录；`mcp_only` 只约束工具面 | 没有专用账户/ACL/受限令牌时外部 Agent 标 experimental；UI 明示；OS 隔离是唯一强边界 |

### 11.3 评审缺口登记与处置

| 编号 | 缺口 | 处置 | 状态 |
|---|---|---|---|
| F1 | A2A StateDb 没有落点，P6 可能直接构建失败 | StateDb 上移 `duthub-core`（§4.5）；P0 加 `rusqlite` bundled/`x509-parser` 与 `+crt-static` 构建门 | 设计裁决；P0 验证 |
| F2 | Card 只写 `supportedInterfaces`，缺 `securityRequirements`/`protocolBinding` 改名与 `-32009`/`Vary` | §8.2 列出全部字段；ADR-8 取代 T39 `preferred_transport`；加 schema 契约测试 | 设计裁决；P6 验证 |
| F3 | HostManager 与 `AlertCenter` crate 边界未闭合 | HostManager 放 `apps/desktop/src/agent_host/`；`duthub-acp` 只做协议/进程，不依赖 desktop；加编译边界检查 | 设计裁决；P1/P2 验证 |
| F4 | 每会话 runtime 是否独立未说清 | 采用共享 runtime + overlay；`authenticate_http` 只查持久，`authenticate_internal` 只查 overlay；主 listener 不被临时 token 打开 | 设计裁决；P1 验证 |
| F5 | A2A 关闭期间不可用，持久化不是可用性修复 | §8.1 明确 GUI 绑定；ADR-0 产品签字；否则 experimental | 设计裁决；需产品签字 |
| F6 | `tool.call` 只记长度/条数，Inspector 没有证据 seq | §10.2 增记 `last_seq`/`session_id`/`line_count`/segment/offset/count（不记正文） | 设计裁决；P2 验证 |
| F7 | env 最小集合会切断 goose/Node/CLI 配置与认证 | §5.2 列标准 env 白名单（含 `APPDATA`/`LOCALAPPDATA`/`USERPROFILE`/`TEMP`/`HOME`/`COMSPEC`/`PATHEXT` 等）；不注入 duthub 密钥 | 设计裁决；P2 验证 |
| F8 | CLI 隔离 config dir 会丢用户登录/模型认证 | §9.3 两种模式（复制/只读挂载用户认证，或显式 API key env）；P3 让 OAuth 与 API key 用户各跑一次；npm `.cmd` 校验包版本/integrity | 设计裁决；P3 验证 |
| F9 | goose/dsh 只读门没有关闭/验证机制 | goose 默认 `mcp_only`，不写「Plan 只读」；`verified_readonly` 需文档化配置 + 探针；dsh 读配置/探针，未验证前不授予 | 设计裁决；P3/P5 验证 |
| F10 | sandbox cwd 不是路径边界，默认无 OS 隔离 | §6.2 明确「协议级绑定不是安全边界」；受限令牌只挡写；真正隔离需专用账户/ACL/AppContainer 或标 experimental | 设计裁决；P5 验证 |
| F11 | `settings.a2a`/`A2aConfig.peers` 与 `peers.json` 双源 | §4.6/§8.3：入站只认 `peers.json`；`settings.a2a` 只作默认/兼容；`A2aConfig.peers` 保留为出站，UI 标注；`host` 废弃 | 设计裁决；P6 验证 |
| F12 | `DutA2aConfig.bind_addr` 没有序列化/迁移方案 | `Option<String>` + `serde(default, skip_serializing_if)`；默认 `None`；启动校验；加 `duts_example_roundtrip` 用例 | 设计裁决；P0/P6 验证 |
| F13 | TLS/SAN 依赖缺失；`x509-parser` 不在 lock | P0 加 `x509-parser`；链验证用 `rustls-webpki`；P0 更新 lock 并跑构建门 | 设计裁决；P0 验证 |
| F14 | `dutKey`/TLS 私钥存储未闭合 | `dutKey` 用 per-install salt 的 HMAC；salt 存 keyring/受 ACL 保护的文件，轮换需审计；TLS 私钥 PEM + `icacls`，轮换 = 重新导入 + 重启 listener；Card 去敏测试 | 设计裁决；P6 验证 |
| F15 | `--mcp-stdio/--bridge` stdout 纯净性未验证 | §4.8 早退、不初始化 stdout fmt layer；e2e 断言纯 JSON-RPC；`--trace`/`--self-test` 分支单独覆盖 | 设计裁决；P2 验证 |
| F16 | `a2a_running`/页面行号已漂移 | 本轮已核实为 `workspace.rs:631`/`:940`、`pages.rs:3621`；P6 改 UI 前重新 `grep`，不按旧行号找代码 | 设计裁决（基线本轮已核对）；P6 验证 |
| F17 | A2A `Working` 恢复与「无 headless」冲突、启动顺序未定义 | §8.8：DB→listener→Working 恢复→重签 token；未启用不启动；GUI 关闭期间不可用；ADR-0 签字 | 设计裁决；P6 验证 |
| R2-1 | 租约 `OnceLock` 与每会话 runtime 分裂 | 共享 runtime + overlay；租约唯一 `device.lease_manager()`；补 `with_gui_session_lifetime` | 设计裁决；P1 验证 |
| R2-2 | per-session listener 无法关闭 | `ListenerHandle` + graceful shutdown + join；会话注册表；测试线程/端口回基线 | 设计裁决；P1/P2 验证 |
| R2-3 | `settings.json` 明文 token + 通用 listener 全局读 | 承认「协议级绑定不是安全边界」；canary；ADR-10；UI 明示 | 设计裁决；P5 验证 |
| R2-4 | A2A Working 恢复不可确定性重跑 | journal/checkpoint/evidence pin；Unknown 对账；NeedsInspection；不写「确定性重跑」 | 设计裁决；P4 验证 |
| R2-5 | CLI resume 与隔离 config dir 矛盾 | 持久化 `config_dir`/`cwd`；复制/只读挂载认证；重启 resume 测试 | 设计裁决；P3 验证 |
| R2-6 | `x509-parser` 不是链验证器 | 链验证用 `rustls-webpki`；`x509-parser` 只做 SAN/notAfter/SPKI；P6 实测 | 设计裁决；P6 验证 |
| R2-7 | `--mcp-stdio/--bridge` 重复实现 | 抽 duthub-mcp 的 `bin_support`，两个入口共用；e2e 钉 stdout | 设计裁决；P2 验证 |
| R2-8 | P0 漏 `scripts/regression`/`--self-test` | 「分期」与 P0/P2 验收显式加入；self-test 跳过 Host/A2A、临时 StateDb、不绑端口 | 设计裁决；P0/P2 验证 |
| R2-9 | 无 `server/discover` 项目级测试 | P1 同时测 2026-07-28 `server/discover` 与 legacy `initialize`；rmcp 3.10 已有 `DiscoverRequestMethod` | 设计裁决；P1 验证 |
| R2-10 | Origin/Host 可能 403 | §10.4 实测；显式 `allowed_origins` 或仅 loopback 内部 listener 关闭空 allowlist；不盲目 disable | 设计裁决；P2 验证 |
| R2-11 | DUT 绑定回归只覆盖少数工具 | §10.1 覆盖全部 13 个 Plan 工具 + token 被盗 canary | 设计裁决；P2 验证 |
| R2-12 | token 吊销时已打开 SSE 流未覆盖 | `ListenerHandle` shutdown + join；测试吊销后 401 且线程/端口回基线；长连接场景补 e2e | 设计裁决；P2 验证 |
| R2-13 | A2A 升级迁移（`bind_addr=None`、重复 8790、旧 token） | §8.4/§8.3 的 `NeedsBindAddr`/`PortConflict`/`local-operator` 迁移；失败 fail closed | 设计裁决；P6 验证 |
| R2-14 | Agent 更新/哈希/npm shim/Authenticode 流程未定义 | §9.5：manifest 锁版本 + SHA-256；只 `--version` 探测；npm shim 校验包版本/integrity 或原生 exe；显示 Authenticode | 设计裁决；P3 验证 |
| R2-15 | A2A `ListTasks`/`GetExtendedAgentCard`/push 未定义 | §8.2/§8.7：返回 `-32601`，Card capabilities 不承诺 | 设计裁决；P6 验证 |

### 11.4 未运行与未核实（设计调研时点；W4 已部分覆盖）

- **设计调研时点未运行**：`cargo build`、`cargo test`、`cargo clippy`、`cargo fmt --check`、`bash scripts/regression`、`cargo run -p duthub-desktop -- --self-test`、release 构建、GUI、真机、真实 goose/ACP/A2A peer。**2026-11-21 W4 已实测** clippy/test/self-test/regression/fmt 全部 EXIT=0，并由测试启动真实 goose-acp（假模型服务）；详见 `docs/reviews/AGENT_IMPLEMENTATION_REPORT.md`。
- **仍未复核**：Defender/EDR 行为、真实在线模型、真实 CLI 的运行行为、goose 是否消费 `mcpServers`（W2 测试已覆盖假 MCP 注入）、goose 关闭 developer/本地工具扩展的机制（W2 managed config 已覆盖）、dsh read-only 的验证方式、Windows 计划任务/Session 0 下的串口/ADB/继电器/DirectShow 行为。
- **静态阅读不是验收**：本文的 `path:line` 只证明现状，不证明目标已实现；任何「设计裁决」都要在对应阶段用测试/命令闭环。

### 11.5 行号漂移与计划冲突

- **行号漂移**：`a2a_running` 当前实际位置是 `workspace.rs:631`/`:940` 与 `pages.rs:3621`；上一轮的 938/3283 已失效。`apps/desktop/src/workspace.rs`、`pages.rs` 正在并行修改，P6 改 UI 前必须重新 `grep`。
- **计划冲突**：v5 与 T02 的 12 成员、T39 的 `url`/`preferred_transport`、旧 best-practice §4.5 的内部 26 工具、旧 W6/W7 的无人值守存在差异；ADR-0/ADR-8/ADR-9 未签字前，实现者可能按旧文档开工。P0 验收必须包含 `AGENT_IMPLEMENTATION_TASKS`、`AGENT_INTEGRATION_BEST_PRACTICE`、`API_CONTRACT`、`SECURITY`、`ARCHITECTURE` 的 supersession 标注。
- **成员数冲突**：旧 T02 写 12 成员；本方案按 11 成员（现有 9 + `duthub-acp` + `duthub-a2a`）。ADR-0 必须明确，不能两套数字并行。

---

## 事实核实记录

### 12.1 成立（可作为事实陈述）

> 说明：本节「成立」指**调研方**（外部研究轨道）一手核对/实测；「本轮」只做文档修订与仓库静态核对，未复跑外部 Agent。Task Scheduler、Session 0、无人值守相关条目只作背景，v5 不采用其形态（见 §11.2）。外部断言给 URL；仓库内断言给 `path:line`。

| # | 断言（摘要） | 结论 | 来源 |
|---|---|---|---|
| 1 | crates.io 的 `agent-client-protocol` 最新版为 5.4.0（2026-11-18，Apache-2.0，MSRV 1.88.0），2025-07-24 建包、91 个版本；2026-06-24 起连发 1.0/2.0/3.0/5.0 四个 major（无 4.x）；5.0.0→5.4.0 仅 16 天；Zed 以 `=5.4.0` 精确锁定 | 成立 | https://crates.io/api/v1/crates/agent-client-protocol |
| 2 | Rust SDK 客户端覆盖 initialize、session/new、session/prompt、session/update、session/request_permission，并提供 `load_session`/`resume_session`；恢复取决于 `loadSession`/`sessionCapabilities.resume`，Zed 对不支持的 Agent 报错 | 成立 | https://github.com/agentclientprotocol/rust-sdk/blob/main/src/agent-client-protocol/src/session.rs |
| 3 | ACP 默认传输是 stdio 上的按行 JSON-RPC；协议方法表不含进程生命周期；Zed 在 Windows 用 Job Object（`KILL_ON_JOB_CLOSE` + `TerminateJobObject`）杀整棵进程树，且自述 spawn→assign 之间子进程可能逃逸 | 成立 | https://github.com/zed-industries/zed/blob/main/crates/util/src/process.rs；https://github.com/agentclientprotocol/rust-sdk/blob/main/md/transport-architecture.md |
| 4 | crates.io 的 `goose` 0.19.2 是 tag1consulting 的压测工具；可嵌入 agent loop 以 `oi-kit-agent` 0.1.0-alpha.15（2026-11-19，Apache-2.0）发布，此前叫 `goose-agent`（最后 0.1.0-alpha.14，2026-11-17）；README 写「oi-kit is in alpha」 | 成立 | https://crates.io/crates/oi-kit-agent |
| 5 | goose 自带 ACP server：`goose acp` 走 stdio，`goose serve` 提供 HTTP/WebSocket；服务端实现 `session/load`；Zed 的 ACP Registry 以 goose 为 ACP agent；goose 自家 Electron 桌面端也通过 ACP 驱动内核 | 成立 | https://github.com/aaif-goose/goose/blob/main/crates/goose/src/acp/server/load_session.rs#L367 |
| 6 | goose 没有原生 A2A：全仓库对 a2a/agentcard/agent2agent 无实现命中；`goose-roaming` 是基于 iroh(QUIC) 的 ACP 点对点传输，README 明说「只做 ACP 传输，不添加任何会话语义」 | 成立 | https://github.com/aaif-goose/goose/blob/main/crates/goose-roaming/README.md |
| 7 | openai/codex 的 Rust 内核不能作为常规 crates.io 依赖：`codex-core` 返回 404；同名 `codex-protocol` 0.63.0 是第三方 `namastexlabs` 的 fork；核心 crate 只在 workspace 内以 path 依赖存在，`codex-core` 列出 90 个 `codex-*` 依赖（含 dev），`codex-windows-sandbox` 有 3 个 helper bin | 成立 | https://crates.io/api/v1/crates/codex-core |
| 8 | codex 的长时任务三件套：会话按 JSONL rollout 落盘（`recorder.rs:77`），`RolloutRecorder::resume` 恢复，索引走 SQLite state DB，上下文默认自动压缩、可用 `AutoCompactionDisabled` 关闭 | 成立 | https://github.com/openai/codex/blob/main/codex-rs/rollout/src/recorder.rs |
| 9 | Temporal 把每个 Activity 结果、Timer 与消息写入 Event History；Worker 崩溃后另一个 Worker 重放历史重建状态（含局部变量），已完成步骤直接取记录结果不再执行 | 成立 | https://docs.temporal.io/encyclopedia/durable-execution |
| 10 | Temporal 的 Continue-As-New 用同一 Workflow Id、新 Run Id 开启新 Event History，并把最新状态作为参数传给新执行；文档称这样「Workflow 可以永远运行」 | 成立 | https://docs.temporal.io/workflow-execution/continue-as-new |
| 11 | A2A 规范把 `TASK_STATE_INPUT_REQUIRED` 与 `TASK_STATE_AUTH_REQUIRED` 定义为非终态中断态（客户端可用同一 taskId 继续）；长任务靠 GetTask/CancelTask、SubscribeToTask 流与推送通知；v1.0 JSON-RPC 方法为 PascalCase 并以 `A2A-Version` 头协商（缺省按 0.3） | 成立 | https://a2a-protocol.org/latest/specification/ |
| 12 | MCP 2026-07-28（上一版 2025-11-25）把协议改为无状态：删除 initialize/notifications/initialized 握手、`Mcp-Session-Id` 与 SSE 断点续传；协议版本与客户端能力随每请求 `_meta` 传递；服务端必须实现 `server/discover`；服务端发起的 elicitation 等改为 MRTR | 成立 | https://modelcontextprotocol.io/specification/2026-07-28/changelog |
| 13 | MCP 长任务能力移入官方扩展 `io.modelcontextprotocol/tasks`：`CreateTaskResult`（taskId、ttlMs、pollIntervalMs）、`tasks/get`、`tasks/update`，状态含 working/input_required/completed/failed/cancelled，并建议客户端持久化 taskId 以便崩溃/重启后续 poll | 成立 | https://modelcontextprotocol.io/extensions/tasks/overview |
| 14 | ACP 在 `session/new` 中把 MCP server 连接信息作为会话参数：Client MAY 提供 `mcpServers`，Agent SHOULD 连接 Client 指定的全部 server；ACP 文档明确 MCP 用于 Agent 访问外部工具和数据源 | 成立 | https://agentclientprotocol.com/protocol/session-setup |
| 15 | 截至 2026-11-20，A2A 规范最新发布版为 v1.0.1（2026-05-28），主仓 `a2aproject/A2A`（Apache-2.0），文档站 `a2a-protocol.org`；1.1 只有 2026-10-29 的 `v1.1.0-rc` tag，GitHub 上没有对应 release | 成立 | https://github.com/a2aproject/A2A/releases |
| 16 | A2A 长任务：SendMessage 默认阻塞到终态（COMPLETED/FAILED/CANCELED/REJECTED）或中断态（INPUT_REQUIRED/AUTH_REQUIRED）；`return_immediately=true` 创建后立即返回；客户端用 GetTask 轮询、SubscribeToTask 流或 push webhook 取更新；任务生命周期独立于单条流 | 成立 | https://github.com/a2aproject/A2A/blob/v1.0.1/specification/a2a.proto#L155-L208 |
| 17 | A2A 1.0 对 server 的硬要求：生产部署 MUST 使用加密传输（HTTP 绑定 HTTPS、gRPC TLS）；Agent Card 从 `https://{server_domain}/.well-known/agent-card.json` 发现且 server MUST 提供；认证方案由 Card 的 `securitySchemes` 声明，server MUST 逐请求认证；规范不强制某一种方案 | 成立 | https://a2a-protocol.org/latest/specification/#71-protocol-security |
| 18 | A2A push notification 是 server 向 client webhook 发 HTTP POST；§13.2 要求 agent SHOULD 校验 webhook URL 并拒绝 127.0.0.0/8、10.0.0.0/8、172.16.0.0/12、192.168.0.0/16、localhost 与 link-local 目标以防 SSRF；client 可用 Subscribe/GetTask 轮询作为替代 | 成立 | https://a2a-protocol.org/latest/specification/#132-push-notification-security |
| 19 | `a2a-server-lf` 0.5.3 默认 fail-open：`DefaultRequestHandler::new` 的 authorizer 为 `None`，`authorize()` 在 `None` 时返回 `Ok(())`；唯一授权钩子 `RequestAuthorizer` 只收到传输层元数据与 `task_id`，拿不到 TCP 源地址或 TLS 客户端证书 | 成立 | https://github.com/a2aproject/a2a-rs/blob/a2a-server-lf-v0.5.3/a2a-server/src/handler.rs#L485-L583 |
| 20 | Windows Task Scheduler 默认在触发器启动任务 72 小时后终止它（按需启动绕过；PT0S 才无限运行）；`StartWhenAvailable` 默认 false（仅定时任务）、`RestartOnFailure` 未配置、`WakeToRun` 默认 false、`MultipleInstancesPolicy` 默认 `IgnoreNew` | 成立 | https://learn.microsoft.com/en-us/windows/win32/api/taskschd/nf-taskschd-itasettings-get_executiontimelimit；https://learn.microsoft.com/en-us/windows/win32/taskschd/taskschedulerschema-settingstype-complextype |
| 21 | DPAPI 用户范围通常只有同一登录凭据可解密，`CRYPTPROTECT_LOCAL_MACHINE` 则本机任意用户可解密；Credential Manager 凭据绑定当前 token 的登录会话（网络登录会话没有凭据集）；keyring 3.6.3 的 windows-native 后端使用 Windows Credential Manager，项目锁 3.6.3（2025-07-27），最新 4.2.0（2026-08-29，MIT OR Apache-2.0） | 成立 | https://learn.microsoft.com/en-us/windows/win32/api/dpapi/nf-dpapi-cryptoprotectdata；https://learn.microsoft.com/en-us/windows/win32/api/wincred/nf-wincred-credwritew；https://crates.io/api/v1/crates/keyring |
| 22 | 美国 29 CFR 1910.147(d)(4)-(6) 要求：每个能量隔离装置上装设锁定或挂牌装置（锁定装置须使隔离装置保持在「safe」或「off」位），释放残余能量，并在开始作业前由授权人员核实隔离与断电已完成 | 成立 | https://www.govinfo.gov/content/pkg/CFR-2023-title29-vol5/xml/CFR-2023-title29-vol5-sec1910-147.xml |
| 23 | labgrid 的 place 锁是排他租约：锁只能由一个人取得；reservation 未续期或未被锁使用时会在短时间后超时 | 成立 | https://labgrid.readthedocs.io/en/v26.0.1/usage.html |
| 24 | 欧盟机械指令 2006/42/EC 附录 I 1.2.4.3：急停指令必须由急停装置保持到被明确解除；解除急停不得使机器重新启动，只能允许重新启动；急停装置是其他防护措施的后备而非替代 | 成立 | https://kompendium.bg-verkehr.de/bgverkehr/docs/bgverkehr_eu/bgverkehr_eu-Documents/e2006-42e/e2006-42e_53_.html；https://publications.europa.eu/resource/celex/32006L0042 |
| 25 | Copilot CLI 的 npm 元数据 license 为「SEE LICENSE IN LICENSE.md」；`LICENSE.md` 只授予把「未修改副本」随功能更丰富的应用再分发的权利（须附许可证、保留声明、不得独立分发），并明确禁止修改/翻译/衍生作品与商标使用 | 成立 | https://github.com/github/copilot-cli/blob/main/LICENSE.md |
| 26 | Copilot CLI 自带 ACP server：v0.0.397（2026-01-28）起提供 `copilot --acp`，默认 stdio、`--port` 走 TCP/NDJSON，官方标注 ACP 为 public preview；ACP Registry 条目 `github-copilot-cli` 以 `npx @github/copilot@1.0.114 --acp` 分发，license 标 `proprietary` | 成立 | https://docs.github.com/en/copilot/reference/copilot-cli-reference/acp-server |
| 27 | Copilot CLI 运行需有效 Copilot 订阅并 GitHub 登录（`/login` 或带 Copilot Requests 权限的 fine-grained PAT，Classic `ghp_` 不支持）；每条提示扣减一次 premium request；Windows 由 `@github/copilot-win32-x64` 提供原生 `copilot.exe`（196,113,184 B），README 要求 PowerShell 6+ | 成立 | https://github.com/github/copilot-cli；https://registry.npmjs.org/@github/copilot-win32-x64/latest |
| 28 | Zed 自带 Agent 通过进程内 `NativeAgentServer` 接入：`connect` 接收 `&mut App` 与 `Entity<Project>`，在 Zed 进程内创建 `NativeAgent`，不是 spawn 子进程或 stdio ACP server | 成立 | https://github.com/zed-industries/zed/blob/main/crates/agent/src/native_agent_server.rs#L32-L52 |
| 29 | `crates/agent` 的许可证字段是 `GPL-3.0-or-later`；Zed README 写仓库以 GPL-3.0-or-later 为主、仅标注组件为 Apache-2.0 | 成立 | https://github.com/zed-industries/zed/blob/main/crates/agent/Cargo.toml#L6 |
| 30 | `agent` crate 通过 `publish.workspace = true` 继承 workspace 的 `publish = false`，并依赖 `gpui`、`project`、`client`、`db`、`language_model` 等编辑器内部 crate，不能作为普通 crates.io 依赖被第三方宿主直接引用 | 成立 | https://github.com/zed-industries/zed/blob/main/crates/agent/Cargo.toml#L5-L54 |
| 31 | goose v1.60.2（2026-11-20T06:18:55Z）为 Windows 单独发布 `goose-acp-x86_64-pc-windows-msvc.zip`，9,832,133 B；同版本 CLI zip 54,449,330 B、桌面 zip 237,837,415 B；只随包带 ACP 资产的体积约为桌面整包的 1/24 | 成立 | https://github.com/aaif-goose/goose/releases/tag/v1.60.2 |
| 32 | goose 的 ACP 入口是 `goose acp`（stdio，官方文档写「客户端管理 goose acp 进程的生命周期」）；配置在 `%APPDATA%\Block\goose\config\config.yaml`，密钥默认进系统 keyring，可用 `GOOSE_DISABLE_KEYRING=1` 关闭；文档警告无人值守时 keyring 读取可能无限阻塞 | 成立 | https://raw.githubusercontent.com/aaif-goose/goose/main/documentation/docs/guides/goose-cli-commands.md |
| 33 | GitHub Copilot CLI 使用非 OSI 自定义许可（SPDX 记为 NOASSERTION）：允许把未修改副本「作为应用或服务的一部分」再分发，但要求宿主提供超出该软件本身的功能、不得单独分发或作为主产品、必须附许可全文，并明确禁止修改、改编与衍生作品 | 成立 | https://raw.githubusercontent.com/github/copilot-cli/main/LICENSE.md |
| 34 | ACP 的 `session/new` 要求 cwd 为绝对路径并纳入会话根集，但根集只是文件工具边界的 SHOULD；规范明说 shell 只有在沙箱执行时才受根集约束，cwd 之外的访问走 permission request——cwd 是协议约定，不是 OS 强制 | 成立 | https://github.com/agentclientprotocol/agent-client-protocol/blob/schema-v1.33.0/docs/protocol/v1/session-setup.mdx#L438-L453 |
| 35 | goose v1.60.2 的 `GooseMode` 只有 auto/approve/smart_approve 三个变体，`Auto` 标注为 `#[default]`；旧 `chat` 名已退役、仅解析为 approve——权限模式里没有只读/plan 选项 | 成立 | https://github.com/aaif-goose/goose/blob/v1.60.2/crates/goose-provider-types/src/goose_mode.rs#L22-L49 |
| 36 | Windows 受限令牌在每次访问可保护对象时做两次检查（令牌 SID 与 restricting SIDs），两者都允许才放行；用调用者自身令牌的受限版本调 `CreateProcessAsUser` 不需要 `SE_ASSIGNPRIMARYTOKEN` 特权，文档称这使普通应用可以创建受限进程 | 成立 | https://learn.microsoft.com/en-us/windows/win32/secauthz/restricted-tokens |
| 37 | Claude Code 的 headless 契约是 `claude -p` 子进程：`--output-format` 支持 text（默认）/json/stream-json，`stream-json` 在 stdout 输出逐行 JSON 事件，`--input-format stream-json` 支持流式输入，会话用 `--resume <session_id>` 或 `--continue` 恢复；npm 最新 2.1.329（2026-11-19），许可为商业条款 | 成立 | https://code.claude.com/docs/en/headless.md |
| 38 | Claude Code ≥2.1.302 的 `claude -p` 在未设置权限模式时以 Manual 启动，需要批准的调用会被直接拒绝（除非 allow 规则、hook 或 `--permission-prompt-tool`）；`plan` 模式允许读取，但在计划被批准前阻止编辑 | 成立 | https://code.claude.com/docs/en/permission-modes.md |
| 39 | DeepSeek Harness 官方 Windows x64 安装包存在：win-x64 更新清单指向 `deepseek-harness-0.2.2-rc.5-win-x64.exe`（299,593,552 B，releaseDate 2026-11-18T11:35:49Z），HEAD 返回 200 且 Content-Length 与清单一致 | 成立 | https://download.deepseek.com/dsh-desk/feeds/win-x64/nightly.yml |
| 40 | dsh 官方 CLI 定义三类接口：`dsh --profile acp` 走 ACP stdio、`dsh --profile headless --json` 输出 newline-delimited run events、`dsh --profile sdk` 走 stdio JSON-RPC；本机 Windows（Node 24.14.1）跑通 `--version`、ACP `initialize` 与 `session/new` | 成立 | https://github.com/deepseek-ai/deepseek-harness/blob/master/docs/cli-help.md |
| 41 | dsh 的 ACP 面是标准 v1 的最小自动化面：stdout 只走协议流量，`session/new` 要求绝对 cwd（单一主工作区），可挂 stdio/HTTP MCP server（客户端角色），但 modes、commands、plans 被明确省略/拒绝，`authMethods` 为空 | 成立（调研方实测 initialize/session-new；未跑真实 prompt；本轮未复跑） | https://github.com/deepseek-ai/deepseek-harness/blob/master/packages/acp/acp/README.md |
| 42 | 仓库 `Cargo.lock` 已有 `rustls-webpki` 0.103.17，可作为 A2A 证书链验证依赖 | 成立 | 本地 `Cargo.lock:6152-6153` |

### 12.2 被推翻或需修正

> 来源约定：外部断言给 URL；仓库内断言给 `path:line`（本轮已读或素材已核对）。

| # | 断言 | 结论 | 冲突/修正 | 来源 |
|---|---|---|---|---|
| 1 | goose GDK 仍是 0.1.0-alpha.14（2026-11-17），agent loop 不带持久化 | **被推翻（部分）** | `oi-kit-agent` 已到 0.1.0-alpha.15（2026-11-19）；旧名 `goose-agent` 止于 alpha.14。README 的「宿主实现 SessionLoader/EffectHandler、每步重载会话」仍成立 | https://github.com/aaif-goose/goose/blob/main/crates/oi-kit-agent/README.md；https://crates.io/crates/oi-kit-agent |
| 2 | 官方 Rust A2A SDK 仓库 open issues 已达 70 个 | **被推翻** | GitHub Search API 返回 `total_count: 49`（另有 31 个 open PR），不是 70 | https://docs.rs/a2a-server-lf/latest/a2a_server/；https://api.github.com/search/issues?q=repo%3Aa2aproject%2Fa2a-rs+type%3Aissue+state%3Aopen |
| 3 | gpui-pre 0.3.12 的 headless/windowing 切换只对 Linux/macOS 生效（Windows 无效果） | **被推翻（部分）** | `app.rs:298-301` 注释确写「Has no effect on other platforms」，但同版本 Windows 后端实现 `WindowingRequest::Headless => detach_gpu()`；Windows 可 headless 运行，但不能开窗。Session 0 无视频驱动仍成立 | https://docs.rs/crate/gpui-pre/0.3.12/source/src/app.rs；https://docs.rs/crate/gpui-pre-windows/0.3.12/source/src/platform.rs |
| 4 | Zed 生态接 Claude Code/Codex 仍各需一个 Node 适配器，`@zed-industries/codex-acp` 0.16.0 是最后一个 | **被推翻（部分）** | `@zed-industries/codex-acp` 0.16.0 已被 `@agentclientprotocol/codex-acp` 取代；Claude 适配器仍存在。其余关于 ACP SDK 5.4.0/stdio/JSON-RPC 的描述成立 | https://docs.rs/agent-client-protocol/latest/agent_client_protocol/struct.AcpAgent.html |
| 5 | 旧 `43c58dc:docs/GOOSE_AGENT_FEASIBILITY.md:201` 的「仅绑 127.0.0.1（`assert_loopback`）」是必须保真契约 | **被推翻** | 当前代码用 `assert_bind_allowed`（`transport.rs:213-221`），`lanAccess` 已是正式配置，仓库不存在 `assert_loopback`；由 ADR-2 取代 | 本地 `43c58dc:docs/GOOSE_AGENT_FEASIBILITY.md:201`、`transport.rs:213-221`；本轮未重新跑 `grep assert_loopback` |

### 12.3 未核实（引用时必须保留标注）

> 来源约定：外部断言给 URL；仓库内断言给 `path:line`（本轮已读或素材已核对）。

| # | 断言 | 结论 | 不确定点/来源 |
|---|---|---|---|
| 1 | Zed 在 `settings.json` 的 `agent_servers.<id>` 下声明外部 Agent，多 Agent 按 thread 绑定 | 未核实 | 未在本轮复读 Zed 配置文档；来源 https://github.com/zed-industries/zed/blob/main/docs/src/ai/external-agents.md |
| 2 | Zed 编辑器本体 GPL-3.0-or-later、agent-client-protocol Apache-2.0，两者许可证不同 | 未核实 | 未逐文件核对 Zed 全仓双许可；来源 https://github.com/zed-industries/zed/blob/main/README.md |
| 3 | 官方 ACP Registry（v1.0.0）收录 46 个 Agent，含 Claude/Codex/Gemini/goose/OpenCode 等 | 未核实 | registry 数量与条目未本轮复核；来源 https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json |
| 4 | block/goose 已 301 迁移到 aaif-goose/goose，归属 Agentic AI Foundation / Linux Foundation | 未核实 | 迁移归属未本轮复核；来源 https://github.com/aaif-goose/goose |
| 5 | goose v1.59.0→v1.60.2 11 天发 6 个 release；56.8k stars/6.6k forks/485 open issues | 未核实 | 发布节奏与仓库统计未本轮复核；来源 https://github.com/aaif-goose/goose/releases/tag/v1.60.2 |
| 6 | goose 会话落盘为 SQLite `sessions.db`（sqlx、WAL、`CURRENT_SCHEMA_VERSION=18`），Agent 内核可重入 | 未核实 | 表结构与状态机未本轮复核；来源 https://github.com/aaif-goose/goose/blob/main/crates/goose/src/session/session_manager.rs#L1172 |
| 7 | goose 的 MCP client 配置在 `config.yaml` 的 `extensions:` 段，`type: streamable_http` 支持 `uri`/`headers`/`env_keys`/`envs`，header 值可用 `$VAR` 展开；SSE 已被拒绝 | 未核实 | 未本轮复读 `extension.rs`；来源 https://github.com/aaif-goose/goose/blob/main/crates/oi-kit-agent/src/extension.rs#L194 |
| 8 | openai/codex 许可证为 Apache-2.0，最新 Rust release 为 rust-v0.171.0（2026-11-20T00:44:47Z） | 未核实 | 版本元数据来自素材，未本轮复核；来源 https://github.com/openai/codex/releases/tag/rust-v0.171.0 |
| 9 | codex 的审批与沙箱面向 shell 命令而非工具级授权：execpolicy 以 prefix_rule 匹配命令行 token，sandboxing 为 OS 级隔离，没有 per-MCP-tool grant 或租约语义 | 未核实 | 未本轮复读 execpolicy；来源 https://github.com/openai/codex/blob/main/codex-rs/execpolicy/README.md |
| 10 | duthub 自研薄运行时的底座已就位：rmcp 3.10 已启用 client 与 transport-child-process，`provider_models.rs:344` 已有 Chat Completions 兼容客户端；`duthub-agent` 仍是空壳 | 未核实 | 未本轮核对 `duthub-mcp/Cargo.toml:10` feature 与 `apps/desktop/src/provider_models.rs:344`；本地 `crates/duthub-agent/src/lib.rs:1` |
| 11 | ACP 的 `session/load` 要求 Agent 在回复前以 `session/update` 重放完整对话历史（需 loadSession 能力）；`session/resume` 不得重放历史（需 sessionCapabilities.resume）；文档称 load 使会话可跨重启持久化 | 未核实 | 未本轮复读 session-setup 全文；来源 https://agentclientprotocol.com/protocol/session-setup |
| 12 | SQLite WAL 的提交是向 WAL 追加 commit 记录，崩溃后由首个新连接独占恢复；`synchronous=NORMAL` 时事务不再持久，掉电或硬复位可能回滚 | 未核实 | 未本轮复读 SQLite 文档；来源 https://sqlite.org/wal.html |
| 13 | Anthropic 的上下文工程实践把 compaction 与 structured note-taking 列为长任务保持连贯的主要手段 | 未核实 | 未本轮复读原文；来源 https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents |
| 14 | rmcp 最新版为 3.11.0（2026-11-17），crates.io 标 Apache-2.0，累计下载约 5,251 万次；3.7.0→3.11.0 集中在 2026-10-22 至 2026-11-17 | 未核实 | 未本轮查 crates.io；来源 https://crates.io/api/v1/crates/rmcp |
| 15 | rmcp 3.11.0 的 client/server 均覆盖 stdio 与 Streamable HTTP，client 另有 child-process transport，server 以 Tower service 挂载；client 提供 Discover/Auto/Initialize 生命周期模式 | 未核实 | 未本轮复读 README；来源 https://raw.githubusercontent.com/modelcontextprotocol/rust-sdk/rmcp-v3.11.0/README.md |
| 16 | MCP 2026-07-28 的服务端核心能力仍是 tools/resources/prompts，客户端能力是 elicitation；规范把工具安全责任放在宿主，要求调用任何工具前取得显式用户同意，工具描述/annotations 视为不可信 | 未核实 | 未本轮复读规范正文；来源 https://modelcontextprotocol.io/specification/latest |
| 17 | 另一候选 `a2a-rs`（EmilLindfors，MIT，0.10.5，2026-10-20，MSRV 1.96）覆盖 v1.0 全表面并提供非规范 ConnectRPC 默认传输与 a2a-mcp 双向桥 | 未核实 | 未本轮复核该 crate；来源 https://github.com/EmilLindfors/a2a-rs#readme |
| 18 | 官方 A2A SDK 的最小服务端面是实现 AgentExecutor + TaskStore，用 DefaultRequestHandler 组装，挂 jsonrpc_router 或 rest_router，并在 `/.well-known/agent-card.json` 提供 Card；官方 JSON-RPC 方法名为 PascalCase，与 v0.3 的 message/send、tasks/get wire 不兼容 | 未核实 | 未本轮复读 a2a-server 源码；来源 https://github.com/a2aproject/a2a-rs/blob/main/a2a-server/src/lib.rs#L9-L28 |
| 19 | A2A 官方口径：MCP 是纵向协议（agent→工具/资源/API），A2A 是横向协议（agent↔agent）；A2A server agent 内部仍用 MCP 调用自己的工具 | 未核实 | 未本轮复读专题；来源 https://a2a-protocol.org/latest/topics/a2a-and-mcp/ |
| 20 | `a2a-server-lf` 0.5.3 于 2026-10-16 发布（Apache-2.0，累计下载约 9.3 万次；2026-09-30 至 10-16 连发 0.5.0/0.5.1/0.5.2/0.5.3） | 未核实 | 下载量与发布节奏未本轮复核；来源 https://crates.io/crates/a2a-server-lf |
| 21 | A2A `INPUT_REQUIRED`/`AUTH_REQUIRED` 是中断态，跨机续跑靠同一 taskId/contextId；AUTH_REQUIRED 的凭证必须带外传递；该状态本身 MUST NOT 被视为对任何操作的授权；规范不定义凭证的作用域/表示/有效期/撤销；a2a-server-lf 只内置 InMemoryTaskStore | 未核实 | 未本轮复读 §7.6.1–7.6.4；来源 https://a2a-protocol.org/latest/specification/#76-in-task-authorization |
| 22 | Windows 证书存储中的不可导出私钥可通过 `rustls-cng` 与 rustls 的 client/server 两侧配合使用；a2a-server-lf 的 TLS 模块只重新导出 rustls/axum-server，不会自动读取 Windows 证书存储 | 未核实 | 未本轮复核 rustls-cng；来源 https://github.com/rustls/rustls-cng/blob/dev/README.md |
| 23 | 「无论用户是否登录都运行」的任务以非交互方式执行、不显示 UI；S4U 不保存密码且无网络/EFS 访问，Password 才保存凭据；两者都要求账户具备 Log on as batch job | 未核实 | 未本轮复读 Task Scheduler 安全上下文文档；来源 https://learn.microsoft.com/en-us/windows/win32/taskschd/security-contexts-for-running-tasks |
| 24 | 服务可在 Session 0 通过 RegisterDeviceNotification + SERVICE_STATUS_HANDLE 接收 USB 变更；COM 口通过 CreateFile 打开；DirectShow 采集与 ADB 在 Session 0 的可用性没有一手文档支持 | 未核实 | 未本轮实测；来源 https://learn.microsoft.com/en-us/windows/win32/services/receiving-events-in-a-service |
| 25 | Task Scheduler 的任务历史由 Microsoft-Windows-TaskScheduler 事件日志记录且可被关闭；本机 `wevtutil gl Microsoft-Windows-TaskScheduler/Operational` 返回 `enabled:false` | 未核实 | 未本轮执行该命令；来源 https://learn.microsoft.com/en-us/previous-versions/windows/it-pro/windows-server-2008-R2-and-2008/cc722006(v=ws.11) |
| 26 | bmaptool 的 create 为所有映射块生成 SHA256，copy 在写入时校验；bmap 文件本身也有 SHA256 并在开始烧录前校验 | 未核实 | 未本轮复核 bmaptool；来源 https://github.com/yoctoproject/bmaptool |
| 27 | Stripe 幂等键：同一 Idempotency-Key 的重试返回首次保存的状态码与响应体，v1 的键至少保留 24 小时，参数与首次请求不一致会报错 | 未核实 | 未本轮复核 Stripe 文档；来源 https://docs.stripe.com/api/idempotent_requests |
| 28 | Copilot CLI 运行时闭源：公开仓库只有 LICENSE/README/changelog/install/container，无源码；GitHub 博客称运行时由 TypeScript 重写为逾 80 万行 Rust，源码在私有仓库 | 未核实 | 未本轮复核博客与仓库语言统计；来源 https://github.blog/ai-and-ml/generative-ai/migrating-the-github-copilot-runtime-to-rust-using-copilot/ |
| 29 | Copilot CLI 的 ACP 客户端可在 session/new 中提供 MCP 服务器，支持 stdio/HTTP/SSE；ACP 会话进入历史与 `session_store_sql`；A2A 在官方 ACP 文档与 changelog 中零命中 | 未核实 | 未本轮逐条复核 changelog；来源 https://github.com/github/copilot-cli/blob/main/changelog.md |
| 30 | 截至 2026-11-20：Copilot CLI 稳定版 1.0.114（2026-11-18）、prerelease 1.0.115-2（2026-11-20）；仓库 11,441 stars、2,179 open issues；goose 56,821 stars；Zed v1.29.2（2026-11-18） | 未核实 | 未本轮复核仓库统计与 release；来源 https://github.com/github/copilot-cli/releases |
| 31 | ACP 由 Zed 创建并以 Apache 许可开放，Zed 在协议中扮演 client，Agent 由各上游实现独立接入 | 未核实 | 未本轮复读 zed.dev/acp；来源 https://zed.dev/acp |
| 32 | Zed 的 Agent 体系支持 MCP：内置 Agent 直接使用 `context_servers` 中配置的 MCP servers，这些 servers 也能经 ACP 转发给外部 Agent | 未核实 | 未本轮复读 Zed MCP 文档；来源 https://zed.dev/docs/ai/mcp |
| 33 | Windows Job Object 默认把子进程自动加入父进程所在 job，`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 在关闭最后一个 job 句柄时终止全部关联进程；`BREAKAWAY_OK`/`SILENT_BREAKAWAY_OK` 可让子进程脱离 | 未核实 | 未本轮复读 Job Object 文档；来源 https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects |
| 34 | Codex v0.171.0 的 `SandboxMode` 把 `read-only` 标为 `#[default]`，另有 workspace-write 与 danger-full-access 需显式选择 | 未核实 | 未本轮复核源码；来源 https://github.com/openai/codex/blob/rust-v0.171.0/codex-rs/protocol/src/config_types.rs#L104-L114 |
| 35 | Copilot CLI 对读类操作一般自动放行，写文件、破坏性 shell 与访问 URL 需要批准；`--deny-tool` 优先于 `--allow-tool`/`--allow-all`，`--available-tools` 可限制白名单 | 未核实 | 未本轮复读 GitHub 文档；来源 https://docs.github.com/en/copilot/how-tos/copilot-cli/use-copilot-cli/allowing-tools |
| 36 | duthub 同时存在两套工作根解析：`ensure_portable_work_dir` 取 CLI `--workdir` > settings `workspaceDir` > exe 目录，而同 crate 的 `resolve_work_dir_in` 取 settings > CLI | 未核实 | 本地 `crates/duthub-core/src/store.rs:77-103`/`:133-157`；本轮已读并确认两段代码优先级相反；但「哪些调用点用哪套」未全部枚举，故标未核实全貌 |
| 37 | `codex exec --json` 在 stdout 输出每行一个 JSON 事件（thread.started、turn.started/completed/failed、item.*、error），默认模式 stdout 只写最终消息、其余走 stderr；`codex exec resume <SESSION_ID>`/`--last` 与 `fork` 用于恢复/分叉 | 未核实 | 未本轮复读 CLI 源码；来源 https://raw.githubusercontent.com/openai/codex/main/codex-rs/exec/src/cli.rs |
| 38 | Codex 用 `--sandbox read-only / workspace-write / danger-full-access` 选沙箱、`-C/--cd <DIR>` 指定工作根、`--add-dir` 追加可写目录、`--ephemeral` 关闭会话落盘；npm 最新 0.171.0（2026-11-20），Apache-2.0，提供 win32-x64/arm64 平台包 | 未核实 | 未本轮复核 npm 与 CLI 源码；来源 https://raw.githubusercontent.com/openai/codex/main/codex-rs/utils/cli/src/shared_options.rs |
| 39 | dsh 是 DeepSeek AI 官方开源的 agent harness（MIT），README 标 developer preview 并明确会有破坏兼容性的变更；最近 release 为 dsh-v0.2.2-rc.5（2026-11-18），当前只有预发布 | 未核实 | 未本轮复核仓库与 release；来源 https://github.com/deepseek-ai/deepseek-harness |
| 40 | dsh 的 sandbox 提供 read-only/workspace-write/danger-full-access 三档文件效果策略，但 Windows ACL 后端对 read-only 只报告 partial enforcement | 未核实 | 未本轮复读 sandbox 文档；来源 https://github.com/deepseek-ai/deepseek-harness/blob/master/docs/subsystems/sandbox.md |
| 41 | Claude Code `claude -p` 的后台工作默认有 10 分钟空闲上限（`CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS` 可调），SIGTERM 退出码 143，中断回合默认不续跑（需 `CLAUDE_CODE_RESUME_INTERRUPTED_TURN=1`） | 未核实 | 来源 https://code.claude.com/docs/en/headless.md；本轮未复读该限制，正文已标注 |

### 12.4 本轮本地核对（命令与输出）

见 §2.8。补充说明：

- 上述命令全部在 `D:\Users\luo-j\Desktop\serialmcp\duthub` 执行；输出为 Git Bash 文本。
- `grep -c '#\[tool('` 统计的是宏标注数，不是运行时注册数；本文把它与 `plan_tool_allowed` 的 13 个名字、A2A 的 8 个证据工具交叉核对，不把 27 当作对外可见数。
- `Cargo.lock` 的 `rusqlite`/`redb`/`x509-parser` 计数为 0，说明 P0 必须新增依赖与 lock 变更；不是「已有依赖可直接用」。
- `grep -rniE '\bacp\b|agent-client-protocol'` 只扫 `*.rs`/`*.toml`，不包含文档；文档中的 ACP 讨论不影响「代码层无 ACP」的结论。
- `a2a_running` 的行号按本轮 `grep` 核对为 `workspace.rs:631`/`:940`、`pages.rs:3621`；`workspace.rs`/`pages.rs` 仍在并行修改，后续引用需重新定位。

### 12.5 设计调研时点未运行（W4 状态见报告）

- 设计调研时点未运行 `cargo build`、`cargo test`、`cargo clippy`、`cargo fmt --check`、`bash scripts/regression`、`cargo run -p duthub-desktop -- --self-test`、release 构建。
- 设计调研时点未运行桌面 GUI、真机串口/继电器/烧录、Session 0、计划任务。
- 设计调研时点未实连 goose、Copilot、Claude Code、codex、dsh 或任何 ACP/A2A 真实 peer。**2026-11-21 W4** 已由默认测试启动真实 `goose-acp 1.60.2`（ACP initialize/session-new、`mcpServers` 注入、假模型服务、`--version`）；真实在线模型、真实 CLI 与 A2A peer 仍未运行。
- 9,832,133 B 资产、Defender/EDR 行为、真实 CLI 运行行为仍未复核（资产哈希已由 W2/W4 测试固定）。
- 因此，本文旧「设计裁决」不构成验收通过；W4 的实测范围以 `docs/reviews/AGENT_IMPLEMENTATION_REPORT.md` 为准。
