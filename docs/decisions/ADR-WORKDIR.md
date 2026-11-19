# ADR-WORKDIR：工作根双解析器并存与 Agent 路径约束

- 状态：已接受（Accepted），签字见 `docs/decisions/ADR_SIGNOFF.md`
- 日期：2026-11-21
- 阶段：W0 签字；W3 Agent 接线起生效；T24 完成后由 T24 统一收口
- 相关：T24、AGENTS.md 坑 #9 / #11、`docs/design/CONFIG_DIR_DESIGN.md`、
  `docs/reviews/AGENT_INTEGRATION_BEST_PRACTICE.md` §2.9 / §4.6

## 1. 问题

同一 crate 中存在两套工作根解析，优先级相反：

| 解析器 | 位置 | 优先级 | 是否触盘 | 语义 |
|---|---|---|---|---|
| `ensure_portable_work_dir(exe_dir, settings_dir, cli_dir)` | `crates/duthub-core/src/store.rs:79-105` | CLI `--workdir` > settings `workspaceDir` > exe 目录 | 创建 / 校验 `duthub-data` 子目录；不可写回退 exe 并返回提示 | 返回 **artifact_root**；`main.rs` 再拼 `duthub-data` 得 `work_dir` |
| `resolve_work_dir_in(exe_dir, settings_dir, cli_dir)` | `crates/duthub-core/src/store.rs:295-315` | settings `workspaceDir` > CLI `--workdir` > 默认 exe 同级 `duthub-data` | 纯函数，不触盘 | 返回 **work root**（默认已含 `duthub-data`） |
| `ensure_work_dir_in(exe_dir, settings_dir, cli_dir)` | `crates/duthub-core/src/store.rs:320-338` | 同 `resolve_work_dir_in`（settings > CLI） | 创建 / 回退 | 返回 work root + 回退原因 |

调用现状：

- 桌面 bootstrap 用 `ensure_portable_work_dir`（`apps/desktop/src/main.rs:83-94`），
  `work_dir = artifact_root.join("duthub-data")`；
- `duthub-mcp` stdio bin 用 `resolve_work_dir_in`（`crates/duthub-mcp/src/main.rs:36-46`），
  但它先按 CLI > settings 预选配置，再以 `Some(configured)` 传入，因此 bin 的**有效**顺序
  仍是 CLI > settings，而解析器本身仍是 settings > CLI；两者不要混为一谈；
- `ensure_work_dir_in` 由 `store.rs:341-347` 的 `ensure_work_dir` 封装；`ensure_work_dir` 被
  `crates/duthub-device/src/lib.rs:78` 的 `DeviceService::new` 使用（桌面实际用
  `DeviceService::custom(1024, work_dir.join("logs"), 25)`，见
  `apps/desktop/src/main.rs:185`，不走 `DeviceService::new`）；
- 另有若干注释引用旧解析链：`crates/duthub-device/src/logs.rs:7`、
  `crates/duthub-mcp/src/skill.rs:5`、`apps/desktop/src/workspace.rs:856`；其中
  `workspace.rs:856` 仍写成 settings > `--workdir`。这些注释与实际调用点不一致，是本 ADR
  冻结的风险之一，T24 一并校正。

既有文档对此不一致：`docs/design/CONFIG_DIR_DESIGN.md:28` 把 `ensure_portable_work_dir` 写成
CLI < settings，`:66` 又把两套统一写成 settings > CLI；
`docs/reviews/AGENT_INTEGRATION_BEST_PRACTICE.md:465-471`、`:1003` 记录了相反的实测结论。

## 2. 决定

1. **本轮接受两套解析器并存**，不实施 T24 的统一。W0 只冻结边界，不改变现有调用点行为。
2. **Agent 的 cwd / sandbox / 写边界只使用 `ensure_portable_work_dir`**：
   - 桌面启动时由 `main.rs` 解析 `artifact_root`，`work_dir = <artifact_root>/duthub-data`；
   - 每 DUT 的 Agent 目录必须位于 `<work_dir>/agent/...` 下，sandbox 为该目录内的绝对路径；
     本轮计划布局为 `<work_dir>/agent/duts/<hex(dut_id)>/sandbox`，子目录名可在实现波次调整，
     但根必须是 `ensure_portable_work_dir` 的结果；
   - 不得把 `.duthub`、`duts.json`、DUT 资料目录、exe 目录或 `resolve_work_dir_in` 的返回值
     作为 Agent cwd。
3. **`duthub-mcp` bin 本轮不改**：
   - 它继续用现有 `discover_data_dir` / `resolve_work_dir_in` 发现 stdio 快照；
   - 不得以「Agent 路径统一」为由改它的优先级或调用链；
   - 它的有效 CLI > settings 来自 `crates/duthub-mcp/src/main.rs:40-46` 的预选，不是
     `resolve_work_dir_in` 本身；两者不要混为一谈。
4. **不新增 `resolve_work_dir_in` 的 Agent 调用点**。新 Agent 代码不得调用
   `resolve_work_dir_in` / `ensure_work_dir_in` 来推导 cwd；需要路径纯函数时，由 T24 新增
   统一的 `resolve_work_dir` / `resolve_agent_cwd`。
5. **T24 是唯一的统一入口**：
   - 目标顺序统一为 CLI > settings > exe；
   - 必须同时改 `resolve_work_dir_in`、`ensure_work_dir_in` 的语义与既有单测
     （`crates/duthub-core/src/store.rs:816-838` 的
     `work_dir_priority_settings_over_cli_over_default` 需显式改名 / 改断言并在任务卡记录，
     不得静默留旧断言）；
   - 必须新增 `crates/duthub-core/tests/work_dir.rs`，覆盖 CLI / settings / default、相对 /
     绝对、空白、回退；
   - 必须评估 `duthub-mcp` bin、`ensure_work_dir` / `DeviceService::new`、以及 logs / skill /
     workspace 的注释与调用点；逐个加回归；
   - T24 完成前，本 ADR 的两套解析器并存状态是**有意接受**，不是遗漏。
6. **配置加载与生效时机不变**：settings 只在进程启动时加载一次（AGENTS.md 坑 #9），
   `--workdir` / `workspaceDir` 变更重启生效；Agent 会话不得在运行中重新解析工作根。

## 3. 后果

正面：

- 不阻塞本轮 Agent 主链；Agent 写边界有唯一入口，避免把 `.duthub` / DUT 资料暴露为 cwd；
- 不触碰 `duthub-mcp` stdio 快照发现与既有日志 / 技能路径，降低回归面。

负面 / 风险：

- 两个同名概念（artifact_root vs work root）仍可能被误用；代码审查必须检查 Agent 新代码
  是否只走 `ensure_portable_work_dir`；
- `resolve_work_dir_in` 的 settings > CLI 与桌面 CLI > settings 在极端配置（同时设置
  `--workdir` 与 `workspaceDir`）下会得到不同目录；本轮用「Agent 只走前者」隔离，T24 前
  不得把两者互相「校正」；
- T24 改既有单测是必要的契约变更，必须走任务卡与评审，不能以「既有测试不可改」为由保留
  相反优先级。

## 4. 验收 / 检查

- 代码审查：`grep -rn "resolve_work_dir_in" apps/desktop/src/agent_host`
  （ACP 客户端位于 `apps/desktop/src/agent_host/acp.rs`）应为 0（Agent 新代码不调用）；
- Agent 测试断言 cwd 位于 `<work_dir>/agent/...`，且不等于 `.duthub` / `duts.json` 所在目录；
- `cargo test -p duthub-core --lib` 现有解析器测试在本轮保持通过；T24 再按新契约改名 /
  改断言；
- `cargo test -p duthub-core --test work_dir` 由 T24 新建；本轮不建；
- `duthub-mcp` stdio e2e / 快照测试本轮不改、不回归失败。

## 5. 参考

- `crates/duthub-core/src/store.rs:79-105`、`:295-315`、`:320-338`、`:816-838`、`:876-890`；
- `apps/desktop/src/main.rs:83-94`；
- `crates/duthub-mcp/src/main.rs:22-53`；
- `43c58dc:docs/AGENT_IMPLEMENTATION_TASKS.md` T24（`:727-742`）；
- `docs/reviews/AGENT_INTEGRATION_BEST_PRACTICE.md` §2.9 / §4.6、`:465-471`、`:1003`；
- `docs/design/CONFIG_DIR_DESIGN.md:28`、`:66`；
- `AGENTS.md` 坑 #9 / #11。
