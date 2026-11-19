# ADR-9：A2A 任务状态库（W3 仅内存；持久化边界与后续 StateDB）

- 状态：提议（Proposed，未签字——签字流程见 `docs/decisions/ADR_SIGNOFF.md`）
- 日期：2026-11-21
- 阶段：W3（A2A 回环最小面）
- 相关：A2A v1.0 规范 §7（任务模型）；`docs/decisions/ADR-2-A2A-LAN.md`、`docs/decisions/ADR-8-AGENT-CARD.md`
- 代码：`crates/duthub-a2a/src/executor.rs`（进程内去重 + 任务归属）、`crates/duthub-a2a/src/task.rs`（StrictRequestHandler）

## 1. 背景

`a2a-server-lf` 0.5.3 只内置 `InMemoryTaskStore`（事实 20：长任务持久化需宿主自建）。
duthub 的每 DUT 端点把 A2A 任务用作「证据工具调用的一次执行」，任务状态要不要落盘、
messageId 幂等要不要跨进程、重启后任务归属怎么复核，决定后续 StateDB 的形状。

## 2. 决定

### 2.1 W3 已实现

| 决定 | 实现 | 语义 |
|---|---|---|
| 任务状态 = `InMemoryTaskStore`（每监听器一份） | `DefaultRequestHandler::new(executor, InMemoryTaskStore)` | 进程内可 `GetTask`/`CancelTask`；不落盘、不恢复 |
| messageId 进程内去重 | executor 维护 `messageId → 已记录 artifact 结果` 映射（上限 4096 条，进程内） | 同一 messageId 重放**不重复执行** MCP 工具：新任务直接携带已记录结果并 `Completed`；**不承诺 taskId 相同**（新任务新 ID，artifact 内容一致） |
| 任务归属登记 | executor 在任务创建时登记 `taskId → dut_id`；authorizer 对 `GetTask`/`CancelTask` 逐请求复核 | 跨 DUT / 未登记 taskId → `task_not_found`；不依赖「store 里只有本 DUT 任务」的隐式约定 |
| 失败语义 | TextPart / 未知 skill / 跨 DUT / 证据白名单外 skill → 任务终态 `TASK_STATE_FAILED` + policy 说明（协议层任务失败，不是传输层错误） | Agent 可读原因；不出现在 HTTP 4xx |
| 取消 | `executor.cancel` 发 `TASK_STATE_CANCELED`；`with_test_delay` 仅为测试构造 Working 窗口 | 取消覆盖执行中任务；终态不可取消（store 拒绝） |

### 2.2 明确不实现（deferred）

- **持久化 StateDB**：任务历史、artifact 结果、去重表都不落盘。重启后所有 taskId 失效
  （`task_not_found`），重放的 messageId 重新执行一次。后果：进程重启 = 任务状态清零，
  Agent 侧必须容忍（重发消息即可重取证，证据工具是只读幂等探测，重复执行无副作用）。
- **跨进程 messageId 幂等**：持久化去重表需要 WAL/compaction 设计，等有真实的「重启续跑」
  需求（长任务/审批中任务）再立项；W3 的证据调用都是秒级，不值得引入。
- **任务配额/多租户**：每 DUT 单监听器、单 token；不区分并发来源。
- **StateDB 位置**（后续采纳时再定）：倾向 `<work_dir>/duthub-data/runtime/` 下按 DUT 分文件，
  沿用既有 runtime 快照目录约定；不进 `.duthub/`（配置目录）。

## 3. 后果

- 客户端契约：A2A 端点重启 = 任务全失；导出给 Agent 的 skill 文案必须写明「用 SendMessage
  重发而非依赖旧 taskId 续跑」。
- 去重表是进程内软状态：不保证跨重启幂等，也不保证 taskId 复用；测试与文档按此口径。
- `with_test_delay` 是 pub 但标注测试专用；生产代码不得调用（review 门）。

## 4. 签字要求

StateDB 实施前需修订本 ADR（补 §2.2 落地方案）并签字；当前「仅内存」是实现与测试的基准。
