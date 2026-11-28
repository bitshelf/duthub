# MCP 工具契约

桌面串口、连接管理和设置由 GPUI Workbench 调用应用服务。外部客户端通过下文列出的 MCP 工具访问。

## 事件（可靠顺序 & 背压）

```json
{
  "seq": 1082,
  "ts_unix_ms": 1794400000000,
  "session_id": "sess-rk3576-a",
  "dut_id": "rk3576-a",
  "kind": "SerialChunk",
  "payload": { "encoding": "utf8-lossy", "data": "root@rk3576:~# " }
}
```

界面切换 Agent 视图不应重建 serial task。事件队列满时回传 `Lagged(n)` 并让 UI 从日志缓冲补读，不应静默丢失内核 panic 数据。

## MCP 工具契约

客户运行 `duthub.exe`，通过内置 Streamable HTTP 使用 MCP。接入分两类：

- **通用接入**（「Agent 页 → Agent 接入」，原设置页面板已迁入）：供 LLM / 外部 Agent 使用，主端口默认 `127.0.0.1:8797`，可追加通用端口；凭据可按 DUT 授权，保持既有全局只读语义。
- **DUT 专用接入**（「连接管理 → MCP 服务」）：全局认证开启时，为每台已保存 DUT 自动分配独立端口和 Token，端口可手改并在重启后生效；Token 只有该 DUT 的 `read` Grant，不能写；`dut_files` 省略 `dutId` 时默认落到该 DUT；跨 DUT 请求返回 `LeaseRequired/UNAUTHORIZED`。DUT 专用监听器不出现在通用 Agent 接入页。

`agentAccess.lanAccess` 默认关闭（只绑 `127.0.0.1`）。显式开启后监听 `0.0.0.0`，同一端口同时接受回环与局域网地址，且强制 Bearer（认证关闭时 fail closed）。局域网为明文 HTTP，Token 与 DUT 数据可被同网段嗅探，仅限可信网络并配合防火墙；跨公网仍使用可信 HTTPS 网关或安全隧道。Token 缺失、无效、停用、跨监听器或接入关闭时返回 HTTP 401。`Host` 校验在回环监听下仅放行 `localhost`/`127.0.0.1`/`::1`；开启 `lanAccess`（非回环/通配监听）时自动追加本机局域网接口 IP 与本机主机名，并支持在 `agentAccess.allowedHosts`（`camelCase`，默认空且省略序列化）中追加自定义域名或反向代理 `host:port`；`Origin` 头默认全部拒绝（防浏览器 CSRF/DNS rebinding）。打开串口会话默认保持 `ready`（不自动抢占永久 GUI 租约），仅在人工显式开启「人工独占」/「强制接管」或桌面终端正在键入（有限 TTL）时进入 `manual_takeover`/`busy_gui`。MCP 返回给 Agent 的串口与归档日志文本（`serial_read`、`serial_exec.output`、`log_segment_read`、`serial/tail`、`logs/{segmentId}`）自动剥离 ANSI 颜色/高亮转义序列以减轻上下文负担，落盘原始日志保持原始字节不变。

### 本地 Agent（goose，下载安装）

duthub 不再内嵌 goose：用户在「工具下载」页经 `duthub-tooldown` 从上游 release 安装（`<config>/tools/goose/`，安装内核 manifest 记录实测 SHA-256），运行时按「显式外部路径（固定 `expectedSha256`，启动复算）> 已下载安装（按 manifest 记录复算现盘字节）> PATH 裸名」解析，版本门槛 >= 1.60.2；均不可用时返回可展示的 `RegistryError::NotInstalled`（`registry::goose_install_state()`）。每个 DUT 一个 goose 子进程与一个 ACP 会话；goose 通过 `session/new.mcpServers` 接收**每会话 loopback listener**（ID `session-<uuid>`、临时端口、`listenerDut=Some(dut)`）与只读 Token。该 Token 只有绑定 DUT 的 `read` grant，工具面仍是 13 个 Plan 工具（桌面发起的 Agent 会话不接通知器，因此没有 `notify_user`）；会话停止或结束立即吊销 Token 并关闭 listener。下载由用户显式触发，不自动下载/更新；外部 goose 路径可由用户显式选择并固化哈希（`agents.json` + `agent_host/registry.rs`，Host 经 `registry::resolve` 启动）。模型注入：`GOOSE_MODE=approve`，支持 OpenAI 兼容协议与 Anthropic Messages（key 只经环境变量注入）。

MCP 2026-07-28 客户端通过 `server/discover` 发现能力，并为每个请求发送协议版本和路由头。该版本不分配 `Mcp-Session-Id`。普通工具请求可收到单个 JSON 响应，客户端仍应接受 JSON 和事件流。旧版客户端继续通过 `initialize` 协商。两种流程均逐请求核验 Bearer。

### Plan 模式

使用 `tools/list` 获取工具与 JSON Schema。服务器按白名单分发。**用户配置的通用/DUT MCP 监听器**额外提供 `notify_user`（13 个只读工具 + 1 个通知工具）；桌面发起的 Agent 会话与其他端点只有 13 个只读工具。通用接入的已认证 Agent 可以读取工作目录中的所有 DUT 资料，无需逐台授权；DUT 专用接入严格限制为绑定 DUT（日志、文件与在线会话同样受限）。在线会话、连接状态和媒体流继续按 DUT 授权。

| 工具 | 契约 |
|---|---|
| `dut_list`、`dut_status(dutId)` | 通用接入返回该 Token 已授权的 DUT；DUT 专用接入只返回绑定 DUT。统一返回机读 **`dutState`**（`ready`/`busy_gui`/`busy_agent`/`flashing`/`manual_takeover`/`needs_inspection`/`disconnected`）、**`nextActionHint`**、**`capabilities`**（按当前凭据实际授权动态计算）、脱敏 **`flashPresets`**、**`transfer`**（流式 `PUT` 与 tus 1.0.0 上传端点）、**`logsExport`**、**`sessionEpoch`** 以及 `controlStatus`（含 `sessionName`、`acquiredAtUnixMs`、`occupiedSecs`） |
| `serial_read(dutId, sinceSeq?, limit?, tail?, mode?, waitFor?, waitRegex?, timeoutMs?)` | 只读接收实时串口流或 VT100 渲染屏快照（`mode="screen"`，亦可通过 Resource `duthub://dut/{dutId}/serial/screen` 直读），自动剥离 ANSI 颜色/高亮转义并返回 `sessionEpoch`。传入 `waitFor` / `waitRegex` 时基于 `sinceSeq` 游标无漏行等待（上限 30s），返回 `matched`/`matchedBy`/`elapsedMs`；关闭会话改查归档 |
| `log_segments(dutId, offset?, limit?)`、`log_segment_read(dutId, segmentId, offset?)`、`log_export(dutId, sinceSegmentId?, tailSegments?, stripAnsi?, inline?)` | 分页或一次性快照导出 DUT 归档日志（返回固定快照边界、SHA-256 与同端口受鉴权 HTTP 下载链接 `GET /dut/{dutId}/logs/export`）。目录身份按稳定 DUT ID 校验；DUT 专用接入仅能读绑定 DUT |
| `dut_files(dutId?, category?, offset?, limit?)`、`dut_file_read(dutId, fileId, offset?)` | 列出工作目录中的 DUT 或其文件（含 `mtimeUnixMs`），返回六大标准目录分类（`image` 含 `image/`、`images/data/` 及根目录 `.img`/`.bin`/`.iso`；`TEST`、`压力&老化`、`串口日志`、`misc`、`feedback`）；按返回的文件 ID 读取 Base64 分块；镜像上传走同端口流式 `PUT /dut/{dutId}/image/{filename}`（`curl -T`）或 tus 1.0.0 `/dut/{dutId}/transfer`（`ztus`/`curl`，强制要求 `flash` 权限） |
| `adb_list_devices()` | 查询已运行的 127.0.0.1:5037 ADB 服务。只返回授权 DUT 的绑定，服务不可用则返回错误 |
| `media_streams(dutId)` | 读取已启动的采集及状态；流 URL 应保密。带 `op` 时按需取数（需该 DUT 的 `capture` 授权）：`{"kind":"snapshot","source?","note?"}` 截取静帧；`{"kind":"recordStart","source?","note?"}` 开始录屏并返回 `recordingId`；`{"kind":"recordStop","recordingId"}` 取回产物 |
| `skill_export()` | 返回嵌入的 `skills/duthub/SKILL.md` 正文（支持 `npx skills add bitshelf/duthub` 安装），不写盘 |
| `notify_user(message, title?, level?, dutId?)` | **仅用户配置的 MCP 监听器**：在 duthub 用户界面弹出通知。不是设备写动作（无审批、无租约），但会唤起界面，受宿主告警开关、抑制与限流约束；`queued` 只表示已入待呈现队列，不代表用户已看到 |
| `agent_feedback(category, title, detail, dutId?, severity?, toolName?, sessionName?)` | **仅用户配置的 MCP 监听器**：将 Agent 使用反馈以带元数据 Markdown 文件原子写入 `<DUT>/feedback/`（通用监听器省略 `dutId` 时写入 `<work_dir>/feedback/`），用于沉淀痛点与后续优化依据 |
| `serial_exec(dutId, text?, keys?, triggerReset?, waitFor?, send?, thenSend?, expect?, expectRegex?, settleMs?, stallTimeoutMs?, timeoutMs?, markLabel?, ...)` | **仅用户配置的 MCP 监听器**（需 `serialWrite`，`triggerReset` 另需 `reset`）：发送串口文本或白名单逻辑按键（`keys`，先全量校验再发送，非法返回 `INVALID_KEY`），支持 `expect`/`expectRegex`/`settleMs` 静默稳定检测与 `stallTimeoutMs` 零回显停滞识别，返回 `matchedBy`/`bytesObserved`/`stalled`/`screenTail` |
| `dut_control` / `adb_exec` | **仅用户配置的 MCP 监听器**：原子硬件控板/烧录/多 Agent 租约（`acquire_lease` 必填 `sessionName`）与限定 DUT 目录的 ADB 调试 |

#### `notify_user` 契约

```json
{"message": "string", "title": "string?", "level": "info" | "warning" | "error", "dutId": "string?"}
```

- **参数**：`message` 必填，去控制字符并截断到 **200 字符**，空串（含纯空白）→ `invalid_params`（`code=MESSAGE_REQUIRED`）；`title` 截断到 **48 字符**，省略时宿主用默认标题；`level` 只接受 `info`/`warning`/`error`，**其他任何字符串 → `invalid_params`（`code=INVALID_LEVEL`，不静默降级为 info）**；`dutId` 可选。参数结构 `deny_unknown_fields`，未知键按 `invalid_params` 拒绝。
- **授权**：不是设备写动作，所以**没有审批令牌、不进 `approval_request`、不取租约**。授权按目标 DUT 分派：省略 `dutId` 时通用监听器要求凭据至少持有一个 `read` grant（零 grant 拒绝），DUT 专用监听器默认绑定该 DUT；显式 `dutId` 时逐 DUT 校验 `read` grant，DUT 专用监听器跨 DUT、以及通用监听器对未授权 DUT 的显式标注，都返回 `LeaseRequired/UNAUTHORIZED`。
- **Plan 模式允许**：`notify_user` 是 Plan 面里唯一的非只读工具（它不触设备），因此在通用/DUT 监听器上可在 Plan 模式调用。它能唤起用户界面，这是产品拍板的明确行为；滥用由宿主的 DUT 告警开关（`alertsByDut.<id>.enabled=false`）、来源抑制、30 秒去重、每来源容量与令牌桶限流约束（不属于设备安全边界）。
- **返回**（如实反映宿主处理结果，绝不暗示「用户已看到」）：

  | `outcome` | 含义 | `queued` |
  |---|---|---|
  | `queued` | 已进入 AlertCenter 待呈现队列 | `true` |
  | `throttled` | 被限流，`reason` 取 `dedup`（30 秒去重）/`capacity`（同来源容量满）/`rate`（令牌桶空） | `false` |
  | `suppressed` | 被 DUT 或来源抑制规则拦下 | `false` |
  | `disabled` | 该 DUT 的告警开关关闭（`alertsByDut.<id>.enabled=false`；全局总开关已移除，无通知器的端点根本不会暴露该工具） | `false` |

  响应恒带 `"source":"agentNotice"`；`reason` 只在 `throttled` 时出现。判定顺序固定为 `enabled → suppressed → 去重 → 容量 → 限流`，所以 DUT 告警被关闭时返回 `disabled` 而不是 `suppressed`。
- **审计**：沿用 `tool.call`，只记 tool/dut/耗时与结果长度，**通知正文不进审计**。
- **已知限制（R6 口径）**：只覆盖用户配置的通用/DUT MCP 监听器。桌面发起的全部 Agent 会话（goose 与 claude/codex CLI profile，均经 `spawn_http_listener_loopback`）不接通知器，`notify_user` 对它们不可见也不可调用（调用返回 `PLAN_MODE_ONLY`）。A2A 证据端（`EVIDENCE_SCOPE_ONLY`）同样没有该工具。

`media_streams` 的 `op` 分支是 Plan 面内唯一需要写类 Grant 的能力。查询（不传 `op`）仍只要 `Read`，响应与旧版逐字一致（`{streams, status?}`；DUT 不在快照时只有 `{streams:{}}`）。`snapshot`/`recordStart`/`recordStop` 需要该 DUT 的 `capture` 授权：无授权返回 `LeaseRequired/UNAUTHORIZED`，DUT 专用监听器跨 DUT 同样拒绝，且默认没有任何凭据持有 `capture`。

- `snapshot` → `{artifact:{path, bytes, width?, height?, note?, source}}`（`width`/`height` 取自 PNG 头，解析失败时省略，不伪造）。
- `recordStart` → `{recording:{recordingId, source, firstPath, note?, startedAtMs}}`；`recordStop` 必须用同一个 `recordingId` 配对，返回 `{artifact:{path, firstPath, bytes, rotated, durationSecs, note?, source}}`。`path` 是末段、`firstPath` 是首段、`rotated` 表示发生过轮转；中间段无法枚举。`durationSecs` 是实测墙钟，可远超任何固定窗口。
- **没有时长参数、没有时间上限**：schema 不含 `seconds`，任何未知键按 `invalid_params` 拒绝（`deny_unknown_fields`）。录屏只由配对的 `recordStop`、面板停止采集、绑定变更/DUT 删除或进程退出结束；配额是唯一存储边界。
- 单次调用有界（`snapshot`/`recordStart`/`recordStop` 各 30 秒），录制时长不计入任何 MCP 超时。失败码：`MEDIA_NOT_BOUND`、`MEDIA_CAPTURE_UNAVAILABLE`、`MEDIA_START_FAILED`、`MEDIA_CAPTURE_FAILED`、`MEDIA_CAPTURE_ENDED`、`MEDIA_RECORDING_ENDED`、`MEDIA_RECORDING_UNKNOWN`、`MEDIA_BUSY`、`MEDIA_RECORDING_IN_PROGRESS`、`MEDIA_OP_DROPPED`、`MEDIA_OP_TIMEOUT`；失败一律是工具错误，绝不回落成空的 `streams`。

凭据按监听器隔离：`listenerId` 必须与实际端口匹配，Token 不能跨端口使用；DUT 专用端点各自一个只读 Token，重新生成后旧 Token 立即失效并回收其租约。**通用监听器的文件/日志全局读至少需要一个 Read grant**：`log_segments`/`log_segment_read`/`dut_files`/`dut_file_read` 在通用监听器上保持「已认证可读全部 DUT 资料」语义，但零 grant 的凭据一律拒绝（`server.rs` `authorize_scoped_read` → `authorize_any_read`）；DUT 专用监听器严格限定绑定 DUT。

串口写入、交互 Shell、烧录、继电器动作、修改心跳、创建日志标记、上传文件等请求均返回 `PLAN_MODE_ONLY`。**`notify_user` 是唯一的非只读例外**：它不触设备、无需审批，但只在用户配置的监听器上存在，且能唤起用户界面。未经 DUT 授权的读请求返回 `LeaseRequired/UNAUTHORIZED`。本地源码构建的旧 stdio 工具只供兼容离线快照，客户使用内置 HTTP。

### A2A 端点（实验性，未经产品签字）

每台已启用 A2A 的 DUT 一个独立回环端点（`crates/duthub-a2a`）。**实验性边界**：

- **仅回环**：只绑 `127.0.0.1`；`bind_addr` 为其他值（含 `0.0.0.0`）一律拒绝并以
  `NeedsProductSignoff` 状态报告（`listener.rs`）。**无 LAN 监听**，无 TLS，无 peer 认证。
- **JSON-RPC 3 方法**：`SendMessage` / `GetTask` / `CancelTask`（Bearer fail-closed）；
  其余 A2A 方法一律返回 JSON-RPC 错误 `-32601`（method_not_found）。
- **8 个证据工具**：执行体把 DataPart `{skill, dutId?, args}` 映射到该 DUT 的证据 MCP 服务，
  白名单 `EVIDENCE_TOOL_NAMES`（`server.rs:1353`）恰好 8 个：`dut_list`、`dut_info`、
  `dut_control_status`、`serial_read`、`analyze_serial`、`log_segments`、`log_segment_read`、
  `heartbeat_get`；`dut_files`/`dut_file_read`/`media_streams` 等不在面内；`args.dutId`
  与端点绑定 DUT 不一致一律拒绝。
- **请求体上限 256 KiB**（`A2A_BODY_LIMIT_BYTES`）；任务状态仅内存，进程重启即失
  （无持久化，ADR-9 StateDb 未接）；messageId 进程内去重，不承诺 taskId 复用。
- **状态词映射**：对外只回标准 A2A 状态；duthub 内部 Paused/NeedsInspection/Interrupted
  经 metadata 表达，远端 peer 永远看不到非标准状态。
- **实验性声明**：A2A 未经产品签字（ADR-2/8/9 为 Proposed），不得进产品验收、不得作为
  对外发布承诺；端口变更重启生效，与 MCP/A2A 端口做全局唯一性探测。

### 便携资料目录

每台 DUT 使用 `<工作目录>/<DUT 名称>/`。优先使用启动参数 `--workdir`，其次使用设置页保存的目录，默认使用可执行文件所在目录。例如 `target/release/RK3576/` 与 `target/release/duthub.exe` 平级。串口日志写入 `RK3576/logs/RK3576-065_20260918_114520.txt`。录屏、截图、镜像和报告都计入每台 DUT 的容量，默认 10 GiB。程序保存新资料需要腾出空间时，按修改时间从该 DUT 中删除最旧的未占用文件。配置文件仍在可执行文件同级的配置目录。

## 失败与取消

统一错误类别：`NotFound`, `PortBusy`, `PermissionDenied`, `Timeout`, `Disconnected`, `InvalidConfig`, `ToolExited`, `ApprovalRequired`, `LeaseRequired`, `UnsupportedAction`。 `CancellationToken` 随请求传播，关闭 DUT 时按序取消 Agent → 串口 task → 设备句柄。

- `ApprovalRequired/{Missing|Unknown|Mismatch}`：审批令牌失败（高危动作二次确认闸门）。
- `LeaseRequired/{DUT_BUSY|LEASE_EXPIRED|LEASE_UNKNOWN|NOT_HOLDER|DUT_PROTECTED|GENERATION_STALE|UNAUTHORIZED|Missing}`：控制租约/授权失败（方案 §6.4）——业务互斥不伪装 401，401 只在 HTTP 认证层。

## 采集 op 复核补注

> 本节由收尾文档轮追加，只补充上文 `media_streams` 的客户端可见细节；上文契约逐字不变。行号为 2026-11-24 实测。

- **`capture` 授权的写法**：授权存在凭据的 `grants[].actions` 里，值是 `"read"` / `"capture"`（`DutAction::as_str`，`crates/duthub-core/src/agent_access.rs:173`）。新建凭据默认没有任何 grant；DUT 专用监听器的 Token 只带绑定 DUT 的 `"read"`（`apps/desktop/src/agent_host/listener.rs:99`），因此专用端点上带 `op` 的调用默认返回 `LeaseRequired/UNAUTHORIZED`。桌面授权矩阵当前只渲染「读」一列（`apps/desktop/src/pages.rs:4249` 的 `GRANT_ACTIONS`），授予 `capture` 需直接编辑 `settings.json`；手写的 `capture` 不会被界面勾「读」清掉（`toggle_agent_grant` 只增删单个动作，`apps/desktop/src/workspace.rs:2449-2453`）。
- **`source` 取值**：桌面返回的采集键，大小写不敏感。原有绑定使用 `hdmi` / `camera`，新设备使用 `capture-<id>`。请求的源不可用时选择第一个已启用且绑定视频的设备。没有可用设备或 DUT 资料目录缺失时返回 `MEDIA_NOT_BOUND`。
- **失败码逐条含义**（`MEDIA_ERROR_CODES`，`apps/desktop/src/capture_backend.rs:157-169`，共 11 个）：

  | 码 | 含义 |
  |---|---|
  | `MEDIA_NOT_BOUND` | DUT 没有可用视频绑定 / 资料目录缺失 |
  | `MEDIA_CAPTURE_UNAVAILABLE` | 当前状态源没有采集能力（stdio、证据 server 或桌面未运行） |
  | `MEDIA_START_FAILED` | 起采失败，或按需会话 10 s 内未 ready |
  | `MEDIA_CAPTURE_FAILED` | 取帧/落盘失败，**含配额耗尽** |
  | `MEDIA_CAPTURE_ENDED` | op 进行中采集被停 |
  | `MEDIA_RECORDING_ENDED` | 录屏被停采打断 |
  | `MEDIA_RECORDING_UNKNOWN` | `recordingId` 不存在或已作废 |
  | `MEDIA_BUSY` | 同源已有在途 op、在途操作达上限（`MAX_IN_FLIGHT` = 4），或同一录屏已有收尾请求 |
  | `MEDIA_RECORDING_IN_PROGRESS` | 同源已在录屏 |
  | `MEDIA_OP_DROPPED` | 桌面未取走请求（工作台已退出） |
  | `MEDIA_OP_TIMEOUT` | 30 s 内桌面未应答 |
- **没有 `MEDIA_QUOTA` / `MEDIA_BACKEND_UNAVAILABLE` 码**：配额是存储边界，按 `MEDIA_CAPTURE_FAILED` 报；这两个名字只在测试里断言「不在面内」（`apps/desktop/tests/capture_backend_logic.rs:287-288`）。
- **`note`**：去控制字符、截到 64 字符，仅作元数据——不参与文件名、不进审计（`crates/duthub-mcp/src/server.rs:183`、`:334-342`）。

## 流式上传、并发烧录与 `.duthub/logs/` 契约补注

- **同端口高吞吐上传**：同一 MCP HTTP 监听器挂载 `PUT /dut/{dutId}/image/{filename}`（适配 `curl -T`）与 tus 1.0.0 `/dut/{dutId}/transfer`（适配 `ztus` `h2c` 及标准 tus 客户端），统一采用 4 MiB 流水线 `BufWriter`、增量 SHA-256 及调优后的 TCP/HTTP1/HTTP2 接收窗口。均要求 `DutAction::Flash` 授权与 `.img`/`.bin`/`.iso` 白名单。
- **上传与烧录文件级互斥**：`LeasePhase::Protected` 期间按规范化文件名（不区分大小写）检查：上传**不同文件名**允许与烧录同步进行；上传或覆盖正在烧录的**同一文件**返回 `409 FILE_IN_USE`；反向保护在 `flash_update` 入口拒绝正在上传中的同一文件（`UPLOAD_IN_PROGRESS`）。人工接管（`MANUAL_CONTROL`）与硬件核检锁（`NEEDS_INSPECTION`）仍拦截全部上传。
- **批量日志导出与 `.duthub/logs/` 规范**：`log_export` 工具与 `GET /dut/{dutId}/logs/export`（支持 `format=gzip` 与 ANSI 清理）提供单次快照全量导出。宿主机 Agent 审计与 `--trace` 日志统一落盘到 `.duthub/logs/audit/` 与 `.duthub/logs/traces/`；远端/本地 Agent 侧下载日志与传输日志规范存放在其工作区 `.duthub/logs/<dutId>/`。
- **阻断告警降噪与多语言**：`LEASE_UNKNOWN`/`LEASE_EXPIRED`/`LEASE_REQUIRED`/`NOT_HOLDER`/`STALE_GENERATION`/`QUEUED_AHEAD` 与正常烧录保护态不弹桌面人工介入窗。阻断告警的标题、正文和 MCP 错误数据中的 `reason`、`suggestedAction` 使用生成时 duthub 的界面语言（`zh-CN`/`en-US`）。独立 MCP 默认使用 `zh-CN`。
