# duthub Agent 编码指令（每次会话必读）

Rust 工作区 + gpui-kit 0.9.1 桌面应用（串口调试 + MCP Agent 工作台）。所有经验教训沉淀于此，**开始任何修改前先读完本文件**；修完新坑后必须回写本文件。

## 硬约束

- **不引入 Python**（全 Rust，含测试与脚本；node/perl 仅限既有资产生成器）。
- UI 一律用 gpui-kit 组件（`gpui-kit = "0.9.1"`，facade crate）；新组件先查 `component` 模块，不要手造。
- 每个阶段收尾：`cargo fmt` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace` 全绿。
- 提交信息用中文，概述做了什么与为什么；不 push。

## 品牌命名规范（2026-11-20 全量统一，勿回退）

- **产品字样唯一为 duthub**：所有代码、文档、UI 文案、生成器模板中的品牌字样一律 duthub，不得使用其他历史名号。
- 标识符：crates `duthub-{core,device,adb,control,flash,mcp,agent}`；包名 `duthub-desktop`（bin 为 `duthub`）、`duthub-tui`；bin `duthub-mcp`（exe `duthub-mcp.exe`）；技能目录 `.agents/skills/duthub-*`。
- 配置目录为 exe 同级 `.duthub/`。运行数据目录统一为 `duthub-data/`。
- keyring service 为 `duthub`（条目 `model/<model_id>`），不做兼容回退；更换命名后模型 Token 需重新录入一次。
- 生成器（scripts/gen-icons.pl 等）输出模板为 duthub 字样，重新生成不得引入其他名号。

## Rust 编码与迁移规范

以下规则适用于本仓库的新代码和改动部分。工作区已有配置与本文件的硬约束优先。保持改动聚焦，格式化时避免夹带无关重排。

### 格式与命名

- 遵守 Rust 官方默认风格以及工作区的 `rustfmt` 配置。用 `cargo fmt --all` 格式化，用 `cargo fmt --all --check` 验证。采用四空格块缩进，多行参数与字段保留尾逗号。类型使用 `UpperCamelCase`，函数、变量和模块使用 `snake_case`，常量使用 `SCREAMING_SNAKE_CASE`。 [The Rust Style Guide](https://doc.rust-lang.org/style-guide/)

### API 与实现

- 优先保持代码易读。显式导入符号，作用域尽可能小。可借用时传 `&str` 或切片等引用，避免为传参复制数据。公开接口中的多个操作或状态用命名枚举和结构体表达。业务与硬件错误用 `Result` 返回并保留上下文。仅在不变量明确时用带原因的 `expect`。优先安全 Rust，必要的 `unsafe` 需隔离并用 `SAFETY:` 说明安全前提。库 API 从严，应用内部按实际复杂度取舍。指南中的 Chromium、Fuchsia 等专属规则仅适用于其对应环境。 [Google Rust Style Guide](https://google.github.io/styleguide/rustguide.html)

### 重构与回归

- 先记录现有行为契约和验收入口，再把改动拆成可独立验证的小步骤。跨语言迁移先从依赖少、有测试的模块开始，同一阶段优先保持外部行为。涉及串口、继电器、权限或平台边界时，新增回归测试应能复现问题，并覆盖正常流程、失败与收尾。每个阶段跑现有端到端测试。出现失败时修实现，保持既有断言和快照的验收标准。审查时核对行为契约、状态生命周期、取消与错误路径。 [GitHub Copilot Runtime 的 Rust 迁移实践](https://github.blog/ai-and-ml/generative-ai/migrating-the-github-copilot-runtime-to-rust-using-copilot/)

## 构建与运行

- 旧 HTML 原型的静态预览脚本放在 `.agents/skills/duthub-regression-test/scripts/`，由脚本定位仓库根目录。产品验收走 Rust 桌面版和 `scripts/regression`。

- release 构建前必须 `taskkill /F /IM Duthub.exe`（stdio 端为 `duthub-mcp.exe`）：**链接失败 os error 5 时旧 exe 会静默继续跑**，表现为"改了没生效"。
- 本机 fxc.exe 路径由 `.cargo/config.toml` 的 `GPUI_FXC_PATH` 提供；CI 由 workflow 自行探测。
- MCP HTTP 仅监听回环。主端口默认 `127.0.0.1:8797`，附加端口使用各自凭据，监听拓扑重启生效。

## 已踩过的坑（勿重蹈）

1. **gpui WCA 命中**：容器设 `WindowControlArea::Drag` 后，其内所有交互子元素点击会被系统判为拖拽（命中回调取第一个 WCA hitbox）→ 每个交互簇加 `.occlude()`。
2. **kit Button 在 Dialog 浮层内**渲染异常（无文字色块）→ 浮层内按钮用手绘 div 方案。
3. **kit Notification 背色**读 `theme.tokens.popover`（legacy 面，不随 colors 自动同步）→ `theme.rs apply_brand` 必须同时注入 tokens.* 与 colors.*，否则白底白字。
4. **`env!("CARGO_MANIFEST_DIR")`** 在编译期烘焙绝对路径，且 cargo 指纹不含路径——目录改名后 workspace 测试会读到失效路径 → 测试里用 `std::env::var("CARGO_MANIFEST_DIR")` 运行时读取。
5. **`ClickEvent` 没有 stop_propagation** → 用 `cx.stop_propagation()`。
6. **keyring 3.6** API 是 `delete_credential`；**tokio-serial 5.5** 用 `SerialStream::open(&builder)`。
7. **串口初次打开失败**（从未连上）只报错一次并结束任务；仅"曾连上后断线"走指数退避重连，否则被占端口会无限刷错。
8. **脚本体改 PowerShell 自动化脚本时**：类名与方法声明要成对（WinDrv 缺 `GetWindowThreadProcessId` 曾导致截图永远 WINDOW NOT FOUND）。
9. **工作目录解析必须收口**：桌面工作目录经 `ensure_portable_work_dir` 解析（CLI `--workdir` > 保存的 `workspaceDir` > exe 所在目录）。DUT 资料直接写入工作目录下按名称建立的子目录。settings.json/duts.json 永远留在 exe 同级的 `.duthub/`，逐文件补齐默认值且保留既有配置；**settings.json 进程内只准加载一次**（`load_settings` 对损坏文件有改名备份副作用，读两遍会把损坏告警吞掉）——desktop 在 main.rs 加载后经 `StartupContext` 注入 Workbench。
10. **多上下文并行时共享 `target/` 会把 link 卡死**（PDB 竞争，LNK1318 反复出现且 link.exe 挂 28 分钟不退）：验证/测试类构建用 `CARGO_TARGET_DIR=<隔离目录>` 跑，等对方工作流结束再合流；不要杀对方持有的 Duthub.exe。
11. **stdio 快照发现要匹配桌面端写盘路径**：`--workdir` 优先于 `workspaceDir`。指定的根目录指向 DUT 资料的上级，桌面快照位于根目录下的 `duthub-data/runtime/mcp-state.json`。旧的直接存放在根目录 `runtime/` 下的快照仍可发现。跨进程启动时建议配置绝对路径。
12. **D: 盘满（0 可用）时隔离 target 会 os error 112**：`CARGO_TARGET_DIR` 隔离目录放有空间的盘（C: 盘；全量 workspace 隔离 target 约 50G+，gpui 链从零编译约 20 分钟）。开建前先 `df -h` 探空间，别把半成品 target 留在满盘上挤垮并行工作流。
13. **审批动作清单三处同步**：新增控制动作（如 `control.test`）要同时改 ① duthub-mcp `server.rs` `approval_request` 的 `allowed` 列表、② 共用入口的校验/消费处（ControlAction 或独立方法如 `ControlService::channel_test`）、③ docs/API_CONTRACT.md 工具清单——漏一处会出现「能签发不能消费」或「能消费不能签发」。
14. **`input_set` 分支里先 clone 后写 dirty**：`self.duts.selected_mut()` 的借用若在 `self.duts.dirty = true` 之后还被使用（如 `d.heartbeat.clone()` / `d.id.clone()`）会 E0506——「改 DUT 字段 + 同步运行时」的分支，先把要克隆的值取完，`dirty = true` 放在对 `d` 最后一次使用之后（既有分支都是这个形状，新分支照抄顺序）。
15. **serde camelCase 容错的静默坑**：`#[serde(rename_all = "camelCase", default)]` 的配置结构（如 `HeartbeatConfig`），测试里 JSON 用 snake_case 键（`interval_secs`）**不会报错而是静默落 default 值**——解析"成功"但断言必炸。往返测试必须写 camelCase 键（`intervalSecs`）。
16. **C 盘隔离 target 可复用**：`C:/Users/luo-j/duthub-hb-target`（心跳/日志分割任务建的 warm 隔离 target；全 workspace check 约 1.5 分钟、test 数分钟）。并行期各自用隔离 target，全部工作流结束后再合流主 target，别在满盘 D: 上新建。
17. **kit Dialog 关闭路径默认有四条（Esc/遮罩点击/右上角 X/按钮）**：「需要人工干预」这类由代码管理生命周期的弹窗必须 `overlay_closable(false) + keyboard(false) + close_button(false)`，关闭只走显式按钮——否则 Esc 关掉后 Workbench 的 `alert_dialog_open` 残留为 true，后续告警永不弹；再配每帧 `window.has_active_dialog(cx)` 自愈兜底（渲染层只能读这个判定，`WindowState.active_dialogs` 是 pub(crate)）。
18. **告警弹窗与表单 Dialog 禁止叠层**：`close_dialog` 关的是**最新**一层——alert Dialog 若叠在 NewDUT 表单上，表单的「取消」会误关告警层。调度规则：只在 `!window.has_active_dialog(cx)` 时从队列 pop 弹出（apps/desktop/src/workspace.rs `render_root`），表单开着时告警排队等下一帧。
19. **SettingsFile 新增段必须 `skip_serializing_if = "…is_default"`**：`docs/examples/settings.json` 往返测试（crates/duthub-core/tests/roundtrip.rs，既有不可改）要求类型化 → 再序列化后语义相等——只加 `#[serde(default)]` 会让默认值段多序列化一个 key 直接炸既有测试。照 `workspace_dir`（None 跳过）/`tools.capture`（默认跳过）模式（如 `alerts` 段的 `alerts_is_default`）。
20. **MCP 线程 → GUI 告警回传走共享 `Arc<Mutex<AlertCenter>>`**：AlertCenter 同时被 Workbench 与 RuntimeMcpState 持有，弹窗本体只在 GUI 渲染层（render_root 每帧出队）打开；MCP 侧 `ApprovalRequired/*` 是审批协议正常环节（客户端走 approval.request），**不算动作失败、不弹窗**，只有 `ControlEntryError::Device` 转 AlertSource::Control。
21. **alacritty_terminal 0.26 集成要点**（duthub-device/src/term.rs，2026-11 终端仿真重构）：① `alacritty_terminal` 顶层 `pub use vte;`——**不要**单独加 vte 依赖；② 字节流入口是 `vte::ansi::Processor`（Term 实现 `ansi::Handler`），不是裸 `vte::Parser`；③ `Processor::new()` 在 let 绑定上类型推断会 E0283（默认类型参数不参与推断），必须写 `Processor::<StdSyncHandler>::new()`；④ `Term::new`/`resize` 的尺寸参数要自己实现 `grid::Dimensions`（tuple 的 impl 是 cfg(test)）；⑤ **ED2（CSI 2J）把被清内容卷入 scrollback 历史**（与真实终端一致），测试别断言"旧内容消失"；⑥ `grid.display_iter()` 只给可见区，回滚历史要自己遍历 `grid[topmost..=bottommost]`。
22. **终端快照事件分层**（SerialScreen 重构）：`UiEvent::SerialScreen` 只走总线供 UI 渲染（不进 .log）；审计/MCP serial_read 走 `SerialChunk`（RX 按读取块 utf8-lossy，TX 仅 .log 记 `[TX]` 行不再进终端流）；UI 侧 device 会话只把注记行（Dim/Error/Mark 样式）放进 `SessionView.lines`，设备输出一律来自快照——加新事件类型时先想清楚"进不进审计流"。
23. **drive-window.ps1 的 `-Key` 透传给 SendKeys**：`-Key Enter` 会**打出字面文本 "Enter"**（每个字符当按键），回车要写 `-Key "{ENTER}"`，退格 `"{BACKSPACE}"`。打印窗口截图是物理像素，GPUI `window.viewport_size()` 是逻辑像素（125% DPI → 1196 物理 = 957 逻辑）——涉及尺寸的 UI 判定按逻辑像素算。
24. **roundtrip 既有测试的默认值字段同样适用坑 #19**：`AdbConfig.tool` / `ToolsSettings.hdc_executable`（f650196 引入）曾因漏 `skip_serializing_if` 炸 `duts_example_roundtrip`/`settings_example_roundtrip`——给配置结构加任何"有默认值的键"都要配 `skip_serializing_if`（含嵌套在 DUT 内的段）。
25. **skill 生成链路（需求⑥，duthub-mcp/src/skill.rs）三处共用同一生成器**：MCP 工具 `skill_export`（state trait 方法，RuntimeMcpState 用 Workbench work_dir / ReloadingSnapshotState 从快照路径反推 `<workdir>/../`）、桌面设置页「导出 skill」按钮（workspace.rs `export_skill_md`）都落盘 `<工作目录>/skill/duthub/SKILL.md`。改工具清单时：server.rs `#[tool]` 方法名 ↔ skill.rs 工具表 ↔ docs/API_CONTRACT.md 三处同步（坑 #13 变体）。**stdio e2e 测试（tests/stdio_e2e.rs）别调用有落盘副作用的工具**（如 skill_export）——bin 的 `discover_data_dir` 在测试 cwd 下会退化成仓库内 `duthub-data/`，污染工作树；e2e 只断言 list_tools，落盘行为用单测 + 显式 temp 目录覆盖。

26. **终端键盘焦点应绑定到输出视口**：真实串口依赖设备回显，刚连接时可能只有「已打开」注记。不要把 `track_focus` 和 `on_key_down` 挂在滚动区尾端的空输入行。新开会话在表单 Dialog 关闭后聚焦视口，点击会话标签时重新聚焦；真机验收需在连接后不点终端直接按 Enter，确认收到对端提示符。
27. **CLI 直连独立于连接表单**：`--open-port` 在 `Workbench::new` 直接创建会话，也要按初始视图设置终端焦点请求。快照节流使用跨 `select!` 循环的 `interval`；循环内重建 `sleep` 会在设备持续输出时一直推迟屏幕更新。
28. **心跳命令输入需要草稿**：逐键修改时不能立即更新调度器。桌面端明确保存后，和 Agent 更新使用同一把锁落盘并发布配置。被租约拦截或未进入发送队列的探测不计为设备丢联。
29. **核对实际启动的 exe**：根目录 `Duthub.exe` 和隔离 target 下的验收构建是两个文件。交付便携版时核对启动路径和文件哈希。方向键和粘贴跟随设备通过 ANSI 切换的终端模式，勿用固定序列覆盖应用光标模式或 bracketed paste。
30. **gpui-kit 0.9.1 Button 的填充色读 `tokens.button*`**：切换主题时同时设置普通、Primary、Secondary 的默认/悬停/按下填充。只同步 `colors` 的文字色会让暗色主题的「保存绑定」和「刷新设备」不可读。
31. **无控制台 GUI 拉起 CLI 会闪出终端**：重定向 stdout/stderr 不阻止 Windows 分配控制台。后台 ADB/HDC/烧录探测及超时清理统一通过带 `CREATE_NO_WINDOW` 的进程入口。连接成功信令仍驱动串口状态和波特率回填，终端面板只隐藏该注记。
32. **已保存的空 DUT 列表是有效配置**：删除最后一个磁贴后仍保存 `duts.json`。缺失时初始化空列表，损坏时保留备份并回退空列表。删除确认绑定稳定 DUT ID，落盘后再移除磁贴和会话；已关闭会话的迟到事件不能重建标签。
33. **MCP 文档和回执反映实际运行时**：导出的 Skill 使用实际监听端口，各监听器分别落盘。串口写回执表示入队，设备执行需读回显核对。重试沿用同一 `requestId` 和正文。宿主禁用的动作在审批签发和消费前都应拒绝。新增响应字段应核对既有客户端对结构的依赖，stdio 空采集响应保持原有格式。
33. **共享继电器板与 DUT 通道绑定要分离**：板卡配置在 `duts.json.relayBoards`，DUT 使用板卡 ID 和各功能通道。迁移旧串口绑定时保留通道，在注册控制后端前解析为物理地址。下拉列表延后绘制，避免后面的表单控件盖住菜单。
34. **人工接管必须覆盖发送队列和控制执行点**：仅阻止 MCP 新请求会漏掉已入队的无租约兼容写入。串口队列携带接管代数，真实 I/O 和 Agent 继电器驱动派发持有运行门锁，无法安全切换时不宣告成功。
35. **DirectShow 设备枚举以 stderr 返回列表**：`-list_devices true -i dummy` 的结尾是枚举占位输入退出消息。界面显示解析后的候选；Windows 采集使用单个 `video=…:audio=…` 输入，面板预览订阅同一采集流。
36. **含普通 Rust 单测的 UI 模块避免 `use gpui_kit::*`**：通配导入会引入 GPUI 的 `test` 属性宏，普通 `#[test]` 可能递归展开。按需导入类型与组件扩展 trait。
37. **独立编辑只提交目标记录**：删除 DUT 从磁盘版本移除目标及提醒规则，再更新内存与关闭会话。不能借用全量保存的草稿校验，避免未完成的继电器表单拦住删除或被连带写盘。新工作区从空列表开始，CLI 临时直连不创建连接配置。
38. **按住测试的断开指令与闭合使用同一物理目标**：松键、失焦、离开页面和关闭窗口都释放，工作线程收到信号后发送 OFF 并在退出时完成收尾。实体动作只在明确授权的通道验证。
39. **调试设备的已保存绑定与实时枚举分开**：`duts.json` 中的序列号仅表示绑定。ADB/HDC 候选只取命令实际枚举结果，空列表时不回填历史设备，探测错误与未检测到设备分别显示。
40. **继电器测试异常不能锁住串口**：按住测试用继电器写入门保证独占，闭合或断开失败应返回错误并记录告警。不要用 DUT 全局 Protected 包裹继电器测试，否则断开失败会把仍在线的串口送进 NeedsInspection。回归测试覆盖正常释放、ON 失败及 OFF 失败后的实际串口 actor 输入。
41. **自建 input 的草稿臂必须成对**：`input_value` 读 `drafts` 的字段（如 A2aPort），`input_set` 必须同步 insert/remove 草稿——漏掉时解析失败（清空/中间态）不写模型，display 永远取回模型旧值，表现为「输入无法清空/回跳」。新增数值输入照 `In::Baud`/`In::HeartbeatInterval` 模式：非空 insert 草稿，解析成功才写模型。UI 检查轮 #9 修过 Baud 但 A2aPort 漏修（2026-11-18 补齐）。
42. **设置行内限制 gpui-kit Select 的外层宽度**：组件上的 `w` 只约束触发按钮。用固定宽度的外层容器对齐按钮和菜单，避免搜索菜单横跨整行。终端字号变更后，以选中字体的字宽重新计算串口及 USB Shell 网格。
43. **租约错误变体语义化（P2-019 族）的同步点**：给 `LeaseError`/`WriteIdentity` 加变体时同步三处——① `duthub-mcp/src/auth_http.rs` `lease_error_code` 的穷尽 match（编译器会拦）；② 分别验证取消与等待超时的测试（取消走 `Busy`，真正等待到期才走 `WaitTimeout`）；③ 行为门控开关（如 `gui_session_bound`）要核对**所有产品构造链**——`DeviceService::new` 与 desktop `main.rs` 自建 `ControlLeaseManager` 是两条链，漏一条修复在真实进程不生效（`--open-port` 直连实例正是自建链）。状态词（「等待超时」「队列前序」）永远不要塞进 `Busy.holder`——那是指向真实持有者的字段。

## 测试红线

- **区分租约取消与超时**：`cancel_wait` 移除请求后，原申请返回 `Busy` 并退出。验证 `WaitTimeout` 时保留持有者，让等待请求自行到期。

- 客户只分发 `duthub.exe`。HTTP MCP 无令牌、接入关闭或授权不匹配时默认拒绝。Plan 工具白名单同时用于发现和分发，认证通过也不能使用任何写工具。导出的 Skill 通过 MCP 返回正文，GUI 导出才写文件。
- 默认 DUT 资料目录直接与可执行文件平级，显式工作目录会改变边界。用稳定 DUT ID 验证文件夹所有权，从已保存的名称寻址。录屏和截图不得直接将外部可控文件名交给 FFmpeg，桌面不允许旧工作目录配置把输出重定向到上级目录。

- DUT 名称用连接管理的同一份草稿驱动输入框和连接标签。已打开的终端用稳定 DUT ID 查找同一份名称，避免改名后继续显示会话创建时的旧名称。

- **Token 与租约句柄是两层凭据**。HTTP 认证使用实际监听器绑定的 ID，不能信任客户端声明的端口。共享 Token 的 Agent 重复申请不得得到已有租约句柄，续期必须提交句柄。停用凭据时先阻止新写入，等待中的请求移除，已下发的设备操作经过执行门收尾后才能交接。
- **心跳主机名解析要区分内核启动输出与 Shell 响应**。`Linux version … (builder@lab)` 中的账户与主机是内核构建机。只从有效 Shell 提示符或 `uname` 输出提取 DUT 主机名。
- **多行心跳按一轮执行**：保持 `heartbeat.command` 字符串兼容，通过一次串口入队发送完整 Shell 命令组。匹配该轮起止标记后才提取系统信息和校验响应，避免第一条命令的输出或串口回显提前结束探测。
- **桌面会话租约和普通计时租约的生命周期不同**。设备会话打开和关闭成对管理 GUI 租约状态，通用管理器继续遵循配置 TTL。取消等待必须让原申请退出，避免轮询把它重新排入队列。

- **桌面 Agent 按 DUT ID 绑定当前串口**：切换终端时恢复该 DUT 的 Agent 草稿与进度。后台任务和告警始终使用原 DUT ID，不能从当前选中的设置项读取目标。
- **设备监控跟随当前串口的已保存视频绑定**：音频字段或别的 DUT 有视频时不能显示图标。切换目标先释放旧帧和预览，再打开新绑定，录制中的旧采集交给原任务结束。
- **采集入口共用已保存的启用状态与设备所有权**：连接管理中的手动启动也必须按 DUT ID 读取已保存绑定。保存停用、改绑或删除后，先撤销旧拉流地址并停止采集，再按新绑定显示预览。DirectShow 同名设备用替代设备 ID 区分，启动前校验所选端点。
- **桌面默认启用真实硬件**：打开串口下拉时重新枚举。真实枚举失败时不能显示演示端口。打开菜单或切换前景设置页时清除旧的下拉状态，避免后台的 deferred 列表盖住前景窗口。
- **采集能力按当前设备查询**：DirectShow 的 `-list_options true` 输出在 stderr，列完会退出。查询结果按 DUT、采集源和设备匹配。输出分辨率由 FFmpeg 缩放，与原生采集尺寸分开。
- **ADB Shell 与 Android 预览独立探测**。只有 Shell 的 Debian 设备不应因缺少 Android 工具而连接失败。Android 多显示器可能在 PNG 前写提示文本，应优先指定显示器 ID，并在有限前缀内识别二进制 PNG 头。自动获取序列号必须排除其他 DUT 的已绑定设备，候选有歧义时要求人工选择。
- **视频输出尺寸在采集后设置**：DirectShow 的 `-video_size` 只接受设备原生模式，UGREEN 35866 会拒绝 1024×600。输出尺寸用缩放并补边实现，保留画面比例。
- **设置输入使用 gpui-kit Input 和 Textarea**：编辑状态必须跨帧保留。只有外部模型变更或切换 DUT 等编辑目标时才刷新控件，逐帧写回会清空选区、撤销记录和输入法组合。
- **本地路径通过原生选择器回填当前输入的草稿**：Windows 的文件夹选择模式不能同时返回文件。支持两种路径的输入要分别提供文件与文件夹入口。异步结果回填前核对原来的编辑器和 DUT 范围。
- **重置终端同时更新网格和 ANSI 解析器**：只清界面缓存会被下一份串口快照覆盖，单独清网格会留下全屏模式和未完成的转义序列。右键菜单用捕获阶段的 `on_mouse_down_out` 关闭，确保外部控件仍可在同一次点击中响应。
- **VTE 同步输出需要宿主驱动超时**：串口读循环在快照计时器内检查解析器的同步期限，过期后调用 `stop_sync` 并重新发布快照。结束序列缺失且串口不再输出时也要刷新画面。
- **串口回显避免两级轮询**：空闲后的首个回显直接发布，连续输出按帧合并。桌面通过总线事件唤醒并分批处理，按键成功入队时等待设备回显。性能验收用 `--trace` 区分按键入队、驱动写入、串口 RX、快照和界面渲染。
- **创建连接复用连接管理表单**：新建草稿放在已保存 DUT 列表外。保存成功后一起发布 USB、采集、心跳和提醒绑定。切换到已有 DUT 再保存时，不得一并保存新建草稿。USB 下拉框以实时枚举的候选为准，选中设备后才写入设备标识。
- **首页磁贴的「…」按 DUT ID 打开同一连接管理表单**：不要恢复旧简版编辑弹窗或「留空自动发现」入口。
- **分割线宽度与终端列数同源**：拖动左右边栏时给中间终端预留宽度，串口网格计算扣除两侧实际宽度，Shell 使用右栏实际宽度。

- **USB 控制设备掉线后使句柄失效**：I/O 失败或 EOF 后清除驱动缓存句柄。下一条明确指令重新打开串口，只有幂等的 STATUS 可以自动重试。写操作结果不明确时不能重发 ON。
- **分段必须发生在控制指令前**：供电操作先落段再派发，结果注记追加到当前段。按 DUT 名称存放，查询按稳定 DUT ID 匹配，改名和重连都保留可查询的历史。原始文件只记录串口数据。

- **既有 E2E/回归测试不许删、不许改**来让门禁变绿（详见 scripts/regression）；发现测试挡路 = 实现有问题，先修实现。
- **"编译通过 ≠ 正确"**：所有已知回归都能编译。验收以 `bash scripts/regression` + self-test + 针对性端到端实测为准。
- 移植/重构期**禁止顺手优化**：一次只变一个变量，否则失败不可归因。
- 新发现的失败，修完后**把教训回写本文件**（失败→指令回路）。

## 并行协作

- 多上下文可并行**调查**；对同一文件的**修改**只归一个上下文（相邻代码无协调人禁止双写）。
- 对真实硬件（继电器动作、烧录、电源）的首次实动前必须 escalate 征求确认；只读探测（枚举/STATUS）不需要。
- COM25 继电器板：通道 1 的 ON→STATUS→OFF 往返已获用户授权并验证通过（2026-11，`relay_live_roundtrip` ignored 测试，`cargo test -p duthub-control --features real-hardware -- --ignored --nocapture` 可复跑）；其他通道首次实动仍需逐一授权，绝不同时动作两个通道。
- "自动完成"不含越界决策：改配置、动硬件、对外发布都要先问。
