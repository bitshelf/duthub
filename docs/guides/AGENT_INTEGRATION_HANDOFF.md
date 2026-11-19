# duthub Agent 集成交接

## 当前入口

- 桌面宿主和 ACP 客户端位于 `apps/desktop/src/agent_host/`。会话按稳定 DUT ID 归属，每个托管会话使用独立的 MCP 证据监听器。
- 从「工具下载」安装 goose，或配置外部程序路径。安装记录区分归档摘要和可执行文件摘要，托管会话启动前复核程序字节。模型凭据保存在系统凭据库。
- Windows 托管会话使用受限令牌和 Job Object。专用身份的启用条件见[安全边界](../SECURITY.md)。
- 实验性 A2A 按 DUT 绑定回环，任务状态存于内存。产品签字以 [ADR 状态表](../decisions/ADR_SIGNOFF.md)为准。
- 镜像接收端可通过 `duthub-transfer` 示例运行。桌面配置、授权与监听生命周期仍待接线，参阅[接收端说明](TRANSFER_RECEIVER.md)。

## 开发与验收

- 外部接口参阅 [MCP 工具契约](../API_CONTRACT.md)，配置和资料路径参阅[数据模型](../DATA_MODEL.md)。
- 从仓库根目录运行 `bash scripts/regression`。分别记录模拟、真实 goose 与专用 Windows 身份用例的结果。
- 历史实现与审计记录见 [Agent 实现报告](../reviews/AGENT_IMPLEMENTATION_REPORT.md)。
