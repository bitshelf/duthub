# Tasks: docs 目录清理与代码互证

**Input**: `specs/001-docs-cleanup/spec.md` + `specs/001-docs-cleanup/plan.md`（终审处置清单：删除 0 / 更新 15 / 保留 7 / 未实现报告 13）

## Format: [ID] [P?] [Story?] Description

- **[P]**：可与同 Phase 其他任务并行（不同文件、无依赖）。
- **[US1]…[US5]**：对应 spec.md 用户故事，供追溯。
- 每项完成前必须跑该条目的复核命令；全部改点**先读原文定位再改**；行号以 plan.md「实测锚点」为准，发现漂移以实测为准并在本任务下登记。
- **执行状态**：T001–T905 全部完成；O1–O6 为 owner 裁量项，保持未勾选、不顺手定案。执行由工作流分包完成（分类/归并/规格由子代理，收尾轮由主会话手工补验）。

## Phase 1: Setup（基线快照）

- [x] T001 基线核对与快照：`git status --porcelain` 记录 HEAD 与改动基线；`ls` 核对 15 份更新文件 + 7 份保留文件 + 13 份报告文件全部在位；确认 `docs/examples/`（settings.json、duts.json）、`docs/duthub.html`、`docs/rust-reference/` 在位。任何缺件 → 停止并上报，不得静默跳过。**（2026-11-23 执行：35 份全部在位；基线快照存 /tmp/docs_baseline.txt；ADR mtime 与 plan.md 锚点一致）**

**Checkpoint**: 基线就绪。

## Phase 2: User Story 1 — 权威状态口径唯一 (Priority: P1) 🎯

- [x] T101 [P] [US1] `docs/decisions/ADR-11-OS-ISOLATION.md`
  - :3 状态行（实测「状态：已接受（Accepted）」）对齐 ADR_SIGNOFF.md §7（:122 实测「ADR-2/8/9/11 为 Proposed」）的 **Proposed** 口径，消除两说；最终措辞加「owner 复核」注记。
  - 保留能力事实注记：os_acl.rs:78 `harden_private_dir`、crates/duthub-core/src/restart.rs 退避均在位（本轮实测）——能力实现 ≠ 产品签字。
  - 复核：`grep -n "已接受" docs/decisions/ADR-11-OS-ISOLATION.md` 状态行零自称。
- [x] T102 [P] [US1] `docs/decisions/ADR_SIGNOFF.md`
  - §6（:110 附近）把「ADR-0/2/5/6/7/8/9/10」整体称旧 v5 计划未签字，与 §7 :122 单列现行「ADR-2/8/9/11 为 Proposed」编号冲突 → 分开表述「旧 v5 计划编号」与「现行 docs/decisions/ ADR-2/8/9/11」两组。
  - §7 把 ADR-9 概括为「StateDb 上移 core」与实文不符（ADR-9 :27-35「明确不实现（deferred）：持久化 StateDB」、:47「仅内存是基准」——行号为简报给定，执行时读原文核对）→ 概括对齐实文。
  - §7「Agent Card 1.0.1」与 ADR-8 实文（A2A v1.0 规范 §8 的 Agent Card 内容最小化）不符 → 措辞对齐。
  - 复核：`grep -n "StateDb 上移" docs/decisions/ADR_SIGNOFF.md` 零命中；§6/§7 编号互斥消除（人工通读）。

**Checkpoint**: 唯一事实互斥消除（US1 可独立验收）。

## Phase 3: User Story 2 — 失效引用清零 (Priority: P2)

- [x] T201 [P] [US2] `docs/decisions/ADR-WORKDIR.md`
  - §4 验收命令引用 `crates/duthub-acp`（本轮 ls 实测不存在）→ 改指 `apps/desktop/src/agent_host/acp.rs`（实测在位）。
  - 行号按当前树修正：`ensure_portable_work_dir` store.rs:79、`resolve_work_dir_in` :295、`ensure_work_dir` :341（本轮 grep 实测；如文中有引 `ensure_work_dir_in` 为 :320 一并核对）。
  - 复核：`grep -n "duthub-acp" docs/decisions/ADR-WORKDIR.md` 零命中。
- [x] T202 [P] [US2] `docs/GOOSE_AGENT_FEASIBILITY.md`
  - §5.1 契约 2「仅绑 127.0.0.1（assert_loopback）」失效：assert_loopback 全仓 0 命中（实测），现行 transport.rs:385 `assert_bind_allowed`（LAN+Bearer 合法），已被 ADR-2 取代 → 改写或加取代注记（注记注明权威出处 ADR-2）。
  - interop_a2a_server.py 与 docs/A2A_MCP_INTEROP.md 引用（两者实测不存在；grep 实测命中 :76/:77/:198/:240，简报另记 :226/:264 以实测为准）→ 删除或标注存档。
  - §1.5/§4「Rust 侧仅配置模型+设置页壳（a2a_running: false）」过时（workspace.rs 该符号 0 命中实测；crates/duthub-a2a 回环运行时已落地，lib.rs 头注「W3 最小面」实测）→ 更新表述。
  - 行号七处按当前树修正（均本轮实测）：export_skill_md workspace.rs:3199、STEP_INTERVAL agent.rs:35、OFFLINE DEMO agent.rs:506、快照 sleep mcp_runtime.rs:599、apr- approval.rs:143、enforce_origin_validation transport.rs:189、Plan 白名单 server.rs:1320-1337。
  - 复核：`grep -n "interop_a2a_server\|A2A_MCP_INTEROP\|assert_loopback" docs/GOOSE_AGENT_FEASIBILITY.md` 零裸命中（存档注记内引用不计）。
- [x] T203 [P] [US2] `docs/CONFIG_DIR_DESIGN.md`
  - :3 状态「设计稿（待评审后实施）」（实测）→ 改为已实施状态行。
  - :42/:153 引用 docs/A2A_MCP_INTEROP.md（实测不存在；执行时先 grep 定位）→ 删引用；:155 dwfrun-cc551c03 并行协调注记（实测在位）已失效 → 删。
  - §3「stdio 候选扩为四路径」对齐实现：main.rs:38 实测仅 `[config_dir_in(&anchor), config_dir_in(&cwd)]` 两候选，旧 duthub-data 根仅保留 mcp-state.json 快照发现。
  - 复核：`grep -n "A2A_MCP_INTEROP\|dwfrun" docs/CONFIG_DIR_DESIGN.md` 零命中。

## Phase 4: User Story 3 — 架构与选型对齐 (Priority: P2)

- [x] T301 [P] [US3] `docs/ARCHITECTURE.md`
  - :12「Tauri 2 + HTML/JS」（实测）→ GPUI（gpui-kit 0.9.1）现行选型。
  - :11 duthub-agent 描述未落地（src/lib.rs 实测 1 行占位）→ 标注占位；:13 duthub-tui 未落地（apps/tui/src/main.rs 实测 4 行 placeholder）→ 标注。
  - :23/:73 AppService 为 M0 演示骨架待删（AUDIT F02）→ 加待删标注。
  - :42-49 workspace 树补齐并对齐当前树：本轮实测 **12 成员**（简报记 11，缺 duthub-tooldown）；必含 crates/duthub-a2a 与 crates/duthub-transfer，并核对 duthub-tooldown。
  - 复核：树与根 Cargo.toml members（12）一致；`grep -n "Tauri" docs/ARCHITECTURE.md` 命中均为带注记历史表述。
- [x] T302 [P] [US3] `docs/DATA_MODEL.md`
  - :9 DUT 字段清单补 media（DutMediaConfig）与 heartbeat（HeartbeatConfig）：实测 DutMediaConfig = model.rs:518、`pub media` = :699、`pub heartbeat` = :702（简报记 :646/:649 属漂移，以实测为准）。
  - :9「A2A 运行时尚未接入」过时 → crates/duthub-a2a 回环运行时已落地（lib.rs 头注「W3 最小面」实测）。
  - 复核：字段清单与 model.rs 实测一致（人工通读）。
- [x] T303 [P] [US3] `docs/RUST_INTEROP_DESIGN.md`
  - :64 末注「bundled Python server is an integration prototype」→ 删（全仓 .py 实测 0）。
  - 类型草稿（A2AServerConfig/A2APeer/SerialContext 等）加「实际落地形态」注记：DutA2aConfig + crates/duthub-a2a，避免误当 API 契约。
  - 复核：`grep -in "python" docs/RUST_INTEROP_DESIGN.md` 零命中；草稿节注记在位。
- [x] T304 [P] [US3] `docs/MODEL_CREDENTIALS.md` — 三处机械清理
  - :17-23「Frontend state」节（sessionStorage / fetch('/api/llm/test') / Origin 校验）描述已退役 HTML 桥接架构。
  - :14「front-end simulation」过时（真实 Agent 会话已 ACP 接入，apps/desktop/src/agent_host/）。
  - :27「Migrate into Rust Tauri IPC」过时（已落地 GPUI/reqwest + keyring，keys.rs）。
  - 复核：`grep -n "Tauri\|sessionStorage\|front-end" docs/MODEL_CREDENTIALS.md` 命中均带历史注记或零命中。
- [x] T305 [P] [US3] `docs/PROJECT_REFERENCES.md`
  - :8「gpui 仅作为中长期原生方案」与 :9-10 Tauri 两行过时（GPUI 已是现行选型，根 Cargo.toml gpui-kit 0.9.1）→ 标注现行选型；Tauri 条目标注历史或移除（倾向标注以保留可追溯性）。
  - 复核：`grep -n "Tauri\|中长期" docs/PROJECT_REFERENCES.md` 命中均带注记或零命中。
- [x] T306 [P] [US3] `docs/API_CONTRACT.md`
  - :3「建议通过 Tauri invoke」（过时痕迹）→ GPUI Workbench 直调口径。
  - :5-17「前端命令」表 11 个 IPC 命令未同名实现：实际 AppCommand 位于 crates/duthub-core/src/ipc.rs:17-27，仅 Ping/GetVersion/SubmitLine 三探针（实测；简报记 ipc.rs:13-27 未带路径）→ 先行加「历史草案：未以同名实现，现行能力由桌面 GUI 方法承担」注记；是否改写为现状由 owner 拍板（裁量项 O3），本轮只加注。
  - 注：本文件同时出现于更新清单与未实现报告清单，按更新清单执行本任务。
  - 复核：`grep -n "Tauri" docs/API_CONTRACT.md` 零命中；:5-17 节注记在位。

## Phase 5: User Story 4 — ui-spec 溯源与数值可复核 (Priority: P3)

- [x] T401 [P] [US4] `docs/ui-spec/pages.md`（改点行号为简报给定，先读原文定位）
  - :3「V35 冻结原型」溯源失效（现存 docs/duthub.html 为 2026-11 复刻版，实测 :3 在位）→ 修正溯源。
  - 「4 个顶层 section」与现存不符（实测 `class="view"` = 2；简报记 :5，:5 实为「6 类可交互界面」表述，执行时 grep "section" 定位实际行）→ 修正。
  - :61「93 键」失效（HTML 内嵌 i18n 0 命中；locales 现各 683 键，实测）→ 修正。
  - :44「平台外观」当前 HTML 设置页无此项 → 处理。
  - :57 toast 位置 → 右上角（HTML:383 实测 `top:46px;right:14px`）。
  - :48 烧录预设 → 已移至配置浮层 data-page=flash（HTML:713 实测）。
  - 复核：逐处数值对现存 docs/duthub.html / locales 复核通过。
- [x] T402 [P] [US4] `docs/ui-spec/theme.md`
  - Dark 段 --radius:12px 过时 → radius 6px / radius-lg 10px（HTML:10 与 theme.rs:160-161 双实测）。
  - 补录 4 个已落地 token：--accent-contrast / --backdrop / --accent-ring / --sidebar（HTML:10 实测在列）。
  - 注明 scripts/extract-ui-spec.pl:3 输入指向不存在的仓库根 duthub.html：本轮手改 theme.md，脚本修复方式留 owner 裁量（O4）。
  - 复核：数值与双实测一致。
- [x] T403 [P] [US4] `docs/ui-spec/i18n.md`
  - 头部「来源：duthub.html 内嵌 const i18n 表」失效（HTML grep 0 命中）→ 标注为 V35 存档快照；「zh=93/en=93、63 引用键」注明无法对现存文件复核。
  - flashTest 族键已被应用 locales 取代（实测 zh-CN.json flashTest/flashExecutable/flashHelp 0 命中、flashProbe*/flashPresets 6 命中）→ 加取代注记。
  - 复核：存档注记与取代注记在位。

## Phase 6: User Story 5 — 停更文档停止误导 (Priority: P3)

- [x] T501 [P] [US5] `docs/agent/TASKS.md`
  - :3「执行 agent 的唯一进度事实源」已失效（停更于 2026-11-18，事实源迁 AGENT_IMPLEMENTATION_TASKS.md T 卡与 HANDOFF）→ 加 supersession 指向。
  - :107「sermcp dut-ctrl」（实测）→ 改 duthub（品牌唯一字样）。
  - :78/:192-197 旧产物名 duthub-GUI.exe / duthub-GUI-windows-x64.zip → 注记现行 release.yml 构建（`cargo build -p duthub-desktop --bin duthub`，产物 duthub.exe）；H1 清单若复启用需改产物名。
  - M5 段 5.4/5.5 勾选与 BEST_PRACTICE.md:150 口径争议 → **不改勾选**，加 owner 裁量注记（O2）。
  - 复核：`grep -n "sermcp" docs/agent/TASKS.md` 零命中；`grep -n "duthub-GUI" docs/agent/TASKS.md` 仅存于带注记行。

## Phase 7: Polish & Cross-Cutting（交叉核验与收尾）

- [x] T901 失效引用总扫描（SC-2）：对 15 份更新文件 `grep -n "duthub-acp\|A2A_MCP_INTEROP\|interop_a2a_server"` → 零裸命中（存档注记内引用不计）。**（2026-11-23 执行通过：duthub-acp 0 命中；interop/A2A_MCP_INTEROP 残余命中全部带「已不存在/已清除」注记）**
- [x] T902 选型与品牌总扫描（SC-3/SC-4）：Tauri 四文件注记核查；sermcp/duthub-GUI 零裸命中。**（2026-11-23 执行通过：Tauri 残余命中均在带注记段落内——ARCHITECTURE:19 由 :37 注记覆盖、MODEL_CREDENTIALS:31 由 :29 注记覆盖；sermcp 0 命中；duthub-GUI 仅存注记行）**
- [x] T903 保护边界核查（SC-5）：`git status --porcelain -- docs/` 改动集合恰为 15 份更新文件；`specs/` 仅三个新文件；docs/examples / duthub.html / rust-reference / 保留 7 份 / 报告文件（除 API_CONTRACT 外 12 份）零改动。T903 未通过不得进入 T904/T905。**（2026-11-23 执行通过：与基线快照 diff 的新增行恰为 10 份 tracked 新 M，另 3 份 ADR 在 `?? docs/decisions/` 内、API_CONTRACT/GOOSE 沿用基线 M；保护边界文件 status 零条目）**
- [x] T904 回归测试（SC-6）：`cargo test --workspace` 全绿（至少 `cargo test -p duthub-core --test roundtrip` 必须绿）。⚠️ 多上下文并行期用隔离 `CARGO_TARGET_DIR`（AGENTS.md 坑 #10/#16），勿与对方工作流争抢 link。**（2026-11-24 执行通过：`cargo test --workspace` exit 0，全部 test binary `ok` / 0 failed（ignored 均为 real-hardware/真机类用例）；`cargo test -p duthub-core` 全绿，roundtrip 7 项含 settings/duts_example_roundtrip；docs/examples fixture 零改动。运行环境：本机 Git Bash 非交互 shell 的 PATH 无 cargo，必须用绝对路径 `C:/Users/luo-j/.cargo/bin/cargo.exe`——否则 `cargo … | tail` 因 command not found 让管道 exit 0 造成假绿，本轮两次险误判）**
- [x] T905 收尾：spec.md SC-1..SC-6 逐条勾验记录；执行中发现的新坑回写 AGENTS.md（如有）；提交（中文提交信息，概述做了什么与为什么；**不 push**）。**（2026-11-24 执行：SC-1..SC-6 勾验记录已写入 spec.md「Verification Record」节；AGENTS.md 无需回写——本轮唯一新坑「非交互 shell PATH 无 cargo 导致管道假绿」属本机工具链事实，已记入 spec.md 运行环境注记与本任务；**提交留 owner**：工作树同时含 Agent 集成/串口性能/工具下载等多个任务的未提交改动，拆分提交由 owner 决定，本轮不代提交）**

## Dependencies & Execution Order

- Phase 1 → Phase 2-6 → Phase 7；Phase 2-6 组内任务均 [P]（不同文件、无依赖）可并行，组间无硬依赖。
- T101（ADR-11 状态行）与 T102（ADR_SIGNOFF 概括）互为引用，建议同一上下文连续完成，避免中途出现新两说。
- Phase 7 必须在全部更新完成后执行；T903 未通过不得进入 T904/T905。

## Owner 裁量项（非本轮任务，保持未勾选，不顺手定案）

- [ ] O1 ADR-11 最终措辞复核（本轮已统一 Proposed 口径）
- [ ] O2 agent/TASKS.md M5 段 5.4/5.5 勾选回退与否（与 BEST_PRACTICE.md:150 口径）
- [ ] O3 API_CONTRACT.md :5-17 前端命令节：保留历史草案 vs 改写为现状
- [ ] O4 scripts/extract-ui-spec.pl 修复方式（其 :3 输入指向不存在的仓库根 duthub.html；theme.md 本轮手改）
- [ ] O5 ARCHITECTURE_REVIEW.md 的 dutabo 残留（实测 :39/:40/:42/:44 共 4 行；该文件本轮列报告零改动）
- [ ] O6 ADR-2/8/9/11 产品签字、duthub-transfer 去留（HANDOFF 待拍板项）

## Notes

- 红线：零文件删除（untracked 删除不可恢复，docs/decisions/ 整目录当前 untracked）、零代码/脚本改动、不引入 Python、品牌唯一 duthub、不 push。
- 「未实现报告」13 份与「保留」7 份的完整清单及理由见 spec.md「范围与保护边界」。
- 模板纪律：不保留样例任务；跨故事不设依赖（各文件独立）；每故事完成后按 Checkpoint 验证再继续。
