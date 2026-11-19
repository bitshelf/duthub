# 镜像传输接收端安全审核报告（duthub-transfer）

- 审核对象：`crates/duthub-transfer`（Salvo `tus` 1.0.0 镜像接收端）
- 审核日期：2026-11-25（第二版，按读稿意见修订）
- 判定依据：本轮任务说明给出的五条验收基准（用户 2026-11-24 拍板，第 5 条 2026-11-25 追加）。**仓库内没有这五条的独立文档**；本报告按任务说明复述，若有出入以用户口径为准。
- 审核方式：逐行源码复核 + 示例接收端端到端复现 + 静态测试计数；只读审核，未改任何代码
- 报告文件：`docs/reviews/TRANSFER_SECURITY_REVIEW.md`

---

## 1. 结论

### 1.1 总判定（按范围拆开）

`crates/duthub-transfer` 目前**尚未接入产品**：没有宿主 crate 依赖它，`SettingsFile` 没有 `transfer` 段，`PortRegistry` 没有 transfer 归属，keyring 没有 transfer 条目，产品进程不会监听 `/transfer`（证据见 §2.4）。因此下列缺陷是「接线即触发」的设计级缺陷，不是当前分发二进制的现网漏洞；但 F-01 已能在仓库自带的示例接收端上直接复现。

| 基准 | transfer 面判定 | 全局范围/说明 |
| --- | --- | --- |
| 1. 目录限定 | **不满足**（F-03） | 只审了 transfer 根；串口日志、截图/录制、`DutQuota`、`dut_artifacts`、Agent 目录等其它写入面**没有逐一审计**，不能据本报告宣称全局基准 1 满足。`<exe>/.duthub` 与任意绝对 `workspaceDir` 是既有契约，是否豁免需产品确认。 |
| 2. 通道唯一 / 认证失败 fail closed | **不满足**（F-01/F-04/F-17） | `TransferGuard` 的认证在规范路径上无令牌 401；`dut_files`/`dut_file_read` 只读；A2A 源码只绑回环且无文件写入。但 F-01 实测多点段/双斜杠绕过；`authorize_resource` 读失败放行；授权还依赖 `Tus-Resumable` 头。其它 MCP/设备写入面（`media_streams` 的 capture、`skill_export`、ADB/HDC/烧录）**未系统枚举**，全局基准 2 不成立。 |
| 3. 每 DUT 严格隔离 | **不满足**（F-02） | 整条链路没有 DUT 维度；唯一租户轴 `board` 是可省略的客户端元数据；无每 DUT 端点/令牌/存储根。 |
| 4. 删除不要求级联，但位置正确、可按 DUT 找到 | **不满足**（F-02/F-03） | 扁平 `data/<filename>`；MCP `dut_files` 根与传输根无包含关系。现有删除流程清端点/凭据/Agent 目录、不删 DUT 目录（符合「无需级联」），但「按 DUT 找到」仍不成立。 |
| 5. 解耦与可演进性 | **不满足** | 路径、租户、存储、端点生命周期焊在 HTTP crate 内；新增 DUT 维度需同步改 10 处以上（§5.3）。§5 给出目标分层，但它是设计，不是现状。 |

### 1.2 最大风险

**F-01：非规范路径使整个鉴权守卫被跳过，且路由仍把请求交给 tus handler；`authorize_create` 的校验（文件名白名单、board、预算、subject 绑定）全部不执行。**

本轮用仓库既有的 `target/debug/examples/receiver.exe`（2026-11-25 00:22 构建，静态令牌 `review-token`，数据目录在 `%TEMP%` 下，测后 `taskkill` 并删除）复现：

- 规范路径无令牌 `POST /transfer` → **401**（对照）；
- 无令牌 `POST //transfer` → **201**；`POST ///transfer` → **201**；
- 无令牌 `POST /./transfer` → **201**；`POST /foo/../transfer` → **201**（`curl --path-as-is`，`-v` 确认请求行）；
- 无令牌 `POST //transfer` + `filename=evil.exe` → **201**；同 metadata 走规范路径带令牌 → **400**；
- 无令牌 `POST //transfer` + `filename=CON.tar.img` → **201**；
- 无令牌 `PATCH //transfer/<id>` → **204**；错误令牌 `HEAD` → **200**（`upload-offset: 5`）；错误令牌 `GET` → **200** 且响应体为已传内容；无令牌 `DELETE //transfer/<id>`（在途）→ **204**；
- 通过 F-01 创建的 sidecar 只有 `filename`，没有 `subject`（`metadata/<id>.json` 实测）；零长度上传走规范路径时 sidecar 有 `subject`。

根因是「守卫看原始 URI，路由看归一化后的 URI」：`TransferGuard::handle` 用 `req.uri().path()` 调 `target()`（`router.rs:111-115`），`target()` 只做 `strip_prefix(base)` 加 `Mount/Upload/Malformed/Outside` 分类（`router.rs:190-207`）；`//transfer`、`/./transfer`、`/foo/../transfer` 都不以 `/transfer` 开头 → `Target::Outside` → `ctrl.call_next(...)` 直接放行。路由侧 `salvo_core-1.2.1/src/routing/path_state.rs:324-367` 的 `parse_path_parts` 会跳过空段、丢弃 `.`、对 `..` 弹栈，再把请求交给 tus handler。`fix-http1-request-uri` 只补 scheme/authority（`salvo_core-1.2.1/src/service.rs:483-497`），**不折叠 path**；第一版报告把它当根因是错的。

F-01 的后果不止「无令牌读写删」：

1. **任意扩展名/保留名**：绕过 `authorize_create` 后 `names::validate_publish_name` 不执行；`evil.exe` 实测 201（规范路径带令牌 400），`CON.tar.img` 也 201（该名即使走校验也会因 F-12 的多段保留名缺陷通过）。此时唯一的发布名防线是上游 `DiskStore::sanitize_filename`（`salvo-tus-1.2.1/src/stores/disk.rs:333-345`）：它用 `Path::file_name()` 剥目录、拒绝 `.`/`..`，但**不校验扩展名和 Windows 保留名**。
2. **无预算预留**：`free_space` 检查也跳过，可创建超过剩余空间的上传。
3. **无 subject 绑定**：sidecar 没有 `subject`，后续 `authorize_resource` 在 `upload_subject=None` 时跳过主体检查（`authorize.rs:149-159`）；任何已认证 token 都能对这些上传执行 PATCH/HEAD/DELETE（元数据已实测无 subject；双 token 运行时未做，见 §3 F-02）。

修复不能只补 `//`：应在最外层 hoop 对路径做**规范形态门禁**（拒绝 `//`、`.`/`..` 段、反斜杠、百分号编码、非法尾斜杠），再进入路由；并保证鉴权/名称/预算在同一个必须在 handler 之前执行的 guard 内。回归至少覆盖 `//transfer`、`///transfer`、`/./transfer`、`/foo/../transfer`、`/%2Ftransfer`（用户复核 `/%2Ftransfer` → 404，本次未重跑）。

### 1.3 测试基线

- ask 提供的基线：`cargo test -p duthub-transfer` 退出码 0，共 92 个测试（50 + 8 + 7 + 20 + 4 + 3）；**本次未执行**。
- 复核者用 `grep -cE '^\s*#\[(tokio::)?test\]'` 静态计数同样是 92；我本轮复跑了该静态计数，结果一致（`src=50`、`auth_failclosed=8`、`happy_path=7`、`reject_paths=20`、`resume_restart=4`、`ztus_client=3`），但静态计数不等于测试通过。
- 全绿不构成安全证明：`//`/点段绕过没有用例；无 board 创建被 `tests/ztus_client.rs:33-39`、`:63`、`:98` 钉成预期；绝对 `transferDir` 原样通过被 `config.rs:409-416` 钉成预期。

### 1.4 处置优先级

- **P0（接线前必须）**：F-01 路径规范门禁 + 校验前置；F-02 DUT 维度；F-03 工作根/包含校验；F-17 `Tus-Resumable` 条件式授权（与 F-01 同族，至少先 fail closed）。
- **P1（接线前应完成）**：F-04 fail-open、F-05 每 DUT 配额、F-06 预算预留、F-07 清扫孤儿、F-08 锁边界、F-09 持久化声明、F-10 端点生命周期、F-18 零长度上传。
- **P2（随文档/低危修复）**：F-11 文档一致性、F-12 多段保留名、F-13 绝对路径回执、F-14 marker 接缝、F-15 示例默认令牌、F-16 校验失败仍带 artifact 头、F-19 全局哈希闸。

### 1.5 修复状态（2026-11-25，crate 级已落地）

> 代码改动集中在 `crates/duthub-transfer`；`cargo fmt` / `clippy --all-targets -D warnings` / 103 条测试全绿。标「待接线」的项需要改宿主（core/desktop），本轮未动。

| 发现 | 状态 |
| --- | --- |
| F-01 非规范路径绕过鉴权 | **已修复**：最外层规范形态门禁 + 原始 TCP 回归（`tests/canonical_paths.rs`） |
| F-02 无 DUT 维度 / board 可省略 | **已修复（crate 级）**：`TransferServer::for_dut`（`<dut>/images` + `.duthub-dut-id` 所有权）、服务端绑定 `dutId`、跨 DUT 403；宿主按 DUT 起实例与令牌待接线 |
| F-03 工作根/包含校验/reparse | **已修复**：`new_in(work_root)` 词法归一 + canonicalize 包含 + 拒绝 reparse；绝对 `transferDir` 出界拒绝 |
| F-04 读记录/解码失败静默放行 | **已修复**：仅 `NotFound` 放行给 404，其余 500 `corrupt_upload_record` |
| F-05 无每 DUT 配额 | **已修复（2026-11-25 口径拍板）**：`dutStorageBytes` 配额按 `data/` 载荷 + 在途余量 + 声明长度计算；不足时按修改时间淘汰最旧已完成制品（在途不淘汰），仍不足 507——与 `API_CONTRACT.md:123` 一致 |
| F-06 预算快照可超卖 | **已修复**：创建在 `create_gate` 内串行化；零长度默认拒绝消除了「零写入顶满」路径 |
| F-07 孤儿/临时/损坏记录残留 | **已修复**：清扫新增 `data/` 孤儿、`metadata-tmp/`、损坏 ledger、无主 `.verified`、人工删文件的墓碑（含单测） |
| F-08 进程内锁跨实例失效 | **已修复**：传输根独占锁文件，第二个写者 `Locked` 失败（含测试） |
| F-09 断电一致性过度宣称 | **已修复**：lib/文档改为「无断电一致性保证」 |
| F-10 端点生命周期/配置在 HTTP crate | **待接线**：`SettingsFile` 段、`PortOwner::Transfer`、keyring 授权器、宿主预绑定 listener |
| F-11 文档矛盾/缺安全面 | **已修复**：`ARCHITECTURE_REVIEW` 状态句、`SECURITY.md` 新增 `/transfer` 节、状态码表补 500 与路径门禁、回执头相对路径；未实现的 `artifact_*`/`flash_*` 工具仍属阶段二 |
| F-12 多段保留名 | **已修复**：按首个点前判定；补 `CON.tar.img` 等用例 |
| F-13 回执泄露绝对路径 | **已修复**：`x-duthub-artifact` 改为相对传输根；测试改为相对根解析 |
| F-14 marker 路径未校验 id | **已修复**：`verified_marker`/`failed_marker` 返回 `Result` 并校验 id |
| F-15 示例 `--lan` 用默认令牌 | **已修复**：`--lan` 必须显式 `--token`，拒绝 `dev-token` |
| F-16 失败响应仍带 artifact 头 | **已修复**：仅校验通过/无摘要时写入，拒绝路径移除该头 |
| F-17 版本门在鉴权前放行 | **已修复**：缺失/不支持 `Tus-Resumable` 在鉴权前直接 412 |
| F-18 零长度上传无限累积 | **已修复**：默认 400，`allowZeroLengthUploads` 显式开启（原用例改为开启后仍验证 201） |
| F-19 全局哈希串行 | **已修复**：`verifyConcurrency`（默认 2）信号量限流 |

---


## 2. 现状与验收基准

### 2.1 五条验收基准（原文式复述）

> 以下五条来自本轮任务说明（用户 2026-11-24 拍板，第 5 条 2026-11-25 追加），不是仓库内文件；`grep -rn "目录限定\|通道唯一\|每 DUT 严格隔离\|删除 DUT 不要求联动\|解耦与可演进" --include=*.md .` 除本报告外无命中，属预期。

1. **目录限定**：所有文件必须落在指定的工作路径内（有效工作目录下的 duthub-data 等），不得写到工作路径之外；绝对路径配置也必须校验包含关系。
2. **通道唯一**：文件只能经指定通道进入（传输端点 + 已授权令牌），别处不得成为后门写入路径；认证失败必须 fail closed。
3. **每 DUT 严格隔离**：每 DUT 独立端点/凭据、按 DUT 落盘、跨 DUT 一律拒绝。
4. **删除 DUT 不要求联动清理**：文件由人工删除，因此缺少级联删除不算缺陷；但写入位置必须正确且可按 DUT 找到。
5. **解耦与可演进性（2026-11-25 追加）**：路径策略、鉴权与租户维度、存储布局、端点生命周期必须是彼此独立的模块/接缝；新增或修改 DUT 维度不得牵动无关模块；禁止「一处新增要同步改多处」的隐式耦合（AGENTS.md 坑 #13 同族）；改造设计必须写清每层的职责边界与依赖方向。

### 2.2 逐条判定（含范围）

| 基准 | 判定 | 关键依据 |
| --- | --- | --- |
| 1. 目录限定 | **transfer 面不满足；全局未审计** | `config.rs:207-210` 默认 `<exe_dir>/duthub-data/images`；`:236-239` 绝对路径在 `..` 拒绝之前原样返回；`:267-276` 只 `create_dir_all` + 探针；`lib.rs:177-183` 自取 `current_exe()`；全 crate 无 `canonicalize`/`reparse`/`symlink`/`plain`（grep 无输出）。发布路径还依赖上游 `DiskStore::sanitize_filename`（`disk.rs:333-345`），它只剥目录、拒绝 `.`/`..`，不校验扩展名/保留名。其它写入面（串口日志、截图/录制、`DutQuota`、`dut_artifacts`、Agent 目录）未逐一审计。 |
| 2. 通道唯一 / fail closed | **transfer 面不满足；全局未系统枚举** | 规范路径 `POST /transfer` 无令牌 401（本轮实测）；`dut_files`/`dut_file_read` 只读（`server.rs:988`、`:1038`，本轮阅读）；A2A 只绑回环、非回环 `NeedsProductSignoff`（`crates/duthub-a2a/src/listener.rs:41-47`、`:277-283`），`grep -rn "std::fs::\|tokio::fs::" crates/duthub-a2a/src` 无输出。但 F-01 实测 201/204/200/204，`authorize_resource` 读失败放行（`router.rs:331-333`），授权还依赖 `Tus-Resumable`（F-17）。`media_streams`（`server.rs:804`）的 snapshot/recordStart/recordStop 需要 `capture` 授权并由桌面写制品；`skill_export` 返回正文、桌面导出写 `<work_dir>/skill/...`（AGENTS #25）；`artifact_*`/`flash_*` 未实现。其它设备写入面未审计。 |
| 3. 每 DUT 严格隔离 | **不满足** | `UploadScope` 只有 `subject/boards/max_bytes/expires_at`（`authorize.rs:46-56`）；`AuthContext.board` 可选（`:60-70`）；`TransferState` 无 DUT（`router.rs:41-53`）；扁平 `data/<filename>`（`lib.rs:54-62`）；无每 DUT 端点/令牌。 |
| 4. 删除不要求级联，但位置正确、可按 DUT 找到 | **不满足（找得到这一半）** | 扁平布局；MCP `dut_files` 用 `dut_artifact_root()`（`mcp_runtime.rs:285-287`）调 `dut_artifacts::list_files`（`server.rs:1003-1008`），传输根在 `<exe>/duthub-data/images`，两棵树无包含关系。现有删除流程 `run_remove_dut` 会删 MCP 端点、A2A token、Agent 目录，并停止会话/持久化删除结果，但**不删 DUT 目录**（`agent_ui.rs:233-265`、`workspace.rs:1458-1460`）——这符合「无需级联」，但没有解决「按 DUT 找到传输文件」。 |
| 5. 解耦与可演进性 | **不满足** | `TransferConfig`/路径解析在 HTTP crate（`config.rs:1-8` 自述 stage one 不进 `SettingsFile`）；`serve` 自 bind（`lib.rs:246-257`）；`PortOwner` 只有 `A2a`/`External`（`ports.rs:14-21`）；keyring 只有 `model/`、`a2a/`（`keys.rs:13/47`）。 |

**既有豁免**：配置目录 `<exe>/.duthub` 按既有契约位于工作路径之外；`ensure_portable_work_dir` 接受任意绝对 `workspaceDir` 且不做包含校验（`store.rs:85-96`）。这两点与基准 1 的字面要求冲突：若产品豁免，基准 1 的全局结论必须注明例外；若不豁免，另需改 `ensure_portable_work_dir`/`resolve_work_dir_in` 并把 T24 收口。

### 2.3 当前实现是否具备 DUT 维度

**结论：不具备。** 逐层证据：

1. **授权作用域**：`UploadScope { subject, boards, max_bytes, expires_at_epoch }`（`authorize.rs:46-56`），没有 `dut_id`。
2. **请求上下文**：`AuthContext { op, board, declared_len, upload_subject }`（`authorize.rs:60-70`），`board: Option<&str>`，没有 DUT。
3. **服务状态**：`TransferState { authorizer, store, free_space, config, base, verify_gate }`（`router.rs:41-53`），没有 DUT。
4. **配置**：`TransferConfig` 是单实例配置（`transfer_dir`、`lan_access`、`base_path`、全局限额、TTL、GC 周期），没有 `listeners[]`、没有 `dutId`（`config.rs`）。
5. **落盘**：`<transfer_dir>/data/<filename>`、`metadata/<id>.json`、`metadata/<id>.verified`（`lib.rs:54-62`），没有 DUT 段。
6. **端点**：`TransferServer::serve(port)` 单实例自 bind（`lib.rs:246-257`），没有每 DUT 端口/监听器。
7. **产品配置/凭据/端口**：`SettingsFile` 无 `transfer` 段（`grep -n -i transfer crates/duthub-core/src/model.rs` 退出 1）；`PortOwner` 无 transfer（`ports.rs:14-21`）；keyring 无 transfer 条目（`keys.rs` 只有 `model/` 与 `a2a/`）。
8. **`board` 可省略**：创建时 `let board = metadata_text(&metadata, KEY_BOARD)?`，`None` 直接跳过校验（`router.rs:244-258`）；`authorize` 的两个 `if let Some(board)` 在 `None` 时都不成立（`authorize.rs:136-140`、`:155-159`）。**带 board 但不在 scope 时仍会 403**；问题是「省略即跳过」而不是「board 完全无效」。

因此「每 DUT 严格隔离」不是「隔离做得不够」，而是**这一维根本不存在**。`board` 是客户端提供的次级标签，不是可信租户身份；把 DUT 维度继续挂在 `board` 上会重复同类缺陷。

### 2.4 现状事实清单（本轮已核实）

- `crates/duthub-transfer` 只是 workspace 成员：`grep -rn -E "duthub[_-]transfer|TransferServer" --include=Cargo.toml --include=*.rs . | grep -v "^./crates/duthub-transfer/"` 只命中根 `Cargo.toml:13` 与 crate 自身；`apps/`、`crates/duthub-core`、`duthub-mcp`、`duthub-a2a` 无消费方。另 `grep -rn '"/transfer\|/transfer"' --include=*.rs apps/ crates/`（排除 transfer crate）无输出。
- `SettingsFile` 无 transfer 段（`grep -n -i transfer crates/duthub-core/src/model.rs` 退出 1）。
- `PortOwner` 仅 `A2a { dut_id }` 与 `External { label }`（`ports.rs:14-21`）。
- keyring service `duthub` 仅有 `model/<model_id>` 与 `a2a/<dut_id>`（`keys.rs:13/47`）。
- 默认值：单上传 30 GiB（`config.rs:24-27`）、保留空间 1 GiB（`:48-50`）、未完成 TTL 86400 秒（`:32-38`）、GC 300 秒（`:56-61`）、允许扩展名 `img/iso`（`:63-66`）。
- 布局：`<transfer_dir>/data/<filename>`、`data/<id>.bin`、`metadata/<id>.json`、`metadata/<id>.verified`（`lib.rs:54-62`）。
- 文档状态：`docs/ARCHITECTURE_REVIEW.md:31-36` 写「2026-11-24 已落地」，`:121` 同文写「镜像传输部分为设计方案，尚未实现或压测」；`docs/SECURITY.md` 全文 44 行 0 次提及 transfer。
- 契约：`docs/API_CONTRACT.md:123` 的 10 GiB 是**每台 DUT 的总容量**（「录屏、截图、镜像和报告都计入」），不是单次上传上限；`config.rs:24-27` 的 30 GiB 是单次 `Upload-Length` 上限。`docs/guides/TRANSFER_RECEIVER.md:107-108` 又写「已完成文件永不按 TTL 删除」——两者对「已完成镜像是否参与每 DUT 淘汰」冲突。
- 测试：ask 基线 92 个、退出码 0（未执行）；本轮静态计数 50+8+7+20+4+3=92（见 §1.3）。
- 示例接收端：`examples/receiver.rs:39` 的 `--token` 默认 `dev-token`，`:41` 的 `--lan` 打开 `lan_access`，`:52` 赋给 `lan_access`；`lib.rs:235-242` 的 bind 策略只要求「有 authorizer」，不要求令牌非默认。
- 删除流程：`run_remove_dut` 会删 MCP 端点、A2A token、Agent 目录，并停止会话/持久化删除结果；**不删 DUT 目录**（`agent_ui.rs:233-265`；`workspace.rs:1458-1460` 的 `delete_agent_dir`）。

### 2.5 审核范围与限制

**已做**：逐行阅读 `crates/duthub-transfer/src/{router,authorize,config,lib,store,names}.rs`、示例接收端、相关测试（`ztus_client`/`auth_failclosed`/`reject_paths`/`happy_path`/`resume_restart`）、`duthub-core/{model,dut_artifacts,dut_quota,store,keys}.rs`、`duthub-a2a/src/{ports,listener}.rs`、`duthub-mcp/src/{server,transport}.rs`、桌面 `main.rs`/`mcp_runtime.rs`/`agent_ui.rs`/`workspace.rs`、`duthub-sandbox/src/policy.rs`，以及上游 `salvo_core-1.2.1/src/{service,routing/path_state}.rs`、`salvo-tus-1.2.1/src/{stores/disk,lockers/memory_locker}.rs`；运行了消费方/文档/配置段/文件写入的 `grep`；用示例接收端做了 F-01/F-17/F-18 的端到端复现。

**未做**：未执行 `cargo test -p duthub-transfer`；未做 30 GiB 双机/带宽/断电压测；未构造并发 create 超卖、同目录多实例、孤儿清扫残留的运行时用例；未做 junction/绝对路径的运行时探针；未在多个 Windows 版本验证设备名；未抓取真实 460 响应；未双 token 实测「F-01 创建的无 subject 上传被另一 token 操作」；未系统枚举 MCP/设备的所有写入面；未审计串口日志、截图/录制、`DutQuota`、`dut_artifacts`、Agent 目录是否满足基准 1/3。

---

## 3. 发现清单

严重度定义：**high** = 直接破坏验收基准或可导致未授权写入/读取；**medium** = 接线后会造成隔离/资源/契约缺陷，或安全语义与声明不符；**low** = 边界/文档/示例问题，当前不可直接利用。`verified` 表示本轮有代码/源码/运行时证据；`unconfirmed` 表示只有代码推演或依赖他人输出，未本轮实测。

### 3.1 High

#### F-01 非规范路径使鉴权整体跳过，且跳过名称/预算/subject 绑定

- **位置**：`crates/duthub-transfer/src/router.rs:111-115`（`TransferGuard::handle`）；`router.rs:190-207`（`fn target`）；对照 `salvo_core-1.2.1/src/routing/path_state.rs:324-367`（`parse_path_parts`）、`salvo_core-1.2.1/src/service.rs:483-497`（`fix-http1-request-uri`）。
- **问题**：守卫用原始 `req.uri().path()` 计算 `Target`；`//transfer`、`/./transfer`、`/foo/../transfer` 不匹配 `strip_prefix("/transfer")` → `Target::Outside` → 直接 `call_next`。Salvo 路由的 `parse_path_parts` 跳过空段、丢弃 `.`、对 `..` 弹栈，随后仍把请求交给 tus handler。于是 create/patch/head/get/delete 全部在无鉴权下执行；`authorize_create` 也因此不跑，文件名白名单、board、`free_space` 预算、`KEY_SUBJECT` 注入全部跳过。
- **证据**：
  - 代码：`let target = target(req.uri().path(), &state.base); if method == Method::OPTIONS || target == Target::Outside { ctrl.call_next(req, depot, res).await; return; }`（`router.rs:111-115`）；`target()` 的 `strip_prefix(base)` 失败即 `Outside`（`:190-207`）。
  - 机制：`parse_path_parts` 的 `.`/`..`/空段处理在 `path_state.rs:324-367`；`fix-http1-request-uri` 只补 scheme/authority（`service.rs:483-497`），不碰 path。第一版把根因归给该特性是错的。
  - 本轮实测（示例接收端，`%TEMP%` 数据目录，测后删除）：规范 `POST /transfer` 无令牌 → 401；`POST //transfer` → 201；`POST ///transfer` → 201；`POST /./transfer` → 201；`POST /foo/../transfer` → 201；`PATCH //transfer/<id>` 无令牌 → 204；错误令牌 `HEAD` → 200（`upload-offset: 5`）；错误令牌 `GET` → 200 且响应体 `hello`；无令牌 `DELETE //transfer/<id>`（在途）→ 204。
  - 名称/预算：规范路径 `filename=evil.exe` 带令牌 → 400；`POST //transfer` 同 metadata 无令牌 → 201；`POST //transfer` + `CON.tar.img` → 201（`CON.tar.img` 同时也命中 F-12 的多段保留名缺陷，不能单独证明白名单被绕过）。
  - subject：F-01 创建的 `metadata/<id>.json` 只有 `filename`，没有 `subject`；规范路径创建的零长度上传 sidecar 有 `subject`。
- **复核状态**：**verified**（代码 + Salvo 源码 + 本轮 curl；双 token 的跨主体操作未实测）。
- **影响**：一旦接线到 `0.0.0.0`，同网段任何进程可无令牌创建 30 GiB 级、任意扩展名/保留名的上传，读取/删除在途上传，且不占预算；F-01 创建的上传没有 subject，任何已认证 token 都能对其执行 PATCH/HEAD/DELETE（`authorize.rs:149-159` 在 `upload_subject=None` 时跳过主体检查）。即使回环，也能绕过令牌边界。
- **修复方向**：最外层「规范路径门禁」——拒绝 `//`、`.`/`..` 段、反斜杠、百分号编码、非法尾斜杠，返回 400；`target()` 只接受与路由层相同的规范形态；鉴权/名称/预算必须在同一 guard 内、且在 handler 之前执行。补充 `//transfer`、`///transfer`、`/./transfer`、`/foo/../transfer`、`/%2Ftransfer` 回归。该修复不依赖 DUT 维度，应作为独立 P0 先落。
- **修复状态（2026-11-25）**：**已修复**。`router.rs` 在鉴权之前新增规范形态门禁（`is_canonical_path` / `non_canonical_path`）：只接受以单个 `/` 开头、段非空且非 `.`/`..`、无 `\`、无 `%`、至多一个尾斜杠；挂载点之下报 `invalid_upload_id`，其余报 `invalid_path`（均 400）。真实接收端复核：`//transfer`、`///transfer`、`/./transfer`、`/foo/../transfer` 无令牌/带令牌均 400；`//transfer/<id>` 的 PATCH/HEAD/DELETE 均 400；规范路径无令牌 401、带令牌 201；`/%2Ftransfer` 由路由层 404 且不产生上传。回归：`tests/canonical_paths.rs`（原始 TCP，2 例）＋ `router::tests::canonical_gate_rejects_what_the_router_would_normalize`；crate 测试 95 条全绿。其余发现（F-02 起）未改动。

#### F-02 整条传输链路没有 DUT 维度，唯一租户轴 `board` 可省略

- **位置**：`crates/duthub-transfer/src/authorize.rs:46-56`、`:60-70`、`:128-163`；`router.rs:244-258`、`:311-353`；`lib.rs:54-62`；对照 `crates/duthub-mcp/src/server.rs:988-1010`、`apps/desktop/src/mcp_runtime.rs:285-287`。
- **问题**：`UploadScope` 没有 `dut_id`，`AuthContext` 没有 DUT，`TransferState`/`TransferConfig`/落盘布局也没有 DUT；`board` 是可选客户端元数据，**省略**即整段作用域校验跳过（带 board 但不在 scope 仍 403）。落盘扁平 `data/<filename>`，MCP `dut_files` 看不到这些文件，且没有每 DUT 端点/凭据。基准 3 与 4 均不满足。
- **证据**：
  - `authorize.rs:46-56` 只有 `subject/boards/max_bytes/expires_at_epoch`；`:136-140`、`:155-159` 的 `if let Some(board)` 在 `None` 时不成立。
  - `router.rs:244-247` 缺 board 不 400；`:250-258` 把 `board.as_deref()` 交给 `authorize`。
  - 测试 `tests/ztus_client.rs:33-39` 的 scope 只有 `boards: ["board1"]`，`:63` 创建元数据只有 `filename`，`:98` 断言 `CREATED`——「无 board 创建并发布」被钉成预期。
  - `lib.rs:54-62` 布局无 DUT 段；`server.rs:988-1010` 的 `dut_files` 走 `dut_artifacts::list_files(root, dut_id, name)`，`root` 是 `artifact_root`；默认传输根是 `<exe>/duthub-data/images`，两者无包含关系。
  - F-01 创建的 sidecar 无 `subject`（本轮 metadata dump）；后续 `authorize` 在 `upload_subject=None` 时跳过主体检查（`authorize.rs:149-159`）。
- **复核状态**：**verified（代码/测试/元数据）；跨 DUT 双 token 运行时未做**。
- **影响**：跨 DUT 无法强制拒绝；不同 DUT 的文件混在同一 `data/`；MCP 端按 DUT 列不到传输文件；未带 board 的创建不受 board 约束；F-01 创建的上传无 subject，任何已认证 token 可对其执行 PATCH/HEAD/DELETE。
- **修复方向**：每 DUT 一个 `TransferServer`（DUT 由服务端构造时绑定），不修改 `UploadScope`（见 §5.2 的兼容设计），用 per-DUT `DutTokenAuthorizer` + `TransferServer.dut_id` 做结构隔离；`board` 保留为 DUT 内的次级校验，不再承担租户边界；存储根改为每 DUT 目录。该设计可保留 `ztus_client` 的「不报 board 也能创建」，不撞既有测试。

#### F-03 传输根锚在 `current_exe()`，绝对 `transferDir` 无包含校验，目录不拒绝 reparse

- **位置**：`crates/duthub-transfer/src/config.rs:207-210`、`:236-239`、`:250-256`、`:267-276`、`:409-416`；`lib.rs:177-183`；对照 `apps/desktop/src/main.rs:127-138`、`crates/duthub-core/src/store.rs:85-96`、`:301-304`、`crates/duthub-core/src/dut_artifacts.rs:11-22`；上游 `salvo-tus-1.2.1/src/stores/disk.rs:333-345`。
- **问题**：默认根 `<exe_dir>/duthub-data/images`；`resolve_transfer_dir` 在 `ParentDir` 拒绝之前对 `path.is_absolute()` 直接返回，因此 `C:/a/../../x` 这类绝对路径整体绕过 `..` 拒绝；`ensure_transfer_dir_writable` 只 `create_dir_all` + `NamedTempFile` 探针，不 canonicalize、不校验与工作根的包含关系、不拒绝 junction/symlink/reparse point。`lib.rs` 自己取 `current_exe()`，构造签名没有 `work_dir` 参数；桌面 `main.rs:127-138` 先 `ensure_portable_work_dir` 再 `artifact_root.join("duthub-data")`，设了 `--workdir`/`workspaceDir` 时两棵根分离。
- **证据**：
  - `config.rs:236-239` `if path.is_absolute() { return Ok(path.to_path_buf()); }` 位于 `:250-256` 的 `ParentDir` 检查之前；`:409-416` 单测 `transfer_dir_absolute_passes_through` 把 `C:/elsewhere/images` 原样通过钉成断言。
  - `config.rs:267-276` 只有 `create_dir_all` + `NamedTempFile::new_in`；全 crate `grep -rn "canonicalize\|reparse\|symlink\|plain(" crates/duthub-transfer/src/` 无输出。
  - `lib.rs:177-183` 用 `std::env::current_exe()` 的父目录作锚；构造器 `new`/`with_free_space` 无 `work_dir` 参数（`:162-183`）。
  - `main.rs:127-138` 的 `artifact_root` 来自 `ensure_portable_work_dir`，`work_dir = artifact_root.join("duthub-data")`；`mcp_runtime.rs:285-287` 的 `dut_artifact_root` 是 `work_dir.parent()`。
  - 上游 `sanitize_filename`（`disk.rs:333-345`）用 `Path::file_name()` 剥目录、拒绝 `.`/`..`，因此 `../../x.img` 不会逃出 `data/`；但它不校验扩展名/保留名。F-01 绕过 guard 时这是唯一的发布名防线。
  - 另：`store.rs:85-96` 的 `ensure_portable_work_dir` 取 `[cli_dir, settings_dir]`（CLI 优先），`store.rs:301-304` 的 `resolve_work_dir_in` 取 settings 优先；`docs/decisions/ADR-WORKDIR.md:3/41` 已接受两套解析并存（T24 deferred）。
- **复核状态**：**verified（代码/单测/上游源码）；junction/绝对路径的运行时探针 unconfirmed**。
- **影响**：绝对 `transferDir` 可把 30 GiB 上传写到工作路径之外；junction/symlink 可让「看起来在根内」的路径实际指向根外（代码推演）；绝对路径中的 `..` 绕过现有防护。
- **修复方向**：传输根由宿主传入唯一生效工作根；`resolve_transfer_dir(work_root, configured)` 对绝对路径做 canonicalize + 包含关系校验；创建后逐段拒绝 reparse point；所有写入通过已校验的 `cap-std::Dir` 句柄。发布名仍必须过 `names::validate_publish_name`，不能只靠上游 `sanitize_filename`。

### 3.2 Medium

#### F-04 `authorize_resource` 在读记录失败或解码失败时静默放行

- **位置**：`crates/duthub-transfer/src/router.rs:311-353`（`fn authorize_resource`），关键 `:331-333`、`:334-343`。
- **问题**：`let Ok(upload) = state.store.get_upload_file_info(id).await else { return Ok(()); };` 把**任何** I/O 错误都当成「未知 id 让 tus 答 404」；`.map(|metadata| (get_text(KEY_SUBJECT).ok().flatten(), get_text(KEY_BOARD).ok().flatten())).unwrap_or_default()` 把解码错误吞成 `None`；随后 `authorize` 的两个 `if let Some(...)` 都不成立即 `Ok(())`。这与「认证失败必须 fail closed」相反。
- **证据**：代码路径如上；守卫读记录不持 tus 的每上传锁，上游 handler 在锁内重读可能成功，因此「读失败 → 放行 → 操作执行」结构上可达。现有 `tests/reject_paths.rs:517-538` 的 `cross_subject_writes_are_403` 只覆盖读成功的正常路径。上一版提到的「GET 会走 `else` 分支」是错的：方法矩阵在 `router.rs:127-154`，非 PATCH/HEAD/DELETE 已在 guard 返回 405，`authorize_resource` 的 `else`（`:326-327`）当前不可达。
- **复核状态**：**unconfirmed**（代码路径已读，错误注入/运行时可达性未构造）。
- **修复方向**：区分 `NotFound`（放行给 tus 404）与其他错误（500/503）；subject/board 解码失败一律拒绝（403 或 500）；记录读取失败时不要用「未知 id」语义。

#### F-05 容量只有一份全局账，无每 DUT 配额；transfer 文件不计入每 DUT 容量

- **位置**：`crates/duthub-transfer/src/router.rs:269-287`；`crates/duthub-transfer/src/store.rs:43-55`、`:121-132`；`crates/duthub-transfer/src/config.rs:22-26`、`:48-50`；`docs/API_CONTRACT.md:123`；对照 `crates/duthub-core/src/dut_quota.rs:12`、`:43-71`。
- **问题**：预算只比较「整根空闲 + 全库在途 + 全局 reserved」，没有每 DUT 配额、没有已发布制品保留期/淘汰。`docs/API_CONTRACT.md:123` 的 10 GiB 是**每台 DUT 总容量**（录屏、截图、镜像、报告合计），不是单次上传上限；`config.rs:24-27` 的 30 GiB 是单次 `Upload-Length` 上限。真正的问题是 transfer 文件没有计入每 DUT 容量，且单次 30 GiB 可能超过 10 GiB 的每 DUT 总容量。
- **证据**：`router.rs:269-287` 的 `budget_required(declared, reserved_disk_bytes, BUDGET_SLACK_BYTES, inflight)` 对比 `free_space.available_bytes(state.store.root())`，无 `dut` 参数；`store.rs:121-132` 对全库未完成上传求和；`config.rs:24-27` 默认 `32_212_254_720`（30 GiB），`:48-50` 默认 `1_073_741_824`（1 GiB）；`docs/API_CONTRACT.md:123` 原文；`docs/guides/TRANSFER_RECEIVER.md:107-108` 又明确已完成文件永不按 TTL 删除。`DutQuota` 需要已打开的 `owner: &Dir`（`dut_quota.rs:43-71`），而 transfer 的 `Cargo.toml` 当前既无 `duthub-core` 也无 `cap-std`；即使补依赖，上游 `DiskStore` 也不暴露该目录句柄，仍须宿主注入。
- **复核状态**：**verified**（代码/文档）。
- **修复方向**：宿主注入每 DUT 的 `TransferQuota`（§4.6），create 时原子预留、完成时转已用、失败/过期/删除时释放；`maxUploadBytes` 默认不得大于每 DUT 配额，或由配额在 create 时拒绝；契约明确「已完成镜像是否参与每 DUT 淘汰」——`API_CONTRACT.md` 与 `TRANSFER_RECEIVER.md` 目前冲突，必须二选一。

#### F-06 预算是一次无预留快照，并发 create 可超卖；零写入声明可把端点顶成 507 到 TTL

- **位置**：`crates/duthub-transfer/src/router.rs:269-287`；`crates/duthub-transfer/src/store.rs:121-132`；`crates/duthub-transfer/src/config.rs:32-38`；`crates/duthub-transfer/src/lib.rs:227-229`。
- **问题**：`authorize_create` 先读 `inflight`、再读 `available`，中间没有占位；上传记录在 guard 放行之后才由 tus 落盘（`Router::new().hoop(TransferGuard(state)).push(tus.into_router())`，`lib.rs:227-229`）。并发 create 各自看到 `inflight=0`，可同时通过。零写入声明按完整长度计入 `inflight`，可把后续 create 顶成 507 直到 TTL（默认 86400 秒）。
- **证据**：代码顺序如上；按默认值算，单次 `required = 30 GiB + 1 GiB + 64 KiB`，40 GiB 可用时两个并发 30 GiB POST 可同时通过（算术推演）；`authorize` 无按 subject 的数量/总量上限；现有测试只有单发 507（`tests/reject_paths.rs:298` 用 `FixedFreeSpace(0)`），无并发用例。
- **复核状态**：**unconfirmed**（代码/算术推演；并发未实测）。
- **修复方向**：预算改为原子预留（`TransferQuota::reserve`），create 时先占位、完成/失败释放；把「在途」与「已预留」分开记账；对零字节/长时间无写入的声明设置更短的回收或按实际 offset 调整。

#### F-07 清扫只枚举 `metadata/*.json`/`*.failed`，`data/` 孤儿、`metadata-tmp`、损坏记录永久残留

- **位置**：`crates/duthub-transfer/src/store.rs:93-107`（`ids`）、`:191-210`（`sweep_ids`）、`:214-246`（`sweep`）；对照上游 `salvo-tus-1.2.1/src/stores/disk.rs:487-493`。
- **问题**：`ids()` 只按 `metadata` 下的 `.json`；`sweep_ids()` 只补 `.failed`；`sweep` 对读不出的记录 `Err(_) => false`（视为未过期）、对 `remove` 失败 `Err(_) => {}`（保留）。全 crate 没有函数枚举 `data/` 或 `metadata-tmp/`。上游 finalize 在 `remove_file(current_path)` 失败时只 `warn!`，留下 `<id>.bin` 从此无记录。
- **证据**：代码如上；`docs/guides/TRANSFER_RECEIVER.md:109` 声称「清扫与写者共用同一把每上传锁」，但没有覆盖孤儿枚举。
- **复核状态**：**unconfirmed**（代码；残留场景未实机验证）。
- **修复方向**：清扫增加 `data/*.bin` 无记录、`metadata-tmp/*` 超龄、损坏 `.json`、已完成记录但 `storage.path` 不存在（人工删除后的墓碑）四类；删除仍持每上传锁；把手动删除产生的「记录在、文件不在」收敛为删除记录而非报错。

#### F-08 `MemoryLocker` 是进程内且每实例一份，同目录多实例/多进程锁互不可见

- **位置**：`crates/duthub-transfer/src/lib.rs:185-191`；`crates/duthub-transfer/src/store.rs:227-230`；上游 `salvo-tus-1.2.1/src/lockers/memory_locker.rs:12-36`。
- **问题**：`let locker = MemoryLocker::new();` 每个 `TransferServer` 各一份，构造器没有 locker 注入接缝；上游是进程内 `Arc<Mutex<HashMap>>`。两个实例或两个进程指向同一 `transfer_dir` 时，清扫可能删掉另一实例正在写的上传；另一实例按已提交 offset 写前 `file.set_len(offset)` 会截断其尾部。
- **证据**：`lib.rs:185`、`:190`/`:211` 分别交给 store 与 Tus；`docs/guides/TRANSFER_RECEIVER.md:109` 的「共用同一把每上传锁」只在单实例内成立，文档未要求同目录单实例独占。
- **复核状态**：**unconfirmed**（代码/上游源码；多实例行为未实测）。
- **修复方向**：构造函数注入 `Arc<dyn Locker>` 或按 canonical root 的 `LockerRegistry`；同目录加独占锁文件（`fs4`/`LockFileEx`），第二个实例/进程启动失败；清扫与写者共用同一 registry 实例。

#### F-09 上游不做 fsync，但文档声称「声明摘要时断电一致性由整文件复读兜住」

- **位置**：`crates/duthub-transfer/src/lib.rs:64-66`；`docs/guides/TRANSFER_RECEIVER.md:81-82`；`crates/duthub-transfer/src/router.rs:359-431`；上游 `salvo-tus-1.2.1/src/stores/disk.rs:527-546`、`:704`、`:421`。
- **问题**：正常 PATCH 只 `file.flush()`（`disk.rs:704`），不 `sync_all`；`write_meta_atomic` 是 `fs::write` + `rename`，也不 fsync（`:527-546`）；`sync_all` 只在 hard-link 失败后的 copy 兜底（`:421`）。`complete()` 的整文件复读发生在崩溃前且经页缓存，校验结果只写一次 `metadata/<id>.verified`；重启后 HEAD 一律 200 进不了校验分支，最终 offset 的零字节 PATCH 只回放旧摘要（`tests/resume_restart.rs:196-245`），重放旧 offset 则 409。因此「断电一致性由摘要兜住」不成立。
- **证据**：上游源码逐行阅读；`lib.rs:64-66` 与 `TRANSFER_RECEIVER.md:81-82` 的同一承诺。
- **复核状态**：**unconfirmed**（代码/上游源码；断电行为未实测）。
- **修复方向**：若保留承诺，在完成响应前对发布文件 `sync_all`（宿主 `CompletedArtifactSink`），并对 metadata 目录做 `fsync`；否则把文档改成「无断电一致性保证，摘要只保证传输期完整性」。

#### F-10 端点生命周期与配置模式锁在 HTTP crate，无法作为独立层演进

- **位置**：`crates/duthub-transfer/src/lib.rs:235-242`、`:246-257`；`crates/duthub-transfer/src/config.rs:1-8`；`crates/duthub-a2a/src/ports.rs:14-21`；`apps/desktop/src/mcp_runtime.rs:738`；`crates/duthub-mcp/src/transport.rs:233-240`。
- **问题**：`serve(port)` 自行 `TcpListener::new(addr).try_bind()`，宿主无法预绑 listener；`TransferConfig` 住在 transfer crate，导致 core 反向依赖不可行；`PortRegistry` 没有 transfer 归属；bind 策略是与 MCP 同判定的第二份拷贝。接入产品需同时改 core model、settings 示例/roundtrip、端口探测枚举、keyring 授权器与 UI。
- **证据**：`lib.rs:246-257` 自 bind；对照 MCP `transport.rs:233-240` 接受宿主绑好的 listener，桌面 `mcp_runtime.rs:738` 先 `TcpListener::bind` 再交付。`lib.rs:235-242` 的 `addr.ip().is_loopback() || self.authorizer.is_some()` 与 `transport.rs:423-431` 同形但独立实现。`config.rs:1-8` 自述 stage one 不进 `SettingsFile`；`grep -n -i transfer crates/duthub-core/src/model.rs` 退出 1；`ports.rs:14-21` 只有 A2a/External；`grep -rn "Entry::new" crates/duthub-core/src/keys.rs` 只有 model/ 与 a2a/。
- **复核状态**：**verified**（代码/grep）。
- **修复方向**：配置类型下沉 core；端点模型（`TransferListener { id, dut_id, port, enabled, lan_access }`）与 `PortOwner::Transfer` 进 core/a2a 注册表；宿主先 bind、注册端口、检查冲突，再 `serve_on(listener)`；bind 策略抽成共享函数或宿主统一判定（§4.3、§4.5）。

#### F-11 文档与契约自相矛盾、承诺未实现、安全面缺失

- **位置**：`docs/ARCHITECTURE_REVIEW.md:31-36` 与 `:121`；`docs/SECURITY.md`（全文 44 行）；`docs/API_CONTRACT.md` 工具表与 `:123`；`docs/guides/TRANSFER_RECEIVER.md:100-103`。
- **问题**：
  1. 同一评审既写「2026-11-24 已落地」，又写「镜像传输部分为设计方案，尚未实现或压测」。
  2. `SECURITY.md` 全篇不提这个局域网明文、30 GiB 写入面。
  3. `ARCHITECTURE_REVIEW.md:68-72` 承诺的 `artifact_prepare`/`artifact_status`/`artifact_delete`/`flash_start`/`flash_status` 在工具表与 `API_CONTRACT.md` 都不存在（`grep -n -E "artifact_prepare|artifact_status|artifact_delete|flash_start|flash_status" docs/API_CONTRACT.md crates/duthub-mcp/src/server.rs` 退出 1）。
  4. `API_CONTRACT.md:123` 的「每 DUT 10 GiB…删除最旧未占用文件」与 `TRANSFER_RECEIVER.md:107-108` 的「已完成文件永不按 TTL 删除」冲突。
  5. 状态码表缺 `500 storage_probe_failed`/`verification_failed`（`router.rs:546/570`），也缺 `x-duthub-artifact` 回执头；**排除本报告后** `grep -rn -i "x-duthub" docs/ --exclude=TRANSFER_SECURITY_REVIEW.md` 只命中 `X-Duthub-Checksum`（`TRANSFER_RECEIVER.md:60`）。报告自身包含该字符串，第一版的裸 grep 现在不可复现，属自指。
- **复核状态**：**verified**（grep/文档阅读）。
- **修复方向**：见 §6；先统一「已实现 / 未接线 / 未压测」三态，再补安全面与契约。

#### F-17 授权被放在 `Tus-Resumable` 版本门之后，是条件式安全门

- **位置**：`crates/duthub-transfer/src/router.rs:220-222`（`authorize_create`）、`:319-321`（`authorize_resource`）。
- **问题**：两处都是 `if !has_supported_version(headers) { return Ok(()); }`——当 `Tus-Resumable` 缺失或不精确等于 `1.0.0` 时，鉴权、名称、board、预算、subject 检查全部跳过。上游 handler 随后会因版本不支持返回 412，所以当前没有形成实际绕过；但这是与 F-01 同族的「守卫与协议层解析不一致」风险：一旦上游对缺失/变体版本放宽（或新增一个不检查版本的方法），就是未授权面。
- **证据**：代码如上；本轮实测 `Tus-Resumable: 1.0.0, 0.2.2` 带令牌 `POST /transfer` → **412**（未形成 201）；规范 `Tus-Resumable: 1.0.0` 才进入 401/201 逻辑。
- **复核状态**：**verified**（代码 + 本轮 curl）。
- **修复方向**：鉴权先于版本判断；或版本判断失败也走 fail-closed 的统一拒绝（401/412 由同一处决定），不要把 `Ok(())` 当作「交给下游」。补缺失/变体/逗号列表的用例。

#### F-18 零长度上传立即发布且永不过期，没有每主体/总数上限

- **位置**：`crates/duthub-transfer/src/router.rs:214-310`（create）；`crates/duthub-transfer/src/store.rs:167-187`（`is_expired`）、`:121-132`（`inflight`）。
- **问题**：`Upload-Length: 0` 创建后 `size==offset==0`，`is_unfinished` 为 false，`is_expired` 永远 false；`inflight_remaining_bytes` 记 0，预算不占；上游在 create/零字节 PATCH 时直接 finalize 到 `data/<filename>`。结果是可无限创建「已完成、永不清理、不占预算」的零字节制品文件，没有每主体/总数上限。
- **证据**：本轮实测 `POST /transfer` + `Upload-Length: 0` 带令牌 → **201**，`data/zero.img` 立即出现（`find` 输出 `zero.img 0`）；sidecar `size:0, offset:0, storage.path:.../data/zero.img`；`is_expired` / `is_unfinished` 代码如上。
- **复核状态**：**verified**（代码 + 本轮 curl）。
- **修复方向**：零长度上传要么拒绝（400），要么计入每 DUT 配额与总数上限；清扫/墓碑逻辑必须把「已完成但 0 字节」当作可管理制品，不能无限累积。

### 3.3 Low

#### F-12 多段保留名 `CON.tar.img`/`NUL.tar.gz` 通过校验

- **位置**：`crates/duthub-transfer/src/names.rs:53-61`（`is_reserved_device_name`）；`:3-8` 自述。
- **问题**：只对「最后一个点之前」的整段做保留名比对；`CON.tar.img` 的 stem 是 `CON.tar`，不等于 `CON`，于是通过；默认扩展名白名单 `img/iso` 使其 ext 合法，`validate_publish_name` 返回 Ok。这与模块自述「every casing and stem form」矛盾，也与 core 的保留名判定（首个点前）分叉。F-01 绕过 guard 时连这个校验都不执行，实测 `CON.tar.img` 无令牌 201。
- **证据**：`names.rs:54-57` 的 `rsplit_once('.')`；`config.rs` 默认白名单；现有用例 `names.rs:139-151` 与 `tests/reject_paths.rs:541` 都只有单段形态（`CON.img`、`NUL.img` 等）。Windows 设备名的实际后果（是否被 OS 当设备、发布失败还是普通文件）本轮**未复现**；第一版引用的「cmd/.NET 写普通文件」缺少可复现命令，已删除。
- **复核状态**：**规则缺陷 verified；Windows 设备名后果 unconfirmed**。
- **修复方向**：保留名判定改为「任意一段（按 `.` 与空格切分）等于设备名即拒绝」，或与 core 统一为同一函数；补多段用例。

#### F-13 完成回执把服务端绝对路径塞进 `x-duthub-artifact`，`media_streams` 也回传绝对 path/firstPath

- **位置**：`crates/duthub-transfer/src/router.rs:36`、`:373-378`；`crates/duthub-mcp/src/server.rs:388-391`。
- **问题**：完成回执头值来自 `upload.storage.path`（上游 `DiskStore` 的绝对路径），泄露盘符/用户名/安装位置；`media_streams` 也回传绝对 `path`/`firstPath`。两者都未进契约文档，却已被测试当契约（`tests/happy_path.rs:96-105` 对头做 `std::fs::canonicalize` 后比对；`tests/reject_paths.rs:641` 同）。
- **证据**：代码与测试如上；排除本报告后 `grep -rn -i "x-duthub" docs/ --exclude=TRANSFER_SECURITY_REVIEW.md` 只命中 `X-Duthub-Checksum`（`TRANSFER_RECEIVER.md:60`）。
- **复核状态**：**verified（代码/测试）；线上响应形态 unconfirmed**。
- **修复方向**：回执改为相对 DUT 根的 artifact id/相对路径；若确需绝对路径，限定为已授权本机调用方并写入契约；文档补齐。

#### F-14 `marker` 路径用未校验 id 直接拼接，`store()`/pub 方法暴露接缝

- **位置**：`crates/duthub-transfer/src/store.rs:135-147`、`:153-158`；`crates/duthub-transfer/src/lib.rs:302-305`。
- **问题**：`marker(id, kind)` 直接 `self.root.join("metadata").join(format!("{id}.{kind}"))`，无 `..`/`is_safe_upload_id` 检查；`verified_marker`/`failed_marker`/`flag_failed` 为 `pub`，`store()` 公开给宿主。当前 HTTP 路径喂入的是 `is_safe_upload_id` 校验过的 id（`router.rs:202`），不可利用。
- **证据**：代码如上；`router.rs:202` 只接受 `!rest.contains('/') && is_safe_upload_id(rest)`。
- **复核状态**：**verified（代码）；当前不可利用**。
- **修复方向**：`marker` 内部复用 `is_safe_upload_id`，不合法返回 `InvalidInput`；或把 marker 相关方法收窄到 crate 内。

#### F-15 示例接收端默认 `dev-token`，`--lan` 时用公开默认值绑 `0.0.0.0`

- **位置**：`crates/duthub-transfer/examples/receiver.rs:39-52`；`crates/duthub-transfer/src/lib.rs:235-242`。
- **问题**：`arg("--token").unwrap_or_else(|| "dev-token".to_owned())`（`:39`）；`--lan` 打开 `lan_access`（`:41`/`:52`）；`assert_bind_allowed` 只判 `is_loopback() || authorizer.is_some()`，不检查令牌是否默认/空。示例不是分发二进制，风险低。
- **证据**：代码如上；产品侧 keyring 无 transfer 条目，也无 `TransferServer` 构造点。
- **复核状态**：**verified**。
- **修复方向**：`--lan` 时要求显式 `--token` 且拒绝 `dev-token`；或示例只允许 loopback。

#### F-16 校验失败时 `x-duthub-artifact` 已被写入，460 响应仍可能带路径

- **位置**：`crates/duthub-transfer/src/router.rs:359-431`（`complete`），关键 `:373-378` 与 `:407-451`；`render_rejection` 在 `:496`（第一版写 `:483` 是错的）。
- **问题**：`complete()` 在读取 `storage.path` 后立即 `res.headers_mut().insert(HEADER_ARTIFACT, value)`，之后才做摘要校验；校验失败走 `discard` + `render_rejection`，后者只改 status/body（并移除 `upload-offset`，`:573-580`），不删除该头。因此 460 响应可能携带一个已被删除或即将被清扫的绝对路径。
- **证据**：代码顺序如上；`render_rejection` 不处理 `HEADER_ARTIFACT`。
- **复核状态**：**unconfirmed**（代码路径已读；真实 460 响应未抓取）。
- **修复方向**：只在「校验通过/无摘要」且响应为 201/204 时插入 artifact 头；校验失败路径先移除该头。

#### F-19 全局 `verify_gate` 串行所有整文件哈希，存在可用性风险

- **位置**：`crates/duthub-transfer/src/router.rs:415`（`let _gate = state.verify_gate.lock().await;`）。
- **问题**：所有完成上传的整文件校验共用一把全局 mutex，30 GiB 级哈希会串行阻塞其它完成请求；与 F-01 的无认证创建叠加时，可被用来拖住完成路径。
- **证据**：代码如上；`TransferState` 字段在 `router.rs:49`，初始化在 `lib.rs:225`。未做并发/大文件实测。
- **复核状态**：**unconfirmed**（代码；运行时未实测）。
- **修复方向**：按 DUT 或按上传 id 限流，至少不要全局串行；设置并发上限与超时；与配额/每 DUT 实例一起设计。

### 3.4 复核方法与未复核项

**本轮实际运行**：

- 静态测试计数（见 §1.3）：`grep -cE '^\s*#\[(tokio::)?test\]'` 分文件统计，合计 92；**未执行** `cargo test`。
- 示例接收端端到端（`target/debug/examples/receiver.exe`，端口 18082，`%TEMP%` 数据目录，测后 `taskkill` + 删除）：
  - 规范 `POST /transfer` 无令牌 → 401；`//transfer`/`///transfer`/`/./transfer`/`/foo/../transfer` → 201；
  - 规范 `evil.exe` 带令牌 → 400；`//transfer` + `evil.exe` → 201；`//transfer` + `CON.tar.img` → 201；
  - `PATCH //transfer/<id>` 无令牌 → 204；错误令牌 `HEAD` → 200；错误令牌 `GET` → 200 + `hello`；无令牌 `DELETE`（在途）→ 204；
  - `Upload-Length: 0` 带令牌 → 201，`data/zero.img` 出现；
  - `Tus-Resumable: 1.0.0, 0.2.2` 带令牌 → 412；
  - F-01 创建的 sidecar 无 `subject`；零长度规范上传的 sidecar 有 `subject`。
- `grep`：消费方、`SettingsFile`、`PortOwner`、keyring、文档、A2A 文件写入、`x-duthub`（排除本报告）、`canonicalize/reparse/symlink/plain`。
- 阅读上游 `salvo_core-1.2.1/src/service.rs`、`routing/path_state.rs`、`salvo-tus-1.2.1/src/stores/disk.rs`、`lockers/memory_locker.rs`。

**未运行**：

- `cargo test -p duthub-transfer`（92 个测试的通过状态仍是 ask 基线，不是本轮结果）。
- `cargo fmt`/`cargo clippy` 与全 workspace 门禁。
- 30 GiB 双机/带宽/断电、并发 create、同目录多实例、孤儿清扫、junction/绝对路径探针、Windows 设备名、真实 460 响应头、双 token 跨主体操作。
- MCP/设备的其它写入面（`media_streams` 的 capture、`skill_export`、ADB/HDC/烧录、串口日志、截图/录制、`DutQuota`、`dut_artifacts`、Agent 目录）未逐一审计；不能据此断言全局基准 1/2/3 满足。

---

## 4. 每 DUT 严格隔离改造设计

> 本节给出目标形态与接缝；不要求一次改完，分阶段见 §4.8。设计前提是「每 DUT 独立端点/凭据」与「文件由人工删除，不做级联清理」。

### 4.1 目标形态与不变量

1. **一个 DUT 一个接收实例**：每个启用的 DUT 一个 `TransferServer` + 一个监听器 + 一个令牌 + 一个存储根；不存在「全局接收端」。
2. **DUT 由服务端绑定，不由客户端声明**：请求路径不带 DUT id，客户端元数据不能改变租户；`board` 最多是 DUT 内的次级标签。
3. **结构隔离优先于检查隔离**：A 的 token 在 B 的 authorizer 里查不到 → 401；A 的存储根不在 B 的路径下。
4. **所有落盘都在唯一工作根内**：路径策略先解析、再校验、再以句柄操作；传输层不调用 `current_exe()`/`temp_dir()`。
5. **配额与清扫按 DUT 独立**：每 DUT 的未完成 TTL、配额、在途账、锁都只作用于该 DUT 的根；已完成制品不自动删除。
6. **删除 DUT 不删文件**：只停端点、删令牌、停清扫；文件保留到人工删除；清扫只做「记录与文件不一致」的墓碑对账。
7. **鉴权先于一切**：路径规范门禁、Bearer、名称/board/预算/subject 校验都在 handler 之前；版本判断不得成为 `Ok(())` 的旁路。

### 4.2 目录形态与创建顺序

推荐布局（`<work_root>` 为 `ensure_portable_work_dir` 解析出的唯一生效根）：

```text
<work_root>/
  duthub-data/                         # 运行数据：audit/runtime/...（既有）
  <DUT 名称>/                           # dut_artifacts 所有权目录
    .duthub-dut-id                      # 稳定 DUT ID（既有 dut_artifacts 约定）
    logs/                               # 既有串口日志
    .transfer/                          # 传输工作区（点目录，默认 list_files 跳过）
      data/<filename>                   # 已完成并发布的镜像（唯一副本）
      data/<id>.bin                     # 在途分片
      metadata/<id>.json                # 上传记录：dut_id/filename/subject/digest/offset
      metadata/<id>.verified            # 校验通过的摘要
      metadata/<id>.failed              # 失败标记（先写标记再删文件）
      metadata-tmp/                     # 上游原子写临时区
      transfer.lock                     # 单实例/单根独占锁
```

**创建顺序是硬约束**：`dut_artifacts::open(create=true)` 只在目录为空时写 `.duthub-dut-id`（`dut_artifacts.rs:53-70`）。如果先建 `<DUT>/.transfer` 再调 `open`，`dir.entries()?.next().is_none()` 永远为 false，owner 不会写入，随后 `symlink_metadata(OWNER)` 报「DUT 目录归属未知」。正确顺序：

1. `let (name, dir) = dut_artifacts::open_dut_for_write(artifact_root, dut_id, name)?;`（`dut_artifacts.rs:129`）——先创建/打开 DUT 目录并校验/写入 owner；
2. 在返回的 `Dir` 下 `create_dir(".transfer")`（或 `open_dir`），再用同一句柄创建 `data/`/`metadata/`/`metadata-tmp/`；
3. 对 `.transfer` 及各级子目录做 `plain()`（`dut_artifacts.rs:11-22`）与包含校验。

**遗留目录**：如果 DUT 目录非空但没有 owner（或 owner 与 dut_id 不符），`open` 会拒绝；transfer **不得**自行补写 owner，必须先走既有的 DUT 目录迁移/所有权确认流程，确认后再建 `.transfer`，否则 fail closed。单测必须覆盖：空目录、已存在 `.transfer` 但 owner 缺失（拒绝/迁移路径）、owner 与 dut_id 不符（拒绝）、DUT 名非法（`COM8` 等）、rename 后仍按 ID 找到。

其余要点：

- **不改上游 `DiskStore`**：`DiskStore::new().disk_root(root)` 接受任意根（`disk.rs:166-185`）；每 DUT 传 `<dut>/.transfer` 即可，`data/`/`metadata/`/`metadata-tmp/` 由上游生成。
- **点目录是纵深防御**：`dut_artifacts::list_files` 只列 `public_filename`，点目录被跳过（`dut_artifacts.rs:176-216`）；要展示已完成镜像，走显式的 `transfer` 视图（§4.5），不要放开整个点目录。
- **不把 `data/` 直接放在 DUT 根**：否则 `list_files` 会递归列出 `data/<id>.bin` 与 `metadata/<id>.json`，把在途分片和 sidecar 暴露给 `dut_files`/`dut_file_read`。
- **DUT 名称**：必须过 `dut_artifacts::name_component`；非法/保留名走既有 `archive_safe_name` 的稳定 slug（AGENTS #78）。

### 4.3 端点与令牌维度

配置（放 `SettingsFile`，默认值用 `skip_serializing_if` 保持 roundtrip，见 AGENTS #19/#24）：

```json
{
  "transfer": {
    "listeners": [
      { "id": "tr-<uuid>", "dutId": "<stable-dut-id>", "port": 18090, "enabled": true, "lanAccess": false }
    ],
    "allowedExtensions": ["img", "iso"],
    "maxUploadBytes": 10737418240,
    "uploadExpiresSecs": 86400,
    "reservedDiskBytes": 1073741824,
    "gcIntervalSecs": 300
  }
}
```

- **端口**：每个 `listener` 一个端口；`PortOwner::Transfer { dut_id }` 进入全局注册表，与 MCP、A2A、媒体端口一起做唯一性/占用探测（LAN 开启时按 `0.0.0.0` 探测）。保存/启用时用「MCP + enabled transfer + enabled A2A」合集做冲突检测；端口变更重启生效（AGENTS #45）。
- **令牌**：keyring service `duthub`，条目 `transfer/<dut_id>`（每 DUT 一个；如需多监听器可改 `transfer/<listener_id>`，但默认一 DUT 一端点）。CSPRNG 生成，只在创建时显示一次；不写入 `settings.json`、不写日志、不回显到 UI 的明文列表。**keyring 实现放宿主 desktop**，core 只提供 key 格式 helper（`transfer/<dut_id>`），不实现 `UploadAuthorizer`（避免 core → transfer 依赖）。
- **绑定**：宿主先 `std::net::TcpListener::bind(addr)`，注册端口，再把 listener 交给 `TransferServer::serve_on(listener)`；bind 失败把错误回传 UI（不要 `eprintln!`+continue）。非回环绑定要求令牌存在且非默认/非空；否则 fail closed，不启动该监听器。
- **生命周期**：启用/禁用、DUT 删除、端口/令牌变更都走同一个 `TransferRuntime::rebuild(settings, duts)`；**rebuild 只消费 `StartupContext` 注入的已加载 `SettingsFile`/`DutsFile`，不重新读 `settings.json`**（AGENTS #9：`load_settings` 有损坏改名备份副作用，进程内只准加载一次）。删除 DUT 时停监听器、删 keyring `transfer/<dut_id>`、清端口注册，但**不删文件**（基准 4）。停止时等在途请求按 `SHUTDOWN_GRACE` 收尾；在途上传可保留 offset 供客户端续传。
- **UI**：放在「连接管理 →（DUT）→ 镜像传输」或与 MCP 服务同级；显示端点 URL、端口、令牌状态（已配置/未配置）、DUT 名称，不显示令牌明文；不要放进通用 Agent 接入页（与 AGENTS #45 的 DUT 专用端点口径一致）。

### 4.4 跨 DUT 拒绝（不破坏既有测试）

**不修改 `UploadScope` 与 `TransferServer::new`**：

- `UploadScope` 有 5 处结构体字面量（`authorize.rs:171`、`tests/auth_failclosed.rs:120`、`tests/reject_paths.rs:689`、`tests/common/mod.rs:27`、`tests/ztus_client.rs:33`）；`TransferServer::new`/`with_free_space` 有 17 处调用（源码/测试/示例）。直接加 `dut_id` 字段或改 `new` 签名会让这些测试编译失败，违反「既有测试不许改」。
- 设计改为：`dut_id` 放在**每 DUT 的 `TransferServer`/`DutTokenAuthorizer`**，不放进 `UploadScope`。`UploadAuthorizer` trait 增加默认方法 `fn dut_id(&self) -> Option<&DutId> { None }`（默认实现不破坏现有 impl）；生产 `DutTokenAuthorizer` 返回 `Some(dut_id)`，`TransferServer::with_host(config, host)` 在构造时校验 `authorizer.dut_id() == Some(host.dut_id)`，不匹配拒绝启动。
- `TransferServer::new(config, authorizer)` 保留为**测试/未绑定兼容构造**（`#[doc(hidden)]`），生产代码只允许 `with_host`；加一条源码契约测试/CI grep，断言 `apps/` 不调用 `new`。这样 `tests/ztus_client.rs` 等既有测试不改仍绿。
- **board**：保留 `UploadScope.boards`/`AuthContext.board` 作为 DUT 内的**次级作用域**（带 board 但不在 scope 仍 403）；它不再承担租户边界。若产品要求彻底改成展示字段，需另行裁定并改 5 处测试字面量——本设计不假装这是零改动。

三层拒绝：

1. **结构层**：每 DUT 一个 `TransferServer`，`TransferState` 持 `dut_id: DutId`；authorizer 只装该 DUT 的令牌。A 的 token 在 B 的 authorizer 里查不到 → 401。
2. **构造层**：`with_host` 校验 `authorizer.dut_id() == host.dut_id`；不一致直接拒绝启动，不进入请求路径。
3. **元数据层**：`board` 若出现，必须等于绑定 DUT 的名称/ID（按产品规则），不等 → 400/403；**省略允许**（服务端已知 DUT），保留 `ztus_client` 的「只发 filename」用例。

其他不变量：

- 传输路径不接受 `dutId` 查询参数/请求头；如果未来做「单端口多 DUT」，路径中的 DUT 段必须与 token 的 DUT 完全一致，否则 403，且绝不能「省略即全局」。
- `dut_files`/`dut_file_read` 继续按 `dut_id` 走 `authorize_scoped_read`（AGENTS #44）；传输文件通过 §4.5 的视图进入同一 DUT 目录，不新增跨 DUT 读路径。
- **无 subject 的上传**：生产模式对 `upload_subject=None` 的 PATCH/HEAD/DELETE 应 fail closed（403），避免 F-01 创建的旧记录被任意 token 操作；或在 create 时注入 per-DUT sentinel subject。该行为不改变现有测试（现有测试的 create 都带 subject）。
- 审计/告警记录 `dut_id` 与 `listener_id`，不记录 token 明文。

### 4.5 人工删除前提下的可发现性

基准 4 不要求级联删除，但要求「位置正确且可按 DUT 找到」。设计：

- **按 DUT 定位**：文件在 `<work_root>/<DUT 名称>/.transfer/data/<filename>`；DUT 目录由 `.duthub-dut-id` 归属（`dut_artifacts::open`，`dut_artifacts.rs:35-80`），改名/重连后仍按稳定 DUT ID 校验所有权。创建顺序见 §4.2。
- **MCP 可见（transfer_view 契约）**：
  - `id` 固定为 `transfer/<filename>`（相对 DUT 根，不暴露绝对路径、不允许 `..`、不允许盘符）；
  - 只列「`metadata/<id>.json` 存在、`offset==size`、`data/<filename>` 存在且 `plain()`」的完成制品；
  - 返回 `{ id, bytes, sha256? }`；在途 `.bin`、`metadata`、`metadata-tmp` 永不暴露；
  - `dut_file_read` 的 `file_id` 走同一视图与 `read_file` 规则（`dut_artifacts.rs:218-251` 的 `public_filename`/`plain` 检查）；
  - 权限与 `dut_files` 相同（`authorize_scoped_read`），不新增跨 DUT 读路径；
  - 单测：完成前不可见、完成后可见、`.bin`/sidecar 不可见、`..`/绝对路径 400、跨 DUT 403。
- **索引**：`metadata/<id>.json` 由上游写入，包含 `filename`、size/offset、`dut_id`（本 crate 在 create 时注入 server-owned 字段）；不另建一份易漂移的 index。若需要「原文件名/发布名/摘要」人读清单，生成 `metadata/index.jsonl` 作为派生缓存，启动/清扫时重建，不作为权威。
- **UI**：DUT 详情页显示「镜像目录」与「打开文件夹」；删除 DUT 前提示「文件不会自动删除，需手动清理」；删除后端点停止、令牌删除。现有 `run_remove_dut` 清端点/凭据/Agent 目录、不删 DUT 目录（`agent_ui.rs:233-265`），与「不级联」一致；本设计不把「删 DUT 目录」加进去。
- **手动删除的对账**：用户删除 `data/<filename>` 后，下一轮清扫发现「记录存在、文件不存在」→ 删除该记录与 `.verified`/`.failed` 标记（墓碑对账），不报错、不重建文件；`dut_files` 自然不再列出。删除整个 DUT 目录同理：下次启动/打开时目录不存在则视为空，不自动重建空目录（或按产品习惯重建，但不得写回已删除文件）。
- **不级联**：删除 DUT 不删除 `.transfer` 下任何字节；只停端点/删令牌。若产品希望「删除 DUT 同时删文件」，那是另一个显式选项，不默认。

### 4.6 配额与清扫

**配额**：

- 每 DUT 配额来自 `SettingsFile.dut_storage_gib`（默认 `DEFAULT_DUT_STORAGE_GIB = 10`，`dut_quota.rs:12`），通过宿主注入的 `TransferQuota` 生效；传输 crate 不知道 `DutQuota` 的 `&Dir` 细节。
- create 时 `reserve(dut_id, declared_len)` 原子预留；没有 `Upload-Length` 的请求直接 400（服务端不宣告 deferred length，见 `store.rs` 的扩展裁剪），避免无账可记。
- PATCH 用 `write_limited` 把剩余配额作为上限；超限 413/507 并回滚到 committed offset。
- 完成：预留转「已用」；失败/过期/客户端 DELETE：释放。已发布文件计入 DUT 已用，直到人工删除（对账后释放）。零长度上传也计入（F-18）。
- 全局空闲空间探测保留为第二道闸（防止写满整盘），但不再是唯一的容量策略；`maxUploadBytes` 默认不得大于每 DUT 容量（10 GiB），否则单个上传就必然超出该 DUT 的总容量；`API_CONTRACT.md:123` 的 10 GiB 与 `config.rs` 的 30 GiB 应在文档/默认值上对齐。

**清扫**：

- 每 DUT 的 `ExpiringDiskStore` 只扫自己的根；TTL 只作用于未完成上传。
- 枚举四类：`metadata/*.json`（正常）、`metadata/*.failed`（失败标记）、`data/*.bin` 无记录（孤儿）、`metadata-tmp/*` 超龄（临时残留）；另加「已完成记录但 `storage.path` 不存在」（人工删除后的墓碑）。
- 删除持每上传写锁；锁被占留到下一轮。`Err(_) => false`/`Err(_) => {}` 改为分类：读不出的记录按损坏处理（标记/隔离后删除），删除失败记日志并保留标记，不静默。
- 已完成制品**永不按 TTL 删除**（基准 4 + `TRANSFER_RECEIVER.md:107-108`）；只做墓碑对账。零长度成品同样不自动删，但计入配额/总数。
- 同根单实例：`transfer.lock` 独占；第二个进程/实例启动失败并提示，避免 F-08 的双写/互删。

### 4.7 「所有文件必须在指定工作路径内」的校验

**先承认现有解析器不止一套**：

- `store.rs:80` `ensure_portable_work_dir`、`:108` `ensure_portable_child`、`:138` `ensure_agent_dir`、`:168` `agent_dut_dir`；
- `dut_artifacts.rs:35` `open`、`:103` `rename_owned`、`:129` `open_dut_for_write`；
- `duthub-sandbox/src/policy.rs:476` `CellSpec::for_dut`、`:396` `ScratchLayout`。

新增一个独立 `PathPolicy` 会变成第五套解析器，违反基准 5。设计改为 **transfer 专用 facade**（`duthub_core::transfer_path`，或 `duthub-transfer` 内的薄封装）：

```rust
pub struct TransferPathPolicy { work_root: PathBuf, artifact_root: PathBuf }

impl TransferPathPolicy {
    /// Wraps ensure_portable_work_dir; does not invent a new work-root rule.
    pub fn resolve(exe_dir: &Path, settings: Option<&str>, cli: Option<&str>) -> io::Result<Self>;
    /// Reuses dut_artifacts::open_dut_for_write; only adds containment/reparse checks.
    pub fn open_dut(&self, dut_id: &str, name: &str) -> io::Result<(String, Dir)>;
    /// Absolute/relative transferDir containment; canonicalize + component compare.
    pub fn resolve_transfer_root(&self, configured: Option<&str>) -> io::Result<PathBuf>;
    pub fn ensure_contained(&self, candidate: &Path) -> io::Result<PathBuf>;
}
```

`ensure_contained` 的判定顺序（缺一不可）：

1. **先归一化输入**：拒绝 UNC（`\\`、`//`）、设备路径（`\\?\GLOBALROOT`、`\\.\`）、盘符相对（`D:images`）、无盘根（`\images`）；相对路径按 `work_root` 拼接；绝对路径保留但必须走第 3 步。
2. **词法拒绝**：路径组件中出现 `ParentDir`（`..`）一律拒绝；不要先 `join` 再指望 `canonicalize` 兜底。当前 bug 正是「绝对路径在 `..` 拒绝之前返回」（`config.rs:236-239`）。
3. **先校验祖先再创建**：先 canonicalize `work_root`；对 `candidate` 的**最近存在祖先**做 canonicalize，并按组件边界确认它在 `work_root` 内（绝对路径不在根内立即拒绝，**不得先创建任何目录**）；只有通过后才 `create_dir_all` 缺失段，再对完整 candidate canonicalize 复验。Windows 上统一剥离 `\\?\`/`\\?\UNC` 前缀；按 `Component` 逐段比较，大小写不敏感（Windows），必须是**组件边界**的 `starts_with`，不能用字符串前缀（`C:\work` 不能包含 `C:\workshop`）。
4. **拒绝 reparse point**：从 `work_root` 到 `candidate` 的每一段（含 DUT 目录、`.transfer`、`data`、`metadata`）都用 `symlink_metadata` 检查；Windows 判 `FILE_ATTRIBUTE_REPARSE_POINT (0x400)`，非 Windows 判 `file_type().is_symlink()`；复用 `dut_artifacts::plain()`（`dut_artifacts.rs:11-22`）的语义。目标本身是目录/文件时也要检查，不能只查父目录。
5. **创建后再复核 + 接受残余 TOCTOU**：`create_dir_all` 可能被并发替换为 junction；创建后重新 `canonicalize` + 逐段 `plain` 检查。上游 `DiskStore` 按路径操作，不能直接接收 `cap-std::Dir` 句柄；因此 setup 校验 + 对 DUT/`.transfer` 做 ACL 加固（`os_acl::harden_private_dir`）+ 单根锁只能降低、不能彻底消除同用户并发替换的风险。若要求彻底消除，需要在 `DataStore` 外包一层用 `cap-std` 相对句柄写入（更大改动，另立任务）。`dut_artifacts` 的读取路径仍走 `Dir` 句柄。
6. **临时文件也在根内**：`NamedTempFile::new_in(root)`，禁止 `std::env::temp_dir()`；`x-duthub-artifact` 回执只能是根内相对路径或 artifact id。
7. **发布名仍要校验**：上游 `DiskStore::sanitize_filename`（`disk.rs:333-345`）只剥目录、拒绝 `.`/`..`，不校验扩展名/保留名；不能把它当作基准 1/安全边界的全部。
8. **失败即拒绝**：绝对路径不在工作根内 → `InvalidConfig`/`TransferDir`，不静默回退到默认。工作根本身不可写/不可 canonicalize → 启动失败并提示。

对既有 `ensure_portable_work_dir` 的补充：它已 `canonicalize`（`store.rs:90-91`），但不拒绝工作根本身是 junction，也不校验绝对 `workspaceDir` 的包含关系（任意绝对路径都接受）。在 T24 收口前，transfer facade 只复用 `ensure_portable_work_dir` + `dut_artifacts` 两个入口，不声称统一所有解析器；`ADR-WORKDIR` 的双解析器（CLI 优先 vs settings 优先）必须由产品先定唯一入口。

### 4.8 分阶段落地与验收

**阶段 0（独立 P0，不改 DUT 模型）**：路径规范门禁 + `//`/`.`/`..` 回归；`authorize_resource` fail closed；`Tus-Resumable` 版本门与鉴权解耦（F-17）。验收：`//transfer`/`/./transfer`/`/foo/../transfer` 400；无令牌/错令牌 401；读记录错误不 200/204；既有 92 测试不回归（新增用例只加不改）。

**阶段 1（路径策略层）**：`TransferPathPolicy` 复用 `ensure_portable_work_dir` + `open_dut_for_write`；`resolve_transfer_dir` 改为 `(work_root, configured)`；绝对路径包含校验、reparse 拒绝、临时文件在根内。**契约裁定**：`config.rs:409-416` 的 `transfer_dir_absolute_passes_through` 钉死了旧行为，改包含校验必须同步改这条测试意图（属契约变更）；阶段 1 不能同时宣称「测试不改」。验收：`C:/a/../../x` 拒绝；工作根外绝对路径拒绝；junction 拒绝；默认根在 `<work_root>/duthub-data/images`；`--workdir` 与 settings 的优先级有唯一测试。

**阶段 2（租户维度，保持既有测试）**：`DutId` newtype；`TransferState.dut_id`；`DutTokenAuthorizer` 返回 `dut_id`；`TransferServer::with_host` 校验 authorizer 的 DUT；`new` 保留为测试/未绑定兼容构造 + CI 禁止 `apps/` 使用；`board` 保留为 DUT 内次级校验；生产拒绝 `upload_subject=None` 的 PATCH/HEAD/DELETE。验收：`tests/ztus_client.rs` 等既有测试**不改仍绿**（因为 `UploadScope`/`new` 未变）；新增 A token 在 B 监听器 401、构造时不匹配拒绝启动、无 subject 上传 403。

**阶段 3（存储布局）**：每 DUT `.transfer` 根；`TransferRoots` 注入；`open_dut_for_write` 先于 `.transfer`（§4.2）；`dut_artifacts` 的 transfer 只读视图；人工删除墓碑对账。验收：A 的文件只在 A 目录；`dut_files` 能看到 A 的完成镜像、看不到 `.bin`/sidecar；删除文件后列表消失、记录被清扫；删除 DUT 不删文件；`COM8`/owner 缺失/owner 不符有测试。

**阶段 4（端点生命周期）**：`TransferListener` 配置、`PortOwner::Transfer`、宿主 bind + `serve_on`、UI、启停/删除联动；`rebuild` 只消费 `StartupContext` 已加载配置。验收：端口与 MCP/A2A 冲突被拒；bind 失败可见；LAN 无令牌 fail closed；删除 DUT 停端点/删令牌/留文件。

**阶段 5（配额/清扫/持久化）**：`TransferQuota` 原子预留；每 DUT 10 GiB 与单次上限对齐；零长度计入配额/总数；清扫四类残留 + 墓碑；单根锁；fsync 或改文档。验收：并发 create 不超卖；每 DUT 10 GiB；零长度不能无限累积；孤儿/临时/损坏记录可回收；断电承诺与实现一致。

**分层门禁（可量化）**：

1. `cargo tree -p duthub-core -e normal | grep -q duthub-transfer` 必须失败（core 不依赖 transfer）。
2. `grep -rn "current_exe\|temp_dir\|keyring" crates/duthub-transfer/src` 在重构后必须为空（DUT/根/令牌由宿主注入）。
3. `grep -rn "TransferServer::new" apps/` 必须为空（生产只用 `with_host`）。
4. 「新增/删除 DUT 端点」的生产代码 `git diff --name-only`（排除 docs/tests/配置数据）必须只落在 `crates/duthub-core/src/{transfer,keys,ports}.rs` + `apps/desktop/src/transfer_runtime.rs`（端点生命周期层），不得出现 `duthub-transfer/src/{router,store,authorize,config,lib}.rs`。
5. 「改路径策略」的 diff 只允许 `duthub-core/src/transfer_path.rs` + 测试；不得出现 transfer crate。
6. 「换鉴权」的 diff 只允许宿主 authorizer 实现 + 测试；不得出现 router/store/path。
7. 新增 `tests/ztus_client.rs` 之外的跨 DUT/无 subject 用例；既有文件不改。
8. 路径回归：`//transfer`、`/./transfer`、`/foo/../transfer`、`/%2Ftransfer` 全部 400（最后一项按用户复核 404，需统一为 400 或明确的拒绝码）。

---

## 5. 分层与解耦边界

### 5.1 四层职责与依赖方向

| 层 | 职责 | 依赖 | 明确不做 |
| --- | --- | --- | --- |
| **路径策略层**（`duthub-core::transfer_path`，薄 facade） | 解析唯一工作根；复用 `ensure_portable_work_dir` + `dut_artifacts::open_dut_for_write`；只新增包含/reparse/临时文件校验；提供已校验根路径 + DUT `Dir` 句柄（上游 `DiskStore` 仍按路径写，残余 TOCTOU 见 §4.7） | 仅 std + `cap-std`；不依赖 tokio/HTTP | 不发明第五套工作根/DUT 名规则；不知道 HTTP、令牌、上传、端口 |
| **鉴权/租户层**（`duthub-transfer::authorize` trait + desktop 实现） | Bearer→`UploadScope`；per-DUT `DutTokenAuthorizer`；过期/长度/主体/操作检查；跨 DUT 结构隔离 | trait 在 transfer；keyring 实现在 desktop | 不知道路径、存储布局、端口 |
| **存储布局层**（`duthub-transfer::store` + `TransferRoots` 注入） | 每 DUT 根、staging/publish、TTL、quota hook、清扫 | 上游 `DiskStore`；root 由宿主传入 | 不知道 DUT 名、端口、令牌 |
| **端点生命周期层**（`duthub-core` 的 `TransferListener` + `PortRegistry` + desktop `TransferRuntime`） | 配置段、端口唯一性/占用、bind、启停、凭据生命周期、UI | core 模型 + 端口注册表 + desktop | 不知道 tus 路由/协议细节 |

依赖方向（箭头 = 依赖）：

```text
duthub-core (model / transfer_path / ports / dut_artifacts / dut_quota / keys)
      ↑
duthub-transfer (HTTP + tus + traits; no current_exe / keyring / DUT name)
      ↑
duthub-desktop (host: settings, keyring, TransferQuota/CompletedSink impl, TransferRuntime, UI)
      ├──→ duthub-mcp (read-only artifact view)
      └──→ duthub-a2a (ports re-export only)
```

约束：

- `duthub-core` **不得**依赖 `duthub-transfer`；`UploadAuthorizer` 实现在 desktop（或独立 adapter crate），core 只定义 key 格式 helper（`transfer/<dut_id>`），不实现 trait。
- `duthub-transfer` **不得**依赖 `duthub-a2a`/`duthub-mcp`/`duthub-desktop`，不得调用 `current_exe()`、`std::env::temp_dir()`、keyring、`dut_artifacts`；它只认 `DutId`、`TransferRoots`、trait。
- 宿主 desktop 是唯一装配点；MCP 只通过 core 的只读视图看文件，不直接依赖 transfer。

### 5.2 接缝清单（收敛同步点的具体位置）

1. **`TransferHostContext` 构造函数参数**（生产入口；`new` 保留兼容）：

```rust
pub struct TransferHostContext {
    pub dut_id: DutId,
    pub roots: TransferRoots,              // staging + publish, resolved by transfer_path
    pub authorizer: Arc<dyn UploadAuthorizer>, // DutTokenAuthorizer, dut_id must match
    pub quota: Arc<dyn TransferQuota>,
    pub locker: Arc<dyn Locker>,           // one per canonical root
    pub completed: Arc<dyn CompletedArtifactSink>,
}

impl TransferServer {
    pub fn with_host(config: TransferConfig, host: TransferHostContext) -> Result<Self, TransferError>;
    pub async fn serve_on(&self, listener: std::net::TcpListener) -> Result<TransferListenerHandle, TransferError>;
    /// Compatibility for existing tests/unbound use; production must use with_host.
    #[doc(hidden)]
    pub fn new(config: TransferConfig, authorizer: Option<Arc<dyn UploadAuthorizer>>) -> Result<Self, TransferError>;
}
```

2. **`UploadAuthorizer::dut_id` 默认方法**：`fn dut_id(&self) -> Option<&DutId> { None }`；`DutTokenAuthorizer` 返回 `Some`；`with_host` 校验匹配。不改 `UploadScope` 字段，避免 5 处字面量断裂。
3. **`TransferPathPolicy`**：§4.7 的薄 facade；`resolve_transfer_dir` 改为接收 `&TransferPathPolicy`，不再接收 `exe_dir`。
4. **`TransferRoots`**：

```rust
pub struct TransferRoots {
    pub staging: PathBuf,   // <dut>/.transfer
    pub publish: PathBuf,   // <dut>/.transfer/data (upstream data dir)
}
```

5. **`TransferQuota` trait**：原子 `reserve/release`；宿主用 `DutQuota`/`measure` 实现；transfer 只在 create/patch/complete/abort 调用（hook 是一次性接缝安装）。
6. **`PortRegistry` + `TransferListener`**：`PortOwner::Transfer { dut_id }`；`TransferListener` 进 core model；`TransferRuntime::rebuild(settings, duts)` 是唯一启停入口，只消费 `StartupContext` 已加载配置。
7. **`dut_artifacts` transfer 只读视图**：MCP `dut_files`/`dut_file_read` 通过它看完成制品；不新增第二套路径解析。
8. **`CompletedArtifactSink`**：完成时 `sync_all`、索引刷新、UI 通知；失败不改变 HTTP 语义（或按产品决定 500）。

### 5.3 一处改动会牵动哪些文件（现状）

| 变更 | 现在必须同步改（隐式耦合） |
| --- | --- |
| 新增/修改 DUT 维度 | `authorize.rs`（`UploadScope`/`AuthContext`）、`router.rs`（`KEY_BOARD`/`authorize_create`/`authorize_resource`/guard）、`config.rs`（root/defaults）、`lib.rs`（构造/`serve`）、`store.rs`（root/inflight）、`names.rs`（board 校验）、`core/model.rs`（`SettingsFile`）、`core/keys.rs`（keyring）、`a2a/ports.rs`（`PortOwner`）、`desktop`（runtime/UI）、`mcp/server.rs`（`dut_files` 可见性）、docs/tests |
| 改路径策略 | `config.rs` + `lib.rs` + `store.rs` + `apps/desktop/src/main.rs` + `mcp_runtime.rs` + docs |
| 换鉴权实现 | `authorize.rs` + `router.rs` + `lib.rs` + `config.rs` + 宿主 |
| 改存储布局 | `store.rs` + `lib.rs` + `config.rs` + `names.rs` + `mcp/server.rs` + docs |
| 加一个端点 | `lib.rs`（`serve`）+ `config.rs` + desktop + `ports.rs` + `keys.rs` |

这张表就是基准 5 所指的「一处新增要同步改多处」；也是 AGENTS #13、#44/#92 的同族。

### 5.4 改造后：一次性接缝安装 vs 后续层内演进

第一版说「transfer crate 改动 0」是错的：`TransferQuota` 的 create/patch hook、`render_rejection` 的状态码、`TransferHostContext`/`serve_on` 都在 transfer crate，必须一次性安装。正确的口径是：**先做一次性接缝安装，之后每类策略变更只落在对应层**。

**表 A：一次性接缝安装（必须改 transfer，属阶段 0-2 的前置）**

| 接缝 | 改动 |
| --- | --- |
| 路径规范门禁 / 鉴权前置 | `router.rs`（guard、`target`、`authorize_create`、`authorize_resource`） |
| `TransferHostContext` / `serve_on` / `TransferServer::new` 兼容 | `lib.rs` |
| `dut_id`、per-DUT state | `router.rs`、`lib.rs`、`authorize.rs`（只加 trait 默认方法，不改 `UploadScope` 字段） |
| `TransferQuota` hook | `router.rs`（create/patch/complete/abort） |
| `TransferRoots` | `lib.rs`、`store.rs`、`config.rs` |
| bind 策略抽成宿主/共享函数 | `lib.rs` 的 `assert_bind_allowed` 或 core 共享函数 |
| 状态码/文档映射 | `router.rs` 的 `render_rejection`、`docs/` |

**表 B：接缝安装后，后续变更只动一层**

| 变更 | 只改 | transfer crate 改动 |
| --- | --- | --- |
| 运行时新增/删除一个 DUT（数据驱动） | `duts.json`/`settings.json` 数据 + `TransferRuntime::rebuild`；0 代码文件 | 0 |
| 首次实现/修改端点层（配置 schema、端口策略、凭据生命周期） | 端点生命周期层：`core` 的 `TransferListener` + `keys.rs` key helper + `ports.rs` + `desktop::transfer_runtime` | 0 |
| 改路径策略（包含/reparse/工作根） | 路径策略层：`core::transfer_path` + 单测 | 0 |
| 换鉴权（keyring/静态/外部 IdP） | 鉴权层：`desktop` 的 `UploadAuthorizer` 实现 | 0 |
| 改存储布局（staging/publish/隐藏目录） | 存储层：宿主 `TransferRoots` 构造 + `dut_artifacts` 视图 | 0（若布局需要新字段，仅扩展 `TransferRoots`，不动路由/协议） |
| 改配额（每 DUT/全局/预留） | 鉴权/资源层：宿主 `TransferQuota` 实现 | 0（hook 已在表 A 安装） |
| 改端口/绑定策略 | 端点层：`PortRegistry` + 宿主 runtime | 0 |
| 改文档/状态码 | docs + `render_rejection` 一处 | 1 处（`render_rejection`）；这不是「DUT 维度」变更 |

「只改一层」的判定单位是**层**，不是「一个文件/一个 crate」：运行时新增/删除一个 DUT 是数据变更（`duts.json`/`settings.json` + `rebuild`），0 代码；首次实现或修改端点层才动 core 配置、key helper、端口注册表、desktop runtime 四个文件，且不碰 transfer/路径/鉴权/存储。若要求「只改一个文件」，需要把 `TransferRuntime` 做成单文件模块并把配置/key/ports 都收进去，属于另一个设计取舍。

### 5.5 收敛同步点的具体做法

1. **单一 DUT 身份类型**：`DutId` newtype（core），`TransferState`/`TransferListener`/keyring 条目/审计都用它；`UploadScope` 不改字段，`DutTokenAuthorizer` 持有 `DutId`。编译器阻止「传错字符串」。
2. **单一配置类型**：`TransferConfig` 移到 core（持久化部分），transfer 只消费；`SettingsFile.transfer` 用它，避免两份 serde 定义。`transfer_dir` 由 `TransferRoots` 承担，`lan_access` 进每个 `TransferListener`，避免两个根/两个开关来源。
3. **单一端口注册表**：`PortRegistry` 作为唯一 `reserve` 入口；MCP/A2A/transfer 都在启动前 `reserve`，失败即报 UI；不要在各自 crate 里复制 `assert_bind_allowed`。若本轮不移动 `PortRegistry`，最小改动是扩展 `duthub-a2a::ports::PortOwner`（transfer 仍不依赖 a2a）；长期建议下沉 core 并由 a2a re-export。
4. **单一 DUT 目录归属**：`dut_artifacts::open_dut_for_write` 的 `.duthub-dut-id` 是唯一所有权判定；transfer 不自己拼 `<dut>` 路径，而是接收已打开的 `Dir`/已校验的根；创建顺序见 §4.2。
5. **单一 artifact 视图**：MCP `dut_files`/`dut_file_read` 与 transfer 的完成回执共用 `dut_artifacts` 的路径规则；不要在 transfer 里再定义一套 `public_filename`（`names.rs` 与 core 的保留名判定已经分叉，F-12）。
6. **单一启停入口**：`TransferRuntime::rebuild(settings, duts)` 只消费 `StartupContext` 注入的已加载配置，与当前运行集合做 diff；新增/删除 DUT、改端口/令牌都只改这一处，UI/页面不各自维护列表。**不得重新读 `settings.json`**（AGENTS #9）。
7. **单一失败语义**：`Rejection`/`TusError` 到 HTTP 的映射只在 `render_rejection` 一处；新增错误码时编译器/测试覆盖，不在 handler 里散落 `StatusCode::…`；鉴权/版本/名称/预算的失败顺序固定，版本门不得返回 `Ok(())`。

### 5.6 反例与约束

- **反例 1**：把 `dut_id` 做成 HTTP 路径段 `/transfer/<dut_id>/…`，但 token 仍是全局表——这是「检查隔离」，不是结构隔离；路径归一化再出一次偏差（F-01 同族）就会跨 DUT。应优先每 DUT 独立实例。
- **反例 2**：把 `TransferConfig` 留在 transfer crate，同时在 core 复制一份 `TransferSettings` 做 `SettingsFile`——两个 serde 定义必然漂移。
- **反例 3**：在 `dut_files` 里直接列 `.transfer/data` 整个目录——会把在途 `.bin` 和 sidecar 暴露给 MCP；必须走完成记录的只读视图。
- **反例 4**：删除 DUT 时顺手 `remove_dir_all`——违反基准 4 的「文件由人工删除」；只能停端点、删令牌、留文件。
- **反例 5**：先建 `<DUT>/.transfer` 再调 `dut_artifacts::open`——owner 永远不会写入，`dut_files` 永久报「目录归属未知」（`dut_artifacts.rs:53-70`）。
- **反例 6**：新增一个独立 `PathPolicy` 而不复用 `ensure_portable_work_dir`/`dut_artifacts`——会成为第五套解析器，违反基准 5。

---

## 6. 需同步的文档清单

| 文件 | 要改的点 |
| --- | --- |
| `docs/SECURITY.md` | 新增「镜像传输（transfer）」小节：每 DUT 端点/令牌；默认 loopback、LAN 显式开启且明文；30 GiB 写入面；`//`/点段路径门禁与 F-01；fail-closed；路径包含/reparse；人工删除不级联；token 只存 keyring；版本门不得绕过鉴权。当前全文 0 次提及 transfer。 |
| `docs/API_CONTRACT.md` | ① `:123` 的 10 GiB 是每 DUT 总容量，与 `config.rs` 的 30 GiB 单次上限对齐/说明；② 增加 `transfer` 配置段/端点语义（`listeners[]`、token、状态码、artifact 回执）；③ `ARCHITECTURE_REVIEW.md:68-72` 的 `artifact_prepare/status/delete`、`flash_start/status` 要么补实现要么删承诺；④ 补 `storage_probe_failed`、`verification_failed`、`dut_mismatch`、`non_canonical_path` 等失败码；⑤ 明确已完成镜像是否参与每 DUT 淘汰。 |
| `docs/guides/TRANSFER_RECEIVER.md` | ① `:80` 默认根从 `<exe_dir>` 改为 `<work_root>`，绝对 `transferDir` 加包含校验；② `:81-82` 的断电一致性承诺改为「无保证」或在完成前 `sync_all`；③ `:100-103` 状态码表补 400 非规范路径、403 DUT 不匹配、500 两类错误；④ 补 `x-duthub-artifact` 头与相对路径语义；⑤ `:107-109` 清扫补 `data/` 孤儿、`metadata-tmp`、损坏记录、人工删除墓碑；⑥ 补每 DUT 端点/令牌/端口/配额、零长度上传、`Tus-Resumable` 版本门、整文件哈希的并发/超时边界。 |
| `docs/ARCHITECTURE_REVIEW.md` | ① `:31-36`「已落地」与 `:121`「尚未实现或压测」统一为「接收端已实现并单测通过；未接入产品、未做 30 GiB/断电压测」；② `:38` 的「已落地实现」链接补「未接线」；③ `:68-72` 的 artifact/flash 工具标注「设计/未实现」或移出已实现范围。 |
| `docs/DATA_MODEL.md` | DUT 目录布局补 `.transfer/`（data/metadata/metadata-tmp/transfer.lock）、`.duthub-dut-id` 所有权与**创建顺序**（先 `open_dut_for_write` 再建 `.transfer`）、人工删除与墓碑对账、MCP `transfer_view` 契约（`transfer/<filename>`、只列完成制品）；补 transfer listener 配置归属。 |
| `docs/design/CONFIG_DIR_DESIGN.md` | 补 `transfer` 配置段（默认值 `skip_serializing_if`、roundtrip 契约）、工作根锚点、绝对路径包含校验、keyring `transfer/<dut_id>`、端口注册；说明 `rebuild` 只消费启动期已加载配置。 |
| `docs/decisions/ADR-WORKDIR.md` | T24 仍 deferred；基准 1 要求唯一工作根，需明确 transfer 采用 `ensure_portable_work_dir`（CLI > settings）还是先实施 T24；同时修正 `store.rs:85-96`（CLI 优先）与 `:301-304`（settings 优先）的口径。 |
| `docs/examples/settings.json` | 新增 `transfer` 示例段（默认值省略，保持 roundtrip）；当前文件无任何 transfer 键。 |
| `AGENTS.md` | 新增坑条目：① 守卫用原始 URI、路由用 `parse_path_parts` 归一化（`//`/`.`/`..`）导致绕过（坑 #97 同族）；② 每 DUT transfer 端点/令牌/端口；③ 路径包含 + canonicalize + reparse 拒绝 + 先校验祖先再创建；④ `MemoryLocker` 每实例/进程内，同根要单实例锁；⑤ 上游不 fsync，别在文档里承诺断电一致性；⑥ `SettingsFile` 新段必须 `skip_serializing_if`（#19/#24 同族）；⑦ `.duthub-dut-id` owner 只在空目录写入，transfer 必须先 `open_dut_for_write` 再建 `.transfer`；⑧ `Tus-Resumable` 版本门不能当鉴权旁路；⑨ 零长度上传会立即发布且永不过期，需计入配额/总数；⑩ 全局 `verify_gate` 串行整文件哈希，需按 DUT/并发限流。 |
| `docs/ARCHITECTURE.md` | 目录树已列 `duthub-transfer`；补「传输运行时/端口注册/凭据」的箭头与「未接线」状态。 |
| `43c58dc:docs/AGENT_IMPLEMENTATION_TASKS.md` | `:39-40` 的 transfer 描述补「已实现、未接线、安全审核 F-01/F-02/F-03/F-17 未关闭」。 |
| `README.md` | 若产品对外暴露 transfer 端点，补一段「局域网明文 + 每 DUT 令牌 + 默认关闭」的说明；当前 README 无 transfer 内容。 |
| `docs/reviews/TRANSFER_SECURITY_REVIEW.md` | 本报告；建议从 `docs/SECURITY.md` 与 `docs/ARCHITECTURE_REVIEW.md` 链接，作为接线前的门槛文档。 |

---

## 附录 A：本轮端到端复现记录

环境：Windows 10.0.26300，Git Bash，仓库 `D:\Users\luo-j\Desktop\serialmcp\duthub`；二进制 `target/debug/examples/receiver.exe`（2026-11-25 00:22 构建，源文件时间 2026-11-24 21:50）；数据目录 `%TEMP%/duthub-transfer-review2-*`，测后 `taskkill //F //IM receiver.exe` 并删除临时目录；未在仓库内创建文件。

上游源码路径：`C:/Users/luo-j/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/salvo-tus-1.2.1/`、`salvo_core-1.2.1/`。

启动：

```text
./target/debug/examples/receiver.exe --port 18082 --token review-token --dir "$TEMP/duthub-transfer-review2-<pid>/root"
# 输出：serving http://127.0.0.1:18082/transfer  root=...  token=review-token
```

请求与结果（`curl 8.21.0`，全部 `--http1.1`，绕过请求带 `--path-as-is`）：

```bash
# 1) canonical control: 401
curl --http1.1 -s -o /dev/null -w '%{http_code}\n' -X POST http://127.0.0.1:18082/transfer \
  -H 'Tus-Resumable: 1.0.0' -H 'Upload-Length: 5' -H 'Upload-Metadata: filename Ym9vdC5pbWc='

# 2) no token, double slash: 201
curl --http1.1 -s --path-as-is -o /dev/null -w '%{http_code}\n' -X POST http://127.0.0.1:18082//transfer \
  -H 'Tus-Resumable: 1.0.0' -H 'Upload-Length: 5' -H 'Upload-Metadata: filename Ym9vdC5pbWc='

# 3) no token, dot segment: 201
curl --http1.1 -s --path-as-is -o /dev/null -w '%{http_code}\n' -X POST http://127.0.0.1:18082/./transfer \
  -H 'Tus-Resumable: 1.0.0' -H 'Upload-Length: 5' -H 'Upload-Metadata: filename Ym9vdC5pbWc='

# 4) no token, parent segment: 201
curl --http1.1 -s --path-as-is -o /dev/null -w '%{http_code}\n' -X POST http://127.0.0.1:18082/foo/../transfer \
  -H 'Tus-Resumable: 1.0.0' -H 'Upload-Length: 5' -H 'Upload-Metadata: filename Ym9vdC5pbWc='

# 5) canonical evil.exe with token: 400
curl --http1.1 -s -o /dev/null -w '%{http_code}\n' -X POST http://127.0.0.1:18082/transfer \
  -H 'Tus-Resumable: 1.0.0' -H 'Authorization: Bearer review-token' -H 'Upload-Length: 5' \
  -H 'Upload-Metadata: filename ZXZpbC5leGU='

# 6) bypass evil.exe no token: 201
curl --http1.1 -s --path-as-is -o /dev/null -w '%{http_code}\n' -X POST http://127.0.0.1:18082//transfer \
  -H 'Tus-Resumable: 1.0.0' -H 'Upload-Length: 5' -H 'Upload-Metadata: filename ZXZpbC5leGU='

# 7) bypass CON.tar.img no token: 201
curl --http1.1 -s --path-as-is -o /dev/null -w '%{http_code}\n' -X POST http://127.0.0.1:18082//transfer \
  -H 'Tus-Resumable: 1.0.0' -H 'Upload-Length: 5' -H 'Upload-Metadata: filename Q09OLnRhci5pbWc='

# 8) zero length with token: 201, data/zero.img
curl --http1.1 -s --path-as-is -o /dev/null -w '%{http_code}\n' -X POST http://127.0.0.1:18082/transfer \
  -H 'Tus-Resumable: 1.0.0' -H 'Authorization: Bearer review-token' -H 'Upload-Length: 0' \
  -H 'Upload-Metadata: filename emVyby5pbWc='

# 9) weird Tus-Resumable with token: 412
curl --http1.1 -s --path-as-is -o /dev/null -w '%{http_code}\n' -X POST http://127.0.0.1:18082/transfer \
  -H 'Tus-Resumable: 1.0.0, 0.2.2' -H 'Authorization: Bearer review-token' -H 'Upload-Length: 5' \
  -H 'Upload-Metadata: filename Ym9vdC5pbWc='
```

| # | 请求 | 结果 |
| --- | --- | --- |
| 1 | 规范 `POST /transfer`，无 Authorization | **401**（对照） |
| 2 | `POST //transfer`，无令牌 | **201 Created**，`location: /transfer/4d91bced-…`（另一次 `///transfer` 也 201） |
| 3 | `POST /./transfer`，无令牌 | **201** |
| 4 | `POST /foo/../transfer`，无令牌 | **201** |
| 5 | 规范 `evil.exe`，带令牌 | **400** |
| 6 | `//transfer` + `evil.exe`，无令牌 | **201** |
| 7 | `//transfer` + `CON.tar.img`，无令牌 | **201** |
| 8 | `Upload-Length: 0`，带令牌 | **201**；`data/zero.img` 大小 0 |
| 9 | `Tus-Resumable: 1.0.0, 0.2.2`，带令牌 | **412** |
| 10 | F-01 创建的 sidecar | 只有 `filename`，**无 `subject`** |
| 11 | 零长度规范创建的 sidecar | 有 `subject: manual-trial`，`size:0, offset:0, storage.path: .../data/zero.img` |

复现结论：F-01 不是理论路径，`//`/`./`/`../` 都能在无令牌下创建；绕过 guard 后名称白名单和预算也不执行；F-01 创建的记录没有 subject。已完成上传的 DELETE 被上游 403 挡住（`disable_termination_for_finished_uploads`），但读取/下载和在途删除不受影响。

## 附录 B：未覆盖与未验证

- 未执行 `cargo test -p duthub-transfer`；92 个测试的通过状态是 ask 基线，静态计数本轮复核为 92（未执行）。
- 未做 30 GiB 双机上传、带宽、断电压测；F-09 的持久化危害是代码/上游源码推演。
- 未构造并发 create 超卖（F-06）、同目录多实例（F-08）、孤儿清扫（F-07）、junction/绝对路径（F-03）的运行时用例。
- 未在多个 Windows 版本验证 `CON.tar.img` 的设备名后果；F-12 只确证规则与自述不符。
- 未抓取真实 460 响应确认 F-16 的 artifact 头；未抓取线上 `x-duthub-artifact` 形态（F-13）。
- 未双 token 实测「F-01 创建的无 subject 上传被另一 token 操作」；该结论来自 sidecar 元数据 + `authorize.rs:149-159` 代码。
- `duthub-transfer` 未接入产品，除示例接收端外无监听；F-01 的复现只在示例接收端，不能据此声称分发二进制存在现网漏洞。
- 未系统枚举 MCP/设备的其它写入面（`media_streams` 的 capture、`skill_export`、ADB/HDC/烧录、串口日志、截图/录制、`DutQuota`、`dut_artifacts`、Agent 目录）；不能推断它们满足基准 1/2/3。
- 配置目录 `<exe>/.duthub` 与任意绝对 `workspaceDir` 属既有契约，需产品明确是否豁免基准 1。

## 附录 C：发现与验收基准映射

| 发现 | 基准 1 | 基准 2 | 基准 3 | 基准 4 | 基准 5 |
| --- | --- | --- | --- | --- | --- |
| F-01 非规范路径绕过 |  | ✓ 破坏 |  |  |  |
| F-02 无 DUT 维度 |  | ✓ board 省略 | ✓ | ✓ | ✓ |
| F-03 工作根/绝对路径 | ✓ |  |  | ✓ | ✓ |
| F-04 读失败放行 |  | ✓ | ✓ |  |  |
| F-05 全局配额/10 vs 30 GiB |  |  | ✓ | ✓ | ✓ |
| F-06 预算超卖 |  |  |  |  | ✓ |
| F-07 清扫孤儿 | ✓ |  |  | ✓ |  |
| F-08 MemoryLocker |  |  |  |  | ✓ |
| F-09 fsync 声明 |  |  |  |  | ✓ |
| F-10 端点生命周期 |  |  |  |  | ✓ |
| F-11 文档矛盾 |  |  |  |  | ✓ |
| F-12 多段保留名 |  | ✓ |  |  | ✓ |
| F-13 绝对路径回执 | ✓ | ✓ | ✓ |  |  |
| F-14 marker 接缝 | ✓ |  |  |  |  |
| F-15 示例默认令牌 |  | ✓ |  |  |  |
| F-16 失败仍带 artifact 头 | ✓ |  |  |  |  |
| F-17 Tus 版本门 |  | ✓ |  |  |  |
| F-18 零长度上传 |  |  | ✓ | ✓ | ✓ |
| F-19 全局哈希闸 |  |  |  |  | ✓ |

> 读表：✓ 表示该发现影响/破坏对应基准；F-02 是基准 3/4 的主因，F-03 是基准 1 的主因，F-01 是基准 2 的唯一运行时实证绕过（F-04/F-17 是代码级条件式放行）；全局基准 1/2 还受未审计写入面和既有豁免影响。
