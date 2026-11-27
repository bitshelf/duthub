# duthub — 串口调试工作台

使用 AI 编写的个人嵌入式 Linux Agent 调试工具

## 系统要求

- **Windows**：最低要求 **Windows 11（Build 26100 / 24H2 及以上，且系统已启用原生 Process Security Environment / PSEC 1.1+）**
- **不支持** Windows 7、Windows 10 以及未提供原生 PSEC 1.1 的早期 Windows 11 版本

## 安装 Agent Skill

Windows 运行 `duthub.exe` 并启用 MCP 服务；agent 安装 skill，并通过 Streamable HTTP 连接 `duthub.exe`：

```bash
# 安装到当前项目
npx skills add bitshelf/duthub

# 或全局安装
npx skills add bitshelf/duthub -g
```

## Roadmap
 - [x] 支持 HDMI，camera 采集设备信息
 - [x] MCP
 - [ ] OpenOCD(CMSIS-DAP v2, FTDI MPSSE)
 - [ ] 支持 Agent(WIP)
 - [ ] 模拟鼠标，多点触控，模拟键盘
 - [ ] 支持 TUI
 - [ ] 支持 Debian
