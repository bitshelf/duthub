# duthub Agent 编码指令（每次会话必读）

Rust 工作区 + gpui-kit 0.9.1 桌面应用（串口调试 + MCP Agent 工作台）。所有经验教训沉淀于此，**开始任何修改前先读完本文件**；修完新坑后必须回写本文件。

## 硬约束

- **不引入 Python**（全 Rust，含测试与脚本；node/perl 仅限既有资产生成器）。
- **编译禁止使用 C 盘，只在当前仓库 `target/` 目录编译**：不得将 `CARGO_TARGET_DIR` 指向 C 盘或仓库外部路径；所有 `cargo build`/`check`/`clippy`/`test` 一律使用当前仓库 `target/` 目录（历史条目 #12/#16 中在 C 盘建隔离 target 的做法已废止）。
- UI 一律用 gpui-kit 组件（`gpui-kit = "0.9.1"`，facade crate）；新组件先查 `component` 模块，不要手造。
- **全球化 / i18n 约束（禁止硬编码中文文案）**：桌面 UI 所有可见文案（按钮、标签、占位符、toast、弹窗、告警）一律走 `crate::i18n::label` / `crate::i18n::message`，并同步维护 `apps/desktop/locales/zh-CN.json` 与 `en-US.json`（中文界面弹中文，英文界面弹英文）。所有库 crate（`crates/*`）禁止硬编码中文错误或告警字符串：需向用户展示的警告/状态返回类型化枚举由桌面层本地化，底层/系统/协议技术错误统一使用英文。
- 每个阶段收尾：`cargo fmt` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace` 全绿。
- 提交信息用中文，概述做了什么与为什么；不 push。

## 品牌命名规范（2026-11-20 全量统一，勿回退）

- **产品字样唯一为 duthub**：所有代码、文档、UI 文案、生成器模板中的品牌字样一律 duthub，不得使用其他历史名号。
- 标识符：crates `duthub-{core,device,adb,control,flash,mcp,agent}`；包名 `duthub-desktop`（bin 为 `duthub`）、`duthub-tui`；bin `duthub-mcp`（exe `duthub-mcp.exe`）；技能目录 `.agents/skills/duthub-*`。
- 配置目录为 exe 同级 `.duthub/`。运行数据目录统一为 `duthub-data/`。下载的外部工具（adb/ffmpeg 等）是持久静态资产，安装在 `.duthub/tools/<tool>/`（`manifest.json` 记录 exe 绝对路径，duthub-tooldown 写入），属于配置目录的扩展而非运行产物，不进 `duthub-data/`；桌面解析优先级 = 显式设置路径 > 已下载（`duthub_desktop::tool_resolver`）> PATH。
- keyring service 为 `duthub`（条目 `model/<model_id>`），不做兼容回退；更换命名后模型 Token 需重新录入一次。
- 生成器（scripts/gen-icons.pl 等）输出模板为 duthub 字样，重新生成不得引入其他名号。

## Rust 编码与迁移规范

以下规则适用于本仓库的新代码和改动部分。工作区已有配置与本文件的硬约束优先。保持改动聚焦，格式化时避免夹带无关重排。

### 格式与命名

- 遵守 Rust 官方默认风格以及工作区的 `rustfmt` 配置。用 `cargo fmt --all` 格式化，用 `cargo fmt --all --check` 验证。采用四空格块缩进，多行参数与字段保留尾逗号。类型使用 `UpperCamelCase`，函数、变量和模块使用 `snake_case`，常量使用 `SCREAMING_SNAKE_CASE`。 [The Rust Style Guide](https://doc.rust-lang.org/style-guide/)
- **代码注释一律用英文**（2026-11-21 起；docs/ 与本文件等文档保持中文）：新增或改动的注释用地道、通俗易懂的英文表达意图，不做逐字直译；术语优先采用专业术语与社区共识术语（如 `SAFETY:`、`TODO`/`FIXME`、rustdoc 惯例措辞）。字符串字面量、UI 文案与 locales 的语言不受此条约束。

### API 与实现

- 优先保持代码易读。显式导入符号，作用域尽可能小。可借用时传 `&str` 或切片等引用，避免为传参复制数据。公开接口中的多个操作或状态用命名枚举和结构体表达。业务与硬件错误用 `Result` 返回并保留上下文。仅在不变量明确时用带原因的 `expect`。优先安全 Rust，必要的 `unsafe` 需隔离并用 `SAFETY:` 说明安全前提。库 API 从严，应用内部按实际复杂度取舍。指南中的 Chromium、Fuchsia 等专属规则仅适用于其对应环境。 [Google Rust Style Guide](https://google.github.io/styleguide/rustguide.html)

### 重构与回归

- 先记录现有行为契约和验收入口，再把改动拆成可独立验证的小步骤。跨语言迁移先从依赖少、有测试的模块开始，同一阶段优先保持外部行为。涉及串口、继电器、权限或平台边界时，新增回归测试应能复现问题，并覆盖正常流程、失败与收尾。每个阶段跑现有端到端测试。出现失败时修实现，保持既有断言和快照的验收标准。审查时核对行为契约、状态生命周期、取消与错误路径。 [GitHub Copilot Runtime 的 Rust 迁移实践](https://github.blog/ai-and-ml/generative-ai/migrating-the-github-copilot-runtime-to-rust-using-copilot/)

## 构建与运行

- **编译产物只落当前仓库 `target/`，严禁使用 C 盘**：不要设置指向 `C:` 盘或仓库外的 `CARGO_TARGET_DIR`，统一在当前仓库 `target/` 目录内完成编译与测试。
- 旧 HTML 原型的静态预览脚本放在 `.agents/skills/duthub-regression-test/scripts/`，由脚本定位仓库根目录。产品验收走 Rust 桌面版和 `scripts/regression`。

- release 构建前必须 `taskkill /F /IM Duthub.exe`（stdio 端为 `duthub-mcp.exe`）：**链接失败 os error 5 时旧 exe 会静默继续跑**，表现为"改了没生效"。
- 本机 fxc.exe 路径由 `.cargo/config.toml` 的 `GPUI_FXC_PATH` 提供；CI 由 workflow 自行探测。
- MCP HTTP 默认只监听回环（`127.0.0.1`）。`agentAccess.lanAccess` 显式开启时监听 `0.0.0.0`，同一端口同时接受回环与局域网地址，且强制 Bearer（认证关闭时 fail closed）。局域网为明文 HTTP，Token 与 DUT 数据可被同网段嗅探，仅限可信网络并配合防火墙；监听拓扑/绑定变更重启生效。
- 每台 DUT 的 MCP 端点是 `agentAccess.listeners[]` 中带 `dutId` 的监听器，配独立只读 Token（read grant 仅该 DUT），在「连接管理 → MCP 服务」维护；通用 Agent 接入页只显示/管理 `dutId = None` 的监听器。DUT 专用端点跨 DUT 请求返回 `LeaseRequired/UNAUTHORIZED`，`dut_files` 省略 `dutId` 时默认绑定 DUT。每 DUT A2A 预配置存 `dut.a2a`，Token 走 keyring `a2a/<dut_id>`。桌面按配置启动实验性回环监听器，任务状态仅存于内存。

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
25. **skill 生成链路（需求⑥，duthub-mcp/src/skill.rs）三处共用同一生成器**：MCP 工具 `skill_export` **只返回正文、不落盘**；桌面「导出 skill」按钮（workspace.rs `export_skill_md`）落盘 `<工作目录>/skill/duthub/SKILL.md`，带监听器地址时写 `skill/http-<port>/duthub/SKILL.md`。改工具清单时：server.rs `#[tool]` 方法名 ↔ skill.rs 工具表 ↔ docs/API_CONTRACT.md 三处同步（坑 #13 变体）。**stdio e2e 测试（tests/stdio_e2e.rs）别调用有落盘副作用的工具**（如 skill_export）——bin 的 `discover_data_dir` 在测试 cwd 下会退化成仓库内 `duthub-data/`，污染工作树；e2e 只断言 list_tools，落盘行为用单测 + 显式 temp 目录覆盖。

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

44. **DUT 专用 MCP 监听器的授权口径**：`AgentListener.dut_id = Some(...)` 的监听器必须走 `authorize_scoped_read`——对通用监听器保留「已认证可读全部 DUT 资料」的既有语义，对专用监听器严格拒绝跨 DUT 并默认绑定 DUT。新增日志/文件读工具时不要只加 `authorize_read`（会把通用 LLM Token 的全局读能力改坏），也不要只加 bound 检查（会绕开 Grant）。`log_segments`/`log_segment_read`/`dut_files`/`dut_file_read` 是历史上漏授权的高危点。
45. **每 DUT 端口的生命周期与草稿作用域**：MCP 端口存 `agentAccess.listeners[]`（`dutId` 区分），A2A 预配置存 `dut.a2a`，两者的端口都要纳入全局唯一性/占用探测（LAN 开启时按 `0.0.0.0` 探测）。绑定/端口变更重启生效；删除 DUT 必须联动删除 listener、凭据、租约与 keyring `a2a/<dut_id>`。`In` 草稿键是全局的，`DutMcpPort`/`DutA2aPort`/`DutA2aName` 必须在 `select_connection`/`show_new_connection` 清除并加入 `editor_scope` 的 DUT 作用域，否则切 DUT 会串值（坑 #41 同族）。
46. **快照尾随空白决定空格/退格的可见性（2026-11 串口性能任务）**：`snapshot_screen` 裁行尾空白会把"光标推进"型按键（空格、退格）的前后两帧变得逐字节相同，叠加无本地回显 = 用户看到"按键无效果"。规则：光标落在尾随空白区时保留空白至光标列（无光标行维持裁剪）；空白边界按**字符数**计（`cursor_char` 是字符偏移，与字节 `trim_end().len()` 混比在多字节行尾误判）。改这段必须带 term.rs 四个单测（尾随空格/退格擦除/行中光标/多字节）。
47. **串口日志落盘走独立写线程，逐条 flush 语义不变**：`emit_raw`/`log_tx`/`log_keystrokes`/`emit_mark`/`note` 全部经 `SessionCtx` 的 std mpsc 入队 → 每会话一个写线程执行与旧同步路径完全相同的 `SessionLogWriter` 调用（实时落盘、零丢失），磁盘 I/O 不再卡 RX 回显路径。要点：① 写线程 `recv()` 返回 Err（通道关闭且排空）才退出，`Drop` 发关闭 + condvar 3s 兜底等待——会话结束 = 日志完整收尾；② 供电分段要同步成败（失败拦截派发）→ `LogMsg::MarkSync` ack 通道，不许改成 fire-and-forget；③ `send` 失败（写线程失效）必须内联落盘兜底，静默丢消息违反零丢失红线；④ SerialChunk 总线发布保持在 `emit_raw` 内同步（MCP serial_read 源）。
48. **TX 合并与 real.rs 事件循环形状**：键盘突发合并（`absorb_keystrokes`，cap 256B，仅连续同租约 WriteBytes）要求循环支持"吸收途中拉出异型命令"——命令先进 `pending: VecDeque` 再逐条分发，异型命令退回队列，**禁止在事件臂里复制其他命令的处理逻辑**；`select!` 只在 pending 空时等待。键入审计按原始命令逐条落盘（`.bin` 字节与块边界都不变）。absorb 函数与单测放 feature-free 的 demo.rs（real 模块 `#[cfg(feature = "real-hardware")]`，默认门禁不编译 real.rs）。
49. **终端列表必须虚拟化**：4000 注记行 + 400 网格行每帧全量重建 div 是删除卡顿主因——`uniform_list`（gpui_kit 再导出 `::gpui::*`；gpui-pre-0.3.12 签名 `uniform_list(id, item_count, |range, &mut Window, &mut App| -> Vec<R>)`，闭包内 `WeakEntity::upgrade` 读状态）。行高必须显式 `h(row_height)` 统一（Mark 行的 mt/mb 归一进行内样式）；near_bottom 从 `handle.0.borrow().base_handle` 取 offset/max_offset（保留 65px 跟随语义）；外层视口不再 `overflow_y_scroll`（焦点/按键留在视口 div）。串口快照从 I/O actor 经总线到 UI 共享同一份 `Arc<ScreenSnapshot>`。启用 Serde 的 `rc` 特性保持既有 JSON 形状。
50. **「串口」设置段（ConfigPage::Serial）接线清单**：`SerialSettings`（highlightRules/backspace/enterEnding/scrollbackRows）照 alerts 同款 `is_default` 模式（roundtrip 不动，bool 字段配 `is_false` skip）；旧 `terminal.promptHighlight`（单条、从未接线渲染）在 `load_settings::migrate_prompt_highlight` 播种为规则首条，UI 已迁入串口页。键盘回车/退格字节在 `on_key_down` 走 `enter_bytes()/backspace_byte()`；滚动缓冲经 `DeviceService::set_terminal_buffer`（AtomicUsize，新会话生效，默认保留 1,000,000 行，实时快照独立默认 400 行）；串口日志分段设置与会话写入见 #158–159。高亮消费端在 `apps/desktop/src/highlight.rs`（纯函数 + 单测，避坑 #36）。
51. **写线程内广播 mark 错误注记必须在释放 `logs` MutexGuard 之后**：`apply_log_msg` 持锁期间调用 `on_mark_error`（其内部要重新 `logs.lock()` 写注记）会同线程重入 std Mutex 直接死锁——异步 `LogMsg::Mark` 失败路径曾带此雷。形状：锁内只取 mark 结果，锁外广播；`MarkSync` 的 ack 在注记广播之后发出，调用方拿到 Err 时注记已落盘。供电边界 sink 是独立的 `PowerBoundarySink`（返回 `DeviceResult<()>`，失败拦截派发，LOG-5），动作结果注记仍用不返回值的 `LogMarkSink`——后动作注记失败不得拦改动作成败。
52. **dev-dependency 的 feature 不传递**：duthub-device 真机测试经 dev-dep 使用 duthub-control 的 `Ch340RelayDriver` 时，必须在 dev-dep 上写 `features = ["real-hardware"]`——只开本 crate 的特性时 control 仍按默认特性编译，`Ch340RelayDriver::new()` 的 opener 全部报 `Unsupported`（症状：`channel_status` 报 `Device(Unsupported)` 而 `RelayDriver::open` 直连却正常——两条路径经的 opener 不同）。
53. **真机键入验收的测试形状**（`tests/com8_io_latency_live.rs`）：① 控制台连接时不会重印提示符——回显流为空就先发 CR 唤醒；② 启动期 tty 不回显键入——先等静默（2s 无新文本）再等提示符，Android init 到控制台 shell 需数分钟（等待窗 ≥300s，周期性 CR 唤醒）；③ 逐键延迟基线先排空再按**字符标记**匹配——"任意增长"判定会被启动洪流的缓冲块污染出 0ms 假数据；④ 快照行带 shell 提示符前缀（`# ab cd123`），断言不能假设行首；退格删的是**行尾**字符；⑤ 真机延迟基线（115200）：TX→回显 3.1–19.6ms（avg ~7-12ms，USB 适配器 RX 聚合造成 16-19ms 尖峰），**回显与快照同刻到达**（空闲首发布实机成立），突发 8 退格入队 28µs、擦除回显 13.7ms 内全部到达。

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

## 规格与文档

- 长期维护的项目资料从 [docs/README.md](docs/README.md) 进入。功能规格、计划和任务放在同一个 `specs/` 功能目录，项目原则见 [constitution](.specify/memory/constitution.md)。

## 并行协作

- 多上下文可并行**调查**；对同一文件的**修改**只归一个上下文（相邻代码无协调人禁止双写）。
- 对真实硬件（继电器动作、烧录、电源）的首次实动前必须 escalate 征求确认；只读探测（枚举/STATUS）不需要。
- COM25 继电器板：通道 1 的 ON→STATUS→OFF 往返已获用户授权并验证通过（2026-11，`relay_live_roundtrip` ignored 测试，`cargo test -p duthub-control --features real-hardware -- --ignored --nocapture` 可复跑）；**通道 1 = DUT 复位**（2026-11-21 用户确认接线），其他通道首次实动仍需逐一授权，绝不同时动作两个通道。
- COM8+COM25 组合真机端到端（2026-11-21 用户授权实动，**已验证通过**）：`crates/duthub-device/tests/com8_reset_boot_live.rs`（ignored，real-hardware）——复位脉冲 → 启动洪流（5110 字符/50 帧快照）→ 关闭排空 → 日志零丢失核对（.bin 5158 字节 ≥ 收到文本量）；键入延迟与空格/退格正确性走 `tests/com8_io_latency_live.rs`（同 ignored 形状，坑 #53 有基线数据）。运行前置 = GUI 实例必须先释放 COM8/COM25（串口互斥打开，GUI 占口时 open 干净失败）。duthub-device 的 `[dev-dependencies]` 反向依赖 duthub-control 构成合法测试环（坑 #52）。
- "自动完成"不含越界决策：改配置、动硬件、对外发布都要先问。

## 2026-11-21 Agent 集成收口（自动追加）

> 本节由收口文档轮自动追加（Agent 集成 `feat/agent-integration` 收口），只新增、不改/不重排既有内容；坑编号接续上文，详见 `docs/reviews/AGENT_IMPLEMENTATION_REPORT.md` §11。

### A2A 与签字边界

54. **A2A 只到签字为止**：ADR-2/8/9 全部 Proposed 未签字 → `duthub-a2a` 只做 experimental——仅绑 `127.0.0.1`（非回环 `bind_addr` 一律 `NeedsProductSignoff`，绝不绑 0.0.0.0）、Bearer fail-closed、`StrictRequestHandler` 对 ListTasks/GetExtendedAgentCard/push/subscribe 等统一 -32601；UI 在无 Running listener 时不得生成可复制的 `http://` Card URL，A2A 不进产品验收与发布说明。
55. **社区 RPC server 默认 fail-open，门控要自己装**：`a2a-server-lf` 不装 authorizer 就是不设防（HTTP 401 之外还要逐请求二次复核）；请求体上限必须用 `tower_http::limit::RequestBodyLimitLayer` 放在 JSON-RPC 路由**最外层**——库内自带的 10MiB `DefaultBodyLimit` 在内层，外面再包一层 `DefaultBodyLimit` 盖不住（实测 300KiB 返回 200）；`Vary: A2A-Version` 用 append，别覆盖 Card 路由既有的 `Vary: Origin`；集成前先读库的路由/参数解析顺序（RequestHandler 参数解析先于 handler 门控）。

### 依赖与 Cargo.lock

56. **Cargo.lock 多版本共存是正常行为**：新增依赖引入第二版本时，Cargo 会把既有依赖行重命名为带版本寻址（`"foldhash"`→`"foldhash 0.2.0"` 等 4 组）。删除行核验用 `git diff -U0 -- Cargo.lock | grep -E '^-[^-]'` 逐条归类；不要 `cargo update --precise` 强行统一版本（要么降级、要么越 owns 改区间）。
57. **社区 crate 先查默认特性与许可证**：`a2a-lf`/`a2a-server-lf`（Apache-2.0）默认 fail-open；`backon` 取 `default-features=false, features=["std"]` 只拿 1s/2s/4s 退避序列（不拉 tokio-sleep）；`fileloft-*`（MIT）只当协议层用，存储/锁自写；Job Object/ACL 用 `windows 0.62.2`（SetNamedSecurityInfoW + PROTECTED_DACL）。

### 端口与恢复

58. **端口注册表的 reserve 语义**：PortRegistry 对任何已有持有者拒绝 reserve——不能把 enabled DUT 的 `a2a.port` 全部预登记进 `A2aRuntime::start`（会把运行时自己的端口判 PortConflict）。正确形状：MCP 端口以 External 预登记，保存/启用时用「MCP + enabled DUT 合集」做冲突检测。MCP bind 失败要回传 failures 给 UI 展示，`eprintln!`+continue 等于静默丢端点。
59. **恢复状态机三件套**：① Fresh 与 Degraded 都必须先 `validate_extension_results` 再写 `<dut>/session-id`/`resume.json`——Degraded 落盘未校验会话等于把没验过的当验过的；② `resume.json{agentId,programSha256,sessionId}` 做权威判定，跨 Agent/换程序哈希一律按新会话并注记；③ Resumed 先 probe tools/list 复验工具面，失败转 Failed。**显式停止（含启动期每个提前返回）都要清标记**——清理进统一退出路径，别散在各分支。

### ACP 子进程与 OS 沙箱

60. **ACP 子进程 EOF 必须唤醒 pending 请求**：stdout EOF/读错误/进程退出要 `mark_closed` 唤醒全部 pending，否则 initialize/session-new 的 20s/30s 阻塞超时叠加 Drop join，关窗卡 20-50s；Drop 用有界 join，stop 要能取消退避等待。
61. **OS 沙箱边界**：目录 ACL 保护持久化资料。托管 Agent 使用 Windows 受限令牌和 Job Object，专用身份与读隔离的条件见 #142–145。重启预算「3 次/10 分钟」对应首启加最多 3 次重启。

### 工具面、审批与授权

62. **工具面校验不分分支**：extensionResults 维持「恰好 1 条 duthub success」（内置/未知扩展都拒，fail closed 是显式兼容策略）；goose 升级如新增内置扩展会 fail closed，需同步 allowlist 并加测试。
63. **审批令牌与脱敏**：审批令牌 CSPRNG（`apr-`）且审计只存指纹（token_sha256/dut/action/args 哈希），明文 token 进内存审计 = 泄露面；审计脱敏键名要归一化（小写、去 `_`/`-`、按后缀匹配 token/key/secret/password），否则 `access_token`/`api-key` 全漏；tool/dut 等客户端可控审计字段限长 + 去控制字符。
64. **通用 listener 授权口径**：保留 #44 全局读语义，但通用分支必须 `authorize_any_read`（零 grant 拒绝）、停用凭据即时拒绝（`credential_enabled` 合并持久表+overlay 复核）；`CurrentPrincipal` 在 access=Some 且无 HTTP Parts 时 401，不回退 LocalAdmin（stdio/证据 server 的 access=None 才是 LocalAdmin）。

### 桌面 UI 与测试形状

65. **注册表编辑器与删除联动**：args 编辑用 JSON 数组（空格 `join`/`split_whitespace` 会把带空格参数拆碎写回配置）；env 值用掩码编辑器写 keyring；删除 Agent 联动删凭据、删除 DUT 走显式五步清理序（可失败步骤先行、目录删除幂等），失败写 `<work_dir>/agent/pending-cleanup.json` 下次启动重试——不能只 toast 后谎称清理完成。外部程序必须绝对路径（盘符/UNC）。
66. **UI 纯逻辑下沉 lib target**：`duthub_desktop::agent_ui` 放纯函数 + `apps/desktop/tests/agent_ui_logic.rs`；bin 模块（workspace/pages 的 spawn 栈）无法在 lib test 实例化，用「源码契约测试」钉住（repo 既定模式）；lib target 无 i18n 模块，展示文案经枚举变体交 bin 层翻译。
67. **self-test 必须绕过落盘型加载**：`load_agents` 在 agents.json 缺失时会创建并写入默认文件——self-test 用内存默认值；验收断言 `! -f <workdir>/duthub-data/agents.json`、不绑 MCP 端口（netstat 前后无监听）。self-test 的 frames 是 1200ms 窗口内运行时观察值（`main.rs:238-245`，判定仅要求 `rendered != 0`），2/3 帧波动不是回归。
68. **transcript 贴底与截断**：内存 transcript 到 2000 条 push_entry 丢最旧、item_count 恒 2000——贴底判定用单调递增序号（`SessionView::total_entries()`），不是 `item_count > seen`；prompt 待清空做成 per-DUT map 且只在输入仍等于已发送文本时清空。
69. **demo 面板与真实会话隔离**：旧演示 AgentState/RunStatus/900ms STEP 面板会与真实 Agent 会话混淆——已从 side_panel 卸载产品渲染；完整删除会连带删既有单测（违反测试红线），迁移另立任务；Inspector 接真实 SessionView 未做。

### 外部 CLI 与并行协作

70. **rmcp 3.10.0 outputSchema 坑**：返回 `Json<Value>` 的 tool 会生成非 object `outputSchema`，Claude Code 2.1.204 直接拒绝注册全部工具（`Invalid input: expected "object" (at tools.0.outputSchema.type)`，`claude mcp get` 报 tools fetch failed）——修复属 duthub-mcp（typed 输出/显式 override/兼容层剥离）；真实 CLI 端到端在此修复前关闭不了。
71. **共享工作树的 fmt 竞态与瞬时链接失败**：并行上下文未格式化的文件会让 regression fmt 步瞬时失败——轮询等待对方自行格式化，不要替禁改文件跑 fmt；隔离 target 的 LNK1140/LNK1285 多为磁盘压力/PDB 损坏瞬时失败，删 `debug/incremental`（纯缓存）与损坏产物目录后复跑即可。

## 2026-11-23 zerocopy 记录流（.rec）（自动追加）

> 本节由 .rec 结构化记录流任务追加（格式规范见 `docs/reference/REC_LOG_FORMAT.md`）；只新增、不改/不重排既有内容；坑编号接续上文。

72. **zerocopy 0.8 无默认 feature**：derive 宏（FromBytes/IntoBytes/Unaligned/KnownLayout/Immutable）必须显式 `features = ["derive"]`，否则报 "cannot find derive macro … note: `X` is a trait"；`byteorder` 模块（`little_endian::U32/U64`）无 feature 门控。新增头结构统一 `#[repr(C)]` + 五个 derive 全上，解析用 `FromBytes::ref_from_prefix`。
73. **新日志流与 `list_segments` 的前缀冲突**：`list_segments` 按 `{session_id}.` 前缀整目录匹配，新加 `.rec` 会把既有「精确集合」测试（sessions_in_shared_dir…）顶爆——修实现不修测试：`list_segments` 收窄为只列 `.log`/`.bin` 家族（按首段扩展名 match），每条新流配独立枚举函数（`rec::list_rec_segments`，数值序轮换段）；MCP `log_segments` 只认 `.log`/`.txt` 后缀，天然隔离（动手前先 grep 确认）。
74. **多段 `.rec` 不能原始字节拼接后解析**：每段带独立 24B 文件头，拼接缓冲会在第二段头部的 `DUTHUBR1` 处停机——必须按段逐个 `parse_records` 再合并记录（LOG-9 红线锁定）。
75. **Mark 记录的写序与失败隔离**：`.rec` Mark 记录必须写在 `writer.mark()` **之前**（普通模式 mark 内含 split，后写会落进新段，与 `.log` 标记行落旧段的既有语义错位）；rec 失败不得进入 MarkOutcome::Failed——供电边界成败只看 `.log`/`.bin`（LOG-5），rec 是伴生流。
77. **chrono 0.4 `timestamp_opt` 秒与纳秒分开传**：只传 `(secs, 0)` 会把毫秒静默打成 `.000`——纳秒要 `timestamp_opt(secs, (ns % 1e9) as u32)`；格式化用 `time.format("%H:%M:%S%.3f")` 免 trait 导入（hour()/minute() 在 Timelike 上）。
78. **port 名直当 DUT 名会让 portable 归档静默丢掉全部日志**（2026-11-23 实机验收发现）：`--open-port`/「新建会话」在无已保存 DUT 时把端口名（"COM8"）兜底成 DUT 名传给 `open_named_session`，portable 归档的 `name_component` 因 Windows 保留名（COM1–9 映射 `_COM8` ≠ 原名）逐条拒绝——而 `apply_log_msg` 对 append 类失败 `let _` 静默（坑 #47 形状），表现 = 会话活着、键入回显正常、**`.txt/.bin/.rec` 一字节不落盘**。修复：`archive_safe_name`（workspace.rs）守卫——非法名换 dut_id（程序 slug 合法），三处兜底（auto-open/open_real_port/磁贴保存）全走它；LOG-12 锁 portable+写线程三流落盘。**遗留风险**：用户显式起保留名（如 "COM8"）仍会静默零写盘，写盘连续失败无任何用户可见告警——后续应给写线程加「首败广播错误注记」。

## 2026-11-23 工具下载 UI（设置页「工具下载」+ 下载代理，自动追加）

> 本节由工具下载 UI 轮追加（tooldown 进度回调 + `duthub_desktop::tool_ui` + 设置页两面板）；只新增、不改/不重排既有内容；坑编号接续上文。

79. **apps/desktop/tests 的源码契约测试钉住逐字锚点（含缩进与换行）**：`agent_ui_logic.rs::self_test_stays_pure_no_agents_json_no_listeners` 钉住 `if !self_test {\n            let (pending_done, pending_failures) =` 这样的原文片段——往既有启动门控块**内部**插入新语句会掐断锚点、整包测试炸一条；新启动期工作放进**自己的** `if !self_test` 块（或 `if !self_test && let Some(x) = …` 的 let-chain 形态），绝不嵌进既有门控块（修实现不修测试）。
80. **`dyn Fn` 裸别名默认 'static，借用局部状态的回调全被拒**：tooldown `InstallOptions.progress: Option<&'a ProgressCallback<'a>>` 必须把 `'a` 写进 trait object（`dyn Fn(DownloadProgress) + Send + Sync + 'a`）——不带 `'a` 的 `dyn Fn` 在该位置按 'static 解析，`&|p| slot.lock().push(p)` 这类借用闭包直接 E0597；结构体字段同理。进度消费形状 = 线程每 chunk 写 `Arc<Mutex<Option<InstallProgress>>>` 槽、UI tick 采样对比（变化才 notify），不在渲染路径同步等下载。
81. **reqwest `Proxy::all` 对怪 scheme 的 build 期静默**：`"nope://x"` 这类非 http/https/socks5 的 URL **build_client 照样成功**（连接期才失败，tooldown proxy tests 注释已载）——UI 保存代理前必须自己做 scheme 预检（`tool_ui::validate_custom_proxy_url`），别指望 build_client 拦；真正解析不了的（`http://[bad`、`http://`）build 期才报 InvalidProxyUrl，保存流两层都拦。
82. **下载进度只画真实百分比**：服务器不给 Content-Length（chunked 响应）时 `progress_percent` 返回 None——状态行显示已接收字节（`format_bytes`），**不画进度条**；未知总长画条 = 伪造进度（诚实红线）。kit 资产回退的 Lucide 是**子集**（`gpui-kit-assets` 的 default-icons.txt，无 download）——面板图标优先用本地 sprite 清单（`icons_gen.rs`，已有 `download`/`network`），kit 回退只兜 Lucide 默认集里存在的名字（如 `globe`）。
83. **reqwest 关 default-features ≠ 拿到 env-only 代理**（2026-11-24 门禁轮）：tooldown 对 reqwest 0.13 设了 `default-features = false`（省掉 `system-proxy`），但 Cargo feature 全图叠加——workspace 里其他 reqwest 0.12 默认特性使用者把 hyper-util 的 `client-proxy-system` 统一进来后，reqwest 默认 `auto_sys_proxy` 的 `ProxyMatcher::system()` 在 Windows 会把注册表 `ProxyServer`/`ProxyOverride`（含 `127.*` 旁路 → `127.0.0.0/8`）折叠进判定，静默盖掉 env 代理契约（症状：`proxy_env_behavior` 里设了死代理 `HTTP_PROXY` 请求却直连 200）。**env-only 语义必须自己读 env 并显式注册 `Proxy::http/https`（或无 env 时 `no_proxy()`）**——添加任何显式代理或调 `no_proxy()` 都会关掉 auto_sys_proxy（reqwest 0.13 没有单独关它的公开 API）；查找顺序对齐 hyper-util：大写优先小写、空串=未设、`ALL_PROXY` 双 scheme 回退、`REQUEST_METHOD`（CGI）全部不可信、`NO_PROXY` 经 `NoProxy::from_env` 挂到每个代理。依赖系统代理语义的话就直接承认并测试它，别靠"默认行为"。

## 工具下载收尾与并行运行复活

> 本节由工具下载收尾轮追加（goose 下载分发 + 逐工具更新 + 评审遗留修复）；只新增、不改/不重排既有内容；坑编号接续上文。

84. **被 amend 取代的旧运行会在 provider 限额重置后自动复活，与新运行双写同一批文件**：`AmendWorkflow` 把 dwfrun-5c7969cd 标为 superseded 后，共享账号的 5 小时/周限额一重置，**新旧两个运行都被平台自动续跑**（旧运行的 门禁修复 与继任运行的 UI 轮同时写 workspace.rs/pages.rs/locales）——旧运行方还以为「另一个上下文」是别的会话。处置：子代理报告「另一上下文在改同一批文件」时先查 `ListWorkflowRuns`，确认对方是不是被取代运行的复活；让旧运行一侧收手（答复其升级问题即可），必要时把两侧都停掉由主会话收尾。另：周限额（code 1310）会把该 provider 的全部子代理冻结到重置（本次 6 天），主会话换模型后可继续手工收尾，不必等。
85. **共享工作树里他人半成品阻断全树编译时的口径**：`crates/duthub-core/src/os_acl.rs` 被并发上下文保存成半成品（E0432 import 模块路径 / E0603 模块内私有 fn / E0271 cast 推断），mtime 静止 45+ 分钟后仍无活跃运行认领，阻断 `cargo check -p duthub-desktop` 及 `scripts/regression` 全树编译。口径（用户确认）：**只做最小补全或注释，绝不删除他人代码**——本次三处 = import 归到 `Win32::Security` 组、`sid_to_string` 改 `pub(super)`、迭代器 `ace.cast_const().cast()`；每一行都记入 caveats 供属主恢复后按原意图覆盖。动手前置三条同时成立：错误客观阻断全部门禁 + 属主长时间无活动 + 运行列表无活跃运行认领。

## 2026-11-24 门禁脚本 PATH 依赖（自动追加）

> 本节由门禁修复轮追加；只新增、不改/不重排既有内容；坑编号接续上文。

86. **`scripts/regression` 的仓库根解析不能依赖外部 `dirname`**：门禁由 Windows 进程直接拉起 Git Bash 时 PATH 可能没有 MSYS `bin`，`dirname: command not found` 使 `cd "$(dirname "$0")/.."` 落到无关目录，随后 `cargo fmt --check` 报 `could not find Cargo.toml in C:\Program Files\Git`——`set -e` 抓不到，因为 `cd` 到另一个已存在目录仍然成功。修复：用参数展开 `${0%/*}` 取脚本目录，并加 `Cargo.toml` 存在性守卫，让这类环境问题立刻以明确信息失败。门禁脚本只准用 shell 内建与 `cargo`。

## 2026-11-24 应用级进程容器 Job（自动追加）

> 本节由采集孤儿进程修复轮追加（强杀/崩溃后 ffmpeg/ffplay 仍占采集卡）；只新增、不改/不重排既有内容；坑编号接续上文。

87. **应用级进程容器 Job 只能设 `KILL_ON_JOB_CLOSE`，且句柄要泄漏到进程结束**：`fn main()` 第一条语句调 `duthub_desktop::agent_host::job::contain_current_process()`（`apps/desktop/src/main.rs`），把整个 duthub 进程树放进 Job 并 `mem::forget` 句柄——崩溃/任务管理器「结束任务」关句柄时内核杀掉全部后代（ffmpeg/ffplay/预览/录制），`Capture::Drop` 只兜正常退出。要点：① 容器 Job **绝不**带 `ActiveProcessLimit`/`ProcessMemoryLimit`（那会把整个应用的子进程预算压到 16 进程/4 GiB，桌面并发起多路 ffmpeg 时是自伤）；② goose 自己的 Job 仍走 `Job::create` 并**嵌套**在容器 Job 之下——Windows 8+ 允许，规则是子 Job 的进程集必须是父 Job 的子集，所以容器先分配、Agent 子 Job 后分配；实测见 `job.rs::process_already_in_a_job_can_join_a_nested_job`；③ 容器分配失败必须 fail-open（`eprintln!` 警告后继续启动），与 goose 子进程「分配失败即拒绝启动」的 fail-closed 相反；④ 句柄默认不可继承（`CreateJobObjectW(None, None)`），后代无法把杀死触发器续命；⑤ 测试**绝不**把 cargo test 自身放进 Job——用 `cmd /c ping -n 30` 子进程验证「只关句柄 10s 内被杀」，失败路径先 kill 再 panic。

## 2026-11-24 工具下载入口与门禁抖动（自动追加）

> 本节由工具下载入口轮追加（用户实机反馈「没看到下载/更新按钮」）；只新增、不改/不重排既有内容；坑编号接续上文。

88. **工具下载入口必须贴着对应路径字段，否则用户找不到**：下载面板原本只在「设置」页最底部（外观→字体→工作日志→工作目录→命令→配置文件→工具下载→下载代理），而用户在「采集」页的「采集工具」面板找 FFmpeg 下载按钮（该面板在 `media_page.rs`，是另一页）→ 实机反馈「没看到按钮」。修法：在「采集工具」面板直接加「下载/更新/取消 FFmpeg」按钮（`duthub_tooldown::spec_by_id("ffmpeg")` + `installed_executable` 读盘判定 + `start_tool_install` 复用同一安装中心/代理/进度槽），进行中显示真实进度并可取消。新增工具时两处入口都要有：设置页总面板（全工具）+ 对应功能页快捷入口。
89. **`goose_lifecycle::kill_terminates_grandchild_process` 在并行构建负载下会偶发超时**（`孙进程未写出 PID`）：该测试等孙进程写 PID 文件，另一上下文同时 cargo build 时窗口内等不到，单独复跑 4/4 通过；不是回归。门禁判红前先单独复跑该测试确认（同坑 #71 的瞬时失败族）；另注意 `btn()` 返回的 `gpui_component::Button` 的 `.disabled()` 来自 `Disableable` trait（`gpui_kit::component::Disableable`），不通配导入就报 "private field, not a method"。

## 2026-11-24 采集生命周期与浮窗（自动追加）

> 本节由采集生命周期/浮窗收尾轮追加（`media_streams.op` 按需取数 + 共享帧浮窗 + 录屏无上限）；只新增、不改/不重排既有内容；坑编号接续上文。

90. **调研外部二进制必须在项目内 `test-output/scratch/` 下运行，CWD 不得是仓库根**：MediaMTX 调研（结论见 `docs/design/CAPTURE_STREAM_BACKENDS.md` §8.1「已否决」）当时直接在仓库根启动它，它按 CWD 自签证书，根目录生成了 `auto.crt`/`auto.key`；本轮已清理（`ls auto.crt auto.key` → 不存在）。规则：下载的第三方 exe/脚本一律在自己新建的空目录里跑——`test-output/scratch/<主题>/`（`/test-output/` 已在 `.gitignore:28` 忽略），跑完整目录删除；仓库根只留给 `cargo` 与门禁脚本，别让调研产物落进工作树。
91. **磁盘：坑 #16 的 `C:/Users/luo-j/duthub-hb-target` 已于 2026-11-24 删除**（实测 `ls` 报 `No such file or directory`），别按旧笔记再去找它。并行构建确实需要独立 target 时，优先在项目内开（`target/par-<用途>`，`target/` 已被忽略），工作流结束就删掉合流；坑 #12 的「D 盘满时 os error 112」仍成立，先探空间再开建。
92. **`media_streams.op` 的 `DutAction::Capture` 有四个同步点**（坑 #13 同族，漏一处就是「能签发不能消费」或「能消费没界面」）：① 序列化 `DutAction::as_str()` → `"capture"`（`crates/duthub-core/src/agent_access.rs:173`），配置写成 `grants[].actions: ["read","capture"]`；② 授权 `McpServerCore::authorize_capture`（`crates/duthub-mcp/src/server.rs:545`），op 分支在 `:758` 调用，查询分支仍走 `authorize_read`，DUT 专用监听器的跨 DUT 拒绝由 `authorize` 的 `listener_dut` 检查兜底（`:504-508`）；③ 桌面授权矩阵 `GRANT_ACTIONS`（`apps/desktop/src/pages.rs`）独立管理「读」和「画面」授权；④ 文档 `docs/API_CONTRACT.md`（工具表 + 失败码）与 `docs/SECURITY.md`（采集按需取数）。另配 `crates/duthub-core/tests/capture_settings_roundtrip.rs` 的 `as_str`/JSON 键断言。
93. **gpui 多窗口：`Window::drop_image` 只清本窗图集**：浮窗一开就有自己的 sprite 图集，同一个 `Arc<RenderImage>` 同时在主窗侧栏与浮窗渲染时，谁用 `Window::drop_image` 谁只回收自己那份，另一份纹理永远不释放——每帧约 0.9 MB（帧是 640×360 BGRA = 921,600 B，`apps/desktop/src/media.rs:959` 的断言就是这个尺寸）。规则：换帧/清帧一律走 App 级 `Context::drop_image`（`cx.drop_image(img, window)` 顺带清当前窗，`cx.drop_image(img, None)` 清所有窗；`apps/desktop/src/monitor_window.rs:101`、`:176`，侧栏 `side_panel.rs:309`）。配套不变量：解码帧只能有一个消费者——生产路径是 `FramePump`（`monitor_window.rs:38`，由 `workspace.rs:1818-1837` 的 100 ms 定时器驱动），50 ms 主 tick（`workspace.rs:1791-1816`）只搬运已解码帧，绝不在渲染路径再 `poll` 一次。
94. **录屏没有时间上限，唯一硬边界是磁盘配额**：`recordStart` 只发 `recordingId`，结束只有两条路——配对的 `recordStop`，或 `stop_capture`（面板停采、改绑、删 DUT、进程退出），后者给在途请求回 `MEDIA_RECORDING_ENDED`（`apps/desktop/src/media_page.rs:664-702`）。录制期间会话被 pin，空闲回收不碰它（`capture_backend.rs:363-368` 的 `decide` 先判录制，注释明写 no upper bound）；`RECORD_STOP_GRACE`/`ABORTED_TTL` 只是收尾答复窗口与墓碑寿命，不是时长闸门（`media_page.rs:196-200`）。配额耗尽按 `MEDIA_CAPTURE_FAILED` 报，码表里**没有** `MEDIA_QUOTA`（`capture_backend.rs:155-157`）——所以别给 op schema 加 `seconds`，也别在 tick 里加「最长录制」计时器。

## 2026-11-24 工具下载独立页 / 并发下载 / 默认高亮（自动追加）

> 本节由工具下载入口与串口默认高亮轮追加；只新增、不改/不重排既有内容；坑编号接续上文。

90. **工具下载独立成页 + 并发下载**：新增 `ConfigPage::ToolDownloads`（导航「工具下载」，图标 `download`），四处必须同步——enum（workspace.rs）、`page_title`、页面 dispatch、侧栏 `nav` 列表；打开该页（与 Settings 一样）刷新行快照。并发：`ToolInstallCenter` 的 `active`/`progress` 从单槽改为按 `tool id` 的 map，`start` 只拒同工具重复、`cancel_active(tool)` 按工具取消、`drain` 只清该工具槽；UI 行内仅本行 busy 时显示「取消」，其他行不再禁用；tick 用 `progress_snapshot()`（HashMap）与上一帧比较决定重绘。
91. **默认高亮规则按 sermcp 语义播种**：`user@host` 绿 `#7ec699`、Android `状态|host` 绿、prompt 路径蓝 `#56a8f5`、行尾 `$`/`#` 浅灰 `#c8ccd4`（参考 bitshelf/sermcp 的 PromptSemanticHighlighter）。约束：渲染引擎是 `regex` crate（**无 lookaround**），故路径规则含冒号（`:[~/][^\s$#]*`）、sigil 用行尾锚点（`[$#]\s*$`）、user 规则不排除 `builder@lab` 这类内核日志误染（可接受）。播种只发生在 load/default 路径且仅当规则为空（`seed_default_highlight_rules`）；`docs/examples/settings.json` 与 roundtrip 契约不动，`SerialSettings::default()` 仍为空（单测锁定）。
92. **代理「已保存生效」必须可见**：「下载代理」面板显示 `已保存生效：<模式>[ · <URL>]` 与环境变量检测结果（env 模式：检测到/未检测到 → 直连），草稿与已保存不一致时追加「有未保存的修改」。实机现场：用户在 URL 框填了 `socks5://127.0.0.1:7897` 但模式仍是「跟随环境变量」且从未保存，ffmpeg 直连 gyan.dev 极慢、长时间 0%，看起来像「下载不下来」——`connect_timeout(30s)/read_timeout(60s)` 只在真正卡死时报错，慢速直连不会。展示即应用，别让草稿看起来像已生效。

## 2026-11-24 工具下载入口最终形态（自动追加）

> 本节由工具下载入口收口轮追加；只新增、不改/不重排既有内容；坑编号接续上文。

93. **工具下载/更新的入口以独立页为准（用户 2026-11-24 实机拍板）**：坑 #88 的「设置页总面板 + 各功能页快捷入口」中，**功能页快捷入口不保留**——采集页「采集工具」面板的 FFmpeg 下载按钮已被用户明确要求移除（工具下载统一走侧栏「工具下载」独立页：安装行 + 下载代理）。ADB 页的快捷按钮暂留待确认。`pages::tool_install_controls` 仍由 `render_usb_debug` 使用；若 ADB 快捷入口也移除，该 helper 一并删除。

## 2026-11-24 滚动缓冲放大（自适应快照节奏，自动追加）

> 本节由滚动缓冲放大轮追加；只新增、不改/不重排既有内容；坑编号接续上文。

94. **终端回滚容量与实时快照行数分开**：`scrollbackRows` 接受 100–1,000,000 行，新建会话才生效。Term 保存历史，实时总线最多带 1,000 行。更早数据在串口 actor 内分页读取，回看期间禁止键盘和粘贴写串口。实时画面按 16ms 节奏合并，历史容量不延长发布间隔。

## 2026-11-24 控制设备（继电器板）编辑交互重做（自动追加）

> 本节由控制设备交互重做轮追加；只新增、不改/不重排既有内容；坑编号接续上文。

95. **「控制设备」表单改为「列表 + 详情」设置页范式，且语义统一为草稿提交**：布局参考 macOS 系统设置 / Superlogical Rex（左窄列表、右分组设置行、底部右对齐主操作）：左侧为已配置设备列表（名称 + 串口，选中高亮，脏态小圆点，底部「+ 添加设备」），右侧用既有 `setting_row`（标签左、控件右）呈现 名称 / 设备串口（带刷新）/ 板卡通道数 / 适配驱动（唯一驱动 CH340 → 只读值，不再用单项下拉）；底部左侧分离「删除控制设备」（红色文字，走 `ModalKind::RemoveControlDevice` 确认弹窗，与删除连接同族），右侧「取消修改」（文字按钮）+「保存设备」（Primary，干净时禁用）。**交互语义**：所有字段只写 `relay_board_draft`（`input_value` 草稿优先、`input_set` 只进草稿），「保存设备」才写回板卡列表并 `save_duts()`；切换设备 / 离开页面 / 「取消修改」丢弃草稿；「添加设备」立即建板并落盘（避免幽灵草稿）；列表标签在串口为空时不再输出悬空的「 · 」。**不要**再恢复「边改边写内存 + 独立保存按钮」的混合语义——那正是用户反馈「不自然」的根源。相关坑 #14（先 clone 后写 dirty 的借用顺序）在 new draft helpers 里同样适用。

## 2026-11-24 镜像传输接收端改用 Salvo + salvo-tus + blake3（自动追加）

> 本节由镜像传输接收端重写轮追加（`crates/duthub-transfer` 整包替换，契约详见 `docs/guides/TRANSFER_RECEIVER.md`）；只新增、不改/不重排既有内容；坑编号接续上文。

96. **存储/锁/GC/发布一律用上游，别再自写**：接收端协议层改用 Salvo 官方 `tus` 特性（crate `salvo-tus` 1.2.1，Salvo 组织维护），它自带单文件追加写、每上传读写锁（`MemoryLocker`，带超时与空闲回收）、每 PATCH 原子提交元数据、失败回滚、完成时**定名发布**（按 `filename` 元数据硬链接/复制，绝不覆盖，重名加 `-<id>` 后缀）。它**不提供** expiration 与 checksum：前者由存储后端实现（本 crate 用 `ExpiringDiskStore` 装饰器补，`get_expiration` + `delete_expired`），后者必须在守卫层做（`Upload-Checksum` 由客户端在创建时声明，完成时单遍校验）。Salvo 默认特性会拉 `aws-lc-rs`/`test`(brotli/zstd)/cookie 等 —— 接收端用 `default-features = false` + 显式特性，**必须含 `http2-cleartext`**（ztus 走 prior-knowledge h2c，缺了直接连不上；`conn/ctrl.rs` 会拒 HTTP/2 前导帧）。**坑 #57 中「`fileloft-*` 只当协议层用、存储/锁自写」的口径随本次重写失效**（该依赖已从 workspace 移除），选型原则以本条为准。
97. **路径参数会被百分号解码 → 守卫必须自己分类并只认规范 ID**（真实授权绕过，已复现并修复）：`salvo_core/routing/path_state.rs` 对含 `%` 的段解码后再匹配路由，因此 `/transfer/%63…` 与 `/transfer/c…` 指向同一上传。守卫若用原始路径尾段去查记录会查不到 → 放行 → 任意令牌可读写删他人上传（实测 HEAD 返回 200）。修法：`router::target()` 自行分类路径，`{base}/{id}` 只接受 `salvo::tus::utils::is_safe_upload_id` 的规范写法，其余一律 400（`invalid_upload_id`）；授权校验的 ID 必须与 tus 层将要操作的 ID 完全一致。回归测试：`tests/reject_paths.rs::percent_encoded_ids_cannot_bypass_the_subject_check`。以后给任何"路由解码 vs 中间件取原始串"叠加的端点加断言，形状照此。
98. **ztus 兼容口径（源码核实，提交 `87292e8`）**：`--header "Authorization:Bearer …"` 是唯一鉴权入口；`--metadata k:v`；`--checksum sha1|sha256|none` 默认 sha256，摘要在**创建请求**随 `Upload-Checksum` 发出（hex，非 base64）；缺 `filename` 时按文件路径自动补。服务端不支持 sha1 时，400 报文里必须出现 `unsupported checksum algorithm` 字样它才回退 sha256。**续传前的 HEAD 必须 200**（204 会中止）；每个 PATCH 必须回 `Upload-Offset`。`ztus resume` 只有 `--header`、没有 `--metadata`：状态文件缺失时它回退成新建上传 → 服务端按契约 400（缺 filename），这是可接受且已记入文档的边界。构建 ztus 做端到端时：`reqwest` 需 `default-features = false`（否则 aws-lc-sys 要 cmake，本机没有），克隆目录要加空 `[workspace]` 脱离仓库 workspace——只改 scratch 克隆，不动上游源码。
99. **校验失败必须「先写失败标记、再删文件」**：Windows 上刚写完的大文件常被杀毒/索引器持有，删除会失败；若只 `let _ = remove`，未校验通过的字节会留在 `data/` 且看起来像已发布。形状：`flag_failed` 写 `metadata/<id>.failed`（本 crate 自有 sidecar，先 `create_dir_all` 再写），随后尝试删除；**标记写到文件系统之后才尝试删除**，周期清扫的 `sweep_ids()` 同时枚举 `.json` 与 `.failed`（含只剩标记的孤儿），拿到写锁后删除并清标记。`DataStore::remove` 只在非 `Internal` 错误时清标记——被占用时保留标记等下轮。
100. **上游 DiskStore 不 fsync**：每 PATCH 只 `flush` 到 OS 缓存，acknowledged offset 可跨**进程崩溃**恢复（元数据原子 rename），但**断电**一致性不保证。声明了摘要的上传在完成后整文件复读校验，可兜住落盘错误；需要更强保证时在上传侧或文件系统层解决，别在这个 crate 里另写一套 WAL。另：空 `Upload-Metadata` 头按「无元数据」处理（ztus resume 会发空头），只有非空且格式错误才是 400。
101. **验证 ztus/curl 端到端的最小闭环**（不含真实 DUT）：`cargo run -p duthub-transfer --example receiver -- --port 18081 --token dev --dir <tmp>` 起示例接收端；`--lan` 才绑 `0.0.0.0`（无鉴权时 fail closed）。ztus 状态目录跟随 `USERPROFILE`，把它指到项目内目录（如 `target/scratch-transfer/e2e/home`）即可避免写 C 盘。真实验收基线（2026-11-24，本机 loopback）：24 MiB `--checksum sha256` 与 8 MiB `--no-checksum` 均一次通过；1.5 GiB 中断在 ~216 MiB 后 `ztus resume` 续传完成、逐字节一致。

## 2026-11-24 工具下载页去小字（自动追加）

> 本节由工具下载页文案精简轮追加（用户实机反馈「小字全去掉」）；只新增、不改/不重排既有内容；坑编号接续上文。

102. **`workspace::tool_failure_label` 返回的是 i18n 键名，调用方必须再过 `i18n::label`**：该函数只做 `InstallFailureKind → &'static str`（`"toolFailNetwork"` 等）映射；工具下载页状态行曾直接 `.to_string()` 上屏，实机显示原始键名「toolFailNetwork」。新增失败/状态映射时把「映射函数 + 本地化调用」当成一对（坑 #13 同族：能映射不能显示）。
103. **工具下载页只保留功能性文字（2026-11-24 用户实机拍板）**：副标题（toolsDownloadSub）、面板说明（toolsDownloadHint）、每行「安装于… · 当前生效…」facts、失败技术详情、「已保存生效/有未保存的修改」、代理 URL help 与保存后提示全部移除；两个 locales 的对应键同步删除（键集必须保持一致，`apps/desktop/tests/agent_host.rs` 有等长/双向包含断言）。每行只留工具名 + 校验徽标 + 状态行 + 按钮 + 进度条，代理面板只留「代理策略」选择 + 「代理 URL」输入 + 保存。**代价**：草稿与已保存代理的差异不再可视化（坑 #92 的现场线索被用户主动放弃）；若要恢复提示，做成按钮态或控件态，不要再加回小字说明。`setting_row` 现在跳过空 desc（传 `""` 不留空行，其它页面不受影响）。

## 2026-11-24 工具下载页二轮修正 + 组合框浮层（自动追加）

> 本节由同日实机反馈轮追加（校验徽标去除、双横线、代理不生效、组合框推挤分割线）；只新增、不改/不重排既有内容；坑编号接续上文。

104. **组合框候选列表要浮层，不能内联推挤**：`Workbench::combobox` 原先把列表放在文档流里（注释称避免兄弟遮挡），展开时把下面各行推下去、`setting_row` 上下分割线跟着移动——用户实机反馈「串口设备上下的两条线不要变」。改为与 `select_field` 同形状：`.absolute().top_full().left_0().mt(px(4.))` + `gpui_kit::deferred(list).with_priority(1)`，列表浮在后续行之上、行高与分割线不动。
105. **`download.proxy.url` 只在 `custom` 模式被读 → 存了 URL 而模式仍是 `env` 等于直连**：旧保存逻辑无条件写 url、模式保持 env，实机表现为 ffmpeg 慢/失败、goose 缓慢、Clash 无流量（坑 #92 的 0% 现场同源）。两处修：① core `load_settings` 的 `migrate_download_proxy`：`env` + 非空 url → `custom`（`off` 是显式直连，不动）；② 桌面 `save_tool_proxy`：非空 URL 一律按 `custom` 提交（即便下拉选的是 env），保存 toast 直接报出生效模式与 URL。测试：`store.rs::env_mode_with_a_saved_url_migrates_to_custom`（构造 settings 测试 JSON 必须带 `appearance` 段，否则 `missing field appearance`）。
106. **工具下载行列表：校验徽标按用户要求删除，末行不画下边线**：坑 #103 里「工具名 + 校验徽标」作废——徽标（校验和已验证/未验证）与 `toolChecksum*` 文案已移除。校验由 tooldown 内核在下完后执行：pinned sha256 不符返回 `ChecksumMismatch`，staging TempDir 随错误丢弃，最终目录只在成功时原子换入（失败不会装上半成品），失败以 toast 报「下载失败 <tool> — <原因>：<细节>」。行间分割线只在非末行画（`.when(!last_row, ...)`），否则末行边线与 `settings_panel` 外壳边线叠成双横线。
107. **代理 URL 输入框只在 `custom` 模式渲染，保存不再按 URL 有无改写模式（2026-11-24 同日追加，坑 #105 第二点作废）**：`env`/`off` 下隐藏 URL 框（显示由选择器草稿驱动，切到「自定义代理 URL」立即出现），避免「填了 URL 就以为生效」的误导——正是坑 #92/#105 的现场。`save_tool_proxy` 因此去掉「非空 URL → custom」的自动改写，并且在保存 `env`/`off` 时**清空存储的 URL**：字段不可见时留下的 URL 无意义，且留着会在下次启动被 `migrate_download_proxy` 又翻回 `custom`，覆盖用户的显式选择。验收：选「跟随环境变量」/「不使用代理」时 URL 框消失，选「自定义代理 URL」时出现；env/off 保存后重启仍是 env/off。

## 2026-11-25 烧录工具预置（自动追加）

> 本节由烧录工具预置需求追加；只新增、不改/不重排既有内容；坑编号接续上文。

108. **工具预置是数据不是代码（2026-11-25 用户拍板）**：烧录工具预置只写在 `settings.json` 的 `flashTools`（普通配置项，界面可增删改），**不要**在 `duthub-core` 的 `default_settings` / `load_settings` 里硬编码播种——预置调整只改配置文件，不碰 core 加载路径，避免与默认值跳过、roundtrip 契约、其他上下文在改的 core 代码互相影响。预置条目形状与 `docs/examples/settings.json` 一致（`name`/`executable`/`testArgs`；`executable` 可用绝对路径或 PATH 命令名）。

## 2026-11-25 goose 从内嵌改为下载分发（自动追加）

> 本节由 goose 去内嵌轮追加（`apps/desktop/assets/goose/` 删除 + 解析链改为「外部路径 > 已下载 > PATH」）；只新增、不改/不重排既有内容；坑编号接续上文。

109. **goose 不再内嵌：许可/状态文案与"未安装"必须可展示，不能 unwrap**：`apps/desktop/assets/goose/` 全删，`agent_host/bundle.rs` 不再 `include_bytes!`/`include_str!`。解析顺序 = 显式外部路径（`expectedSha256` 启动复算）> 已下载安装（`duthub_tooldown::spec_by_id("goose")` + `tool_resolver::downloaded_install`，按安装内核 manifest 记录的 sha256 复算现盘字节，字节不符落空到下一级、哈希相符但 `--version` 不通过直接报错）> PATH 裸名；下载/解包/哈希全归 tooldown 内核，duthub 不自己下载、不 pin。三级都没有时返回 `RegistryError::NotInstalled`（可展示中文），公开状态 `registry::goose_install_state() -> GooseInstallState`（`Downloaded{path,sha256}` / `Path{path}` / `NotInstalled`，`needs_install()` 供 Agent 页出「去工具下载」引导），`goose_install_state_in(tools_root)` + `resolve_builtin_from(definition, env, downloaded, path_program)` 是测试注入的公开缝。`bundle` 保留兼容面：`authenticode_status()` 返回「未内置（随上游发行包）」而**不是** `NotSigned`（未内置时宣称未签名等于伪造结论），`license_text()`/`notice_text()`/`manifest_json()` 都是说明文案，`GOOSE_SHA256` 只剩 12 字符占位 `"not-embedded"`（`pages.rs` 仍 `&GOOSE_SHA256[..12]`，删它要连那行一起改），`GOOSE_VERSION` 语义 = 推荐/最低版本并与 `registry::MIN_GOOSE_VERSION` 同源（`1.60.2`）。**测试形状**：ACP 用例需要真实 goose，统一「`DUTHUB_TEST_GOOSE`（绝对路径）优先，否则 `goose_install_state()`；都没有就 `eprintln!` SKIP 并 return」——本机未装 goose 时这些用例跳过（断言不删、只不执行），装好后原断言回归。**待核对**：下载安装的是通用 `goose.exe`（zip 内 `goose-package/goose.exe`），ACP 入口按文档是 `goose acp` 子命令，而解析层沿用旧 `goose-acp.exe` 的空参数启动——`goose.exe` 是否默认 ACP 未实测，需先用真实下载安装核对再决定是否给默认 goose 条目加 `acp` 参数。
110. **内嵌资产的断言散落在发布门禁，产品改口径时必须同步**：`scripts/release-gate.ps1`（`goose-acp.exe`/zip 的字节数与 SHA-256、goose `Cargo.lock` 审计、manifest `id`）与 `.github/workflows/release.yml` 仍断言已删除的 `apps/desktop/assets/goose/*`；去内嵌后它们必然失败。改发布口径时同步这两处，别让发布门禁长期"永远红"。

## 2026-11-25 OS 沙箱（ADR-12）收口（自动追加）

> 本节由沙箱选型轮追加（`os_sandbox` 登记 + rappct 评估 + ADR-12）；只新增、不改/不重排既有内容；坑编号接续上文。

111. **`os_sandbox` 长期不在模块树：未编译的「实现」和未编译的测试都等于零**：`crates/duthub-core/src/os_sandbox.rs` 之前既没有 `lib.rs` 声明，`#[cfg(windows)] mod imp;` 又指向不存在的目录——8 个单测从未编译。登记 `pub mod os_sandbox;` 后立刻暴露 3 个编译错误（`Option<PathBuf>::filter(Path::is_absolute)` 签名不匹配 ×2、`HOMEDRIVE` 的 `String` 未转 `OsString`、测试里 `Vec<&str>` 收 `String`）与 3 个失败测试（`validate_policy` 的「cwd may be deeper」没建子目录；`drive_of` 对 UNC 前缀返回 `\\srv\share` 而不是回退 `C:`；`split_drive` 消费掉 RootDir 导致 `HOMEPATH` 少了前导 `\`——都是实现/测试自身的错，按文档语义修实现、保留断言）。口径：新模块先挂 `lib.rs` 再谈实现完成；`cargo check` 不编译 `#[cfg(test)]`，带测试的判定用 `--all-targets`。现在 `AppContainerExecutor::execute` 在所有平台返回 `Unsupported`（fail-closed 占位），单测 `executor_fails_closed_without_a_backend` 锁定；`os_acl` 的 6 个测试（含坑 #85 那三处最小补全）保持通过，`os_acl.rs` 未被改动。

112. **复用 AppContainer 封装前先跑「零 capability + 自定义环境」两档**：rappct 0.13.3（MIT；crates.io 版本元数据、发布包 `Cargo.toml.orig`、仓库 `LICENSE` 三处一致，GitHub API 只报 NOASSERTION，不构成许可不清）在 `src/ffi/sec_caps.rs` 对空 capability 列表传非空悬垂 `Capabilities` 指针 → `CreateProcessW` 返回 87；`env: Some(...)` 同样 87，只有 `env: None`（继承宿主环境）能启动。duthub 的 `NetworkPolicy::Deny` 就是零 capability，且「不继承宿主环境」是硬要求，故不引入（评估与真实输出见 `docs/decisions/ADR-12-OS-SANDBOX.md` §4）。实测补充：最小直连 `CreateProcessW`（`Capabilities = NULL`）能建立真边界（写沙箱外「拒绝访问」、读 `.duthub/settings.json` 失败、同命令读沙箱内成功作对照）；显式环境块缺 `USERPROFILE`/`LOCALAPPDATA` 等返回 203（`ERROR_ENVVAR_NOT_FOUND`），补齐后 PowerShell 5.1 在容器内可运行且退出码如实返回——此前「PS 不能进容器」的结论是把 cmd 的 `/C` 拼进了 PS 命令行，探测拼错命令行会得出假结论，先自检再写结论。UI 侧 `apps/desktop/src/agent_ui.rs:718-721` 仍把「DACL 私有 + 受限令牌探测可用」显示为「OS 沙箱（已验证）」，按 ADR-12 属过度宣称，待 UI 属主修正。

## 2026-11-25 串口乱码检测（自动追加）

> 本节由串口乱码检测轮追加（core `GarbleMeter` + 桌面每 DUT 开关 / 会话标签琥珀点 / 告警）；只新增、不改/不重排既有内容；坑编号接续上文。

113. **串口乱码判据必须挂在 `emit_raw` 的原始字节层，桌面只做开关与告警边沿**：① vte/alacritty 会把 C1 控制字节（0x80–0x9F）当 CSI 单字节序列吞掉，屏幕文本会低估乱码——检测器只能挂在 `SessionCtx::emit_raw`（`crates/duthub-device/src/garbled.rs` 的 `GarbleMeter`，4 KiB 滑窗 + permille 判定），不得改 `SerialChunk` 字节/文本/顺序，也不得新增总线事件。② 判据只统计异常控制字节占比（0x00–0x1F 排除 07/08/09/0A/0D/1B，再加 0x7F），不看非法 UTF-8、不做编码识别、不设灵敏度分档；阈值 30/60‰、最小窗口 512 字节、滞回 15/30‰。随机字节天然约 10.5%，二进制协议/大段控制字节会误报——这是每 DUT opt-in 的已知代价。③ `ScreenSnapshot` 派生 `Eq`，统计字段只能是整数 `control_permille`/`window_bytes`，**禁 f32/f64**；`GarbleLevel` 的序列化拼写（camelCase `clean`/`watch`/`garbled`）是跨 crate 契约，由 `crates/duthub-core/src/ipc.rs` 的单测钉住。④ 每 DUT 开关存 `duts.json` 的 `garbledByDut`（空 map 不序列化，`docs/examples` 不动）；**关闭即 `remove` 条目**，别写 `{"dut":{}}` 让 skip 失效。检测调度走 `DeviceService::set_garbled_detection_for` 的每 DUT map（同一 `Arc` 被 `SessionCtx` 持有，运行中即时生效），桌面有效开关 = 检测开关 && 该 DUT 提醒开关（`Workbench::sync_garbled_runtime`，在启动、保存连接、提醒开关、检测开关、删除 DUT 后重算）。⑤ 全局提醒总开关已删除：`settings.alerts` 只剩 `suppressedSources`，`AlertCenter::enabled_for` 缺省 true，旧 `enabled=false` 配置不再生效；副作用是非 DUT 主体（继电器板、通用监听器 `agentNotice`）从此只能按来源抑制，`ReportRejection::Disabled` 只对 `alertsByDut.enabled=false` 的 DUT 可达。⑥ 告警只在会话从非 `Garbled` 变为 `Garbled` 时 `report_for(..., AlertSource::SerialGarbled, ...)`，不得往 `SessionView.lines` 插提示行、不得每帧重复弹（`garble_ui::alert_on_transition` + `AlertCenter` 30s 去重）；会话标签圆点用既有 `p.warn`，断线红优先，不新增主题 token。⑦ `real.rs` 的两处 `ctx.reset_garble()` 只在 `real-hardware` 特性下编译（`crates/duthub-device/Cargo.toml` 的 `default = []`），`cargo test -p duthub-device` 看不到它，验证要 `cargo clippy -p duthub-device --features real-hardware`。⑧ 遗留待办：`docs/DATA_MODEL.md` 的 duts/settings schema 尚未同步 `garbledByDut` 与 `alerts` 段变更（本轮未改该文件）。

## 2026-11-25 传输路径规范门禁（自动追加）

> 本节由传输方案审核 P0 修复轮追加（审核报告 `docs/reviews/TRANSFER_SECURITY_REVIEW.md` F-01）；只新增、不改/不重排既有内容；坑编号接续上文。

114. **HTTP 路径必须与路由层同源判定：原始 URI ≠ 路由后的 URI**：Salvo 的 `routing/path_state.rs` 会跳过空段、丢弃 `.`、对 `..` 弹栈并解码百分号后再匹配；守卫若拿 `req.uri().path()` 直接 `strip_prefix(base)` 分类，`//transfer`、`///transfer`、`/./transfer`、`/foo/../transfer` 全被判成 `Target::Outside` 而 `call_next` 放行，但路由仍把请求交给 tus handler——实测无令牌 `POST` 201、`PATCH` 204、`HEAD` 200、`GET` 200 读到内容、`DELETE` 204 删掉在途上传；且 `authorize_create` 整段不跑，文件名白名单（`evil.exe` 201）、磁盘预算、`subject` 绑定全部跳过。修法（`crates/duthub-transfer/src/router.rs`）：最外层先做**规范形态门禁**——只接受以单个 `/` 开头、段非空且非 `.`/`..`、无 `\`、无 `%`、至多一个尾斜杠的路径；非规范且位于挂载点之下报 `invalid_upload_id`，其余报 `invalid_path`（均 400），此后 `target()` 的分类才与路由一致。回归必须用**原始 TCP** 发请求（reqwest/curl 会在客户端归一化 `.`/`..`，测不出攻击面），且 `#[tokio::test]` 默认单线程运行时会与阻塞 socket 读互相饿死，必须 `flavor = "multi_thread"`；`/%2Ftransfer` 在路由层就是 404（守卫不运行），断言按“4xx 且不产生上传”写，不要放宽到成功。未来任何“路由会重写路径 + 中间件按原始串判定”的端点（A2A/artifact 路由同理）都要照此加门禁。

## 2026-11-25 托盘关窗告警 / 继电器巡检 / notify_user 收口（自动追加）

> 本节由托盘告警收口与实机验收轮追加（托盘关窗隐藏 + 隐藏态气球/闪烁 + 继电器板掉线监测 + `notify_user` 工具）；只新增、不改/不重排既有内容；坑编号接续上文。

115. **托盘 v4 回调的菜单锚点在 `wParam`，键盘唤起的菜单绝不能用 `GetCursorPos`**：`NOTIFYICON_VERSION_4` 下 `GET_X_LPARAM(wParam)`/`GET_Y_LPARAM(wParam)` 是锚点；键盘按 Shift+F10/菜单键时 shell 投 `WM_CONTEXTMENU` 且 `wParam` 是图标左上角，而鼠标可能停在屏幕任意位置——照设计旧稿用 `GetCursorPos` 定位会把菜单弹到光标处。`apps/desktop/src/tray_ui.rs:442-476` 的 `callback_point`/`callback_carries_anchor`/`callback_anchor` 做有符号小端拆包并把 `(0,0)` 当「无锚点」；`apps/desktop/src/tray.rs:1186-1215` 的定位顺序 = 回调锚点 → `Shell_NotifyIconGetRect` 图标矩形左下角 → 光标。

116. **`NIIF_RESPECT_QUIET_TIME` 会让气球在静默时段被直接丢弃（不排队），critical 告警必须省略该标志**：官方原文是 “not queued; it is simply dismissed unshown”，所以「QuietTime 只放行 critical」唯一可实现的路径是让 critical 不带该标志——`apps/desktop/src/tray_lifecycle.rs:268` 用 `respect_quiet_time: !critical`（`critical = source != AgentNotice`），`apps/desktop/src/tray.rs:1460-1467` 只在为真时置位。附带两条：`Shell_NotifyIconW(NIM_MODIFY)` 返回 TRUE 只代表 shell 接受请求，`balloons_sent` 不能当作「用户看到了」的证据；按官方语义静默期被丢弃时会补发 `NIN_BALLOONHIDE`，而托盘把该事件当停闪信号，即带标志的 critical 连闪烁也会被提前掐掉（源码推导，未在静默时段实测——该状态只在新建用户首小时/系统升级后出现）。

117. **托盘必须「先确认图标存在，再隐藏窗口」**：`hide_main` 先 `set_icon_visible(true)`（`NIM_ADD` + `NIM_SETVERSION`）成功才 `ShowWindowAsync(SW_HIDE)`，失败直接返回 false 且窗口保持可见（`apps/desktop/src/tray.rs:456-470`）——不要实现成「先隐藏再异步回滚」，回滚竞态会让用户拿到一个没有图标的隐藏窗口。`TaskbarCreated` 重放同理：GUID 仍有残留注册时 `NIM_ADD` 会失败，要先 `NIM_DELETE` 再重试一次，两次都失败才置 `icon_added=false` 并发 `TrayEvent::IconLost`（`tray.rs:1471-1490`）。

118. **所有 `NIM_*` 只在托盘线程执行，UI 线程调用必须有上界并安全失败**：这样才不会与 `TaskbarCreated` 重放交错；`TrackPopupMenuEx` 是模态循环、可能不分发投递消息，所以 `set_icon_visible` 走 `sync_channel` + `COMMAND_TIMEOUT = 2s`（`apps/desktop/src/tray.rs:233`、`:496-522`），超时按 false 处理（调用方据此不隐藏）。HICON 由托盘线程在自己退出前 `DestroyIcon`（`LoadIconW(IDI_WARNING)` 要先 `CopyIcon` 拿私有副本），不要跨线程释放别的线程仍在用的图标；线程自己创建的窗口会在线程结束时被系统释放，早退未 `DestroyWindow` 不构成进程级泄漏（复核轮的 P/Invoke 探针核对过线程 join 后 `IsWindow=False`）。

119. **Win11 溢出区不随 `NIM_MODIFY` 重绘，托盘成功路径也不打日志**：新图标默认进溢出面板且应用无权提升，所以截图/UIA 只能证明「图标存在」与「气球已发」，**不能**证明闪烁帧可见——本轮验收在 `tray_win32::flash_stops_itself_after_the_cap` 运行期间以 300 ms 采样溢出面板 130 帧，哈希恒定。托盘只在失败路径 `eprintln`，`run.log` 里没有「图标已添加」这类成功行；隐藏期存活的硬证据是 `duthub-data/runtime/mcp-state.json` 的 `tsUnixMs` 递增（快照由 50 ms tick 刷新），图标本身用 UIA 在 `TopLevelWindowForOverflowXamlIsland` 里按 `NotifyItemIcon` 定位。

120. **`--self-test` 的惰性 `TrayHandle` 与 `close_disposition(tray.is_some())` 是两套口径**：`tray::install` 自己读 `--self-test` 并返回 `enabled:false` 的句柄（不建线程、不装图标，`apps/desktop/src/tray.rs:332-356`），而关闭决策只看 `Option::is_some()`；一旦该句柄进入生产路径，`HIDE_PLAN` 没有 `DestroyMain` 兜底、`hide_main` 恒 false，结果 X 既不隐藏也不退出（只 toast `trayHideFailed`）。调用点必须保留 `!self.agent_self_test &&` 守卫（`workspace.rs:8090`，契约测试逐字钉住），更稳的做法是自检分支返回 `Err` 或给句柄暴露 `ready()`。

121. **源码契约测试只匹配字面量 = 空断言**：① `apps/desktop/tests/tray_contract.rs` 曾在 `run_close_plan` 里 `find("CloseStep::*")` 并比较相对位置，命中的其实是 `debug_assert!(plan.iter().all(...matches!(...)))` 的臂列表（release 编译掉）——变异实验（把执行体循环反转）测试仍绿，把 `debug_assert` 里两个枚举换序测试才变红。现在的断言钉真实分派 `actions_for_close_plan(plan)` + `for action in actions`，并显式拒绝 `for action in transition.actions`（`tray_contract.rs:79-87`；`workspace.rs:1302-1313` 的 `debug_assert_eq!` 只作交叉校验）。② `main_guards_a_single_instance` 曾只匹配 `main.rs` 文档注释里的 `CreateMutexW`/`ERROR_ALREADY_EXISTS`/`MessageBoxW`，删掉 `single_instance::acquire()` 调用测试照绿。判据：**契约要锚在调用点与执行语句上，实现细节的锚点放实现文件侧**。

122. **程序化关浮窗禁止「先 `take()` 再 `remove_window()`」**：唯一的 `cx.on_window_closed` 观察者靠 `float_window.handle.window_id()` 匹配，take 后永不命中 → placement 停在 `Floating` → `release_monitor` 提前返回 → 退出路径的 `StopPreview` 成空操作；反过来经观察者走进 `on_float_window_closed` 又会把隐藏态的预览/采集停掉（违反「隐藏态采集继续运行」）。正确形状是执行器只 `remove_window`、由 `forget_float_window()`（`workspace.rs:7484`）清 `float_window`/`frame_pump` 并把 `Floating` 归位 `Docked`，同时让 `release_monitor` 在 `quitting` 时越过 `Floating` 守卫（`workspace.rs:7397-7407`）。gpui 语义：`remove_window` 只置 `removed`，`window_closed` 观察者在 trail 内同步执行，所以 body 必须 `cx.defer`，否则会在别人的租约里 `weak.update` 自己（`double_lease_panic`）。

123. **隐藏后 `render_root` 停摆，后台职责必须进 50 ms tick**：隐藏窗不消费 `WM_PAINT`，一切「每帧」逻辑都会停；`drain_jobs()`、`refresh_snapshot()`（250 ms 节流，`workspace.rs:8006-8010`）、`alert_present_tick` 都迁进 tick，`dialog_open` 由 `render_root` 每帧写入、tick 读字段（允许一帧延迟，不在 tick 里嵌套 window update）。跨线程告警（MCP `notify_user`）只写 `AlertCenter`、**不唤醒渲染**：可见态 Dialog 要等下一次重绘才由 `render_root` 出队（`workspace.rs:8291`），本轮验收实测有约 2 s 可感知延迟（未精确测量），属已知行为不是缺陷。

124. **继电器写门是 `ControlService` 级单锁：按住测试要有界等待，探针 `Gated` 必须静默**：`probe_board_status` 持 `relay_writes.try_read()` 跨一次 STATUS I/O（健康板 ≈300 ms，端口 task 卡死最坏 `IO_TIMEOUT = 30s`）；`hold_channel` 若用裸 `try_write` 会在任意一块板/任意 DUT 按键时立刻 `Busy`，故实现为 `RELAY_WRITE_GATE_WAIT = 1s` 的有界重试（`crates/duthub-device/src/control.rs:435-460`）。反向代价：门是全局的，按住任一板会让所有被监测板 `Gated`，所以巡检的 `Gated` 分支只置 `Unobservable` + 清 `consecutive`、**不发事件**（否则每板一条「暂时无法探测」toast，且文案「继电器按住测试进行中」把别板的动作按到本板头上，`crates/duthub-device/src/relay_watch.rs:493-499`）。

125. **巡检的恢复/复提判据是「已告知」标记，不是当前状态**：`reported_offline` 决定 `Recovered` 与提醒节流（`last_emit_ms`）；若只看 `state == Offline`，则「Offline → 按住/端口占用 Unobservable → Answered」永远不发 `Recovered`，解除按住后还会对同一段掉线重复告警（`relay_watch.rs:464-490`）。`Recovered` 只出现在事件里，`states()` 永不返回它。

126. **继电器巡检集合的三条易漏口径**：`is_watchable_board` 除 driver 白名单（空或 CH340）与 `node` 非空外，还排除 `host` 非 `local` 的板卡（当前没有远程继电器传输，本地打开同名 COM 会误报掉线，`relay_watch.rs:196-200`）；板卡被 DUT 绑定但 `RelayChannels` 全 None 时 `board_probe_channels` 不产出条目（`relay_watch.rs:202-222`）→ 探针返回 `Unsupported("板卡未分配继电器通道")` → 仅 1 次 `Unobservable`，**绝不 Offline、不占端口**；「至少一个已保存 DUT 绑定」只能由调用方预过滤（`sync_boards` 只接 `&[RelayBoard]`，不带 DUT 列表）。

127. **桌面侧巡检签名必须含板卡名**：`Workbench::relay_watch_signature` 是 `(id, node, channel, driver)`（`apps/desktop/src/workspace.rs:1121`、`:7811-7820`），只改板卡名不会触发 `sync_boards`，而 `RelayBoardEvent.name` 取自 `BoardEntry.board.name`（`relay_watch.rs:266-274`），于是后续 Offline/Recovered/Unobservable 的告警与 toast 一直显示旧板名（重启或改 node/通道/驱动才恢复）。服务端 `BoardSignature` 本已含 name，缺的只是调用方；给它加字段时注意状态保持语义（node/driver 不变则保留状态）。

128. **继电器板端口必须与 DUT 串口做冲突校验，否则巡检会往 DUT 控制台写二进制帧**：保存控制设备只校验 name/node 非空（`apps/desktop/src/workspace.rs:9671-9678`），而巡检每 30 s 打开 `board.node` 并写 4 字节 CH340 STATUS 帧（9600 波特，`crates/duthub-control/src/ch340.rs:28-35`、`driver.rs:543-546`）；端口误配成某 DUT 控制台且该会话未占口时，设备控制台会周期性收到二进制。保存时应复用 `Workbench::dut_id_for_port`（`workspace.rs:1498-1504`）拒绝与任一已保存 DUT 串口相同（locale 的 `chooseSeparateRelayPort` 已无任何 `.rs` 引用）。同源副作用：`Ch340RelayDriver::ensure_task` 的端口 task 会长期持有继电器 COM 口（`driver.rs:198-217`），巡检把「用过才开」变成「首次探测后长期独占」。

129. **后台线程 `spawn(...).ok()` 会静默丢掉整条能力**：继电器巡检线程创建失败时，唯一的 `tick_at` 驱动与唯一事件源一起消失，`drain_relay_events` 的 `try_recv` 对断开的通道也无感知（`workspace.rs:2003-2023` 的 `.ok()`）。对照托盘线程失败会返回 `TrayError::ThreadSpawn`；新增后台线程至少 `eprintln` + 一条用户可见提示。

130. **`notify_user` 的「可见面 = 可用面」靠两张静态路由表，不是 `plan_tool_allowed`**：Plan 面仍是 13 个只读工具（`plan_tool_allowed` 保持 13 个名字），`notify_user` 只存在于 `PLAN_ROUTER_WITH_NOTIFY`（14 条），由 `self.notifier.is_some()` 选表（`crates/duthub-mcp/src/server.rs:505-512`、`:1834-1857`）；把名字写进 `plan_tool_allowed` 会把通知工具暴露给所有 13 工具端点，直接顶爆「会话端点恰好 13 个」的既有契约。没有 notifier 的端点（stdio 快照、A2A 证据端、桌面 Agent 会话）连 `tools/list` 都看不到它，调用回 `PLAN_MODE_ONLY`/`EVIDENCE_SCOPE_ONLY`。

131. **显式 `dutId` 的授权走 `authorize_read` / `authorize_any_read`，不能用 `authorize_scoped_read`**：`notify_user` 按 `match scoped_dut(args.dut_id) { Some(dut) => authorize_read(principal, dut), None => authorize_any_read(principal) }` 分派（`server.rs:1639-1644`）；`authorize_scoped_read` 在通用监听器上会退化成 any-read、不校验显式 `dutId`，于是 DUT A 的 token 能把通知标成 DUT B（坑 #44/#64 同族：新增读/通知工具都照此分派）。

132. **`SPI_GETCLIENTAREAANIMATION` 要在闪烁进行中重新求值**：只在 episode 启动时查一次的话，用户中途打开「减少动态效果」仍会闪满 30 s。两条路都要有——托盘线程处理 `WM_SETTINGCHANGE` 并调 `tray_ui::flash_stops_for_animation_change`（`apps/desktop/src/tray.rs:1107-1132`），50 ms tick 在闪烁活动期间再轮询一次（`workspace.rs:7877-7881`）；`FlashStop::AnimationsDisabled` 只有纯逻辑测试在用时，说明生产路径还没接线。

133. **被镜像进共享运行时的 settings 段必须在每个替换点重新同步**：`AlertCenter` 的全局抑制集合是 `settings.alerts.suppressed_sources` 的镜像，导入 settings.json 只替换 `self.settings` 时，界面显示新集合、运行时仍按旧集合抑制（反向导入空表后连「重置」都不出现）。修法：`AlertCenter::set_suppressed_sources(&mut self, keys)` 并在导入分支调用（`apps/desktop/src/alerts.rs:431-436`、`workspace.rs:3679`，契约测试钉住）。注意 `AlertCenter` 已无全局 `enabled` 字段：`enabled_for` 对非 DUT 主体恒 true，只能按来源抑制（坑 #113 ⑤）。

134. **桌面 lib 绿 ≠ bin 能编译；集成测试会连带构建 bin**：`workspace.rs`/`alerts.rs` 是 bin target 模块（`lib.rs` 不声明），`cargo check -p duthub-desktop --lib` 通过时 bin 仍可能因 core 配置类型漂移（`AlertSettings` → `GlobalAlertSettings`）挂掉；而 `cargo test -p duthub-desktop --test tray_*` 会因为集成测试要构建 bin（`CARGO_BIN_EXE`）一起失败，整批托盘/告警测试无法运行。改 core 的配置段或共享类型后，先跑 `cargo check -p duthub-desktop --bin duthub`。

135. **`winresource::set_icon` 只嵌 ID 1，第二套图标要 `set_icon_with_id`；`LoadIconMetric(2)` 缺资源报 `0x80070715`**：`apps/desktop/build.rs` 只有 `set_icon("assets/app.ico")`、`assets/tray-alert.ico` 不存在，所以 `RESOURCE_ALERT_ICON = 2` 必然加载失败，实际走 `LoadIconW(IDI_WARNING)` + `CopyIcon` 的私有副本（`apps/desktop/src/tray.rs:208`、`:1013-1024`）。这不是设计 §4.2 的「资源缺失只发气球、不伪造闪烁」降级——要回设计口径就删掉回退分支，补资源时 `build.rs` 与 `assets/` 同时变。

136. **托盘菜单/tooltip 文案在 `install` 时冻结，语言切换不会自动刷新**：`TrayOptions` 进托盘线程后不可变，菜单每次弹出读 `Shared.menu_labels`，所以要有 `TrayHandle::set_menu_labels`（`apps/desktop/src/tray.rs:550-556`）+ 语言切换点调用。**API 存在 ≠ 已接线**：`set_tooltip` 曾长期只有测试调用，`set_menu_labels` 目前也没有生产调用方，而 `workspace.rs:8110-8112` 的注释还写着「菜单文案没有更新入口」（已过时）。加端口方法时顺手查调用点。

137. **Windows 默认高并发跑全量测试会撞提交内存上限，不是代码错**：本机 `CommitLimit` 约 72.9G（物理 27.8G + 页面文件），`cargo test --workspace`（默认 -j16）+ gpui 链会报 `error[E0786] ... failed to mmap ... 页面文件太小 (os error 1455)`、`LNK1123`、`LNK1318`、`can't find crate for duthub_desktop`；同一套代码换 `-j 2`（必要时隔离 target）可全绿。判红前先降并发复跑，别按编译错误改代码（坑 #10/#12/#71 同族）。

138. **GUI 验收会被别的进程的权限弹窗吞掉点击**：其他工作流的测试二进制（如防火墙提示）会弹全屏模态（遮罩 `Shell_SystemDim`），此时 `drive-window.ps1` 的点击全落到遮罩上。做法：先 Esc 逐个关闭（等同取消，不改防火墙/注册表），确认没有别的测试进程在起网络服务；托盘验收还要求被测进程是本次构建（核对 exe 哈希与 mtime，别对着旧 `Duthub.exe` 验收）。

## 2026-11-25 传输审核 F-02..F-19 修复（自动追加）

> 本节由传输审核全量修复轮追加（报告 `docs/reviews/TRANSFER_SECURITY_REVIEW.md` §1.5 有逐条状态）；只新增、不改/不重排既有内容；坑编号接续上文。

139. **传输面从「能跑」到「可接线」的六条硬口径**：① 传输根必须由宿主传工作根（`TransferServer::new_in`），解析走词法归一 + canonicalize 包含 + 拒绝 symlink/junction；绝对 `transferDir` 出界一律拒绝——旧的「绝对路径原样放行」测试已改为「出界拒绝 + 界内接受」。② 每 DUT 用 `for_dut(dut_id, dut_dir, …)`：根为 `<dut_dir>/images`，启动时读写 `.duthub-dut-id`（与 core 同一所有权格式，不一致即失败），上传创建时由服务端写入 `dutId` 元数据，`authorize_resource` 对 `dutId` 不一致返回 403；令牌隔离靠「每 DUT 一个实例 + 该 DUT 的 authorizer」，不要往 `UploadScope` 加 DUT 字段（会撞所有既有构造点）。③ 守卫里「读记录失败」只有 `TusError::NotFound` 可以放行给 404，其余（I/O、JSON、非 UTF-8 元数据）必须 500 `corrupt_upload_record`；版本头 `Tus-Resumable` 缺失/不支持要在鉴权**之前**返回 412，不能用 `Ok(())` 把决定权交给下游。④ 预算必须在 `create_gate` 互斥内跨「读取→tus 落记录」完成，否则并发 create 各自看到 inflight=0 超卖；零长度上传默认拒绝（`allowZeroLengthUploads` 显式开启），否则可无限产出「已完成、不过期、不占预算」的空制品。⑤ 回执 `x-duthub-artifact` 只能是相对传输根的路径（绝对路径泄露盘符/用户名），且只在「无摘要或校验通过」时写；`render_rejection` 要显式移除该头与 `upload-offset`。⑥ 同一传输根只能有一个写者：`serve` 用 `.duthub-transfer.lock` 独占锁（Rust 1.89+ 的标准库 `File::try_lock` 即可；fs4 1.1 的方法名是 `try_lock`/`try_lock_shared`，**没有** `try_lock_exclusive`），锁文件句柄随 listener 句柄存活；清扫只做「过期 + 孤儿 + 损坏 + 墓碑」，不碰已完成制品的 TTL。

## 2026-11-25 goose 下载分发的启动门（自动追加）

> 本节由「先修 Agent 不可用」轮追加（门禁复核 F1/F2 两条 high）；只新增、不改/不重排既有内容；坑编号接续上文。

140. **安装记录的 `sha256` 是归档摘要，不能拿去校验可执行文件**：`duthub-tooldown` 的 `InstallManifest.sha256` 记录的是**下载的 zip 字节**摘要（manifest.rs 注释与 install.rs 一致），而启动门要校验的是安装后真正执行的 exe。两者永不相等，症状是「页面显示 Downloaded、启动必报『已下载候选的字节与安装记录不一致，需要重新下载』」，用户重下也无法恢复（下载装的还是同一个 zip，记录的还是归档摘要）。修法：安装时在 staging 阶段为每个 `EntryKind::Executable` 计算并写入 `executableSha256`（与 `executables` 同序），`tool_resolver::downloaded_install_in` 改为返回 **exe 摘要**；旧 manifest 无该字段时按「需要重新安装」处理（fail-closed），不要回退到拿归档摘要比较。断言：`pinned_install_...` 增加 exe 摘要断言、`manifests_without_executable_digests_still_parse`、`downloaded_install_reports_the_executable_digest_not_the_archive_digest`、`legacy_manifest_without_executable_digest_counts_as_not_installed`。

141. **发布版 `goose.exe` 必须带 `acp` 子命令启动，`goose-acp.exe` 不能带**：通用 goose 是多命令 CLI，空参数会进 `configure`（实测 stderr `Error: goose configure requires an interactive terminal.`、EXIT=1），ACP 会话永远初始化不了；`goose.exe acp` 才会按 ACP 应答 initialize。独立 `goose-acp.exe` 本身就是 server、不接受 `acp` 参数。规则集中在 `registry::goose_acp_args(program, configured)`：显式 `args` 非空时原样优先；否则文件主干为 `goose` 时补 `acp`，为 `goose-acp` 时不补。下载档、PATH 档、外部路径档三条启动链都必须走同一个判定（`resolve_builtin_from` 两处 + `resolve_external` 一处），别只修其中一条。**外部路径档不看 agent id**：任何条目指向名为 `goose.exe` 的程序都补 `acp`（`agent_registry::two_acp_entries_are_selected_per_dut` 就是「第二个 ACP 条目复用同一 goose 二进制」，按 id 判断会让它在 `configure` 处退出、报「ACP 进程已关闭」）。**第四条入口是 `AcpClient::spawn` 本身**：测试与未来启动器会直接 `AcpClient::spawn(exe, &[], …)`（`goose_lifecycle`/`goose_mcp_injection`/`agent_host` 共 8 处），所以规则也放在 `spawn` 里统一补，别只修 registry 三条链（否则表现为 initialize 拿 `Err(Closed)`）。

## 2026-11-25 每 DUT 容量口径拍板（自动追加）

> 本节由容量口径落定轮追加（用户拍板「已完成镜像计入每 DUT 容量、按时间淘汰最旧」）；只新增、不改/不重排既有内容；坑编号接续上文。

140. **每 DUT 容量的记账与淘汰是两条独立路径，别混进 TTL 清扫**：`dutStorageBytes` 的判据 = `data/` 载荷（`used_bytes`，不含 `metadata/` 开销）+ 全库在途余量（`inflight_remaining_bytes`）+ 本次声明长度；不足时 `evict_completed(need)` 按**修改时间从最旧**删除已完成制品（持每上传锁、在途上传一律跳过、零字节制品淘汰无收益所以跳过），淘汰后仍不足才 507。已完成制品**不按 TTL 删除**——`is_expired` 继续只看未完成上传；把"淘汰"塞进 `sweep` 会让正常保有的镜像被周期清除。回归：`tests/isolation.rs` 的 `quota_evicts_the_oldest_completed_artifact` / `quota_never_evicts_an_inflight_upload`；`API_CONTRACT.md:123` 与 `TRANSFER_RECEIVER.md` §6 现在同口径。

## 2026-11-25 Agent 沙箱照搬 Codex（专用身份 + deny-read）（自动追加）

> 本节由「按 Codex 方式修复读隔离」轮追加（`crates/duthub-sandbox` 重写 windows 后端）；只新增、不改/不重排既有内容；坑编号接续上文。

142. **AppContainer 令牌带宿主用户 SID 与普通组，读隔离只能靠「换主体」**：实测容器内 `[WindowsIdentity]::GetCurrent()` 得到的用户是宿主 SID，组里有 `Everyone/Users/Interactive/Authenticated Users`，**没有** `ALL APPLICATION PACKAGES`；对兄弟 DUT 单元逐个 SID deny 的结果是：deny 容器 SID、`S-1-15-2-1`、capability SID 都不生效，只有 deny `Everyone` 才拦得住——也就是说读授权来自宿主身份。结论：无专用身份的 AppContainer 只能做写/网络/进程树隔离，**读隔离必须在机制上换主体**（Codex 的做法）。已照搬落地：`identity.rs`（专用本地组 `DuthubSandboxUsers` + 账户 `DuthubSandbox`、随机口令 `UF_DONT_EXPIRE_PASSWD`、口令只存 DPAPI 封存件 `%LOCALAPPDATA%\duthub\sandbox\identity.txt`）、`dpapi.rs`、`token.rs`（`LogonUserW` → `CreateRestrictedToken(DISABLE_MAX_PRIVILEGE|LUA_TOKEN|WRITE_RESTRICTED)`，restricting = capability + 登录 SID + Everyone，回验「令牌用户 ≠ 宿主、非启用管理员」）、`deny_read_acl.rs`（ACE 只发给沙箱账户 SID，绝不发给宿主用户，否则连带伤 duthub；加失败回滚）、`no_reparse_dir.rs`（`OBJ_DONT_REPARSE` + 盘符→卷设备名）、`acl.rs` 改句柄式 `GetSecurityInfo/SetSecurityInfo` 消除 TOCTOU。**一次性管理员初始化**：`cargo run -p duthub-sandbox --example provision-sandbox`（非提权只报错、不建账户、不弹 UAC），`--check` 只读；未初始化时 `deny_read` 非空一律 `Setup` 错误（fail closed），T3/T4 与两个 `whoami` 身份用例按「打印原因后跳过」处理，`status.identity/read_deny = Unverified`，UI 因此不得宣称已沙箱。

143. **capability SID 不能放在可写单元里**：旧实现把 `capability.sid` 写在单元根（被授予 `FILE_ALL`），同一 DUT 的 agent 可删除/重建它并写入任意合法 SID，下一次 `prepare` 会信任它并据此构造 restricting SID——写边界可被持久化绕过。现移至宿主管辖、沙箱不可写的 `<work>/duthub-data/sandbox/capabilities/<dut_key>.cell`，加载时校验：记录的 canonical root == 本单元、SID 形状为本 crate 生成（`S-1-5-21-a-b-c-d`、末段 ≥1000、非 well-known）、文件不可被 Everyone/Users 写入。另：`GOOSE_PATH_ROOT`（`<dut>/goose`）在单元之外，受限令牌下 goose 能启动但写不了 config/sessions——`agent_policy(cell, Some(goose_root))` 必须把该目录作为**同一 DUT 的第二个可写根**（仍不跨 DUT），否则表现为「会话能连上、一存就失败」。

## 2026-11-25 通用 goose 的内置扩展与会话失败（自动追加）

> 本节由「真实下载 goose 跑 ACP」轮追加；只新增、不改/重排既有内容；坑编号接续上文。

144. **通用 `goose.exe` 自带 `apps`/`analyze` 内置扩展，会被工具面校验 fail-closed 拒绝**：`validate_extension_results` 要求 `session/new` 的 `_meta.extensionResults` **恰好一个** `duthub` success（AGENTS #62 的红线，不许放宽）。精益版 `goose-acp` 只有 duthub；从「工具下载」装的通用 `goose.exe 1.60.2` 会返回 `[duthub, apps, analyze]`，表现为会话停在 `Failed("goose 工具面校验失败: 期望恰好 1 个扩展…实际 3 个")`，`agent_host_hardening` 里等待 Ready 的用例全部超时。修法：在托管配置 `model_env::goose_config_yaml()` 里把 `apps` 与 `analyze` 一并 `enabled: false`（与既有的 developer/summon/tom/extensionmanager 同形状），校验函数保持严格；goose 以后再新增内置扩展时同步该表与 `managed_config_disables_all_builtins`。验证：`DUTHUB_TEST_GOOSE=<真实 goose.exe> cargo test -p duthub-desktop --test agent_host_hardening` → 9 passed / 0 failed。

## 2026-11-25 专用沙箱身份与提权现实（自动追加）

> 本节由管理员初始化实测轮追加；只新增、不改/重排既有内容；坑编号接续上文。

145. **专用身份初始化成功 ≠ 能用：`CreateProcessAsUser` 在非提权进程一律 1314**（2026-11-25 实测）：按 Codex 建好 `DuthubSandbox` 账户/组并 DPAPI 存凭据后，所有沙箱启动失败在 `CreateProcess 失败: 客户端没有所需的特权 (os error 1314)`——`SeAssignPrimaryTokenPrivilege`/`SeIncreaseQuotaPrivilege` 只在提权令牌里。这正是 Codex 必须有 `elevated_impl` + framed pipe（一次性提权 helper）的原因。处置口径：**能力探测必须包含这两个特权**（`OpenProcessToken` + `LookupPrivilegeValueW` + `PrivilegeCheck`，不要用「是否管理员」代替），缺任一 → `status.identity = Unverified` 并写明「已初始化但当前进程未提权（1314）」，`credentials` 只在三门（凭据可加载 + 特权持有 + 账户可登录且非启用管理员）全过时下发，**结构上禁止「先试后降级」**；身份不可用时回到宿主受限令牌路径（写/网络/进程树/Job 照常），`deny_read` 非空仍 `Setup` 硬拒绝。读隔离在当前分发形态（非提权桌面应用）**不可交付**，两条未实现路径写在 ADR-12 §4.1.3：① 提权 helper（父 PID + nonce + DuplicateHandle）；② `CreateProcessWithLogonW`（无需该特权，但没有 AppContainer，只能按用户防火墙断网，会连带挡回环）。凭据目录默认 `%LOCALAPPDATA%\duthub\sandbox\identity.txt`（与「数据只落项目目录」有偏差，已留 `set_identity_root`/`DUTHUB_SANDBOX_IDENTITY_ROOT`/`provision-sandbox --root` 三个改道口）。

## 2026-11-25 goose 版本探测的用户目录写入（自动追加）

> 本节由 GOOSE_PATH_ROOT 收敛测试实机失败轮追加；只新增、不改/重排既有内容；坑编号接续上文。

146. **通用 `goose.exe` 连 `--version` 都会写 CLI 日志，探测必须自带 `GOOSE_PATH_ROOT`**：`goose_path_root::host_keeps_goose_state_under_goose_path_root` 断言「会话结束后 `%APPDATA%\Block\goose` 零变化」，实测失败——新增文件是 `data/logs/cli/<日期>/<时间>.log`，来源不是 Agent 会话（会话在受限令牌下写不了那里），而是 duthub 自己每次解析 goose 时跑的 `--version` 探测。修法：`duthub-core::process::run_binary_with_env`（`run_binary` 的零改动包装）+ `bundle::probe_version` 注入 `GOOSE_PATH_ROOT=<exe 同级 .goose-probe>`（不可写时回退 OS temp）、`GOOSE_DISABLE_KEYRING=1`、`GOOSE_TELEMETRY_OPTOUT=1`。**不要**为了让该测试变绿去放宽「用户目录零写入」的断言——探测属于可信宿主代码，同样不该污染用户目录。

147. **配置导出复用原子保存，CI 摘要只列失败用例名**：导出与配置落盘共用原子替换逻辑。常驻 MCP 凭据的明文 Token 在设置模型内，导出副本必须清空凭据表。导入按钮按界面显示的配置目录重新加载。CI 摘要和 annotation 可公开，测试日志与失败详情留在步骤日志。

148. **MCP 快照按最新窗口读取并在锁外组装**：按会话尾部读取最新 150 条事件，再筛出串口内容，保留原有 seq 顺序和窗口语义。完整快照组装结束后一次替换共享值。用首段补读替代尾部读取会让 stdio 的 seq 游标停在旧位置。

149. **虚拟终端视口必须显式启用 flex 布局**：GPUI 的 `flex_col()` 只设置方向。`uniform_list` 默认使用 Auto 并由父容器分配高度，父容器漏掉 `.flex()` 会把列表压成零高。排查空白终端时同时核对实际 TX、RX 与可见画面。

150. **模拟进程测试仍会走可执行文件解析**：ADB 客户端在调用试验替身前解析路径。测试使用自身可执行文件的绝对路径，避免本机 PATH 已装 adb 时通过、GitHub runner 未装时失败。

151. **编译禁止使用 C 盘，只在当前仓库 `target/` 目录编译（2026-11-25 用户拍板，废止 #12/#16 的 C 盘 target 口径）**：任何构建、检查、测试均不得将 `CARGO_TARGET_DIR` 指向 C 盘或仓库外路径，统一在当前仓库 `target/` 目录内编译；自检或临时测试目录同样避免往 C 盘写大产物。

152. **默认继电器预设不要求立即配置串口**：未绑定的空串口板卡可随 DUT 保存。连接页只列出串口已配置的板卡，绑定时再次校验板卡的串口和通道范围。

153. **Windows PowerShell 初始化位置要与沙箱单元一致**：脚本启动前使用受宿主管理的工作路径执行 `Set-Location -LiteralPath`，失败立即停止。AppContainer 在某些卷上的路径访问仍可能被 ACL 拒绝，应报告真实状态。编译与测试二进制继续落仓库 `target/`。

154. **终端右键菜单按文案测量宽度**：保留最小点击宽度，菜单项不换行。用 GPUI `anchored` 根据实测尺寸避开窗口边缘，避免固定宽度给短文案留出过多空白。

155. **串口鼠标操作只绑定终端视口**：右键粘贴不能从查找框等其他输入控件触发。自动复制只在有效选区结束时执行一次，保留选区，避免拖动期间持续覆盖剪贴板。

156. **模型探测原因优先显示服务返回的错误消息**：提取结构化响应中的 `error.message` 或 `message`，保留 HTTP 状态码，并在截断前脱敏 API Key。

157. **启用启动日志参考时以串口字节识别分段**：参考文本从多轮确认的上下电采样生成，按最低两两相似度验收。路径和阈值取该 DUT 在 `duts.json` 的配置，未指定路径时查找该 DUT 的学习产物。继电器动作只记标记，等待观察到启动前缀再同步切分文本、原始和结构化日志。跨读取块匹配只保留可能命中的尾部，结束时排空。

158. **串口日志分段与终端输出分离**：归档目录冲突和分段失败写应用诊断，不向串口事件总线发送注记。供电前持久化失败仍由控制入口接收。连接管理预览草稿使用独立的回环采集，播放器退出时释放设备。

159. **串口持续保存，分段模式按 DUT 生效**：`serial.splitLogs` 随连接保存，并通过该会话的日志队列切换。关闭分段时跨会话追加 `logs/serial.txt` 及配套的 `.bin/.rec`。连续文件沿用 DUT 所有权和容量控制，并纳入 MCP 历史日志查询。

160. **默认本地回归不启动桌面窗口**：`scripts/regression` 只跑格式、Clippy 和工作区测试。验证窗口渲染时显式传 `--desktop-self-test`，脚本在其他检查之后启动桌面自检。

## 2026-11-26 全仓重构与安全加固（Wave 0–6，自动追加）

> 本节由全仓 Rust 1.100 / 安全 / 架构 / 性能 / 全球化重构轮追加；只新增、不改/不重排既有内容；坑编号接续上文。

159. **MCP Bearer Token 迁移至 OS keyring，落盘走 `save_settings_secured`**：`settings.json` 不再明文写入常驻凭据的 `token`（`skip_serializing_if = "String::is_empty"`），桌面启动经 `hydrate_mcp_tokens` 从 `duthub/mcp/<cred_id>` 回填内存并对旧明文自动迁移脱敏；所有桌面写 settings 路径统一走 `save_settings_secured`，撤销凭据与删除 DUT 联动 `delete_mcp_token`。`hydrate_mcp_tokens` 返回类型化枚举 `McpTokenHydrateWarning` 由桌面按当前语言本地化（禁止库层硬编码中文）。
160. **Windows 非提权桌面执行策略 = 内核写边界 + Codex 风格 PowerShell AST 策略（`duthub-agent`）**：`crates/duthub-agent` 承载 HTTP LLM 探测、`policy.rs`（`WorkspaceScope`：活跃 DUT RW、兄弟 DUT RO、`.duthub/` 与用户凭据目录读写双禁）、`ps_ast.rs`（调用 PowerShell 原生 `Parser::ParseInput` 零执行提取 AST，按 `Forbidden > Prompt > Allow` 最严优先格判定）与 `sandbox_exec.rs`（AST 闸门后经 `duthub-sandbox` `RestrictedToken` + Job Object 启动）。未来 Windows 11 25H2+ MXC（`microsoft/mxc`）在 `SandboxBackend` trait 预留扩展缝。
161. **通用 MCP 监听器与 A2A / Transfer 安全收紧**：① `authorize_scoped_read` 在通用监听器上同样强制校验每 DUT 的 `DutAction::Read` `DutGrant`（废止旧"任意读 grant 跨 DUT 读文件"口径，与坑 #44/#131 一致收紧）；② `serial_write_tool` 在返回 `write_receipts` 缓存回放前先复核 `lease_identity`；③ `McpServerCore::list_tools`/`get_tool` 在 `evidence_scope` 下只返回 8 个证据工具；④ `duthub-a2a` 增加非回环 `Origin` 拒绝（403）、认证失败滑窗限流（429）、`TaskOwnership` 4096 上界 FIFO 淘汰、`CancelFlags` 退出清理、`(message_id, skill, sha256(args))` 去重键与可选 `AuditSink`；⑤ `duthub-transfer` 改为 SHA-256 恒时比较、`UploadOp::Create` 在板卡作用域非空时缺 `board` fail closed、`ensure_transfer_dir_writable` 在 `create_dir_all` 前先检全链路 reparse point。
162. **桌面渲染热路径与模块拆分不变量**：`apps/desktop/src/{pages,workspace}.rs` 已按领域拆为 `pages/` 与 `workspace/` 子模块，并由 `apps/desktop/tests/architecture_invariants.rs` 钉住五项红线：① `duthub-core`/`duthub-agent` 维持 `#![forbid(unsafe_code)]` 且 `duthub-core` 零 `windows-sys`/`reqwest`；② `PLAN_TOOL_NAMES` 单一真相源；③ `zh-CN.json`/`en-US.json` 1:1 键集对齐；④ 桌面 UI 渲染模块禁止出现硬编码中文界面文案；⑤ 桌面共用 `main.rs` 单一 Tokio 运行时（`DeviceService::with_runtime`、`ControlService::with_runtime`、`A2aRuntime::start_with_handle`），`SnapshotWriter` 按内容摘要脏检查零空闲写盘，`probe_os_isolation` 10s TTL 缓存，`compiled_highlight_rules` 按 `Arc` 缓存，`refresh_tool_rows` 仅在安装完成时触发。

163. **AppContainer 的 PowerShell 驱动器与 NTFS 目录权限是两回事**：如果默认盘符无法解析已授权的工作目录，进程内用 FileSystem `New-PSDrive` 挂载该目录，再切换位置。保留 Win32 的原生工作目录和原有权限。挂载或切换失败时不运行用户脚本。回归用 D 盘单元验证相对路径写入、越界拒绝和超时终止进程树。
