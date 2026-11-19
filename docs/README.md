# duthub 文档

## 当前行为

- [架构](ARCHITECTURE.md)、[数据模型](DATA_MODEL.md)、[MCP 与 A2A 契约](API_CONTRACT.md)、[安全边界](SECURITY.md)
- [Agent 接入](guides/AGENT_INTEGRATION_HANDOFF.md)、[镜像传输接收端](guides/TRANSFER_RECEIVER.md)
- [模型凭据](reference/MODEL_CREDENTIALS.md)、[串口记录格式](reference/REC_LOG_FORMAT.md)、[工程参考](reference/PROJECT_REFERENCES.md)
- [配置示例](examples/)、[界面对照规格](ui-spec/)

## 设计与证据

- [设计记录](design/)和 [ADR](decisions/)保留方案、约束及取舍。产品签字以 [ADR 状态表](decisions/ADR_SIGNOFF.md)为准。
- [审核记录](reviews/)保留取证结论。两份 Rust 现代化审核仍有未关闭项，保留[完整报告](reviews/RUST_MODERNIZATION_AUDIT.md)和[摘要](reviews/RUST_MODERNIZATION_AUDIT_SUMMARY.md)。
- [功能规格](../specs/)按需求保留 `spec.md`、`plan.md` 和 `tasks.md`。本轮整理见[功能规格](../specs/002-docs-organization/spec.md)。项目约束见 [constitution](../.specify/memory/constitution.md)，开发与验收入口见 [AGENTS.md](../AGENTS.md)。
- [HTML 原型](duthub.html)供预览和资产生成，产品验收走 Rust 桌面版。操作指引见[回归技能](../.agents/skills/duthub-regression-test/SKILL.md)。
