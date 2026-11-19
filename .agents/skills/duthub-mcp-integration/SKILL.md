---
name: duthub-mcp-integration
description: Implement and verify Duthub MCP transports, Bearer authentication, tools and read-only evidence access.
---

# duthub-mcp-integration

- Use the existing rmcp handlers and [tool contract](../../../docs/API_CONTRACT.md). The customer executable includes loopback Streamable HTTP. Rust stdio supports development compatibility.
- Verify listener-scoped Bearer authentication and the Plan allowlist at discovery and dispatch. Live reads are scoped to the authorized DUT. An authenticated client can read other DUT archives inside the effective work directory.
- Exercise missing, disabled and wrong-listener tokens, direct calls to hidden mutating tools, cross-DUT file reads, traversal rejection and pagination. Update the public contract, generated skill and regressions together. MCP skill_export returns Markdown, and the desktop export writes it to disk.
