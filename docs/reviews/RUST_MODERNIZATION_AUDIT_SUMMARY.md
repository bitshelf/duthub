# duthub Rust 全库现代化审计 — 交付摘要

> 全量报告见 [RUST_MODERNIZATION_AUDIT.md](RUST_MODERNIZATION_AUDIT.md)（主表 F01–F87 连续编号）。
> 本文件是交付时的结论摘要（2026-11-21，工作流 `dwfrun-2dab9613`，纯只读审计，未改任何代码）。

## 数字总览

| 项 | 值 |
|---|---|
| 审计对象 | 52,559 行 Rust 工作区（7 crates + 2 apps，edition 2024，含在途未提交的 agent_host/ACP 新代码） |
| 审计域 | 9 个：core / device / mcp / 硬件边界 / workspace.rs 巨石 / pages+agent / 外围+agent_host / 工作区治理 / 测试回归 |
| 去重后发现 | **87 条**（高严重度 4、medium 39、low 44；quick-win 57） |
| 独立复核 | 12 条事实性结论（死代码/无调用点/重复实现类）由全新上下文复核员逐条核实，**11 条 verified** |
| 报告质量关 | 独立读者 36 条审读意见修订（含「量级数字复测口径表」13 行实测勘误） |
| 路线图 | P0=57 / P1=28 / P2=2，总量估算 **57–141 人天** |

## 高严重度 4 条

1. **MCP 错误语义往返链**（F26 族，needs-regression / wave）——`McpState` 全链 `Result<_,String>`（state.rs），desktop 边界格式化为 `LeaseRequired/{code}: {message}` 字符串，server 层再 `strip_prefix + split_once` 反向解析恢复语义；tool_call_denied 还靠 `starts_with` 判审计口径。建议 `StateError` 枚举单点收敛，坑 #43 的三同步点从「人工同步」变成「编译器强制」。
2. **workspace.rs 8713 行巨石**（structural / equivalent）——struct Workbench 88 字段、109 个 pub 方法、380 行构造函数，连接管理/Agent/终端/告警/持久化全内联；审计期间实测 +22 行，是并行冲突热区。分阶段拆分策略：先模块内拆文件（input/connection/agent_panel/terminal/events/modals），再评估升 crate。
3. **渲染帧全量克隆快照**（needs-regression / wave）——`render_root` 每帧对每个 DUT `read_log(0, usize::MAX)`（bus.rs 全量 clone 缓冲后仅取尾 150 行，每帧每 DUT 最多克隆并丢弃 1850 条 envelope），且构建过程持 snapshot 锁与 MCP HTTP handler、2s 落盘线程互堵。建议按 last_seq 水位增量重建 + 锁外构建。
4. **CI 门禁缺口**（equivalent / quick-win）——release.yml 无 `pull_request` 触发；fmt/clippy/test 三步门禁只存在于工作树未提交的 gate job（HEAD 零命中）；gate 与 scripts/regression 手工双维护。

## 独立复核确认的快赢亮点（verified）

- **`ApprovalManager.audit` 无界 Vec 只写不读**：`audit_tail` 全仓零调用，issue/consume 每次 push 永不清理 → 长期运行的 MCP 进程内存泄漏（approval.rs:68,106-108,170-181）。
- **`VoidListener` 丢弃 alacritty 全部驱动事件**：设备侧 DSR/DA1 查询的应答（CSI 5n/6n）永远发不回串口 → 真机 vim/htop 会挂起或渲染错乱（term.rs:57-77）。
- **driver.rs 常量不变量矛盾**：`REPLY_TIMEOUT=30s < PULSE_MAX_MS=60s`，>30s 的合法脉冲必报 Timeout 而脉冲实际仍会正常执行（driver.rs:32,34,292）。
- **死代码族**（均可安全删除，equivalent）：
  - `serve_http_blocking` / `serve_http_listener_blocking`（transport.rs:171-209，全仓零调用方，且与 spawn 版六步前处理逐行重复）
  - `AppCommand` / `AppService` / `AppEvent`（M0 演示骨架，ipc.rs + event.rs，注释还写着误导性的「M1.1 将替换」）
  - `AgentAccessStore` + `JsonAgentAccessStore`（未接线的 DAO 预留，两头不沾）
  - `DeviceService::new`（零调用，装配逻辑与 desktop main.rs 逐行重复双份维护）
  - workspace.rs 五处死状态/死分支（`agent_model_*` 只写不读、`In::Prompt`/`ModalKind::RenameDut` 无构造点、`p()` 无调用、`focus()` 的 allow 误标）
- **pages.rs:4583 A2aPort 传模型值绕过草稿** → 坑 #41 显示侧残余真 bug：键入 `9999x`（解析失败模型停在 9999）下一帧被覆盖回跳。
- **每会话只读 Token（`dth-…`）不在 stderr 脱敏表**：agent 以非 Bearer 格式把 Token 打进 goose 日志则无兜底（acp.rs:518-527）。
- **rmcp `transport-child-process` feature 生产零使用**（仅 2 个测试文件用）→ 迁 dev-dependencies，生产构建省编译。
- **CI tag 触发 `"0.01"` 笔误 glob**（实际 tag 是 v0.1.0，疑似想写 `"0.*"`）。
- **`TrySendError`→`DeviceError` 映射 4 处逐字重复**（lib.rs:304/561/611/693）+ 3 处退化映射把队列满折叠成断线。
- **Plan 工具 13 项清单双份硬编码**（server.rs `matches!` + dut_binding.rs const）——新增工具漏改测试会静默失效。

## 其他结构性发现（摘要）

- 草稿/编辑器体系三重冗余：line_editor/textarea 双实现 ~50 行同构、`In` 枚举 50+ 变体联动 5 处 match、13+ 处调用点成对书写（坑 #41/#45 是该形状的结构性产物）——建议 `TextLike` trait + 自绑定入口 + 声明式字段描述表。
- 心跳引擎同一 dut_id 键的五张 HashMap 各自独立加锁，tick 反复跨 2–4 把锁（heartbeat.rs:92-110）。
- `McpState` 14 处重复的「能力不可用」错误文案；40 个工具方法的授权入口四种形状互异——**不宜宏收敛**（宏抹平差异恰是坑 #44 漏授权的温床），改为按组拆文件。
- 测试助手跨文件大量复制（MockBackend ×2+1、StubState ×3、access_context ×5、temp_dir ×8）——对照 codex-rs core_test_support 模式收敛。
- 全仓 25 处 `#[allow]`、0 处 `#[expect]`（MSRV 1.100 无兼容障碍），含 1 处已过期误标——expect 化让占位 API 接入时编译器自动提醒。
- 无 `[workspace.lints]`/clippy.toml/rust-toolchain.toml/[profile.release]/deny.toml 五项治理配置缺位；reqwest 0.12+0.13、base64/sha2/getrandom 多版本并存。
- 真机 I/O 主路径（real.rs，real-hardware 门控）不参与任何默认 clippy/test 门禁——建议 regression 加第 5 步带 feature 的 clippy/check。
- agent_host/ 全链错误是「语义变体外壳 + String 内芯」（七处 `map_err(|e| e.to_string())` 拍平 source 链）。
- `SessionCtx::Drop` 在仅 2 个 worker 的 runtime 内阻塞等待日志排空最长 3s（洪流积压时占死 worker）——移入 `spawn_blocking`，零丢失语义不变。

## 验证与诚实声明

- ✅ 抽查 3 条发现对照源码全部命中：`serve_http_blocking` 零调用方、workspace.rs:1522 `focus()` 的 allow 误标（6648 实际调用）、`TrySendError` 4 处逐字重复映射。
- ✅ 既往 5 份报告（MCP 测试 39 条、架构评审 5 条、goose 12 风险、五轨 R1-R12、实施延期 16 项）的已报问题仅在 §④ 作排除引用，无重复报告。
- ⚠️ 未覆盖（报告 §⑤ 如实声明）：未跑 cargo clippy/test/build（避让在途实施工作流 + 磁盘约束）；真机行为与 live 测试未验证；**审计期间工作树持续被另一工作流修改**，部分行号已漂移（报告内逐条标注漂移观察）；docs/rust-reference 排除目录与 goose-acp.exe 二进制未审计。

## 审计方法（可追溯）

- 基线：`git rev-parse HEAD` 固定 + `git status --porcelain` 快照（审计 HEAD=fe24fc6 + 在途未提交改动一并审计）。
- 12 维度评分基准（R1–R12）蒸馏自 4 份标杆调研（~135 条带来源条目）：Zed/rust-analyzer/matklad、wezterm/Alacritty、ripgrep/codex-rs、Copilot Runtime 迁移 + Bun AI 编码实践 + 大厂案例（Microsoft/Firecracker/s2n-quic/Pingora/Volo/GreptimeDB/TiKV）+ Google Rust Style Guide。
- 拓扑：9 分域审计（并行）→ 共享分诊员跨批去重（87 条）→ 12 条事实性断言独立复核（11 confirmed）→ 撰写 → 独立读者冷读修订（36 条意见）→ 交付。

## 建议的下一步

1. 等收口工作流（`dwfrun-ae1591b0`）结束后再动代码——审计与实施不得与在途工作流同文件并行（AGENTS.md 并行协作）。
2. P0 快赢按批挑：死代码删除批（零行为风险）→ 依赖/门禁批（workspace.lints + rust-toolchain.toml + CI 触发器）→ 真机相关批（VoidListener 桥接、REPLY_TIMEOUT）需按 AGENTS.md 实动授权流程。
3. 4 条高严重度中 MCP 错误链与快照克隆属于「需回归验证」档，建议单独开实施方案后再动手。
