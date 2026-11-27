# duthub 开发约定

Rust 工作区，桌面应用使用 gpui-kit。修改前先读本文件，按任务范围补齐代码、测试和必要的契约文档。

## 构建与验收

- 源码与新增测试使用 Rust。Node 和 Perl 仅用于既有资产生成器。
- Cargo 编译产物统一写入本仓库的 target/，禁止使用 C 盘或仓库外的 CARGO_TARGET_DIR。存在并行构建时，先确认目标目录和进程归属。
- 在仓库根目录用 Git Bash 运行 scripts/regression。门禁依次检查 cargo fmt --all --check、cargo clippy --workspace --all-targets --all-features -- -D warnings（含 pedantic）与 cargo test --workspace。全功能验收另跑 cargo test --workspace --all-features。
- 只有涉及窗口渲染时才加 --desktop-self-test。真实测试区分软件、原生 Windows 与硬件结果。涉及 unsafe 时运行目标相关的 cargo +nightly miri test，记录平台限制，Win32 路径补原生测试。
- Windows release 链接前确认对应 Duthub.exe / duthub-mcp.exe 已退出。遇到锁定或 PDB 竞争先核对正在运行的实例、构建路径和产物，不终止其他任务的进程。Windows FXC 路径由 .cargo/config.toml 和 CI 探测提供。
- 保留既有行为验收与测试覆盖。断言仅随确认后的产品契约变更同步。需要串口输入、继电器、烧录或电源实动时，先核对 DUT、端口、通道和授权。提交说明写清改动及原因。不 push。

## Rust 与界面

- 自有源码、rustdoc、测试、脚本、生成器和 CI 的代码注释一律用英文，且禁止在注释或诊断文案中写历史变更日期（如 `2026-xx-xx`）。技术日志（`tracing::*`）、控制台打印（`println!`/`eprintln!`）、CLI 诊断、`#[ignore]` 原因、`panic!` 与测试失败消息也一律用英文，禁止代码打印中文。产品界面通过 `crate::i18n::label` / `message` 本地化，同步维护 `zh-CN.json` 和 `en-US.json`。
- 使用 Rust 官方格式与 `clippy::pedantic` 门禁。库用类型化 `Result`，定义错误优先 `thiserror`；应用与 CLI 入口用 `anyhow` 补充上下文。生产库代码禁止无理由 `.unwrap()`；`Mutex`/`RwLock` 用 `duthub_core::util::{lock,read,write}_unpoisoned` 或写明不变量的 `.expect(...)`。Lint 抑制一律用带 `reason` 的 `#[expect(...)]`，禁用 `#[allow(...)]`。
- 借用参数优先 `&str` 和切片；移除可避免的 `.clone()`、`.collect::<Vec<_>>()` 与 `push_str(&format!(...))` 中间分配；避免 >16 KiB 大栈数组。连续多分支 `if / else if` 优先用 `match`（含元组/守卫匹配），状态约束用枚举、新类型或 typestate，数值转换优先 `try_from` / `cast_signed` / `cast_unsigned` 替代有损 `as`。所有公开类型实现 `Debug`（含凭据/Token 的类型手写脱敏 `Debug`），合理标注 `#[must_use]` 与 `#[non_exhaustive]`，rustdoc 标识符加反引号并正确缩进列表续行。
- 同步锁的 guard 必须在 `.await` 前释放（工作区强制 `await_holding_lock = "deny"`）。阻塞 I/O 和长计算放到异步 API、`spawn_blocking` 或既有工作线程，并检查取消和资源收尾。
- 全部库 crate（`crates/*`）强制 `#![forbid(unsafe_code)]`，平台 FFI `unsafe` 仅隔离在 `apps/desktop`。每个 `unsafe` 块或 `unsafe impl` 紧邻英文 `// SAFETY:` 注释，指出不变量及调用方或本地实现如何建立它；禁止模块级屏蔽 `undocumented_unsafe_blocks`。每个 `unsafe fn` 的 rustdoc 必须包含 `# Safety` 章节（`missing_safety_doc = "deny"`）。审核别名、初始化、线程约束、`Send`/`Sync`、`Drop` 和 unwind。
- 测试环境变量用 Command::env 启动隔离子进程，或为查询注入依赖。Tokio 的单线程运行时无法保证整个进程没有并发环境访问。
- UI 优先使用 gpui-kit 组件。输入状态跨帧保留，在外部模型或编辑目标变化时更新。异步选择结果回填前核对 DUT 和编辑范围。不要叠加告警与表单 Dialog，避免关闭错层。
- 参考 [Microsoft Pragmatic Rust Guidelines](https://microsoft.github.io/rust-guidelines/guidelines/checklist/)、[Rust API Guidelines](https://rust-lang.github.io/api-guidelines/checklist/)、[Rustonomicon](https://doc.rust-lang.org/nomicon/) 和 [std SAFETY 注释规范](https://std-dev-guide.rust-lang.org/policy/safety-comments.html)。

## 持久化与设备

- 配置固定在 exe 同级 .duthub/，缺失时逐文件补默认值并保留已有配置。桌面进程只加载一次 settings.json，损坏文件保留备份并报告。保存空 DUT 列表是有效操作。
- 工作目录优先级为 --workdir、已保存 workspaceDir、exe 所在目录。DUT 资料按稳定 DUT ID 确认所有权。运行数据放 duthub-data/，stdio 与桌面共用快照定位规则。工具资产放 .duthub/tools/，解析顺序为显式路径、已下载路径、PATH。
- 保留现有 JSON、MCP 和 A2A 契约。Serde 持久化键沿用 camelCase，新默认字段应遵循现有 skip_serializing_if 约定。独立编辑只提交目标记录。令牌存系统 keyring，MCP 凭据落盘走 save_settings_secured。
- 桌面默认使用真实硬件。串口初次打开失败只报一次，曾连上后断线才退避重连。终端键入依赖设备回显，视口持有焦点并尊重设备设置的 ANSI 模式。
- 串口终端快照与审计流分离。原始 RX/TX 日志保持顺序、字节和完整收尾。供电动作前同步确认分段写入。日志回调释放 writer 锁后再发错误注记。
- 心跳草稿明确保存后生效，一轮探测按完整命令组校验。硬件租约拒绝和入队失败不计为设备失联。ADB/HDC 候选取实时枚举，历史绑定独立。
- 控制动作按稳定 DUT ID 路由，人工接管在真实 I/O 处复核已入队写入。按住测试的继电器 ON/OFF 使用同一物理通道，失焦、离开或关闭时必须释放。采集基于已保存绑定，同一输入只由一个所有者打开。

## Agent 与接入

- MCP HTTP 默认回环，局域网必须显式开启并强制 Bearer。授权使用实际监听器和已绑定 DUT。Token、租约句柄和审批是不同凭据，设备派发前复核。Plan 白名单同时用于发现和分发。
- 新 MCP 工具同步检查注册、Skill 生成和 API 契约。MCP Skill 工具返回正文，GUI 导出才写文件。串口写回执表示入队，实际执行由设备回显确认。
- Agent 会话按稳定 DUT ID 绑定，恢复草稿、结果和告警时保持原目标。启动期、运行期与停止期都要有明确的资源所有者。后台结果回填前核对代数，停止时清理访问凭据、监听器和子进程。
- 沙箱由 mxc-sdk 提供 OS 强制边界。先校验策略再创建单元。Windows 无默认环境继承时仍需 SYSTEMROOT，并把 LOCALAPPDATA 指向单元内部。不要恢复手写提权路径或为验证而自动降级隔离。
- 架构和维护入口见 [docs/README.md](docs/README.md)。补充规则只保留当前有效且会影响后续修改的约束。
