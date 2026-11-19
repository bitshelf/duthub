---
name: duthub-mcp-integration
description: Implement or verify duthub MCP stdio and Streamable HTTP, authentication, tool discovery and evidence access.
---

# duthub-mcp-integration

- Use rmcp and the [MCP contract](../../../docs/API_CONTRACT.md). HTTP tests cover modern `server/discover` and legacy `initialize`. Keep stdio snapshot and bridge tests distinct from HTTP listener tests.
- Verify Bearer binding to the actual listener, per-DUT evidence scope and the Plan allowlist at discovery and dispatch. General listeners and DUT-bound listeners have different read semantics. Consult [security policy](../../../docs/SECURITY.md) before changing scope.
- Update the tool contract, `crates/duthub-mcp/src/skill.rs` and integration tests together when tools change. MCP skill export returns text. Desktop export writes it to disk.
