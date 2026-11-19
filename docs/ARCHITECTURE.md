# duthub Rust 架构

## 运行时

- GPUI 桌面工作台通过 `duthub-core` 管理 DUT 配置、工作目录和授权，通过 `duthub-device` 管理串口会话。设备事件经有界总线送到界面。终端画面与会话日志各自维护。
- `duthub-adb` 调用 ADB 或 HDC，按实际枚举的设备标识连接。继电器由 `duthub-control` 管理。烧录工具的只读探测由 `duthub-flash` 执行。
- `duthub-mcp` 通过 stdio 和 Streamable HTTP 提供工具。桌面 Agent 宿主位于 `apps/desktop/src/agent_host/`，按 DUT 管理进程与证据端点，Windows 进程隔离由 `duthub-sandbox` 提供。
- `duthub-a2a` 提供实验性回环接入。`duthub-transfer` 提供独立的镜像接收端和运行示例。`duthub-tooldown` 管理外部工具的下载与安装。
- `apps/tui` 和 `duthub-agent` 是工作区占位成员。

## Rust Workspace

```
duthub/
├── Cargo.toml
├── crates/
│   ├── duthub-core/
│   ├── duthub-sandbox/
│   ├── duthub-device/
│   ├── duthub-adb/
│   ├── duthub-control/
│   ├── duthub-flash/
│   ├── duthub-mcp/
│   ├── duthub-a2a/
│   ├── duthub-agent/
│   ├── duthub-transfer/
│   └── duthub-tooldown/
├── apps/
│   ├── desktop/                 # GPUI 原生桌面（gpui-kit 0.9.1）
│   └── tui/                     # Ratatui（占位）
├── .agents/skills/
└── tests/
    ├── contract/
    ├── integration/
    └── fixture-logs/
```

## 线程/并发边界

1. 每个串口会话独占一个 UART handle，通过单个 task 读写，避免多个 Agent 与用户互相覆盖。
2. 将终端显示缓冲和持久化日志分开。UI 最多保存滚动窗口，磁盘日志按大小/日期轮换；二进制/ANSI 数据另有原始存储策略。
3. UI 使用有界 `tokio::sync::broadcast` 或 `mpsc` 事件，显式处理滞后，事件应包含 `dut_id`、`session_id` 和单调序号。
4. MCP Agent 为只读优先；`reset/power/flash` 受设备级互斥锁与批准闸门保护。
5. 路径：`PathBuf`/`OsString`，不可把 Windows 可执行路径拼接为 shell 命令。
6. 一个 DUT 至少对应一个串口标签；重命名同步标签标题，删除 DUT 关闭关联 task；Agent 目标跟随活跃串口。
7. 运行模式与传输模式分离：本机进程 `stdio` 无 TCP 端口；Streamable HTTP 接受完整 `http(s)://.../mcp` URL。

## 采集生命周期与媒体工具面（2026-11-24）

> 设计依据 `docs/design/CAPTURE_STREAM_BACKENDS.md`（v5，纯 FFmpeg）；本节只描述本树已落地的代码形状，行号为 2026-11-24 实测。

### 采集执行与所有权

- 每个 `(dut_id, Source)` 同时最多一路 `Capture`：1 个 ffmpeg（stdout 出 MPEG-TS）+ 一个本机 HTTP 拉流地址（`apps/desktop/src/media.rs:104`、`:185`、`:209` 的 `Capture::start` / `mpegts` / `axum::serve`）。预览、录屏、截图、MCP 与浏览器都从同一个 URL 各自消费，不复制采集进程。
- `MediaView` 是唯一所有者：持有 `sessions: BTreeMap<(String, Source), Capture>`、按 `tools.capture` 解析后的 `CaptureSettings`（ffmpeg/ffplay 已经过 `tool_resolver`，`apps/desktop/src/capture_backend.rs:83`）与容量配额。唯一创建点是 `start_bound_capture`，面板、侧栏监控与 MCP 取数三条路径共用。
- 输出分辨率只做 `scale+pad` 滤镜，绝不下发 `-video_size`：DirectShow 只接受设备原生模式，缩小与补边在输出侧完成。

### 租约账本与空闲回收

- 「谁还在用」由纯逻辑账本表达：`DemandKind::{Manual, Monitor, ExternalPreview, CopyUrl, AgentOp}`（`capture_backend.rs:184`）。`Manual` 是 pin（面板「开始采集」，永不自动回收）；`ExternalPreview`/`CopyUrl` 是 hold（`HOLD_SECS` = 300 s，每次使用重新续期，`capture_backend.rs:46`），避免丢掉的 ffplay 或已复制的 URL 永久钉住采集。
- `DemandEntry::decide` 的优先级（`capture_backend.rs:365`）：pin 或录制中 → `Pinned`；仍有存活租约 → `Keep`；ephemeral 且空闲窗口已过 → `Reclaim`；交互式最后租约已释放 → `StopNow`；从未被租用 → `NotLeased`。
- 空闲窗口读 `settings.tools.capture.idleSecs`：保存时 `validate_idle_secs` 只接受 1..=3600（`capture_backend.rs:65`）；运行时 `effective_idle` 对缺失/越界值回退默认 60 s（`capture_backend.rs:53`），不夹到 1 秒——手改配置写错不该变成激进回收。
- 唯一回收执行点是 `MediaView::tick`（`apps/desktop/src/media_page.rs:740`）：先 sweep 过期的 hold，再按 `decide` 收集需停的会话并 `stop_capture`（停 `Capture` 即真停 ffmpeg）。**只有 Agent 按需打开（ephemeral）的采集会被空闲回收**；面板 Manual 与监控预览只在自己的租约释放后停。
- 录制（手动或 Agent）经 `recording` / `agent_recording` 把会话钉住：录屏期间不计空闲，也没有时长上限；只有配对的 `recordStop` 或 `stop_capture` 能结束它。

### 媒体工具面（MCP）

- 工具名不变：`media_streams(dutId)`。不带 `op` 是只读查询（`DutAction::Read`）；带 `op` 的 `snapshot` / `recordStart` / `recordStop` 需要该 DUT 的 `capture` 授权（`crates/duthub-mcp/src/server.rs:540` 的 `authorize_capture`）。凭据 `grants` 默认为空（deny by default），只读 Token 得到 `LeaseRequired/UNAUTHORIZED`，DUT 专用监听器的绑定检查同样生效。桌面授权矩阵提供「读」和「画面」两列，可分别管理这两项授权。
- 桌面与 MCP 经 oneshot 桥连接：MCP 侧 `McpState::media_op` 提交请求（默认实现返回 `None`，`crates/duthub-mcp/src/state.rs:243`），桌面 `RuntimeMcpState` 转入 GUI 队列（`apps/desktop/src/mcp_runtime.rs:195`、`media_page.rs:645`），由 50 ms tick 消费并回 `MediaOpReply`。stdio、证据服务器或桌面未运行时直接回答 `MEDIA_CAPTURE_UNAVAILABLE`，不假装具备采集能力。
- 失败面固定为 `MEDIA_*` 码表（`capture_backend.rs:141`，共 11 个）；成功时 artifact 带 `path/first_path/rotated/bytes/width/height/duration_secs/note/source`（`crates/duthub-core/src/media_op.rs:105`）。
- 落盘与手动截图/录屏同源：`<artifact_root>/<DUT 名>/`，经 `capture_file`（`apps/desktop/src/media.rs:88`）走 DUT 归属校验与每 DUT 容量配额——这正是 op 分支需要写类授权、查询分支保持只读的原因。拉流地址按秘密对待，只交给可信客户端。
- `media_streams` 仍在 Plan 只读面（13 个工具，`server.rs:1545` 的 `plan_tool_allowed`）内：本轮不改工具名与白名单，op 分支的写能力单独由 `capture` Grant 裁决，不靠增删工具实现。

### 通知工具面（MCP，2026-11-24）

- `notify_user(message, title?, level?, dutId?)` 是第 14 条 `#[tool]` 路由，**只在用户配置的通用/DUT MCP 监听器上出现**（`PLAN_ROUTER_WITH_NOTIFY`）；桌面发起的 Agent 会话、stdio 快照端与 A2A 证据端仍只有 13 个只读工具（调用得 `PLAN_MODE_ONLY` / `EVIDENCE_SCOPE_ONLY`）。
- 它不是设备写动作：无审批令牌、无租约，但会唤起用户界面。MCP 侧负责授权（通用监听器要求任一只读 grant；显式 `dutId` 逐 DUT 校验，DUT 专用监听器默认绑定并拒绝跨 DUT）、参数校验（去控制字符、200/48 字符上限、非法 `level` → `invalid_params`）与如实回执（`queued|throttled|suppressed|disabled`，`throttled` 带 `dedup|capacity|rate`）。
- 呈现与限流归宿主（桌面 `AlertCenter`）：DUT 告警开关（`alertsByDut`；无全局总开关）、来源抑制、30 秒去重、每来源容量与令牌桶。MCP 侧不缓存、不丢结果，`queued` 只表示进入待呈现队列。
- 实现：`crates/duthub-mcp/src/notify.rs`（`UserNotifier` 抽象、`NotifyRequest/Outcome`、限长常量）与 `crates/duthub-mcp/src/server.rs`（两张静态路由表 + 手写 `list_tools`/`get_tool`/`call_tool`）。

### HDMI 浮窗与还原

- 共享帧，不开第二路采集：`FramePump`（`apps/desktop/src/monitor_window.rs:35`）是 `InlinePreview::poll` 的唯一消费者，由独立的 100 ms 任务驱动（`apps/desktop/src/workspace.rs:1810`）；50 ms 主 tick 不再碰帧（`workspace.rs:1787`）。换帧走 App 级 `drop_image`，避免浮窗存在后图集只在单窗回收。
- 状态机是 lib 纯逻辑 `capture_ui::next`（`apps/desktop/src/capture_ui.rs:95`）：`Docked/Floating/Closing` × 事件 → 开窗、关窗、还原、停预览。不变量：同时最多一个浮窗；`Floating` 期间右侧栏只画占位（`apps/desktop/src/side_panel.rs:1077`），关侧栏或切面板都不停预览（否则浮窗停在死帧）；主窗关闭先关浮窗。
- 浮窗是普通窗口且 `focus: false`（`workspace.rs:6968`），不抢主窗焦点，键盘仍归终端。「还原到右侧」由 `prepare_redock`（`side_panel.rs:1102`）翻回 `Docked`、关闭浮窗、把监控面板切回该 DUT 并激活主窗。
- 采集源始终取自当前 DUT 的已保存视频绑定：切换 DUT、改绑或停用都先释放旧帧与预览，再按新绑定启动；`Floating` 期间释放 `Monitor` 租约是 no-op（租约由浮窗持有）。

## 关键工程权衡

- **Ratatui 独立视图**：共享 `AppService`（M0 演示骨架，待删——AUDIT F02，见「数据流」节注），不尝试 HTML 转 TUI。
- **ADB 不直接用平台 USB API**：调用指定路径的官方 `adb`，从 `adb devices -l` 解析状态，按 serial 锁定目标。
- **不得把 MCP 与任意 OS shell 混为一谈**：应用级 Tool API 最小授权。
