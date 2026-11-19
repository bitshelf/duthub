# ADR-8：Agent Card 内容最小化与发现面（每 DUT 端点）

- 状态：提议（Proposed，未签字——签字流程见 `docs/decisions/ADR_SIGNOFF.md`）
- 日期：2026-11-21
- 阶段：W3（A2A 回环最小面）
- 相关：A2A v1.0 规范 §8（Agent Card）、§9.5（版本协商）；`docs/decisions/ADR-2-A2A-LAN.md`
- 代码：`crates/duthub-a2a/src/card.rs`、`crates/duthub-a2a/src/listener.rs`

## 1. 背景

A2A v1.0 要求 server 在 `/.well-known/agent-card.json` 公开 Agent Card 用于发现。duthub 的
每 DUT A2A 端点绑定一个 DUT，其 Card 若照抄连接配置（DUT 名称、COM 口、工作目录、主机名、
token）会把设备拓扑泄露给任何能取到 Card 的进程。`docs/reviews/AGENT_INTEGRATION_BEST_PRACTICE.md`
事实 17 明确认证方案由 Card `securitySchemes` 声明、server 逐请求认证；事实 11 明确
`A2A-Version` 协商（缺省按 0.3 处理）。

## 2. 决定

### 2.1 Card 内容（`build_agent_card(port)`）

| 字段 | 值 | 理由 |
|---|---|---|
| `name` | 固定 `duthub` | 不随 DUT 名称/agentName 漂移；Card 是端点自述，不是 DUT 档案 |
| `description` | 固定中性描述（本机单 DUT 证据端点） | 不含 DUT 名 |
| `version` | workspace 版本 | 与发布对齐 |
| `supportedInterfaces` | 恰 1 条：`url = http://127.0.0.1:<port>`、`protocolBinding = JSONRPC` | 回环字面量非主机名；只声明实际支持的绑定 |
| `capabilities` | `streaming=false`、`pushNotifications=false`、`extendedAgentCard=false` | 服务端把 SendStreamingMessage/SubscribeToTask/push/GetExtendedAgentCard 全部以 `-32601` 拒绝（`StrictRequestHandler`），Card 与行为一致（§13.3：显式 `false` = 拒绝） |
| `securitySchemes` | `duthub-bearer` = `httpAuth(bearer)` | 与 HTTP 侧 401 + Bearer 校验一致 |
| `securityRequirements` | `[{duthub-bearer: []}]` | 声明全部 JSON-RPC 方法必须持 Bearer |
| `skills` | 8 条证据技能（`dut_list`/`dut_info`/`dut_control_status`/`serial_read`/`analyze_serial`/`log_segments`/`log_segment_read`/`heartbeat_get`），描述为通用能力文案 | 与服务端白名单（`EVIDENCE_TOOL_NAMES`）一致；文案不含 DUT 细节 |
| `provider`/`documentationUrl`/`iconUrl`/`signatures` | 缺省不输出 | 不引入外网 URL |

**禁止出现**（Card 序列化结果的硬断言，crate 内 e2e 覆盖）：

- 顶层 `url` / `preferredTransport` / `security`（A2A 0.2.x 遗留字段；v1 用
  `supportedInterfaces` + `securityRequirements`）；
- DUT 名称、DUT ID、COM 口/串口号、文件路径、主机名、token/keyring 字样；
- 任何非 `127.0.0.1` 的地址。

**体积上限**：序列化后 ≤ 16 KiB（超限视为实现缺陷，测试护栏）。

### 2.2 服务行为

- Card 路由公开（无需 Bearer）：发现必须无凭据可用，且 Card 本身按 §2.1 不含敏感信息。
- Card 响应带 `Cache-Control: max-age` / `ETag`（`a2a-server-lf` 内建）；
  外层中间件对**所有**响应（Card 与 JSON-RPC）追加 `Vary: A2A-Version`（append，不覆盖
  Card 路由已有的 `Vary: Origin`）——版本协商结果随请求头变化，代理不得缓存错版本。
- 缺失/不支持的 `A2A-Version` → 信封错误码 `-32009`（`a2a-server-lf` 内建：缺省按 0.3，
  本实现只支持 1.x）。

## 3. 不实现 / 边界

- 不做 per-caller 扩展卡（`GetExtendedAgentCard` 拒绝）；不签名 Card（`signatures` 恒缺省）。
- 非默认回环 `bind_addr` 下 Card URL 仍为 `http://127.0.0.1:<port>`（ADR-2：此类配置在 W3
  一律 `NeedsProductSignoff`，不 bind 不发卡）。
- Card 不承担发现清单职能：多 DUT = 多端点、多 Card；不提供聚合计数的「总卡」。

## 4. 签字要求

若后续需要把 DUT 名称等设备上下文写进 Card（例如为了 Agent 可读性），必须修订本 ADR 并
重走签字；当前状态为实现与测试的硬约束。
