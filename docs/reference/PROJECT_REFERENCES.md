# 一手开源项目与官方资料

以下为 2026-11-12 检索验证的项目链接；使用前仍应以具体发布版本的 README、许可证和兼容性说明为准。

| 领域 | 项目 / 文档 | 用途 |
|---|---|---|
| 界面及交互 | https://github.com/zed-industries/zed | Zed 标签、工作区、Agent 栏行为参考 |
| Rust UI | https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md | GPUI 类型与上下文——**现行桌面选型**（gpui-kit 0.9.1，2026-11 起落地；原「仅作为中长期原生方案」表述过时） |
| Desktop 包装 | https://github.com/tauri-apps/tauri | **历史条目（未采用）**：早期「Tauri 2 保留现有 HTML」的包装方案未落地，现行桌面为 GPUI（上条）；保留作检索记录 |
| Tauri 子进程 | https://tauri.app/plugin/shell/ | **历史条目（未采用）**：随 Tauri 方案一并放弃，现行子进程经 `tokio::process`（带 CREATE_NO_WINDOW）；保留作检索记录 |
| Ratatui | https://github.com/ratatui/ratatui | 完整 CLI/TUI 客户端 |
| 串口库 | https://github.com/serialport/serialport-rs | 跨平台 USB/COM 枚举与同步串口 |
| 异步串口 | https://github.com/berkowski/tokio-serial | tokio 串口读写 |
| 终端参考 | https://wezterm.org/serial.html | WezTerm 真实串口模式 |
| 终端 ANSI 解析 | https://github.com/alacritty/vte | 终端控制序列解析 |
| Agent | https://github.com/masoncl/kres | Linux 内核代码审查、验证、修复与 TODO |
| MCP Rust SDK | https://github.com/modelcontextprotocol/rust-sdk | stdio / Streamable HTTP client+server |
| MCP 规范 | https://modelcontextprotocol.io/specification | 传输、安全、工具规范 |
| ADB 工具 | https://developer.android.com/tools/adb | `adb devices -l` 输出与设备状态官方说明 |
| Rockchip 刷写 | https://github.com/rockchip-linux/rkdeveloptool | Rockchip USB flashing 的开源命令行参考 |
| 其他 Rust GUI | https://github.com/emilk/egui | 跨平台纯 Rust UI 替代备选 |
| Agent Skills | https://learn.chatgpt.com/docs/build-skills | Skills 的组织、触发与本地文件位置 |
| Codex 插件示例 | https://github.com/openai/plugins | 实用 Skill/插件示例，新仓库 |
| Claude Skills | https://github.com/anthropics/skills | Agent Skills 框架和模板示例 |

**避免混淆**：`kres` 处理内核源码及工作流；它不会自动提供串口发现/刷机/ADB 设备控制。`rmcp` 提供 MCP 协议实现，并不代替业务权限与设备互斥。Zed 的 UI 源码可以学习，但它是完整编辑器，不适合直接当 duthub 的业务框架。
