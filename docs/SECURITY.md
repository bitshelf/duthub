# 安全与可靠性边界

1. **进程执行**：仅 `Command::new(executable).args(argv)`，绝不把任意字符串拼接进 shell；先解析并规范化路径、限制执行目录和运行权限。Windows PATH 与 Linux PATH 都应按实际运行进程的环境解析。
2. **串口独占**：同一 host 的同一 USB 串口不得绑定到两个有效 DUT；串口与继电器 USB 适配器也应检查冲突；Linux 检查 group/dialout/uaccess 权限。
3. **Plan 模式**：Agent 和 MCP 仅能执行白名单内的读操作；两个例外：① 显式授予 `capture` 后 `media_streams` 的取帧/录屏（见下文「采集按需取数」，默认不授予）；② `notify_user`——不触设备、无需审批，但会唤起用户界面，只存在于用户配置的监听器，并受告警开关、抑制与限流约束（见文末「`notify_user` 的边界」）。有效 Token 可以读取工作目录内全部 DUT 文件。在线接口仍按 DUT 授权。
4. **烧录安全**：即使 `--help` 通常只读，也不可完全信任工具；对需要刷写的参数、文件路径、设备选择启用二次确认和独占锁，建议测试阶段使用模拟 executor。
5. **SNMP**：不要明文保存 community/password；推荐 v3，凭据引用在 OS Secret Store；考虑超时与网络隔离、同一 PDU 插座的互斥操作。
6. **ADB**：执行 `adb devices -l` 后验证设备 `state == device`，且 `-s <serial>` 明确指定绑定 DUT；谨防脱机/未授权设备。`adb` 可执行路径从全局配置读取。
7. **MCP**：HTTP 默认只绑 `127.0.0.1`；显式 `agentAccess.lanAccess` 时绑 `0.0.0.0` 且强制 Bearer（认证关闭 fail closed），局域网明文 HTTP 仅限可信网络并配合防火墙。`Host` 白名单在回环监听下保持 `localhost`/`127.0.0.1`/`::1`，非回环/通配监听自动加入本机可用局域网接口 IP 与主机名，并支持经 `agentAccess.allowedHosts` 显式追加（过滤非法字符与通配符，保证白名单非空）；`Origin` 校验默认拒绝一切浏览器 `Origin` 头。同端口挂载的流式镜像上传路由（`PUT /dut/{dutId}/image/{filename}` 与 tus 1.0.0 `/dut/{dutId}/transfer*`）复用同一 `Host`/`Origin`/Bearer 校验与限流，并强制要求目标 DUT 的 `flash` 权限（只读 Token / 跨 DUT 一律 403，拒绝 `..`/符号链接/非 `.img`/`.bin`/`.iso` 后缀，支持 SHA-256 校验与原子落盘，`manual_takeover`/`flashing`/`needs_inspection` 期间返回 409）；`GET /dut/{dutId}/logs/export` 强制要求目标 DUT 的 `read` 权限。DUT 专用端点使用独立端口与只读 Token，跨 DUT 请求返回 `LeaseRequired/UNAUTHORIZED`，日志/文件读取同样受限；通用接入保留既有全局只读语义，但文件/日志读取至少需要一个 Read grant（零 grant 凭据一律拒绝）。`/mcp` 请求体上限 1 MiB（`transport.rs` `MAX_MCP_REQUEST_BYTES`）。文件访问经 `cap-std` DUT 归属目录，拒绝 `..`、绝对路径与 junction/symlink。stdio 子进程配置最小环境变量、有限额 stdout/stderr；对 stdio 的 stdout 不能混入任意普通日志。认证失败限流：同一监听器滑动窗口内 Bearer 失败超过预算后返回 `429`（`Retry-After: 60`），且只记一条聚合 `auth.fail` 审计（防 LAN 暴力猜 token，不泄露 token 是否存在）。审批令牌签发/消费写 `approval.issue`/`approval.consume` 审计时只记 SHA-256 指纹（dut/action/参数摘要），明文令牌永不落审计。
8. **日志**：对 panic/oops 抓取保留原始字节副本，避免 ANSI/编码处理误修改证据；轮换容量限制，清洗敏感串口日志。
9. **文件写入**：settings 与 duts 采用临时文件 + fsync + rename 的原子更新；不同目录权限分离；schema 校验后再提交。
10. **实验室遥控**：显式区分 PC、Dev Host、DUT 三层，remote relay 操作应认证、限流和审计，不信任来自 DUT 的命令回显。
# Windows 工作机上的 Agent 和 MCP

- Duthub 不向 MCP 暴露 Agent Shell；MCP 使用官方 Rust SDK 的工具路由，在服务端只公开固定的读取能力（两个例外：采集取帧/录屏需要显式 `capture` 授权；`notify_user` 不触设备但会唤起用户界面，见下）。外部 Agent 连接内置 HTTP 时使用与其他客户端相同的 Bearer Token：通用接入的 Token 可以读取工作目录下所有 DUT 的资料；DUT 专用接入的 Token 只能读取绑定 DUT，跨 DUT 请求拒绝。服务端的白名单独立于 Agent 的设置。
- **goose 由下载安装提供（2026-11-25 起，取代 ADR-3 的内嵌口径）**：duthub 不再打包任何 goose 字节；用户在「工具下载」页触发 `duthub-tooldown` 从上游 release 下载安装到 `<config>/tools/goose/`，安装内核在 `manifest.json` 记录实测 SHA-256（goose 没有固定 pin，属未验证下载）。启动链 `registry::resolve` 的顺序是「显式外部路径（固定 `expectedSha256`，每次启动复算）> 已下载安装（按 manifest 记录复算现盘字节，字节已变则落空）> PATH 裸名（现算哈希入记录）」，版本门槛 >= 1.60.2；三者都没有时返回可展示的 `RegistryError::NotInstalled`（`registry::goose_install_state()`），不 panic、不静默执行别的东西。每 DUT 一个子进程，Windows 上经 Job Object（`agent_host/job.rs`：CREATE_SUSPENDED → Assign → Resume，`KILL_ON_JOB_CLOSE`、16 进程上限、单进程 4 GiB 内存上限，任一步失败即终止挂起子进程，fail closed）启动，cwd 为该 DUT 的 sandbox，`GOOSE_PATH_ROOT` 指向每 DUT 目录；`GOOSE_MODE=approve`（不用 auto——auto 不发 request_permission 会让工具面静默扩大）；模型 API Key 只经环境变量注入，Token 不落 goose 配置或日志。duthub **不做签名链验证**，徽标对下载/外部程序一律显示「未验证」，不得当作可信。
- goose 许可随上游发行包提供。duthub 发布物附来源说明、许可证、SBOM 和摘要。普通分支产物记录实际签名状态，标签发布要求有效的 Authenticode 签名。当前工作流执行签名校验，发布需要接入签名步骤。
- **A2A 端点（实验性）**：每 DUT 只绑 `127.0.0.1`，非回环绑定一律 `NeedsProductSignoff` 拒绝；无 LAN、无 TLS、无 peer 认证。JSON-RPC 仅 SendMessage/GetTask/CancelTask，其余方法 `-32601`；执行面白名单 8 个证据工具且绑定端点 DUT；请求体上限 256 KiB。任务状态仅内存（无持久化），进程重启即失。**A2A 未经产品签字（ADR-2/8/9 为 Proposed），不得进产品验收、不得作为对外承诺。**
- **Windows Agent 隔离**：托管会话需要受限令牌和 Job Object，能力不可用时拒绝启动。目录 ACL 加固保护持久化记录。专用身份需要管理员初始化及 Windows 启动特权，普通桌面进程无法使用专用身份提供读隔离。见 [ADR-12](decisions/ADR-12-OS-SANDBOX.md)。
- 资料根由可信启动参数或设置指定，缺省为可执行文件目录。DUT 写入使用 `cap-std` 目录句柄和稳定 ID 所有权校验，既拒绝跨设备目录，也不会覆盖现有文件。FFmpeg 截图通过管道交回 PNG，由应用写入授权目录。容量达到上限时只清理所属 DUT 中最旧的未占用文件。
- **采集按需取数（`media_streams` 的 `op`）**：查询分支仍只要 `Read`；`snapshot`/`recordStart`/`recordStop` 需要该 DUT 的 `capture` 授权——它会启动采集、把 PNG/TS 写进该 DUT 目录并计入配额，所以只读 Token 得到 `LeaseRequired/UNAUTHORIZED`，DUT 专用监听器跨 DUT 在 grant 之前就被拒绝。默认没有任何凭据持有 `capture`。`recordStart` 与 `recordStop` 必须配对，**录屏没有时长上限**（单次调用各 30 秒有界，录制时长不计入任何超时），人工可在面板停止采集，绑定变更/DUT 删除/进程退出同样收尾；配额是唯一存储边界。`note` 只作元数据：去控制字符、限 64 字符，不参与文件名、不进审计。
- [AAIF goose](https://github.com/aaif-goose/goose) 可用其 [chat 模式](https://github.com/aaif-goose/goose/blob/main/documentation/docs/mcp/developer-mcp.md) 对手动提供的证据做纯规划，该模式会关闭工具调用。需要自动读取证据时使用已配置的 Duthub MCP，客户端关闭 Developer、Shell、文件写入和动态扩展等能力。本轮已改为由 duthub 托管「工具下载」安装的 goose（见上两条）；本节的手动配置形态仍适用于其他外部客户端，其进程权限由客户端或 Windows 隔离能力管理。
- 如使用 GitHub Copilot CLI，使用其 [Windows 本地沙箱](https://docs.github.com/en/copilot/how-tos/cloud-and-local-sandboxes/using-local-sandboxing)，按需仅放行实际 `target/release` 目录和 MCP 连接。由组织策略禁用沙箱绕过并关闭凭据共享。宿主不满足官方隔离条件时保持只读 MCP，避免启用外部 Agent 的执行能力。

## 采集 op 的授权现状

> 本节由收尾文档轮追加，只补充上文「采集按需取数」的实现事实；上文边界不变。行号为 2026-11-24 实测。

- **`capture` 是独立动作，不隐含在 `read` 里**：`DutAction::Capture` 的配置值是 `"capture"`（`crates/duthub-core/src/agent_access.rs:173`），只出现在凭据的 `grants[].actions`。`media_streams` 只有带 `op` 的分支调 `authorize_capture`（`crates/duthub-mcp/src/server.rs:545`、`:758`），查询分支仍只要 `Read`；DUT 专用监听器的跨 DUT 拒绝由 `authorize` 的 `listener_dut` 检查先于 grant 生效（`:504-508`）。
- **默认确实没有凭据持有 `capture`**：新建凭据的 grants 为空；DUT 专用监听器的 Token 只带绑定 DUT 的 `read`（`apps/desktop/src/agent_host/listener.rs:99`），因此专用端点带 `op` 的调用默认 `LeaseRequired/UNAUTHORIZED`。
- **桌面授权**：凭据矩阵提供「读」和「画面」两列，服务端按请求和 DUT 独立校验。
- **配额是存储边界，不是时长闸门**：录屏没有时长上限（见上）；配额耗尽按 `MEDIA_CAPTURE_FAILED` 返回，码表里没有 `MEDIA_QUOTA`（`apps/desktop/src/capture_backend.rs:155-157`），不要把配额实现成录制计时器。

## `notify_user` 的边界（2026-11-24）

> 通知工具受调用端点、DUT 授权和宿主呈现策略约束。

- **为什么不需要审批**：`notify_user` 只把一条文本交给宿主 UI，不写串口、不动继电器/电源、不改配置、不读设备，因此不进 `approval_request` 的 allowed 清单、不取租约。审批令牌保护的是不可逆的设备动作，给它签发/消费只会制造「能签发不能消费」的假闸门。
- **它能做什么（如实）**：在 duthub 窗口内弹出告警 Dialog；窗口隐藏到托盘时按宿主策略发气泡与图标闪烁。也就是说，**Plan 模式的 Agent 可以唤起用户界面**。这是产品 2026-11-24 拍板的行为，不是越权。
- **约束在哪一层**：设备边界在 MCP（授权 + 参数校验 + 截断）；呈现边界在宿主——DUT 告警开关（`alertsByDut.<id>.enabled=false` → `disabled`；全局总开关已于 2026-11-25 移除）、DUT/来源抑制（→ `suppressed`）、30 秒去重（→ `throttled/dedup`）、每来源容量（→ `throttled/capacity`）、每凭据与全局令牌桶（→ `throttled/rate`）。MCP 侧不对结果二次加工，也不自行丢弃（否则客户端会看到与真实呈现不一致的 `queued`）。
- **授权口径**：通用监听器要求凭据至少有一个 `read` grant；显式 `dutId` 逐 DUT 校验 `read`（持 DUT A 的 Token 不能把通知标到 DUT B）；DUT 专用监听器默认绑定自身 DUT，跨 DUT 一律 `LeaseRequired/UNAUTHORIZED`。这条显式检查不能只靠 `authorize_scoped_read`——它在通用监听器上会退化成 any-read，放过越权 `dutId`。
- **输入卫生**：`message` 去控制字符后按字符截断 200、`title` 截断 48，空正文 `invalid_params`，非法 `level` 也是 `invalid_params`（不静默降级）。通知正文不进审计（`tool.call` 只记 tool/dut/耗时与结果长度），避免把 Agent 生成的文本当审计证据。
- **R6 已知限制**：只覆盖用户配置的通用/DUT MCP 监听器。桌面发起的全部 Agent 会话（托管 goose 与 claude/codex CLI profile，均经 `listener::issue` → `spawn_http_listener_loopback`）不接通知器，端点只有 13 个只读工具，`notify_user` 对它们不可见也不可调用（调用得 `PLAN_MODE_ONLY`）。A2A 证据端（`EVIDENCE_SCOPE_ONLY`）同样没有该工具。后续给 `listener::issue` 增加参数即可覆盖，属独立任务。

## 镜像传输接收端（`/transfer`）

- 独立 crate `crates/duthub-transfer`（Salvo + 官方 `salvo-tus`），**尚未接入产品运行时**；接入前视为实验面，不进分发二进制验收。
- 目录限定：传输根必须是宿主传入的工作根之内（`TransferServer::new_in`，词法归一 + canonicalize 包含校验）；绝对 `transferDir` 在工作根之外一律拒绝；传输根不得是 symlink/junction/reparse point。
- 每 DUT：`TransferServer::for_dut` 绑定 `dut_id` 与 `<dut 目录>/images`，目录所有权由 `.duthub-dut-id` 校验；上传在创建时写入服务端 `dutId` 元数据，后续请求跨 DUT 一律 403。每 DUT 用独立端点与令牌由宿主提供（keyring 待接线）。
- 认证 fail closed：无令牌/未知令牌 401；`Tus-Resumable` 缺失或非 `1.0.0` 在鉴权前直接 412；非规范路径（`//`、`.`/`..` 段、反斜杠、百分号编码）在鉴权前 400。
- 资源边界：每 DUT `dutStorageBytes` 配额（可选，只算载荷；不足时按修改时间淘汰最旧已完成镜像，在途上传不淘汰）+ 全局磁盘预算（507）；零长度上传默认拒绝；未完成上传按 TTL 清扫，孤儿字节/损坏记录/临时文件由清扫回收；同一传输根独占一个写者（锁文件）。
- 已知不做：TLS（局域网明文，需配防火墙与可信网络）、断电一致性（上游不 fsync）、删除 DUT 联动删文件（文件人工删除）。
