# 关键数据模型与配置语义

## 持久化文件

`settings.json`: 用户/全局配置，`appearance`, `terminal`, `commands`, `tools.adbExecutable`, `flashTools`, `agent`。

`agentAccess.httpPort` 为主 MCP 监听端口。`agentAccess.listeners[]` 保存附加监听器的稳定 `id` 和 `port`；可选的 `dutId` 表示 DUT 专用端点（省略/`null` 为通用监听器），专用端点每台 DUT 至多一个，配一个只读凭据（`read` Grant 仅该 DUT）。`agentAccess.agents[].listenerId` 关联附加监听器，省略该字段时归属主监听器，旧配置可直接读取。`agentAccess.lanAccess` 默认 `false`（只绑回环），开启后监听 `0.0.0.0` 并强制 Bearer。该配置继续用现有原子 JSON 写入，GUI 负责管理端口和凭据。模型 API Token 由系统凭据库保存；每 DUT A2A Token 存 `duthub/a2a/<dut_id>`，不进 JSON。

`duts.json` 保存 `hosts[]`、`duts[]`、共享继电器板 `relayBoards` 和按 DUT 管理的提醒配置。DUT 由稳定 ID 标识，可配置串口、USB 调试、继电器、MCP、A2A、烧录工具、采集和心跳。`a2a` 可省略，配置项为 `enabled`、`port`、`agentName`。桌面启动时按已保存的配置启动实验性回环监听器，任务状态存于内存。配置使用 v1 schema。

「配置文件」页将当前配置导出到工作目录的 `exports/settings.json` 或 `exports/duts.json`。导入按钮从界面显示的配置目录重新读取同名文件。设置导出保留 MCP 监听器及认证开关。MCP 凭据条目及其授权随导出移除，导入后需重新创建凭据。模型和 A2A 的凭据保存在系统凭据库中。

桌面 ADB 可将 DUT 的 `adb.mode` 设为 `auto`，打开 Shell 时从实时枚举中选择唯一未分配的在线设备。手动绑定使用原有 `adb.serial`，HDC 仍使用连接键。Android 预览按能力显示，不影响只有 Shell 的设备。

## 内部类型

实际类型定义见 `crates/duthub-core/src/model.rs`，以下为 domain 模型说明：

- `DutId`: 新建时生成稳定 ID；用户可改 `display_name`，禁止 UI 名称充当主键。
- `SerialBinding { device_path: String, baud_rate: u32, data_bits, parity, stop_bits, flow_control }`
- `AdbBinding { serial: Option<String> }`：仅绑定 USB 序列号，全局可执行路径在 `Settings.tools.adb_executable`。
- `ControlBinding = Disabled | UsbSerial { device_path, driver, channels } | SnmpPdu { address, version, credential_ref, power_outlet }`。
- `McpTransport = Stdio { command, args, cwd, env } | StreamableHttp { url }`。
- `FlashingPreset { id, display_name, executable, help_args }` 在 settings；DUT 的 `FlashBinding { executable, test_args }` 是复制快照。
- `SerialSession { id, dut_id, state, sequence }` 与 UI tab 对应。
- `AgentRun { id, session_id, read_only, stage, evidence_refs }` 关联当前串口数据。
- `ProcessResult { exit_code, stdout, stderr, duration_ms, truncated }` 用于 Flash Test 与 ADB 命令检查。

## 示例 settings.json

```json
{
  "version": 1,
  "appearance": { "theme": "dark", "language": "zh-CN", "platform": "windows" },
  "tools": { "adbExecutable": "C:\\Android\\platform-tools\\adb.exe" },
  "flashTools": [
    { "name": "upgrade_tool", "executable": "upgrade_tool", "testArgs": "--help" },
    { "name": "upgrade_tool-local", "executable": "/opt/rockchip/upgrade_tool", "testArgs": "--help" }
  ]
}
```

## 示例 duts.json 片段

```json
{
  "id": "rk3576-a",
  "name": "RK3576",
  "serial": { "host": "local", "port": "/dev/ttyUSB0", "baud": 1500000, "format": "8N1" },
  "adb": { "mode": "usb", "host": "local", "serial": "RK3576USB003" },
  "relay": { "mode": "disabled" },
  "mcp": { "transport": "streamable_http", "url": "http://127.0.0.1:8787/mcp" },
  "flash": { "executable": "upgrade_tool", "testArgs": ["--help"] }
}
```

**注意**：这里的 `host=local` 仅说明推荐语义；V30 导入时 `host` 必须存在于当前 `duts.json.hosts[]` 列表。示例完整配置见 `examples/duts.json`。

## 版本升级建议

- v1 可视为 UI prototype schema；生产环境优先设计稳定 `version:2`，区分配置源、用户选择、自动发现缓存。
- 对 `flashTools[]` 逐步引入稳定 `preset_id`，并允许 DUT `flashPresetId` 显式选择“固定引用/复制快照”策略，避免改一个预设影响全部 DUT。
- SNMP `credentialRef` 指向 OS 密钥管理器，不要明文保存在 `duts.json`。
