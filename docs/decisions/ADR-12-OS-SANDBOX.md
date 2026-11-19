# ADR-12：Agent 命令的 OS 级沙箱（duthub-sandbox，双身份分层）

- 状态：Proposed（未签字；签字流程见 `docs/decisions/ADR_SIGNOFF.md`）
- 日期：2026-11-25（实现轮同日更新；同日「专用身份」轮改写 §1/§4.1/§6.3/§6.4/§8）
- 阶段：Agent 集成收尾（W2b 后续，P2）
- 相关：`docs/decisions/ADR-11-OS-ISOLATION.md`、AGENTS.md 坑 #61/#111/#112、`docs/SECURITY.md`
- 代码：`crates/duthub-sandbox/`（策略 + Windows 后端 + 状态 + 安全回归测试）、
  `crates/duthub-core/src/os_acl.rs`（目录 DACL 加固与探测，保留）

## 1. 背景与结论摘要

cwd、路径前缀或命令白名单都不是边界——子进程能用绝对路径访问任意位置，真正的边界只能来自内核：
**子进程运行在什么令牌下**。本轮把沙箱从 `duthub-core` 里的策略占位改成独立 crate
`duthub-sandbox`，并给出两个互不替代的身份：

| 调用方 | 身份 | 内核机制 | 网络 | 读隔离 |
| --- | --- | --- | --- | --- |
| Agent 主程序（goose） | 专用本地账户 `DuthubSandbox` 的 `WRITE_RESTRICTED` 令牌 + 每 DUT 随机 capability SID | 子进程的**用户 SID** 是沙箱账户；写还要过 restricting SID | 保留（可达 127.0.0.1 的 MCP） | **按账户身份隔离**（沙箱账户对宿主资料无 ACE），并对受保护路径写 deny-read ACE |
| 脚本（PowerShell） | 同上账户的登录令牌 + 零 capability AppContainer（每 DUT 一个 profile） | AppContainer/SID + Job Object | 拒绝（含回环） | 同上：容器的用户 SID 是沙箱账户，另加针对该 SID 的 deny-read ACE |

**降级（本机当前状态）**：专用账户不可用时（未初始化，**或已初始化但当前进程未提权**），
两条链回退到"宿主用户身份"（受限令牌用宿主令牌、AppContainer 用宿主令牌）。此时
**写/网络/进程树隔离仍然成立，读隔离不成立**——因此任何 `deny_read` 非空的策略会被**拒绝**
（fail closed），`status().read_deny` 与 `status().identity` 均为 `Unverified`,
`agent_isolation()` 为 false。

> 本机已初始化账户，但桌面应用是非提权进程，`CreateProcessAsUser` 必然 1314（§4.1.3），
> 所以当前实际生效的就是这条降级路径：上表"读隔离"一列**尚未在本机成立**。

`duthub-core` 不再保留 `os_sandbox` 模块（避免两份真相），只保留目录 DACL 加固与 OS 隔离探测。

## 2. 分层与接口（唯一的安全执行层）

```
crates/duthub-sandbox/
  src/policy.rs    纯逻辑：SandboxPolicy / CellSpec / ScratchLayout / 脚本限制 / 环境白名单
  src/backend.rs   唯一 OS 入口 trait SandboxBackend（prepare / launch / status）
  src/windows/     唯一允许 unsafe 的目录
    identity.rs    专用本地账户：DPAPI 封存的口令、一次性提权初始化（NetUserAdd 等）
    dpapi.rs       CryptProtectData / CryptUnprotectData（机器范围，失败即失败）
    token.rs       capability SID（宿主侧存放 + 校验）、沙箱账户登录、WRITE_RESTRICTED 令牌
    no_reparse_dir.rs  QueryDosDeviceW + NtCreateFile + OBJ_DONT_REPARSE 的目录打开
    deny_read_acl.rs  受保护路径的 plan / 物化 / 回滚
    acl.rs         句柄式 grant / deny-read / 受保护 DACL（写入后一律回读）、探测
    container.rs   AppContainer profile（create-first）、探测
    process.rs     Job Object 限额、管道、CreateProcess*、恢复前令牌回验、超时杀树
  src/status.rs    能力探测与诚实枚举（无单一 "sandboxed" 布尔）
  src/stream.rs    长驻进程句柄（stdin 持续可写、stdout/stderr 存活期可读、terminate/Drop 杀树）
  src/unsupported.rs  非 Windows：每个入口显式 Unsupported
tests/security_invariants.rs   安全不变量（T1-T8 + 宿主写边界）
tests/streaming_spawn.rs      长驻生命周期（往返、写边界、树收尾、Job 限额回读）
tests/source_contract.rs       unsafe 只允许在 src/windows、无 Command 回退、外部 crate 不得调安全 API
```

对外 API：`prepare / run / run_script / run_agent / spawn_streaming / status / CellSpec /
SandboxPolicy / SpawnRequest / SandboxedProcess`。**两种生命周期**共用同一条
CreateProcess → 令牌回验 → ResumeThread → Job 限额路径：

- 一次性命令（`run*`）：stdin 是固定字节串，`MAX_TIMEOUT`（600s）上限，输出在进程退出后返回；
- 长驻会话（`spawn_streaming`，ACP Agent 主机）：stdin 持续可写、stdout/stderr 在进程存活期间可读、
  **没有超时**；结束由调用方 `terminate()` / `Drop` 决定，`KILL_ON_JOB_CLOSE` 是最后的杀树兜底。

`apps/desktop` 与 `duthub-core` 不直接调用 Win32 安全 API（源码契约测试钉住；`os_acl` 的
`CreateRestrictedToken` 仅用于能力探测、不 spawn，属既有例外）。

## 3. 微软官方文档（2026-11-25 读取 learn.microsoft.com）

- **AppContainer isolation**（`/windows/win32/secauthz/appcontainer-isolation`）：
  AppContainer 以「用户 + 应用」合成身份运行，文件/注册表/网络/设备默认最小权限，网络需显式能力。
- **CreateProcessInSandbox APIs**（`/windows/win32/secauthz/createprocessinsandbox`）：
  只有 `Experimental_CreateProcessInSandbox` / `Experimental_CreateProcessAsUserInSandbox`，
  文档明写「experimental and subject to change」，**最低仅 Windows 11**、无公开头文件（需
  `GetProcAddress` 从 `processmodel.dll` 取）、沙箱规格是 FlatBuffer（`SandboxSpec.fbs`）。
  结论：**不采用**——实验 API + Windows 11 门槛 + 无头文件，不能作为产品边界。
- 同一文档确认：AppContainer 进程的完整性级别**恒为 Low**，不接受调用方指定更高 IL。

## 4. 参考实现：openai/codex `codex-rs/windows-sandbox-rs`

读的是 main 分支源码（`cap.rs`、`token.rs`、`deny_read_acl.rs`、`no_reparse_dir.rs`、`process.rs`）：

- **cap.rs**：每个 workspace / 额外可写根一个随机 capability SID（`S-1-5-21-a-b-c-d`），持久化在
  `codex_home/cap_sid`，重复启动复用。
- **token.rs**：`CreateRestrictedToken(DISABLE_MAX_PRIVILEGE|LUA_TOKEN|WRITE_RESTRICTED)`，restricting
  SID = capability + 登录 SID + Everyone；令牌**默认 DACL 改为登录 SID**（否则子进程在 DLL 初始化阶段
  直接 `STATUS_DLL_INIT_FAILED`）。
- **deny_read_acl.rs**：deny-read ACE 同时保留词法路径与 canonical 目标，缺失路径先物化再授权，
  失败回滚已加 ACE。代码注释明确：**非提权后端无法让 capability-SID 的 deny-read 在
  `WRITE_RESTRICTED` 令牌下生效**（限制 SID 只参与写检查），因此读隔离要走提权/独立账户。
- **no_reparse_dir.rs**：`QueryDosDeviceW` 解析卷、`NtCreateFile + OBJ_DONT_REPARSE`、目录内 guard
  文件，防 junction 把授权重定向到别处。
- **process.rs**：`CreateProcessAsUserW` + `CREATE_SUSPENDED`、`ProcThreadAttributeList` 的
  handle list / job list、私有桌面。
- **identity.rs / dpapi.rs / setup.rs**：一次性提权创建**专用本地组 + 本地账户**，随机口令用
  DPAPI（`CRYPTPROTECT_LOCAL_MACHINE`）封存在用户目录，之后非提权进程只做 `LogonUserW`。

本 crate 采用其令牌配方（capability + 登录 SID + Everyone、默认 DACL 授登录 SID）、其
`NtCreateFile + OBJ_DONT_REPARSE` 的目录打开形状、其 deny-read 的「词法 + canonical、缺失物化、
失败回滚」，以及其**专用账户**方案（§4.1）；**不**引入私有桌面（实测本机不需要）与
`create_directory_guard`（本 crate 的授予对象是单元根与受保护路径，不需要"防目录变空"守卫）。
Licence：openai/codex 为 Apache-2.0，被移植文件的文件头已标注来源，`NOTICE` 已追加归属说明。

## 4.1 专用沙箱身份：照搬 Codex 的方案（本轮核心改动）

### 4.1.1 为什么必须是独立身份（实测，2026-11-25）

AppContainer 令牌是**加法**而不是减法：探针脚本在容器内打印 `[WindowsIdentity]::GetCurrent()`
得到 `User=S-1-5-21-…-1001`（宿主用户）与 `Everyone / Users / Interactive / Authenticated Users /
CONSOLE LOGON / …` 全组成员，**没有** `ALL APPLICATION PACKAGES`、也没有容器 SID。对同一份
`%TEMP%` 下的兄弟单元文件逐个 SID 做 deny 探测：

| deny 的 SID | 结果 |
| --- | --- |
| 容器 package SID（`S-1-15-2-…`） | 仍可读（ACE 生效但不在判定路径上） |
| `ALL APPLICATION PACKAGES`（`S-1-15-2-1`） | 仍可读 |
| capability SID（从未进过容器令牌） | 仍可读 |
| `Everyone`（`S-1-1-0`） | **拒绝** |

即：读取由**宿主用户身份组**授权，容器 SID 只额外加权限。所以不存在"给容器 SID 写 deny 就能
读隔离"的做法；唯一可用的形状是让子进程运行在**另一个主体**下（专用账户），再对该主体的 SID
写 deny-read ACE 作为第二道防线。这与 Codex 的结论一致。

### 4.1.2 初始化（一次性、需要管理员）

```text
# 只读检查（不需要提权）
cargo run -p duthub-sandbox --example provision-sandbox -- --check

# 管理员终端里执行一次（创建组 DuthubSandboxUsers + 账户 DuthubSandbox + 随机口令）
cargo run -p duthub-sandbox --example provision-sandbox
```

- 代码入口：`duthub_sandbox::provision_identity()`（`NetLocalGroupAdd` / `NetUserAdd` /
  `NetLocalGroupAddMembers`）；非提权调用**只返回明确错误**，不创建任何东西。
- 口令 40 字符（大写+小写+数字+符号，128 位 OS 随机），`UF_DONT_EXPIRE_PASSWD`；用
  `CryptProtectData(CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN)` 封存到
  `%LOCALAPPDATA%\duthub\sandbox\identity.txt`，该目录与文件的 DACL 只有
  「当前用户 + SYSTEM + Administrators」，并在每次加载时**回读校验**（可被 Everyone/Users 写就拒绝使用）。
- 幂等：已能用则不改；账户存在但口令失效则轮换口令（先写账户、后落盘凭据）。
- **不会隐式触发**：`run/run_script/prepare/status` 都不创建账户、不弹 UAC；初始化只由上面
  这条显式命令（或产品后续显式按钮）触发。
- **凭据目录可改**：`duthub_sandbox::set_identity_root(Some(dir))`、环境变量
  `DUTHUB_SANDBOX_IDENTITY_ROOT`、或 `provision-sandbox --root <dir>` /
  `provision_identity_in(dir)`。优先级 = 显式设置 > 环境变量 > `%LOCALAPPDATA%\duthub\sandbox`。
  见 §4.1.4 的已知偏差。
- **初始化 ≠ 可用**：初始化只解决"有没有这个主体"。能否以它启动进程还取决于调用进程的特权
  （§4.1.3），两者都由 `status().identity` 统一反映。

### 4.1.3 现实边界：非提权进程无法 `CreateProcessAsUser`（Win32 1314，本轮实测）

初始化完成后立刻实测（2026-11-25）：

```
cargo test -p duthub-sandbox --test security_invariants      # 初始化后首次运行
→ 6 passed / 11 failed，全部真实启动都是
  Spawn("CreateProcess 失败（…\powershell.exe）: 客户端没有所需的特权。 (os error 1314) (Win32 1314)")
```

跨账户启动使用 `CreateProcessAsUser`。本实现检查启动器令牌是否持有
`SeAssignPrimaryTokenPrivilege` 和 `SeIncreaseQuotaPrivilege`。Windows 可在调用时暂时启用已持有的特权。
本机经 UAC 提升后，令牌持有尚未启用的 `SeIncreaseQuotaPrivilege`，仍缺少 `SeAssignPrimaryTokenPrivilege`。
因此，单次 UAC 提升无法满足此机器上的跨账户启动条件。

本 crate 的处置（fail closed，不做"先试再吞错降级"）：

1. `status()` / `prepare()` 通过 `GetTokenInformation(TokenPrivileges)` 检查启动器实际持有的权限。
   缺少条件时 `status().identity = Unverified`，`detail` 列出缺少的具体权限。
2. `identity` 不是 `Verified` 时，`prepare/spawn` 直接走**已验证的宿主受限令牌路径**
   （写/网络/进程树/Job 仍受限），**绝不**先调用 `CreateProcessAsUser` 再回退。
   `availability()` 在不可用时连 `credentials` 都不返回，调用方拿不到半个身份。
3. `deny_read` 非空时仍是硬拒绝（`Setup`，消息里带上面那条具体原因）。

**因此：读隔离在本仓库当前的分发形态（非提权桌面应用）下不可交付**，`status().read_deny`
与本机 T3/T4 都如实报 `Unverified` / SKIP。两条可行路径（**均未实现**）：

- **路径 ①（Codex 的做法）：一次性提权 helper。** 一个独立可执行体以管理员身份启动，
  通过命名管道（framed/protocol 版本号 + 长度前缀）接收"启动这个单元里的这个程序"的请求，
  在提权上下文里完成 `CreateProcessAsUser` 并把进程/管道句柄传回非提权父进程。
  需要设计：helper 的认证（父进程 PID + 每会话随机 nonce）、请求格式与超时、句柄传递
  （`DuplicateHandle` 到父进程）、helper 生命周期（空闲退出）、以及"helper 未运行时明确拒绝"。
- **路径 ②：`CreateProcessWithLogonW`。** 它不需要 `SeAssignPrimaryTokenPrivilege`，
  非提权进程即可用专用账户启动子进程；代价是**没有 AppContainer**，因此
  没有零 capability 的网络隔离——要挡外网只能按用户加防火墙规则
  （`New-NetFirewallRule -User <sid> -Direction Outbound -Action Block`），
  而该规则会**同时挡掉回环**，必须显式放行 `127.0.0.1`（MCP 端口），或维持"双身份"
  （一个能联网的 Agent 主程序身份 + 一个断网的脚本身份）。这需要管理员一次性配置防火墙，
  与"零管理员前置条件"冲突时只能放弃网络隔离或放弃路径 ②。

在任一实现落地并通过"子进程 `whoami` 显示 `duthubsandbox`"的端到端用例前，
**不得在任何文案里写"读隔离已实现/已沙箱化"**。

### 4.1.4 凭据位置（已知偏差）

默认写在 `%LOCALAPPDATA%\duthub\sandbox\identity.txt`（C 盘用户目录），与仓库
「数据只落项目目录」的约束不一致。偏差是有意的：凭据是**机器/用户级**的秘密（本地账户口令），
跟着 exe 目录走会让便携副本继承别人的口令。缓解与后续：

- 内容始终是 DPAPI 机器范围密文（不是明文口令），且目录/文件 DACL 只有
  当前用户 + SYSTEM + Administrators，加载时回读校验；
- 宿主可显式改到项目内：`set_identity_root` / `DUTHUB_SANDBOX_IDENTITY_ROOT` /
  `--root`（见 §4.1.2）。**默认值仍是用户目录**，这是 open issue；
- 若后续要求默认也落项目内，需要先解决"便携副本 + 多份项目目录共用同一账户"的口令同步问题。

## 5. 选型：不引入 rappct（沿用上一轮结论）

rappct 0.13.3（MIT）零 capability 时给 `CreateProcessW` 传非空悬垂 `Capabilities` 指针（返回 87），
`env: Some(...)` 同样 87，只有 `env: None`（继承宿主环境）能启动；与本 crate 的「零 capability +
白名单环境」硬要求冲突。因此自写最小 FFI 封装，`unsafe` 收敛在 `src/windows/` 并带 `SAFETY:`。

## 6. 实测（2026-11-25，本机 Windows 10.0.26300 x64，普通用户）

### 6.1 受限令牌（Agent 主程序）

用一次性 spike（`target/ref` 下，已清理）验证配方：`WRITE_RESTRICTED` + restricting
[capability, 登录 SID, Everyone] + 默认 DACL 授登录 SID：

| 检查 | 结果 |
| --- | --- |
| `cmd /c mkdir <cell>\inside-dir` | 成功（exit 0） |
| `cmd /c mkdir %USERPROFILE%\...` | 拒绝（exit 1，stderr「拒绝访问。」） |
| `cmd /c cmd /c mkdir %USERPROFILE%\...`（孙进程） | 拒绝 |
| PowerShell 5.1 在令牌内运行 | 成功，`exit 7` 如实返回 |
| 读 `%USERPROFILE%` / `C:\Windows\System32\drivers\etc\hosts` | **成功**（宿主令牌做的受限令牌只在写检查里使用 restricting SID） |

因此 `SandboxPolicy::agent_cell` 的 `deny_read` 仍为空，`validate_policy` **拒绝**受限令牌模式声明
`deny_read`（不假装能拦）。**读隔离由 §4.1 的专用账户提供**：一旦初始化完成，同一份令牌的用户 SID
是沙箱账户，读 `%USERPROFILE%` 与兄弟单元会被内核拒绝（此时上表最后一行不再成立）。

### 6.2 AppContainer（脚本）

- `DeriveAppContainerSidFromAppContainerName` 对**不存在**的 profile 也会返回 SID；用它启动进程
  `CreateProcessW` 失败 `ERROR_FILE_NOT_FOUND`。必须 **create-first**（`CreateAppContainerProfile`，
  `ERROR_ALREADY_EXISTS` 再 derive）。这是本轮最隐蔽的坑。
- 句柄列表里的 stdin 子/父端写反（把不可继承的写端传给子进程）会让 `CreateProcessW` 返回
  `ERROR_INVALID_PARAMETER (87)`；`GetTokenInformation` 的 `ReturnLength` 不能传 NULL
  （否则 `ERROR_NOACCESS (998)`），`TokenAppContainerSid` 要先查长度再取。
- 零 capability 进程可启动 PowerShell，令牌回验（`TokenIsAppContainer` + `TokenAppContainerSid`
  与单元 SID 相等）通过后才 `ResumeThread`。

### 6.3 安全不变量测试现状（`crates/duthub-sandbox/tests/security_invariants.rs`）

判定口径（本轮统一）：脚本自身**不能**作为证据——PowerShell 的 cmdlet 错误默认非终止，
"打印 DENIED" 可能发生在拒绝之后也可能什么都没发生。每条断言都用
`$ErrorActionPreference = 'Stop'`（或 `try/catch`）+ **宿主侧真实文件/内容状态** + 退出码，
并要求"同一操作在沙箱外先成功"作为基线。

| 用例 | 判定 | 本机结果 |
| --- | --- | --- |
| T1 单元内写 | 文件内容 = `inside-ok` | 通过（对照） |
| T2 单元外写 | 宿主真实路径不存在 **且** 脚本被终止（`completed` 见证文件不存在、退出码非 0） | 通过 |
| T3 读宿主配置 `.duthub` | 回写文件不存在/不含 sentinel **且** `read-denied` 见证存在 | **SKIP**：身份已初始化，但当前进程未提权（1314），见 §4.1.3 |
| T4 读兄弟 DUT 单元 | 同上；并先用 `icacls` 把兄弟单元**授予沙箱账户读**，只有 deny ACE 能拦住 | **SKIP**：同上 |
| T5 回环网络 | 连接被拒（`BLOCKED`） | 通过 |
| T6 孙进程 | 先证明 `cmd /c mkdir <cell>\…` 成功，再断言同形状的单元外 mkdir 失败且目录不存在 | 通过 |
| T7 超时杀树 | marker 由**孙进程**写；超时后 1.5s 内尺寸不再增长，且记录的子进程 PID 不在 `tasklist` 中 | 通过 |
| T8 上限 | 超长脚本/超时/破坏性命令拒绝且不 spawn；另用 `ActiveProcessLimit=3` 实测第 3 个之后的子进程起不来、`ProcessMemoryLimit=256MiB` 实测 1GiB 分配失败 | 通过 |

SKIP 是**诚实跳过**而非放宽：T3/T4 与两个 `whoami` 用例在身份不可用时打印
「SKIP …专用沙箱身份不可用（读隔离需要「专用身份 + 提权创建进程」）…」+ `status().detail`
的完整原因并 `return`，没有 `#[ignore]`、没有删除断言
（`tests/source_contract.rs` 同时钉住"T1-T8 名字必须在、且文件里不得出现 ignore 标注"）。
反向的 fail-closed 也有用例：未初始化时 `deny_read` 非空必须返回 `Setup` 错误
（`read_isolation_is_refused_instead_of_pretending_without_the_identity`），
非提权调用 `provision_identity()` 必须返回点名"管理员"的错误
（`provisioning_is_only_ever_explicit_and_reports_the_elevation_requirement`）。

### 6.4 capability SID 移出可写单元

旧实现把 capability SID 存成 `<cell>/capability.sid`——而单元是 Agent **唯一可写**的目录，
等于让被沙箱约束的一方自己定义身份。现在存在宿主管辖的
`<work_dir>/duthub-data/sandbox/capabilities/<dut_key>.cell`：

- 文件记录 `root=<canonical cell root>` 与 `sid=`，加载时必须与**本单元**的规范化根一致，
  否则报 `Setup`（拒绝复用别的单元的身份）；
- SID 必须符合本 crate 的生成形状（`S-1-5-21-a-b-c-d`，末段 ≥ 1000，非 well-known）；
- 目录与文件 DACL 写后回读，可被 Everyone/Users 写即拒绝使用；
- 单元里遗留的 `capability.sid` 不再读取（旧 ACE 指向一个没人持有的 SID，无影响）。

## 7. 决定

1. `duthub-core` 不再有 `os_sandbox` 模块；策略类型与执行器全部迁入 `duthub-sandbox`（单一真相）。
2. `duthub-sandbox` 是唯一安全执行层；`unsafe` 只允许出现在 `src/windows/`，源码契约测试钉住。
3. 受限令牌（Agent 主程序）与 AppContainer（脚本）分层，不强行统一：前者保留网络、写隔离，
   后者用于不受信模型输出。
4. fail-closed：令牌/profile/ACL/Job 任一步失败即拒绝启动；错误区分 Unsupported / PolicyRejected /
   Setup / Spawn / NotSandboxed / Execution；**没有无沙箱回退**。
5. 安全不变量测试即回归闸门：T1-T8 与宿主写边界必须全绿（T3/T4 在未初始化专用身份的机器上
   按 §6.3 诚实 SKIP）才允许宣称沙箱；任何改动让它们变红都视为回归。
6. **读隔离 = 专用身份 + 提权创建进程**（§4.1.3）：两者同时满足时读隔离由账户身份 + deny-read
   ACE 共同保证，T3/T4 可执行并必须通过；任一不满足时 `deny_read` 一律拒绝、
   `status().identity` / `read_deny` 报 `Unverified`、`agent_isolation()` 为 false——不假装能拦，
   也**不先尝试再降级**。
7. 旧结论「受限令牌无法拒读、只能后续轮次」**已被本节取代**：拒读靠的是**换主体**（专用账户），
   不是换 ACE。受限令牌依旧只挡写，所以受限令牌模式仍不接受 `deny_read` 声明；该模式的读隔离
   来自"子进程不是宿主用户"。

## 7.1 桌面接线（2026-11-25 接线轮）

- `apps/desktop/src/agent_host/acp.rs` 的 `AcpClient` 不再自行 `Command::new` + 自建 Job：
  `spawn_streaming` 用该 DUT 的 `WRITE_RESTRICTED` 令牌启动 goose（或任意 ACP 程序），
  cwd = `<work_dir>/agent/dut/<hex>/sandbox`，Job 限额沿用 16 进程 / 4 GiB / `KILL_ON_JOB_CLOSE`。
  环境仍只含系统白名单 + 调用方注入变量，`TEMP`/`TMP` 固定在单元内（重复键先删再写，
  不依赖 `environment_block` 的「首个拼写优先」）。
- `host.rs` 每次 attempt 启动前调用 `duthub_sandbox::status()`：`restricted_token` 或
  `job_limits` 未 Verified、或 `prepare` 失败时，会话直接 `Failed` 并留下可见注记；
  **没有完整令牌回退**。`CellSpec::for_dut` 的 scope 对超长 DUT id 用稳定 FNV 哈希收进 64 字符。
- `SessionView.os_isolation` 改由沙箱 status + `PreparedCell` 填充（逐能力），不再把
  `duthub_core::probe_os_isolation` 当作边界证明；目录 DACL 加固仍是独立的 best-effort 步骤。
  UI 文案是否升级（`os_sandboxed` → 已验证）仍归 UI 属主，本轮未改任何 UI 文件。
- 长驻入口的 Job 由沙箱 crate 建立一次并只 Assign 一次（`PROC_THREAD_ATTRIBUTE_JOB_LIST`）；
  桌面 `job.rs` 的 `job::spawn` 只保留给 CLI profile 的每轮进程（不在本轮范围），
  goose 路径不再叠第二层 Job。
- 事实边界：脚本/命令的 AppContainer 路径在 `agent_host` 中**尚无调用点**（goose 自己派生的
  工具进程不由 duthub 拦截），该身份仍只有 crate 内测试覆盖；T4 仍红，读隔离未宣称。

## 8. 准入条件（后续轮次）

1. T4 读隔离（**未交付**）：账户已初始化，但非提权进程无法 `CreateProcessAsUser`（1314，§4.1.3），
   因此 T3/T4 目前 SKIP。两条路径二选一：
   ① 提权 helper（Codex 的 `elevated_impl` + framed pipe）——保持 AppContainer 网络隔离；
   ② `CreateProcessWithLogonW` + 按用户防火墙断网——放弃 AppContainer，必须显式放行回环或改双身份。
   落地判据：端到端用例里子进程 `whoami` 显示 `duthubsandbox` 且 T3/T4 真实通过；
   在此之前 UI 与文档不得宣称读隔离。不得改宿主用户 SID 的 ACE（会连带影响 duthub 自身）。
2. 凭据目录（§4.1.4）：默认仍在 `%LOCALAPPDATA%`，与"数据只落项目目录"存在偏差；
   已提供 `set_identity_root` / `DUTHUB_SANDBOX_IDENTITY_ROOT` / `--root` 三个口子，默认值待定。
3. T2/T7 写语义：已改为「不修改宿主真实路径」的可验证断言，并在单元内递归定位 marker（§6.3）；
   后续保持该判定方式，不要回到「脚本自报」。
4. 进程数/内存限额：**已补**真实超限用例（T8 的 `ActiveProcessLimit` / `ProcessMemoryLimit` 两段）。
5. reparse 防御：**已落地** `QueryDosDeviceW` + `NtCreateFile(OBJ_DONT_REPARSE)`，所有授权根、
   受保护路径与 capability 文件都经句柄打开后再改 ACL（`windows/no_reparse_dir.rs`）。
6. 每 DUT 的 AppContainer profile 生命周期：删除 DUT 时 `DeleteAppContainerProfile`（当前未接）。

## 9. UI 约束（不变）

- `os_sandboxed` 只是信任级别声明，不是探测结果；`status()` 逐能力返回 Verified/Unverified/Unavailable，
  UI 必须按能力展示，禁止合并成「沙箱已验证」。
- `apps/desktop/src/agent_ui.rs` 的「OS 沙箱（已验证）」文案在 T1-T8 全绿前仍属过度宣称，
  由 UI 属主修正。

## 10. 明确不做

- 不引入 `rappct` 或其他第三方 AppContainer 封装；不 fork。
- 不使用 `Experimental_CreateProcessInSandbox`（Windows 11 实验 API）。
- 不给受限令牌模式声明读隔离，也不把 deny-read ACE 当成已生效的读边界。
- **不在后台创建本地账户、不弹 UAC**：初始化只走显式的
  `cargo run -p duthub-sandbox --example provision-sandbox`（或产品后续的显式按钮），
  非提权调用只报错。
- **不"先试后降级"**：身份不可用时不会尝试 `CreateProcessAsUser` 再吞掉 1314 回退；
  路径在 `prepare` 之前就已决定（§4.1.3）。
- 不在本轮实现提权 helper 或 `CreateProcessWithLogonW`：两者都会改变分发形态/防火墙，
  需要单独评审（§8.1）。
- 不动真实硬件；桌面接线只改 `agent_host`（acp/host/job）与 `apps/desktop/Cargo.toml`，
  不改 UI 文件，也不在 `agent_host` 写任何 Win32。

## 11. Windows 非提权桌面路线（方案 A）与 MXC 后端扩展缝

- **当前主线（方案 A，已落地于 `crates/duthub-agent` + `crates/duthub-sandbox`）**：
  - **内核写隔离**：沿用 Codex `WRITE_RESTRICTED` 令牌 + Job Object，写权限仅授予当前活跃 DUT 单元目录；
  - **凭据隔离**：MCP Bearer Token 迁移至 OS keyring（`duthub/mcp/<cred_id>`），脱离磁盘明文配置文件；
  - **Codex 风格 PowerShell AST 策略引擎**（`crates/duthub-agent/src/{policy,ps_ast,sandbox_exec}.rs`）：在启动子进程前调用 PowerShell 原生解析器 `Parser::ParseInput`（零执行）提取命令、重定向与 .NET 成员调用，按最严优先格（`Forbidden > Prompt > Allow`）执行「活跃 DUT 可读写、兄弟 DUT 仅只读、禁止访问 `.duthub/` 与宿主凭据目录、禁止编码命令/代码执行/写注册表」的纵深防御。
- **未来后端缝（Microsoft Execution Containers / MXC）**：
  - `SandboxBackend` trait 预留了无缝接入 Windows 11 25H2+ / `microsoft/mxc` (`mxc-sdk`) 的后端缝。待 MXC 在目标 Windows 版本稳定发布后，可新增 `MxcBackend` 实现非提权下的内核级读隔离，无需改动上层 `duthub-agent` 策略或桌面调用方。
