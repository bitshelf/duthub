# ADR-2：A2A 监听绑定拓扑（W3 仅回环 127.0.0.1；LAN/TLS 延后签字）

- 状态：提议（Proposed，未签字——签字流程见 `docs/decisions/ADR_SIGNOFF.md`）
- 日期：2026-11-21
- 阶段：W3（A2A 回环最小面）
- 相关：`docs/reviews/AGENT_INTEGRATION_BEST_PRACTICE.md`（事实 17/18/19）、`docs/SECURITY.md`、
  `crates/duthub-a2a/src/listener.rs`、`crates/duthub-a2a/src/auth.rs`、`crates/duthub-a2a/src/ports.rs`
- 代码：`crates/duthub-a2a`（新增）；`crates/duthub-core/src/model.rs`（`DutA2aConfig.bind_addr`）

## 1. 背景

A2A v1.0 规范对生产部署要求加密传输（HTTP 绑定 HTTPS、gRPC TLS），Agent Card 从
`https://{server}/.well-known/agent-card.json` 发现；每 DUT 端点又持有该 DUT 的只读证据
能力。duthub 的既有 MCP HTTP 面已经是「默认回环、显式开启才绑 `0.0.0.0` 且强制 Bearer」
（`crates/duthub-mcp/src/transport.rs`），明文 HTTP 局域网模式的风险口径已写入 AGENTS.md。

W3 交付 A2A 回环最小面。需要决定：监听绑什么地址、什么条件才允许非回环、以及与
`a2a-server-lf` 默认 fail-open（authorizer 为 `None` 即放行，事实 19）的关系。

## 2. 决定

### 2.1 W3 已实现（本 ADR 生效部分）

| 决定 | 实现 |
|---|---|
| 默认且唯一监听地址 = `127.0.0.1` | `A2aRuntime::start` 对 `bind_addr = None` / `"127.0.0.1"` 绑定回环；endpoint 记 `http://127.0.0.1:<port>` |
| `bind_addr` 显式配置非回环 → 拒绝绑定，状态 `NeedsProductSignoff{reason}` | 不 bind、不占端口（`PortRegistry` 释放）、不发卡；只有用户（产品所有者）在后续轮明确签字后才允许 |
| Bearer fail-closed | 启动时从 token 源读 `a2a/<dut_id>`；缺失/为空 → `Failed{reason}` 且**不 bind**。请求侧缺/错 `Authorization: Bearer` → 401；`DefaultRequestHandler.with_authorizer` 逐请求二次复核（ServiceParams 中的 authorization 头 + task 归属），不依赖传输层单一防线 |
| 端口冲突 fail-fast | `port = 0`、与已注册监听重复、命中 `reserved_ports`（MCP 监听等外部占用）→ `PortConflict`；OS bind 失败同样归 `PortConflict` |
| 明文 HTTP 仅限回环 | 回环外的明文传输**一律不提供**；无任何配置项可在 W3 打开局域网 |

### 2.2 明确不实现（deferred，需本 ADR 签字后另行实施）

- **局域网（`0.0.0.0` 或具体内网地址）**：需要产品签字 + 防火墙口径 + 「强制 Bearer（认证关闭
  即 fail closed）」同 MCP 面一致；明文 HTTP 下 Token 与 DUT 数据可被同网段嗅探。
- **TLS/HTTPS**：A2A v1 生产要求加密传输；回环明文属于「本机进程间」例外，只接受
  `127.0.0.1`，不向局域网外推。TLS 引入（rustls 已在 workspace，loopback 不新增 TLS）另起 ADR。
- **`bind_addr` 支持 `127.0.0.1` 以外的回环字面量**（如 `::1`、`127.0.0.2`）：W3 一律
  `NeedsProductSignoff`，避免 Agent Card URL（`http://127.0.0.1:<port>`）与实际绑定漂移。
- **`A2aConfig.host` 字段**：`settings.a2a.host` 已废弃，不参与任何绑定决策（仅保留旧文件
  反序列化兼容）。运行时只认 `DutA2aConfig.bind_addr`。

## 3. 影响与后果

- 每 DUT A2A 端点只能被同机进程访问；跨机 Agent（goose 外接、远端 LLM harness）在 LAN/TLS
  落地前不可接入 A2A 端点（MCP 面不受影响）。
- Token 生命周期：keyring 条目 `duthub/a2a/<dut_id>`（`duthub_core::keys`）；运行期换 token 走
  `A2aRuntime::set_token`（内存句柄即时生效 + 写回 token 源），旧 token 立刻 401。
- 端口占用统一走 `PortRegistry`：A2A 启动时 reserve、`stop_dut` 时 release；`reserved_ports`
  由宿主传入（W3 中是 MCP 监听端口），A2A 不得抢占。
- 依赖面：`a2a-server-lf` 以 `default-features = false` 引入（无 rustls-tls/native-tls 特性）；
  `cargo tree -p a2a-server-lf -e features` 不得出现 TLS 特性，loopback 面不新增 TLS 依赖。

## 4. 签字要求

签字前不得实施 §2.2 任何一项；签字记录补入 `ADR_SIGNOFF.md` 并更新本文件状态为「已接受」。
