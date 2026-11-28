# 托盘关窗 · 放大告警 · 继电器掉线监测 · Agent 通知（设计）

> 状态：第 6 版（含 P1–P6 最终定向修订），待实现。本文是唯一设计源；实现方照 §8 冻结签名编码。
> 事实来源：本仓库源码 + `windows 0.62.2` / `gpui-pre-0.3.12` / `rmcp-3.10.0` 源码阅读（行号见正文）。**本轮未编译、未运行任何测试**；「不会 panic / 不丢最大化」类结论均为源码推导，证伪项见 §10。

## 0. 结论（feasibility）

**feasible = true**，四条硬前提：

1. **不引入 GUI/托盘框架**：托盘用现有 `windows 0.62.2` 手写；仅新增 `raw-window-handle = "0.6"`（与 gpui 同版本，`Cargo.lock:6166` 已是 0.6.2）。
2. **关闭 = 隐藏窗口**（`ShowWindowAsync(SW_HIDE)`，由 tray 执行），**不**调 `remove_window()`（`QuitMode::Default` 在 Windows 等价 LastWindowClosed，判定 `quit_on_empty && cx.windows.is_empty()`，`gpui-pre-0.3.12/src/app.rs:3294-3302`）。
3. **关浮窗必须发生在没有 Workbench 租约的上下文**（§2.1）；`window_closed` 观察者在 `trail` 内同步执行（`app.rs:3269-3292`），否则其 `weak.update` 必撞 `double_lease_panic`（`app/entity_map.rs:188-195`、`311-313`）。
4. 心跳/继电器/MCP/Agent/采集都是**渲染无关**执行体（§3）；`render_root` 独有的后台职责只有 MCP 快照，迁入既有 50 ms tick（迁移，不是复制）。

## 1. 需求与行为契约

| # | 用户需求 | 可验收契约 |
|---|---|---|
| R1 | 心跳判板子卡死 → 弹出 ⚠ 放大警告 | 可见态沿用「需要人工干预」Dialog，`icon("alert")` 25px → 44px；只改尺寸/布局，**不动任何既有文案** |
| R2 | 点 X = 最小化到通知区域，程序继续运行 | 首次 render 注册图标（§2.1），四条关闭入口 → 隐藏窗口；进程/会话/采集/MCP 存活；**双开不得产生无法关闭的隐藏进程**（§2.3） |
| R3 | 已在通知区域 → 不弹窗，改为图标抖动 | 隐藏态由 50 ms tick 的 `alert_present_tick` 消费 AlertCenter：气球 + 闪烁（§5.1）；恢复后按队列补弹 |
| R4 | 继电器掉线也是异常 | 板卡级单通道只读 STATUS；`Online→Offline` 才告警；占用/按住测试/PDU/空 node/未绑定**不算掉线**（§6.2） |
| R5 | 最小化后整个程序正常运行 | §3.1 职责表：每行都有隐藏后仍运行的 owner |
| R6 | 服务器 Agent 也能通知我（MCP 工具） | `notify_user` 只对**用户配置的通用/DUT MCP 监听器**可用（§7）；走同一条 `AlertCenter`（来源 `agentNotice`），可抑制，返回真实结果，不得挤掉设备告警（§5.3） |

非目标：任务栏进度/缩略图工具栏、通知中心留档、富通知按钮、A2A 通知、**桌面发起的 Agent 会话**（内嵌 goose 与 claude/codex CLI profile，见 §7）。

## 2. 窗口生命周期

### 2.1 托盘安装（首次 render）+ 关闭（四条入口 → 一个决策点）

**首次渲染时注册托盘图标**：`tray::install(window, options)` 获取 `HWND` 并创建托盘宿主。安装成功后，工作台调用 `set_icon_visible(true)` 注册图标。注册失败时保留宿主，供下次隐藏窗口时重试。`--self-test` 跳过安装。

```rust
// render_root 首次帧
if !self.agent_self_test && !self.quitting && self.tray.is_none() && !self.tray_failed {
    match tray::install(window, TrayOptions { tooltip: t(trayTooltip), show_label: t(trayShowMain), quit_label: t(trayQuit) }) {
        Ok(tray) => {
            let _ = tray.set_icon_visible(true);
            self.tray = Some(tray);
        }
        Err(error) => {
            self.tray_failed = true;
            eprintln!("[tray] install failed: {error}");
            self.notify_toast(crate::i18n::label("trayUnavailable"));
        }
    }
}
```
`tray` 为 None 时 `close_disposition` 返回 `Quit`：关闭退回原生退出并已 toast 说明，**绝不假装隐藏**。

入口：自绘标题栏 X（`workspace.rs:7965-7971` → HTCLOSE → `PostMessageW(WM_CLOSE)`，`gpui-pre-windows/src/events.rs:1153`）、Alt+F4、系统菜单、任务栏关闭。全部落到 `WM_CLOSE` → `handle_close_msg`（`events.rs:317-323`）→ `on_window_should_close`（唯一挂载点 `workspace.rs:7104-7113`，回调在 `weak.update(Workbench)` 内）。

**关闭顺序是纯函数（P2）**：`tray_ui::close_plan(disposition) -> &'static [CloseStep]` 固定为 `HideToTray → [ReleaseRelay, CloseFloat, HideWindow]`、`Quit → [ReleaseRelay, CloseFloat, DestroyMain]`；顺序由单测锁定，源码契约只钉「关闭处理器调用同一个执行器 `run_close_plan`」，**不断言**任何步骤相对 `weak.update` 的位置（`release_relay` 只能经 `weak.update` 到达）。

```text
should_close(window, cx)                     // 回调内只读字段，绝不做窗口操作
 ├ quitting → true                            // 托盘菜单可在主窗口打开或隐藏时发起退出
 └ close_disposition(tray.is_some(), false) 的两个分支都：cx.defer(move |cx| run_close_plan(plan)); false

run_close_plan(cx: &mut App)                 // defer 回调：此刻没有 Workbench 租约
 ├ ReleaseRelay → weak.update(|t, cx| t.release_relay(cx)) + 若确有按压则 log_mark 留痕
 ├ CloseFloat   → if let Some(h) = float_handle { let _ = h.update(cx, |_, w, _| w.remove_window()); }
 │                 浮窗观察者（唯一 on_window_closed 的 float 分支，body 已 defer）→ UserClosedWindow → Floating→Docked
 └ HideWindow   → weak.update(|t, cx| t.finish_hide_to_tray(cx))：tray.hide_main()（失败→ tray.show_main() 回滚 + hidden=false
                  + toast(trayIconFailed)）→ hidden=true / set_merge_duplicates(true) / alert_router.arm()
   DestroyMain → main_handle.update(|_, w, _| w.remove_window())   // 主窗观察者（main 分支，同样 defer）
                 → MainWindowClosed → Docked → windows 空 → quit_on_empty → cx.quit()
```

**为什么 Quit 也必须走这条链**：浮窗也是窗口，主窗销毁后 `windows` 不空 → 进程不退且只剩孤立浮窗；随后主窗观察者 feed `MainWindowClosed` → `(P::Floating, E::MainWindowClosed) => (P::Closing, A::CloseFloatWindow)`（`capture_ui.rs:112-116`）→ `close_float_window`（`workspace.rs:7042-7049`）在自己的租约内 `float.handle.update(...remove_window())` → 浮窗 `trail` 再调观察者 → `double_lease_panic`。先关浮窗再销毁主窗，`windows` 才会真的空。**两条必须同时做的防呆**：① **唯一的 `cx.on_window_closed` 订阅（float / main 两分支）body 一律改为 `cx.defer(move |cx| { let _ = weak.update(...) })`**——观察者在 `trail` 里同步执行，推迟一拍就从根上消除「在别人的租约里 weak.update 自己」这一类 panic。② **运行期 `NIM_ADD` 失败必须有回退**（`hide_main()` 返回 false 时同步回滚；`TaskbarCreated` 重放失败由托盘线程发 `TrayEvent::IconLost`，UI 收到后 `show_from_tray` + toast `trayIconFailed`）。也不能改成「先 `take()` 再 `remove_window`」：观察者靠 `float_window.handle.window_id()` 匹配（`workspace.rs:1593-1611`），take 后永不匹配 → placement 停在 `Floating`、`frame_pump.float` 被清空 → `release_monitor` 因 `Floating` 提前返回（`workspace.rs:6970-6976`）。

`show_from_tray`：`tray.show_main()`（内部 `ShowWindowAsync(main_hwnd, SW_SHOW)`；**不是** `SW_SHOWNORMAL`——后者在最大化/最小化时恢复原始尺寸会丢最大化）→ `main_window.update(cx,|_,w,_| w.activate_window())` → `hidden=false`、`set_merge_duplicates(false)`、`tray.set_alert(false)`。恢复后托盘图标继续保留。显示窗口应先于激活：`activate()` 只在 `IsIconic` 时 `SW_RESTORE`，对隐藏窗什么都不做（`gpui-pre-windows/src/window.rs:817-829`）。一律 `ShowWindowAsync`（gpui 最小化路径同样用异步版，`events.rs:1139-1142`）：从 update 里同步 `ShowWindow` 会重入 WndProc 触发一次被 defer 的 draw。

### 2.2 状态与退出

`Visible`（可见、有托盘图标、应用内 Dialog）→ `Hidden`（隐藏、有图标、气球+闪烁）→ `Quitting`（图标删除、不呈现告警）。`quitting` 置位后 `close_disposition` 恒 `Quit`。`Workbench::begin_quit(&mut self, cx)`：① **`release_relay_and_join(cx, 3s)`**——`release_relay` 只发 oneshot（`workspace.rs:5014-5020`），worker 要等 `released.blocking_recv()`（`control.rs:469`）之后才 `backend.power(off)`（`:472-478`），退出必须等它：发信号后轮询 `RelayPress.worker.is_finished()` 至多 3 s，完成即 `join()` 取结果；超时记日志「继电器 OFF 未确认」并继续退出（绝不再重发 ON）。隐藏路径只发信号不 join（UI 还在跑，worker 会自己完成）。② 逐个 `device.close_session(dut_id)`（`try_send(Close)` + 异步 abort，**不阻塞 UI 线程**；写线程收尾在会话 task 的 `SessionCtx` Drop 里，3 s 兜底，坑 #47）。③ `agent_host.take() → stop_all()`、`a2a.take() → shutdown()`、`mcp_runtime.shutdown()`（每监听器 ≤500 ms，`mcp_runtime.rs:512-520`）。④ `tray.remove()`（§4.5）→ `cx.quit()`。UI 线程预算 ≤5 s（含 ①），超时仍继续退出。**用 `take()` 保证 `Drop for Workbench`（`workspace.rs:1200-1211`）幂等**。采集/录屏不在此拆：`Capture::Drop`/`InlinePreview::Drop`（`media.rs:526` 起）+ 应用级 Job 容器（`main.rs:38-47`）兜底；录制中会话被 pin（坑 #94）。

**系统关机/注销**：`WM_QUERYENDSESSION → 1`、`WM_ENDSESSION → handle_end_session → quit 回调 → PostQuitMessage`（`gpui-pre-windows/src/platform.rs:1221-1222`、`1274-1290`），**绕过** `should_close`，隐藏态照样退出；托盘线程随进程退出（注销时 shell 自清图标），`--self-test` 从不安装托盘。副作用：不带 `/F` 的 `taskkill` 结束不了进程（WM_CLOSE 被吞），回归脚本用 `/F`。**最小化按钮保持原生**：`WindowControlArea::Minimize` 由 gpui 直接 `ShowWindowAsync(SW_MINIMIZE)`（`events.rs:1139-1142`）。官方要求「隐藏挂 Minimize、不要劫持 Close」；本产品按用户需求偏离，故偏离只限 Close。

### 2.3 单实例

`fn main()` 在 `contain_current_process()` 之后、开窗之前：`CreateMutexW(None, FALSE, "Local\\duthub-<hash>")`，`hash` = 小写 exe 绝对路径的 `DefaultHasher` 64 位（std，无新依赖）。`ERROR_ALREADY_EXISTS` → `MessageBoxW`（「duthub 已在运行：请在通知区域图标上右键「显示主窗口」，或先退出再启动」）→ `exit(0)`；`--self-test` **跳过**该守卫。理由：同一 exe 共享同一 `NIF_GUID`，第二实例的 `NIM_ADD` 会顶掉第一实例的图标，随后自身窗口被隐藏却收不到托盘事件 → X 被吞。

## 3. 隐藏状态的后台调度

**证据（源码阅读，未运行）**：50 ms 服务 tick = `cx.spawn` + `cx.background_executor().timer(..)`（`workspace.rs:1798-1824`）；timer 由 scheduler 独立 OS 线程驱动（`gpui-pre-0.3.12/src/executor.rs:200-205` → `gpui-pre-scheduler-0.3.12/src/scheduler.rs:219-231`），唤醒经 `PostMessageW(…, WM_GPUI_TASK_DISPATCHED_ON_MAIN_THREAD)` 投到 gpui 自建隐藏平台窗口（`gpui-pre-windows/src/dispatcher.rs:119-129`、`platform.rs:213-238`），主循环是线程级 `GetMessageW(&mut msg, None, 0, 0)`（`platform.rs:583-589`）——与主窗可见性无关。隐藏窗不消费 `WM_PAINT`（gpui 注释 `platform.rs:468-476`）→ `render_root` 停摆。

### 3.1 职责表（隐藏后 + 单一 owner）

| 职责 | 隐藏后 | owner |
|---|---|---|
| 心跳探测 + 告警入队 | ✅ | 50 ms tick → `heartbeat_tick`（`workspace.rs:5612-5633`） |
| **隐藏态告警呈现（气球/闪烁）** | ✅ | 50 ms tick → **新增 `alert_present_tick`**（§5.1） |
| 告警 Dialog 出队 | ❌（设计如此） | 只在 `render_root`；隐藏期排队，恢复后补弹 |
| 继电器巡检 + 事件回传 + 集合对齐 | ✅ | 独立 `std::thread`（§6.3）→ mpsc → tick 转 AlertCenter/toast；tick 直接对齐 `self.duts.file`（幂等、廉价，不枚举持久化点） |
| MCP 快照 | ✅ | **迁入** 50 ms tick，250 ms 节流（见本表下方「快照节流」） |
| MCP HTTP / 监听器 / Agent / 采集 | ✅ | 各自独立线程（`mcp_runtime.rs:588`、`main.rs:38-47`、`workspace.rs:1829-1845`） |
| 托盘事件排空 / 图标闪烁 / 托盘安装 | ✅ | 50 ms tick `drain_events` / 托盘线程 `WM_TIMER`；`install` 在 `render_root` 的 `!self_test` 分支（§2.1） |

媒体绑定对账**不迁移**：`MediaState::tick` 已调 `reconcile_bindings`（`media_page.rs:738-740`），50 ms tick 已调 `media.tick`（`workspace.rs:1807`）。本轮**只删** `render_root:7140` 那一行。

**隐藏前必须释放按住测试（安全红线）**：`hold_channel` 的 worker 独占 `relay_writes` 写门并阻塞在 `released.blocking_recv()`；现有释放入口只有渲染期的 `!window.is_window_active()` 守卫（`workspace.rs`）与 `observe_window_activation`，而隐藏后 `render_root` 停摆、`SW_HIDE` 是否投递 `WM_ACTIVATE` 未验证。因此 `ClosePlan` 的**第一步**恒为 `ReleaseRelay`（§2.1），`begin_quit` 同理且会 join。隐藏与退出都不允许把任何通道留在 ON。**门的方向**：巡检探测持读门（一次 STATUS 往返），`hold_channel` 取写门时**有界等待**在途探测（`RELAY_WRITE_GATE_WAIT = 1 s`，健康板 ≤300 ms）而不是立刻 `Busy`；端口卡死超过该上界仍如实返回 `Busy`，避免用户按下时被巡检窗口打断（回归测试 `tests/relay_hold_during_probe.rs`）。

**快照节流**：`refresh_snapshot` 对每台 DUT 调 `device.read_log(session, 0, usize::MAX)`（`workspace.rs:5097-5101`），而 `EventBus::read_log` 会克隆整个会话缓冲（`bus.rs:77-90`，`SESSION_BUFFER_CAP = 2000`，`bus.rs:18`）再只留最后 150 行。迁入 tick 后按 `snapshot_due_ms` 节流到 **250 ms**（隐藏期从 20 次/秒降到 4 次/秒；可见态同口径，不做双分支）。

## 4. 托盘实现（Win32）

### 4.1 线程、窗口与特性

独立托盘线程（`std::thread` + 自己的 `GetMessageW` 循环）：`TrackPopupMenuEx` 是阻塞模态循环，放 UI 线程会卡住 gpui。窗口 = **隐藏的顶层窗口**，类名 `DuthubTrayHost`，`WS_EX_TOOLWINDOW` + `WS_POPUP`（无 `WS_VISIBLE`）。**绝不用 `HWND_MESSAGE`**：`TaskbarCreated` 等广播只发顶层窗（gpui 同款判断 `gpui-pre-windows/src/platform.rs:213-218`）。**主窗 `HWND` 由 `install(window, …)` 用 `HasWindowHandle::window_handle(window)`（`gpui-pre-0.3.12/src/window.rs:7866-7868`）取得并存进 `TrayHandle`**，`hide_main`/`show_main` 都用它——A2 不接触 `windows` crate，也不需要在 tick 里拿 `Window`。`apps/desktop/Cargo.toml` 的 `windows` 特性目前只有 Foundation/Security/ToolHelp/JobObjects/Threading，**必须新增**：`Win32_UI_Shell`（`UI::Shell` 模块门 + `SHQueryUserNotificationState`；`NOTIFYICONDATAW`/`Shell_NotifyIconW` 自身 cfg 是 `Win32_UI_WindowsAndMessaging`）、`Win32_UI_WindowsAndMessaging`（`Shell_NotifyIconW`、`NIF_*`/`NIM_*`/`NIN_*`、`RegisterWindowMessageW`、`CreateWindowExW`、`SetTimer`/`KillTimer`、`CreatePopupMenu`/`AppendMenuW`/`TrackPopupMenuEx`（含 `TPM_RETURNCMD`）/`DestroyMenu`、`SetForegroundWindow`/`GetCursorPos`/`PostMessageW`/`PostThreadMessageW`/`WM_NULL`/`WM_QUIT`、`ShowWindowAsync`、`MessageBoxW`、`LoadImageW`/`DestroyIcon`/`GetSystemMetrics`、`SystemParametersInfoW`、`LoadCursorW`）、`Win32_UI_Controls`（`LoadIconMetric`、`LIM_SMALL`）、**`Win32_Graphics_Gdi`**（`WNDCLASSW`/`WNDCLASSEXW`/`RegisterClassW` 的 cfg 就是这个特性）、`Win32_System_LibraryLoader`（`GetModuleHandleW`）；`Win32_System_Threading`（`CreateMutexW` 还需已有 `Win32_Security`；`GetCurrentThreadId`）已有。**`NIN_KEYSELECT` 未被 0.62.2 导出**（`UI/Shell/mod.rs:51984-52016`）→ 本地 `const NIN_KEYSELECT: u32 = 1025;`。

### 4.2 图标与身份

图标与主窗同源：`build.rs:1-11` 把 `assets/app.ico` 嵌为 ID 1，托盘用 `LoadIconMetric(hinst, 1, LIM_SMALL)`；告警帧计划用 `assets/tray-alert.ico` = **ID 2**（`winresource::set_icon_with_id`），**本轮未交付该资源与 build 改动**：`load_alert_icon` 先试 ID 2，缺失时用 `IDI_WARNING` 的私有副本闪烁（绝不把普通图标当告警帧），连系统警告图标都拿不到才退化为纯气球；补齐资源是独立交付项（§12.6）。**不用 `LR_SHARED`**：两个 HICON 归我们所有，线程退出各 `DestroyIcon` 一次（gpui 主窗那个是 `LR_SHARED`，`platform.rs:1686-1700`，绝不混用）。**每一条 `Shell_NotifyIconW`（ADD/SETVERSION/MODIFY/DELETE）都必须带 `NIF_GUID` + 同一个 `guidItem`**：官方要求「用 GUID 识别后，后续任何调用都要用同一 GUID」；`uFlags = NIF_MESSAGE|NIF_ICON|NIF_TIP|NIF_GUID|NIF_SHOWTIP`（v4 下标准 tooltip 默认被抑制），`cbSize = size_of::<NOTIFYICONDATAW>()`。**GUID 由当前 exe 绝对路径（小写）哈希成 128 位确定性生成，同一路径永不改变**——不能写死 const：单实例互斥量按 exe 路径分键，不同目录两份可同时运行，共用 GUID 时后退出者的 `NIM_DELETE` 会删掉先启动者仍在用的图标。代价是搬目录后图标偏好失效（与平台「GUID 注册绑定二进制路径」一致，§11.4）。

### 4.3 注册、版本、重建

`NIM_ADD → NIM_SETVERSION(uVersion = NOTIFYICON_VERSION_4)` 每次成对（版本设置不跨登录保持）。`TaskbarCreated = RegisterWindowMessageW("TaskbarCreated")` 必须在首次 ADD **之前**注册好；收到后按**当前期望状态**整体重放 `NIM_ADD + NIM_SETVERSION`；**重放失败 → `send(TrayEvent::IconLost)`**（UI 恢复主窗 + toast，§2.1 防呆②）。首次渲染成功注册后，图标在最小化、隐藏和恢复期间持续存在，退出时由 `remove()` 清理。

### 4.4 事件与菜单

v4 编码：`LOWORD(lParam)`=事件、`HIWORD(lParam)`=图标 ID。`NIN_SELECT`(1024)/`NIN_KEYSELECT`(1025)/`WM_LBUTTONUP`（防御性）→ `Show`；`WM_CONTEXTMENU`/`WM_RBUTTONUP`（防御性）→ 弹菜单；`NIN_BALLOONUSERCLICK` → `Show` + 停闪烁；`NIN_BALLOONTIMEOUT`/`NIN_BALLOONHIDE` → 停闪烁；`TaskbarCreated` → 重放（失败发 `IconLost`）；`WM_TIMER` → 交替 `NIM_MODIFY(NIF_ICON|NIF_GUID)` 并到时自停；poke（`WM_APP+2`）→ 读共享期望状态，幂等对齐。

菜单：`SetForegroundWindow(tray_hwnd)` → `GetCursorPos` → `TrackPopupMenuEx(menu, TPM_RIGHTBUTTON|TPM_BOTTOMALIGN|**TPM_RETURNCMD**, x, y, tray_hwnd, None)`（返回值**直接**是选中项 id，**不需要 `WM_COMMAND` 分支**）→ `PostMessageW(tray_hwnd, WM_NULL, 0, 0)` → `DestroyMenu`；**返回 0（未选/被 Esc 取消）时**调 `NIM_SETFOCUS` 把焦点还给通知区域，选中项不调。两项文案由 `TrayOptions` 传入：`显示主窗口` → `Show`、`退出` → `Quit`。

### 4.5 气球、抖动与线程收尾

标题 ≤48 / 正文 ≤200 字符（缓冲上限 64/256 含 NUL），按 **UTF-16 码元**截断；**非 critical 告警恒带** `NIIF_RESPECT_QUIET_TIME`，critical 设备告警**必须省略该标志**——Windows 在 quiet time 对带该标志的气球直接丢弃且不排队，省略是「QuietTime 仅放行 critical」唯一可实现的路径（`TrayNotification.respect_quiet_time`，由 `tray_lifecycle::balloon_notification` 按 `source != AgentNotice` 决定）。两种气球都带 `NIF_GUID`；等级由告警的 `AlertLevel` 映射（Info/Warning/Error → `NIIF_INFO|WARNING|ERROR`，§5.1）。时长不可控（Vista 起约 9 s）；Win10 留通知中心、Win11 超时即消失；一次只一个气球，其余排队 → 风暴靠 §5.3 合并 + 限流。溢出区（如实）：新图标默认进溢出区、**应用无权提升**，系统会为隐藏图标临时提升显示通知；故隐藏态必须发气球，不能只靠图标闪烁。`FLASH_INTERVAL_MS = 500`，硬上限 `FLASH_MAX_MS = 30_000`，托盘线程 `WM_TIMER` 自停（UI 侧另有判据）。停止：恢复窗口、点击气泡、超时、确认、系统关动画 → `set_alert(false)` → `KillTimer` → `NIM_MODIFY` 回常态图标；托盘线程接 `WM_SETTINGCHANGE`（`SPI_SETCLIENTAREAANIMATION` 变化即广播），`tray_ui::flash_stops_for_animation_change` 为真时停闪，不需要自建轮询。闪前查 `animations_enabled()`（`SPI_GETCLIENTAREAANIMATION`，`FALSE` → 不闪，只发气球）；通知前查 `notification_gate()`（`QUNS_ACCEPTS_NOTIFICATIONS` → `Allowed`；`QUNS_QUIET_TIME` → `QuietTime`，仅 `critical` 告警才发气球；`QUNS_BUSY`/`QUNS_PRESENTATION_MODE`/`QUNS_RUNNING_D3D_FULL_SCREEN`/`QUNS_NOT_PRESENT` 与读失败 → `Blocked`，只排队）。两函数由 **tray.rs（A1）**提供（§8），A2 只调用。官方明文「Don't flash the icon … too distracting」；保留闪烁是用户显式需求，故可停、≤30 s、受动画与通知状态约束，标注为**非推荐做法**。

**收尾顺序**（`remove()` 幂等，`Drop` 再调一次也安全）：`KillTimer` → `NIM_DELETE`（带 GUID）→ `PostThreadMessageW(tray_tid, WM_QUIT)` → 有界 join（500 ms）。**仅 join 成功才 `DestroyIcon` × 2**；join 超时说明线程可能还停在 `TrackPopupMenuEx` 里持有 HICON，此时**故意泄漏**并只记日志。`install()` 的三处失败（线程创建/窗口创建/图标资源加载）都要先 `PostThreadMessageW(WM_QUIT)` + 同一个 500 ms 上界 join 再返回 `Err`，不留裸线程。

## 5. 告警呈现

### 5.1 路由与具名步骤

`tray_ui` 增加**纯**状态机 `HiddenAlertRouter`（唯一 owner = 50 ms tick 的 `alert_present_tick`）：

```
alert_present_tick():                        // 每 50 ms tick 调一次；闭包没有 Window
  gen = alerts.generation()
  if gen == last_gen && !armed: if flash 超时 → tray.set_alert(false) + set_flash_started(None); return
  latest = alerts.latest()                   // Option<AlertSummary{source, level, title, detail}>，只读、不 pop
  env = { visible: !hidden, dialog_open: self.dialog_open, pending: alerts.pending_len(),
          animations: tray::animations_enabled(), gate: tray::notification_gate(),
          critical: latest.map_or(false, |l| l.source != AlertSource::AgentNotice) }   // 只在评估时查系统状态
  match router.step(gen, env, now_ms):        // Present.count = env.pending；step 内部清除 armed
    None                         → 不推进 last_gen（被 Dialog 挡住 / 无可呈现）
    Present{presentation, count} → 推进 last_gen；InAppDialog: 无动作（render_root 下一帧出队）
        BalloonAndFlash / BalloonOnly:
            notify(trayAlertTitle{count}, latest.detail（≤200 截断，空则 title）, level_of(latest.level))
            [BalloonAndFlash 再] set_alert(true) + set_flash_started(Some(now))
        FlashOnly: set_alert(true) + set_flash_started(Some(now))    QueueOnly: 无动作
    StopFlash                    → set_alert(false) + set_flash_started(None)
```

**`dialog_open` 的来源与被挡住的语义**：`has_active_dialog` 必须 `&mut Window`（`gpui-component-0.9.1/src/window_ext.rs:109`），而 50 ms tick 闭包只有 `|view, _cx|`（`workspace.rs:1803-1813`）——因此由 `render_root` 每帧把 `window.has_active_dialog(cx)` 写进 Workbench 字段 `dialog_open`，tick 读字段（**允许一帧延迟**，已拍板；**不在 tick 里嵌套 window update**）。可见态且 `dialog_open` 时 `step` 返回 `None` 且**不推进** `last_gen`，否则告警在可见期被消费掉、藏到托盘后既不弹气球也不闪图标；`arm()` 在进入隐藏态时调用，使下一 tick **无条件评估一次**。

**气球数据通道（P4）**：`AlertItem` 增加 `level: AlertLevel`（`Info|Warning|Error`，由 `report_at_for` 的 `level` 参数写入），`AlertCenter::latest() -> Option<AlertSummary>` 返回 `{ source, level, title, detail }`（只读、不 pop）。`critical` 用 `latest.source`（非 `agentNotice` 即设备级）、NIIF 等级用 `latest.level`——`notify_user` 的 `level` 因此不再被丢弃。`HiddenAlertRouter` 只持 `last_generation`、`flash_started_ms`、`armed`，无 I/O。新 i18n：`trayAlertTitle`（"duthub 需要处理 {count} 条告警"）、`trayShowMain`/`trayQuit`/`trayTooltip`、`relaySource`、`agentNoticeSource`、`agentNoticeTitle`、`trayUnavailable`、`trayIconFailed`、`toastOmitted`。两个 locale 必须同键集（`agent_host.rs:249-262`、`i18n.rs:154-172`）。

### 5.2 放大警告

`render_alert_dialog_body`（`workspace.rs:7320-7449`）：`icon("alert").size(px(25.))` → 独立首行 `px(44.)`（`p.red`），来源 chip 与相对时间同排；标题/正文/按钮排顺序与文案**逐字不变**；浮层按钮继续手绘 div（坑 #2）。

### 5.3 队列：按来源隔离 + 上界 + 抑制路由

告警弹窗中点击「知道了」会一次确认当前弹窗和点击时已排队的所有告警。之后新到的告警继续按既有规则提示。右上角关闭按钮只关闭当前一条，「忽略告警」继续按来源抑制。

- `MAX_PENDING = 64`、`MAX_PENDING_PER_SOURCE = 16`：满时**只在同来源内**丢最旧；同来源也满则**拒收**。`AgentNotice` 永远不能驱逐 `Heartbeat`/`Control`/`Serial`/`Relay`/`Flash`/`Agent`。
- **不改既有签名**：`report_at`/`report_at_for`/`report_for` **全部保持 `-> bool`**（既有测试的 `assert!` 与 4 处裸语句调用原样通过，避免 `unused_must_use` 打红 clippy）。新增 `report_checked_at_for`/`report_checked_for` → `Result<(), ReportRejection>`，`ReportRejection { Disabled, Suppressed, Duplicate, Capacity }`（`Disabled` = 该 DUT 的 `alertsByDut.<id>.enabled=false`；**全局总开关已于 2026-11-25 从 `settings.alerts` 移除**，非 DUT 主体只能按来源抑制）；旧的 bool 版一律 `.is_ok()` 委托；**通知器调用 checked 变体**才能拿到 `reason`。
- **enabled 的判定要可查（P6）**：`enabled_for(dut) -> bool`：`by_dut` 命中看该条 `enabled`，否则恒 `true`（没有全局开关）。判定顺序固定为 `enabled_for → is_suppressed_for → 30s 去重 → 容量`，因此「DUT 告警关闭」不会落成 `suppressed`。
- `set_merge_duplicates(bool)` **仅隐藏态**生效：同 `(dut_id, source)` 已有待呈现条目则保留最早 `at`、刷新 `detail`、`repeat += 1`。可见态恒 `false`，故 `report_accepts_then_dedups_within_window`（`alerts.rs:385-409`）逐字不变；30 s 去重（`alerts.rs:22,241-247`）不动。新增只读：`generation()`、`latest()`、`enabled_for`、`is_suppressed_for`、`pending_len()`；`suppress`/`suppressed_keys`/`reset_suppressed`/`is_suppressed` 从 `#[cfg(test)]` 提升为正式 API（`resume` 保留）。
- **「忽略此类」的两条路径**：`AlertItem.dut_id` 是**已保存 DUT** → 既有 `edit_dut_alerts`（per-DUT `alertsByDut.suppressedSources`）；非 DUT 来源（继电器板 `board_id`、通用监听器省略 `dutId` 的 `agentNotice`，此时 `dut_id = ""`）→ **全局** `suppress_alert_source_globally(source)`：写 `settings.alerts.suppressed_sources` + `save_settings` + `AlertCenter::suppress(source)`。`AlertCenter::allows` 对 `dut_id == ""` 天然走全局分支（`by_dut.get("") = None`）。没有这条，`edit_dut_alerts` 会对未知 id 直接 return（`workspace.rs:3129-3131`），按钮变空操作。
- 非 DUT 来源的忽略记录保存在 `settings.json` 的 `alerts.suppressedSources`。
- **toast 队列也上界**：`pending_toasts`（`workspace.rs:1136`，只在 `render_root:7288-7290` 消费）与 `agent_host_toasts`（`workspace.rs:985`，`agent_host_tick:6421-6427` 转投前者）隐藏期只涨不消 → 两者 `MAX_TOASTS = 200`（丢最旧并计数，恢复后首条用 `toastOmitted` 报「已省略 N 条」）。
- **来源**：新增 `Relay`（key `"relay"`，label 键 `relaySource`）与 `AgentNotice`（key `"agentNotice"`，label 键 `agentNoticeSource`），同步 `key`/`from_key`/`label` 三处 + 设置页来源数组（`pages.rs:3574-3580`）+ 两个 locale 各一个 label 键 + **`alerts.rs` 两个既有枚举测试扩列**（`source_keys_roundtrip` `:496-506`、`master_switch_silences_all_sources` `:450-469`，4 → 6）。`AlertsHint`（`zh-CN.json:294`）不改，代价见 §11.6。

## 6. 继电器掉线监测

### 6.1 语义与巡检集合

按**板卡**（`duts.json.relayBoards`，`model.rs:607-621`）监测：共享设备、**不依赖 DUT 会话**、只发只读 STATUS、不签发审批、不取租约、不写 `[mark]`、不把 DUT 送进 `NeedsInspection`（坑 #40）。**可巡检集合 = 三个条件同时成立**：① `board.driver` 为空串或 `eq_ignore_ascii_case("CH340")`（`pages.rs:688-695` 新建即 `"CH340"`；`Ch340RelayDriver::channel_status` 不看 `driver` 字段，必须自己拦 PDU）；② **`board.node.trim()` 非空**（新建板默认 `node: ""`，`pages.rs:692`，而 `relay_node` 明确拒绝空节点，`control.rs:567-571`——不排除就会让未选串口的板卡每 30 s 报掉线）；③ 至少一个已保存 DUT 通过 `RelayConfig::UsbBoard{board_id, channels}` 绑定它。不满足的板卡**不探、不告警、不占端口**。

### 6.2 探针与分类

探针 = `ControlService::probe_board_status(node, channel)`：`relay_writes.try_read()`（按住测试持 `try_write` 时立刻 `Gated`，不等待、不让用户吃 `Busy`）+ **单通道** `ControlBackend::probe_channel(node, channel)`（新默认方法；`Ch340RelayDriver` 实现为一次 STATUS，`read_status` 在 `ECHO_TIMEOUT` 300 ms 后返回 `Ok(None)`，`driver.rs:534-559`）。**不再用 `channel_status`**（`driver.rs:351-364`，8 通道 ≈2.4 s），把持锁时间压到 ≈0.3 s。

| 探针结果 | 判定 | 理由 |
|---|---|---|
| `Answered` | `Online` | 板卡应答 STATUS |
| `Silent` | `Offline`（连续 `confirmations` 次） | **端口在位但 MCU 不应答** = 最常见故障（适配器还在、板子挂了），必须告警；首启也告警 |
| `PortMissing` | `Offline` | NotFound/Disconnected/Timeout（`driver.rs:577-599`、`:501-566`） |
| `PortBusy` / `Unsupported` | `Unobservable` | 端口被别的进程占用（`driver.rs:581-589`）；未配节点、无后端（`workspace.rs:1473-1477`）、PDU；**`DeviceError::Invalid`（`InvalidInput`，`driver.rs:594-600`）也归这里** |
| `Gated` | `Unobservable` | **只表示按住测试**占着写门（`control.rs:406-415`）；探针不取租约 |

**通道来源与刷新**：`ControlRelayProbe::set_channels(&[(board_id, channel)])` 由 50 ms tick 的 `sync_relay_watch()` 直接对齐 `self.duts.file`（比较一次 `Vec<(id, node, channel)>` 签名即可，**幂等且廉价**；不枚举 `save_duts`/`save_duts_atomic` 的十余个持久化点）。通道取该板**首个被任何 DUT 绑定的通道**。`RelayWatchService` 与 probe 的内部锁**绝不跨 I/O 持有**：`tick_at` 先取到期板卡快照并释放锁 → 探测 → 再拿锁回填；`sync_boards` 只做集合对齐、立即返回，因此 UI 线程保存连接时不会被一次掉线探测卡住约 30 s。

### 6.3 状态机、执行体与代价

每板卡 `{ state, consecutive_offline, last_probe_ms, last_emit_ms, next_due_ms }`：`Online` 清 `consecutive`，**由 `Offline` 转入时**发 `Recovered`（只 toast）；`Offline` 累加，达 `confirmations`（默认 2）发一次，此后每 `remind_interval_ms`（默认 300 s）复提；`Unobservable` 清 `consecutive`，由非 `Unobservable` 转入时发一次（只 toast），`Offline` 板探到 `Unobservable` **不**发 `Recovered`；默认 `interval_ms = 30_000`；新板卡立即到期。执行体：`Workbench::new` 起独立 `std::thread`（放在**自己的** `if !self_test` 块内，避坑 #79）：`loop { sleep 5s; 若 quit_flag 退出; watch.tick_at(now); for ev in watch.drain_events() { tx.send(ev) } }`。**该线程只探测与发事件**：`std::sync::mpsc::Sender<RelayBoardEvent>` 交回 UI，50 ms tick 里 `rx.try_recv()` 落库——`Offline` → `AlertCenter`（来源 `Relay`，`dut_id = board_id`），`Recovered`/`Unobservable` → toast。**禁止工作线程直接碰 `AlertCenter`/`pending_toasts`**。`tick_at` 同步阻塞（单通道最坏 ≈0.3 s，端口 task 卡死 30 s），**禁止**从 50 ms tick 或渲染路径调用。

**代价与冲突**：巡检持 `relay_writes.try_read()`、按住测试持 `try_write()`：互斥不排队，遇按住直接跳过（`Gated`）。反向代价：健康板每 30 s 一次 ≈0.3 s 的门占用；掉线板单次最多 30 s。`ControlService` 的 runtime 是 `current_thread`（`control.rs:132-135`），巡检 `block_on` 与 GUI 动作共用同一 driver 串行队列。`ensure_task`（`driver.rs:198-217`）的端口 task 会**长期持有继电器 COM 口**：巡检把占用从「用过才开」变成「首次探测后长期独占」（§11.5）。

## 7. MCP notify 工具契约

```
notify_user(message: string, title?: string, level?: "info"|"warning"|"error", dutId?: string)
```
`message` 必填：去控制字符、按**字符**截断 200；空串 → `invalid_params`。`title` 截 48，缺省 `agentNoticeTitle`。`level` **只接受三个合法值，其他任何字符串 → `invalid_params`（不静默降级，拍板 7）**。`dutId` 可选。参数结构 `#[serde(rename_all = "camelCase", deny_unknown_fields)]`。返回 `{"outcome":"queued|throttled|suppressed|disabled","queued":bool,"source":"agentNotice","reason"?:"dedup|capacity|rate"}`——**如实，绝不暗示「用户已看到」**。

**outcome 与 reason 的产出通道**：`queued` = 已进 AlertCenter 待呈现队列；`disabled` = **该 DUT 的告警开关关闭**（`!enabled_for`；全局总开关已移除，无通知器的端点不暴露该工具、不会产生这个 outcome）；`suppressed` = **仅指** DUT/来源抑制（`ReportRejection::Suppressed`）；`throttled` = 30 s 去重（`dedup`）、同来源容量满（`capacity`）、令牌桶空（`rate`）。判定顺序固定 `enabled → suppressed → 去重 → 容量 → 限流`，所以 DUT 告警关闭时不会落成 `suppressed`。`reason` 从 `NotifyOutcome::Throttled(ThrottleReason)` 带出（§8），否则三种 throttled 实现上无法区分；A2A 证据端根本到不了这个工具（`call_tool` 的 `evidence_scope` 分支返回 `EVIDENCE_SCOPE_ONLY`，`server.rs:1720-1722`、`:1589-1594`）。

**授权、注入与限流**：**授权**：不是设备写动作 → 无审批令牌，不进 `approval_request` 的 allowed 清单。按「是否有明确 DUT」分派，**不能只用 `authorize_scoped_read`**（它在通用监听器上转 `authorize_any_read`，不校验显式 `dutId`）：
`match scoped_dut(args.dut_id) { Some(dut) => authorize_read(principal, dut) /* 专用监听器绑 DUT 校验 + 通用监听器按该 DUT 校 grant，跨 DUT → UNAUTHORIZED */, None => authorize_any_read(principal) /* 零 grant 拒绝 */ }`。
**注入**：`McpServerCore` 收 `Option<Arc<dyn UserNotifier>>`，`spawn_mcp_runtime` 新增参数（`state: Arc<dyn McpState>` 不能 downcast，Workbench 必须同时保留 `Arc<RuntimeMcpState>`）。**限流**在 `RuntimeMcpState::notify`（A2）：每凭据 3 次/分钟（突发 2）+ 全局 10 次/分钟（突发 4），键取凭据 ID，避免一个 Agent 饿死其他 Agent。**审计**沿用 `call_tool` 的 `tool.call`（`server.rs:1750-1770`），只记 tool/dut/duration，正文不进审计。

**工具面：会话 13 / 用户监听器 14（P3）**：R6 的口径修正为：**只覆盖用户配置的通用/DUT MCP 监听器**。桌面发起的所有 Agent 会话——内嵌 goose（`agent_host/host.rs:746`）与 claude/codex CLI profile（`agent_host/cli.rs:732`）——都走 `listener::issue`（`listener.rs:63`）→ 4 参 `spawn_http_listener_loopback`（`listener.rs:107`，内部传 `None`），因此它们根本没有 `notify_user`（调用回 `PLAN_MODE_ONLY`，不是运行期 `disabled`）。为保证「看到的工具面 = 能用的工具面」：

- `plan_tool_allowed` **保持 13 个**，`agent_host/cli.rs` 的 `PLAN_TOOLS` **不动**（仍 13），`plan_badges` 与 locale 的「13 个 Plan 工具（只读）」**不改**（`CAPTURE_STREAM_BACKENDS.md:963` 的「plan_badges/locale 文案不改」因此依然成立）——`agent_cli.rs:520` 的「listener 暴露 == `cli::PLAN_TOOLS`」因此仍然成立。
- `notify_user` 作为**第 14 条路由**存在于 `#[tool_router]`，但由**两个静态路由表**分流：`PLAN_ROUTER`（13，移除 notify 路由）与 `PLAN_ROUTER_WITH_NOTIFY`（14）。`ServerHandler` 的 `list_tools`/`get_tool`/`call_tool` **手写**（`#[tool_handler]` 只在 impl 未定义时生成这几个方法，`rmcp-macros-3.10.0/src/tool_handler.rs:64`），按 `self.notifier.is_some()` 选表；`notifier=None` 的端点连 `tools/list` 都看不到它，调用回 `PLAN_MODE_ONLY`。
- 用户配置的监听器（`spawn_mcp_runtime` 注入 notifier）看到 14 个工具，其中 `notify_user` 可用。**Plan 模式因此能唤起用户界面**，文档与 SECURITY 必须写明这一点并说明它受限流与抑制约束（拍板 1）。

**同步清单（P5）**：`docs/API_CONTRACT.md:47/55-70`、`docs/SECURITY.md` 第 3 条（「唯一例外 capture」→「两个例外：capture 与 notify_user，后者不触设备但能唤起界面」）、`docs/ARCHITECTURE.md:101-103`、**`docs/reviews/AGENT_INTEGRATION_BEST_PRACTICE.md:46/50/317/504`**、**`docs/guides/AGENT_INTEGRATION_HANDOFF.md:58/271`**、**`43c58dc:docs/GOOSE_AGENT_FEASIBILITY.md:32`**、`docs/design/CAPTURE_STREAM_BACKENDS.md:963`（确认「不改」仍成立）、`crates/duthub-mcp/src/server.rs:1771` 的 `get_info` instructions（补一句「notify_user 仅用户配置的 MCP 监听器可用，桌面 Agent 会话回 disabled」）、`crates/duthub-mcp/src/skill.rs` 工具表（注明仅用户监听器可用）。统一表述：**「13 个只读 Plan 工具 + 1 个通知工具（仅用户配置的 MCP 监听器可用）」**，不得写成「14 个 Plan 工具」让 Agent 会话以为它可用。

## 8. 冻结接口（实现照此，签名原文）

```rust
// apps/desktop/src/tray.rs （lib target：pub mod tray，#[cfg(windows)]；只在这里碰 windows crate 与 HWND）
pub enum TrayNotifyLevel { Info, Warning, Error }
pub enum TrayEvent { Show, Quit, IconLost }   // IconLost：运行期 NIM_ADD 失败（如 TaskbarCreated 重放），UI 必须恢复主窗
pub enum TrayError { ThreadSpawn(String), WindowCreate(String), IconAdd(String) }
pub struct TrayOptions { pub tooltip: String, pub show_label: String, pub quit_label: String }
/// 测试探针（#[doc(hidden)]）：期望态 + 一次 Shell_NotifyIconGetRect。
pub struct TrayProbe { pub hwnd: isize, pub taskbar_created_msg: u32, pub icon_rect: Option<[i32; 4]>, pub icon_visible: bool, pub main_visible: bool, pub alerting: bool, pub balloons_sent: u32 }
pub struct TrayHandle { /* 内含 main_hwnd、托盘线程句柄、期望态 */ }
pub fn install(window: &mut gpui_kit::Window, options: TrayOptions) -> Result<TrayHandle, TrayError>;   // 取 HWND + 建线程/隐藏窗口；不 NIM_ADD；A2 不碰 windows crate
pub fn animations_enabled() -> bool;        // SPI_GETCLIENTAREAANIMATION（读失败按 true）
pub fn notification_gate() -> crate::tray_ui::NotificationGate;   // 两者由 A1 提供、A2 只调用；类型定义在 tray_ui（纯逻辑，无 windows）
impl TrayHandle {
    /// ShowWindowAsync(main, SW_HIDE)；图标出现失败（NIM_ADD 为 FALSE）返回 false，调用方必须回滚。
    pub fn hide_main(&self) -> bool;
    /// ShowWindowAsync(main, SW_SHOW)（保持最大化/最小化状态，不用 SW_SHOWNORMAL）。
    pub fn show_main(&self) -> bool;
    pub fn main_visible(&self) -> bool;
    /// 加/删图标；true = 期望态已生效。
    pub fn set_icon_visible(&self, visible: bool) -> bool;
    pub fn set_tooltip(&self, text: &str);
    pub fn set_alert(&self, alerting: bool);
    pub fn notify(&self, title: &str, body: &str, level: TrayNotifyLevel);
    pub fn drain_events(&self) -> Vec<TrayEvent>;
    pub fn remove(&self);
    #[doc(hidden)] pub fn test_probe(&self) -> TrayProbe;
}
impl Drop for TrayHandle { fn drop(&mut self); }

// apps/desktop/src/tray_ui.rs （lib target：pub mod tray_ui；纯函数，无 gpui / 无 windows）
pub enum CloseDisposition { HideToTray, Quit }
pub fn close_disposition(tray_ready: bool, quitting: bool) -> CloseDisposition;
/// 关闭有序步骤（纯数据，顺序由单测锁定）：Hide=[ReleaseRelay, CloseFloat, HideWindow]；Quit=[ReleaseRelay, CloseFloat, DestroyMain]。
pub enum CloseStep { ReleaseRelay, CloseFloat, HideWindow, DestroyMain }
pub fn close_plan(disposition: CloseDisposition) -> &'static [CloseStep];
pub enum NotificationGate { Allowed, QuietTime, Blocked }   // 由 tray::notification_gate() 产出；tray_ui 保持无 windows 依赖
pub enum AlertPresentation { InAppDialog, BalloonAndFlash, BalloonOnly, FlashOnly, QueueOnly }
pub fn alert_presentation(window_visible: bool, animations_enabled: bool, gate: NotificationGate, critical: bool) -> AlertPresentation;
// Blocked 恒不发气球；QuietTime 仅 critical 发；critical 由 tick 取自 latest().source（非 agentNotice 即 true）。
pub const FLASH_INTERVAL_MS: u64 = 500;
pub const FLASH_MAX_MS: u64 = 30_000;
pub enum FlashStop { Restored, BalloonClicked, Timeout, Acknowledged, AnimationsDisabled }
pub fn flash_should_stop(started_ms: u64, now_ms: u64, reason: Option<FlashStop>) -> bool;
pub enum FlashFrame { Lit, Dark, Stop }
pub fn flash_frame(started_ms: u64, now_ms: u64) -> FlashFrame;
pub struct HiddenAlertEnv { pub visible: bool, pub dialog_open: bool, pub pending: u64, pub animations: bool, pub gate: NotificationGate, pub critical: bool }
pub enum HiddenAlertAction { None, Present { presentation: AlertPresentation, count: u64 }, StopFlash }
/// 隐藏态告警路由的纯状态机；唯一 owner 是 50 ms tick 的 alert_present_tick。
pub struct HiddenAlertRouter { /* last_generation, flash_started_ms, armed */ }
impl HiddenAlertRouter {
    pub fn new() -> Self;
    /// visible && dialog_open → None 且不推进 last_generation；armed 在本次评估后清除；count 取 env.pending。
    pub fn step(&mut self, generation: u64, env: HiddenAlertEnv, now_ms: u64) -> HiddenAlertAction;
    /// 进入隐藏态时调用：下一 tick 无条件评估一次。
    pub fn arm(&mut self);
    /// 由调用方在 set_alert(true) 时传 Some(now)、set_alert(false) 时传 None；step 用它判超时。
    pub fn set_flash_started(&mut self, now_ms: Option<u64>);
}

// crates/duthub-device/src/relay_watch.rs （默认特性即编译，不进 real-hardware 门）
// 依赖：duthub_core::DeviceError、duthub_core::model::RelayBoard、std::sync::Arc
pub enum RelayBoardState { Online, Offline, Recovered, Unobservable }
pub struct RelayBoardEvent { pub board_id: String, pub name: String, pub state: RelayBoardState, pub detail: String, pub at_unix_ms: u64 }
pub enum RelayProbeSample { Answered(bool), Silent, PortMissing(String), PortBusy(String), Gated(String), Unsupported(String) }
pub trait RelayBoardProbe: Send + Sync { fn probe_board(&self, board: &RelayBoard) -> RelayProbeSample; }
pub struct RelayWatchConfig { pub interval_ms: u64, pub confirmations: u32, pub remind_interval_ms: u64 }   // impl Default
pub enum RelayProbeVerdict { Online, Offline, Unobservable }
pub fn classify_probe(sample: &RelayProbeSample) -> RelayProbeVerdict;
/// 内部锁绝不跨 I/O：tick_at 取到期快照后释放锁再探测，回来再回填。
pub struct RelayWatchService { /* ... */ }
impl RelayWatchService {
    pub fn new(probe: Arc<dyn RelayBoardProbe>) -> Self;
    pub fn with_config(self, config: RelayWatchConfig) -> Self;
    /// 只接「可巡检集合」：调用方（50 ms tick）已按 DUT 绑定预过滤，本函数逐项再校验 driver 白名单与 node 非空。
    pub fn sync_boards(&self, boards: &[RelayBoard]);
    pub fn tick_at(&self, now_ms: u64) -> bool;
    pub fn drain_events(&self) -> Vec<RelayBoardEvent>;
    pub fn states(&self) -> Vec<(String, RelayBoardState)>;   // 诊断/UI 用
}

// crates/duthub-device/src/control.rs（仅追加，不改既有方法）
impl ControlService {
    /// 单通道只读探针：持 relay_writes 读门 → 后端 probe_channel；失败分类见 RelayProbeSample。
    pub fn probe_board_status(&self, node: &str, channel: u8) -> RelayProbeSample;
}
pub struct ControlRelayProbe { /* control: Arc<ControlService>, channels: Mutex<HashMap<String, u8>> */ }
impl ControlRelayProbe {
    pub fn new(control: Arc<ControlService>) -> Self;
    /// board_id → 要探的通道（50 ms tick 的 sync_relay_watch 对齐 self.duts.file 后刷新）。
    pub fn set_channels(&self, channels: &[(String, u8)]);
    /* impl RelayBoardProbe: fn probe_board(&self, board: &RelayBoard) -> RelayProbeSample */
}

// crates/duthub-core/src/backend.rs（追加到 ControlBackend；默认实现 → PDU/测试 mock 行为不变）
pub trait ControlBackend { /* 既有方法不变 */
    async fn probe_channel(&self, _address: &str, _channel: u8) -> DeviceResult<Option<bool>> { Err(DeviceError::Unsupported) }
}
// crates/duthub-control/src/driver.rs：Ch340RelayDriver 的 probe_channel = 一次 STATUS（复用 read_status 的 300 ms 超时语义）

// crates/duthub-mcp/src/notify.rs （新文件；pub mod notify）
// NotifyLevel 是领域类型、不参与 serde：入参用 Option<String> 再映射，非法值 → invalid_params。
pub enum NotifyLevel { Info, Warning, Error }
pub struct NotifyRequest { pub message: String, pub title: Option<String>, pub level: NotifyLevel, pub dut_id: Option<String>, pub credential_id: Option<String> /* 限流键；LocalAdmin/stdio 为 None */ }
pub enum NotifyOutcome { Queued, Throttled(ThrottleReason), Suppressed, Disabled }
pub enum ThrottleReason { Dedup, Capacity, Rate }
impl NotifyOutcome { pub fn as_str(self) -> &'static str; }      // Throttled(_) → "throttled"
impl ThrottleReason { pub fn as_str(self) -> &'static str; }     // "dedup" | "capacity" | "rate"pub trait UserNotifier: Send + Sync { fn notify(&self, request: &NotifyRequest) -> NotifyOutcome; }
pub const MESSAGE_MAX_CHARS: usize = 200;
pub const TITLE_MAX_CHARS: usize = 48;

// crates/duthub-mcp/src/server.rs（追加）
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NotifyUserArgs {
    pub message: String,
    #[serde(default)] pub title: Option<String>,
    #[serde(default)] pub level: Option<String>,   // "info" | "warning" | "error"，其他 → invalid_params
    #[serde(default)] pub dut_id: Option<String>,
}
impl McpServerCore { pub fn with_user_notifier(self, notifier: Option<Arc<dyn UserNotifier>>) -> Self; }
// notify_user 是 #[tool_router] 的第 14 条路由；PLAN_ROUTER（13，无 notify）与 PLAN_ROUTER_WITH_NOTIFY（14）两张静态表，
// list_tools/get_tool/call_tool 手写并按 self.notifier.is_some() 选表（宏只在未定义时生成这些方法）。

// crates/duthub-mcp/src/transport.rs（spawn_http_listener_on 加第 6 参；loopback / http_router_on_dut 保持原签名，内部传 None）
pub fn spawn_http_listener_on(listener: std::net::TcpListener, state: Arc<dyn McpState>, access: Option<Arc<AccessContext>>,
    listener_id: Option<Arc<str>>, listener_dut: Option<Arc<str>>, notifier: Option<Arc<dyn UserNotifier>>) -> Result<ListenerHandle, String>;
pub fn http_router_on_dut_with_notifier(state: Arc<dyn McpState>, access: Option<Arc<AccessContext>>,
    listener_id: Option<Arc<str>>, listener_dut: Option<Arc<str>>, notifier: Option<Arc<dyn UserNotifier>>) -> axum::Router;

// apps/desktop/src/mcp_runtime.rs（state 是 Arc<dyn McpState>，不能 downcast，Workbench 另留 Arc<RuntimeMcpState>）
pub fn spawn_mcp_runtime(state: Arc<dyn McpState>, notifier: Option<Arc<dyn duthub_mcp::notify::UserNotifier>>,
    access: Option<Arc<duthub_mcp::AccessContext>>, port: u16, lan_requested: bool,
    listeners: Vec<duthub_core::agent_access::AgentListener>, snapshot: Arc<Mutex<duthub_mcp::snapshot::StateSnapshot>>,
    data_dir: PathBuf, main_bound_endpoint: Arc<OnceLock<SocketAddr>>) -> McpRuntimeHandles;
impl duthub_mcp::notify::UserNotifier for RuntimeMcpState { fn notify(&self, request: &duthub_mcp::notify::NotifyRequest) -> duthub_mcp::notify::NotifyOutcome; }
```

## 9. 文件归属（同一文件只有一个 owner）

| owner | 文件 |
|---|---|
| **A1 托盘底层** | `apps/desktop/src/tray.rs`(新：HWND 获取、`hide_main`/`show_main`、`TrayBackend` 端口、`animations_enabled`/`notification_gate`、`TrayProbe`/`test_probe`、`IconLost`、路径派生 GUID)、`tray_ui.rs`(新：`CloseStep`/`close_plan`、`NotificationGate`、`AlertRoute`、`FlashPlan`、`HiddenAlertRouter`)、`tray_lifecycle.rs`(新：生命周期协调器)、`apps/desktop/src/lib.rs`(+3 `pub mod`)、`apps/desktop/Cargo.toml`（windows 特性 + `raw-window-handle`）、`apps/desktop/tests/tray_logic.rs`(新)、`tests/tray_lifecycle.rs`(新)、`tests/tray_architecture.rs`(新)、`tests/tray_win32.rs`(新, 含 `#[ignore]` 实机项)、`scripts/accept-tray.ps1`(新)。**未交付**：`build.rs` 资源 ID 2 与 `assets/tray-alert.ico` —— 当前告警帧用 `IDI_WARNING` 的私有副本兜底（见 §12.6） |
| **A2 桌面接线** | `apps/desktop/src/main.rs`（单实例互斥量）、`workspace.rs`（首次 render 调 `tray::install(window, …)`、`run_close_plan` 执行器、唯一的 float/main `window_closed` 观察者 body 改 defer、隐藏前 release_relay、退出时 `release_relay_and_join`、`dialog_open` 字段、`alert_present_tick`、`sync_relay_watch`、继电器事件 `try_recv`、快照 250 ms 节流、toast 上界、告警图标 44 px、`suppress_alert_source_globally`；**只调用** tray 的公开函数，不直接用 `windows` crate）、`alerts.rs`（`AlertLevel` + `AlertItem.level`、`AlertSummary`/`latest()`、`enabled_for`、`ReportRejection{Disabled,…}` + checked 变体（bool 版不变）、每来源配额、merge、generation、`suppress` 系列转正、6 来源）、`mcp_runtime.rs`（双桶限流 + `spawn_mcp_runtime` 新参 + `UserNotifier` impl）、`pages.rs`（来源数组）、`i18n.rs`、`locales/zh-CN.json`、`locales/en-US.json`、`apps/desktop/tests/tray_contract.rs`(新) |
| **B 继电器监测** | `crates/duthub-device/src/relay_watch.rs`(新)、`control.rs`（仅追加）、`lib.rs`(+`pub mod relay_watch;`)、`crates/duthub-device/tests/relay_watch.rs`(新)、`crates/duthub-core/src/backend.rs`（仅追加 `probe_channel` 默认方法）、`crates/duthub-control/src/driver.rs`（仅追加 `probe_channel` 实现） |
| **C 通知工具** | `crates/duthub-mcp/src/notify.rs`(新)、`server.rs`（两张静态路由表 + 手写 list_tools/get_tool/call_tool + `get_info` instructions）、`skill.rs`、`lib.rs`(+`pub mod notify;`)、`transport.rs`、`crates/duthub-mcp/tests/notify_tool.rs`(新)、`docs/API_CONTRACT.md`、`docs/SECURITY.md`、`docs/ARCHITECTURE.md`、`docs/reviews/AGENT_INTEGRATION_BEST_PRACTICE.md`、`docs/guides/AGENT_INTEGRATION_HANDOFF.md`、`43c58dc:docs/GOOSE_AGENT_FEASIBILITY.md`、`docs/design/CAPTURE_STREAM_BACKENDS.md`(确认不改) |
| **D 设计文档** | `docs/design/TRAY_ALERT_DESIGN.md`（本文）、`AGENTS.md` 坑号回写 |

**无 owner，禁改**：`scripts/regression`、`crates/duthub-core/src/model.rs`/`store.rs`（不新增配置段）、`apps/desktop/src/agent_host/cli.rs`（PLAN_TOOLS 保持 13）、`apps/desktop/tests/plan_badges.rs`、`apps/desktop/tests/agent_cli.rs`、两个 locale 的 `agentToolSurface`（**13 不变**）、`crates/duthub-mcp/tests/dut_binding.rs`、既有 E2E 断言文本。

## 10. 测试与验收计划

### 10.1 纯逻辑单测（默认门禁 `cargo test --workspace`）

- `tests/tray_logic.rs`：`close_disposition`（托盘不可用 → `Quit`；`quitting` 恒 `Quit`）；**`close_plan` 顺序**（Hide=`[ReleaseRelay, CloseFloat, HideWindow]`、Quit=`[ReleaseRelay, CloseFloat, DestroyMain]`，`ReleaseRelay` 恒第一）；`alert_presentation` 在 `NotificationGate` × `animations` × `critical` 组合下的取值（Blocked 恒无气球、QuietTime 仅 critical 发）；`flash_frame` 边界；`flash_should_stop` 五类原因；`HiddenAlertRouter::step`（gen 不变且未 armed → `None`；上涨+隐藏+通知允许 → `Present(BalloonAndFlash)`；动画关 → `Present(BalloonOnly)`；可见无 Dialog → `Present(InAppDialog)`；**可见且 `dialog_open` → `None` 且不消费 generation，随后 `arm()`+隐藏 → 仍 `Present`**；**`Present.count == env.pending`**；armed 在评估后清除；`flash_started` 超 `FLASH_MAX_MS` → `StopFlash`）；常量 `FLASH_MAX_MS <= 30_000`。`tests/tray_lifecycle.rs` 另覆盖 `LifecycleEvent → LifecycleAction`（含 quit 顺序与 `AlertArrived`）、`FlashPlan`、`AlertRoute`、`balloon_notification` 的 quiet-time 口径、`flash_stops_for_animation_change` 与 `RecordingTray` mock 适配器。
- `crates/duthub-device/tests/relay_watch.rs`（mock probe + 注入时钟，**不开真机特性**）：`Answered` → `Online` 零事件；连续 `Silent` × confirmations → **恰 1 个 `Offline`**（含首探即 Silent 的冷启动坏板）；`PortMissing` × confirmations → `Offline`；持续 `PortBusy` → 仅 1 个 `Unobservable`、**永不** `Offline`；`Gated` 期间 0 个 `Offline` 且 `consecutive` 清零；`PortMissing`/`Silent` 后 `Answered` → 1 个 `Recovered`；未进入 `sync_boards` 的板卡零事件零探测；到期才复提；多板卡互不影响；**`sync_boards` 在探测进行中调用必须立即返回**（probe 里 sleep 200 ms，主线程调 `sync_boards` 的耗时应 <20 ms）。同文件再加 `ControlService` 侧（mock `ControlBackend`）：`probe_board_status` 失败后 `lease.status(dut).phase == Free`、无 `protected_op`、无 `[mark]` 落盘；`hold_channel` 持锁期间探针返回 `Gated`。
- `crates/duthub-mcp/tests/notify_tool.rs`（`http_router_on_dut_with_notifier` + scripted `UserNotifier`）：**注入 notifier 时 `tools/list` 含 14 个工具且含 `notify_user`；未注入（`http_router_on_dut` 原 4 参路径）时仍是 13 个、列表里没有 `notify_user`、调用回 `PLAN_MODE_ONLY`**（P3）；零 grant → `UNAUTHORIZED`；专用监听器省略 `dutId` → 绑定该 DUT 且 `Queued`；**通用监听器显式传越权 `dutId` → `UNAUTHORIZED`**（走 `authorize_read`）；超长/含控制字符 → 截断（≤200/≤48）；空 `message` → `invalid_params`；**`level:"critical"` → `invalid_params`**；四种 `outcome` 逐字对应，**`NotifyOutcome::Throttled(Dedup|Capacity|Rate)` 分别映射成 `reason:"dedup"|"capacity"|"rate"`**，**该 DUT 的告警开关关闭 → `disabled` 而不是 `suppressed`**；证据端 `EVIDENCE_SCOPE_ONLY`。
- `alerts.rs`（bin 单测）追加：`report_at`/`report_at_for`/`report_for` 仍返回 bool 且既有断言与 4 处裸语句调用原样通过；`report_checked_at_for` 分别产出 `Disabled`/`Suppressed`/`Duplicate`/`Capacity`（**`alertsByDut` 关闭 → `Disabled`**）；`enabled_for("")` 恒 true（无全局开关）与 `enabled_for(dut)` 反映 per-DUT 开关；`latest()` 返回 `source/level/title/detail` 且不改变 `pending_len()`；**`AgentNotice` 不得挤掉 `Heartbeat`**（16 条填满后再报 Heartbeat 仍入队；反向得 `Capacity`）；`MAX_PENDING` 只在同来源内丢最旧；隐藏态合并（`pending_len()==1`、`repeat==1`）；可见态不合并；全局抑制的读写与重置生效；既有两个枚举测试扩到 6 来源。
- `mcp_runtime.rs` 限流单测（A2）：每凭据桶 3/min 突发 2 与全局桶 10/min 突发 4 各自触发 `Throttled{Rate}`；凭据 A 打满不影响 B。
- `tests/tray_contract.rs`（源码契约，钉具体行、不用计数断言）：`render_root` 段含 `!self_test` 与 `tray::install(window`；`prepare_main_window_close` 只 `cx.defer(` + `return false`（`quitting` 除外），且不出现 `handle_monitor_event`/`close_float_window`/`remove_window`；**`run_close_plan` 存在且是唯一执行器，内部按 `close_plan(` 的结果分派**（不断言步骤相对 `weak.update` 的位置）；唯一的 `cx.on_window_closed` 订阅（float/main 两分支）body 以 `cx.defer(` 开头；`render_root` 段含 `self.dialog_open =` 与 `if !self.hidden`；`tray.hide_main(` 与 `tray.show_main(` 出现在 workspace.rs 且其中不出现 `ShowWindow`；`suppress_alert_source_globally` 与 `reset_global_alert_suppression` 存在且被对应分支调用；`self.refresh_snapshot();` 不在 `render_root` 段；`main.rs` 含 `CreateMutexW` 与 `--self-test` 跳过守卫；`begin_quit` 含 `release_relay_and_join`；`mcp_runtime.rs` 含 `impl duthub_mcp::notify::UserNotifier for RuntimeMcpState`；`agent_host/cli.rs` 的 `PLAN_TOOLS` 仍为 13 项且不含 `"notify_user"`。

### 10.2 托盘集成测试（`#[ignore]`，实机显式跑）

`tests/tray_win32.rs`（`#[ignore]` 项前置：必须有交互式桌面会话）：① 安装宿主并注册图标后，窗口可见且 `icon_rect.is_some()`。`hide_main()` 成功后窗口隐藏，图标仍可达；② `show_main()` 后窗口可见，图标仍可达（任务栏按钮和托盘图标共存、最大化窗口恢复后仍最大化，由人工确认）；③ 向 `test_probe().hwnd` 投 `taskbar_created_msg` 后 `icon_rect` 仍可达；④ `set_alert(true/false)` 可停，等到 `FLASH_MAX_MS` 自动停止；⑤ `remove()` 后 `icon_rect.is_none()` 且 `Drop` 二调不 panic；⑥ `animations_enabled()`/`notification_gate()` 可调用不 panic。该文件另含菜单映射/门查询/「图标未建立时命令无副作用」等无 shell 项。`HiddenAlertRouter` + 真实 `TrayHandle` 的联动用例（原 `tray_alert_route.rs`）**本轮未单独落地**：纯路由由 `tests/tray_logic.rs` 覆盖，真实气球由 `icon_lifecycle_against_a_real_shell` 手工调用覆盖；两者组合见 §12.6。

### 10.3 桌面定向测试与实机验收

- **浮窗 panic 定向证伪**：① 开浮窗 → X → 隐藏 → 恢复 → 浮窗按钮仍可用；② 开浮窗 → X（用测试开关强制 `tray_failed`，走 Quit 分支）→ **进程真的退出**；③ 开浮窗 → 托盘菜单「退出」→ 进程 5 s 内退出、无孤儿窗口。`accept-tray.ps1` 增加三步并断言无 panic 日志。
- **按住测试安全**：按住某通道期间点 X → 隐藏；继电器日志出现 OFF（`[mark]` 含「隐藏到托盘：释放继电器按住测试」）、`STATUS` 回读为释放态；再退出一次同样断言（退出路径会 join worker）。
- `accept-tray.ps1` 判据：① 启动 `--workdir <tmp>`，`MainWindowHandle != 0` 且进程存活；② `PostMessageW(hwnd, WM_CLOSE)` → 3 s 后进程存活、`MainWindowHandle == 0`；③ 隐藏期每 5 s 采样 `duthub-data/runtime/mcp-state.json` 的 `ts_unix_ms` 必须递增（tick 存活判据）；④ 隐藏期主监听器 `tools/list` 仍 200 且 **14 个工具**；⑤ 隐藏期 `notify_user` → `queued`，`ShowWindow(SW_SHOW)` 恢复后 Dialog 弹出；⑥ 同一 exe 再启动 → 弹「已在运行」并退出；⑦ 托盘菜单退出 → 进程 5 s 内退出、无残留；⑧ 开浮窗再执行 ② 的两条退出路径。
- 人工清单（截图）：⚠ 图标尺寸、气球文案、右键两项、**最大化后隐藏再恢复仍最大化**、开「减少动画效果」后不闪、演示模式下不弹气球、隐藏 10 分钟后 CPU 占用与快照时间戳仍推进、隐藏后预览/采集的最终状态、隐藏/恢复各一次后 stderr 无 borrow 错误。
- **门禁顺序**：`cargo fmt --all` → `cargo clippy --workspace --all-targets -- -D warnings`（新 `unsafe` 必带 `SAFETY:`，注释英文）→ `cargo test --workspace` → `bash scripts/regression` → §10.2 `--ignored` → 上面的实机项；并行构建用隔离 target（坑 #10/#91）。

## 11. 风险与限制（如实）

1. **隐藏后 50ms tick 是否仍运行**：源码证据强（§3），本轮未实机；判据 = §10.3 ③。失败则把 `heartbeat_tick` 的探针调度迁到独立线程（单 owner，不双发）。
2. **`double_lease_panic` 与「最大化不丢」均为源码/官方语义推导，未经运行验证**：前者依据 `app.rs:3269-3292` + `entity_map.rs:188-195`，后者依据 `SW_SHOWNORMAL` 语义。代价：观察者 body 改 `cx.defer` 后 `on_float_window_closed`（含 `release_monitor`）延后一拍，隐藏后首帧 placement 可能仍是 `Floating`；验收见 §10.3 人工「隐藏后预览/采集最终状态」。
3. **溢出区不可控**：新图标默认进溢出区、应用无权提升；隐藏态提示靠气球（系统会为隐藏图标临时提升），不能承诺「用户一定看到图标」。气球一次一个、约 9 s、Win10 留通知中心 / Win11 不留；quiet time/演示模式/全屏/锁屏下可能被丢弃。
4. **GUID 身份按 exe 路径派生 + 单实例是进程级互斥量**：搬目录后图标偏好失效、新路径首次运行会重新进溢出区（与平台「GUID 注册绑定二进制路径」一致）；互斥量只拦同一 exe 的重复启动，不同目录的两份 exe 各有 GUID，会各占一个图标与端口。
5. **继电器巡检副作用**：端口 task 长期持有继电器 COM 口（`driver.rs:198-217`）；健康板每 30 s 一次 ≈0.3 s 读门占用，掉线板单次最多 30 s，期间 GUI 继电器动作排在 STATUS 之后（`ControlService` 是 `current_thread`，`control.rs:132-135`）；`PortBusy`/PDU/空 node/未绑定一律不告警。**不带 `/F` 的 `taskkill` 失效**（WM_CLOSE 被吞），发布/回归脚本用 `/F`。
6. **设置页说明未列新来源**（`AlertsHint` `zh-CN.json:294` 按「不改既有文案」保留）；非 DUT 来源的「忽略此类」写全局抑制表（§5.3），全局抑制值继续写入 settings.json 的 alerts.suppressedSources。
7. **R6 只覆盖用户配置的 MCP 监听器**（P3）：桌面发起的全部 Agent 会话（内嵌 goose 与 claude/codex CLI profile，均走 `listener::issue` → `spawn_http_listener_loopback`）不接 notifier，`notify_user` 对它们 `disabled` 且不出现在其 `tools/list` 里；后续给 `listener::issue` 加参数即可覆盖。
8. **`NIN_KEYSELECT` 需本地定义**；`LoadIconMetric` 的 HICON 由我们 `DestroyIcon`，与 gpui 的 `LR_SHARED` 严格分离。
9. **`probe_channel` 动到 `duthub-core` 的 `ControlBackend`**：以默认实现（`Err(Unsupported)`）追加，PDU/测试 mock 行为不变；实现只在 `Ch340RelayDriver`。

## 12. 模块边界与依赖方向（端口 / 适配器）

> 本章是实现的架构契约：以后改托盘、告警来源、通知通道或 MCP 工具时，先按本章判断「该改哪个文件、哪些文件不要动」。
> 源码契约测试 `apps/desktop/tests/tray_architecture.rs` 钉住其中的依赖方向：托盘/告警/lifecycle 面只有 `tray.rs` 链接 `windows` crate（整个 desktop crate 仍有 `agent_host/job.rs`，见 §12.6），并钉住关闭计划执行器、单实例端口与浮窗 placement 复位。

### 12.1 依赖方向

```text
apps/desktop/src/workspace.rs              ← 执行器（唯一持有 Window / Workbench / App）
   ├─ duthub_desktop::tray_lifecycle       ← 纯：事件 → 有序动作（无 gpui / windows / alerts）
   │     └─ duthub_desktop::tray_ui        ← 纯：close plan / presentation / AlertRoute / FlashPlan / router
   ├─ duthub_desktop::tray                 ← 端口 TrayBackend + 构造 install；托盘/告警面唯一 windows crate 使用者
   ├─ crate::alerts                        ← 窄接口：report_* / latest / generation / enabled_for /
   │                                          is_suppressed_for / pop_next（无 gpui / OS 类型）
   └─ crate::mcp_runtime                   ← 适配器：impl UserNotifier for RuntimeMcpState

crates/duthub-mcp/src/notify.rs            ← 端口 UserNotifier / NotifyRequest / NotifyOutcome
   └─ crates/duthub-mcp/src/server.rs      ← notify_user 路由 + 两张工具表（有无 notifier）

crates/duthub-device/src/relay_watch.rs    ← 端口 RelayBoardProbe + 纯状态机 RelayWatchService
   ├─ crates/duthub-device/src/control.rs  ← 适配器 ControlRelayProbe → ControlService::probe_board_status
   └─ workspace::drain_relay_events        ← 适配器 RelayBoardEvent → AlertCenter（Relay）/ toast
```

### 12.2 端口与适配器清单

| 端口（trait/纯类型） | 位置 | 生产适配器 | 测试替身 |
|---|---|---|---|
| `TrayBackend`（`hide_main`/`show_main`/`set_visible`/`set_tooltip`/`notify_balloon`/`start_flash`/`stop_flash`/`drain_events`/`remove`）+ `install` + `TrayNotification` | `apps/desktop/src/tray.rs` | `mod platform`（`cfg(windows)` 的 Win32 实现；`cfg(not(windows))` 的惰性实现返回 `install` 失败） | `tests/tray_lifecycle.rs::RecordingTray`、`tray_lifecycle.rs` 单测内的 `RecordingTray` |
| `close_disposition`/`close_plan`/`alert_presentation`/`alert_route`/`FlashPlan`/`HiddenAlertRouter` | `apps/desktop/src/tray_ui.rs` | 纯函数，无适配器 | `tests/tray_logic.rs`、`tests/tray_lifecycle.rs` |
| `LifecycleEvent` → `LifecycleAction`（`on_event`/`actions_for_close_plan`/`alert_actions`/`balloon_notification`/`run_tray_actions`） | `apps/desktop/src/tray_lifecycle.rs` | `workspace.rs`：`prepare_main_window_close`、`run_close_plan`/`run_lifecycle_action`、`begin_quit`、`drain_tray_events`、`alert_present_tick` | `tests/tray_lifecycle.rs` |
| `AlertCenter` 窄接口 | `apps/desktop/src/alerts.rs` | `workspace.rs`（弹窗/气球）、`mcp_runtime.rs`（notify_user） | `alerts.rs` 单测 |
| `UserNotifier` / `NotifyRequest` / `NotifyOutcome` | `crates/duthub-mcp/src/notify.rs` | `apps/desktop/src/mcp_runtime.rs::impl UserNotifier for RuntimeMcpState` | `crates/duthub-mcp/tests/notify_tool.rs` 的 scripted notifier |
| `RelayBoardProbe` / `RelayBoardEvent` / `RelayWatchService` | `crates/duthub-device/src/relay_watch.rs` | `crates/duthub-device/src/control.rs::ControlRelayProbe` | `crates/duthub-device/tests/relay_watch.rs` 的 scripted probe |

### 12.3 允许 / 禁止的依赖

| 模块 | 允许 | 禁止 |
|---|---|---|
| `tray.rs` | `windows`、`raw-window-handle`、`tray_ui`（`NotificationGate`/`FLASH_*`）、`std` | gpui 类型（`install` 只借用 `&mut gpui_kit::Window` 取 HWND）、alerts、workspace |
| `tray_ui.rs` | 纯 `std` | `gpui`、`windows` crate、`crate::tray`（文档链接除外）、`crate::workspace`、alerts |
| `tray_lifecycle.rs` | `tray_ui`、`tray::{TrayBackend, TrayNotification, TrayNotifyLevel}`（端口类型） | `gpui`、`windows` crate、`crate::alerts`、`crate::workspace` |
| `alerts.rs` | `std`、`duthub_core::model::{AlertSettings, GlobalAlertSettings}`、`crate::agent::RunStatus`（领域枚举，仅 `agent_transition_alert_texts` 用）、i18n 文案函数（只产出字符串） | `gpui`、`windows` crate、`crate::tray`、`crate::tray_lifecycle`、`crate::workspace` |
| `workspace.rs` 等其余 desktop 模块 | `tray` 的公开端口、`tray_ui`、`tray_lifecycle`、`alerts` | 直接 `windows::` 调用 / `ShowWindow` / `Shell_NotifyIconW`。**唯一历史例外**：`agent_host/job.rs`（应用级 Job Object，见 §12.6），不属于托盘/告警面 |
| `duthub-mcp` | `duthub-core`、`notify` 端口、rmcp/axum | `duthub-desktop`、`gpui`、tray/alerts 类型 |
| `duthub-device::relay_watch` | `duthub-core` 的 `DeviceError`/`model`、注入的时钟与 `Arc<dyn RelayBoardProbe>` | `AlertCenter`、`gpui`、`duthub_desktop`、`pending_toasts`（事件只回 UI 线程落库） |

### 12.4 常见改动的 recipe

**A. 新增告警来源**（例如「电源异常」「温度过高」）

1. `apps/desktop/src/alerts.rs`：加一个 `AlertSource` 变体，并补齐 `key`/`from_key`/`label` 三处（`key` 同时是 settings 抑制列表的稳定键）。
2. `apps/desktop/locales/zh-CN.json` 与 `en-US.json`：各加一个 label 键（两边键集保持一致）。
3. `apps/desktop/src/pages.rs` 的来源数组加新来源（设置页抑制列表要能看见它）。
4. 生产方调用 `AlertCenter::report_checked_for(dut_id, source, level, title, detail)`；DUT 级用稳定 DUT ID，非 DUT 主体（继电器板等）用 `board_id` 或 `""`——「忽略此类」会自动落到 per-DUT 或全局抑制表（§5.3）。
5. **不需要改**：`tray.rs`、`tray_ui.rs`、`tray_lifecycle.rs`、关闭/退出逻辑、`mcp_runtime.rs`。托盘只消费 `AlertPresentation`/`AlertLevel`，不认来源。

**B. 换通知通道**（例如以后上 Windows App SDK toast）

1. 在 `apps/desktop/src/tray.rs` 的 Win32 `mod platform` 里重新实现 `TrayBackend::notify_balloon`（或新增一个同形状的通道端口），保留其余方法名/语义与 `TrayNotification`（`respect_quiet_time` 的 critical 语义）不变。
2. `tray_lifecycle.rs`：`balloon_notification` 的 quiet-time 口径与 `LifecycleAction` 不变；`run_tray_actions` 只负责无数据的托盘动作，`PresentAlert` 的正文由 workspace 从 `AlertCenter::latest()` 取出后调用端口。
3. `workspace.rs::present_hidden_alert` **不需要改**：它构造 `TrayNotification` 并调用端口方法；`AlertPresentation`/`AlertRoute` 的判定（含通知门槛 `NotificationGate`、动画开关、静默时段）保持不变。
4. **不需要改**：`alerts.rs`、`tray_ui.rs` 的状态机规则、`tray_lifecycle` 的事件规则、关闭/退出、`duthub-mcp::notify`。新通道如果引入 WinRT 依赖，只加在 `tray.rs` 所在的平台依赖里。

**C. 换托盘实现 / 跨平台**

1. 只改 `apps/desktop/src/tray.rs` 的两个 `mod platform`（`cfg(windows)` 与 `cfg(not(windows))`），保持 `TrayBackend` 的九个方法名/语义、`TrayEvent`、`TrayNotification`、`TrayOptions`、`TrayError` 不变；`install` 的失败语义（`Err` → `close_disposition(false)` → 真退出）不变。
2. 新平台必须满足同一条不变量：**先确认图标存在，才允许隐藏主窗口**（`hide_main` 返回 `false` 时调用方保持可见并 toast）。
3. `apps/desktop/tests/tray_architecture.rs` 的 `ALLOWED` 列表是「允许链接 `windows` crate 的文件」的白名单；换实现时同步它（并保持 `tray.rs` 仍在列表里）。
4. **不需要改**：`tray_ui.rs`、`tray_lifecycle.rs`、`workspace.rs` 的关闭/隐藏/恢复流程、`alerts.rs`。

**D. 新增 MCP 工具**

1. `crates/duthub-mcp/src/server.rs`：加 `#[tool]` 方法 + 静态工具表（`PLAN_ROUTER` / `PLAN_ROUTER_WITH_NOTIFY` 由 `self.notifier.is_some()` 选择；Plan 只读工具还要同步 `agent_host/cli.rs` 的 `PLAN_TOOLS` 与 `plan_badges`/locale 的计数）。
2. `crates/duthub-mcp/src/skill.rs` 的工具表与 `docs/API_CONTRACT.md` 的工具清单/失败码三处同步（坑 #13）。
3. 如果工具需要宿主能力（例如提醒用户）：在 `crates/duthub-mcp` 定义 trait（照 `notify::UserNotifier`），**只在** `apps/desktop/src/mcp_runtime.rs` 实现；`duthub-mcp` 保持不依赖 `duthub-desktop`/`gpui`。
4. 如果工具会产生告警：优先复用 `AlertSource::AgentNotice`；只有语义确实不同才走 recipe A。
5. **不需要改**：`tray.rs`、`tray_ui.rs`、`tray_lifecycle.rs`、关闭/退出逻辑、`alerts.rs`（除非确实新增来源）。

### 12.5 本轮实现后的职责迁移

- `workspace.rs` 不再自己决定「关闭该隐藏还是退出」之外的任何顺序：`CloseStep/close_plan` 仍是冻结的窗口处置契约，但每个步骤通过 `tray_lifecycle::actions_for_close_plan` 展开成 `LifecycleAction` 并由 `run_lifecycle_action` 执行；托盘菜单退出、显示、图标丢失都先经过 `tray_lifecycle::on_event`。
- `main.rs` 不再链接 `windows` crate：单实例互斥量/「已在运行」提示移到 `tray::single_instance`；`--self-test` 跳过守卫仍在调用点（§2.1 的守卫口径）。
- 隐藏态告警的通道选择收敛为 `alert_presentation → alert_route → alert_actions` 三个纯函数/纯类型；`workspace::alert_present_tick` 只负责取数据与派发动作。
- `alerts.rs` 的公开面按 §12.1 收敛；`AlertItem.level`/`checked` 变体仍是 MCP 回执的真实来源。

### 12.6 已知残留耦合（如实）

1. **`agent_host/job.rs` 仍是第二个 `windows` crate 使用者**（应用级 Job Object，来自采集孤儿进程修复轮，不属于本轮托盘/告警面）。`tests/tray_architecture.rs` 明确把它列入白名单；把它也收进平台端口是后续独立任务。§12.3 的「唯一」只对托盘/告警/lifecycle 面成立。
2. **`tests/tray_contract.rs::main_guards_a_single_instance` 逐字钉住 `main.rs` 含 `CreateMutexW`/`ERROR_ALREADY_EXISTS`/`MessageBoxW`**：实际调用已移到 `tray::single_instance`，这些字面量只保留在 `main.rs` 的文档注释里以保持既有测试原样通过（不改测试的代价）。为免契约空转，`tests/tray_architecture.rs::the_single_instance_guard_runs_through_the_tray_port` 另行钉住 `main` 的端口调用与 `tray.rs` 的 Win32 实现。
3. **`tray_ui::close_plan` 仍是三步**（`tests/tray_logic.rs::close_plan_order_is_locked` 锁死 `[ReleaseRelay, CloseFloat, HideWindow/DestroyMain]`）；`StopPreview` 由 `tray_lifecycle` 只在 Quit 路径追加。把 `StopPreview` 加进隐藏路径会改变隐藏态采集/预览行为（R5），需要产品另行拍板。
4. **`alerts.rs` 仍持有来源标签与文案函数**（`AlertSource::label`、`heartbeat_alert_texts` 等）："无 UI 类型" 的口径是「不依赖 gpui/OS 类型、只产出字符串」，文案键仍随 `alerts.rs` 走，因为既有单测逐字断言中文文案。`agent_transition_alert_texts` 另依赖 `crate::agent::RunStatus`（领域枚举）；`agent` 模块本身是 UI 重的，所以模块图上有一条 `alerts → agent` 边，但 `alerts.rs` 自身不出现 gpui/OS 类型。
5. **告警图标资源未交付**：`RESOURCE_ALERT_ICON = 2` 之前没有对应的 `assets/tray-alert.ico`，`build.rs` 也只有 `set_icon`（ID 1）。当前告警帧改用 `IDI_WARNING` 的私有副本（`CopyIcon`）闪烁、拿不到时退化为纯气球——不伪造「用普通图标当告警帧」。补齐资源与 `set_icon_with_id` 是独立交付项。
6. **`HiddenAlertRouter` + 真实 `TrayHandle` 的联动没有独立用例**（原计划的 `tests/tray_alert_route.rs` 未落地）：纯路由在 `tests/tray_logic.rs`、真实气球在 `tests/tray_win32.rs` 的 `#[ignore]` 用例各自覆盖，组合路径尚未被自动化钉住。
