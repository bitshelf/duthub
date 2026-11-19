# Implementation Plan: docs 目录清理与代码互证

**Branch**: `001-docs-cleanup` | **Date**: 2026-11-23
**Spec**: `specs/001-docs-cleanup/spec.md` | **Input**: 终审处置清单（删除 0 / 更新 15 / 保留 7 / 未实现报告 13）
**格式说明**: 按 spec-kit plan-template.md 驱动，正文中文。本轮为 documentation-only 变更，模板的「Source Code 三个 Option」均不适用（Structure Decision 见下）。

## Summary

按终审处置清单对 docs/ 做**机械修正级**更新：消除状态两说（ADR-11 ↔ ADR_SIGNOFF）、清零失效引用（crates/duthub-acp / docs/A2A_MCP_INTEROP.md / interop_a2a_server.py）、把过时选型与能力表述对齐 GPUI/12 成员 workspace 现实、修正 ui-spec 溯源数值、给停更任务文档加 supersession。**零删除、零代码改动**；唯一新增是 specs/001-docs-cleanup/ 三份产物。

## Technical Context

| 字段 | 值 |
|---|---|
| Language/Version | Rust workspace（根 Cargo.toml，resolver = 2；本轮实测 members **12** 个：crates ×10（含 duthub-tooldown）+ apps/desktop + apps/tui；`exclude = ["docs/rust-reference"]`）。文档任务：零代码改动、零新依赖 |
| Primary Dependencies | 无新增。gpui-kit 0.9.1 为现行桌面选型（本轮选型表述更新的事实基准） |
| Storage | 无（仅 .md 编辑；不改 settings.json/duts.json 等任何配置） |
| Testing | 无新测试。回归基线 `cargo test --workspace`（最小集 `cargo test -p duthub-core --test roundtrip`，守护 docs/examples fixture 未受影响） |
| Target Platform | 仓库主平台 Windows（win32）；文档内容跨平台 |
| Project Type | documentation-only（模板 Source Code Option 1/2/3 均不适用） |
| Performance Goals | N/A |
| Constraints | 见下「硬约束对照」 |
| Scale/Scope | 15 文件 × 每文件 1-7 处机械改点（明细见 tasks.md） |

### 硬约束对照（AGENTS.md，执行者必读）

- **不引入 Python**：本轮所有核验只用 grep/ls/sed/stat/find/git/cargo；不写任何脚本。
- **品牌字样唯一 duthub**：更新文案不得引入 sermcp/dutabo 等历史名号。
- **untracked 删除不可恢复**：`git status --porcelain -- docs/` 实测 `?? docs/decisions/`（整目录 7 份 ADR）、`?? docs/AGENT_INTEGRATION_HANDOFF.md`、`?? docs/AGENT_IMPLEMENTATION_REPORT.md`、`?? docs/RUST_MODERNIZATION_AUDIT.md`、`?? docs/RUST_MODERNIZATION_AUDIT_SUMMARY.md`——这些文件无 git 历史，删除即永久丢失。本轮 deletes 已裁决为空；执行器禁止 rm/del 任何仓库文件。
- **docs/examples fixture 被 roundtrip 测试引用**：crates/duthub-core/tests/roundtrip.rs:15 `manifest_dir().join("../../docs/examples")`，:28/:34 分别读 settings.json / duts.json（本轮实测）——该目录禁止触碰，收尾必须跑 roundtrip 确认。
- **docs/ 中文**：docs/ 正文保持中文（AGENTS.md 2026-11-21 起代码注释英文、docs/ 中文）。
- **并行协作**：每份文件的修改只归一个上下文；多上下文可并行调查，禁止对同一文件双写。

## Constitution Check

| 仓库硬约束 | 状态 | 说明 |
|---|---|---|
| 不引入 Python | PASS | 零脚本；核验只用只读命令 |
| UI 一律 gpui-kit 组件 | N/A | 无 UI 改动 |
| 品牌唯一字样 duthub | PASS | FR-4 强制；SC-4 复核 |
| 阶段收尾 fmt + clippy + test 全绿 | PARTIAL→PASS | 零代码改动：`cargo test --workspace` 必须跑（SC-6）；fmt/clippy 无 Rust 输入可改，跑与不跑不改变工作树 |
| 提交信息中文、不 push | PASS | Phase 7 收尾 |
| 既有 E2E/回归测试不许删改 | PASS | 不触碰任何测试文件 |
| 未实现报告文件只记录 | PASS | 13 份零改动（API_CONTRACT 双重归类见 spec Edge Case） |

## 实测锚点（2026-11-23 本轮实测；与上游简报数字冲突时以本表为准）

**decisions 组**
- ADR mtime：ADR-11 = 2026-11-21 17:21:02；ADR_SIGNOFF = 2026-11-21 23:16:15（更晚，权威载体）（`stat -c '%y %n' docs/decisions/*.md` 实测）。
- ADR-11:3 = 「状态：已接受（Accepted）」（sed 实测）；ADR_SIGNOFF:110 = 「ADR-0/2/5/6/7/8/9/10 属于旧 v5 长周期计划（T01/P0）」；:122 = 「ADR-2/8/9/11 为 Proposed：A2A LAN 入站、Agent Card 1.0.1、StateDb 上移 core、」（grep 实测）。
- store.rs：`ensure_portable_work_dir` = :79；`resolve_work_dir_in` = :295；`ensure_work_dir` = :341（另有 `ensure_work_dir_in` = :320）（grep 实测）。
- crates/duthub-acp 不存在（ls 实测）；apps/desktop/src/agent_host/ 13 文件含 acp.rs（ls 实测）。

**架构/数据模型组**
- workspace members = **12**（简报记 11）：core/device/adb/control/flash/mcp/a2a/agent/transfer/**tooldown** + desktop + tui（根 Cargo.toml sed 实测）。
- ARCHITECTURE.md：:11 duthub-agent 描述未落地（crates/duthub-agent/src/lib.rs 实测 1 行）、:12 「Tauri 2 + HTML/JS 前端」、:13 duthub-tui Ratatui（apps/tui/src/main.rs 实测 4 行 placeholder）（sed/wc 实测）。
- model.rs：`DutMediaConfig` = :518；`pub media` = :699；`pub heartbeat` = :702（简报记 :646/:649，**漂移**）。
- AppCommand = crates/duthub-core/src/ipc.rs:17-27，仅 Ping/GetVersion/SubmitLine 三探针（实测；简报记 ipc.rs:13-27 未带路径——`apps/desktop/src/ipc.rs` 不存在）。
- server.rs:1320 = `plan_tool_allowed`（Plan 白名单起点；简报记 :1320-1337）。
- transport.rs:189 = `.enforce_origin_validation()`；:385 = `assert_bind_allowed`（grep 实测；assert_loopback 全仓 0 命中）。
- crates/duthub-a2a/src/lib.rs 头注 = 「W3 最小面…任务状态仅内存（ADR-9）」（sed 实测）；workspace.rs grep `a2a_running` = 0 命中。

**GOOSE 组**（grep/sed 实测）
- export_skill_md = workspace.rs:3199；STEP_INTERVAL = agent.rs:35；OFFLINE DEMO = agent.rs:506；快照 sleep(2s) = mcp_runtime.rs:599；apr- = approval.rs:143；Plan 白名单 = server.rs:1320 起。
- interop_a2a_server / A2A_MCP_INTEROP 引用实测命中 GOOSE :76/:77/:198/:240（简报记 §1.5/:77/:198/:226/:240/:264，:226/:264 未复现——执行时以 grep 实测为准）。
- 全仓 .py = 0（`find . -name '*.py'` 排除 target/.git 实测）。

**CONFIG / ui-spec 组**
- CONFIG_DIR_DESIGN.md:3 = 「状态：设计稿（待评审后实施）」；:155 = dwfrun-cc551c03 注记（sed/grep 实测）；:42/:153 A2A_MCP_INTEROP 引用为简报给定，执行时先 grep 定位。
- main.rs:38 = `[config_dir_in(&anchor), config_dir_in(&cwd)]` 两候选（grep 实测）。
- docs/duthub.html：:10 = `--radius:6px;--radius-lg:10px` 且 --accent-contrast/--backdrop/--accent-ring/--sidebar 在列；`class="view"` = 2 处；:383 = toast `top:46px;right:14px`；:713 = `data-page="flash"`（grep/sed 实测）。
- theme.rs:160-161 = radius 6.0 / radius_lg 10.0（sed 实测）。
- locales：zh-CN.json 与 en-US.json 各 683 个键值行（`grep -c '":'` 近似实测，与简报「各 683 键」一致）；flashTest/flashExecutable/flashHelp = 0 命中、flashProbe*/flashPresets = 6 命中（zh-CN.json grep 实测）。
- pages.md:3 = 「V35 冻结原型」（sed 实测）；「4 个顶层 section」未在 :5 复现（:5 实为「6 类可交互界面」表述，执行时 grep "section" 定位实际行）。

**品牌残留**
- sermcp = docs/agent/TASKS.md:107（grep 实测）；dutabo = docs/ARCHITECTURE_REVIEW.md :39/:40/:42/:44（grep 实测，4 行；该文件本轮不修改，属 owner 裁量项）。

**清单点数核对**
- 最终处置清单逐项点数：**更新 15 / 保留 7 / 未实现报告 13 / 删除 0**。仲裁调整文本记「14 份 update」，与逐项点数不符，以逐项点数为准。
- API_CONTRACT.md 同时出现在更新清单与未实现报告清单（双重归类，见 spec Edge Case）。

## Project Structure

```
specs/001-docs-cleanup/          # 新增（本轮唯一新增）
├── spec.md                      # 本轮产物
├── plan.md                      # 本轮产物
└── tasks.md                     # 本轮产物（待执行）

docs/                            # 修改 15 份 .md（分组见 tasks.md）
├── decisions/                   # G1：ADR-11、ADR-WORKDIR、ADR_SIGNOFF（其余 4 份 ADR 属报告零改动）
├── agent/TASKS.md               # G2
├── GOOSE_AGENT_FEASIBILITY.md   # G2
├── ARCHITECTURE.md / DATA_MODEL.md / RUST_INTEROP_DESIGN.md /
│   MODEL_CREDENTIALS.md / PROJECT_REFERENCES.md / API_CONTRACT.md /
│   CONFIG_DIR_DESIGN.md         # G3
└── ui-spec/                     # G4：pages.md、theme.md、i18n.md

docs/examples/                   # 保护边界：roundtrip fixture，禁止触碰
docs/duthub.html                 # 保护边界：生成链上游资产，仅作对照
docs/rust-reference/             # 保护边界：exclude 的离线参考
```

**Structure Decision**: documentation-only——模板三个 Source Code Option 全部不适用，无源码结构变化。

## Execution Phases

- **Phase 1 基线**：`git status --porcelain` 快照 + 清单文件在位核对（tasks.md T001）。缺件即停。
- **Phase 2-6 按用户故事分组更新**（组内文件相互独立可并行，组间无硬依赖）：
  - Phase 2 = US1 权威口径（ADR-11、ADR_SIGNOFF）
  - Phase 3 = US2 失效引用清零（ADR-WORKDIR、GOOSE、CONFIG_DIR_DESIGN）
  - Phase 4 = US3 架构/选型对齐（ARCHITECTURE、DATA_MODEL、RUST_INTEROP_DESIGN、MODEL_CREDENTIALS、PROJECT_REFERENCES、API_CONTRACT）
  - Phase 5 = US4 ui-spec（pages、theme、i18n）
  - Phase 6 = US5 停更 supersession（agent/TASKS）
  - 每文件节拍：读原文 → 按 tasks.md 条目改 → 跑该条目复核命令。
- **Phase 7 交叉核验与收尾**：SC-1..SC-6 逐条勾验（T901-T905）；`cargo test --workspace`；若发现新坑回写 AGENTS.md；中文提交信息，不 push。

**Checkpoint**: Phase 2 完成即消除唯一事实互斥（US1 可独立验收）；全部 Phase 完成后 Phase 7 的集合断言（T903）是放行门。

## Complexity Tracking

模板要求仅在宪法违规需豁免时填写。本轮无违规：

| Violation | Why Needed | Simpler Alternative Rejected Because |
|---|---|---|
| （无） | — | — |
