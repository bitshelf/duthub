# duthub Rust 现代化审计报告

- **审计基线**：HEAD `fe24fc6c8b4346cbfb33755b31f4638c98a87def`（分支 `feat/agent-integration`），与发现集取证基线一致（撰写阶段 `git rev-parse HEAD` 实测复核；修订阶段未再变动）。
- **工作树**：在途未提交改动，发现集取证口径为 39 项（含未跟踪）。取证材料中的 diff 统计为 `git diff --stat` 输出的前 1500 字符截断——经修订阶段实测确认，该截断**恰好完整覆盖当时全部 27 个已跟踪改动文件**（`git diff --stat | wc -l`＝28＝27 个文件行＋1 行汇总；未跟踪文件不进入 `git diff --stat` 统计，另计）。修订时刻实测 `git diff --stat`＝27 files changed, 3699 insertions(+), 221 deletions(-)；`git status --porcelain | wc -l`＝44（在途继续波动，见 §⑤ 局限）。
- **报告日期**：2026-11-21（首版成文与按独立读者意见修订同日）。
- **视角**：12 维度 Rust 现代化（R1–R12，正式定义见 §② 开头「维度定义」）；与既往报告（MCP 测试报告 / 架构评审 / goose 可行性 / Agent 集成五轨 / 实施报告）的边界见 §④。
- **证据来源**：全部 87 条发现来自审计工作流发现集（其中 11 条经独立复核 confirmed，76 条为待实施时现场确认），编号统一为 F01–F87。**原始发现集由审计工作流持有并随任务输入传递，未提交入仓库**——每条发现的完整字段（dimension/where/what/evidence/status/severity/behaviorRisk/effort）以工作流产物为准，追溯需向工作流发起方索取；T01–T82 为该工作流的内部任务编号（见 §⑤）。本报告成文与修订阶段共执行约 10 组只读自证命令（§⑤ 附录），未新增代码级发现，未跑任何 cargo 构建/测试。
- **修订说明**：本版按独立读者审读意见修订——新增严重度列与三档评定标准、行为风险判定标准、12 维度正式定义、评级判定依据、依赖视图、主题×编号交叉表、文件×条目归并表、排期口径，重写 F67 红线建议，并对进入摘要的量级数字逐一复测标注（§⑤「量级数字复测口径」表）。未改动意见未涉及的内容。

---

## ① 执行摘要

**总评：架构方向正确、正面范本齐备，短板集中在「重复未收敛、治理未落地」。**

crate 按域划分（`duthub-{core,device,adb,control,flash,mcp,agent}` + desktop/tui）清晰健康；租约安全状态机、MCP 审批/白名单/security_canary、串口日志 std mpsc 写线程零丢失、TX 合并 absorb、终端快照裁尾随空白语义与主终端 uniform_list 虚拟化（坑 #46/#49）均有可对标标杆的良好实践。本轮未发现灾难性设计错误，评分分布为 **1 个 A、2 个 B、8 个 C、1 个 D**（§②）。拉低整体水位的是三类系统性问题：

1. **重复实现与死代码持续累积**——87 条中 44 条 low（评定标准见 §③），其中约半数（20 条）为死代码与重复实现类：死代码/死状态/未接线抽象 10 条（F01/F02/F03/F08/F09/F10/F11/F17/F46/F87）、重复/双写类 10 条（F26/F34/F36/F37/F39/F43/F49/F61/F86，另 F80/F45 属收敛类）——编号可按 §③ 严重度列逐条抽查；其余 low 条目为文档/命名/单点修正类。全部可机械化收敛，behaviorRisk=equivalent 者占全体的约三分之二（59/87）。
2. **语义化错误与安全边界未推广到新域**——duthub-mcp state trait 全链 `Result<_, String>`、agent_host「语义变体外壳 + String 内芯」、约 192 处生产 `lock().unwrap()`（发现集生产代码估计；修订实测口径见 §⑤：排除 `*/tests/` 目录 237 处、全仓 261 处）中毒即 panic；仓内明明有 `DeviceError`/`AgentConfigError`/`LeaseError` 语义化范本，属「有范本未推广」。Agent 集成新代码（agent_host/）正在复制旧域的最弱实践——趁在途期收敛成本最低。
3. **工程治理与门禁近乎空白**——无 `[workspace.lints]`、无 `rust-toolchain.toml`、无 `[profile.release]`、无 `deny.toml`；HEAD 的 release workflow **无 pull_request/push 触发、无任何 fmt/clippy/test 步骤**（修订实测：`git show HEAD:.github/workflows/release.yml | grep -nE "clippy|cargo test|cargo fmt|pull_request"` 零命中；fmt/clippy/test 三步仅存在于工作树未提交的 gate job，`grep` 命中工作树 release.yml:160/:164/:168），当前在途改动零机器门禁；real-hardware 门控的真机 I/O 主路径（real.rs）不入任何默认 clippy/test 门禁。AGENTS.md 的规范停留在文档，未变成机器门禁。

**数字（N = 87）**：

| 分布口径 | 值 |
|---|---|
| 严重度 | high 4 ／ medium 39 ／ low 44（三档评定标准见 §③ 开头；各条严重度见 §③ 表「严重度」列） |
| 量级 | P0 quick-win（<1 人天/条）57 ／ P1 wave（1–3 人天/条）28 ／ P2 structural（分阶段）2 |
| 行为风险 | equivalent 59 ／ needs-regression 19 ／ behavior-changing 9（三档判定标准见 §③ 开头） |
| 复核状态 | 独立复核确认（verified）11 ／ 待实施时现场确认（unconfirmed）76 |
| 维度分布 | R4×18、R1×12、R2×12、R12×12、R7×10、R3×9、R6×5、R5×3、R9×2、R10×2、R11×2、R8×0 |

**4 条 high（最高优先关注）**：

- **F13**（R2）McpState trait 全链 String 错误：「语义化→String→字符串解析→语义恢复」往返链——desktop 边界把 `LeaseError` 格式化成 `LeaseRequired/{code}: …`，server 层再 `strip_prefix+split_once` 反向解析、`tool_call_denied` 靠 `starts_with` 判审计口径；任何格式微调都静默破坏错误分类。
- **F44**（R4）`render_root` 每渲染帧对每 DUT 调 `read_log(0, usize::MAX)` 全量克隆缓冲（CAP=2000 条 envelope）后仅取尾 150 行，且构建全程持 snapshot mutex、与 MCP HTTP handler 和 2s 落盘线程同锁互堵。
- **F56**（R6）CI 无 pull_request/push 触发、无任何 lint/test 步骤（HEAD 实测零命中）——feat/agent-integration 的在途改动（含 server.rs/transport.rs 等安全面）零机器检查。
- **F77**（R12）workspace.rs 8713 行 / struct Workbench 86 字段（修订实测；取证口径 88） / 109 个方法 / 380 行构造函数的巨石，审计期间行数三次漂移（8064→8691→8713，另一工作流同文件并行），是并行协作的冲突热区。

**三大主题（路线图的组织逻辑；主题×编号交叉表见 §③「实施顺序与依赖视图」）**：
① **收敛重复、建立单一事实源**——清单/文案/微实现/测试助手单点化，把坑 #13/#41/#44 类「多处同步」问题从人工约定变编译器约束；
② **把语义化错误与安全边界推广到新代码域**——StateError 收敛（F13）为中枢，连带两条审批链同型（F14）、文案单点化（F26）、agent_host 错误链保真（F24）、lock 中毒收口（F15）；
③ **补齐治理配置与 CI 门禁**——lints/toolchain/profile/deny 四件套（F55）、PR/push 门禁与 real-hardware 编译口径（F56/F59）、测试分档（F64），让规范可执行。

---

## ② 12 维度评分总览

### 维度定义（R1–R12 考察什么）

- **R1 工作区/架构边界**：扁平 `crates/` 布局与 crate 粒度（对照 Zed 160+ crate 平铺、codex-rs 倾向新增 crate 而非膨胀旧 crate）；空壳/占位成员处置；分层边界（仿真层无渲染知识、协议类型独立）；薄层适配器模式。
- **R2 错误处理**：库 crate 用语义化错误枚举（codex-rs 的 CodexErr 用语义分类驱动重试策略），应用边界才允许字符串错误——String 错误是最弱层（对照 GreptimeDB 的错误上下文模式）；非测试代码 unwrap/expect（尤其 `Mutex.lock().unwrap()` 中毒即 panic）治理；`clippy::result_large_err`。
- **R3 并发与运行时**：避免多 runtime + block_on 桥接（对照 Zed 双执行器与「主线程神圣」原则）；锁内快照拷出（Zed 的 sync()/make_content 模式）；事件批量合并（4ms 窗口/批量上限）；channel 选型一致（std mpsc 与 tokio mpsc 混用需有理由）；锁粒度与双层锁。
- **R4 数据结构与分配**：clone 密度热点（对照 rust-analyzer「premature pessimization」清单）；String 反复分配；Option 套层与用 Option 值表达状态语义（应 enum 化）；网格/scrollback 数据结构对照 alacritty Grid Storage 单块连续与 wezterm VecDeque 行复用；`clippy::large_enum_variant`。
- **R5 依赖治理**：workspace.dependencies 收敛统一版本；消除同功能多版本共存；feature 最小化；`[profile.release]` 的 LTO/strip/panic 策略（ripgrep release-lto 档位）；MSRV 声明；cargo-deny/audit。
- **R6 lint/CI 门禁**：`[workspace.lints]` 与 clippy.toml disallowed-methods（Zed 用 lint 当平台/正确性约束）；CI 跑 fmt/clippy/test（TiKV 零警告、s2n-quic deny panic lints）；rust-toolchain.toml 固定工具链（clippy CI 官方建议）；deny(warnings) 只在 CI 不在源码。
- **R7 测试策略**：测试分层（内联+集成+ref-test 录制回放，Alacritty tests/ref.rs）；无测试模块补齐；行为契约测试优先（Copilot 经验：既有 E2E 是迁移门禁）；测试助手跨 crate 共享（codex-rs core_test_support 模式）；benchsuite/fuzz 思想。
- **R8 unsafe 治理**：集中到 FFI/OS 边界；每块 SAFETY 注释（Firecracker 的 undocumented_unsafe_blocks lint）；非必要不写 unsafe。
- **R9 最新特性采用**：`#[expect]` 替代 `#[allow]`（1.81）；let-chains（1.88）；std 的 LazyLock/OnceLock；async fn in trait；edition 2024 惯用法；新 std API 替代手写实现。
- **R10 终端宿主正确性**：alacritty_terminal 宿主范式——EventListener 回调集中分发；ANSI 查询类应答保序（Zed 的 ColorRequest 教训）；同步输出宿主驱动超时；快照从锁内拷出；裁尾随空白语义；scrollback 上限分层；uniform_list 懒渲染。
- **R11 MCP/Agent 安全**：codex-rs 范式——协议类型独立、工具面 allow/deny 与 schema 大小保护、审批 human-in-the-loop、错误语义分类；rmcp SDK 官方用法。
- **R12 UI 状态与巨石拆分**：gpui Entity/效果冲刷模型贴合度；草稿编辑模式收敛；巨石文件拆分策略（先模块内拆文件再评估升 crate，警惕过早模块化）；too_many_arguments 用参数对象重构。

### 评级口径（可复算规则）

- **D**：维度核心配置/门禁呈「面」级缺位——缺的不是点而是整套。仅 R6 命中（五项治理配置全缺＋HEAD 零 PR/push 门禁＋real-hardware 主路径不入门禁，F56 为 high）。
- **C**：存在跨文件/跨模块的系统性短板，或该维度含 high / behavior-changing 的正确性问题。R1/R2/R3/R4/R5/R7/R10/R12：R2 含 high F13，R4 含 high F44，R12 含 high F77，R10 的 F72 是真机正确性缺口且 behavior-changing，R1/R3/R5/R7 为跨 crate 系统性卫生/形状问题。
- **B**：条目少且均为 low、无正确性风险、有既有正面实践兜底。R9（2 条 low，纯「新特性采用度」问题）、R11（2 条 low，审批/白名单/canary 安全主体齐备）。
- **A**：零发现，或仅点状问题且关键合规已实证。仅 R8（生产 unsafe 17 处全部集中于 FFI/OS 边界，且 SAFETY 注释与 unsafe 一一对应——修订实测 job.rs 15/15、media.rs 2/2；语义级逐块审查仍未做，见 §⑤ 局限）。

区分示例（回应「同为 2 条为何 R9=B 而 R10=C、R5 仅 3 条为何评 C」）：R9 的 2 条均为 low 且是采用度而非错误；R10 的 2 条为 medium 且含 behavior-changing 真机正确性缺口。R5 仅 3 条但全部 medium，且属跨 8 个成员的系统性治理缺位（单一版本事实源不存在、同功能多版本并存），故按「面」级短板评 C 而非 B。

### 评分总览

| 维度 | 评级 | 条数 | 判定依据速览 | 核心问题 | 标杆对照 |
|---|---|---|---|---|---|
| **R1 工作区/架构边界** | C | 12 | 0 high / 1 bc；死代码 6 处＋巨石＋trait 双轨 | crate 粒度健康（按域 8 crate 平铺），但死代码/空壳/未接线抽象累积 6 处（transport 阻塞入口、ipc M0 骨架、AgentAccessStore DAO、DeviceService::new、duthub-agent 空壳、desktop 5 处死状态）＋ lease.rs 1794 行巨石 ＋ trait 双轨（AdbBackend/FlashExecutor 无 dyn 消费方、FlashExecutor::test 与 run_test 已漂移） | Zed 160+ crate 平铺；codex-rs 倾向新增 crate 而非膨胀旧 crate；薄层适配器（AdbBackend::devices 纯转发即既有范本） |
| **R2 错误处理** | C | 12 | 1 high / 5 bc；String 最弱层×3 域＋中毒级联 | 三大域停留在 String 最弱层（McpState trait 全链、agent_host「语义外壳+String 内芯」、llm 6 函数）＋ 约 192 处生产 `lock().unwrap()` 中毒即 panic ＋ 语义失真点（PNG 头缺失→Unsupported、探测失败→None、REPLY_TIMEOUT/PULSE_MAX_MS 不变量矛盾、进程 OS 错误折叠 Invalid）；仓内已有 DeviceError/AgentConfigError/LeaseError 范本未推广 | codex-rs CodexErr 语义分类驱动重试策略；GreptimeDB 错误上下文模式；Google Style Guide「Result 保留上下文」 |
| **R3 并发与运行时** | C | 9 | 0 high / 0 bc；双 runtime＋block_on＋裸线程 | 双内嵌 runtime（DeviceService multi_thread×2 worker ＋ ControlService current_thread）＋ ControlService 5 处 block_on 桥接；渲染线程同步 keyring I/O（save_provider）；裸线程派发 8 处（cx_spawn_job、media.rs×6、agent.rs 探针）；每监听器 2-worker runtime 无理由注释。正面：串口写线程零丢失、TX 合并、每监听器隔离 runtime（设计合理仅缺注释） | Zed 双执行器与「主线程神圣」；锁内快照拷出（sync()/make_content 模式）；channel 选型一致；4ms 事件批量窗口 |
| **R4 数据结构与分配** | C | 18 | 1 high / 1 bc；每帧全量克隆＋微实现重复 | 12 维之最（18 条）：每帧全量克隆热点（refresh_snapshot 每帧每 DUT 至多克隆并丢弃 1850 条 envelope、pages.rs 7× DutConfig 整克隆、heartbeat tick 每 50ms 全量克隆）、transcript `remove(0)` O(n)、uniform_list 每可见行重建高亮规则表、微实现重复（hex×4/时间戳×3+/Raw enum×2）、Option 值表达三态语义 | rust-analyzer premature pessimization 清单；alacritty Grid Storage 单块连续；wezterm VecDeque 行复用；clippy::large_enum_variant |
| **R5 依赖治理** | C | 3 | 0 high / 0 bc；跨 8 成员单一版本事实源缺位 | workspace.dependencies 仅 6 条目、20 余处跨成员散写（tokio 5 成员 6 处 feature 各异、tracing/serde_json 裸声明双轨）；同功能多版本并存（reqwest 0.12/0.13、base64 0.22/0.23、sha2 0.10/0.11、getrandom×3，0.23/0.11/0.3 系钉死的 rmcp 3.10 内部树引入）；rmcp 6 feature 中 1 个生产零引用；无 deny.toml/cargo-deny | ripgrep release-lto 档位（profile 治理）；workspace.dependencies 单一版本事实源实践 |
| **R6 lint/CI 门禁** | D | 5 | 1 high / 1 bc；配置与门禁「面」级缺位 | 治理配置五项全缺（`[workspace.lints]`/clippy.toml/rust-toolchain.toml/`[profile.release]`/deny.toml）＋ HEAD 无 pull_request/push 门禁（lint/test 步骤实测零命中，gate job 仅在未提交工作树）＋ real-hardware 主路径不入任何门禁 ＋ AGENTS.md 写「遵守工作区的 rustfmt 配置」但配置文件不存在 ＋ fxc 探测/资产校验/gate 三步双实现 | Zed 用 lint 当平台/正确性约束；TiKV CI 零警告；s2n-quic deny panic lints；clippy 官方 CI 固定工具链建议（rust-toolchain.toml） |
| **R7 测试策略** | C | 10 | 0 high / 0 bc；助手复制＋缺 ref-test＋无快档 | 测试助手逐份手写（MCP 5 文件＋server.rs 内联约 2800 行四类重复，修订实测 2481＋约 290；取证口径约 2900）；device/desktop 跨 crate 复制（MockBackend×2/StubState×3/temp_dir×8/wait_until×3）；租约 TTL 测试依赖真实 sleep；缺 ref-test 录制回放层；goose 真实 E2E（20MB 资产、等待 30–120s）无快档开关。正面：ignored live 测试、security_canary、grep 契约测试已成型 | Alacritty tests/ref.rs 录制回放；Copilot 迁移经验「既有 E2E 是迁移门禁」；codex-rs core_test_support 助手共享 |
| **R8 unsafe 治理** | A | 0 | 0 条发现；数量少＋位置集中＋SAFETY 注释齐全（修订实测） | 本轮 87 条发现零 unsafe 项；撰写时实测生产代码 unsafe 仅 17 处（agent_host/job.rs 15 ＋ media.rs 2），集中于 OS/FFI 边界（进程作业对象、采集），无散落业务逻辑的 unsafe；SAFETY 注释计数与 unsafe 一一对应（job.rs 15/15、media.rs 2/2） | Firecracker undocumented_unsafe_blocks lint（语义级逐块审查未做，见 §⑤ 局限） |
| **R9 最新特性采用** | B | 2 | 0 high / 0 bc；纯采用度、无正确性风险 | MSRV 1.100 前沿，但 25 处 `#[allow]` 对 0 处 `#[expect]`（占位抑制在 API 接入使用后不会提醒清理，已有一处过期误标）、audit.rs 7 参数签名未参数对象化、main.rs 死导入保活函数 | `#[expect]`（1.81）/let-chains（1.88）/LazyLock/OnceLock；edition 2024 惯用法 |
| **R10 终端宿主正确性** | C | 2 | 0 high / 1 bc；真机正确性缺口＋漏改点 | VoidListener 丢弃 alacritty_terminal 全部驱动事件——设备侧 ANSI 查询（DSR CSI 5n/6n、DA1）应答永远发不回设备，真机 vim/htop 将挂起或渲染错乱（behavior-changing）；side_panel ADB Shell 终端漏虚拟化（主终端已按坑 #49 迁 uniform_list）。正面：快照锁内拷出、同步输出宿主驱动超时、裁尾随空白语义（坑 #46 四单测）已成型 | alacritty_terminal 宿主范式（EventListener 集中分发）；Zed ColorRequest 教训（ANSI 查询应答保序） |
| **R11 MCP/Agent 安全** | B | 2 | 0 high / 0 bc；安全主体实践齐备、余两处低危 | 审批 human-in-the-loop、Plan 白名单、security_canary、DUT 专用监听器 scoped 授权齐备；两处低危：authorize_scoped_read 命名反直觉（通用监听器 no-op，四个调用点正是坑 #44 历史漏授权高危区）、stderr 脱敏表不含已签发 listener token | codex-rs 审批/协议类型独立/schema 大小保护范式；rmcp SDK 官方用法 |
| **R12 UI 状态与巨石拆分** | C | 12 | 1 high / 0 bc；巨石热区＋草稿结构性冗余 | workspace.rs 8713 行/86 字段/109 方法巨石（并发冲突热区，high）；pages.rs 4942 行（修订实测）；草稿体系三重冗余（In 枚举 46 个变体×5 处 match 联动，坑 #41/#45 是该形状的结构性产物，且有 In::A2aPort 回跳现场 bug）；演示面板手写编辑器违反「设置输入使用 kit Input/Textarea」红线；5 处 too_many_arguments | gpui Entity/效果冲刷模型；matklad 警惕过早模块化（先模块内拆文件再评估升 crate）；too_many_arguments 参数对象重构 |

> 注：严重度/行为风险两列的三档口径与各条取值见 §③；「条数/判定依据速览」为评级可复算依据。

---

## ③ 分级重构路线图

复核列：✓ = 独立复核确认（verified）；○ = 待实施时现场确认（unconfirmed，先按「现状证据」行号核对现场再动手）。

**严重度三档评定标准**（转写自发现集取证口径，本报告未重新评级）：
- **high**：影响安全面正确性/审计口径（F13 错误分类静默破坏、F56 在途改动零门禁），或造成持续性性能/结构危害且影响面全局（F44 每帧全量克隆×锁互堵、F77 并发冲突热区巨石）；
- **medium**：跨文件的系统性卫生/形状问题，或有明确正确性影响但单点可修（如 F18 不变量矛盾、F72 真机 TUI 挂起、F65 安全路径测试 flaky 风险）；
- **low**：局部清理/单点修正/文档与命名，无正确性风险或影响面限于单文件（如 F46 死导入、F61 清单同源化、F69 补注释）。

**行为风险三档判定标准**：
- **equivalent**：对外可观察行为不变——错误文案逐字保留、JSON/契约形状不变，含纯删除死代码、内部收敛；
- **behavior-changing**：有意的对外语义修正且变化局部可枚举（错误类别/文案/契约形状/触发时机的显式修正，如 F14 审批链对齐、F20 错误类别改 Invalid、F22 队列满恢复 Busy、F57 tag 触发）；
- **needs-regression**：意图为行为等价，但触及回归敏感区（锁/运行时时序/序列化形状/快照游标/错误分类消费面/巨石拆分），必须靠既有测试套件＋针对性回归验证。

F19 与 F20 的分界（同型改动、不同档位的原因）：F19 的 ErrorKind 拆分会改变多处上层 match 消费点（flash/lib 与 desktop 展示路径），影响面横跨模块，故 needs-regression；F20 是单点错误构造修正、影响面限于该两处错误的类别与文案，故 behavior-changing。

### P0 quick-win（<1 人天/条，57 项）

| 编号 | 复核 | 维度 | 严重度 | 位置 | 现状证据 | 建议 | 行为风险 | 建议验证方式 |
|---|---|---|---|---|---|---|---|---|
| F01 | ✓ | R1 | medium | crates/duthub-mcp/src/transport.rs:171-209；lib.rs:8,23 | serve_http_blocking/serve_http_listener_blocking 全仓零调用方：grep 仅命中定义、内部转发与导出；apps/ 与 tests/ 零调用（tests 只用 http_router*/spawn_*；plan_badges.rs:168-169 断言源码含 spawn_http_listener_on( 强化生产绑定） | 删除两个死入口并同步 lib.rs 导出与文档注释；若需保留阻塞入口，以 prepare_listener(listener, auth_enabled) 收敛与 spawn 版逐行重复的六步前处理（transport.rs:186-199 vs 258-269） | equivalent | cargo test -p duthub-mcp ＋ bash scripts/regression |
| F02 | ✓ | R1 | medium | duthub-core/src/ipc.rs:14-27,108-257（cfg(test) :266-331）、event.rs:10-31、lib.rs:43,45 | AppCommand/AppService/AppEvent 为 M0 演示骨架：全工作区零生产消费者（唯一引用为自身单测与 re-export），注释仍写「M1.1 将替换为真实串口后端的事件流」误导读者 | 删除三类型＋lib.rs 对应 re-export＋仅覆盖死代码的单测（不属削弱门禁红线）；保留仍在总线消费的 UiEvent/EventEnvelope/ScreenRow/ScreenSnapshot/ChunkStyle；event.rs 清空则删文件与 lib.rs:14 模块声明；连带更新 docs/ARCHITECTURE.md:23/73 等历史引用（注意：docs/ 下 ARCHITECTURE.md 与 ARCHITECTURE_REVIEW.md 是两个不同文件且均在位——前者是架构文档、后者是既往评审报告，本条更新的是前者） | equivalent | cargo test -p duthub-core ＋ git grep 复核零引用 |
| F03 | ✓ | R1 | medium | duthub-core/src/agent_access.rs:624-651、lib.rs:26-28 | AgentAccessStore trait＋JsonAgentAccessStore 是「DAO 边界预留（偏差 1）」未接线：生产零调用（:864 唯一使用在 cfg(test) 区），desktop 实走 workspace.rs sync_agent_access 直接读写 SettingsFile.agent_access，SQLite 迁移承诺已失效 | 删除 trait/实现/re-export/仅覆盖其的 cfg(test) 用例；若保留预留则反向落地：sync_agent_access 持久化改经该抽象——二者取一 | equivalent | cargo test -p duthub-core（agents_config/agent_access_overlay） |
| F05 | ✓ | R1 | medium | duthub-core/src/backend.rs:53-67＋duthub-flash/src/lib.rs:50-75 | AdbBackend/FlashExecutor 全工作区无 dyn 消费方（desktop 直接用具体类型）；FlashTester::run_test（走 resolve_executable）与 FlashExecutor::test（不走）白名单错误文案已漂移 | FlashExecutor::test 委托 run_test（或提共用内部函数）统一错误构造；AdbBackend::devices 纯转发可保留；是否删除 trait 属 API 收缩，单独决策 | needs-regression | cargo test -p duthub-flash -p duthub-adb ＋ 烧录工具页手工验证 |
| F06 | ○ | R1 | low | duthub-adb/src/lib.rs:1-4,391-449 | crate 名 duthub-adb 但 391-449 行承载 OpenHarmony HDC 客户端（HdcTarget/HdcClient/parse_targets），文档头只描述 ADB；消费方 workspace.rs:4386 用 HdcClient::real() | lib.rs 头注释显式声明「ADB＋HDC 双连接器发现层」；结构性更名（如 duthub-dut-connectors）另议并同步品牌规范条目 | equivalent | 编译即可（文档级改动） |
| F07 | ○ | R1 | low | duthub-control/src/driver.rs:268-282,351-359 | RelayDriver::open 与 ControlBackend::channel_status 各自实现 1..=MAX_CHANNELS 逐通道 STATUS 循环，仅收集类型不同（ChannelInfo vs (u8,bool)） | open() 复用 channel_status 结果再 map 成 ChannelInfo；错误传播语义一致（两处均为 ？ 短路） | equivalent | cargo test -p duthub-control（driver.rs 内联测试）；继电器实动口径不变（通道 1 往返已有授权，relay_live_roundtrip ignored 可复跑——见「需硬件窗口清单」） |
| F08 | ○ | R1 | low | duthub-agent/src/lib.rs:1、Cargo.toml:12 | 纯空壳成员：lib.rs 仅一行、[dependencies] 空、全仓零引用；占位意图与延期原因无处可查（apps/tui 至少有「M0.3 占位，M7.1 实现」注释） | 照 tui 形状补占位注释（对应延期任务＋实现条件＋「占位成员、勿在此堆放实现」，链接 43c58dc:docs/AGENT_IMPLEMENTATION_TASKS.md）；不删成员（品牌规范保留 duthub-agent 标识符） | equivalent | 编译即可 |
| F09 | ✓ | R1 | medium | duthub-device/src/lib.rs:73-97＋apps/desktop/src/main.rs:160-200 | DeviceService::new 全仓零调用（grep 退出码 1；ADR-WORKDIR 注明桌面走 custom）；TTL clamp、租约/审计接线、归档、日志开关、scrollback 装配在 main.rs:172-198 逐段重复，且内嵌第二处 load_settings 入口（接回会踩坑 #9 损坏文件改名副作用） | 删除 new，或下沉 from_settings(logs_dir, &Settings) 装配函数供 desktop 复用消除双份；保持 custom 为测试/工厂入口；以不破坏 ADR-WORKDIR 裁决为前提 | equivalent | cargo test -p duthub-device ＋ desktop 编译 ＋ grep DeviceService::new 零命中确认 |
| F10 | ✓ | R1 | medium | apps/desktop/src/workspace.rs:1021-1026,195,362-364,1509-1512,1521 | 死状态/死分支五处：agent_model_provider/.agent_model_name 只写不读（读点 1223-1236 用的是同名局部变量，实际消费走 AgentState）；In::Prompt 从未构造（3 个死 match 臂）；ModalKind::RenameDut 从未构造（约 35 行死 UI）；p() 无调用者；:1521 allow(dead_code) 误标（focus() 在 6648 被调用） | ①④⑤直接删除；②③确认无调用后删除——若属路线图占位（Prompt 预留终端 prompt 前缀、RenameDut 供右键菜单）改在任务清单立项而非代码内 allow 长期滞留；顺带清掉 input_value/input_set 死臂 | equivalent | cargo clippy -p duthub-desktop --all-targets ＋ bash scripts/regression |
| F11 | ○ | R1 | low | apps/desktop/src/alerts.rs:186-189,261-264、components.rs:178-191、theme.rs:13-21,27-43,127-135 | 五处死代码/占位（resume/pending_len 仅测试使用、pill 全仓零调用、theme panel 字段零使用、ThemeMode::palette 零调用）＋三处过时 allow(dead_code)（from_setting 实际被 workspace.rs:1184/2847 与 pages.rs:3704 调用；rail/warn 被 apply_brand:211/227 使用） | resume/pending_len 改 #[cfg(test)]（照同文件 suppress 模式）；删 pill/panel/palette；移除过时 allow（编译器立即验证） | equivalent | cargo clippy -p duthub-desktop --all-targets |
| F12 | ○ | R1 | low | apps/desktop/src/agent.rs:1459（对照 data.rs:217-231） | render_inspector 直读 file.duts[w.duts.selected] 裸索引，绕过 duts.selected() 的 editing_new_connection 草稿语义——新建连接草稿时检查器显示上一个已保存 DUT | 改按 active_dut_id()（「后台任务和告警始终使用原 DUT ID」红线口径）或 w.duts.selected() 解析；草稿期显示对象会变（显示草稿 DUT vs 保持旧值），需产品拍板后回归（越界决策先问）——**派发注意：需产品决策，见 P0 派发段** | behavior-changing | cargo test -p duthub-desktop ＋ 新建连接草稿态手工查看检查器 |
| F14 | ○ | R2 | medium | apps/desktop/src/mcp_runtime.rs:87-94 与 :125、duthub-device/src/control.rs:88-94,548、duthub-mcp/src/server.rs:250-262,1347 | 同一 ControlEntryError::Approval 两条 MCP 链两种字符串形状：control_execute 出「ApprovalRequired/{code}: …」（客户端可解析、审计记 Denied），relay_test_execute 走 err.to_string()（Display 是裸 message）→ server 层只识别 LeaseRequired/ 前缀而落 internal_error，审计记 Error 而非 Denied，与 API_CONTRACT 契约不符 | quick-win：relay_test_execute 的 Approval 分支复用 control_execute 的 code_of＋message 格式化；结构性收敛随 F13（StateError::Approval 让编译器强制两条链同型）——**派发注意：对外错误形状修正，实施前与 API_CONTRACT 维护者确认口径** | behavior-changing | cargo test -p duthub-mcp（dut_binding/security_canary）＋ http_auth_e2e（注意：该测试文件本身在在途改动清单中，见 §⑤ 局限）；提交说明明确这是向契约对齐的对外形状修正 |
| F16 | ○ | R2 | medium | duthub-core/src/llm.rs:78,102,116,125,160,269 | 6 个函数全用 Result<_,String>（库 crate 最弱错误层），160-260 三 provider 分支重复同一 send→status→截断脱敏→解析→probe_reply 流水线；同 crate 反例范本：agent_config.rs AgentConfigError 枚举 | 定义 LlmError 枚举（EmptyKey/InvalidBase/Client/Http/Decode），Display 文案逐字保留（调用方 UI 直接显示 message 的行为不变）；抽共享 helper，分支只留差异部分 | equivalent | cargo test -p duthub-core（llm.rs:330-391 TCP fixture） |
| F17 | ✓ | R2 | medium | duthub-core/src/approval.rs:68,106-108,170-181 | ApprovalManager.audit: Mutex&lt;Vec&lt;String&gt;&gt; 无界增长且死代码：issue/consume 各 push 一条永不清理（MCP 长期进程内存缓慢泄漏），唯一读取口 audit_tail 全工作区零调用 | 删除字段、两处 push 与 audit_tail——审计职责已由 audit.rs AuditSink 统一承担（desktop main.rs:172 与 device lib.rs:87 均已建）；若认为审批留痕有价值，复用 AuditSink::record("approval.issue",…) | equivalent | cargo test -p duthub-core ＋ git grep audit_tail 零命中 |
| F18 | ○ | R2 | medium | duthub-control/src/driver.rs:32,34,252,292 | REPLY_TIMEOUT=30s 与 PULSE_MAX_MS=60_000 不变量矛盾：>30s 的脉冲是合法输入（:292 只校验 ≤60s），dispatch 在 30s 报 Timeout 而独占 task 中脉冲继续执行并正常释放——错误语义失真，上层可能把 Timeout 当「状态不明」做补救 | REPLY_TIMEOUT 提到 ≥ PULSE_MAX_MS＋ECHO_TIMEOUT＋余量（如 65s），或把 PULSE_MAX_MS 收紧到 < REPLY_TIMEOUT；两常量注释编码相互约束（「应答上限必须覆盖最长脉冲」）——**派发注意：涉继电器时序，见「需硬件窗口清单」** | behavior-changing | cargo test -p duthub-control --features real-hardware（driver.rs 22 个内联测试）；涉继电器时序，实动验证引用 relay_live_roundtrip ignored（通道 1 已授权） |
| F19 | ○ | R2 | medium | duthub-core/src/process.rs:66,91,110-122 | 进程执行路径 OS 错误全部折叠 Invalid(String)（spawn 失败/try_wait/wait/非零退出），与 driver.rs:577-600 按 ErrorKind 映射 NotFound/PermissionDenied/Timeout 的既有做法不一致，上层无法区分「权限拒绝」与「配置非法」 | 按 std::io::ErrorKind 分类映射，保留 e.to_string() 作上下文；改动后全量检查 Invalid(_) 的 match 点（flash/lib 与 desktop 展示路径）——与 F20 的分界：本条改变多处上层 match 消费点、影响面横跨模块，故 needs-regression（见行为风险判定标准） | needs-regression | cargo test --workspace ＋ 烧录/ADB 失败路径手工验证 |
| F20 | ○ | R2 | low | duthub-adb/src/lib.rs:76,203-205 | 两处错误语义失真：PNG 头缺失报 DeviceError::Unsupported（Display「unsupported action」，语义是动作不被支持）；list_local_server_devices 用 map_err(\|_\|…) 丢弃 mozdevice 原始错误上下文（连接被拒/协议错误不可区分） | PNG 缺失改 DeviceError::Invalid＋「输出前缀中未找到 PNG 帧」上下文；list_local_server_devices 保留错误源 Display（format!("本机 ADB 服务不可用: {e}")）——与 F19 的分界：单点错误构造修正、影响面限于该两处，故 behavior-changing | behavior-changing | cargo test -p duthub-adb ＋ ADB 面板手工验证 |
| F22 | ✓ | R2 | low | duthub-device/src/lib.rs:303-306,560-563,610-613,692-695＋:575,587,682 | TrySendError→DeviceError 映射 4 处逐字重复（发现集初扫线索只列 3 处，独立复核确认 reset_terminal 是第 4 处）；set_baud/set_line_ending/resize_terminal 又把 Full 折叠成 Disconnected——队列满与断线语义混同 | 抽单一 try_send_cmd 助手（Full→Busy、Closed→Disconnected）供全部命令入口共用；三个退化点换统一映射；UI 层按错误类别分支处同步核对 | behavior-changing | cargo test -p duthub-device ＋ UI 错误提示手工回归 |
| F23 | ○ | R2 | low | duthub-device/src/control.rs:563-576 与 579-629 | relay_node 与 resolve_target 的继电器分支重复同文案错误构造（Disabled\|UsbBoard「DUT 未配置可用的控制单元绑定」×2、UsbSerial 空 node「继电器节点（串口）未配置」×2） | 提取共享的 UsbSerial 目标解析（返回 node/驱动/通道表），两臂复用同一实现，错误构造单点化 | equivalent | cargo test -p duthub-device（control.rs 既有测试） |
| F25 | ○ | R3 | low | duthub-mcp/src/transport.rs:309-315＋:199,:269 | 每监听器一个独立多线程 runtime×2 worker：desktop N 端点＝N runtime＋3N 线程，2 worker/端点的必要性无注释说明（MCP 端点负载极低）；独立 runtime 隔离设计本身合理（shutdown_and_join 独立回收、坑 #10 风险回避） | 两选一：worker_threads 降为 1 并注释「每端点单 worker：MCP 流量低，shutdown 独立性优先」；或保留 2 补理由。不合共享 runtime（保 ListenerHandle join 语义与坑 #10 隔离） | needs-regression | cargo test -p duthub-mcp（listener_lifecycle 四用例） |
| F26 | ○ | R3 | low | duthub-mcp/src/snapshot.rs:126-182＋state.rs 默认实现 | 能力不可用文案逐字重复：修订实测 state.rs 10 处默认实现 Err 文案（246/251/256/261 四处「控制租约经此状态源不可用（仅桌面 HTTP 运行时支持）」＋325/337/342/352/357 五处「…经此状态源不可用（仅桌面运行时支持）」＋363 一处「skill.export 经此状态源不可用」，两种句式）＋snapshot.rs 6 处 override「stdio 快照模式为只读…请连接 desktop 的 Streamable HTTP 端点」（grep 字面 6 处）——合计 16 处（发现集取证口径约 14）。改一句文案要动多点 | 随 F13 加 StateError::Unsupported(&'static str) 单一构造点；或 supports_control() 能力位（对照 supports_adb_shell 既有模式，server 层提前给 TOOL_UNAVAILABLE 形错误）；文案与触发时机保持现状。维度说明：本条沿用发现集原始 R3 标注（出自并发/运行时审计域的重复命中），内容实为文案重复、与 R4 同族，修复归宿随 F13 | equivalent | cargo test -p duthub-mcp ＋ stdio e2e |
| F29 | ○ | R3 | low | apps/desktop/src/workspace.rs:8704-8706,4386 | cx_spawn_job 用裸 std::thread::spawn 派发 USB 扫描（每次 next_job＋1 并 spawn、无 in-flight 去重，连点下拉即连开线程）；同文件 1469/4107 与 side_panel.rs:341 已用 background_executor 正确形状。复核修正：scan_usb_devices 签名无 cx，需给调用链（open_combo/select_usb_debug_tool→render_root 闭包）穿线，改动面大于原评估 | 删除 cx_spawn_job；scan_usb_devices 改 cx.background_executor().spawn(...).detach()，结果仍经 job_queue 回流——与同文件 4107 形状统一 | equivalent | cargo clippy ＋ GUI 打开串口下拉枚举验证 |
| F30 | ○ | R3 | low | apps/desktop/src/mcp_runtime.rs:573-584,490-498 | 快照落盘线程 loop 无停止信号（McpRuntimeHandles::shutdown 只关 HTTP 监听，落盘线程留到进程退出才死）；每轮 snapshot.lock().unwrap().clone() 全量克隆持锁，与渲染帧 refresh_snapshot 同锁排队（F44 的另一端） | mpsc 停止通道并入 McpRuntimeHandles::shutdown（与监听器 join 同批收尾，Drop 语义不变）；克隆可保留（2s 间隔成本可接受）；先补停止信号再做 F44 锁外构建（依赖理由见「依赖视图」） | equivalent | cargo test -p duthub-desktop ＋ 手工退出验证日志完整 |
| F32 | ○ | R3 | low | apps/desktop/src/media.rs:207,220,232,393,486,500 | 六处裸 std::thread::spawn 未命名且丢弃 JoinHandle（producer 线程 panic 后 ready 永为 false，状态只显示 waitingForVideo 不报错，排查无线程名可循）；agent_host 已有 Builder::name 惯例（acp.rs:550/host.rs:216） | 六处统一 thread::Builder::new().name("duthub-media-{role}")；线程形状本身不动（每采集源独立 runtime＋block_on 线程与串口写线程惯例一致） | equivalent | cargo build -p duthub-desktop ＋ 采集面板手工验证 |
| F33 | ○ | R3 | medium | apps/desktop/src/agent.rs:25-26,445-490＋workspace.rs:1477,4577-4578 | legacy 模型探针：std::thread::spawn＋每次新建 tokio current_thread runtime＋全局静态 Mutex 槽（MODEL_TEST_RESULT）＋50ms 周期轮询 drain——与 provider_models 的 background_executor 推送形状分叉 | 改 cx.background_executor().spawn＋cx.spawn 完成回推，删全局槽与 drain 轮询臂；仓内三处「线程＋临时 runtime」（agent.rs:472、media.rs:148、provider_models.rs:230）收敛共享 helper；legacy 模型页去留是既有裁决，本条不涉及 | equivalent | cargo test -p duthub-desktop ＋ 模型测试按钮手工验证 |
| F34 | ○ | R4 | low | duthub-mcp/src/server.rs:593,608,632,863,990,1020,1144,1184,1216,1253＋state.rs:79-95 与 mcp_runtime.rs:152-158 | 10 处 to_value().unwrap_or(Value::Null) 样板（约 6 处纯透传 DTO 直转）；ApprovalErr 是纯字段转发 newtype，code() 的 Missing/Unknown/Mismatch match 与 mcp_runtime code_of() 是同一映射第二份 | 纯透传处直接 Ok(Json(dto))（rmcp 3.10 Json&lt;T&gt; 只要求 Serialize＋JsonSchema，state.rs DTO 均已 derive；带附加字段的 dut_info/serial_read 保留 to_value 写法）；code() 映射收进 duthub_core::ApprovalError 单点，code_of 删除改调之 | equivalent | cargo test -p duthub-mcp ＋ http_auth_e2e 形状回归（该文件在途状态见 §⑤） |
| F35 | ○ | R4 | low | duthub-core/src/agent_access.rs:268-285,297-312 | HashMap&lt;String, Option&lt;Arc&lt;AgentCredential&gt;&gt;&gt; 用 Option 值表达第三态（同指纹冲突→歧义拒绝），与「无凭据」混淆；消费侧 and_then(Option::as_ref) 两层解包、构建侧两处同形重复 | enum CredentialSlot { Unique(Arc&lt;AgentCredential&gt;), Ambiguous }（或 Vec＋len==1 才放行）；抽共享 helper fingerprint_insert 统一两处 entry 逻辑；歧义指纹仍拒绝语义不变 | equivalent | cargo test -p duthub-core（agents_config/agent_access_overlay） |
| F36 | ○ | R4 | low | duthub-core/src/agent_access.rs:250-255、store.rs:185-211、approval.rs:53-62、model.rs:322-363、audit.rs:68-73、ipc.rs:259-264 | 微实现多处重复：SHA-256→hex 手写 4 份（token_fingerprint/sha256_hex/sha256_file/args_hash 各自 digest.iter().map(format!("{b:02x}")).collect()）；unix 毫秒时间戳 3 份＋store.rs 3 处内联；model.rs 两个 legacy 反序列化器重复定义同一 untagged enum Raw{Str,List} | crate 内加 hex(bytes) 助手供 4 个哈希口复用；时间戳统一 audit::unix_ms（或抽 crate::time）；string_or_list/string_or_list_vec 抽公共 Raw 反序列化函数（两入口保留 join(" ") 与 split_whitespace 输出差异） | equivalent | cargo test -p duthub-core（roundtrip 回归） |
| F37 | ○ | R4 | low | duthub-core/src/keys.rs:9-74 | keyring 封装六函数＝同一模板×2 组：entry()/a2a_entry() 仅差 service 条目前缀（model/ vs a2a/），set/get/delete×2 除文案外逐行相同（含 NoEntry 幂等处理） | enum KeyScope { Model, A2a }＋entry_for(scope, id)，一套 set/get/delete 内部实现；六个公开函数签名不变（lib.rs:44 re-export 与调用方零改动），错误文案补 scope 描述 | equivalent | cargo test -p duthub-core ＋ Windows 凭据管理器手工录删一次 |
| F39 | ✓ | R4 | low | duthub-control/src/driver.rs:316-347 | power/reset/power_key 三方法重复同一段 target→channel 解析链（assert_relay_target＋u8::try_from＋valid_channel，含同文案「通道号 {} 超出继电器范围」×3） | 提取 resolve_channel(target: &ControlTarget) -> DeviceResult&lt;u8&gt; 三处调用；错误文案与传播顺序保持不变 | equivalent | cargo test -p duthub-control（driver.rs 内联测试） |
| F42 | ○ | R4 | medium | duthub-device/src/heartbeat.rs:278-287,297-307＋bus.rs:77-90 | tick 每 50ms 全量克隆所有启用 DUT 的 HeartbeatConfig（含命令字符串）；在途探测每 tick 经 read_log 克隆窗口内全部 EventEnvelope（CAP 2000，含 dut_id/session_id/文本深拷贝）再 filter_map 出第二份文本副本——启动洪流窗口反复大块分配 | configs 表改存 Arc&lt;HeartbeatConfig&gt;（tick 内克隆 Arc 而非内容）；bus 增 read_log_with(session, since, limit, visitor) 访问器锁内过滤收集；行为等价 | equivalent | cargo test -p duthub-device（heartbeat 既有测试） |
| F43 | ○ | R4 | low | duthub-device/src/bus.rs:132-137 与 heartbeat.rs:105-110＋logs.rs:202-210,283-291 | now_unix_ms 同体实现两份（bus 私有/heartbeat pub）；SessionLogWriter 归档停写收尾（finish＋双 rotation++＋双 bytes=0）在 ensure_writing 与 split 两处重复 | now_unix_ms 收敛单一 pub(crate) 工具（跨 crate 归口与 F36 同一 audit::unix_ms）；logs.rs 提取 close_current_segments(&mut self) 供两处共用；MarkSync 同步成败语义不变 | equivalent | cargo test -p duthub-device（logs/heartbeat 测试） |
| F45 | ○ | R4 | medium | apps/desktop/src/workspace.rs:7043,6982-7007,6902-6928,6647-6659 | uniform_list 闭包每可见行调 compiled_highlight_rules()（重建 Vec&lt;CompiledRule&gt;：regex clone＋color String clone，一帧约 50 次；RefCell 缓存只避免重编译不避免重建）；render_find_bar 打开时每帧对全部注记行 to_lowercase() 计数（至多 4000 行×2 次分配） | render_terminal 入口算一次传入闭包与两个 item 函数（CompiledRule 内 regex 是 Arc，传引用零成本）；find 计数同帧算一次传入 | equivalent | cargo test -p duthub-desktop ＋ 终端高亮/查找手工验证 ＋ long_line_rendering_stays_cheap 护栏 |
| F46 | ○ | R4 | low | apps/desktop/src/main.rs:27,254-256 | 末尾 _keep_imports 占位函数仅为保住未使用的 Mutex 导入（frames/core_lines 实际是 AtomicUsize，Mutex 文件内无真实使用） | 删除 _keep_imports 与注释，导入收窄为 use std::sync::Arc | equivalent | cargo build -p duthub-desktop |
| F47 | ○ | R4 | medium | apps/desktop/src/agent_host/session.rs:194-200,16＋pages.rs:1411,1450 | push_entry 超限 entries.remove(0) O(n) 搬移：MEMORY_TRANSCRIPT_LIMIT=2000，AgentChunk 逐 token 推送（host.rs:664-672），对话高峰每条 chunk 触发 2000 元素 memmove，UI 每帧同锁读 | transcript 改 VecDeque&lt;TranscriptEntry&gt;，push_entry 用 pop_front（O(1)）；VecDeque 支持 len/迭代/Index，pages.rs lock＋迭代消费面无语义改动 | needs-regression | cargo test -p duthub-desktop（goose_binding.rs transcript_snapshot＋plan_badges.rs） |
| F48 | ○ | R4 | low | apps/desktop/src/highlight.rs:57-74,25-26 | row_segments 段合并比较每段分配临时 String（last.color == color.map(str::to_string)）；text.chars().collect() 每帧每行 Vec&lt;char&gt;（4.7k 字符行 ≈ 每帧约 19KB），高亮按行每帧运行 | :63 改 last.color.as_deref() == color（零分配）；chars().collect() 改 char_indices 双指针按字符偏移推进（或仅做 63 行修复——收益已占大头的合并路径） | equivalent | cargo test -p duthub-desktop（highlight 测试＋long_line 护栏 382-408） |
| F49 | ○ | R4 | low | apps/desktop/src/agent_host/model_env.rs:253-277,295-318 | normalize_binding 与 build_binding 重复实现同一归一化（default_base 回退/validate_model_base/空模型名拒绝/trim 两段各自维护），同一不变量两份实现，改一处漏一处的漂移风险 | normalize 收敛为 ensure_protocol_supported＋build_binding＋api_key 空检查三步；错误文案逐字保留（「模型名为空」「OpenAI Compatible provider 必须显式填写 Base URL」） | equivalent | cargo test -p duthub-desktop（goose_model_env normalize_binding_trims_and_rejects_incomplete_values） |
| F51 | ○ | R4 | low | apps/desktop/src/pages.rs:2624,2628-2629＋duthub-core/src/model.rs:181 | save_logs: Option&lt;bool&gt; 实为两态布尔：Some(true) 全仓永不构造，4 处消费全靠 unwrap_or(true)，写侧 (!enabled).then_some(false) 双重否定表达「关闭」 | duthub-core 改 plain bool（default_true＋命中 true 省略），JSON 形状与旧 None（缺省）/Some(false)（显式 false）完全等价；4 处消费去掉 unwrap_or。**等价前提已验证**（修订实测 `grep -n "saveLogs" docs/examples/settings.json docs/examples/duts.json` 零命中——两 examples 数据文件不含显式 true，往返形状不受影响）；预案：若未来 schema v2（F38）的数据文件引入显式 saveLogs:true，本条改法会与之撞线，届时并入 F38 的 v2 任务一并处理，不在本轮强推——**派发注意：roundtrip 敏感区** | needs-regression | cargo test -p duthub-core（roundtrip.rs 敏感区——坑 #19/#24 口径：既有断言不动，跑全量回归） |
| F52 | ✓ | R5 | medium | crates/duthub-mcp/Cargo.toml:10-17 | rmcp =3.10.0 开 6 feature，transport-child-process 在 src/ 零引用（仅 stdio_e2e.rs:8 与 http_auth_e2e.rs:829 用）——生产构建（含 desktop 依赖链）白白编译 child-process 传输；其余 feature src 均有使用 | 主依赖去掉 transport-child-process；[dev-dependencies] 加 rmcp 同版本带该 feature（Cargo dev-dep 特性并集语义），生产 feature 面最小化而测试不变 | equivalent | cargo build（desktop 链）＋ cargo test -p duthub-mcp（stdio_e2e/http_auth_e2e 仍过） |
| F56 | ○ | R6 | high | .github/workflows/release.yml:7-14 等 | CI 三缺口（修订实测坐实）：①无 pull_request/分支 push 触发——HEAD 版 release.yml 对 fmt/clippy/test/pull_request 的 grep 零命中，lint/test 步骤仅存在于工作树未提交的 gate job（工作树 release.yml:160/:164/:168 命中），在途改动零机器门禁；②fxc 探测块两 job 逐字重复（:36-45/:136-145）、Assert-Asset 与 release-gate.ps1 双实现；③gate 三步与 scripts/regression:8-15 手工同步 | 新增 ci.yml：**首选 push 触发（全分支）**——单人仓库以直推为主时 push 触发保证每笔落库都有门禁；pull_request 触发作为可选叠加（需要 PR 审阅流时再加），避免单人被迫走 PR 流程；fxc 探测与 goose 资产校验抽共享脚本；gate 改调与 regression 共用的单一脚本（windows-latest 自带 Git Bash）。在途改动落库前的过渡期：先以本地 `bash scripts/regression`（第 1-3 步）作为过渡门禁，落库后由 ci.yml 接管 | equivalent | CI 实跑一次验证 ＋ bash scripts/regression 本地过 |
| F57 | ○ | R6 | low | .github/workflows/release.yml:13 | tags 触发 ["0.01", "v*"] 中 "0.01" 是字面 glob（只匹配名为 0.01 的 tag），而实际 tag 与版本号不匹配——修订实测 `git tag -l` 输出仅 `v0.1.0`、workspace version 0.1.1——疑似 "0.*" 笔误，无 v 前缀 tag 不会触发发布构建 | 与发布流程约定对齐后改 "0.*"（若用无 v 前缀 tag）或删除仅留 "v*"（对外发布相关决策先问）——**派发注意：发布决策** | behavior-changing | 手工打测试 tag 验证触发 |
| F58 | ○ | R6 | low | .github/workflows/release.yml:156,133 | gate 每次运行 cargo install --locked cargo-cyclonedx@0.5.10 cargo-audit@0.23.0，rust-cache 不缓存 install 产物——缓存未命中时从源码编译两审计工具（分钟级），发布链时长波动 | 改 taiki-e/install-action（两者有预编译分发）或 actions/cache 显式缓存 ~/.cargo/bin（key 含版本号）；版本 pin 保持不变（pin 是做得好的部分） | equivalent | CI 时长对比 |
| F59 | ○ | R6 | medium | scripts/regression:11-15、release.yml:162-168、duthub-device/Cargo.toml:23,30 | real-hardware 门控的真机 I/O 主路径（real.rs 事件循环/TX 合并/重连/快照发布）不参与任何默认 clippy/test 门禁；唯一编译触达是 release 构建与手工 ignored live 测试，real.rs 内联测试在门禁中静默消失 | scripts/regression 增第 5 步 cargo clippy --workspace --all-targets --features duthub-device/real-hardware -- -D warnings（或至少 check -p duthub-device -p duthub-control --features real-hardware，不触硬件不占串口）；随 F56 的 ci.yml 同步加一步，与 release 构建编译口径对齐 | equivalent | 本地跑该 clippy 命令一次 ＋ bash scripts/regression |
| F61 | ✓ | R7 | low | duthub-mcp/src/server.rs:1299-1316 与 tests/dut_binding.rs:34-48 | Plan 模式 13 工具清单双份硬编码（测试 const 自述「与 server::plan_tool_allowed 对齐」，13 项同名同序已逐项比对一致）——新增 Plan 工具漏改测试则 13 工具全覆盖断言静默失效 | server.rs 定义 pub const PLAN_TOOLS，plan_tool_allowed 改 contains（3 个调用点均布尔判断、行为等价）；测试改引 duthub_mcp::server::PLAN_TOOLS；skill.rs/API_CONTRACT 文档面同步点不变（坑 #13 其余两处保留）；属测试引用同源化而非削弱断言——**涉及 tests/**，按 F67 区分规则属「引用同源化」，实施前显式批准** | equivalent | cargo test -p duthub-mcp（dut_binding 13 工具断言仍过） |
| F62 | ○ | R7 | medium | duthub-control/src/driver.rs:576-600 | real-hardware 门控下的 map_open_error 零测试覆盖：「denied/拒绝访问」文案嗅探启发式（区分端口被占用与端口不存在）是用户排查方向的防线，default 特性不编译该分支也无单测 | 加 #[cfg(all(test, feature = "real-hardware"))] 单测：直接构造 serialport::Error 覆盖 NoDevice＋denied→PermissionDenied、NoDevice 其余→NotFound、Io(kind) 各分支（开特性编单测不需硬件，坑 #52 口径）——**涉及 tests/**（新增测试），按 F67 区分规则属「新增」类，实施前显式批准 | equivalent | cargo test -p duthub-control --features real-hardware（不需硬件窗口，见清单） |
| F67 | ○ | R7 | low | AGENTS.md:138（测试红线节） | 红线只覆盖「为让门禁变绿」动机，未覆盖并行上下文/Agent 顺手重构测试（抽 helper、改断言「顺带」）；实施报告 :568 已有同类收口先例但测试文件无对应硬约束 | 硬约束节补一条并明确**禁区边界**：①禁止类——并行上下文/Agent 不得**修改或删除** tests/** 与内联 #[cfg(test)] 模块中的既有断言、验收标准与 fixture 输入（无论动机）；发现测试挡路一律视为实现问题；②允许类——**新增**测试用例、为既有测试补注释/动机说明、加运行时 env 分档开关（不改动既有断言）不属禁改，但涉及 tests/** 的结构性改动（抽 helper、移动文件、改引用路径）仍需**显式人工批准并回写记录**。受此约束的条目清单：F60/F61/F62/F63/F64/F66/F68/F69（F66 新增测试与 fixture、F62 新增单测、F64 加 env 开关、F68 改 ignored 测试读口、F69 补注释、F60/F63 抽 helper、F61 测试引用同源化）——每条实施前按其分类走对应流程 | equivalent | 纯文档改动，无代码影响 |
| F68 | ○ | R7 | low | duthub-device/tests/com8_io_latency_live.rs:24、com8_reset_boot_live.rs:26-29、real.rs:367 | ignored live/硬件测试环境定位两套并存：env-var 驱动（DUTHUB_TEST_SERIAL_PORT/DUTHUB_TEST_POSIX_SHELL/DUTHUB_TEST_CAPTURE_VIDEO/DUTHUB_TEST_SHELL_BIN）可移植，而 com8 两文件与 real.rs:367 硬编码 COM8/COM25/COM30，换机器/换口必须改源码 | 改 env-var 读取并回退现值（DUTHUB_TEST_CONSOLE_PORT 默认 COM8、DUTHUB_TEST_RELAY_PORT 默认 COM25），与 DUTHUB_TEST_* 家族统一；未设 env 行为不变，运行注释同步更新（AGENTS.md 并行协作节的复跑命令不变性一并核对）——**涉及 tests/**（改 ignored 测试文件的端口读取），按 F67 区分规则属「允许类＋结构性改动」，实施前显式批准；**需真机窗口**（见清单） | equivalent | 未设 env 复跑 com8 live（GUI 先释放 COM8/COM25） |
| F69 | ○ | R7 | low | apps/desktop/tests/lease_gui.rs:29-58、plan_badges.rs:1-20 | 两个测试以「读源码文本 grep 断言」替代行为断言（lease_gui 守坑 #43③ 双构造链同口径；plan_badges 断言 locale/源码徽标文案）——bin-only 代码的务实替代但对格式化/重命名脆弱，lease_gui 未注明「为何只能 grep」，易被后人当坏味道误删 | 主体动作（保留测试＋补动机注释）equivalent；建议中的**可选后续**——「main.rs 租约链构造移入 lib 可见模块后换行为断言」——触碰坑 #43③ 双构造链敏感区，届时按 needs-regression 对待并单独决策，本条不承担该后续 | equivalent（主体） | cargo test -p duthub-desktop（lease_gui/plan_badges） |
| F70 | ○ | R9 | low | duthub-core/src/audit.rs:221-230 | core 唯一 #[allow(clippy::too_many_arguments)] 盖 AuditSink::record 7 参数签名，调用方靠位置传参易错序（agent/credential_id/dut 三个 Option&lt;&str&gt; 相邻） | 改 pub fn record(&self, event: AuditEvent)（或增构造器），#[allow] 随签名消失；保留旧签名则至少改 #[expect]；调用方 14 处机械迁移（lease.rs audit_lease wrapper 已收敛大半） | equivalent | cargo test -p duthub-core ＋ 全仓 .record( 调用点编译验证 |
| F71 | ○ | R9 | low | 全仓 25 处 #[allow]（撰写时 grep 实测 25/0） | 25 处 allow 无一改 #[expect]（1.81+ 特性，MSRV 1.100 无兼容障碍）：dead_code 占位抑制在占位 API 接入使用后不会提醒清理，allow 清单只增不减；已有一处过期漂移（workspace.rs:1521 对已被调用的 focus() 误标）正是 expect 能自动暴露的 | dead_code 占位抑制改 #[expect(dead_code)]（F10/F11 清理后剩余的 theme.rs 调色板字段等适用）；too_many_arguments 持续满足类保持 allow 或随 F80/F70 一并处理；零行为影响，为 F55 的 lint 档位铺路 | equivalent | cargo clippy --workspace --all-targets |
| F74 | ○ | R11 | low | duthub-mcp/src/server.rs:385-396（对照 :342-383） | authorize_scoped_read 名字暗示「更严格作用域读」，实际在通用监听器是 no-op（仅 listener_dut.is_some() 才走 authorize），比 authorize_read（通用监听器要求 DutAction::Read Grant）更宽松——四个调用点（log_segments/log_segment_read/dut_files/dut_file_read）正是坑 #44 历史漏授权高危区，命名反直觉增加误用概率 | 不改行为只改签名表达：改名 authorize_listener_scoped_read＋doc 注释两分支语义（DUT 专用监听器＝强制绑 DUT 的 Grant 校验；通用监听器＝既有已认证全读契约，见 SECURITY.md）；或显式 scoped: bool 参数。契约本身有意为之（canary 注明不是要修的漏洞），仅防误读 | equivalent | cargo test -p duthub-mcp（security_canary 契约不变） |
| F75 | ○ | R11 | low | apps/desktop/src/agent_host/acp.rs:518-527,920-957,829-831、host.rs:495-504、listener.rs:77-81 | stderr 脱敏 secrets 只收集 env 中含 KEY/TOKEN/SECRET 的值，而每会话 MCP listener 只读 Token 不经 env、以 Authorization header 注入；token 格式为 `dth-` 前缀拼接两段 uuid（修订实测 listener.rs:78 `format!("dth-{}{}")`，发现集原文「dth-&lt;uuid&gt;×2」即指两段 uuid 拼接）；「bearer 」前缀规则可覆盖标准格式，但 agent 以非 Bearer 格式（token=…/url 参数）打进 stderr/goose 日志则无兜底。注：`dth-` 为既有代码内 token 前缀标识符，非 UI 文案/文档品牌字样，不在品牌命名规范约束范围 | host.rs spawn AcpClient 时把已签发 listener token 追加进脱敏表（spawn 增可选 secrets 参数或 AcpClient::add_secret），复用现有 ≥4 长度＋降序排序逻辑；纯增强，脱敏输出只多不少 | equivalent | cargo test -p duthub-desktop（goose_mcp_injection）＋ 脱敏单测 |
| F80 | ○ | R12 | low | apps/desktop/src/workspace.rs:3653,3835,3983＋mcp_runtime.rs:48,508 | 5 处 #[allow(clippy::too_many_arguments)]：text_field(7 参)/combobox(8 参)/select_field(7 参) 共享同组样式参数（id/value/placeholder/mono/p）；RuntimeMcpState::new(9 参)/spawn_mcp_runtime(8 参) 参数天然是一个装配上下文 | mcp_runtime.rs 引入 McpRuntimeConfig 一次性收口（可一并吸收 :502 EndpointSpec tuple）；workspace 编辑器类方法引入 FieldStyle{id,value,placeholder,mono,p} 参数对象，与 F78 的 TextLike 收敛同批 | equivalent | cargo test -p duthub-desktop -p duthub-mcp |
| F82 | ○ | R12 | low | apps/desktop/src/media_page.rs:685-820,865（另 :1186 同类） | device_field 单函数 136 行：on_click 闭包内嵌 open_dialog 回调再嵌 view.update 构建「DirectShow 同名设备去重＋解绑选项＋点击回填」三层逻辑；mode_field 在渲染路径放 unreachable!（MediaField 未来加变体即渲染线程 panic；:1186 Action::Start\|Stop 同类需一并核查） | 列表构建（728-780 含同名设备 endpoint 展开）提为纯函数 capture_device_choices(raw, audio)（可带单测，照 highlight.rs 纯函数模式，避坑 #36）；mode_field 的 match 加缺省分支返回空 Div 或 i18n 提示替换 unreachable! | equivalent | cargo test -p duthub-desktop ＋ 采集页手工验证 |
| F84 | ○ | R12 | low | apps/desktop/src/agent.rs:400-426,428-442,707-714,1043-1048,1368-1417 | 演示状态机转移散在渲染闭包：stop/pause/next 直接改 session.status，next 复制 start() 尾部逻辑（status=Running; last_tick=None），workflow 变更内联重置 4 字段；tick 用 steps()[step.min(4)-1] 依赖「先自增再索引」隐式不变量 | AgentState/AgentSession 补转移方法（stop/toggle_pause/advance/reconfigure_workflow），渲染闭包只调方法（照 start() 形状）；索引显式 clamp。演示状态机去留为既有产品决策，本条只重塑保留后的形状 | equivalent | cargo test -p duthub-desktop ＋ 演示面板手工验证 |
| F86 | ○ | R12 | low | apps/desktop/src/pages.rs:1156-1200 vs 2388-2465、2559-2571 vs 3250-3266 | 「Token 展示＋复制＋重新生成＋停用」行在 mcp_panel 与 a2a_dut_panel 重复两份；勾选块 check_box（静态）与 grant_check（可交互）重复两份（后者只多 id＋cursor_pointer＋hover）；endpoint_copy_row 已示范同族 URL 行的收敛形状 | 抽 token_display_row（参数化 toast 文案与额外动作）与统一勾选块（grant_check 增静态模式或 check_box 派生可交互版）helper；id 保持现值维持既有交互标识 | equivalent | cargo test -p duthub-desktop ＋ MCP/A2A 面板手工验证 |
| F87 | ○ | R12 | low | apps/desktop/src/agent.rs:631-643,1253-1258,1352-1358 | gpui_btn_icon 的 w 参数未使用（函数体首行 let _ = w 压警告）；has_events 计算后先 let _ = has_events 再于 agent-export 按钮实际使用——死参数与噪声语句 | 删 gpui_btn_icon 的 w 参数并同步两处调用（631-632）；删冗余 let _ | equivalent | cargo clippy -p duthub-desktop |

**P0 派发注意**（quick-win 计量口径：单条、单上下文的机械工时 <1 人天，含编译与针对性测试验证；**不含**外部决策等待、真机窗口与跨上下文协调。以下条目机械工时虽短，但带前置依赖，不宜与上面可直做的条目混批派发）：
- 需前置批准/决策：F12（草稿期显示对象需产品拍板）、F14（对外错误形状修正，需契约口径确认）、F51（roundtrip 敏感区，前提已验证仍需按红线回归确认）、F57（发布 tag 约定）、F61/F62/F67-关联测试条目（见 F67 的允许/禁止分类）；
- 需硬件/真机窗口：F07、F18（继电器，见「需硬件窗口清单」）；
- 需跨上下文协调：凡触及 workspace.rs/pages.rs/agent.rs 的条目（见「文件×条目归并表」）。

### P1 wave（1–3 人天/条，28 项）

| 编号 | 复核 | 维度 | 严重度 | 位置 | 现状证据 | 建议 | 行为风险 | 建议验证方式 |
|---|---|---|---|---|---|---|---|---|
| F04 | ○ | R1 | medium | duthub-core/src/lease.rs:372-1330（1794 行：生产 1332＋测试 460） | 安全核心巨石：锁→entry→expire_if_due→protected→inspection 的 5 行前奏在 9 个状态方法与 admit_write 四分支重复约 13 次；状态机/等待队列/admission 复核/错误类型四块内聚职责挤一文件（acquire 等待轮询与 grant_to_queue_head/grant_free 已收敛良好，拆分保持原状） | ①抽 lock_dut(&self, dut_id)（内部完成前奏与 Protected/NeedsInspection 拒绝，各方法保留 manual/holder 检查）；②按内聚拆 lease/{error,manager,admission}.rs，测试随迁断言不动；LeaseError 单点定义不变（坑 #43 三同步点不受影响） | equivalent | cargo test -p duthub-core（lease_invariants＋内联租约测试全套——安全关键路径） |
| F13 | ○ | R2 | high | duthub-mcp/src/state.rs:224-382、mcp_runtime.rs:147-150、server.rs:250-262,1338-1351 | McpState trait 全链 String 错误（发现集取证 38 处 Result&lt;_,String&gt;；修订实测单行正则 19 处——多行签名的 trait 方法未计入单行 grep，口径差异如实标注）：LeaseError 在 desktop 边界被格式化为「LeaseRequired/{code}: {message}」，server 层用 strip_prefix＋split_once 从字符串反向解析恢复语义，tool_call_denied 靠 message.starts_with 判审计口径——「语义化→String→字符串解析→语义恢复」往返链，任何格式微调静默破坏错误分类 | duthub-mcp 定义 StateError::{Lease(LeaseError), Approval, Unsupported(&'static str), NotFound, Internal(String)}，McpState 方法改返回 Result&lt;_, StateError&gt;；StateError::to_mcp_error() 单点产出 rmcp::ErrorData（对外 message 前缀「LeaseRequired/{code}: 」与 data.kind/code 形状不变，客户端契约不变）；lease_err()/state_err() 改枚举 match。对坑 #43 三同步点的准确表述：auth_http.rs 的 lease_error_code 穷尽 match 本已是编译器强制，保持不变；本条把其余两处「字符串前缀人工约定」（mcp_runtime 的格式化产出、server.rs 的前缀解析消费）改为对 StateError 的枚举 match，使「变体→码/语义」全链不再依赖跨文件字符串同步——新增 LeaseError 变体时由穷尽 match 编译器强制 | needs-regression | cargo test -p duthub-mcp 全套（http_auth_e2e 既有断言保护对外形状＋dut_binding＋security_canary——注意 http_auth_e2e.rs 本身在在途改动清单中，实施前以其落库后版本为契约基准，见 §⑤ 局限）＋ bash scripts/regression |
| F15 | ○ | R2 | medium | core（lease 20/agent_access 约 15）、device（heartbeat 32/lib 24/control 5/bus 5/demo 2）、desktop（workspace 15/side_panel 7/mcp_runtime 4/agent_host 46/media 12/pages.rs:1411,1450/agent.rs:482）合计约 192 处生产 lock().unwrap()（发现集生产代码估计；修订实测：排除 `*/tests/` 目录 237 处、全仓 261 处——两实测口径均含文件内 #[cfg(test)] 块，与发现集「仅生产代码」口径不完全可比，差异如实标注） | 锁中毒即 panic 防御缺口覆盖四 crate 与桌面层：任一持锁线程 panic 即中毒级联——HTTP handler 线程 panic 即端点静默死亡；新增两侧：transcript 锁读侧（pages.rs uniform_list 闭包每帧再锁）与 MODEL_TEST_RESULT 探针槽 | 三步收口：①unwrap_or_else(PoisonError::into_inner)（std 文档认可的中毒恢复语义，锁数据未破坏时继续用，非中毒路径行为完全等价；pages.rs/agent.rs 生产代码仅 3 处 unwrap）；②take().unwrap() 类有不变量点改 expect 带说明；③通知类失败可容忍入口 .lock().map(...)；可选全工作区 parking_lot::Mutex 同批评估；以 [workspace.lints] unwrap_used＋clippy.toml 放行清单固化门控（联动 F55） | equivalent | cargo test --workspace ＋ bash scripts/regression |
| F21 | ○ | R2 | low | duthub-adb/src/lib.rs:118-157 | android_preview 把探测超时/执行失败（两处 .ok()?）与「确认无 Android 能力」折叠为同一 None——getprop 探测超时静默表现为 shell-only 设备，与红线「探测错误与未检测到设备分别显示」（坑 #39 同族精神）相悖 | 返回类型改 DeviceResult&lt;Option&lt;AndroidPreview&gt;&gt;（None=确认无 Android、Err=探测失败），或失败路径 tracing::warn 让 UI/日志可区分两种情况 | behavior-changing | cargo test -p duthub-adb ＋ side_panel Android 预览手工验证 |
| F24 | ○ | R2 | medium | apps/desktop/src/agent_host/host.rs:461-541,729、model_env.rs:51-318、acp.rs:114-125,142 | agent_host 全链「语义变体外壳＋String 内芯」：prepare_and_run Result&lt;(),String&gt;、AcpError::Io/Protocol(String)、HostError 五变体全 String、七处 map_err(\|e\| e.to_string()) 拍平 io::Error/Rpc——source 链与错误类别（超时/权限/路径）全丢；AcpError 已实现 Error 却无 #[source] | 保持 Display 文案不变（UI transcript 同文本）：Io(std::io::Error) 用 #[from]、其余 String 变体补 #[source] Box&lt;dyn Error&gt;；prepare_and_run 改内部 WorkerError 枚举（Sandbox/Registry/Acp/Listener/Session），仅 transcript 边界 to_string；纯错误链保真 | equivalent | cargo test -p duthub-desktop（goose_lifecycle/agent_host 系列） |
| F27 | ○ | R3 | medium | duthub-device/src/control.rs:134-137,366,400,467,477,551-552＋lib.rs:102-106,415-417,438-452 | 每进程双内嵌 tokio runtime（DeviceService multi_thread×2 worker＋ControlService current_thread）＋ControlService 5 处 block_on 桥接同步入口；「单实例＋调用方不得在 runtime 上下文内调用」假设未显式化（当前调用方均不在 GPUI 主线程：hold 走 worker 线程、execute_by_id 仅 MCP 线程）；lib.rs:438-452 block_on 内仅 spawn 冗余 | 近期：ControlService 文档注释固化「单实例＋阻塞调用契约」；结构性：构造改可注入 tokio::runtime::Handle（缺省保留自建 current_thread，独立使用与测试不变），desktop 共享 DeviceService 的 runtime；lib.rs 首开失败回收 watcher 直接 runtime.spawn；不做进程级单 runtime 激进合并（保留服务间隔离性） | needs-regression | cargo test -p duthub-device（control/lease 既有测试）＋ MCP execute_by_id 路径手工验证 |
| F28 | ○ | R3 | medium | duthub-device/src/demo.rs:478-491＋lib.rs:496-503 | SessionCtx::Drop 在 DeviceService runtime worker（仅 2 个）内阻塞等日志写线程排空最长 3s，洪流积压占死 worker；close_session 兜底 abort 的 500ms 定时与 3s 排空上限不耦合，abort 掉未完成任务时 future 销毁同样在 worker 上执行这段阻塞 Drop | ctx 收尾移 spawn_blocking（排空语义与零丢失红线完全不变，仅不再占用 worker）；abort 兜底时长与排空上限对齐或改等 join 完成；写线程本身形状不动（零丢失红线）——**需真机窗口（见清单）** | needs-regression | cargo test -p duthub-device ＋ com8_reset_boot_live ignored 真机回归（日志零丢失核对；GUI 先释放 COM8/COM25） |
| F31 | ○ | R3 | low | apps/desktop/src/provider_models.rs:109-148,229-249 | save_provider 在渲染回调内同步执行 OS 凭据库 I/O（get/set/delete_key，Windows Credential Manager 繁忙时可达百 ms 级）与写盘（:137 save_model_providers）——全在 gpui 渲染线程；同文件 start_probe（:229-249）已示范 background_executor 异步形状，两条路径不对称 | keyring 读写与落盘移入 cx.background_executor().spawn（照 start_probe 形状），notice 以 ProbeState 同款 uuid 防陈旧模式回写；rollback 路径（保存失败回滚 keyring）一并移入后台任务保持原子顺序，核对保存后 draft_key_clear 时序 | needs-regression | cargo test -p duthub-desktop ＋ provider 页保存/回滚手工验证 |
| F40 | ○ | R4 | low | duthub-control/src/driver.rs:489-516,534-560 | read_echo 与 read_status 读循环形状重复（Instant 起点＋Vec acc＋[0u8;64] buf＋剩余时间 timeout 循环，差异仅帧分类函数与超时返回值 Ok(()) vs Ok(None)） | 提取「累计读取直到分类器命中或超时」公共循环（闭包分类、泛型返回）；ChecksumBad→Disconnected、Truncated continue 等分支语义随闭包表达；回显校验是安全关键路径，收敛后全量跑内联测试 | needs-regression | cargo test -p duthub-control --features real-hardware（driver.rs 22 个内联测试全量） |
| F41 | ○ | R4 | medium | duthub-device/src/heartbeat.rs:92-103,235-244,294/334/345,336-399 | 同一 dut_id 键五张 HashMap（configs/states/flights/next_due/alerts）各自独立加锁：tick 每 DUT 反复跨 2-4 把锁（flights 锁 3 次、next_due 锁 5 处），增删 DUT 靠 unregister/reset_schedule 人工同步 5 张表，漏一处即产生幽灵状态 | 收敛 Mutex&lt;HashMap&lt;String, DutHeartbeatEntry&gt;&gt;（entry 内聚 config/state/flight/next_due）；alerts 独立队列与 ticking 互斥保留；tick 内单 DUT 全部状态读取落一次锁 | equivalent | cargo test -p duthub-device（heartbeat 测试；坑 #28 口径：被租约拦截或未进入发送队列的探测不计设备丢联） |
| F44 | ○ | R4 | high | apps/desktop/src/workspace.rs:4496-4573,5774＋duthub-device/src/bus.rs:18,77-90 | render_root 每渲染帧调 refresh_snapshot：每 DUT device.read_log(&session, 0, usize::MAX) 全量克隆缓冲（SESSION_BUFFER_CAP=2000 条 envelope，每条带 String text）后仅取尾 150 行——每帧每 DUT 至多克隆并丢弃 1850 条；!dirty 分支每帧全量 dut.clone()＋resolve_relay 覆盖注册；构建全程持 snapshot mutex，与 MCP HTTP handler（mcp_runtime.rs:187/202）和 2s 落盘线程（:579）同锁互堵 | ①按每 DUT last_seq 水位增量重建（seq 未变跳过），或移入既有 50ms tick 做变更检测；②register_dut 仅在 duts.dirty 或 DUT 集变化时执行（register_dut 本身幂等覆盖）；③新快照移锁外构建、锁内仅字段 swap；**必须先落 F30**（理由见「依赖视图」） | needs-regression | cargo test -p duthub-mcp（stdio 快照内容）＋ serial_read seq 游标连续性（workspace.rs:4538 注释记载的历史「数据冻结」坑——「seq > 窗口末尾 续读恒为空、会话超过 150 行后数据冻结」，系代码注释记录的在途教训、非 AGENTS.md 编号坑——不得复发）＋ bash scripts/regression |
| F50 | ○ | R4 | medium | apps/desktop/src/pages.rs:245,383,535,837,1088,1250,2291（7× DutConfig 克隆）、:1815（agents 注册表整克隆）、:2923（agentAccess 整克隆） | 连接页 7 个面板各自 w.duts.selected().cloned() 整克隆 DutConfig（9 个嵌套段、数十 String），输入框每键一渲染即克隆 7 份；agent 注册表与 AgentAccess 设置每帧整体克隆 | render_connection 入口（:190-192 已有 selected().is_none() 早退）一次快照以 &DutConfig 传参给 7 个面板（快照在栈帧上与 &mut w 无借用冲突，监听器闭包按需 clone 字段）；agent_registry_section 循环体无 &mut w，改直接迭代免整表克隆；agentAccess 克隆因与可变调用交错保留每帧一次 | equivalent | cargo test -p duthub-desktop ＋ 连接页键入手感验证 |
| F53 | ○ | R5 | medium | 根 Cargo.toml:42-58（仅 6 条目）＋各成员 Cargo.toml 20 余处散写 | workspace.dependencies 不收敛：tokio 5 成员 6 处散写 feature 各异（core 两处）；tracing/serde_json 在已有 workspace 条目下仍裸声明（serde_json 存在 workspace=true 与直写双轨）；uuid/base64/sha2/tempfile/async-trait（5 处）/axum/alacritty_terminal/reqwest/subtle/cap-std/chrono 跨成员重复散写——无单一版本事实源 | 跨成员共享依赖全部移入 [workspace.dependencies]，成员 dep.workspace=true；tokio 统一版本条目、features 按需保留（core 仅开 sync 的最小化是正确实践，不强行拉齐）；desktop serde_json 直写改 workspace 引用；纯声明收敛不改锁定版本（tokio lock 单版 1.54.1） | equivalent | cargo check --workspace ＋ Cargo.lock diff 确认仅声明变化 |
| F54 | ○ | R5 | medium | duthub-core/Cargo.toml:19,17-18＋duthub-mcp/Cargo.toml:25＋Cargo.lock（883 包/86 组多版本） | 同功能多版本并存：reqwest 0.12.28(core)＋0.13.5(mcp)、base64 0.22.1＋0.23.1、sha2 0.10.9＋0.11.1、getrandom 0.2.17＋0.3.4＋0.4.3——0.23/0.11/0.3 由钉死的 rmcp 3.10 内部树引入，同一构建编译两份 base64/sha2/reqwest | 三步收敛：①core getrandom 0.2→0.3（getrandom::fill，唯一调用点 agent_access.rs:261）；②base64→0.23、sha2→0.11 对齐 rmcp 树；③core reqwest 0.12→0.13 对齐（core 仅 llm.rs 用，json feature 保留、rustls feature 名改 rustls-no-provider）；windows crate 4 版（0.57/0.58/0.61/0.62）受 gpui-kit 钉死约束不动 | needs-regression | cargo test -p duthub-core（llm.rs:330-391 TCP fixture）＋ -p duthub-mcp（http_auth_e2e） |
| F55 | ○ | R6 | medium | 根 Cargo.toml:14-18、仓库根、duthub-core/Cargo.toml:5、release.yml:27,131,81-85 | 治理配置缺位五项（六个审计域独立命中）：①无 [workspace.lints]/clippy.toml（锁内快照拷出、lock().unwrap()、result_large_err 等红线无机器门禁）；②无 rust-toolchain.toml（CI 两处 @stable 浮动，clippy 警告集随 runner stable 漂移）；③无 [profile.release]（LTO/strip 全默认，crt-static 仅 CI RUSTFLAGS，本地与 CI 产物不一致）；④无 deny.toml；⑤rust-version="1.100" 仅 core 声明无继承，MSRV 靠 cargo 取最大值隐式生效；另 AGENTS.md 写「遵守工作区的 rustfmt 配置」但配置文件不存在 | ①建 [workspace.lints]（先收编现有 -D warnings 口径，再逐步加正确性 lint：unwrap_used 限库 crate 非测试代码、disallowed-methods 治 lock().unwrap() 族联动 F15、result_large_err；测试豁免分层），成员统一 lints.workspace=true，必要时补 clippy.toml disallowed-methods；②rust-toolchain.toml 钉 1.100.0（升级作为独立任务）；③[workspace.package] 补 rust-version="1.100"，各 crate 继承；④[profile.release]：lto="thin"＋codegen-units=1＋strip="symbols"（panic=abort 需产品决策不默认；release 验收前必 taskkill /F /IM Duthub.exe——链接失败 os error 5 时旧 exe 静默续跑）；⑤补 deny.toml（联动 F54 多版本收敛）；⑥AGENTS.md rustfmt 措辞修正 | equivalent | cargo clippy/test 全绿 ＋ bash scripts/regression ＋ release 产物尺寸对比 |
| F60 | ○ | R7 | medium | duthub-mcp/tests 5 文件＋server.rs 内联测试（修订实测 5 文件共 2481 行＋server.rs 内联测试段约 290 行 ≈ 2770 行；发现集取证口径约 2900） | 四类辅助逐份手写：raw HTTP/1.1 探测客户端 3 份（listener_lifecycle.rs:56-92 与 security_canary.rs:91-120 的 post_initialize 近逐行重复）；AccessContext 三件套构造 13 处（修订实测 grep 计数 dut_binding 1＋security_canary 4＋http_auth_e2e 3＋listener_lifecycle 1＋server.rs 内联测试区 4＝13，与取证一致）；「bind listener→set_nonblocking→tokio::spawn axum::serve」样板 ≥4 处（注释「tokio from_std 要求非阻塞」出现 4 次）；initialize body 字面量×5；McpState 替身 2 份（RecordingState/FixtureState） | 建 tests/common/mod.rs：probe_http 客户端（统一超时/重试语义，以 lifecycle 的 initialize_status 重试形状为基准）、access_ctx(enabled, grants) 构造器、spawn_router 辅助、INITIALIZE_BODY 常量；替身不强合并（替身本质是行为契约），仅共享 DutSummary/DutInfo fixture 构造；纯测试代码重组，断言与验收标准逐条不变——**涉及 tests/**（结构性改动），按 F67 区分规则需显式批准 | equivalent | cargo test -p duthub-mcp 全套 |
| F63 | ○ | R7 | medium | device/control.rs MockBackend×2/FailingBackend/dut_with_relay×2＋desktop tests StubState×3（手工 impl McpState 约 90 行样板）/access_context×5/temp_dir×8/wait_until×3＋dev-dep 双向环 | 测试助手跨 crate 大量复制；control dev-dep duthub-device 与 device dev-dep duthub-control real-hardware 的双向测试环，正是「ControlService＋租约＋假驱动」组合 fixture 无处共享的症状 | 对照 codex-rs core_test_support 模式：device/control 侧 duthub-core 加 test-support feature（或独立 duthub-core-test-support crate）承载 MockBackend/FailingBackend/dut_with_relay/租约组合构造器；desktop 建 tests/common/mod.rs（#[path] 或 mod 声明共享）；坑 #52 的 dev-dep feature 声明不动，迁移保持各处断言等价、不删既有测试——**涉及 tests/**（结构性改动），按 F67 区分规则需显式批准 | equivalent | cargo test --workspace |
| F64 | ○ | R7 | medium | apps/desktop/tests goose 6 文件（lifecycle/path_root/binding/mcp_injection/model_env/agent_registry） | 6 个测试启动真实 goose-acp.exe（20MB 内嵌资产，bundle::resolve 每次安装/复算完整 SHA-256），等待窗口 30–120s，全部在 cargo test --workspace 默认门禁内且无 env 开关；仓库已有分档意识（DUTHUB_GOOSE_LIVE=1 门控真实模型调用、com8 live 用 #[ignore]、job_kill 注明不依赖真实 goose）但未覆盖这组——改 UI 的快速回归与发布级全量回归目前只能同一档 | goose 组测试开头加运行时 env 检查（DUTHUB_SKIP_GOOSE_E2E=1 时 eprintln 跳过），scripts/regression 支持 --fast 透传该变量（日常开发快档、阶段收尾仍全量）；cargo test 无 per-test cfg(env)，运行时 early-return 是等价最小改法；默认档保持现状不减功能——**涉及 tests/**（新增 env 分档开关、不改既有断言），按 F67 区分规则属「允许类」，实施前知会即可 | needs-regression | cargo test --workspace 默认档全量 ＋ --fast 档时长对比 |
| F65 | ○ | R7 | medium | duthub-core/tests/lease_invariants.rs:126,133,244,250、desktop/tests/lease_gui.rs:72、lease.rs:21,476-627（unix_ms 时钟源）、duthub-device/src/control.rs:845-850,894-930,1404-1409 | 租约 TTL/等待超时测试依赖真实 sleep（110ms 等 40ms TTL 过期、80ms、started.elapsed()≥70ms 断言等待窗、两处逐字重复的 deadline 轮询 sleep(5ms)＋收尾 sleep(120ms)）；lease.rs 时钟源是 wall clock unix_ms()——慢 CI 调度延迟压缩 sleep 边界可能 flaky，且 lease 是安全关键路径（GUI 会话租约、Agent 拦截） | lease.rs 的 unix_ms() 收敛为可注入时钟（内部时钟 trait 或 cfg(test) 可替换钩子，生产路径默认实现不变），TTL/等待测试改假时钟推进消除 sleep；control.rs 的 deadline＋sleep 轮询抽 wait_for(timeout, predicate) helper 消除两处重复；lease 是安全红线路径，改动后必须跑全套（不需硬件窗口，见清单） | needs-regression | cargo test -p duthub-core（lease_invariants）＋ -p duthub-desktop（lease_gui）＋ 既有租约测试全套 |
| F66 | ○ | R7 | medium | duthub-device/src/term.rs:431-593（18 个合成序列测试）＋tests/fixture-logs/＋demo.rs:1-7＋logs.rs（.bin 原始字节链路） | term.rs 18 个快照测试全手写合成转义序列，缺「真实串口原始字节流录制→回放→快照序列断言」的 ref-test 层；录制基础设施已有雏形（tests/fixture-logs/ 文本日志、会话 .bin 已存原始字节且 com8_reset_boot_live 已验证零丢失）但未接进快照测试；坑 #46 类（裁尾随空白/多字节边界）恰是真实 ANSI/CR/LF 混合流才易踩的形状 | 按 Alacritty tests/ref.rs 模式补录制回放层：从真机会话 .bin 录 1–2 个 fixture（含转义序列/CR/LF 混合/光标移动），term.rs 增「字节流→Processor::&lt;StdSyncHandler&gt;→快照序列」回放断言（序列哈希或逐帧对照）；坑 #46 的四个单测保持不动并作为该层子集；fixture 归档 tests/fixture-logs/——**涉及 tests/**（新增测试与 fixture），按 F67 区分规则属「允许类」，实施前知会；fixture 录制需真机窗口（见清单） | equivalent | cargo test -p duthub-device（term.rs 全部）＋ com8 live 录制产物归档 |
| F72 | ○ | R10 | medium | duthub-device/src/term.rs:57-77＋real.rs:96-151 | 终端宿主用 VoidListener 把 alacritty_terminal 全部驱动事件丢弃：设备侧 ANSI 查询（DSR CSI 5n/6n、DA1）的应答永远发不回设备——真实串口上 vim/htop 等查光标位置的程序将挂起或渲染错乱（alacritty_terminal 0.26 源码：identify_terminal/device_status 均产 Event::PtyWrite；real.rs parser.advance 后无应答回写路径） | 实现桥接 EventListener：Event::PtyWrite 携带的应答字节交回 real.rs 事件循环串口 TX 路径（与用户写入同 task 保序，可在 parser.advance 后立即写出）；ColorRequest/Title 暂维持默认；补真机 TUI（vim/htop）回归用例——**需真机窗口＋验收环境说明（见清单）** | behavior-changing | cargo test -p duthub-device ＋ com8_io_latency_live/com8_reset_boot_live ignored 真机回归（GUI 先释放 COM8/COM25）＋ vim/htop 手工验收 |
| F73 | ○ | R10 | medium | apps/desktop/src/side_panel.rs:782-791 | ADB Shell 终端未虚拟化：每帧对至多 400 行 for row in &screen.rows[first..] 全量重建 div 且逐行 row.text.clone()——主终端已按坑 #49 完成 uniform_list 迁移（workspace.rs:6892），此处是同一性能问题的漏改点，持续串口输出每帧重建 400 个元素 | 迁移到与 workspace.rs:6892-6934 相同的 uniform_list＋UniformListScrollHandle 形状（ScreenSnapshot 已是 Arc 共享、行高统一 h(row_height)、near_bottom 贴底跟随保留） | needs-regression | cargo test -p duthub-desktop ＋ ADB Shell 页连续输出手工验证 |
| F76 | ○ | R12 | low | duthub-mcp/src/server.rs（1766 行；分段经修订实测补全：参数结构体 42-240、核心与授权辅助 267-544、#[tool] 工具方法 546-1297、白名单/审计辅助与 PLAN_ROUTER 构建＋#[tool_handler] ServerHandler call_tool 入口 1299-1476、内联测试 1477-1766——原 1378-1476 段实测为 audit_tool_result 尾部＋PLAN_ROUTER static＋ServerHandler impl） | 工具方法/参数 DTO/授权审计回执辅助/内联测试混单文件（#[tool] 属性修订实测 29 处；发现集取证约 27，在途漂移±2——发现集所述「40 个工具同构样板」为初扫把读/写工具与参数变体一并计入的粗估口径，复核以 #[tool] 方法为准）；「40 工具同构样板宜宏收敛」经核实不宜——各工具授权入口四种（authorize_read/authorize/authorize_any_read/authorize_any_write）＋lease_identity/审批校验组合各异，宏抹平差异恰是坑 #44 漏授权温床 | 保持 #[tool] 方法不抽象，仅按组拆文件：args.rs（参数结构体）、audit.rs（audit_tool_result/summarize_tool_result/tool_call_denied）、receipts.rs（WriteReceipt 表）、core 授权辅助独立模块；#[tool_router] 聚合与 PLAN_ROUTER/ServerHandler 留顶层；R11 敏感面（authorize* 四入口、lease_identity）原地保留并加 doc 强调差异 | needs-regression | cargo test -p duthub-mcp 全套（tool_router 跨模块聚合需一次回归验证） |
| F78 | ○ | R12 | medium | workspace.rs:3565-3651,3738-3815,3044-3491,265-319 等＋pages.rs:4582-4584 | 草稿/编辑器体系三重结构性冗余：①line_editor（InputState）与 multiline_field（TextareaState）同构约 50 行、text_editors/textareas 两个同形 HashMap 并行；②In 枚举 46 个变体（修订实测 awk 块计数；发现集取证口径 50+）联动 5 处 match（key/input_value/input_set/editor_scope/multiline）——坑 #41/#45「草稿臂不成对」是该形状的结构性产物；③调用方成对书写 13+ 处且有现场反例：pages.rs:4583 In::A2aPort 传模型值 a2a.port.to_string() 绕过草稿，键入 '9999x'（解析失败模型停 9999）后下一帧被 workspace.rs:3577-3589 set_value('9999') 覆盖回跳（input_set 草稿臂 2026-11-18 已补齐，显示源仍绕过草稿） | quick-win 先行：In::A2aPort 调用点改传 input_value(In::A2aPort) 独立修复回跳；结构性：①抽内部 trait TextLike 收敛为单个 bound_editor&lt;T&gt; 泛型装配；②照 workspace.rs:3686 path_input 自绑定形状给 text_field/multiline_field 增自绑定入口（内部取 input_value(id)，显示源唯一化后坑 #41 类回跳结构性消失），pages.rs/agent.rs 调用点迁移；③纯文本草稿类收敛声明式字段描述表，数字解析保持 Baud 式「草稿保原文、解析成功才写模型」不变 | needs-regression | quick-win 后手工键入 '9999x' 验证不回跳 ＋ 既有 input_value/input_set 配对单测回归（坑 #41/#45 红线区） |
| F79 | ○ | R12 | medium | workspace.rs:6787-6837,509-519＋side_panel.rs:1040-1091 | 终端键→字节编码三处重复：workspace render_terminal 的 on_key_down 内联 match（enter/backspace/tab/escape/方向键/delete/typed_char）、side_panel key_bytes 完整函数、bracketed-paste 包裹两份；行为有细微差异（workspace 回车/退格读 settings.serial 的 enter_bytes()/backspace_byte()，side_panel 硬编码 \r/0x7f，方向键前缀逻辑两份相同） | 抽共享助手 terminal_key_bytes(event, KeyMap{enter_bytes, backspace_byte}, application_cursor, bracketed_paste, clipboard_text)（放 components.rs 或提升 duthub-device::term 与 arrow_escape_prefix 同层）；两调用点传各自 settings 口径（workspace 传 settings.serial，side_panel 传默认 CR/DEL），差异显式化 | needs-regression | cargo test -p duthub-desktop ＋ 终端回车/退格/方向键/粘贴手工回归（坑 #29：方向键和粘贴跟随设备 ANSI 模式，勿用固定序列覆盖） |
| F81 | ○ | R12 | low | apps/desktop/src/workspace.rs:1125-1505 | Workbench::new 单函数 380 行内联五条子系统运行时接线：租约管理器/AccessContext（1257-1273）、ControlService 与三个 log sink（1274-1297）、心跳引擎与持久化钩子（1298-1322）、AlertCenter（1323-1332）、MCP 状态与监听器装配（1333-1503）——UI 状态初始化与运行时装配混杂，构造参数经 StartupContext 二次解包 | 抽 assemble_runtime(device, data_dir, work_dir, settings, cx) -> RuntimeAssembly{control, heartbeat, alerts, access_ctx, mcp_state, …}，new 只留 UI 状态与 sessions 初始化；与 crates 内既有构造器（DeviceService::custom 链）对齐；行为等价、装配顺序不变；属 F77 拆分的先行子项 | equivalent | cargo test -p duthub-desktop ＋ 启动路径手工验证 |
| F83 | ○ | R12 | medium | apps/desktop/src/pages.rs（修订实测 4942 行；发现集取证 4704，并行工作流在途漂移＋238） | 单文件承载连接管理、设置、串口设置、烧录工具、Agent 接入五块页面，仅靠三处分节注释分隔，共享 helper 仅约 10 个小函数 | 按既有分节拆 pages/{connection,settings,serial_settings,agent_access,agent_page}.rs，共享 helper（panel_shell/field/settings_panel/setting_row/endpoint_copy_row/check_box/grant_check）留 pages.rs 或 pages/shared.rs，对外 pub 签名不变（接线零改动）；遵循「先模块内拆文件再评估升 crate，警惕过早模块化」；与 F77 不同文件可并行，实施须协调窗口（AGENTS.md 并行协作红线）；**拆分会使指向 pages.rs 的其余条目行号失效，实施时机见「先修后拆」决策** | equivalent | cargo test -p duthub-desktop ＋ bash scripts/regression |
| F85 | ○ | R12 | medium | apps/desktop/src/agent.rs:1191-1250＋components.rs:194-203 | 演示面板 prompt 手写 div＋on_key_down 编辑器：逐键 push/pop、逐帧由 session.prompt 重建子节点，无选区/撤销/IME 组合路径（typed_char 只消费 key_char 单字符）——现状即违反 AGENTS 红线「设置输入使用 gpui-kit Input/Textarea：逐帧写回会清空选区、撤销记录和输入法组合」；同族真路径 agent_session_panel 已用 kit Input 实体 | 换 kit Input/Textarea 实体＋每 DUT 草稿（参照 pages.rs:1519-1521 与 workspace.rs:5331 agent_prompt_state 既有机制），Enter 发送/Shift＋Enter 换行在按键分支实现；中文 IME 输入随之可用 | needs-regression | cargo test -p duthub-desktop ＋ 中文输入/Enter 发送真机手工回归（坑 #26 焦点绑视口） |

### P2 structural（分阶段，2 项）

**F77 ○ R12（high）——workspace.rs 巨石拆分**

- 位置：`apps/desktop/src/workspace.rs` 全文件（修订时刻实测 8713 行；struct Workbench 86 个字段（修订实测 awk 计数，发现集取证口径 88）、109 个 pub/pub(crate) 方法、380 行构造函数）。
- 现状证据：连接管理、A2A、Agent 注册表与宿主、终端渲染、告警弹窗、弹窗/首页/菜单、事件吸收、持久化全部内联单文件；审计期间行数三次漂移（基线 8064 → 批材料 8691 → 撰写时 8713，另一工作流同文件并行修改），是并行冲突热区。
- 建议分阶段拆分，不减少任何功能。第一步模块内拆文件——gpui 的 impl Workbench 可合法跨文件，按内聚分组 workspace/ 子模块：input.rs（In/Dd/BoundEditor/input_value/input_set/editor_scope/combobox/select_field）、connection.rs（连接管理＋每 DUT MCP/A2A 端点＋save_duts/persist_bindings/validate_bindings）、agent_panel.rs（Agent 注册表/宿主/模型绑定）、terminal.rs（终端渲染＋会话状态条＋键盘路径）、events.rs（absorb_envelope/absorb_device_events/refresh_snapshot）、modals.rs（open_modal/render_modal_parts/告警弹窗）；先不动 pub(crate) 可见性（警惕过早模块化）。第二步再评估把 agent_access/agent_host 状态升级为独立 crate（对照 Zed subsystem-per-crate）。实施须与在途工作流协调窗口（AGENTS.md 并行协作：同一文件修改只归一个上下文）；**拆分时机遵循「先修后拆」决策（见实施顺序）**。
- 行为风险：equivalent（功能不减）。
- 验证方式：每阶段 `cargo test -p duthub-desktop` ＋ `bash scripts/regression` ＋ lease_gui/plan_badges grep 契约测试。

**F38 ○ R4【仅登记·不排期】——未接线 schema 被 roundtrip 测试数据钉死（版本任务，本轮与后续 P2 排期均不实施）**

- 位置：`crates/duthub-core/src/model.rs:377-392,494-601`；`docs/examples/settings.json`、`docs/examples/duts.json`；`crates/duthub-core/tests/roundtrip.rs:23-27`。
- 现状证据：两块未接线 schema 无法直接删——AgentSettings 的 legacy 字段 fast/secondary/turns/parallel/compare/approve 零代码消费者（仅 provider/slow 有 workspace.rs:1222/5410/5442 与 agent.rs:889 消费），HostConfig＋DutsFile.hosts 无 schema 外消费；但 docs/examples 两文件含全部这些键，roundtrip 对 examples 做 original==re 逐字段 JSON 相等断言，把往返形状锁死。
- 建议本轮不做收窄——收窄等于改 roundtrip 测试输入（红线：既有测试不许删改来让门禁变绿）。登记为 settings/duts schema v2 版本任务：届时同步删除字段、更新 docs/examples 两个数据文件与 DATA_MODEL.md，fast/turns/parallel/compare/approve/secondary/hosts 一并清退；在此之前保持现状（反序列化仍读入，仅无人消费）。F51 的 JSON 等价性预检（examples 当前不含 saveLogs）已确认本轮不受此块影响；若 v2 引入显式 saveLogs:true，F51 撞线预案见其表行。
- 行为风险：behavior-changing（任何提前删除都会使旧配置往返丢键）。
- 验证方式：v2 实施时跑 `cargo test -p duthub-core`（roundtrip）＋ DATA_MODEL.md 对齐审查。

### 实施顺序与依赖视图

**实施顺序建议（批次划分）**：
1. **批 1（门禁与红线，约束先行）**：F56＋F59＋F67 同期落地——后续所有改动才有机器门禁与测试红线兜底；随附 F52/F58/F57（CI 内整改与发布口径）；
2. **批 2（错误与锁中枢）**：F14/F26（quick-win 先行）→ F13（StateError 收敛一并归入）；F15（lock 收口）；F16/F17/F19/F20；
3. **批 3（清理与收敛大宗）**：P0 其余可直做条目按「文件×条目归并表」分上下文推进；F04/F21/F22/F23；
4. **批 4（结构项）**：F27/F28/F40/F41/F44/F47/F50/F63/F65/F66/F72/F73/F76/F77/F78/F79/F81/F83/F85——以依赖视图与「先修后拆」为准。

**依赖视图**（完整 DAG，替代散落在各表行备注的顺序提示）：

| 先行 | 后继 | 理由 |
|---|---|---|
| F67 | F60/F61/F62/F63/F64/F66/F68/F69 | tests/** 约束先落地（禁区边界公布后），测试侧条目按其区分规则走批准/知会流程 |
| F56 | F52/F59/F64 | feature 最小化、real-hardware 编译口径、goose 快档开关随 ci.yml 同批接线 |
| F14、F26 | F13 | quick-win 先对齐两条审批链与文案单点，StateError 收敛后一并归入枚举 |
| F30 | F44 | 先给落盘线程补停止信号（小改动、同时锁定该线程此后行为不变），再做 F44 的锁外构建/增量改造——两者触碰同一把 snapshot 锁，顺序颠倒会造成两次触碰同一锁的重叠冲突面 |
| F81 | F77 | 装配抽取是巨石拆分的先行子项 |
| F78 与 F80 | 互为同批 | TextLike 收敛与 FieldStyle 参数对象同批实施（同组样式参数） |
| F10、F11 | F71 | 死状态/死代码清理后，剩余占位再改 #[expect] |
| F15 | F55 | 先收口 lock().unwrap()，再上 unwrap_used/disallowed-methods 门禁（否则门禁先行会把在改代码卡死） |
| F36、F43 | F54 | 时间戳/hex/归档收尾收敛与依赖版本对齐同批验证 |
| 先修（指向 workspace.rs/pages.rs 的 P0/P1 条目） | 后拆（F77/F83） | 见「先修后拆」决策 |

**「先修后拆」决策**（巨石拆分与行号失效的顺序问题）：指向 workspace.rs（F10/F29/F44/F45/F78/F79/F80/F81）与 pages.rs（F50/F51/F78/F86）的 unconfirmed 条目行号在 F77/F83 拆分后全部作废。默认走法＝**先修后拆**：先按现行行号完成上述小改动，再执行 F81→F77/F83 拆分（拆分时按拆分后新位置搬迁，行号一次归零）；反之先拆会使每个小修都要在拆分 PR 上重定位、放大冲突面。若在途工作流长期占用文件导致小修窗口不足，才考虑「先拆后修」，且须把该偏差记入协调窗口决策。

**文件×条目归并表**（并行协作红线：同一文件的修改只归一个上下文）：

| 文件 | 条目 | 归并要求 |
|---|---|---|
| apps/desktop/src/workspace.rs | F10、F29、F44、F45、F77、F78、F79*、F80、F81 | 同一上下文串行或协调窗口；F77 排最后（先修后拆） |
| apps/desktop/src/pages.rs | F50、F51、F78*、F86 | 同一上下文；F78 横跨 workspace.rs＋pages.rs 两文件，须与 workspace 组协调 |
| apps/desktop/src/agent.rs | F12、F33、F84、F85、F87 | 同一上下文串行 |
| apps/desktop/src/agent_host/（host/acp/model_env/session/listener） | F24、F47、F49、F75（F15 全仓收口覆盖其中 lock 点） | 同域串行 |
| apps/desktop/src/mcp_runtime.rs | F14、F30、F80 | 同域串行 |
| crates/duthub-mcp/src/server.rs | F34、F61、F74、F76 | 同一上下文串行；F76 拆分排最后 |
| crates/duthub-mcp/src/state.rs＋snapshot.rs | F13、F26 | 同批（F26 随 F13） |
| crates/duthub-mcp/src/transport.rs | F01、F25 | 可合并一批 |
| crates/duthub-control/src/driver.rs | F07、F18、F39、F40、F62 | 同一上下文串行 |
| crates/duthub-device/src/lib.rs | F09、F22 | 可合并一批 |
| apps/desktop/src/media_page.rs | F82 | 独立文件，无归并冲突 |

注：F15 为全仓横切（约 190–260 处，见 §⑤ 口径表），不纳入单文件归并；F79 横跨 workspace.rs 与 side_panel.rs 两文件。

**主题×编号交叉表**（按主题推进与按 P 级推进是两种走法，可任选其一为主、另一为校验）：

| 主题 | 直接对应编号 | 说明 |
|---|---|---|
| ① 收敛重复、单一事实源 | F01、F02、F03、F08、F09、F10、F11、F17、F26、F34、F36、F37、F39、F43、F45、F46、F49、F61、F80、F86、F87（另 F60/F63 测试助手收敛、F22 映射收敛、F23 文案单点化） | 死代码/死状态/未接线抽象 10 条＋重复/双写类＋收敛类；绝大多数 equivalent，可按文件归并表分上下文并行 |
| ② 语义化错误与安全边界推广 | F05、F13、F14、F15、F16、F18、F19、F20、F21、F22、F24、F65、F74、F75 | 以 F13 为中枢（依赖视图），agent_host 在途期收敛成本最低（F24/F49/F75）；F65 属安全路径测试加固 |
| ③ 治理配置与 CI 门禁 | F52、F53、F54、F55、F56、F57、F58、F59、F64、F67、F68、F70、F71 | 批 1 优先落地；F38 仅登记不在此列 |
| 其余条目 | F04、F06、F07、F12、F25、F27、F28、F29、F30、F31、F32、F33、F35、F38、F40、F41、F42、F44、F47、F48、F50、F51、F62、F66、F69、F72、F73、F76、F77、F79、F81、F82、F83、F84、F85 | R3/R4/R10/R12 的形状与结构类条目，以 §③ P 级主线＋依赖视图推进为主，不必强行归入主题 |

**需硬件窗口/授权清单**（排期时单列，避免混入可直做批次）：

| 条目 | 依赖 | 前置 |
|---|---|---|
| F07 | 继电器实动（如需实动验证） | 仅通道 1 已授权；GUI 释放 COM25 |
| F18 | 继电器时序（REPLY/PULSE 常量） | 同上 |
| F28 | com8_reset_boot_live 真机回归 | GUI 释放 COM8/COM25（串口互斥打开） |
| F68 | com8 live 复跑（env 化验证） | GUI 释放 COM8/COM25 |
| F72 | 真机 TUI（vim/htop）验收 | 验收依赖 DUT shell 具备可跑 TUI 的环境（busybox/完整 rootfs）；现有 COM8/COM25 授权范围（通道 1 继电器往返、启动洪流、键入延迟基线）**不覆盖 TUI 场景**，需额外真机窗口与人工验收 |
| F62 | 不需硬件：单测开 real-hardware 特性编译即可（坑 #52 口径） | — |
| F65 | 不需硬件（时钟注入属测试改造）；但属安全路径，全套租约回归必须 | — |
| F66 | fixture 录制需真机会话（一次） | GUI 释放 COM8/COM25 |

**排期口径与总量估算**：
- 计量单位：quick-win＝<1 人天/条、wave＝1–3 人天/条（单上下文机械工时，含编译与针对性测试验证）；P2＝分阶段、不设单条人日上限（F77 按 6 个子模块阶段推进，F38 仅登记）。
- 总量：57×(<1)＋28×(1–3) ≈ **57–141 人天**区间（不含 P2、外部决策/批准等待、真机窗口、并行协调与回归失败返工）。单人按日历天推进；多人/多上下文并行必须遵守文件×条目归并表与依赖视图。
- 87 条是按批次分摊收尾（批 1→4 主线），非一次收尾；批次内按文件归并再拆上下文。

---

## ④ 与既往报告的关系

**编号撞名警示**：本报告的 R1–R12（审计维度）与 F01–F87（本轮发现编号）与既往五轨体系**同名不同物**——docs/reviews/AGENT_INTEGRATION_BEST_PRACTICE.md §11 的五轨编号也叫 R1-R12/F1-F17/R2-1..15。本文引用五轨时一律加「五轨」前缀（如「五轨 R2-3」）；凡未加前缀的 R/F 编号均指本报告体系。

**本轮增量**：12 维度现代化视角——架构卫生（R1）、错误/并发形状（R2/R3）、分配与数据结构（R4）、依赖与门禁治理（R5/R6）、测试策略（R7）、最新特性（R9）、终端宿主正确性（R10）、MCP/Agent 安全面（R11）、UI 状态与巨石拆分（R12）。与既往报告的问题域（MCP 功能缺陷、Agent 集成实现、架构功能评审）正交：本轮不重复任何已报问题，仅在其边缘发现新的工程卫生问题（例：F61 发现 Plan 工具清单双份，是坑 #13「清单多处同步」的新实例，而非 MCP 测试报告中的任何既有条目）。

**明确不含（保留原编号体系，不作为本轮发现重复报告）**：

- **docs/MCP_TEST_REPORT.md**：P1-014（serial_write 审批令牌兼容路径失效）、P1-015（多 Agent 并发写字节交错）、P1-027（adb_list_devices 恒空）、P1-028（adb_shell 审批未接线）、P2-019 族（等待超时文案当持有者名）及 P2/P3 其余 30 余条（含 RX 跳号、60s 内存尖峰）。
- **docs/ARCHITECTURE_REVIEW.md** 5 条：DUT 编辑仅回填继电器端口致保存重建清空绑定；按端口派生 ID 与持久化 ID 双标识；MCP 直签一次性令牌；烧录/ADB 结果槽互覆；进程超时不含子进程读线程。
- **43c58dc:docs/GOOSE_AGENT_FEASIBILITY.md**：agent.rs 演示状态机去留、A2A Python 原型处置、出网边界等 12 风险——去留是产品决策；本轮 F84/F85 只重塑保留后的形状，不复议去留。
- **docs/reviews/AGENT_INTEGRATION_BEST_PRACTICE.md §11** 五轨：R1-R12/F1-F17/R2-1..15 均「设计裁决待验证」——本轮核对实现与裁决一致性时不把裁决本身复述为新发现（如 F09 的 DeviceService::new 死代码与 ADR-WORKDIR 互相印证，不构成对裁决的异议）。
- **docs/reviews/AGENT_IMPLEMENTATION_REPORT.md §6** 延期 16 项：legacy model/&lt;model name&gt; keyring 代码删除、A2A crate、watchdog 等——本轮 F08 仅要求给 duthub-agent 空壳补占位注释，不重开延期决策。

**有意为之、勿当缺陷（本轮未列为发现，相关建议以不破坏为前提）**：

- 双工作目录解析器并存（ADR-WORKDIR，T24 前接受）——F09 的处置建议以此为前提；
- gpui-kit 0.9.1 钉死版本——F54 的 windows crate 多版本收敛因此明确不动；
- 串口日志 std mpsc 写线程（零丢失红线）——F28 只把 Drop 收尾移入 spawn_blocking、F43 只收敛归档收尾重复，均不破坏该形状；
- TX 合并 absorb 形状（坑 #48）——本轮无改动建议；
- 快照裁尾随空白规则（坑 #46）——F66 的 ref-test 层以该规则四个单测为子集；
- 租约错误变体三同步点（坑 #43）——F13 保留 auth_http.rs `lease_error_code` 穷尽 match 为唯一「变体→码」映射，F04 拆分不影响该单点定义；
- roundtrip 既有测试不可改（坑 #19/#24）——F38 因此登记为版本任务而非本轮收窄，F51 的 JSON 形状等价性以此为准则；
- 既有 E2E/回归测试不许删改来让门禁变绿——F60/F61/F63 等测试侧改动均为「断言逐条保持不变」的重组/同源化，且 F67 已把该红线扩展为「改/删既有断言禁止、新增/注释/分档开关允许但结构性改动需批准」的明确边界。

---

## ⑤ 附录：审计方法与局限

### 审计方法

- **流程**：12 维度 × 多批扫描材料 → 逐条独立复核（复核结论并入发现集 status 字段：verified 11 / unconfirmed 76）→ 按本报告结构成文 → 按独立读者意见修订（补严重度/评级口径/依赖视图/归并表，复测摘要级数字）。本报告只依据发现集与下述自证命令，未新增代码级发现。
- **编号体系**：发现统一编号 F01–F87（按 R1→R12 维度分组、组内保持发现集原序）。发现集内部引用的任务编号（T01–T82）已按映射改写，映射表**仅收录发现集文本中实际引用的 21 个 T 编号**：T01→F13、T05/T10→F15、T06→F55、T07→F60、T44/T08→F63、T21→F36/F43、T23→F54、T24→F70、T47→F77/F81、T48→F44、T49→F78、T53→F10、T55→F80、T62→F31、T68→F32、T78→F56、T80→F58、T81→F59、T82→F64。其余 T 编号在本轮发现集中无引用、不对应任何 87 条之一，其完整任务清单不在本报告范围。
- **原始材料位置**：发现集 JSON（87 条，含 dimension/where/what/evidence/status/severity/behaviorRisk/effort 字段）与 T01–T82 任务清单均为审计工作流的内部产物，由工作流脚本持有并随任务输入传递，**未提交入仓库**；追溯某条发现的完整证据链需向工作流发起方索取该产物，本报告各条「现状证据」列是其可核验的转写。

### 自证命令与实测结果（均在仓库根执行，只读；两轮：成文阶段＋修订阶段）

1. `git rev-parse HEAD && git rev-parse --abbrev-ref HEAD` → `fe24fc6c8b4346cbfb33755b31f4638c98a87def` / `feat/agent-integration`（与审计基线一致）。
2. `git status --porcelain | wc -l` → 首轮 **44**（发现集取证口径 39——在途继续波动）；修订阶段 `git status --porcelain -- crates/duthub-mcp/tests/http_auth_e2e.rs` → `M`（**该验证基石文件在在途改动清单中**，见局限）。
3. `git diff --stat` → 修订阶段实测 27 files changed, 3699 insertions(+), 221 deletions(-)（28 行＝27 文件＋汇总行，确认取证材料「前 1500 字」截断未截掉任何文件行）。
4. `wc -l`（修订时刻）：workspace.rs **8713**、pages.rs **4942**、server.rs **1766**、lease.rs **1794**、state.rs 382、term.rs 755、agent.rs 1688、media_page.rs 1386、heartbeat.rs 827。其中 pages.rs 较发现集取证的 4704 行 **+238**（并行工作流在途改动），其余与发现集一致。
5. `git tag -l` → 仅 `v0.1.0`（支撑 F57）。
6. `ls docs/ARCHITECTURE.md docs/ARCHITECTURE_REVIEW.md` → **两文件均在位**（支撑 F02 注与 §④——前者架构文档、后者既往评审报告）。
7. `grep -n "数据冻结" apps/desktop/src/workspace.rs` → :4538 `// （seq > 窗口末尾）续读恒为空、会话超过 150 行后数据冻结。`（支撑 F44 出处注：代码注释，非 AGENTS.md 编号坑）。
8. `grep -c "SAFETY:" agent_host/job.rs media.rs` → **15 / 2**，与生产 unsafe 计数一一对应（支撑 R8 评级；语义级逐块审查未做）。
9. `git show HEAD:.github/workflows/release.yml | grep -nE "clippy|cargo test|cargo fmt|pull_request"` → **零命中**；工作树 `grep -nE …` → 命中 :160（fmt）/:164（clippy）/:168（test）（支撑 F56「gate 仅在未提交工作树」）。
10. `grep -n "saveLogs" docs/examples/settings.json docs/examples/duts.json` → 零命中（支撑 F51 等价前提）。
11. `grep -c "#\[tool" crates/duthub-mcp/src/server.rs` → **29**（发现集取证约 27，在途漂移±2）；`sed -n '1376,1480p'` 确认 1378-1476 段为 audit_tool_result 尾部＋PLAN_ROUTER＋ServerHandler impl（支撑 F76 分段补全）。
12. `grep -oE "Result<[^;()]+, String>" crates/duthub-mcp/src/state.rs | wc -l` → **19**（单行口径；发现集取证 38 处含多行签名，见口径表）。
13. `grep -rn "lock().unwrap()" --include="*.rs" crates apps | wc -l` → **261**（全仓）；加 `--exclude-dir=tests` → **237**（见口径表）。
14. `grep -nE 'Err\("' crates/duthub-mcp/src/state.rs` → 10 行默认实现文案；`grep -c "stdio 快照模式为只读" snapshot.rs` → 6（支撑 F26 实测计数 16）。
15. `awk '/pub enum In/,/^}/' apps/desktop/src/workspace.rs` 变体计数 → **46**；`grep -cE "^    (pub|pub(crate)) fn " workspace.rs` → **109**；`awk '/pub struct Workbench/,/^}/'` 字段计数 → **86**（支撑 F77/F78 修订值）。
16. `wc -l crates/duthub-mcp/tests/{http_auth_e2e,dut_binding,security_canary,listener_lifecycle,stdio_e2e}.rs` → **2481 合计**（支撑 F60 修订值 2770≈2481＋290）。
17. `grep -c "AgentAccessRuntime::from_settings"`（五文件）→ 1+4+3+1+4＝**13**（与 F60 取证一致）。

**未跑任何 cargo 构建/测试/clippy**——撰写约束为只依据发现集写作；构建类验证留给各项实施时按「建议验证方式」列执行。

### 量级数字复测口径表（凡进入 §①/§② 的数字逐条标注）

| 数字 | 发现集取证值 | 修订实测 | 结论 |
|---|---|---|---|
| McpState trait String 错误处数 | 38（含多行签名） | 单行正则 19（`Result<[^;()]+, String>`） | 口径不同：保留取证 38 并注明实测 19 仅计单行，实施时以 trait 方法全量核对为准 |
| 生产 lock().unwrap() | 192（生产代码估计） | 全仓 261／排除 tests 目录 237（均含文件内 cfg(test) 块） | 同向佐证「量级约 190–260」；正文取「约 192（发现集口径）＋实测口径注」 |
| 能力不可用重复文案 | 约 14 | state.rs 10 处默认实现＋snapshot.rs 6 处 override＝16 | 实测 16，正文更新 |
| MCP 测试助手行数 | 约 2900 | 5 文件 2481＋server.rs 内联约 290 ≈ 2770 | 正文更新为约 2800 |
| In 枚举变体 | 50+ | awk 块计数 46 | 正文更新为 46（取证 50+，口径差注明） |
| Workbench 字段 | 88 | awk 计数 86 | 正文更新为 86（取证 88 注明） |
| Workbench 方法 | 109 | 109 | 一致 |
| AccessContext 构造 | 13 | 13 | 一致 |
| #[tool] 方法 | 约 27 | 29 | 在途漂移±2，正文注明 |
| unsafe（生产） | —（本轮零发现） | 17 处；SAFETY 注释 15/15＋2/2 一一对应 | R8 评级依据 |
| #[allow] / #[expect] | 25 / 0 | 25 / 0 | 一致 |
| 巨石行数 | workspace 8713 / pages 4704 / server 1766 / lease 1794 | 8713 / 4942（+238） / 1766 / 1794 | pages.rs 在途漂移，正文已标 |

表内其余细节计数（如「post_initialize 3 份」「temp_dir×8」「十处前奏重复」）仍为发现集取证值，未逐一复测——实施时先按行号核对现场（见局限）。

### 局限

- **未跑构建与测试**：所有「编译器会拦」「测试会炸」类断言（如 F13 依赖 http_auth_e2e 既有断言、F51 依赖 roundtrip、F52 依赖 dev-dep feature 并集语义）基于代码形状与既有测试清单推断，未经本轮构建验证。
- **验证基石自身的在途状态**：F13/F14/F34/F52/F54 等条的验证方式依赖 http_auth_e2e 既有断言，而该文件在在途改动清单中（修订实测 git status＝M）。实施前以其落库后版本为契约基准；「既有测试不许改」红线约束的是实施者不得为过门禁改测试，不约束在途工作流对自身的开发改动，但 F13 实施时不得顺手修改该文件。
- **工作树在途波动**：pages.rs 在审计期 +238 行、`git status` 计数 39→44、#[tool] 27→29——报告行号为取证时刻值，实施前必须重新定位；F77/F83 两项巨石拆分须与在途工作流协调窗口（AGENTS.md 并行协作红线），并遵循「先修后拆」决策。
- **unconfirmed 76 条未经独立复核**：实施时先按「现状证据」行号核对现场再动手；发现集已记录多处 ±2/±3 行漂移实例（如 workspace.rs 批材料 6646→实测 6648、「数据冻结」注释 4534-4536→实测 4538），行号允许小幅漂移。
- **R8 评级的残余缺口**：SAFETY 注释齐全为计数级实证（15/15＋2/2），语义级「每条 SAFETY 说明是否成立」未逐块审查——实施涉 unsafe 文件（agent_host/job.rs、media.rs）改动时顺带补齐语义审查。
- **标杆对照以实践模式为单位**（crate 粒度、lint 门禁、ref-test、审批范式等），未逐项核对这些仓库当前版本的具体实现细节；「Bun AI 实践」「大厂案例」引用自审计维度定义，未做二次溯源；评级旁证强度以此为准（§② 已公布可复算的判定规则予以补偿）。
- **F29 复核修正记录**：原评估「删一个函数、改一处调用」的关键证据被代码否定（scan_usb_devices 签名无 cx，需给调用链穿线）——按保守原则保留 unconfirmed，实施时按修正后的改动面评估。
- **F57 的 tag 约定与 F12 的草稿期显示**等条目带外部决策依赖（发布约定、产品拍板），本报告不代做决策，实施前先问（AGENTS.md：越界决策先问）。

### 标杆来源列表

| 来源 | 引用内容 |
|---|---|
| [Zed](https://github.com/zed-industries/zed) | 160+ crate 平铺、lint 当平台/正确性约束、GPUI 双执行器与「主线程神圣」、锁内快照拷出（sync/make_content）、ColorRequest 查询应答保序教训、subsystem-per-crate |
| [rust-analyzer](https://github.com/rust-lang/rust-analyzer) ／ [matklad 笔记](https://matklad.github.io/) | premature pessimization 清单；警惕过早模块化 |
| [wezterm](https://github.com/wezterm/wezterm) | VecDeque 行复用 |
| [Alacritty](https://github.com/alacritty/alacritty) | tests/ref.rs 录制回放；Grid Storage 单块连续 |
| [ripgrep](https://github.com/BurntSushi/ripgrep) | release-lto profile 档位 |
| [codex-rs](https://github.com/openai/codex) | CodexErr 语义分类驱动重试、协议类型独立、工具面 allow/deny 与 schema 大小保护、审批 human-in-the-loop、core_test_support 助手共享、倾向新增 crate |
| [Google Rust Style Guide](https://google.github.io/styleguide/rustguide.html) | 错误上下文、显式导入、小作用域 |
| [Copilot Runtime Rust 迁移](https://github.blog/ai-and-ml/generative-ai/migrating-the-github-copilot-runtime-to-rust-using-copilot/) | 行为契约先行、既有 E2E 是迁移门禁、小步可验证 |
| [Bun](https://github.com/oven-sh/bun) | AI 代理参与开发下的仓库卫生与门禁回路（引用自审计维度定义） |
| [TiKV](https://github.com/tikv/tikv) | CI 零警告 |
| [s2n-quic](https://github.com/aws/s2n-quic) | deny panic 类 lints |
| [Firecracker](https://github.com/firecracker-microvm/firecracker) | undocumented_unsafe_blocks lint |
| [GreptimeDB](https://github.com/GreptimeTeam/greptimedb) | 错误上下文包装模式 |
| [Clippy 官方文档](https://doc.rust-lang.org/clippy/continuous_integration.html) | CI 固定工具链建议 |
| [std 文档](https://doc.rust-lang.org/std/sync/struct.PoisonError.html) | PoisonError::into_inner 中毒恢复语义 |
| [rmcp](https://github.com/modelcontextprotocol/rust-sdk) | 3.10.0 官方 SDK 用法（feature 门控、Json&lt;T&gt; IntoCallToolResult） |

*注：标杆引用以实践模式为单位，未逐项核对各仓库当前版本的行级实现；alacritty_terminal 0.26 的宿主范式细节（EventListener/同步输出超时/快照拷出）以本仓 AGENTS.md 坑 #21/#22/#46 与本地 registry 源码核验记录为准。*
