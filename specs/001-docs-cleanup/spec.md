# Feature Specification: docs 目录清理与代码互证

**Feature Branch**: `001-docs-cleanup`
**Created**: 2026-11-23
**Status**: Draft
**Input**: docs/ 全量交叉审查（34 份逐文件判定 + 仲裁调整）产出的最终处置清单
**格式说明**: 按 spec-kit 官方三模板（2026-11-23 抓取自 github/spec-kit `templates/`）驱动产物格式，正文中文。spec-kit spec 模板无独立 Out of Scope 节，按任务要求增设「范围与保护边界」节，语义等同模板 Assumptions 的范围排除部分。

## Overview

docs/ 目录经全量代码互证审查后，存在三类与代码现实脱节的内容：①过时表述（Tauri 2 + HTML/JS 时代的架构描述、已退役的 HTML 桥接架构、已被 ADR-2 取代的 A2A 绑定约束）；②指向不存在资产的引用（crates/duthub-acp、docs/A2A_MCP_INTEROP.md、interop_a2a_server.py）；③同一事实两处说法矛盾（ADR-11 状态行自称 Accepted，权威载体 ADR_SIGNOFF.md 记 Proposed）。本特性**不删除任何文件、不修改任何代码**，仅对 15 份文档做机械修正级更新，并新增 specs/001-docs-cleanup/ 三份规格产物。

**Why（为什么做）**
- 文档是 Agent 技能与新人接入的第一入口：失效路径、漂移行号、两说状态会直接误导实现与验收。
- 品牌字样唯一为 duthub（AGENTS.md 规范）：agent/TASKS.md:107 的 sermcp 残留必须消除。
- 权威签字载体（ADR_SIGNOFF.md）与被裁决文件（ADR-11）状态矛盾，必须统一为权威载体口径。

**Who（受益者）**
- 后续开发者与 Agent 会话：读到的架构、路径、状态断言与当前树一致。
- 仓库 owner：签字状态只剩一个口径；遗留裁量项集中成清单而非散落文档。

## User Scenarios & Testing *(mandatory)*

### User Story 1 — 权威状态口径唯一 (Priority: P1)

作为仓库 owner 与后续开发者，我希望 ADR 状态只存在一个权威口径，以便签字状态不被误读为已签字。

**Why this priority**: 唯一「事实互斥」类问题——其余是过时或失效引用；两说直接影响产品签字判断。

**Independent Test**: 仅改 ADR-11 与 ADR_SIGNOFF 两份文件即可独立验收（grep 断言），不依赖其他故事。

**Acceptance Scenarios**:
1. **Given** docs/decisions/ADR-11-OS-ISOLATION.md:3 现为「状态：已接受（Accepted）」（mtime 2026-11-21 17:21:02）而权威载体 ADR_SIGNOFF.md:122 记「ADR-2/8/9/11 为 Proposed」（mtime 2026-11-21 23:16:15，更晚；本轮 stat/grep 实测），**When** ADR-11 状态行对齐 Proposed 口径，并加注「能力已在位：os_acl.rs:78 harden_private_dir、crates/duthub-core/src/restart.rs 退避——实现≠产品签字，最终措辞 owner 复核」，**Then** ADR-11 状态行不再自称 Accepted，两说消除。
2. **Given** ADR_SIGNOFF.md §6（:110 附近）把「ADR-0/2/5/6/7/8/9/10」整体称旧 v5 计划未签字，与 §7（:122）单列现行「ADR-2/8/9/11 为 Proposed」编号冲突，**When** 分开表述「旧 v5 计划编号」与「现行 docs/decisions/ ADR-2/8/9/11」两组，**Then** §6 与 §7 不再互相矛盾。

### User Story 2 — 失效引用清零 (Priority: P2)

作为依赖文档导航的开发者/Agent 会话，我不再按文档指引找到不存在的文件或目录。

**Why this priority**: 高频误导点，但每处影响面小、修复纯机械。

**Independent Test**: 对 ADR-WORKDIR / GOOSE_AGENT_FEASIBILITY / CONFIG_DIR_DESIGN 三份文件做失效路径 grep 即可独立验收。

**Acceptance Scenarios**:
1. **Given** ADR-WORKDIR §4 验收命令引用 crates/duthub-acp（本轮 `ls` 实测不存在），ACP 客户端实际在 apps/desktop/src/agent_host/acp.rs（本轮 ls 实测在位），**When** 改指实际路径并按当前树修行号（ensure_portable_work_dir = store.rs:79、resolve_work_dir_in = :295、ensure_work_dir = :341，本轮 grep 实测），**Then** `grep -n "duthub-acp" docs/decisions/ADR-WORKDIR.md` 零命中。
2. **Given** GOOSE_AGENT_FEASIBILITY 引用 interop_a2a_server.py 与 docs/A2A_MCP_INTEROP.md（两者本轮 ls/find 实测不存在；grep 实测命中 :76/:77/:198/:240，简报另记 :226/:264 属行号漂移），**When** 删除或标注存档，并更新 §1.5/§4「Rust 侧仅配置模型（a2a_running: false）」的过时表述（workspace.rs 该符号 0 命中，crates/duthub-a2a 回环运行时已落地），**Then** 该文件失效引用零裸命中。
3. **Given** CONFIG_DIR_DESIGN.md:3 状态「设计稿（待评审后实施）」、:42/:153 引用不存在的 A2A_MCP_INTEROP.md、:155 残留 dwfrun-cc551c03 并行协调注记（本轮实测均在位），且 §3「stdio 候选四路径」与实现偏差（main.rs:38 实测仅 `[config_dir_in(&anchor), config_dir_in(&cwd)]` 两候选），**When** 改状态行、删引用删注记、§3 对齐实现，**Then** 该文件无失效引用与失效协调注记。

### User Story 3 — 架构与选型对齐 GPUI 现实 (Priority: P2)

作为新接入的开发者，架构与选型文档反映 GPUI 现行选型、真实 workspace 树与已落地能力。

**Why this priority**: 影响技术栈判断，但全部是机械改写。

**Independent Test**: 对六份文件逐一 grep 断言（Tauri、字段名、树成员）独立验收。

**Acceptance Scenarios**:
1. **Given** ARCHITECTURE.md:12 写「Tauri 2 + HTML/JS 前端」（本轮实测），桌面实为 GPUI（gpui-kit 0.9.1）；:11 duthub-agent 描述未落地（src/lib.rs 实测 1 行占位）、:13 duthub-tui 未落地（apps/tui/src/main.rs 实测 4 行 placeholder）、:23/:73 AppService 为 M0 演示骨架（AUDIT F02）、:42-49 树缺 crates（本轮实测 workspace 12 成员：简报记 11，另有 duthub-tooldown），**When** 改选型表述、标占位、加待删标注、按当前树补齐成员（必含 duthub-a2a 与 duthub-transfer，并核对 duthub-tooldown），**Then** 文档树与根 Cargo.toml members 实测一致。
2. **Given** MODEL_CREDENTIALS.md:17-23「Frontend state」（sessionStorage/fetch('/api/llm/test')/Origin 校验）描述已退役 HTML 桥接、:14「front-end simulation」过时（真实 Agent 会话已 ACP 接入）、:27「Migrate into Rust Tauri IPC」过时（已落地 GPUI/reqwest + keyring，keys.rs），**When** 三处机械清理，**Then** 残留表述为零或带历史注记。
3. **Given** PROJECT_REFERENCES.md:8「gpui 仅作为中长期原生方案」与 :9-10 Tauri 条目过时（GPUI 已是现行选型），**When** 标注现行选型并处理 Tauri 条目，**Then** 无裸的过时选型表述。
4. **Given** API_CONTRACT.md:3「建议通过 Tauri invoke」过时、:5-17「前端命令」表 11 个 IPC 命令未同名实现（实际 AppCommand 位于 crates/duthub-core/src/ipc.rs:17-27，仅 Ping/GetVersion/SubmitLine 三探针，本轮实测），**When** :3 改 GPUI Workbench 直调口径、:5-17 加「历史草案」注记（改写与否留 owner 拍板），**Then** 读者不会把该节误当现行契约。
5. **Given** DATA_MODEL.md:9 DUT 字段清单缺 media/heartbeat（DutMediaConfig = model.rs:518、字段 `pub media` = :699 / `pub heartbeat` = :702，本轮实测；简报记 :646/:649 属漂移）且「A2A 运行时尚未接入」过时（crates/duthub-a2a lib.rs 头注「W3 最小面」，本轮实测），**When** 补字段、更新表述，**Then** 字段清单与 model.rs 实测一致。
6. **Given** RUST_INTEROP_DESIGN.md:64 末注「bundled Python server is an integration prototype」失效（本轮 find 实测全仓无 .py）且类型草稿（A2AServerConfig/A2APeer/SerialContext 等）未以该名实现，**When** 删注并加「实际落地形态：DutA2aConfig + crates/duthub-a2a」注记，**Then** 草稿不被误当 API 契约。

### User Story 4 — ui-spec 溯源与数值可复核 (Priority: P3)

作为 UI 实现/验收者，ui-spec 三份文件的溯源与数值能对现存资产复核通过。

**Why this priority**: 仅影响 UI 验收基准的准确性。

**Independent Test**: 对现存 docs/duthub.html 与 apps/desktop/locales/*.json 重跑数值断言。

**Acceptance Scenarios**:
1. **Given** pages.md:3「V35 冻结原型」溯源失效（现存 docs/duthub.html 为 2026-11 复刻版，`grep -c 'class="view'` 实测 = 2；简报记「4 个顶层 section」不符，:5 实测为「6 类可交互界面」表述）、「93 键」失效（HTML 内嵌 i18n 0 命中；locales 现各 683 键，本轮实测）、:44「平台外观」当前设置页无此项、:57 toast 为右上角（HTML:383 实测 `top:46px;right:14px`）、:48 烧录预设已移至配置浮层（HTML:713 实测 `data-page="flash"`），**When** 逐处按现存资产修正/标注，**Then** 数值断言复核通过。
2. **Given** theme.md Dark 段 --radius:12px 过时（HTML:10 与 theme.rs:160-161 双实测均 radius 6px / radius-lg 10px），**When** 修正并补录 4 个已落地 token（--accent-contrast/--backdrop/--accent-ring/--sidebar，HTML:10 实测在列），注明生成器 extract-ui-spec.pl:3 输入指向不存在的仓库根 duthub.html、本轮手改，**Then** 数值与双实测一致。
3. **Given** i18n.md 头部「来源：duthub.html 内嵌 const i18n 表」失效（HTML grep 0 命中）且 flashTest 族键已被取代（zh-CN.json 实测 flashTest/flashExecutable/flashHelp 0 命中、flashProbe*/flashPresets 6 命中），**When** 标注 V35 存档快照（93 键无法对现存复核）并加取代注记，**Then** 不再被当作现行键表。

### User Story 5 — 停更文档停止误导 (Priority: P3)

作为接手者，停更的历史任务文档显式声明被取代，不被人当开工依据。

**Why this priority**: 单文件、影响面清晰。

**Independent Test**: grep 品牌残留与 supersession 注记即可独立验收。

**Acceptance Scenarios**:
1. **Given** docs/agent/TASKS.md:3 自称「执行 agent 的唯一进度事实源」但停更于 2026-11-18（事实源已迁 AGENT_IMPLEMENTATION_TASKS.md T 卡与 HANDOFF），**When** 加 supersession 指向，**Then** 不再被当开工依据。
2. **Given** :107 残留「sermcp dut-ctrl」（本轮 grep 实测）、:78/:192-197 旧产物名 duthub-GUI.exe / duthub-GUI-windows-x64.zip（现行 release.yml 以 `cargo build -p duthub-desktop --bin duthub` 构建产物 duthub.exe），**When** 品牌替换与产物名注记，**Then** `grep sermcp` 零命中、H1 复启用注记指向正确产物名。
3. **Given** M5 段 5.4/5.5 勾选与 BEST_PRACTICE.md:150 口径存在争议（勾选描述的是模型探针功能而非 Agent 状态机），**When** 不改勾选、加 owner 裁量注记，**Then** 争议显式化且不被本轮顺手定案。

### Edge Cases

- **行号漂移**：执行时实测与清单坐标不符 → 以实测为准，在任务备注登记漂移；不阻塞、不顺手扩大改动面（本轮已发现并登记：workspace 成员 11→12、model.rs :646/:649→:518/:699/:702、GOOSE 引用 :226/:264→实测 :76/:77/:198/:240、ipc.rs 实际在 crates/duthub-core/src/）。
- **注记措辞引入新两说**：每处「已取代/存档」注记必须指明权威出处（文件+节或行）。
- **基线缺件**：Phase 1 基线核对发现清单中文件不在位 → 停止执行并上报，不得静默跳过。
- **API_CONTRACT.md 双重归类**：该文件同时出现在更新清单与未实现报告清单——按更新清单执行改点；报告条目仅记录其「前端命令节未实现」事实，不二次处理。

## Requirements *(mandatory)*

### Functional Requirements

- **FR-1**: 15 份更新文件的每一处改点必须先读原文定位再修改；禁止凭清单盲改。
- **FR-2**: 所有失效路径引用必须改为实测存在的路径；行号引用以 2026-11-23 实测坐标为准（见 plan.md「实测锚点」），执行时发现漂移以实测为准并登记。
- **FR-3**: ADR-11 状态行必须消除与 ADR_SIGNOFF.md §7（:122）冲突的 Accepted 自称；对齐 Proposed 口径并加 owner 复核注记。
- **FR-4**: 更新文案品牌字样唯一为 duthub，不得新引入 sermcp/dutabo 等历史名号（AGENTS.md 品牌规范）。
- **FR-5**: 保留的历史表述（API_CONTRACT :5-17 草案节、i18n V35 快照、GOOSE 被取代契约等）必须带显式「历史/存档/已取代」注记并指明权威出处，不得裸露在现行叙述中。
- **FR-6**: 改动范围仅限 docs/ 下 15 份 .md 与 specs/001-docs-cleanup/ 三份新产物；零文件删除、零代码/脚本改动。
- **FR-7**: 保护边界文件零触碰（见「范围与保护边界」）。

### Key Entities

不涉及数据模型变更。涉及的文档资产分类：**更新 15** / **保留 7** / **未实现报告 13** / **保护 fixture**（docs/examples/*.json）/ **生成链资产**（docs/duthub.html）/ **参考 crate**（docs/rust-reference）。

## Success Criteria *(mandatory)*

### Measurable Outcomes

- **SC-1**: US1 验收场景全过——`grep -n "已接受" docs/decisions/ADR-11-OS-ISOLATION.md` 状态行零自称；ADR_SIGNOFF §6/§7 编号互斥消除。
- **SC-2**: 对 15 份更新文件扫描 `duthub-acp` / `A2A_MCP_INTEROP` / `interop_a2a_server` → 裸引用零命中（带存档注记的引用不计）。
- **SC-3**: `grep -n "Tauri" docs/ARCHITECTURE.md docs/MODEL_CREDENTIALS.md docs/PROJECT_REFERENCES.md docs/API_CONTRACT.md` 每处命中均为带注记的历史表述；ARCHITECTURE.md workspace 树与根 Cargo.toml 实测（12 成员）一致。
- **SC-4**: `grep -n "sermcp" docs/agent/TASKS.md` 与 `grep -n "duthub-GUI" docs/agent/TASKS.md` 零裸命中（duthub-GUI 仅存于带注记的历史产物名行）。
- **SC-5**: `git status --porcelain -- docs/` 的改动集合恰为 15 份更新文件；`specs/` 下仅三个新文件；保护边界文件零改动。
- **SC-6**: `cargo test --workspace` 全绿（零代码改动后的回归基线；至少 `cargo test -p duthub-core --test roundtrip` 必须绿，确认 docs/examples fixture 未受影响）。

## 验收记录

| 判据 | 结果 | 证据 |
| --- | --- | --- |
| SC-1 | ✅ | `ADR-11:3` 已为「Proposed（未经产品签字；权威口径见 ADR_SIGNOFF §7——最终措辞待 owner 复核）」+ 能力注记（os_acl.rs:78、restart.rs）；`grep "已接受" ADR-11` 零命中；ADR_SIGNOFF §6（:112「旧 v5 长周期计划…」）与 §7（:127「ADR-2/8/9/11 为 Proposed」）已分组表述，`StateDb 上移` 零命中 |
| SC-2 | ✅ | 15 份更新文件中 `duthub-acp` 零命中（ADR-WORKDIR 已改指 `apps/desktop/src/agent_host/acp.rs`）；`interop_a2a_server` / `A2A_MCP_INTEROP` 残余命中全部带「已不存在（2026-11-23 实测）」注记（GOOSE:76/:198/:240、CONFIG_DIR_DESIGN:42/:146） |
| SC-3 | ✅ | ARCHITECTURE.md 树已按根 Cargo.toml 实测 **12 成员**（crates×10 含 a2a/transfer/tooldown + apps×2）补齐并带 2026-11-23 注记；MODEL_CREDENTIALS:14/:19/:29 三处过时表述均带历史注记；PROJECT_REFERENCES:8-10 GPUI 标为现行、Tauri 两条标历史未采用；API_CONTRACT `Tauri` 零命中 |
| SC-4 | ✅ | `grep sermcp docs/agent/TASKS.md` 零命中；`duthub-GUI` 仅存于 :78 带「旧产物名」注记行与 :196/:197 历史发布记录；:3 已加 Superseded 注记指向 AGENT_IMPLEMENTATION_TASKS/HANDOFF |
| SC-5 | ✅ | `git status --porcelain -- docs/examples docs/duthub.html docs/rust-reference docs/AGENT_ACCESS_CONTROL_PLAN.md docs/SKILL_RECOMMENDATIONS.md` 零条目（AGENT_INTEGRATION_HANDOFF.md 保持未跟踪、零修改）；`specs/001-docs-cleanup/` 恰 3 文件；零文件删除、零代码/脚本改动 |
| SC-6 | ✅（crate 级） | `cargo test -p duthub-core` 全绿：roundtrip 7 项（含 `settings_example_roundtrip`/`duts_example_roundtrip`）+ process 9 项 + tools_download_roundtrip 6 项 + doc-tests，0 failed（2026-11-24 实测）。全 workspace 套件另行在收尾轮后台执行，结果记录于 tasks.md T904 |

**运行环境注记**：本机 Git Bash 非交互 shell 的 PATH 无 `cargo`——`cargo ... | tail` 会因 `command not found` 让管道 exit 0 造成**假绿**；必须用绝对路径 `C:/Users/luo-j/.cargo/bin/cargo.exe` 并核对 `test result: ok` 行。

## 范围与保护边界（模板扩展节）

### In Scope（做什么）

- 更新 docs/ 下 15 份 .md（逐文件改点清单见 tasks.md Phase 2-6；按故事分布：US1 = ADR-11 + ADR_SIGNOFF，US2 = ADR-WORKDIR + GOOSE + CONFIG_DIR_DESIGN，US3 = ARCHITECTURE + DATA_MODEL + RUST_INTEROP_DESIGN + MODEL_CREDENTIALS + PROJECT_REFERENCES + API_CONTRACT，US4 = ui-spec 三份，US5 = agent/TASKS.md）。
- 新增 specs/001-docs-cleanup/ 三份产物（本轮唯一新增）。

### Out of Scope / 不做的事

1. **不删除任何文件**：终审裁决 deletes 置空；且 **untracked 删除不可恢复**——docs/decisions/（整目录 7 份 ADR）、AGENT_INTEGRATION_HANDOFF.md、AGENT_IMPLEMENTATION_REPORT.md、RUST_MODERNIZATION_AUDIT*.md 当前均为 untracked（本轮 `git status --porcelain -- docs/` 实测 `?? docs/decisions/` 等），无 git 历史，删除即永久丢失。执行器禁止 rm/del 任何仓库文件。
2. **不修改任何 Rust 代码/脚本**：代码引用仅作证据（os_acl.rs、restart.rs、server.rs、ipc.rs 等）；能力实现 ≠ 产品签字。
3. **保护边界（禁止触碰）**：
   - `docs/examples/*.json` —— roundtrip fixture：crates/duthub-core/tests/roundtrip.rs:15 `manifest_dir().join("../../docs/examples")`，:28/:34 分别读 settings.json / duts.json（本轮实测）；未提名、未受影响。
   - `docs/duthub.html` —— scripts/gen-icons.pl 的输入与主题 token 同源资产（HTML:10 与 theme.rs 双实测一致），仅作对照基准。
   - `docs/rust-reference/**` —— 根 Cargo.toml `exclude` 的离线参考，不向现行代码对齐。
   - **7 份保留文件**（零改动）：AGENT_INTEGRATION_HANDOFF.md、AGENT_ACCESS_CONTROL_PLAN.md、SKILL_RECOMMENDATIONS.md、SECURITY.md、duthub.html、rust-reference/Cargo.toml、rust-reference/src/lib.rs。
   - **13 份未实现报告文件**（只记录不修改）：AGENT_IMPLEMENTATION_REPORT.md、AGENT_IMPLEMENTATION_TASKS.md、AGENT_INTEGRATION_BEST_PRACTICE.md、decisions/ADR-2-A2A-LAN.md、ADR-3-GOOSE-BUNDLE.md、ADR-8-AGENT-CARD.md、ADR-9-STATEDB.md、RUST_MODERNIZATION_AUDIT.md、RUST_MODERNIZATION_AUDIT_SUMMARY.md、ARCHITECTURE_REVIEW.md、MCP_TEST_REPORT.md、IMPLEMENTATION_PLAN.md、API_CONTRACT.md*。*API_CONTRACT.md 的更新改点按 In Scope 执行（见 Edge Case 双重归类）。
4. **owner 裁量项不顺手定案**：ADR-11 最终措辞；agent/TASKS M5 5.4/5.5 勾选；API_CONTRACT :5-17 改写 vs 保留草案；extract-ui-spec.pl 修复方式；ARCHITECTURE_REVIEW.md 的 dutabo 残留（本轮实测 :39/:40/:42/:44，该文件本轮不修改）；ADR-2/8/9/11 产品签字；duthub-transfer 去留（HANDOFF 待拍板）。
5. 不动 AGENTS.md（执行中发现新坑按 AGENTS.md 规则回写除外）、不改 API 行为、不新增脚本/依赖、不引入 Python。

## Assumptions

- 行号坐标基于 2026-11-23 工作树实测；后续树漂移需重测。禁止顺手优化（AGENTS.md）：一次只改清单内改点。
- spec-kit CLI 不安装：ZCode 不在其支持 agent 列表，且仓库硬约束「不引入 Python」；仅借鉴其产物格式。
- 报告类文件登记的「未实现项」本身是有效历史信息，保留原样即为正确处置。
- 文件中的历史性行（如 H1 清单里的旧产物名）以「注记」方式处理，不重写历史语义。

## Risks

- **行号二次漂移** → 缓解：执行时逐点实测、以实测为准并登记（plan.md 锚点表已含本轮漂移记录）。
- **注记措辞引入新两说** → 缓解：FR-5 出处强制；ADR-11 最终措辞留 owner 复核。
- **误触保护边界** → 缓解：Phase 1 基线快照 + SC-5 集合断言。
- **并行上下文双写同一文件** → 缓解：AGENTS.md 并行协作规则——每份文件的修改只归一个上下文。
