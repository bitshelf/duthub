---
name: duthub-mcp-integration
description: Implement or verify duthub MCP stdio and Streamable HTTP, authentication, tool discovery, and evidence access.
metadata:
  internal: true
---

# duthub-mcp-integration

- Follow `rmcp` conventions, `crates/duthub-mcp` (`#![forbid(unsafe_code)]`), and the [MCP contract](../../../docs/API_CONTRACT.md). Keep stdio snapshot/bridge tests distinct from Streamable HTTP listener tests, and cover both modern `server/discover` and legacy `initialize`.
- Enforce fail-closed auth: loopback by default, explicit LAN opt-in with mandatory Bearer, listener-bound principal and DUT grant checks, distinct credential/lease/approval verification before hardware dispatch, and Plan allowlist enforcement at both discovery and dispatch. Redact secrets in `Debug` and logs; consult [security policy](../../../docs/SECURITY.md) before changing scope.
- When adding or changing tools, update tool registration, `crates/duthub-mcp/src/skill.rs`, [API_CONTRACT.md](../../../docs/API_CONTRACT.md), and integration tests together. MCP skill tools return text only; desktop GUI export writes files.
