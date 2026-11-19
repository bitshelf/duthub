# 采集生命周期、Agent 按需取数与 HDMI 浮窗（设计）

- 状态：设计 v5（2026-11-24 **范围收窄：纯 FFmpeg**；未实施，无代码改动）
- 范围：`apps/desktop` 的采集生命周期（`media_page.rs` / `side_panel.rs` / `workspace.rs` / `mcp_runtime.rs`）、
  `crates/duthub-core` 的 `tools.capture` 扩展与 op DTO、`crates/duthub-mcp` 的媒体工具面、HDMI 预览浮窗
- 相关：`docs/ARCHITECTURE.md`、`docs/API_CONTRACT.md`、`docs/SECURITY.md`、`docs/DATA_MODEL.md`、
  `AGENTS.md` 坑 #1/#13/#19/#24/#31/#41/#42/#43/#44/#64/#66
- 决策记录：2026-11-24 用户拍板**只做纯 FFmpeg 路线**，不引入 go2rtc / MediaMTX / webrtc-rs。
  本设计因此**不新增 ADR**（原 v4 的候选 ADR-12 已按否决归档，见 §8.1）。

> **v5 = 范围收窄（纯 FFmpeg）。** 相对 v4 的实质变更：
> ① **删除采集方案抽象**：`StreamBackendKind`、传输计划（`TransportPlan`/`plan_transport`/`resolve_backend_url`）、
>   「新方案注册四处加法」、四方案的能力矩阵全部删除；§2 重写为「唯一执行形态 + ffmpeg 参数如何构造 + 拉流地址如何产生」的事实描述。
> ② **配置改为扩展现有 `tools.capture`**：只新增 `idleSecs: Option<u32>`，删除 `stream` 段与
>   `go2rtc`/`mediamtx`/`externalFfmpeg` 四个子结构；`apps/desktop/src/media.rs` 一行不改。
> ③ `MediaView.stream` / `set_stream_settings` → `MediaView.capture` / `set_capture_settings`。
> ④ **Agent 录屏改为 `recordStart` / `recordStop` 配对，不设时间上限**（用户 2026-11-24 指示）：删除此前草稿的 `seconds` 参数与
>   `clamp_clip_seconds`（`MIN_CLIP_SECS`/`MAX_CLIP_SECS`/`CLIP_DEADLINE` 一并删除）；`recordStart` 回 `recordingId`，`recordStop` 回产物；
>   录屏期间会话被 pin、不参与空闲回收；停止沿用既有 `Capture::record`/`stop_recording`，配额轮转照旧（§5.2/§5.5）。
> ⑤ §8 删除 8.2/8.5/8.9/8.13；8.1 记为**已否决**（含实测人天与体积）、8.3 记为**已签字**、8.7 记为**已定**。
> ⑥ §9 删除 H 车道与 P4 行，估算按收窄后重算；冻结文件清单保留。
> ⑦ 错误码 `MEDIA_BACKEND_UNAVAILABLE` 更名为 `MEDIA_CAPTURE_UNAVAILABLE`（已无「后端」概念）。
>
> **v4 / v3 的评审阻断项修复已并入正文**（三段计时、`InFlight` 的 lease 字段、`agent_recording` 在 `record()` 成功时写入、
> 删除 `MEDIA_QUOTA`、`release_monitor` 在 `Floating` 下 no-op、op DTO 放 core、`In` 全部 unit 变体等），不再单列变更块。

> 核对方式：本文所有代码结论均来自本会话在本工作树执行的只读检索（`grep`/`sed`/`cat`）与实测行号（2026-11-24 复校）。
> **未运行任何 `cargo build` / `cargo check` / `cargo test`**（本任务只改设计），
> 因此「测试会通过/不会失败」均为源码静态推断，不是执行结果。

---

## 1. 现状与硬约束

### 1.1 现状链路（事实基线）

| 环节 | 现状 | 锚点 |
|---|---|---|
| 唯一所有者 | `MediaView` 持有 `sessions: BTreeMap<(String, Source), Capture>`，`MediaView` 由 `Workbench` 持有 | `apps/desktop/src/media_page.rs:106-109`、`workspace.rs:1040` |
| 构造 | `Workbench::new` 用 `artifact_root` 构造 `MediaView::new(dir)`，并把 `media.urls` 克隆给 MCP 运行时 | `workspace.rs:1431`、`workspace.rs:1532` |
| 创建点 | 只有 `start_bound_capture` 创建 `Capture`；Windows 先按当前枚举把友好名解析成 DirectShow 端点，启动成功后再把 `capture.input` 还原为用户配置值 | `media_page.rs:250-281`（还原在 `:279`） |
| 启动入口（面板） | 连接管理面板 `Action::Start` → `run_action` → `start_bound_capture` + `sessions.insert` | `media_page.rs:1116-1141`（`Action::Start` 在 `:1137`，`insert` 在 `:1141`） |
| 启动入口（监控） | `open_inline`（内部复用或新建） | `media_page.rs:287-309`、`side_panel.rs:417-439` |
| 生产读取点 | 面板渲染直接读 `w.media.sessions.get(&key)` 并调 `Capture::status/ready/has_ended/is_recording` | `media_page.rs:1006-1030` |
| 进程模型 | 每路 `Capture` = 1 个 ffmpeg（stdout MPEG-TS）+ stderr 收集线程 + 188×7 读包广播线程 + 独立 current_thread tokio runtime 与 axum 服务线程 | `media.rs:113-240`（读包线程 `:233-242`、stderr `:219-230`） |
| 停止 | `stop_capture`：先摘 `MediaUrls` 再 `sessions.remove`（`Drop`：停录制 → `ended` → oneshot shutdown → 杀 ffplay → 杀 ffmpeg → join 录制线程并丢弃末段路径） | `media_page.rs:342-353`、`media.rs:532-548` |
| 绑定对账调用点（**生产 7 处**） | `open_inline` 内、`tick` 内、`run_action` 内、`workspace.rs` 四处（DUT 删除 / 保存 / 每帧 / 连接切换） | `media_page.rs:294`、`:370`、`:1126`；`workspace.rs:2951`、`:3184`、`:6787`、`:8342`。注意 `render_bindings`（`media_page.rs:577`）**不调**对账。既有测试另调 `media_page.rs:1373`、`:1379` |
| 消费者 | 录制（进程内 broadcast 订阅）/ 外置 ffplay / 内嵌预览 / 截图 / MCP 与浏览器，全部从同一个 `capture.url` 各自拉流 | `media.rs:280-326`、`328-372`、`378-459`、`554-580` |

### 1.2 钉住的测试清单（既有，**一行都不改**）

采集域（`apps/desktop/src`）：

| 测试 | 位置 | 钉住什么 | 本设计如何不破坏 |
|---|---|---|---|
| `output_resolution_is_not_sent_to_the_camera` | `media.rs:838` | 输出分辨率只做 `scale+pad` 滤镜，不下发 `-video_size`；`1024×600`（`×`/`x`）、空值 `None`、奇数边与夹带滤镜报错 | 采集仍只走 `Capture::start`；参数构造不进任何抽象层（§2.2） |
| `parses_device_sizes_and_frame_rates` | `media.rs:863` | DirectShow/V4L2 能力解析、排序去重、15/24/25/30/50/60 补齐 | 不动 `parse_video_modes` |
| `capture_stream_outputs_configured_resolution`（ignored） | `media.rs:888` | 真机 1024×600 经 `capture.url` 上 ffprobe 正好 1024×600 | URL 语义不变（§2.3） |
| `inline_preview_decodes_capture_frames`（ignored） | `media.rs:928` | lavfi 采集后内嵌预览每帧恰好 `640*360*4`，首帧前 `poll` 不得返回 error | `InlinePreview` 不改；浮窗共享同一份帧（§6） |
| `inline_preview_reads_configured_capture_device`（ignored） | `media.rs:942` | 真机视频+音频绑定同走内嵌预览 | 同上 |
| `executable_resolves_via_path_absolute_and_env_var`（ignored） | `media.rs:969` | ffmpeg 解析三态 + 未设置变量报变量名 + ffplay 同目录兜底 | `executable()`（`media.rs:592-620`）与 `tool_resolver` 调用序不变（§3.6） |
| `capture_honors_requested_port`（ignored） | `media.rs:1006` | `CaptureInput.port` 非空时 URL 必含 `:{port}/`，固定端口回 `200 OK` + `video/mp2t` | 端口仍由 `CaptureInput.port` 一次性绑定（R3） |
| `capture_shares_audio_and_video_with_two_http_clients_and_recorder`（ignored） | `media.rs:1045` | 同路采集供 2 个 HTTP 客户端 + 录制订阅 + 截图；`record()` 回首段、`poll_recording()` 回末段且相等；PNG magic；落 `<root>/<DUT 名>/` 且有 `.duthub-dut-id` | `record` 两段式返回原样保留（§5.6） |
| `probe_result_is_only_used_by_its_dut_source_and_video_device` | `media_page.rs:1198` | 模式探测缓存键是 `ModeTarget` 四元组 | 不动 |
| `probes_real_capture_device_options`（ignored） | `media_page.rs:1251` | 真机能力探测返回非空分辨率/帧率 | 不动 |
| `saved_video_belongs_to_one_enabled_dut` | `media_page.rs:1271` | 「草稿不能启动采集」；enabled/视频为空视为未绑定；同设备不得被第二个 (dut, source) 占用 | `saved_capture` 保持唯一入口 |
| `directshow_matches_the_exact_endpoint_and_rejects_ambiguous_names` | `media_page.rs:1308` | 同名设备必须按 Alternative name 精确端点消歧 | 端点解析仍在 `start_bound_capture` |
| **`saving_disabled_binding_revokes_old_stream_without_reassigning_it`**（ignored） | `media_page.rs:1335-1385` | `view.sessions.insert(("a".into(), Source::Hdmi), capture)`（`:1352`，`capture: Capture`）、`view.sessions.is_empty()`（`:1380`）、`view.tick(&duts)`（`:1355`）、`view.reconcile_bindings(&duts)`（`:1373`/`:1379`）、`view.urls.lock()` | **`sessions` 的值类型必须仍是 `Capture`；`tick`/`reconcile_bindings` 的签名不变** → §2.4 不做 trait 的直接原因 |
| `monitor_uses_the_active_duts_saved_video_binding` | `side_panel.rs:1102` | 监控源只取当前 DUT 的已保存绑定 | 浮窗复用同一 `monitor_target` |
| `media_fields_edit_and_backfill_per_dut_and_source` | `workspace.rs:9739` | `MediaField` 的 edit/value 往返；ffmpeg/ffplay/listenIp 只写 settings 不挂 DUT；音频可清空 | `idleSecs` **不复用** `MediaField`（§3.5） |

**被既有测试冻结的 `MediaView` 公开面**（改动任何一项都会让上述测试编译失败或断言失败；**新增**方法与字段不受限）：

| 成员 | 形态 | 使用者 |
|---|---|---|
| `pub urls: MediaUrls` | 类型不变 | `media_page.rs:1375`（测试）、`workspace.rs:1532` |
| `sessions: BTreeMap<(String, Source), Capture>` | 值类型必须是 `Capture` | `media_page.rs:1352`、`:1380` |
| `pub fn new(dir: PathBuf) -> Self` | 单参数 | `media_page.rs:1346`、`workspace.rs:1431` |
| `pub fn tick(&mut self, duts: &DutsState) -> bool` | 不接设置、不接配额 | `media_page.rs:1355` |
| `pub fn reconcile_bindings(&mut self, duts: &DutsState)` | 同上 | `media_page.rs:1373`、`:1379` |
| `pub fn open_inline(...) -> Result<(InlinePreview, bool), String>` | 保留 | `side_panel.rs:430` |
| `pub fn close_inline_capture(&mut self, dut_id: &str, source: Source)` | 保留 | `side_panel.rs:480` |
| `pub fn capture_error(&self, dut_id: &str, source: Option<Source>) -> Option<String>` | 保留 | `side_panel.rs:993` |

配置域（`crates/duthub-core`）：

| 测试 | 位置（实测） | 钉住什么 | 对设计的约束 |
|---|---|---|---|
| `settings_example_roundtrip` / `duts_example_roundtrip` | `crates/duthub-core/tests/roundtrip.rs:19-30`（`assert_eq` 在 `:24`） | 整份文档「类型化 → 再序列化」语义相等 | 新增键默认值必须 `skip_serializing_if` |
| `default_tools_settings_serialize_without_download_section` | `crates/duthub-core/tests/tools_download_roundtrip.rs:44-53` | 默认 `ToolsSettings` 序列化恰为 `{"adbExecutable":"adb"}` | `idleSecs` 默认 `None` + `capture_is_empty` 不变 → `tools.capture` 整段仍被跳过（§3.3） |
| `initializes_default_config_beside_executable` | `store.rs:634` | `load_settings == default_settings()` | `default_settings()` 用 `capture: Default::default()`（`store.rs:376`）→ 新字段自动 `None`，**store.rs 不用改** |
| `save_load_roundtrip_and_no_tmp_leftover` | `store.rs:670` | save→load 整体相等 | 默认值必须经 serde 往返 |
| `duts_roundtrip_keeps_media_bindings` | `store.rs:687` | `DutConfig`/`CaptureInput` 字面量逐字段列全；未配置 media 的 DUT 只出现一次 `"media"` | **不给 `DutConfig`/`CaptureInput`/`DutMediaConfig` 加字段** |
| `specs.len() == 3`（单元测试） | `crates/duthub-tooldown/src/spec.rs:154` | 下载工具注册表恰好 3 项（`adb`/`ffmpeg`/`goose`） | 本设计**不加 spec**；ffmpeg 已在表内（`spec.rs:108-114`，`id = "ffmpeg"`） |

### 1.3 既有契约（不可回归）

1. **拉流 URL**：`http://{bind_addr}/live/{Uuid::simple}.ts`；host 来自 `settings.tools.capture.listenIp`（空→`127.0.0.1`，unspecified/多播拒绝，`media.rs:121-132`），端口来自该 DUT/源的 `CaptureInput.port`（空或 0→OS 临时端口，`media.rs:142-143`），路径为随机 UUID（`media.rs:146-147`）。
2. **HTTP 拉流**：单条 `GET {path}` → 200 + `video/mp2t` + `no-store`；并发超过 16 permit 直接 `503`；**无鉴权层**，靠 URL 保密（`media.rs:196-204`、`554-580`）。
3. **截图**：`Capture::screenshot(&self, root, dut_id, name, quota) -> impl FnOnce() -> Result<PathBuf, String> + use<>`；12s 超时、32 MiB 上限，校验 PNG magic 后经 `capture_file`（`open_dut_for_write` + `DutQuota::create`）写 `<root>/<DUT 名>/<uuid>.png`（`media.rs:88-101`、`328-372`）。
4. **录制**：`record()` 立刻返回**首段**路径，后台线程订阅 broadcast 原样写 MPEG-TS，超过 `min(quota/4, 1 GiB)` 轮转；`stop_recording()` + `poll_recording()` 返回**最后一段**（`media.rs:378-435`、`:447-459`）；`Lagged` 视为致命（`media.rs:424`）；**`record` 不设防，直接覆盖 `self.recording = Some(...)`**（`media.rs:433`）。
5. **内嵌预览**：独立 ffmpeg，`fps=10,scale/pad 640:360`、`bgra`，`poll()` 是 **take 语义**（`media.rs:462-530`）。
6. **外置预览**：ffplay；路径 = `settings.ffplay` > ffmpeg 同目录 > PATH（`media.rs:280-326`）；ffmpeg 走 `CREATE_NO_WINDOW`（`media.rs:582-590`），ffplay 靠自建 SDL 窗口。
7. **状态机**：`ready()`/`has_ended()`/`status()` 同时决定按钮分支、URL 是否进 `MediaUrls`（只有 `ready` 才写）、`open_inline` 复用还是重建、监控错误兜底（`media.rs:255-278`、`media_page.rs:376-390`、`:1023-1047`、`side_panel.rs:989-998`）。
8. **读取侧**：`media_streams(dutId)` → `{streams:{hdmi|camera:url}, status}`，status ∈ `streaming`/`bound_not_started`/`not_bound`；DUT 不在快照时只回 `{streams:{}}`（`server.rs:566-592`）。stdio 无流表（`crates/duthub-mcp/src/state.rs:225-232`、`crates/duthub-mcp/tests/stdio_e2e.rs:123-134`）。
9. **未保存不启动**：`saved_capture` 只读 `saved_media`；编辑未保存时面板隐藏 URL 但采集仍在跑（`media_page.rs:180-203`）。

### 1.4 硬约束（本设计必须服从）

- **R1** 不改 `crates/duthub-device` 与 `crates/duthub-control` 的现有文件。
- **R2** 不改 `apps/desktop/src/media.rs`：新能力只组合 `Capture` 的既有构造与 `pub` 方法。
- **R3** URL 与端口不抽象化：不自行分配/改写端口，不做「智能重绑」。
- **R4** 不引入自动重启：停止只在 `stop_capture` 一处，且不自动重起（axum 服务线程从不 join，固定端口「停→立刻起」本就有竞态）。
- **R5** 不改 16 路 fan-out 形态，不改「第 17 路 503」的既有失败方式。
- **R6** 不改编码/容器参数：188 字节 TS 包、1s 强制关键帧、`-muxdelay 0 -flush_packets 1` 保持原样。
- **R7** 既有测试不动（§1.2 全表）；**`MediaView` 的现有公开面按 §1.2 冻结表保持不变**（新增成员允许）。
- **R8** MCP HTTP 运行时是 `worker_threads(2)`（`crates/duthub-mcp/src/transport.rs:345-351`，实测 `:347`）：**任何工具处理器都不得阻塞等待**；跨线程等待只能用 `async` + `tokio::time::timeout`。
- **R9** 只读 grant 不得因本设计获得采集/落盘能力（见 §5.3、§8.3）。
- **R10** 不引入 Python、不引入 go2rtc/MediaMTX/webrtc-rs 等新运行时；不提交 git；UI 只用 gpui-kit；代码注释英文、文档中文。

---

## 2. 仅内置 FFmpeg 的采集执行计划

### 2.1 唯一执行形态

**一路 `(dut, source)` = 一个 `Capture` = 一个 ffmpeg 子进程。** 面板「开始采集」、侧栏监控、Agent 按需取数三条入口全部收敛到 `start_bound_capture`（`media_page.rs:250-281`）：Windows 上先把友好名按当前枚举解析成精确 DirectShow 端点并做跨 DUT 独占校验，再调 `Capture::start`，最后把 `capture.input` 还原为用户配置值。**没有第二种采集进程，没有中继，没有外部服务。**

| 维度 | 事实 | 锚点 |
|---|---|---|
| 谁打开设备 | duthub 自己的 `Capture`（ffmpeg 直接开 DirectShow/V4L2） | `media.rs:104-110`、`:644-678` |
| duthub 侧进程数 | 1 个 ffmpeg + 1 个 axum 服务线程 + 2 个 I/O 线程 | `media.rs:113-240` |
| `capture.url` | 本机 `/live/*.ts` | `media.rs:146-147` |
| 静帧/内嵌预览 | 各自再起一个 ffmpeg 从同一个 `capture.url` 拉流 | `media.rs:328`、`:462` |
| 录屏 | **不重新拉流**：订阅既有 188 字节广播，原样写 MPEG-TS（不额外解码） | `media.rs:378-435` |
| 空闲回收 | 停 `Capture`（= 真停 ffmpeg 进程） | `media_page.rs:342-353` |
| 改绑定生效 | 立即（对账 → `stop_capture`） | `media_page.rs:355-367` |
| 错误可见性 | `error_tail`（stderr 尾部 4 KiB）+ `status()` | `media.rs:219-230`、`:263-278` |

代价（明确写出）：Agent 静帧与内嵌预览都要**再解码一路**（各自再起一个 ffmpeg 从 `capture.url` 拉流）；同一台机器上每路消费者的 CPU 由本机承担。
**录屏是例外，不额外解码**：`Capture::record` 直接订阅既有的 188 字节广播把 MPEG-TS 原样写盘（`media.rs:378-435`），代价是持续占盘而不是 CPU。
这些是纯 FFmpeg 路线的固有成本，本设计不做优化（已知缺口见 §8.6/§8.11）。

### 2.2 ffmpeg 参数如何构造（事实描述，本设计不改）

`Capture::start`（`media.rs:104-110`）只做两件事：按 `input.audio` 是否为空算音频 map（Windows `0:a:0` / Linux `1:a:0`），然后调 `Capture::start_with_args(settings, input, &device_args(input)?, audio)`。参数按以下顺序拼装（全部在 `start_with_args`，`media.rs:113-188`）：

1. **设备输入参数**（`device_args`，`media.rs:644-678`）：
   `-thread_queue_size 16 -f dshow|v4l2`；Windows 追加 `-rtbufsize 512M`；
   `input.frame_rate` 非空时 `-framerate <rate>`；
   `-i` 的取值：Windows `video=<video>`（音频非空则 `video=<video>:audio=<audio>`），Linux 为 `/dev/videoN`；
   Linux 且音频非空再追加 `-thread_queue_size 128 -f alsa -i hw:...`。
   无视频 → 直接报 `captureNeedsVideo`（`media.rs:648-650`）。
2. **前缀**：`-hide_banner -loglevel warning -nostdin`（`media.rs:157-161`）。
3. **视频映射与缩放**：`-map 0:v:0`（`media.rs:161`）；`input.video_size` 非空时追加 `-vf`（`media.rs:162-164`），内容由 `output_resolution_filter` 产出
   `scale={w}:{h}:force_original_aspect_ratio=decrease:force_divisible_by=2,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1`（`media.rs:621-641`）。
   分辨率**只走滤镜**，绝不下发 `-video_size`（`output_resolution_is_not_sent_to_the_camera` 钉住）。
4. **音频**：有 audio → `-map <track> -c:a aac -b:a 128k`，否则 `-an`（`media.rs:165-169`）。
5. **编码与封装**（`media.rs:171-188`）：
   `-c:v libx264 -preset ultrafast -crf 18 -tune zerolatency -pix_fmt yuv420p -force_key_frames expr:gte(t,n_forced*1) -f mpegts -muxdelay 0 -flush_packets 1 pipe:1`。
6. **进程创建**：`command(&ffmpeg)` 在 Windows 加 `CREATE_NO_WINDOW`（`media.rs:582-590`，坑 #31）。

两条必须记住的推论：

- `-c:v …` 固定在**输入参数之后**，所以任何输入源的参数都盖不过它；R6 的「1s 强制关键帧 / TS 包边界 / 输出分辨率」对所有会话一致。若将来要 `-c copy`，必须给 `Capture` 增加「跳过编码参数」的能力 = 改 `media.rs`（R2），见 §8.4（已关闭）。
- ffmpeg 可执行文件的解析在 `media.rs:120`（`executable(&settings.ffmpeg, "ffmpeg")`），顺序是 **绝对路径且存在 > 已下载安装 > `%VAR%` 展开 > PATH**（`media.rs:592-620`）。本设计在把 `CaptureSettings` 交给 `MediaView` **之前**先经 `tool_resolver` 解析一次（§3.6），`media.rs` 一行不改。

### 2.3 拉流地址如何产生

1. **监听 IP**：`settings.tools.capture.listenIp`，空 → `127.0.0.1`；unspecified（`0.0.0.0`）或多播一律拒绝并报「请选择具体的本机 IP」（`media.rs:121-132`）。
2. **端口**：`CaptureInput.port`，空串或 0 → `TcpListener::bind(ip, 0)` 由 OS 选临时端口，随后 `local_addr()` 取回（`media.rs:133-146`）。端口一次性绑定，**不重绑**（R3）。
3. **路径与 URL**：`/live/{Uuid::new_v4().simple()}.ts`，`url = format!("http://{addr}{path}")`（`media.rs:146-147`）。URL 随 `Capture` 存在，`Drop` 即失效。
4. **服务语义**：单条 `GET {path}` 返回 `200 + video/mp2t + no-store`；`Semaphore::new(16)`，第 17 个并发直接 `503`；读包按 188×7 字节块广播（迟到的订阅者从下一个整包开始）；**无鉴权**（`media.rs:196-204`、`:233-242`、`:554-580`）。
5. **可见性**：URL 只在 `capture.ready()` 为真时写进 `MediaUrls`（`media_page.rs:376-390`），未 ready 的会话对外不可见。

### 2.4 桌面侧新增的纯逻辑层（不抽象采集执行）

```
apps/desktop/src/capture_backend.rs   ← lib：账本、空闲判定、artifact 解析（PNG 尺寸/字节数）、错误码（纯函数）
apps/desktop/src/capture_ui.rs        ← lib：浮窗状态机（§6）
apps/desktop/src/media_page.rs        ← MediaView：唯一所有者，调用既有 start_bound_capture / Capture 方法
crates/duthub-core/src/media_op.rs    ← DTO（§5.5）
```

> 录屏登记簿（`recordingId` → 会话/首段/租约）放在 `media_page.rs`（bin）而不是 lib：它的键要用 bin 的 `Source` 类型，且必须与 `sessions` 同一把锁内维护（§5.5）。

**明确不新增**（v4 已删除，勿在实施时复活）：`StreamBackendKind`、`TransportPlan`、`plan_transport`、`resolve_backend_url`、`resolve_tool`、`dut_slug`、`tool_supervisor`、任何 `Box<dyn …Session>`。

**为什么不做 trait / 装箱会话**：`sessions` 的值类型被 `media_page.rs:1352` 的既有测试钉死为 `Capture`（R7），任何 `Box<dyn StreamSession>` 都会让该测试编译失败；而 `Capture` 已经封装了本设计要保留的全部不变量（子进程、axum 服务、188 字节广播、`CREATE_NO_WINDOW`、URL/端口、`Drop` 收尾顺序），再包一层只会把它们复制一遍。

**范围判据**：如果某个新需求要求「第二个采集进程 / 非本机设备 / 另一种传输协议」，它**不在这份文档的范围内**——先按 §8.1 重开评估，再谈抽象；本设计不预留。

### 2.5 进程容器：崩溃/强杀后的孤儿 ffmpeg

Windows 上 `duthub.exe` 在 `fn main()` 的第一条语句把自己放进一个只设 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 的 Job Object（`apps/desktop/src/main.rs` → `duthub_desktop::agent_host::job::contain_current_process`，实现见 `apps/desktop/src/agent_host/job.rs`）。该 Job **不设** `ActiveProcessLimit`/`ProcessMemoryLimit`（不限制整个应用的子进程预算），句柄故意泄漏到进程结束：任何退出路径（正常关闭、`cx.quit()`、崩溃、任务管理器「结束任务」）在句柄关闭时都会终止 Job 内全部进程。`media.rs` 一行不改——ffmpeg/ffplay/预览/录制都是本进程的后代，创建时自动继承 Job 成员关系，因此应用死亡后采集卡不再被孤儿进程占用。

- **嵌套 Job**：goose 子进程仍有自己的 Job（`Job::create`：16 进程 / 单进程 4 GiB / `KILL_ON_JOB_CLOSE`）。Windows 8+ 允许进程同时属于多层 Job（子 Job 的进程集必须是父 Job 的子集），因此 goose 的 Job 嵌套在容器 Job 之下；关闭容器句柄时整个层级的进程一起终止。容器句柄不可继承（`CreateJobObjectW` 不传属性），后代无法把杀死触发器续命。
- **fail-open**：容器 Job 创建或分配失败（例如父启动器 Job 的形态无法再嵌套）只 `eprintln!` 警告，**不阻止启动**——正常退出仍由 `Capture::Drop` 逐进程收尾（§1.1「停止」行）。这与 goose 子进程的 fail-closed 相反：那条路径分配失败即拒绝启动该 Agent 会话。
- **验证**：`job.rs` 单测 `containment_job_only_kills_on_close`（`QueryInformationJobObject` 实测 `limit_flags == 0x2000`、`active_process_limit == 0`、`process_memory_limit == 0`）、`closing_containment_handle_kills_assigned_child`（`cmd /c ping -n 30` 只加入容器 Job，随后只关闭句柄，10 s 内被杀）与 `process_already_in_a_job_can_join_a_nested_job`（嵌套分配实机可用）。三条测试都**不**把测试进程自身放进 Job。

---

## 3. 配置段：扩展现有 `tools.capture`

### 3.1 JSON 形状

```json
{
  "tools": {
    "adbExecutable": "adb",
    "capture": { "ffmpeg": "", "ffplay": "", "listenIp": "", "idleSecs": 90 }
  }
}
```

- 默认：`idleSecs` 键**不存在**（`None`）；`ffmpeg`/`ffplay`/`listenIp` 也为空时 `capture` 段整段省略（`capture_is_empty` 不变）。
- 没有新的顶层段，没有 `stream`/`go2rtc`/`mediamtx`/`externalFfmpeg`。也没有每 DUT 的采集执行配置——DUT 级只有既有的 `CaptureInput`（设备/分辨率/帧率/端口）。

### 3.2 Rust 类型（`crates/duthub-core/src/model.rs`）

```rust
// 现有定义在 model.rs:458-464，capture_is_empty 在 :466-468；只加一个字段。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CaptureSettings {
    pub ffmpeg: String,
    pub ffplay: String,
    pub listen_ip: String,
    /// Idle seconds before an Agent-opened ephemeral capture is reclaimed.
    /// None = DEFAULT_EPHEMERAL_IDLE_SECS (60s). Hand-edited out-of-range values
    /// fall back to the default at read time (effective_idle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_secs: Option<u32>,
}

// 语义不变（仍是「等于默认值」），因此默认文档里 capture 段继续被跳过。
fn capture_is_empty(capture: &CaptureSettings) -> bool {
    capture == &CaptureSettings::default()
}
```

三个必须同时成立的结论：

1. **`Option::is_none` 跳过是硬要求**：`tools.capture` 挂在 `ToolsSettings` 下，而默认 `ToolsSettings` 必须恰好序列化为 `{"adbExecutable":"adb"}`（`crates/duthub-core/tests/tools_download_roundtrip.rs:44-53`）。`None` 不写键 → `capture_is_empty` 仍为真 → 整段跳过。
2. **`capture_is_empty` 不改**：它只比较是否等于默认值；新字段默认 `None` 不破坏该等式。任何「顺手把比较改成逐字段」的改法都会引入行为差异。
3. **`store.rs` 不用改**：`default_settings()` 里是 `capture: Default::default()`（`store.rs:376`），新字段自动为 `None`。

### 3.3 为什么不会炸既有 roundtrip

| 测试 | 锚点 | 为什么仍绿 |
|---|---|---|
| `settings_example_roundtrip` | `crates/duthub-core/tests/roundtrip.rs:19-30` | `docs/examples/settings.json` 实测只有 `"tools": { "adbExecutable": "adb" }`，没有 `capture` 段；默认 `None` 不序列化 → 语义相等 |
| `default_tools_settings_serialize_without_download_section` | `crates/duthub-core/tests/tools_download_roundtrip.rs:44-53` | 默认 `ToolsSettings` 仍是 `{"adbExecutable":"adb"}` |
| `save_load_roundtrip_and_no_tmp_leftover` | `store.rs:670` | `idleSecs: 90` 经 save→load 原样回来（camelCase 键，坑 #15：测试 JSON 必须写 camelCase） |
| `initializes_default_config_beside_executable` | `store.rs:634` | `default_settings()` 与 `load_settings` 都拿到 `None` |
| `duts_roundtrip_keeps_media_bindings` | `store.rs:687` | 本设计不给 `DutConfig`/`CaptureInput`/`DutMediaConfig` 加任何字段 |

`docs/examples/settings.json` 仍旧**一个键都不加**（该文件不含 `idleSecs`）。

### 3.4 校验与钳制（纯函数，落 `apps/desktop/src/capture_backend.rs`）

```rust
pub const IDLE_MIN_SECS: u32 = 1;
pub const IDLE_MAX_SECS: u32 = 3600;
pub const DEFAULT_EPHEMERAL_IDLE_SECS: u32 = 60;

/// Never returns Duration::ZERO. Out-of-range / None values fall back to the
/// default instead of clamping to 1s: a hand-edited typo must not turn into
/// aggressive reclaim.
pub fn effective_idle(capture: &CaptureSettings) -> Duration {
    let secs = capture
        .idle_secs
        .filter(|v| (IDLE_MIN_SECS..=IDLE_MAX_SECS).contains(v))
        .unwrap_or(DEFAULT_EPHEMERAL_IDLE_SECS);
    Duration::from_secs(u64::from(secs))
}

/// Save-time gate: Some(secs) when inside 1..=3600, None otherwise.
pub fn validate_idle_secs(value: u32) -> Option<u32>;
```

- **读路径自带钳制**：手改 `settings.json` 写 `idleSecs: 0` 或 `99999` 时按默认 60s 处理，绝不出现 1 秒的激进回收。
- **校验只在保存动作里调用**（`save_capture_idle`，§3.5），**不放进 `persist_settings`**：后者有 20+ 调用点（`workspace.rs:1805`），放进去会连带拦掉无关设置（如终端字号）的保存。
- 保存期非法值（`0`、`>3600`）→ 面板 toast，**不写盘**，内存与磁盘都保持旧值。

### 3.5 设置页落点与草稿

- 字段放进现有「采集工具」面板（`media_page.rs:444-518`），与 ffmpeg / ffplay / listenIp 同屏，标签「空闲回收（秒）」，占位提示写明「留空 = 60」。
- **不复用 `In::Media(MediaField::*)`**：`MediaField::value` 返回 `&str`、`edit` 返回 `&mut String`（`media_page.rs:55-93`），装不下 `Option<u32>`；且 `media_fields_edit_and_backfill_per_dut_and_source`（`workspace.rs:9739`）钉住既有往返。
- 新增 `In::CaptureIdle`（**unit 变体**：`In` 派生 `Debug, Clone, Copy, PartialEq, Eq, Hash`，`workspace.rs:196-198`；作 `drafts: HashMap<In, String>`（`:1127`）与 `focus_map`（`:1132`）的键，不能带载荷）。**四个同步点**（缺一即「输入无法清空/回跳」，坑 #41 形状）：

| 位置 | 本设计写法 | 参照 |
|---|---|---|
| `In::key()`（`workspace.rs:271`，穷尽 match，编译器强制） | `In::CaptureIdle => "capture-idle"` | `In::SerialScrollback => "serial-scrollback"`（`:300`） |
| `input_value`（`workspace.rs:3387`） | **优先草稿**：`drafts.get(&id).cloned()`，否则 `settings.tools.capture.idle_secs.unwrap_or(60).to_string()` | `In::SerialScrollback`（`:3499-3505`） |
| `input_set`（`workspace.rs:3610`） | **只写草稿**：`self.drafts.insert(id, value)`，不碰模型 | `In::SerialScrollback`（`:3792-3794`） |
| `editor_scope`（`workspace.rs:3891`） | **不加**：工具级设置不随 DUT 变化，落到 `_ => None`（`:3927`） | — |

- 保存动作用专用函数 `save_capture_idle`（照 `save_dut_storage_limit` 的形状，`workspace.rs:1812-1830`）：

```rust
pub(crate) fn save_capture_idle(&mut self) {
    let raw = self.input_value(In::CaptureIdle);
    let trimmed = raw.trim();
    let idle = if trimmed.is_empty() { None } else {
        match trimmed.parse::<u32>().ok().and_then(validate_idle_secs) {
            Some(secs) => Some(secs),
            None => { self.notify_toast(crate::i18n::label("invalidCaptureIdle")); return; }
        }
    };
    let mut candidate = self.settings.clone();
    candidate.tools.capture.idle_secs = idle;
    if let Err(error) = store::save_settings_atomic(&self.data_dir, &candidate) {
        self.notify_toast(format!("{} {error}", crate::i18n::label("saveFailed")));
        return;
    }
    self.settings = candidate;
    self.drafts.remove(&In::CaptureIdle);
    let resolved = duthub_desktop::capture_backend::resolved_capture_settings(&self.settings.tools.capture);
    self.media.set_capture_settings(&resolved);   // 立即生效于尚未到期的空闲计时
}
```

- 面板上「空闲回收」与既有「保存采集设置」按钮**分开**（后者的 handler 仍是 `persist_settings()`，`media_page.rs:499-502`）——两件事的失败语义不同，合并会让一次 ffmpeg 路径保存也去校验空闲秒数。

### 3.6 ffmpeg 路径解析：复用 `tool_resolver`，不改 `media.rs`

```rust
// apps/desktop/src/capture_backend.rs
/// Same contract as tool_resolver's `_in` variants: explicit path/command wins,
/// a downloaded install shadows only the canonical bare name, otherwise the
/// configured value (which may still contain %VAR%) is passed through.
pub fn resolved_capture_settings_in(
    base: &CaptureSettings,
    tools_root: Option<&std::path::Path>,
) -> CaptureSettings;

/// Anchored at `tool_resolver::config_tools_root()`.
pub fn resolved_capture_settings(base: &CaptureSettings) -> CaptureSettings;
```

- 实现（**不要另写一套解析**）：`out.ffmpeg = match tools_root { Some(root) => tool_resolver::effective_spec_in(&base.ffmpeg, "ffmpeg", root), None => tool_resolver::effective_spec(&base.ffmpeg, "ffmpeg") }`；`ffplay`/`listen_ip` 原样保留。`effective_spec_in` 在 `apps/desktop/src/tool_resolver.rs:87-99`，`effective_spec` 在 `:102-109`，`downloads_shadow` 在 `:78-81`。
- 工具 id **核实为 `"ffmpeg"`**：`crates/duthub-tooldown/src/spec.rs:108-114`（`TOOL_FFMPEG`，`id: "ffmpeg"`，`entries` 依次 `ffmpeg.exe`/`ffprobe.exe`/`ffplay.exe`）。`installed_executable` 取 manifest 的**第一个** executable = `ffmpeg.exe`（`crates/duthub-tooldown/src/manifest.rs:102-108`），所以 ffplay 由 `media.rs` 的「ffmpeg 同目录」兜底自然命中（spec 注释 `spec.rs:104-106` 明确这一顺序约束）。
- 复位语义（三条都要写进实现注释）：
  1. 显式配置了绝对路径/其它命令名 → `downloads_shadow` 为假 → 原样返回，交给 `media.rs` 的 `executable()` 判存在/展开 `%VAR%`（`media.rs:592-620`）；
  2. 配置为空或就是裸名 `ffmpeg` 且有已下载安装 → 返回**绝对路径**；
  3. 既无配置也无下载 → 返回裸名 `ffmpeg`（= `media.rs` 的默认值）。
- **幂等**：把解析结果再喂一次，第 1 条命中（绝对路径不是裸名），结果不变。
- **时机**：在**构造**交给 `MediaView` / `Capture` 的 `CaptureSettings` 之前解析（§3.7 的 `set_capture_settings` 调用点、`side_panel` 的 `open_inline` 调用点），因此 `media.rs` 一行不改；`media.rs:601` 自己也有一层 `downloaded_preferred` 兜底，两层顺序一致、不冲突。
- **ffplay 不单独解析**：没有 `ffplay` 的 tool id；同目录兄弟文件规则由 `media.rs:288-300` 负责。

### 3.7 注入 `MediaView`

`MediaView::new(dir)`、`tick`、`reconcile_bindings` 的签名都冻结（§1.2），所以外部输入用**写入口**注入：

```rust
pub fn set_capture_settings(&mut self, capture: &CaptureSettings);  // 新公开方法
pub fn capture_settings(&self) -> &CaptureSettings;                  // 新公开方法
pub fn set_artifact_quota(&mut self, quota: Arc<DutQuota>);          // 新公开方法
```

- `MediaView` 新增字段：`capture: CaptureSettings`、`quota: Option<Arc<DutQuota>>`（**没有** `stream` 字段）；账本与录屏登记字段见 §4.2。
  `capture` 是 **AgentOp 路径的必需项**：`Capture::start` / `start_with_args` 都要 `&CaptureSettings`（`media.rs:104`、`:113`），而 `tick(&DutsState)` 与 `drain_media_ops` 手里只有 `DutsState`——没有这个字段，Agent 起的会话根本拿不到 ffmpeg 配置。
- **不新建 `DutQuota`**：`DutQuota` 带全局上限状态（`workspace.rs:1825` 的 `set_limit`），自建会绕开配额；必须用 `Workbench` 持有的同一个 `artifact_quota`（`workspace.rs:1319`/`:1601`）。
- 调用点（`Workbench::new`，`MediaView::new` 在 `workspace.rs:1431` 之后、结构体字面量之前）：

```rust
let mut media = crate::media_page::MediaView::new(artifact_root.clone());
media.set_capture_settings(
    &duthub_desktop::capture_backend::resolved_capture_settings(&settings.tools.capture),
);
media.set_artifact_quota(artifact_quota.clone());
```

- 监控入口换数据源（签名不变，仍是 `&CaptureSettings`）：`side_panel.rs:430-431` 的 `&self.settings.tools.capture` 改为 `self.media.capture_settings()`；`render_bindings` 的「枚举设备」用 `self.settings.tools.capture.clone()`（`media_page.rs:511-516`）保持原样即可（设备枚举不需要已解析路径）。
- **面板「开始采集」也换数据源**：`run_action` 里的 `let settings = &w.settings.tools.capture;`（`media_page.rs:1135`，随后在 `:1140` 传给 `start_bound_capture`）改成取 `MediaView` 的已解析副本。否则同一次采集从面板与从 Agent 两条入口会拿到不同的 ffmpeg 解析结果（面板漏掉已下载安装/绝对路径），表现为「Agent 能起、面板起不来」。
- **借用形状（坑 #14 同族，照抄否则编译不过）**：`capture_settings()` 借的是 `self.media`，而 `open_inline` / `start_bound_capture` 要 `&mut self.media`——**先 clone 成局部变量再传**：

```rust
// side_panel.rs start_monitor
let settings = self.media.capture_settings().clone();
let result = self.media.open_inline(&self.duts, &dut_id, source, &settings);

// media_page.rs run_action（Action::Start 分支）
let settings = w.media.capture_settings().clone();
let capture = w.media.start_bound_capture(&w.duts, key, input, &settings)?;
```
- `drain_media_ops` 用 `self.quota.clone().ok_or(MEDIA_CAPTURE_UNAVAILABLE)`；`quota` 为 `None` 时 op 一律失败，绝不 panic。
- **生效语义**：`idleSecs` 对**尚未到期**的计时立即按新值重算（`effective_idle` 每 tick 现算）；`capture.ffmpeg` 只影响**之后启动**的会话——已运行的 `Capture` 进程已定，面板在有活动会话时显示「路径变更需重启采集」提示（不自动重启，R4）。

---

## 4. 按需启动与空闲回收

### 4.1 租约种类与来源

| 租约 | 持有者 | 今天的等价行为 | 释放时机 | 是否 pin |
|---|---|---|---|---|
| `Manual` | 面板「开始采集」（`Action::Start`） | 一直跑，直到用户停止/对账判 stale | 「停止采集」、绑定变更、DUT 删除 | **是**（永不自动回收） |
| `Monitor` | 侧栏监控预览 | `close_inline_capture`（非录制即停） | 关面板、切目标、预览报错 | 否 |
| `ExternalPreview` | 「外置预览」ffplay | 不阻止停采 | 用户关 ffplay（不可感知）→ **定时过期** | 否（Hold） |
| `CopyUrl` | 「复制 URL」 | 无跟踪 | **定时过期**（`HOLD_SECS` 默认 300s，每次复制刷新） | 否（Hold） |
| `Recording`（**不是租约**） | `Capture::record`。两个来源：面板「录屏」与 Agent `recordStart` | 录制中不释放 | `stop_recording` + `poll_recording`（手动由面板停止，Agent 由配对的 `recordStop` 触发） | 是（录制期间，经 `DemandEntry.recording` 镜像，§4.2） |
| `AgentOp` | `media_streams.op` 的 `snapshot` / `recordStart` | **今天不存在** | `snapshot`：完成/失败/被停采打断；`recordStart`：**由配对的 `recordStop` 释放**（中途停采则随 `stop_capture` 一并释放） | `snapshot`：否；`recordStart`：**是**（`recordStart`→`recordStop` 期间经 `DemandEntry.agent_recording`，§4.2/§5.6） |

### 4.2 账本（`MediaView` 新增字段，不改既有字段类型）

```rust
// apps/desktop/src/capture_backend.rs（lib，纯逻辑）
pub type LeaseId = u64;
use duthub_core::media_op::RecordingId;

/// Leases only. "Recording" is a *state of the Capture*, not a lease — it is
/// mirrored into `DemandEntry::recording` by `tick`, because `decide` must be
/// able to see it without touching `Capture`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemandKind { Manual, Monitor, ExternalPreview, CopyUrl, AgentOp }

impl DemandKind {
    pub fn is_pin(self) -> bool { matches!(self, Self::Manual) }
    pub fn is_hold(self) -> bool { matches!(self, Self::ExternalPreview | Self::CopyUrl) }
}

#[derive(Debug, Default, Clone)]
pub struct DemandEntry {
    leases: BTreeMap<LeaseId, DemandKind>,
    /// Hold leases expire on their own so a lost ffplay/CopyUrl can never pin forever.
    holds: BTreeMap<LeaseId, Instant>,
    /// Set when the last non-pin lease is released, cleared on any acquire.
    idle_since: Option<Instant>,
    /// True when the session was created solely by an AgentOp (ephemeral).
    pub ephemeral: bool,
    /// Mirrored from `Capture::is_recording()` on every tick: a recording (manual
    /// or Agent) pins the session exactly like `DemandKind::Manual` does.
    pub recording: bool,
    /// Set while an Agent recording owns the recording slot (§5.5). Holds the
    /// `RecordingId` so `recordStop` attribution and the manual-record mutex are
    /// decided from one place. Cleared by `recordStop`, `stop_capture`, or timeout.
    pub agent_recording: Option<RecordingId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclaimDecision { Keep, StopNow, Reclaim, Pinned, NotLeased }

impl DemandEntry {
    pub fn acquire(&mut self, id: LeaseId, kind: DemandKind, now: Instant);
    pub fn acquire_hold(&mut self, id: LeaseId, kind: DemandKind, now: Instant, ttl: Duration);
    pub fn release(&mut self, id: LeaseId);
    pub fn sweep_expired(&mut self, now: Instant);          // holds / 过期租约
    pub fn has_pin(&self) -> bool;
    pub fn is_empty(&self) -> bool;
    /// `Pinned` when `has_pin() || recording || agent_recording.is_some()`;
    /// otherwise the table in §4.3. There is no time-based unlock for a
    /// recording: only `recordStop`/`stop_capture` clear it.
    pub fn decide(&self, now: Instant, idle: Duration) -> ReclaimDecision;
}
```

`MediaView` 新增字段（**不改 `sessions` 类型**）：

```rust
pub struct MediaView {
    pub urls: MediaUrls,                                  // 不变
    dir: PathBuf,                                         // 不变
    sessions: BTreeMap<(String, Source), Capture>,         // 不变（测试钉住）
    statuses: BTreeMap<(String, Source), String>,          // 不变
    // 新增：
    demand: BTreeMap<(String, Source), DemandEntry>,
    recordings: BTreeMap<RecordingId, ActiveRecording>,   // §5.5 在录的 Agent 录屏
    aborted: BTreeMap<RecordingId, Instant>,              // §4.5 停采墓碑（10min TTL）
    next_recording_id: RecordingId,                       // 单调递增，从 1 起
    capture: CaptureSettings,                             // §3.7 set_capture_settings（已解析 ffmpeg）
    quota: Option<Arc<DutQuota>>,                         // §3.7 set_artifact_quota（不新建）
    ops: MediaOpPort,                                     // §5.5 的通道端
    // 其余字段不变
}
```

`demand` 与 `sessions` 的键集合始终同步：`stop_capture` 同时清掉两边的条目（§4.5），`ensure_session` 同时插入。`recordings` 与它们不同构（一个 key 至多一条活动录屏，另有与 key 无关的 `aborted` 墓碑），但每条 `ActiveRecording.key` 必须指向一个现存 `sessions` 条目——`stop_capture` 负责把它摘掉并把 id 移进墓碑。`tick` 在算回收前先刷新镜像状态：

```rust
for (key, entry) in self.demand.iter_mut() {
    entry.recording = self.sessions.get(key).is_some_and(Capture::is_recording);
}
```

转移表：

| 当前 | 事件 | 结果 |
|---|---|---|
| 空 | `acquire(Manual)` | 存活，`Pinned` |
| 空 | `acquire(AgentOp)`（`ephemeral = true`） | 存活 |
| 非空 | `release` 后仍非空 | 存活 |
| `Interactive` 且非空 | 最后一个非 pin 租约 release | `StopNow`（= 今天的 `close_inline_capture`） |
| `Ephemeral` 且非空 | 最后一个租约 release | 记 `idle_since`，`Keep` |
| 空 + `idle_since` | `now >= idle_since + idle` | `Reclaim` |
| 任意 | `has_pin()`、录制中（手动或 Agent）或 `agent_recording.is_some()` | `Pinned` |
| 任意 | hold 到期 | `sweep_expired` 先移除，再按上面判 |

**录屏没有时间上限，因此没有「到点解锁」这一行**：`Pinned` 只在 `recordStop`（`stop_recording()` + `poll_recording` 收尾）或 `stop_capture` 之后才解除。会话被 pin 的时长 = 调用方迟迟不 `recordStop` 的时长，这是 §5.6 明确接受的代价（配额轮转兜住磁盘）。

### 4.3 与「用户手动启停」的兼容（逐条对齐今天）

| 今天的可观察行为 | 新模型下的实现 |
|---|---|
| 手动开始 → 一直跑 | `Manual` pin（`ReclaimDecision::Pinned`） |
| 手动停止 → 立即停 | `stop_capture`（清 `demand` + 回绝在途 op） |
| 开监控 → 采集启动；关监控 → 立即停（非录制） | `open_inline` 取 `Monitor` 租约；`close_inline_capture` 释放 → `Interactive` 走 `StopNow` |
| 录制中关监控 → 不停 | `demand[key].recording`（tick 从 `Capture::is_recording()` 镜像） → `Pinned` |
| 编辑绑定未保存 → 不停 | `reconcile_bindings` 判据仍是 `capture.input`（用户配置值，`media_page.rs:359-361`） |
| 保存停用/改绑 → 立即停 | 7 个对账调用点全部经 `stop_capture` |

**默认值不改行为**：默认 `idle_secs = None`（= 60s），而空闲计时的 `Reclaim` 分支**只对 `ephemeral` 会话生效**——今天所有会话都是 `Interactive`（手动开始、监控），所以默认配置下不存在任何自动回收，与今天逐字相同。手动会话永远 `Pinned`。

### 4.4 与 `open_inline` / `close_inline_capture` 的租约配对（修 v1 的泄漏）

- `open_inline`：无论复用还是新建，都在返回前 `acquire(Monitor)`（同一 `(dut, source)` 的 `Monitor` 租约至多一个，重复调用幂等）。返回类型不变 `(InlinePreview, bool)`。
- `close_inline_capture`：`release(Monitor)` → 若判 `StopNow` 才 `stop_capture`。
- **`side_panel.rs` 的 `stop_preview`（`side_panel.rs:475-482`）必须总是调用 `close_inline_capture`**（在非 `Floating` 时），不能只在 `owns_capture` 时调：`owns_capture=false` 的会话由别处（面板 Manual）持有 pin，释放 `Monitor` 不会停它；反过来若只在 `owns` 时释放，就会永久泄漏 `Monitor` 租约（会话卡在「有人在看」）。**`Floating` 时例外**：预览与租约由浮窗持有，`stop_preview` 必须走 `FramePump::release_monitor` 的 no-op 分支（§6.3），否则会关掉浮窗画面。
- 面板「停止采集」在监控打开时执行 `stop_capture` → `demand` 整条移除 → 预览侧下一帧 `poll` 得到退出错误 → `stop_preview` 再调 `close_inline_capture`（对已移除的 key 为 no-op，幂等）。

### 4.5 回收执行点（唯一落点）与停采的收尾

`MediaView::tick`（50ms，`workspace.rs:1726-1744`，`media.tick` 在 `:1734`）里：

```rust
pub fn tick(&mut self, duts: &DutsState) -> bool {
    self.reconcile_bindings(duts);
    self.drain_media_ops(duts);        // 取请求、pick_source、ensure_session、recordStart
    self.sweep_recording_stops();      // recordStop 超过 RECORD_STOP_GRACE → MEDIA_OP_TIMEOUT
    self.mirror_recording_state();     // demand[key].recording = capture.is_recording()
    let now = Instant::now();
    let idle = duthub_desktop::capture_backend::effective_idle(&self.capture);
    let expired: Vec<_> = self.demand.iter()
        .filter_map(|(key, e)| (e.decide(now, idle) == ReclaimDecision::Reclaim).then(|| key.clone()))
        .collect();
    for key in expired { self.stop_capture(&key); }
    // 既有 statuses / urls / poll_recording / messages 逻辑不变；唯一改动是 poll_recording
    // 拿到结果时先调 finish_recording(&key, result)：命中活动录屏 → 完成 recordStop，
    // 否则落回既有「录屏已保存到 …」面板消息。
}
```

`stop_capture` 是**唯一**收尾路径（7 个对账调用点、`Action::Stop`、tick 回收、DUT 删除都汇入它），它必须按固定顺序做四件事：

1. `self.ops.abort_all_for(key, MEDIA_CAPTURE_ENDED)`：把该 `(dut, source)` 上的在途 `snapshot`/`recordStart` 立即以 `Err(MEDIA_CAPTURE_ENDED)` 回绝；活动录屏的 `recordingId` 移入 `aborted`（10 分钟 TTL）——之后配对的 `recordStop` 回 `MEDIA_RECORDING_ENDED`，**不返回任何 artifact**（`MediaOpReply` 的错误分支没有路径/分段字段，见 §5.5），已写出的分段留在 DUT 目录由配额与清理统一处理；
2. 若录制线程仍在跑：`capture.stop_recording()` 并**不等待**——`Drop`（`media.rs:532-548`）会 join 录制线程并**丢弃**末段路径，所以「被打断的录屏」在契约上一律是失败，绝不伪装成成功；
3. 摘 `MediaUrls`、`sessions.remove`、`demand.remove`、`recordings.remove`（`Capture::Drop` 完成进程收尾）；
4. 面板消息按「谁拥有这次录制」分流（§5.6）。

**为什么必须在这里回绝**：`Capture::Drop` 会 `join` 录制线程但**丢弃** `JoinHandle` 的结果，末段路径无法再取回；若不预先回绝，录屏的 `recordStop` 永远等不到结果，MCP 侧只能吃满超时，而调用方也无法区分「被停采打断」与「id 不存在」。

### 4.6 诚实边界

- **外部 HTTP 读者对账本不可见**：内置 `pull` 只持 `Semaphore` permit（`media.rs:555-557`），没有读者计数/最后读者时间。用 `available_permits()` 反推需要给 `Capture` 加访问器（改 `media.rs`，R2），v1 不做（§8.6）。
- 因此用 `CopyUrl`/`ExternalPreview` 的 **hold 租约（默认 300s，可刷新）** 兜住「用户复制了 URL / 开了 ffplay」的场景：过期后回收，绝不永久 pin。这与今天「复制 URL 后停止采集同样会断流」一致或更保守。
- MCP 返回的 URL 由调用方的 `AgentOp` 租约兜住（查询分支不延长租约：会话在最后一个租约释放后按 `idleSecs` 回收；`recordStart` 分支则持有租约直到 `recordStop`，§4.1）；工具描述继续写「URL 需保密、地址在本次采集期间有效」。

---

## 5. Agent 按需取数

### 5.1 为什么主线是「按需静帧 + 起停配对的录屏」而不是长连视频流

1. LLM 侧收益为零：Agent 消费的是文本/文件证据（`serial_read`、日志段、PNG、TS 片段），长连视频不可消费，只是延长带宽与进程常驻。
2. 资源账：一路采集已是常驻 ffmpeg + 188×7 广播；静帧与内嵌预览各再起一个解码 ffmpeg。Agent 场景是「偶发看一眼」，长连观看等于永久占一个 `Semaphore(16)` 名额与本机 CPU。录屏不额外解码（直接订阅广播写 TS，§2.1），代价转移到磁盘。
3. 既有能力足够：截图走 `Capture::screenshot`（12s / 32 MiB 上限、PNG magic、DUT 归属落盘）；录屏走 `Capture::record`/`stop_recording`/`poll_recording`（`media.rs:378-459`），面板「录屏」按钮已长期使用这条路径。
4. 上下文成本：静帧/录屏只返回路径 + 字节数（必要时再 `dut_file_read` 按需取内容），文本面可控。
5. 失败面：静帧是一次有界超时；录屏拆成**两次各自有界**的调用（`recordStart` 30s、`recordStop` 30s），**录制时长本身不设上限**——Agent 想录多久就录多久，但没有任何一次 MCP 调用会无限挂着。长连流的失败则是「流悄悄断了」，MCP 只能读到 URL 字符串。

结论：**查询（已有）+ 按需静帧 + `recordStart`/`recordStop` 配对录屏** 是主线；长连视频继续作为人类/外部工具能力（`media_streams` 返回 URL），不进 Agent 主循环。

### 5.2 工具面：扩展既有 `media_streams`，不新增工具

新增工具会触碰 4 处被既有测试钉住的数量契约：
`apps/desktop/tests/agent_cli.rs:520-532`（把 listener 的 `tools/list` 与 `cli::PLAN_TOOLS` 做**等值**比较）、同文件 `:667`（`#[ignore]` 测试里硬编码 `"duthub 工具 13 个"`）、`apps/desktop/tests/plan_badges.rs:82`/`:97`（locale 文案 `"13 个 Plan 工具（只读）"` / `"13 Plan tools"`）、`crates/duthub-mcp/tests/dut_binding.rs:34`（`[&str; 13]`）。所以把能力挂在既有工具上（§8.7 已定）。

```jsonc
// 查询（现状，逐字不变）
{"dutId":"dut-a"}

// 静帧
{"dutId":"dut-a","op":{"kind":"snapshot","source":"hdmi","note":"boot-logo"}}

// 开始录屏（不传时长；立即返回 recordingId）
{"dutId":"dut-a","op":{"kind":"recordStart","source":"camera","note":"boot-banner"}}

// 结束录屏（凭上一步的 recordingId 取产物）
{"dutId":"dut-a","op":{"kind":"recordStop","recordingId":7}}
```

```rust
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MediaStreamsArgs {
    pub dut_id: String,
    /// Absent = status query; response shape is byte-for-byte today's.
    #[serde(default)]
    pub op: Option<MediaOpArgs>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields, tag = "kind")]
pub enum MediaOpArgs {
    Snapshot { #[serde(default)] source: Option<String>, #[serde(default)] note: Option<String> },
    /// Starts a recording. No duration field: a recording ends only when the
    /// paired `RecordStop` arrives (or the capture is stopped).
    RecordStart { #[serde(default)] source: Option<String>, #[serde(default)] note: Option<String> },
    RecordStop { recording_id: u64 },
}
```

出参（**op 响应不含 `streams`/`status`**，避免 `{"streams":{},"status":"streaming"}` 这种不自洽组合）：

| 路径 | 响应 |
|---|---|
| 无 `op` | **与今天逐字一致**：`{streams:{hdmi?,camera?}, status?}`；DUT 不在快照时只有 `{streams:{}}`（`stdio_e2e.rs:123-134` 断言不变） |
| `op=snapshot` | `{artifact:{path,bytes,width,height,note?,source}}` |
| `op=recordStart` | `{recording:{recordingId,source,firstPath,note?,startedAtMs}}`（`firstPath` = `record()` 立刻返回的首段，`startedAtMs` = UNIX 毫秒） |
| `op=recordStop` | `{artifact:{path,firstPath,bytes,rotated,durationSecs,note?,source}}` |

- **`segments[]` 全列出不可行**：`record()` 只返回首段、`poll_recording()` 只返回末段，轮转时中间段路径被 `path = new_path` 覆盖丢弃（`media.rs:401-409`），无法枚举。因此录屏只报 `firstPath`（首段）与 `path`（末段）＋ `rotated: bool`；`bytes` 是**末段**文件大小（§8.11）。
- **`recordStart` / `recordStop` 必须配对**：`recordStart` 立即返回（`Capture::record` 同步拿到首段路径，`media.rs:378`），GUI 登记 `recordingId`；`recordStop` 用同一个 id 换产物。**没有被 `recordStop` 消耗的 id 不会自动回收**——录屏会一直持续到 `recordStop`、`stop_capture`（面板停止/绑定变更/DUT 删除）或进程退出。这是本设计明确接受的代价（§5.6 最后一行）。
- **没有 `seconds`、没有时间上限**：schema 不带任何时长字段；`deny_unknown_fields` 会把传了 `seconds` 的请求判 `invalid_params`（旧调用方立刻收到明确错误，而不是被静默忽略）。文档里**不存在**「录到 N 秒自动停」的路径，也不存在 `clamp_clip_seconds`/`MIN_CLIP_SECS`/`MAX_CLIP_SECS`/`CLIP_DEADLINE`。
- **录屏期间会话被 pin**：`DemandEntry.recording` 镜像 `Capture::is_recording()`（§4.2），所以即使没有别的租约，ephemeral 采集在录屏期间也不会被空闲回收；`recordStop` 收尾后才开始计空闲。
- **`recordingId` 的 DUT 归属有校验**：`recordStop` 的 `recordingId` 必须与该请求的 `dutId` 一致，否则回 `MEDIA_RECORDING_UNKNOWN`（不泄露别的 DUT 是否存在该 id，§5.3）。
- **配额耗尽 = 存储边界，不是时间上限**：段轮转阈值仍是 `min(quota/4, 1 GiB)`（`media.rs:401-402`）。起始 `capture_file` 失败（配额已满）→ `recordStart` 回 `MEDIA_CAPTURE_FAILED`；录制中途轮转写盘失败 → 录制线程返回 `Err`、`poll_recording` 带回该错误 → `recordStop` 回 `MEDIA_CAPTURE_FAILED`（message 含原始错误串）。两种情况都**不**返回部分产物，也不改变「没有时长上限」这条契约。
- **没有 `partial` 字段**：被停采打断的录屏一律是 `Err(MEDIA_RECORDING_ENDED)`（§4.5）；成功响应只表示「`recordStop` 拿到了完整收尾的末段」（§5.6）。
- **同一源重复 `recordStart`**：该 `(dut, source)` 已在录（Agent 录屏或用户手动录制）→ `MEDIA_RECORDING_IN_PROGRESS`。
- 需要 URL 的调用方仍走无 `op` 的查询；**新起的 ephemeral 采集在下一 tick 之前不会出现在 `MediaUrls`**（`media_page.rs:376-390` 只在 `ready` 时重建），op 响应因此不承诺 URL。
- `source` 省略时的选择在 **GUI 的 `drain_media_ops` 里**执行（`McpState` 只有 `has_media_binding`/URL 表，见 `state.rs:225-232`，MCP 侧无法按源判定）：`pick_source(duts, dut_id, requested)` 是 `media_page.rs` 内的 bin 函数（**不是 lib**：`DutsState` 是 bin 类型，`main.rs:13` 的 `mod data;`、`media_page.rs:14` 的 `use crate::data::DutsState`），输入是 `DutsState::saved_media(dut_id)` 的**已保存快照**（`data.rs:207`，与 `saved_capture` 同源，`media_page.rs:180-203`），规则 = 指定的源若已绑定则用之 → 否则 `hdmi` → `camera` 中第一个已绑定者 → 都未绑定则 `MEDIA_NOT_BOUND`。**绝不复用「当前选中设置项」**，也绝不读未保存的草稿。

### 5.3 授权：op 需要新动作 `DutAction::Capture`

`media_streams` 在 Plan（只读）面内（`server.rs:570-591`、`plan_tool_allowed` 13 项，`server.rs:1320`），若 `op` 只要 `Read`，任何只读 Token 就能开摄像头、spawn ffmpeg、往 DUT 目录写文件并占配额。

设计：**新增 `DutAction::Capture`**（`crates/duthub-core/src/agent_access.rs:126-147` 的 7 个变体之外再加一个，serde 名 `capture`）：

| 项 | 内容 |
|---|---|
| 谁需要它 | 只有 `media_streams{op:…}` 分支；查询分支仍只要 `Read` |
| 检查 | `self.authorize(principal, &args.dut_id, DutAction::Capture)`（复用既有 `authorize`，DUT 专用监听器的 bound 前置检查天然生效） |
| `recordStop` 的归属校验 | 先 `authorize(Capture)`，再查 `recordings[recordingId].key.0 == args.dut_id`；不匹配回 `MEDIA_RECORDING_UNKNOWN`（与「id 不存在」同码，不给跨 DUT 探测口） |
| 默认 | 无任何凭据拥有 `capture` → deny by default，只读 Token 得到 `LeaseRequired`/`UNAUTHORIZED` |
| 同步点（坑 #43 族） | ① `DutAction::as_str`（编译器强制的穷尽 match，`Capture => "capture"`、`Unknown => "unknown"`）；② `authorize_any_write`（`server.rs:460-479`）**不加**——capture 不是控制写动作，不该能换控制租约；③ `apps/desktop/src/pages.rs:4156` 的 `GRANT_ACTIONS` 加一项（`(DutAction::Capture, "画面")`，当前是 `[(DutAction::Read, "读")]`），否则用户无法在 UI 勾选；④ `docs/API_CONTRACT.md` + `docs/SECURITY.md` + `43c58dc:docs/agent/TASKS.md`；⑤ `apps/desktop/tests/*` 若有断言 grant 动作列表需核对（本会话未发现按列表等值断言的测试） |
| **前向兼容（必做）** | `DutAction` 加 `#[serde(other)] Unknown` 变体（unit-only 枚举可用）。否则「新版本授予过 `capture` → 回退旧版本」会让 `load_settings` 的 `serde_json::from_str::<SettingsFile>` 失败，走 `store.rs:405-419` 的**改名备份 + 整体报错**——等于用户回退一版就丢配置。`Unknown` 不参与任何 `authorize` 匹配（自然 deny），并在加载期从 `grants[].actions` **过滤掉**（不写回磁盘），`GRANT_ACTIONS` 不展示它 |
| 证据面 | 不进 `EVIDENCE_TOOL_NAMES`（`server.rs:1353-1362`），`media_streams` 继续被 `auth_hardening.rs:315-320` 断言 `EVIDENCE_SCOPE_ONLY` |
| 审批 | 不进 `approval_request` 白名单（`server.rs:1244`），不需要控制租约 |
| 审计 | `call_tool` 只记截断后的 `dutId`（`server.rs:1491-1497`）；`note` 不进审计 |

- `DutAction` 是 `#[serde(rename_all = "camelCase")]` 的 `Copy/Eq` 枚举（`agent_access.rs:124-134`），新增 `Capture` + `Unknown` 对既有配置向后兼容（旧文件不含这两个值），对 `tests/*` 的显式 `vec![DutAction::Read]` 无影响。
- **签字状态：已签字（2026-11-24 用户指示 Agent 可控制截图与录屏）**——见 §8.3。代码按 grant 正常裁决，**不**加任何「代码里一律 UNAUTHORIZED」的屏蔽；默认 deny 仍保证未授予凭据无此能力。

### 5.4 落盘位置与 artifact 元数据

- 与截图/录屏同源：`<artifact_root>/<DUT 名>/<file>`，经 `capture_file`（`open_dut_for_write` + `DutQuota::create`，`media.rs:88-101`）。`artifact_root` = `RuntimeMcpState::dut_artifact_root()`（`mcp_runtime.rs:174-176`，即 `work_dir` 的父目录）。
- **`width`/`height` 的数据来源**：`Capture::screenshot` 只返回 `PathBuf`（`media.rs:328-334`）。GUI 侧在 job 完成后读文件头 24 字节并用纯函数解析 PNG IHDR：

```rust
/// PNG IHDR: bytes 16..20 = width, 20..24 = height (big-endian).
pub fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)>;
/// `bytes` = fs::metadata(path).len().
pub fn artifact_bytes(path: &Path) -> Option<u64>;
```

  两者都有单测（§7.1）。`width/height` 缺失（文件异常）时字段省略，**不伪造**。
- 录屏：`path` = 末段、`firstPath` = 首段、`rotated = firstPath != path`、`bytes` = 末段大小；中间段不可枚举（§5.2 首条）。`durationSecs` = **实测墙钟秒数**（从 `record()` 返回的 `started_at` 到 `poll_recording()` 交出末段的整秒差），**不是**请求值（现在也没有请求值）——被停采打断的录屏根本不会走到这里（§4.5），所以 `Ok` 的 `durationSecs` 与「真的录了多久」一致，可以远超一分钟。落地后不做 ffprobe 复测（测试里可复测，见 §7.3）。
- **录屏的 `note` 在 `recordStart` 时固定**：同一份 `ActiveRecording.note`，原样回显在 `recording.note` 与最终 `artifact.note`；`recordStop` 不接受 `note`（`deny_unknown_fields` 会拒）。
- **配额是录屏的唯一边界**：`capture_file` 的 `DutQuota::create` 可能因配额失败（`media.rs:88-101`）；`Capture::record`/录制线程只返回 `Result<_, String>`（R2），错误串原样带回，`data.code` 一律 `MEDIA_CAPTURE_FAILED`——这是**存储边界**，不是时长上限，也不单独设 `MEDIA_QUOTA`（§5.7）。
- **`note` 语义**：`note` **只做元数据**——① 限长 64 字符、去控制字符后原样回显在 `artifact.note`；② 作为人类可读备注。**不做文件名前缀**：`capture_file` 硬编码 `{uuid}.{ext}`（`media.rs:95`），`Capture::screenshot` 不收前缀，事后 `rename` 会绕开 `DutQuota::create` 的记账（R2 与配额红线）——所以这条能力**不存在**。`note` 不写审计（`call_tool` 只记 `dutId`）、不写 DUT 日志。

### 5.5 执行时序（non-blocking 桥，修 2 worker 阻塞）

MCP 运行时只有 2 个 worker（`transport.rs:347`），而工具处理器在 async 任务里被分发（`server.rs:1484-1506`）。**任何同步阻塞等待都会在最坏情况下堵死整个端点**（含审批与 `serial_read`）。因此：

- 工具方法改为 `async fn`（rmcp 3.10 的 `#[tool]` 支持 async：宏把 `async fn` 改写成 `Pin<Box<dyn Future + Send + '_>>`）；
- 桥用 **`tokio::sync::oneshot`**：MCP 侧 `.await` 接收端并包 `tokio::time::timeout`；GUI 侧用 `Sender::send()`（不依赖 reactor，可在 GUI 线程直接调；`Err` 仅表示接收端已丢弃）；
- 请求队列用 `std::sync::mpsc::sync_channel(8)`（GUI 侧 `try_iter` 非阻塞）。

**DTO 必须放在 `crates/duthub-core`**（新增 `duthub_core::media_op` 模块）：依赖方向是 `duthub-desktop → duthub-mcp → duthub-core`（`apps/desktop/Cargo.toml:29`、`crates/duthub-mcp/Cargo.toml:18`），把 DTO 放桌面 lib 会让 `duthub-mcp` 反向依赖 `duthub-desktop` = cyclic package dependency。`duthub-core` 的 tokio 已带 `sync` feature（`crates/duthub-core/Cargo.toml:22`），`tokio::sync::oneshot` 可直接用。

```rust
// crates/duthub-core/src/media_op.rs（新模块；core 只放 DTO 与纯枚举，不含 MediaView）
pub type OpId = u64;
pub type RecordingId = u64;
pub type OpReceiver = tokio::sync::oneshot::Receiver<MediaOpReply>;

#[derive(Debug)]
pub enum MediaOp {
    Snapshot { note: Option<String> },
    /// No duration field by design: a recording runs until the paired `RecordStop`.
    RecordStart { note: Option<String> },
    RecordStop { recording_id: RecordingId },
}

pub struct MediaOpRequest {
    pub id: OpId,
    pub dut_id: String,
    pub source: Option<String>,          // "hdmi" / "camera" / None (ignored for RecordStop)
    pub op: MediaOp,
    /// Moved into the GUI in-flight table on receipt; the GUI is its only sender.
    pub reply: tokio::sync::oneshot::Sender<MediaOpReply>,
}

#[derive(Debug)]
pub enum MediaOpReply {
    /// `snapshot` / `recordStop`.
    Artifact(MediaArtifact),
    /// `recordStart`.
    Recording(RecordingHandle),
    Err(MediaOpError),
}

#[derive(Debug)]
pub struct MediaOpError { pub code: &'static str, pub message: String }

#[derive(Debug)]
pub struct RecordingHandle {
    pub recording_id: RecordingId,
    pub source: String,
    /// First segment returned by `Capture::record` (that call returns immediately).
    pub first_path: String,
    /// UNIX epoch millis when `record()` returned.
    pub started_at_ms: u64,
    pub note: Option<String>,
}

#[derive(Debug)]
pub struct MediaArtifact {
    pub path: String,                    // recording: last segment
    pub first_path: Option<String>,      // recording only (= path when not rotated)
    pub rotated: bool,                   // recording only
    pub bytes: u64,                      // the file at `path`
    pub width: Option<u32>,              // snapshot only
    pub height: Option<u32>,             // snapshot only
    /// Measured wall-clock seconds from `record()` to the last segment; no cap.
    pub duration_secs: Option<u64>,
    pub note: Option<String>,
    pub source: Option<String>,
}
```

> MCP 侧把 `MediaOpReply` 映射成 §5.2 的 JSON：`server.rs` 里手写 `json!`，**字段名以 §5.2 表为准**（`recordingId` ← `recording_id`、`firstPath` ← `first_path`、`startedAtMs` ← `started_at_ms`）；DTO 本身不 derive `Serialize`，避免 GUI 与 MCP 各演化出一套字段名。

```rust
// apps/desktop/src/media_page.rs（bin 侧：通道端与在途表；DTO 来自 core）
use duthub_core::media_op::{MediaOpReply, MediaOpRequest, OpId, OpReceiver};

/// 已完成的工作线程结果（std 通道，GUI 侧非阻塞 try_iter）。
struct OpFinished { id: OpId, key: (String, Source), reply: MediaOpReply }

/// GUI 侧持有的通道端：请求队列 + 在途表。
pub struct MediaOpPort {
    rx: std::sync::mpsc::Receiver<MediaOpRequest>,
    tx: std::sync::mpsc::SyncSender<MediaOpRequest>,   // clone 交给 RuntimeMcpState
    done_rx: std::sync::mpsc::Receiver<OpFinished>,
    done_tx: std::sync::mpsc::Sender<OpFinished>,       // clone 进 SnapshotJob 所在线程
    in_flight: BTreeMap<OpId, InFlight>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InFlightKind { Snapshot, RecordStart, RecordStop(RecordingId) }

struct InFlight {
    key: (String, Source),
    kind: InFlightKind,
    queued_at: Instant,
    started_at: Option<Instant>,        // ready 之后的时刻（start 截止与 op 起点）
    /// The AgentOp lease acquired for this op (released when the op finishes/aborts).
    lease: LeaseId,
    /// Snapshot only: when the 15s job watchdog fires.
    job_deadline: Option<Instant>,
    /// RecordStop only: when `stop_recording()` was issued; `RECORD_STOP_GRACE` counts from here.
    stop_requested_at: Option<Instant>,
    /// The oneshot sender NEVER leaves the GUI thread; workers only report via `done_tx`.
    reply: Option<tokio::sync::oneshot::Sender<MediaOpReply>>,
}

/// One in-progress Agent recording (§5.6). Lives in `MediaView::recordings`.
struct ActiveRecording {
    key: (String, Source),
    note: Option<String>,
    first_path: PathBuf,
    started_at: Instant,
    /// Held for the whole recording; released by `recordStop` / `stop_capture`.
    lease: LeaseId,
    /// Set once, when `Capture::stop_recording()` is called.
    stop_requested_at: Option<Instant>,
}
```

- `MediaOpPort` 的实现面（GUI 侧唯一通道端，供 §4.5/§5.5 的调用点使用）：
  `try_recv_request() -> Option<MediaOpRequest>`（`drain_media_ops` 用）、`insert_in_flight(id, InFlight)`、
  `in_flight() -> impl Iterator<Item = (OpId, &InFlight)>`、`op_by_kind(kind) -> Option<OpId>`、
  `finish(id, reply: MediaOpReply) -> bool`（oneshot 归位，返回 false = 已超时/已回绝的迟到结果，丢弃）、
  `fail(id, code, message)`（用错误回绝一个在途 op）、`abort_all_for(key, code)`（停采回绝）。
  它留在 bin（`key` 用到 bin 的 `Source` 类型），因此**不为它新建 lib 单测**：桥的行为由 §7.2 的 MCP 侧假 `McpState` 与 §7.3 的 bin `#[ignore]` 编排用例覆盖。
- 录屏**不占跨 tick 的 `InFlight` 常驻项**：`recordStart` 在收到请求的同一 tick 内完成（`record()` 同步返回首段），reply 立刻发出。此后「这次录屏还在进行」的唯一事实来源是 `MediaView::recordings: BTreeMap<RecordingId, ActiveRecording>`；`DemandEntry.agent_recording` 只是它的镜像（用于 pin 判定与手动录制互斥），不存副本。
- `ActiveRecording.first_path` / `started_at` 是 `firstPath`、`rotated`（`first != last`）与 `durationSecs`（`stop - start` 的整秒）的唯一数据来源；`lease` 让 `recordStart` 取到的 `AgentOp` 租约在 `recordStop`/停采时**由同一个 op 释放**（不靠 `drop` 猜测）。
- `recordStart` 的落盘参数与面板 `Action::Record` **完全同源**（`media_page.rs:1175-1180`）：`capture.record(&self.dir, &dut_id, &duts.saved_name(&dut_id)?, quota)`，其中 `quota` 是 §3.7 注入的 `Workbench.artifact_quota`（`self.quota.clone().ok_or(MEDIA_CAPTURE_UNAVAILABLE)?`）。DUT 名取自已保存配置（`duts.saved_name`），**不取端口名/会话名**（坑 #78 形状），因此落盘目录与配额记账只有这一条路径。
- `record()` 成功后**立即**插入 `recordings[id]` 并写 `DemandEntry.agent_recording = Some(id)`——**不是**等到 `recordStop` 才写，否则整个录制窗口内手动录屏按钮不会被锁。`recordStop` 到 `poll_recording` 交付末段之间的收尾由 `RECORD_STOP_GRACE` 兜底（下表）。

**通道所有权（`oneshot::Sender` 不 `Clone`，只有 `send(mut self, …)`）**：

| 角色 | 持有什么 |
|---|---|
| MCP 处理器 | 只持 `OpReceiver`；`MediaOpRequest` 由它构造并 `try_send` 进队列 |
| 队列 | `MediaOpRequest`（内含那个唯一的 `Sender`） |
| GUI `drain_media_ops` | 收到请求后把 `reply` **移动**进 `InFlight.reply`；此后它是唯一发送者 |
| Snapshot 工作线程 | 只持 `done_tx`（`std::sync::mpsc::Sender` 可 clone）与一个 owned `SnapshotJob`；结果以 `OpFinished{id, key, reply}` 回 GUI，**不碰 oneshot** |
| `abort_all_for` / 超时 | 从 `InFlight` 取出 `reply` 并 `send` 一次错误；随后该 op 再无发送者 |

因此三处主张互斥：`Sender` 自始至终只有「队列 → InFlight」一次移动，worker 侧只用 std 通道。

时序：

```
MCP 线程（async 任务）                      GUI 线程（50ms tick）                     工作线程
  │ oneshot::channel() → (tx, rx)            │ drain_media_ops()                        │
  │ try_send(MediaOpRequest{reply: tx})      │  pick_source / ensure_session(ephemeral) │
  │ timeout(OP_TIMEOUT, rx).await            │  tx 移入 in_flight[id].reply             │
  │                                          │  snapshot → SnapshotJob                  │
  │                                          │  recordStart → capture.record()（同步）  │
  │                                          │  ── job + done_tx.clone() ─────────────▶ run()（≤12s，仅 snapshot）
  │                                          │                                          │
  │                                          │ ◀── done_rx: OpFinished{id, reply} ──────┘
  │                                          │ reply.send(Artifact / Recording / Err)
  │  ◀── oneshot 完成 ─────────────────────── │ snapshot：release(AgentOp)
  │                                          │ recordStart：租约保持、会话 pin
  │
  │ （另一次工具调用）recordStop              │  查 recordings[id] → stop_recording()
  │ timeout(OP_TIMEOUT, rx).await            │  stop_requested_at = now
  │                                          │  poll_recording() 交出末段 → 组装 artifact
  │  ◀── oneshot 完成 ─────────────────────── │  reply.send(Artifact) + release(lease)
```

- **计时表（录屏时长不在表内：`recordStart` 与 `recordStop` 是两次独立调用）**：

| 计时 | 起点 | 时长 | 到期动作 |
|---|---|---|---|
| `START_TIMEOUT` | `queued_at` | 10s | `snapshot`/`recordStart` 先复查 `ready()`；仍未 ready 且未 ended → `MEDIA_START_FAILED`（**不停会话**） |
| `JOB_TIMEOUT`（**snapshot only**） | `started_at`（job 派发时刻） | 15s（截图内部 12s `media.rs:360` + 3s 余量） | GUI 侧 `MEDIA_OP_TIMEOUT`（message 注明「采集无响应」）；标记该 op 结束 |
| `RECORD_STOP_GRACE`（**recordStop only**） | `stop_requested_at` | 5s | `poll_recording()` 仍无末段 → `MEDIA_OP_TIMEOUT`（只结束该 op 与登记项，**不停采**） |
| MCP `OP_TIMEOUT` | 请求发出 | snapshot **30s**、recordStart **30s**、recordStop **30s** | 客户端侧 `MEDIA_OP_TIMEOUT`，不再等 |

- **「不设时间上限」在实现层就是「这张表里没有录制时长」**：两个工具调用各自有界，两次调用之间隔 1 秒还是 3 小时都不影响任何超时；没有 `CLIP_DEADLINE`、没有 `seconds`、没有到点自动停录的代码路径。
- 最坏路径（snapshot）：慢启动 10s + 截图 12s = 22s < 30s。最坏路径（recordStop）：一个 tick（≤50ms）触发 `stop_recording()` + 录制线程 flush（`media.rs:430-434`）+ 下一 tick `poll_recording()` < 5s < 30s。
- `JOB_TIMEOUT` / `RECORD_STOP_GRACE` 到期只结束**本次 op**（不是停采）：若 worker/录制线程之后才产出结果，`in_flight`/`recordings` 已无该 id → **丢弃结果**（幂等）；此时 PNG/TS 可能已落盘，属已知的孤儿文件（§8.12），不影响回复。
- **不走「启动缓慢 → 误杀」**：`START_TIMEOUT` 到期时**先复查** `ready()`——刚落地的会话直接进入 op；仍未 ready 也**不停会话**，只把**本次 op** 回 `MEDIA_START_FAILED`，会话交给空闲回收（ephemeral 无租约 → `idleSecs` 后自然回收）。这样 10.05s 才 ready 的采集不会被 `stop_capture` 打断，也不会误报成功。
- **录屏没有工作线程**：`record()`/`stop_recording()` 都在 GUI 线程调用并立即返回，`poll_recording()` 由既有 tick 轮询（`media.rs:447-459`）。`recordStart` 在同一 tick 内完成登记与回复；`recordStop` 只做「`stop_recording()` + 记 `stop_requested_at`」，末段由下一 tick 的 `poll_recording()` 交付。
- **`recordStop` 的收尾形状**（先收集再动作，避免 `self.sessions` 可变借用与 `self.recordings`/`self.ops` 交错 —— 坑 #14 同族；下为伪代码，早退即 `reply Err`）：

```rust
// ① 收到 request（drain_media_ops）：校验归属后立刻停录
let rec = self.recordings.get_mut(&id).ok_or(MEDIA_RECORDING_UNKNOWN)?; // aborted 里命中 → MEDIA_RECORDING_ENDED
if rec.key.0 != request.dut_id { return Err(MEDIA_RECORDING_UNKNOWN); }
if rec.stop_requested_at.is_some() { return Err(MEDIA_BUSY); }          // 同 id 的第二个 recordStop
self.sessions.get_mut(&rec.key).ok_or(MEDIA_RECORDING_ENDED)?.stop_recording();
rec.stop_requested_at = Some(now);
self.ops.insert_in_flight(request.id, InFlight { kind: InFlightKind::RecordStop(id), .. });

// ② tick 的 poll_recording 段落：拿到结果先问归属（否则落回既有面板消息）
let mut finished = Vec::new();
for (key, session) in self.sessions.iter_mut() {
    if let Some(result) = session.poll_recording() { finished.push((key.clone(), result)); }
}
for (key, result) in finished {
    if !self.finish_recording(&key, result) { /* 手动录制：既有「录屏已保存到 …」 */ }
}

// ③ finish_recording：命中活动录屏 → 组装 artifact → 唯一一次回复
fn finish_recording(&mut self, key: &(String, Source), result: Result<PathBuf, String>) -> bool {
    let Some(id) = self.recordings.iter().find(|(_, r)| &r.key == key).map(|(id, _)| *id) else {
        return false;
    };
    let rec = self.recordings.remove(&id).unwrap();
    let outcome = match result {
        // 只有「先收到 recordStop，再拿到末段」才是成功；会话在未收到 recordStop 前结束
        // （stream ended / Drop）一律按被打断处理，绝不把半段伪装成完整产物。
        Ok(last) if rec.stop_requested_at.is_some() => MediaOpReply::Artifact(MediaArtifact {
            first_path: Some(rec.first_path.display().to_string()),
            rotated: rec.first_path != last,
            bytes: artifact_bytes(&last).unwrap_or(0),
            duration_secs: Some(rec.started_at.elapsed().as_secs()),
            path: last.display().to_string(), width: None, height: None,
            note: rec.note, source: Some(key.1.key().into()),
        }),
        Ok(_) => MediaOpReply::Err(MediaOpError { code: "MEDIA_RECORDING_ENDED", message: "采集已结束".into() }),
        Err(message) => MediaOpReply::Err(MediaOpError { code: "MEDIA_CAPTURE_FAILED", message }),
    };
    if let Some(entry) = self.demand.get_mut(key) { entry.agent_recording = None; }
    if let Some(op) = self.ops.op_by_kind(InFlightKind::RecordStop(id)) {
        self.ops.finish(op, outcome);      // oneshot 只在这里发送一次
    }
    self.release(rec.lease);
    true
}

// ④ 每 tick 的宽限扫描：录制线程卡死也不许让调用方永久挂着（§8.12）
fn sweep_recording_stops(&mut self) {
    self.aborted.retain(|_, at| at.elapsed() < ABORTED_TTL);        // 墓碑 10 分钟
    let stranded: Vec<RecordingId> = self.recordings.iter()
        .filter(|(_, r)| r.stop_requested_at.is_some_and(|t| t.elapsed() >= RECORD_STOP_GRACE))
        .map(|(id, _)| *id).collect();
    for id in stranded {
        let rec = self.recordings.remove(&id).unwrap();
        if let Some(entry) = self.demand.get_mut(&rec.key) { entry.agent_recording = None; }
        if let Some(op) = self.ops.op_by_kind(InFlightKind::RecordStop(id)) {
            self.ops.fail(op, "MEDIA_OP_TIMEOUT", "采集无响应");
        }
    }
}
```

- **`recordStop` 完成（或宽限到期）后释放租约** → ephemeral 会话进入 `idle_since`，再过 `idleSecs` 才被回收（§4.3）。录屏本身不参与空闲回收，也不会因为「录久了」被任何计时器打断。
- `MEDIA_START_FAILED` 的 `MediaUrls` 清理：未 ready 的会话从未入表（`media_page.rs:376-390`），无需清理；`statuses` 在下一 tick 自然收敛。
- GUI 未取走/未处理：`try_send` 失败（队列满/通道关闭）→ MCP 立即回 `MEDIA_OP_DROPPED`；`OpReceiver` 收到 `Err`（Workbench 已销毁，`Sender` 被 drop）→ `MEDIA_OP_DROPPED`；`timeout` 到期 → `MEDIA_OP_TIMEOUT`。`recordStart` 超时不会留下录屏：GUI 没跑到 `record()` 就没有登记项；跑到了而 reply 送不出去，录屏继续，直到面板停止或 `stop_capture`。
- 上限：同一 `(dut, source)` 同时 1 个在途 op（第二个 `MEDIA_BUSY`）；同一 `(dut, source)` 同时 ≤1 个活动录屏（第二个 `recordStart` → `MEDIA_RECORDING_IN_PROGRESS`）；全局在途 ≤4、全局活动录屏 ≤4（GUI 侧计数）；**没有任何时长上限**。
- GUI 不存在（stdio 快照、`--self-test`）：`McpState::media_op` 默认实现返回 `None` → `MEDIA_CAPTURE_UNAVAILABLE`（与 stdio 今天「看不到活动流」一致，`state.rs:225-232`）。

### 5.6 op 与手动录制/停采的互斥与归属

| 竞争 | 规则 |
|---|---|
| Agent 录屏 vs 用户手动录制 | 两者共用一个 `Capture::recording` 槽，而 `record()` **不设防**（`media.rs:433` 直接覆盖）。因此：① `recordStart` 若 `capture.is_recording()` → `MEDIA_RECORDING_IN_PROGRESS`；② 面板 `Action::Record` 与 `Action::StopRecording`（`media_page.rs:1173-1184`）在 `demand.agent_recording.is_some()` 时**既不派发也不渲染**（按钮在 `render_bindings` 里置灰），`run_action` 同时加硬守卫并提示「Agent 正在录屏」；③ **`agent_recording` 在 `record()` 成功的那一刻写入**，所以整个录制窗口（**不设上限**）都处于锁定态 |
| `poll_recording` 结果归属 | `recordings` 里有该 key 的活动录屏 → 结果归 `recordStop`（回 reply，**不再**写 `self.output` 的「录屏已保存到 …」）；没有 → 走既有 `media_page.rs:391-397` 消息。因登记项从录制开始即存在，Agent 录屏的 `poll_recording` 结果**永远不会**落进 `self.output` |
| 在途 op vs 停采（7 个对账调用点 + `Action::Stop` + tick 回收） | 全部经 `stop_capture`：在途 `snapshot`/`recordStart` 回 `MEDIA_CAPTURE_ENDED`；活动录屏进 `aborted` 墓碑，配对的 `recordStop` 回 `MEDIA_RECORDING_ENDED`（无 artifact），见 §4.5 |
| 在途 op vs 绑定变更（保存停用/改绑） | 同上（`reconcile_bindings` → `stop_capture`）；不尝试用旧绑定继续 |
| 静帧 job 的线程 | job 只持 `done_tx`；即使 `Capture` 被停，worker 的 ffmpeg 失败也会以 `Err(MEDIA_CAPTURE_FAILED)` 回 GUI 并 `send`，所以静帧**不会**悬挂；录屏依赖 §4.5 的显式回绝与 §5.5 的 `RECORD_STOP_GRACE` |
| **`recordStop` 永远不到来** | 录屏不会自动停：会话保持 `Pinned`、TS 持续写盘并按 `min(quota/4, 1 GiB)` 轮转，直到配额写满（`recordStop` 回 `MEDIA_CAPTURE_FAILED`）、用户停止采集、绑定变更/DUT 删除或进程退出。**不设隐含时长上限**（用户 2026-11-24 决定）；这是「Agent 可自由录屏」的代价，面板以「Agent 正在录屏 · 已录 Ns」持续可见，人工可随时停止 |

### 5.7 失败语义（完整表）

| 场景 | `data.code` | 说明 |
|---|---|---|
| DUT 未绑定视频源 | `MEDIA_NOT_BOUND` | 复用 `captureNeedsVideo` 文案 |
| GUI 未运行 / 本构建无采集能力 / ffmpeg 不可用 | `MEDIA_CAPTURE_UNAVAILABLE` | 新 i18n 键 |
| 启动后 10s 未 `ready()`（未结束） | `MEDIA_START_FAILED` + `detail` | GUI 侧 `START_TIMEOUT` 复查 `ready()` 后再判（§5.5）；**不停会话**，交给空闲回收 |
| 截图超时/非 PNG/文件读取失败/落盘失败；`record()` 起始落盘失败；录制中途轮转写盘失败（**含配额耗尽**） | `MEDIA_CAPTURE_FAILED` + `message` | `screenshot`/`record` 只返回 `Result<PathBuf, String>`（`media.rs:328-334`、`:378-386`），R2 下无法把配额/落盘与采集失败分类 → **不设 `MEDIA_QUOTA`**；原始错误串截断后进 `message`。配额耗尽属存储边界，不是时长上限（§5.2/§5.4） |
| 采集已结束/被停采打断；在途 `snapshot`/`recordStart` | `MEDIA_CAPTURE_ENDED` | 无 artifact；部分文件留在 DUT 目录由配额清理 |
| 录屏在录时被停采打断，之后 `recordStop` | `MEDIA_RECORDING_ENDED` | 由 `aborted` 墓碑判定（§4.5）；无 artifact，不返回任何分段 |
| `recordStop` 的 `recordingId` 不存在 / 不属于该 DUT / 已收尾超时被清出登记簿 | `MEDIA_RECORDING_UNKNOWN` | 与「跨 DUT 探测」同码；GUI 重启后所有旧 id 都是这个码 |
| 同源已有在途 op | `MEDIA_BUSY` | 纯函数判定；同一 `recordingId` 的第二个 `recordStop` 也是它 |
| 该源已在录屏（Agent 或用户手动） | `MEDIA_RECORDING_IN_PROGRESS` | §5.6 |
| 请求未被 GUI 取走 / Workbench 已退出 | `MEDIA_OP_DROPPED` | `try_send`/`oneshot` 失败 |
| GUI 侧 `JOB_TIMEOUT`（15s）/ `RECORD_STOP_GRACE`（5s）到期，或客户端超时到期 | `MEDIA_OP_TIMEOUT` | 同码不同 message（「采集无响应」/「请求超时」）；GUI 到期只结束该 op，不停采（§5.5） |
| 无 `capture` 授权 | 既有 `LeaseRequired`/`UNAUTHORIZED` | §5.3，先于一切 |
| 跨 DUT（专用监听器） | 既有 `LeaseRequired`/`UNAUTHORIZED` | bound 前置检查 |
| 未知 `kind` / 缺 `recordingId` / 任何未知键（**含 `seconds`**） | `invalid_params` | `INVALID_MEDIA_OP` / serde 拒绝；**没有** `INVALID_CLIP_SECONDS` |

所有失败都是 `rmcp::ErrorData`（与既有工具同形状），**不得**回落到「返回空 streams 假装成功」。

### 5.8 工具面同步点（本轮改动清单）

| 位置 | 本轮 | 说明 |
|---|---|---|
| `crates/duthub-mcp/src/server.rs` 的 `#[tool] media_streams` | **改**（参数 + async + 授权分支 + 三个超时常量） | 工具名不变 → 路由/白名单不变 |
| `crates/duthub-mcp/src/state.rs` `McpState` | **加** `fn media_op(&self, _: duthub_core::media_op::MediaOpRequest) -> Option<duthub_core::media_op::OpReceiver> { None }` 默认方法 | 默认实现丢弃请求（其中的 `Sender` 随之 drop）并回 `None` → 工具层直接 `MEDIA_CAPTURE_UNAVAILABLE`，不进 `await`；保 stdio/证据服务器行为 |
| `crates/duthub-mcp/src/server.rs` `plan_tool_allowed` | **不改** | 13 项不动 |
| `apps/desktop/src/agent_host/cli.rs` `PLAN_TOOLS` | **不改** | 与 `agent_cli.rs:520-532` 的等值比较保持一致 |
| `crates/duthub-mcp/src/skill.rs` 工具表 | **改文字**（说明 `op`：`snapshot` / `recordStart` / `recordStop` 必须配对、无时长上限、需 `capture` 授权） | 该文件测试做 contains 断言（`skill.rs:116-141`）；不得引入 `serial_write`/`control_power`/`approval_request`/`adb_shell` 字样，也不得出现「秒数」措辞 |
| `docs/API_CONTRACT.md` + `docs/SECURITY.md` | **改** | 文档 |
| `plan_badges.rs` / locale 的「13 个 Plan 工具」文案 | **不改** | `plan_badges.rs:82`/`:97` 钉住 |

---

## 6. HDMI 浮窗与还原

### 6.1 方案选定：共享帧

| 方案 | 成本 | 结论 |
|---|---|---|
| **A 共享帧** | 一份解码 + 每窗一份 640×360×4 ≈ 0.9 MB 纹理 | **采纳** |
| B 浮窗自己解（再 `open_inline`，`owns_capture=false`） | 多一个 ffmpeg + 一路解码，两窗不同步；关浮窗若误调 `close_inline_capture` 会连右侧栏一起停（`media_page.rs:311-320`） | 否决 |
| C 把 `Workbench` 当第二窗口 Root | `render` 跑两遍，`InlinePreview::poll` 的 take 语义会两边抢帧 | 否决（红线） |

框架依据（本会话核对 `gpui-kit 0.9.1` 源码）：`gpui_kit::open_window(options, cx, build) -> anyhow::Result<(AnyWindowHandle, Entity<V>)>`（`gpui-kit-0.9.1/src/lib.rs:181-196`）；`Arc<RenderImage>` 与窗口解耦、绘制时按窗上传；`Window::drop_image` 只清本窗图集，`App::drop_image(image, current_window)` 遍历所有窗口（`gpui-pre-0.3.12/src/app.rs:3217`）——浮窗存在后**必须**改用 App 级清理。

### 6.2 状态机（lib 纯逻辑，`capture_ui.rs`）

```rust
pub enum MonitorPlacement { Docked, Floating, Closing }

pub enum MonitorEvent {
    OpenFloat,
    Redock,
    UserClosedWindow,        // 浮窗被 X 关闭（on_window_closed）
    SidePanelClosed,         // close_side_panel（side_panel.rs:507-513）
    PanelSwitched,           // open_side_panel 切到 Shell/别的面板（side_panel.rs:486-505）
    PreviewFailed,
    MainWindowClosing,       // 主窗 on_window_should_close
    MainWindowClosed,
}

pub enum MonitorAction {
    None,
    OpenFloatWindow,
    CloseFloatWindow,
    FocusMainWindow,
    StopPreview,
    ShowDockedPlaceholder,
}

pub fn next(state: MonitorPlacement, event: MonitorEvent) -> (MonitorPlacement, MonitorAction);
```

| 当前 | 事件 | 下一状态 | 动作 | 说明 |
|---|---|---|---|---|
| `Docked` | `OpenFloat` | `Floating` | `OpenFloatWindow` | 复用既有 capture，不新建 |
| `Floating` | `Redock` | `Docked` | `CloseFloatWindow` + `FocusMainWindow` | |
| `Floating` | `UserClosedWindow` | `Docked` | `ShowDockedPlaceholder` | 不抢焦点 |
| `Floating` | **`SidePanelClosed`** | `Floating` | `None` | **预览由浮窗继续消费帧**；不得 `StopPreview`（否则浮窗停在死帧） |
| `Floating` | **`PanelSwitched`** | `Floating` | `None` | 同上 |
| `Docked` | `SidePanelClosed` / `PanelSwitched` | `Docked` | `StopPreview` | 今天的 `close_side_panel`→`stop_preview` 语义 |
| `Floating` | `MainWindowClosing` | `Closing` | `CloseFloatWindow` | 先关浮窗再放行主窗 |
| 任意 | `PreviewFailed` | `Docked` | `StopPreview` + `CloseFloatWindow` | 失败不留在浮窗 |
| `Closing` | `MainWindowClosed` | `Closing` | `None` | 进程随最后窗口退出 |

不变量：同时最多一个浮窗；`Floating` 期间右侧栏只画占位；两窗共用唯一 `FramePump`。

### 6.3 帧所有权与 `FramePump`

`side_panel.rs` 现在把帧、预览、drop 三件事混在渲染路径里（`side_panel.rs:982-998`），浮窗化必须把所有权上提：

| 归属 | 改造前 | 改造后 |
|---|---|---|
| `InlinePreview` | `side_panel.preview: Option<InlinePreview>` | `FramePump { preview: Option<InlinePreview>, frame: Option<Arc<RenderImage>>, last_error: Option<String> }`（`Workbench` 字段） |
| 帧轮询 | `render_monitor` 渲染时 `preview.poll()` | `FramePump::tick()` 由 `cx.spawn` + `background_executor().timer(100ms)` 驱动（`main.rs:235-238` 已有该用法）；主窗最小化/切面板不停帧 |
| `tick_side_panel` | 负责 shell 续租等 | **不再驱动帧**；帧驱动只在 `FramePump::tick` |
| 换帧清理 | `window.drop_image(old)`（仅本窗） | **`cx.drop_image(old, Some(window))`**（App 级，清所有窗图集） |
| 关闭清理 | `close_side_panel` 里 `window.drop_image(frame)`（`side_panel.rs:509-511`） | 先问状态机：`next(placement, SidePanelClosed)`。**`Docked`** → `FramePump::release_monitor(cx)`（释放 `Monitor` 租约 + `clear`，不直接操作帧）；**`Floating`** → 只 `FramePump::clear_docked_view(cx)`（清右侧栏占位，**不动** `preview`/租约，浮窗继续出帧）。`close_side_panel` 本身不再手写 `drop_image` |
| 谁可以停预览 | `side_panel.stop_preview()` | `FramePump::release_monitor()`（内部 `media.close_inline_capture`，§4.4 的幂等释放）；**调用前必须确认 `placement != Floating`**，否则就是「关掉浮窗预览」bug |

`FramePump` 的公开形状（bin 内 `pub(crate)`）：

```rust
pub(crate) struct FramePump {
    preview: Option<media::InlinePreview>,
    frame: Option<Arc<gpui_kit::RenderImage>>,
    last_error: Option<String>,
    key: Option<(String, media::Source)>,
    /// Mirrors `MonitorPlacement` so `release_monitor` can refuse while floating.
    placement: MonitorPlacement,
}
impl FramePump {
    pub fn attach(&mut self, key: (String, media::Source), preview: media::InlinePreview);
    pub fn tick(&mut self, cx: &mut Context<Workbench>) -> bool;   // poll + drop_image(App 级)
    pub fn frame(&self) -> Option<Arc<gpui_kit::RenderImage>>;
    pub fn error(&self) -> Option<&str>;
    /// Clear the docked view only (keeps `preview` alive for the float window).
    pub fn clear_docked_view(&mut self, cx: &mut Context<Workbench>);
    /// Release the Monitor lease + clear. No-op (returns false) while Floating.
    pub fn release_monitor(&mut self, cx: &mut Context<Workbench>) -> bool;
    pub fn set_placement(&mut self, placement: MonitorPlacement);
}
```

- 帧失败：`tick` 把 `last_error` 交给 `media.capture_error(...)` 兜底（保留 `side_panel.rs:989-999` 的既有优先级），并按 `PreviewFailed` 驱动状态机。
- 右侧栏与浮窗都只读 `frame.clone()`；`Floating` 时右侧栏画占位（不双上传）。
- **`release_monitor` 在 `Floating` 下返回 `false` 并不做任何事**：这是 §6.2「`Floating + SidePanelClosed/PanelSwitched` → `None`」的实现保证；只有 `Docked` 才真正释放 `Monitor` 租约（`Floating` 期间的租约由浮窗持有，关浮窗时才释放）。

### 6.4 窗口生命周期（bin，`monitor_window.rs`）

1. **开窗**：`WindowOptions { window_bounds: Windowed(centered), titlebar: Some(TitlebarOptions { title, appears_transparent: true }), focus: false, show: true, kind: WindowKind::Normal, is_resizable: true, ..Default::default() }`；`gpui_kit::open_window(...)` 存 `(AnyWindowHandle, Entity<FloatMonitor>)` 到 `Workbench`。
   - `focus: false` → Windows `SW_SHOWNOACTIVATE`，不抢主窗焦点；浮窗内一律不 `track_focus`。
   - **不用 `PopUp`**（`WS_EX_TOPMOST` + 无原生边框 + 只有顶边缩放热区），**不用 `Floating`**（Windows 无特判 = 普通窗）。要「总在最前」见 §8.8。
   - **无系统标题栏**：`appears_transparent: true` 隐藏系统标题栏，窗口成为深色无边框窗；`title` 仍设置（只用于任务栏 / Alt-Tab，浮窗内容区不再重复显示标题）。
   - **内容 = 窄图标工具条 + 帧区**。工具条高 30px、底色 `Palette::surface`、图标水平居中、`gap(6)`、不随窗口缩放（`flex_shrink_0`）；整条设 `window_control_area(WindowControlArea::Drag)`（拖动窗口，双击由平台最大化）；**每个图标按钮必须 `.occlude()`**，否则拖拽命中区会吞掉点击（坑 #1）。
   - **图标按钮**：手绘 div（坑 #2：浮层内 kit `Button` 渲染成空色块），26×26、`rounded(6)`、`cursor_pointer`、hover 才出 `Palette::hover` 底色，内部 `icons::icon(name).size(15).text_color(Palette::muted)`。
   - **三个按钮（左→右）**：还原到右侧 `panel-right`（`prepare_redock` + `remove_window`）；全屏切换按 `window.is_fullscreen()` 取 `minimize-2`（退出全屏）/ `maximize`（进入全屏）并调 `window.toggle_fullscreen()`（点击后 `cx.notify()` 刷新图标）；关闭 `x`（与平台关闭钮同路径：`remove_window` → `UserClosedWindow`）。
2. **关窗路径统一**：`on_window_closed` 比对保存的 `WindowId` → `next(Floating, UserClosedWindow)`；主窗 `on_window_should_close` → `next(Floating, MainWindowClosing)` 并 `remove_window()` 后返回 `true`（否则「最后窗口关闭才退出」会让进程悬着）。
3. **还原**：翻 `Docked` → `remove_window()` → `AnyWindowHandle::update(cx, |_, w, _| w.activate_window())`。
4. **非活动窗节流**：默认 `inactive_frame_interval` ≈ 33 ms，10 fps 预览无影响。

### 6.5 风险与对策

| 风险 | 对策 |
|---|---|
| 主窗最小化停帧（今天在渲染里 poll） | 帧泵上提到 `FramePump::tick`（§6.3） |
| 两处抢 `poll`（take 语义） | 只有 `FramePump::tick` 一个消费者 |
| 浮窗图集泄漏（每帧 0.9 MB） | App 级 `drop_image` + `clear` 时清当前帧 |
| 关侧栏把浮窗变死帧 | 状态机新增 `SidePanelClosed`/`PanelSwitched`（`Floating` 时 `None`） |
| 浮窗夺焦导致终端收不到键 | `focus: false` + 浮窗不 `track_focus` |
| 跨窗帧未实测 | §7.3 手工验收清单 |

---

## 7. 测试计划

> 原则：**既有测试一行不改、不删**（§1.2 全表 + §1.2 冻结的 `MediaView` 公开面）。新增测试全部是新文件或新模块。

### 7.1 纯函数单测（无 GPUI、无进程、无真机）

| 目标 | 文件 | 用例 |
|---|---|---|
| 配置契约 | `crates/duthub-core/tests/capture_idle_roundtrip.rs`（新） | ① 默认 `CaptureSettings` 的 `idle_secs == None`；② `{"tools":{}}` 序列化仍恰为 `{"adbExecutable":"adb"}`；③ `{"idleSecs":90}` 往返语义相等（**camelCase 键**，坑 #15）；④ `idleSecs: null` 与缺键等价；⑤ 读 `docs/examples/settings.json` → `tools.capture == Default::default()` |
| 钳制 | `apps/desktop/tests/capture_backend_logic.rs`（新） | `effective_idle(None) == 60s`；`Some(0)`/`Some(9999)` → 60s（越界落默认，**不是** 1 秒/3600 秒）；`Some(90) == 90s`；`validate_idle_secs(0/3601) == None`、`(1/3600) == Some` |
| 路径解析 | 同上 | `resolved_capture_settings_in`：绝对路径原样返回（**幂等**：二次解析不变）；`%FFMPEG_HOME%/bin/ffmpeg.exe` 原样返回（留给 `media.rs` 展开）；空配置 + 空 tools_root → `"ffmpeg"`；空配置 + 临时目录里伪造的 ffmpeg 安装 → 绝对路径；`ffplay`/`listen_ip` 不被修改 |
| 账本 | 同上 | §4.2 转移表逐行；`Manual` → `Pinned`；`recording = true` → `Pinned`（镜像字段）；`agent_recording = Some(_)` → `Pinned`；交互式最后 release → `StopNow`；ephemeral 最后 release → `Keep` 到点 `Reclaim`；hold 到期被 `sweep_expired` 清掉；重复 acquire/release 幂等；注入 `Instant` 不依赖真实时钟 |
| artifact | 同上 | `png_dimensions`（真实 1×1 PNG 头、截断、非 PNG → `None`）；`artifact_bytes`（存在/不存在 → `None`） |
| 错误码 | 同上 | `MEDIA_*` 全集唯一、与 §5.7 表一致 |
| 浮窗状态机 | `apps/desktop/tests/capture_ui_logic.rs`（新） | §6.2 转移表逐行；`Floating + SidePanelClosed/PanelSwitched` 不产生 `StopPreview`；`Floating + MainWindowClosing` 必须先 `CloseFloatWindow`；`Closing` 下无动作 |
| 工具入参 | `crates/duthub-mcp/tests/media_ops.rs`（新） | `{"dutId":"d"}` → `op=None`；未知 `kind` → invalid；`recordStop` 缺 `recordingId` → invalid；`recordStart` 带 `seconds` → invalid（`deny_unknown_fields`）；`deny_unknown_fields` 拒其它未知键；无 `op` 的响应形状与今天一致 |

> `effective_idle(Some(0))` 的期望值写死为 `DEFAULT_EPHEMERAL_IDLE_SECS`：`0` 是非法输入（非「1 秒」），钳制到默认比钳制到 1 秒更符合「手改配置后行为可预期」，测试按此断言。

### 7.2 适配器/编排测试（不依赖真机）

| 目标 | 位置 | 形态 |
|---|---|---|
| `MediaView` 编排（`#[ignore]`） | `apps/desktop/src/media_page.rs` `#[cfg(test)]`（**新增测试函数**，不改既有） | 用 lavfi `Capture`（与 §1.2 既有 ignored 测试同法）驱动：交互式释放即停、ephemeral 到点回收（`idleSecs: 1` 缩短等待）、`statuses` 不变时不重建 `MediaUrls`、`stop_capture` 回绝在途 op、`recordStart`/`recordStop` 往返（`recordings` 登记与摘除、`firstPath`/`rotated`/`durationSecs`）、录屏期间 `decide == Pinned`（ephemeral 不被回收）、`RECORD_STOP_GRACE` 到期回 `MEDIA_OP_TIMEOUT`、录屏与手动录制互斥、`pick_source` 只读 `saved_media`（**必须放 bin**：`DutsState` 是 bin 类型，`main.rs:13`、`media_page.rs:14`，lib 集成测试拿不到） |
| 自建帧断言助手 | 同上 | `media.rs` 的 `assert_preview_frames` 是 `#[cfg(test)] mod tests` 的私有函数（`media.rs:952`），其它模块不可见，必须重写约 12 行等价实现 |
| op 桥（无 GUI） | `crates/duthub-mcp/tests/media_ops.rs` | `SnapshotState` + `media_op` 默认 `None` → `MEDIA_CAPTURE_UNAVAILABLE`；带 `op` 且无 `capture` grant → `UNAUTHORIZED`（用 `AccessContext` 构造只读凭据） |
| 授权矩阵 | 同上 | 只读凭据：查询 200、op `UNAUTHORIZED`；`capture` grant：op 进入桥（用假 `McpState` 返回 `MEDIA_OP_DROPPED`） |
| 客户端超时常量 | 同上 | snapshot 30s、`recordStart` 30s、`recordStop` 30s（三个常量断言；**不存在**依赖录制时长的公式） |

### 7.3 ignored 真机/集成测试形态

| 测试 | 前置 | 断言 |
|---|---|---|
| `agent_snapshot_writes_png_for_bound_dut` | `DUTHUB_TEST_CAPTURE_VIDEO` | 经 op 链路出 PNG；配置 `1024x600` 时 `artifact.width/height == 1024/600`；`artifact.bytes` 等于文件大小；文件在 `<root>/<DUT 名>/` 且目录有 `.duthub-dut-id`；`artifact.note` 原样回显（**文件名仍是 `<uuid>.png`**，`note` 不做前缀） |
| `ephemeral_capture_idles_out` | 同上 | 启动后无租约 → `idleSecs` 到期后 `sessions` 与 `demand` 均无该 key、`MediaUrls` 无该 DUT 键、进程退出 |
| `manual_start_survives_idle_timeout` | 同上 | `Manual` + 无其他租约 → 超过 idle 仍 `ready()` |
| `recording_runs_until_record_stop` | 同上 | `recordStart` → 等 10s（**不调用任何停止接口，也不设任何上限**）→ `recordStop`；`durationSecs ∈ [9, 12]`（实测墙钟；测试可用 ffprobe 复测，工具只回实测值）；`rotated == false` 且 `firstPath == path`；`path` 与 `poll_recording` 末段一致；10s 期间 `is_recording()` 恒为 true |
| `recording_pins_ephemeral_capture` | 同上 | 只做 `recordStart`（无 Manual 租约），把 `idleSecs` 设 1 → 远超 1s 后仍 `ready()`、`sessions` 有该 key；`recordStop` 收尾后再过 `idleSecs` → 会话被回收、`recordings`/`demand` 均清空 |
| `record_stop_after_binding_change_returns_ended` | 同上 | 录屏中 `reconcile_bindings` 停采 → 之后的 `recordStop` 在 1s 内以 `Err(MEDIA_RECORDING_ENDED)` 到达（**不是**客户端超时）；响应里**没有** artifact 字段 |
| 浮窗 | **无自动化**（真实窗口与 DPI） | 手工清单：① 浮窗不夺焦（终端仍能键入）；② 主窗最小化帧继续更新；③ 主窗关闭进程退出；④ 关侧栏/切 Shell 浮窗继续出帧；⑤ X 关浮窗后右侧栏恢复；⑥ 连续 5 次浮窗内存平稳（图集无泄漏） |

### 7.4 门禁

- 既有测试零改动：`git diff --stat` 对 §1.2 全表文件应为空；`docs/examples/settings.json` 不在 diff 里。
- 阶段收尾：`cargo fmt --all --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace`；真机项用 `--ignored` 单独跑并记录。
- 并行上下文用隔离 `CARGO_TARGET_DIR`（坑 #10/#16）。

---

## 8. 未决项

> **编号保持历史编号**（便于对照历次评审记录）：删除的条目不再占用编号，因此 8.2/8.5/8.9/8.13 空缺。

### 8.1 **已否决**：WebRTC 自研分发与外部中继（go2rtc / MediaMTX）

**结论（2026-11-24，用户拍板）**：本项目**只做纯 FFmpeg 路线**——不引入 go2rtc、不引入 MediaMTX、不引入 webrtc-rs，也不引入任何常驻外部进程或新的 tooldown 下载源。本设计不为它们预留抽象（§2.4 的范围判据）。

**实测/调研数字（保留，供将来重开时引用）**：

| 方案 | 成本 | 体积/许可 | 结论 |
|---|---|---|---|
| go2rtc 中继 | **3–6 人天**（启动/健康检查/YAML 生成/`capture.url` 中继/浏览器联调） | MIT；win64 zip **8.1 MB**（8,123,970 B，sha256 `5b448fc9…`） | 可行但**否决**：引入第二个采集进程，违背「纯 FFmpeg」与 R10 |
| MediaMTX | 6–9 人天 | optional（同一调研结论） | **否决**：同上 |
| webrtc-rs 自研（WHEP + ICE/DTLS-SRTP + H264 payloader + TS 解复用） | 演示 5–8 / 产品级（视频 only）**15–25 人天** | MIT/Apache-2.0；+12–15% 二进制、新增 ~48 个 crate；库内**无 MPEG-TS 解复用** | **reject**：投入产出不成立，且 0.x minor 可能破坏性升级 |

- 三个方案当初的共同收益是「浏览器直看」；本设计不提供该能力，浏览器/外部工具仍从 `capture.url`（本机 `/live/*.ts`）拉流，靠 URL 保密（§1.3 第 2 条）。
- **重开条件**（只在下列任一成立时才重新评估，且必须重走签字）：产品要求浏览器直看且分发物不得含第三方可执行文件；或需要 <200 ms 延迟且并发观众 >2；或出现「第二个采集进程」的硬需求。重开时从本表数字起步，不要再从零调研。

### 8.3 **已签字**：`media_streams.op` 与 `DutAction::Capture`

- **签字结论（2026-11-24，用户指示）**：Agent 可控制截图与录屏（静帧 + `recordStart`/`recordStop` 配对，**不设时长上限**），范围限本机采集——不触达 DUT 电源、串口、烧录。
- 因此 §5.3 的设计按原样实施：新增 `DutAction::Capture`，op 分支单独校验，**默认无人持有 → fail closed**；**代码路径按 grant 正常裁决**，不加任何硬编码屏蔽（测试可构造 `capture` grant 验证桥与 PNG 链路，与 §7.2 的授权矩阵一致）。
- 「门不开」由**授予**层面保证：发布/运维侧不给不需要的凭据授予 `capture`；本地管理员（`Principal::LocalAdmin`）按既有语义直接放行。
- 与 AGENTS「认证通过也不能使用任何写工具」的并置表述：`capture` 属**本地采集能力**（写本机磁盘、占本机配额），不在控制写工具集（`serial_write`/`control_*`/`flash`/`adb_shell`）内；发布说明里按这一句写。
- `grant` 机制与 `authorize_any_write` 语义**不变**：`capture` 不能换控制租约（§5.3 同步点 ②）。

### 8.4 **已关闭**：外部源的 `-c copy` 中继

只对「第二个采集进程 / 外部源」有意义，随 §8.1 否决一起关闭。事实留存：`start_with_args` 在输入参数之后**固定**追加 `-c:v libx264 …`（`media.rs:171-188`），任何外部输入都会被转码；要 `-c copy` 必须给 `Capture` 增加「跳过编码参数」的能力（改 `media.rs`，R2）。本设计没有外部源，因此不做。

### 8.6 内置 ffmpeg 的 socket 级读者计数

需要给 `Capture`/`Stream` 加 `active_readers()`/`last_reader_at`（改 `media.rs` 的 `pull`，`media.rs:554-580`）。收益：ephemeral 会话不会在浏览器仍拉流时被回收、`media_streams` 可报真实读者数。v1 用 `CopyUrl`/`ExternalPreview` 的 hold 租约替代（§4.6）。

### 8.7 **已定**：扩展 `media_streams.op`，不新增工具

结论（v5 定稿）：不新增 `media_snapshot` / `media_record` 之类的新工具，能力挂在既有 `media_streams` 的 `op` 参数上。理由是 4 处被钉住的数量契约（`agent_cli.rs:520-532`、`:667`、`plan_badges.rs:82`/`:97`、`dut_binding.rs:34`，见 §5.2）——新增工具要同步改这些测试，属于「改既有测试」；而 `op` 参数不改工具名、不改白名单、不改 Plan 面计数。

代价（接受）：工具 schema 变复杂（两个变体 + 可选 `note`），授权粒度只能到「整个 op 分支要 `capture`」。将来若 Plan 面要扩容，可把两个 op 拆成独立工具一并处理。

### 8.8 「总在最前」的浮窗

Windows 只有 `WindowKind::PopUp` 能置顶（`WS_EX_TOPMOST`），代价是无原生边框、只有顶边缩放热区、运行期无改置顶 API。v1 用 `Normal`；要置顶需接受自绘标题条与自定义 `NCHITTEST`（四边缩放自补）。

### 8.10 `media_streams` 查询分支的授权口径

查询继续用 `authorize_read`（要求该 DUT 的 read grant，比 `authorize_any_read` 更严），与 AGENTS #44/#64 对「新增读类工具」的期望有偏差。统一口径属权限变更，单独立项并核对 `tests/dut_binding.rs` 与 `docs/API_CONTRACT.md`。

### 8.11 长录屏轮转的中间段无法枚举

`Capture::record` 只返回首段、`poll_recording` 只返回末段，轮转时中间的 `.ts` 路径被 `path = new_path` 覆盖（`media.rs:401-409`），duthub 事后无法枚举它们。当前契约只报 `firstPath`/`path`/`rotated`（§5.4）。**录屏不设时长上限后这条缺口更容易被撞到**：长录屏会按 `min(quota/4, 1 GiB)` 反复轮转，中间段确实存在却无人知道路径。要补齐需要给 `Capture` 加「段路径回调/列表」（改 `media.rs`，R2）或改用独立目录 + 目录扫描（绕开 `DutQuota::create` 记账）；v1 不做——调用方拿到的是「首段 + 末段 + 是否轮转」，需要完整时间线时应缩短单次录屏、分多次 `recordStart`/`recordStop`。

### 8.12 看门狗到期后的孤儿 artifact

`JOB_TIMEOUT`（`started_at + 15s`）/`RECORD_STOP_GRACE`（`stop_requested_at + 5s`）先于 worker 内部超时到期时只可能出现在「12s 内 ffmpeg 已写出 PNG / 录制线程已写完末段、但结果回传被阻塞」的极端情形。GUI 到点回 `MEDIA_OP_TIMEOUT` 并丢弃迟到的结果，那个 PNG/TS 会留在 `<root>/<DUT 名>/` 里并已被 `DutQuota::create` 记账（不是泄漏，只是孤儿）。彻底解决需要 worker 在写盘前与 GUI 握手（改 `media.rs` 的 `screenshot` 契约，R2），v1 不做；DUT 目录清理统一回收。

---

## 9. 实施车道与文件所有权

### 9.1 车道表（一个文件只有一个写者）

| 车道 | 文件（写权限） | 阶段 | 依赖 |
|---|---|---|---|
| **A 配置与授权（core）** | `crates/duthub-core/src/model.rs`（`CaptureSettings.idle_secs`，1 个字段 + skip）<br>`crates/duthub-core/src/media_op.rs`（新，op DTO：`MediaOpRequest`/`MediaOpReply`（含 `RecordingHandle`）/`MediaArtifact`/`MediaOpError`/`RecordingId`/`OpReceiver`）<br>`crates/duthub-core/src/lib.rs`（1 行 `pub mod media_op;`）<br>`crates/duthub-core/src/agent_access.rs`（`DutAction::Capture` + `Unknown` + `as_str`）<br>`crates/duthub-core/tests/capture_idle_roundtrip.rs`（新） | P1 | 无 |
| **B 纯策略（lib）** | `apps/desktop/src/capture_backend.rs`（新：账本、`effective_idle`、`validate_idle_secs`、`resolved_capture_settings`、PNG 解析、错误码；**不含**录屏登记簿——它要 `Source`，放 bin，§2.4）<br>`apps/desktop/src/capture_ui.rs`（新：浮窗状态机）<br>`apps/desktop/src/lib.rs`（模块声明）<br>`apps/desktop/tests/capture_backend_logic.rs`（新）<br>`apps/desktop/tests/capture_ui_logic.rs`（新） | P1 | A（用 `CaptureSettings`） |
| **C 桌面（唯一写者，内部串行 C1→C4）** | `apps/desktop/src/media_page.rs`<br>`apps/desktop/src/workspace.rs`<br>`apps/desktop/src/mcp_runtime.rs`<br>`apps/desktop/src/side_panel.rs`<br>`apps/desktop/src/monitor_window.rs`（新）<br>`apps/desktop/src/pages.rs`（`GRANT_ACTIONS`）<br>`apps/desktop/src/main.rs`（窗口钩子） | P2–P3 | A、B |
| **D MCP 工具面** | `crates/duthub-mcp/src/server.rs`<br>`crates/duthub-mcp/src/state.rs`<br>`crates/duthub-mcp/src/skill.rs`<br>`crates/duthub-mcp/tests/media_ops.rs`（新）<br>`docs/API_CONTRACT.md`、`docs/SECURITY.md` | P2（可与 C 并行） | A、B（用 lib DTO） |
| **G 文案与文档** | `apps/desktop/locales/zh-CN.json`、`en-US.json`（**不碰 `agentToolSurface` 的「13 个」文案**；新增键：`captureIdle` 标签、`invalidCaptureIdle`、`captureRestartRequired`、`agentRecording`（「Agent 正在录屏 · 已录 Ns」）、`mediaRecordingEnded`、`mediaRecordingUnknown`，以及 §5.7 的 `MEDIA_*` 文案中英各一份；**删除**草稿里的 `invalidClipSeconds`）<br>`docs/ARCHITECTURE.md`、`README.md`、`AGENTS.md`（新坑回写） | 随各阶段 | 各车道交付后 |

> v4 的 H 车道（外部副作用进程 `tool_supervisor.rs` + `spec.rs` 门控）随 §8.1 取消；v4 中 `agent_ui.rs`（保留端口）与 `a2a_runtime.rs`（端口透传）的改动不再需要——本设计不新增任何监听端口。

**C 车道内部顺序**（同一写者，避免文件双写）：

| 子阶段 | 内容 | 涉及文件 |
|---|---|---|
| C1 | `In::CaptureIdle`（4 个同步点，§3.5）+ `save_capture_idle` + 面板字段 + `resolved_capture_settings` 接线 + `set_capture_settings`/`capture_settings`/`set_artifact_quota` + `side_panel` 换数据源 | `media_page.rs`、`workspace.rs`、`side_panel.rs` |
| C2 | op 桥（`MediaOpPort`、`drain_media_ops`、`finish_recording`/`sweep_recording_stops` 落在 `tick`、录屏登记簿 + `aborted` 墓碑）+ `mcp_runtime.rs` 接线 + 停采回绝 + 录制互斥 + `GRANT_ACTIONS` | `media_page.rs`、`mcp_runtime.rs`、`workspace.rs`、`pages.rs` |
| C3 | 账本/回收/租约配对（含 `stop_preview` 总是释放） | `media_page.rs`、`side_panel.rs` |
| C4 | `FramePump` 上提 + 浮窗状态机接线 + 窗口生命周期 | `side_panel.rs`、`monitor_window.rs`、`main.rs`、`workspace.rs` |

**明确没有写权限的文件**（任何车道都不得改）：`apps/desktop/src/media.rs`（R2）、`crates/duthub-device/**`、`crates/duthub-control/**`、`crates/duthub-tooldown/src/spec.rs`（本设计不加 spec）、`crates/duthub-core/tests/roundtrip.rs`、`crates/duthub-core/tests/tools_download_roundtrip.rs`、`crates/duthub-mcp/tests/dut_binding.rs`、`auth_hardening.rs`、`stdio_e2e.rs`、`apps/desktop/tests/agent_cli.rs`、`apps/desktop/tests/plan_badges.rs`、`docs/examples/settings.json`。

**为什么不再有跨车道交接点**：`In` 变体、`input_value`/`input_set`、`persist_settings`、面板渲染、`open_inline`/`close_inline_capture` 的调用方（`side_panel.rs:430`/`:480`）、`FramePump`、窗口钩子全部在 `apps/desktop/src/**` 内，由 C 一个写者串行完成；B 只提供 lib 纯函数与测试，D 只改 `crates/duthub-mcp`。

### 9.2 建议节奏与估算（v5 收窄后重算）

| 阶段 | 车道 | 人天 | 出口判据 |
|---|---|---|---|
| P1 | A + B | **3–4** | 新增单测全绿；`cargo test -p duthub-core` 与既有 roundtrip 全绿；`DutAction::Capture` 编译通过且无凭据默认持有；`capture_is_empty` 未被改动（`tools_download_roundtrip` 仍绿） |
| P2 | C1 + C2 + D | **5–6** | `media_streams` 无 `op` 响应逐字不变；`recordStart`/`recordStop` 的 schema、`recordingId` 归属校验与 §5.7 错误码齐备；授权矩阵按 §5.3 生效（无 `capture` grant → `UNAUTHORIZED`；测试构造 grant → 进入桥并回 `MEDIA_OP_DROPPED`，与 §7.2 一致）；**PNG 与录屏起停能力由 C2 的 bin `#[cfg(test)]`（`#[ignore]`，直接调 `MediaView` 不经 MCP）验证**；`idleSecs` 保存后只影响之后启动/尚未到期的会话 |
| P3 | C3 + C4 | **5–8** | 账本/回收/互斥单测；录屏期间 `Pinned` 与 `recordStop` 后恢复空闲的用例（§7.2）；浮窗手工清单（§7.3）通过；主窗关闭联动 |
| 签字后 | 真机验收 | **1** | §7.3 的 ignored 真机用例（快照/录屏起停/回收/打断）按 §8.3 的签字结论进验收 |
| — | 8.1（已否决） | **0** | 无工作项 |

合计（P1–P3）：**13–18 人天**（v4 为 15–22：先减去后端抽象、`stream` 配置段与端口预留接线；本次再把「秒数上限 + 到期自动停录」换成 `recordStart`/`recordStop` 配对，`clamp_clip_seconds`/`CLIP_DEADLINE`/`CLIP_GRACE` 三块机器消失，P2 −1、P3 −1）。签字后的真机验收 +1 人天。

### 9.3 实施红线复述

- 不改既有测试、不改 `media.rs`、不改 `duthub-device`/`duthub-control`、不改 §1.2 冻结的 `MediaView` 现有公开面。
- 不引入 go2rtc/MediaMTX/webrtc-rs 或任何常驻外部进程（§8.1 已否决）；不新增 tooldown spec。
- 不自动重启采集、不改端口分配、不改 16 路并发语义、不改编码参数。
- MCP 工具处理器一律非阻塞（R8）；跨线程等待只能 `async` + `tokio::time::timeout`。
- 真机（继电器/电源）首次实动仍需 escalate；本设计的真机项只有摄像头采集（只读本地视频设备），不触发该条。
