# Model connection test architecture — V33

## Protocol adapters

| Provider | Request API | Model list |
|---|---|---|
| OpenAI | `POST https://api.openai.com/v1/chat/completions` | `GET /v1/models` |
| Anthropic | `POST https://api.anthropic.com/v1/messages` | `GET /v1/models` |
| Google Gemini | `POST https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent` | `GET /v1beta/models` |
| DeepSeek | `POST https://api.deepseek.com/chat/completions` | `GET /models` |
| Qwen (DashScope Beijing) | `POST https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions` | `GET /models` (may differ) |
| OpenAI-compatible | `{baseUrl}/chat/completions` | `{baseUrl}/models` |

Only test/list endpoints call these APIs. （2026-11-23 更新：原「The Agent task renderer remains a front-end simulation」表述过时——真实 Agent 会话已经 ACP 接入外部 Agent 进程（`apps/desktop/src/agent_host/`），LLM 面仅保留连接探针。）
Provider-specific region/base URLs can be supplied with OpenAI-compatible mode.

## Frontend state（历史存档——已退役 HTML 桥接架构）

> 2026-11-23 注记：本节描述的是已退役的 HTML 桥接（本地 dev server）时代的前端状态；现行桌面为 GPUI（gpui-kit 0.9.1）直连，凭据经 OS keyring（`crates/duthub-core/src/keys.rs`）保存，不再使用 `sessionStorage` 与 `fetch('/api/llm/test')` 本地桥。以下原文保留作历史记录，不描述现行实现。

- `settings.json`: provider, model name, endpoint, selected ID (NO raw key).
- `sessionStorage`: temporary model ID -> API key map; cleared with the session.
- Save/export/import: preserve only metadata, not credentials.
- Test button uses `fetch('/api/llm/test')` with JSON body and surfaces HTTP status / error.
- The local runner verifies Origin and handles TLS, timeout and provider differences.

## Production migration

（2026-11-23 更新：原「Migrate the bridge into Rust Tauri IPC」为 V33 草案表述——迁移已落地为 GPUI/reqwest 直连 + OS keyring（`crates/duthub-core/src/keys.rs`），不存在 Tauri IPC。）

Migrate the bridge into Rust Tauri IPC; store the credential in the OS secret store.
Keep two separate capabilities: `test_model_connection` and `run_agent_task`; one must not imply the other is implemented.

## Security caveat

A local development bridge is NOT a hosted public gateway. Bind to loopback only, do not expose a reverse proxy publicly, and do not disable Origin checks.
Never log, export or forward API Keys to serial or MCP debug logs. Real model tests consume provider tokens and can be billable.
