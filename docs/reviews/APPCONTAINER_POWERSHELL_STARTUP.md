# Windows 专用账户下 AppContainer PowerShell 启动失败

## 用户确认的权限边界

duthub 主程序从启动起，以及它启动的 Agent、PowerShell 和后代进程，均使用普通权限。产品启动、查询系统、运行脚本和验收均不得触发 UAC 或依赖 SYSTEM 任务。无法建立所要求的隔离时，应在执行受保护操作前明确报错。

指定工作目录是唯一允许读写用户数据的目录，包含配置、日志、临时文件和脚本产物。允许 Windows 加载器只读加载运行必需的系统 DLL 和程序文件。此例外仅用于程序加载，不授权脚本读取系统目录或其他用户文件。允许通过固定的只读接口查询 Windows 版本、系统架构及设备状态。

下文保留专用账户实验的历史记录。它验证了子进程的部分边界。普通用户直接启动 duthub.exe，以及通过子进程、绝对路径和目录联接访问目录外文件，仍须单独验收。

## 故障与影响

2026-11-26，使用 SYSTEM 启动器和已配置的 DuthubSandbox 账户运行六项验收。受限令牌 Agent 身份用例通过。五项 AppContainer PowerShell 用例在脚本产生首个可观察结果前失败，进程退出码为 0xC0000142（STATUS_DLL_INIT_FAILED）。此码不能确定哪个 DLL 初始化失败。

受影响的验收项包括单元内写入、单元外写入拒绝、脚本账户身份、宿主配置读取拒绝，以及其他 DUT 目录读取拒绝。脚本提前退出不能作为访问被拒绝的证据。

## 已完成的对照

同一台机器、同一测试程序（SHA-256 为 3F606F941DE0AB5F7715B7A631FF5C9DC558F1FC640BFC02042AA65E367AAFDD）在普通用户 AppContainer 下运行，单元内写入和单元外写入拒绝均通过。这两项未使用专用账户。

原始逐项记录在仓库本机的 target/closeout/dedicated-identity-acceptance/，对照记录在 target/closeout/powershell-startup-control/。这些目录属于本地构建产物。

## 调试假设与验收条件

1. 检查桌面继承。启动器以 SYSTEM 运行，子进程使用另一个账户。原实现将 STARTUPINFO.lpDesktop 留为 NULL，按 CreateProcessAsUserW 文档会继承父进程的窗口站和桌面。第一组实验仅在专用身份 AppContainer 启动时传入空字符串，让 Windows 按目标登录会话选择窗口站。
2. 检查配置文件归属。AppContainer 配置文件的创建或复用发生在启动器上下文。需区分 SYSTEM 的配置文件与目标账户的配置文件，并单独验证对启动的影响。
3. 使用相同六项用例验证，记录通过、失败和跳过的数量。身份用例须观测到 DuthubSandbox；读拒绝用例须有脚本开始标记和拒绝标记。继续使用原有 AppContainer、令牌回验、零 capability 和 Job 限制。

[Microsoft 的 CreateProcessAsUserW 参数说明](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-createprocessasuserw)

## 专用身份实测结果与修复

原失败发生在服务会话。仅加载账户配置文件或在目标账户下初始化容器配置文件仍复现 0xC0000142。给每次登录建立独立的低完整性窗口站和桌面后，PowerShell 已能输出错误，退出码变为 -65536，错误说明注册表访问被拒。再加载 DuthubSandbox 的用户配置文件并在该身份下准备 AppContainer，脚本成功执行。保留独立窗口站而省略桌面创建的反向对照再次复现 0xC0000142。

最终实现以登录 SID 和容器 SID 授权私有窗口站、桌面，并由宿主保持所需句柄。创建桌面时，Win32 API 依赖调用进程的当前窗口站。沙箱启动器在互斥区内临时切换并恢复窗口站。沙箱以挂起状态创建，继续回验账户与容器 SID，再恢复执行。

| SYSTEM 启动器实测项目 | 结果 |
| --- | --- |
| PowerShell 在单元内创建文件，宿主读回预期内容 | 通过 |
| 脚本写入尝试标记后，单元外写入被阻止 | 通过 |
| PowerShell 使用专用账户 | 通过 |
| Agent 宿主使用专用账户 | 通过 |
| 读取模拟宿主配置被拒 | 通过 |
| 读取另一个 DUT 的模拟资料被拒 | 通过 |

六项均实际执行，零跳过。本机证据见 target/appcontainer-fix/dedicated/result.json 及同目录逐项日志。launcher.json 记录临时 SYSTEM 任务已注销。普通桌面进程仍须具备创建专用身份子进程的权限，要求读隔离而身份不可用时会拒绝启动。
