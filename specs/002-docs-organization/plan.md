# 文档与项目技能整理实施计划

**规格** [spec.md](spec.md)

## 方法

保留架构、数据、API 和安全契约的稳定入口。指南、设计、格式参考与审核各归入对应目录。删除已被当前契约、技能和 Git 历史覆盖的旧计划与清单。保留已跟踪的审计正文与摘要。

项目约束由 [constitution](../../.specify/memory/constitution.md) 路由到 AGENTS.md。沿用 GitHub Spec Kit 的功能工件结构，由仓库手工维护。

## 退役文件

- `IMPLEMENTATION_PLAN.md` 和 `agent/TASKS.md` 的旧里程碑记录由 Git 历史保留。现行回归入口由项目技能维护。
- `AGENT_IMPLEMENTATION_TASKS.md`、`GOOSE_AGENT_FEASIBILITY.md` 和 `RUST_INTEROP_DESIGN.md` 由现行契约、接入指南、ADR 和保留的审核报告继承。
- `SKILL_RECOMMENDATIONS.md` 的操作指引已归入实际的项目技能。

## 验收

校验相对链接、技能的名称和元数据、被移动或删除路径的调用方。运行格式、全工作区 Clippy、测试与桌面自检。
