# duthub Agent 集成实施报告

- 报告日期：2026-11-21
- 分支：`feat/agent-integration`（未配置 upstream）
- 基线 commit：`fe24fc6c8b4346cbfb33755b31f4638c98a87def`（提交「Agent 集成设计、文档与在途改动」）
- 范围：W1–W4（共享 runtime/授权 → 内嵌 goose/ACP/Host → 桌面接线 → 文档修订与全量回归收尾），
  含 W3、W4 验证失败后的修复轮
- 结论：**4 波全部通过**；最终全量门禁 `bash scripts/regression` 退出码 0
  （39 个测试二进制 / 436 passed / 0 failed / 19 ignored）；`Cargo.lock` diff 指纹与开工基线
  一致；**实现改动未提交、未 push**。

**验证边界（先读）**：

- 真实 `goose-acp 1.60.2` 二进制已在 W2 默认测试中**实际启动**（`--version`、ACP
  `initialize`/`session/new`、`mcpServers` 注入、`GOOSE_PATH_ROOT`、本地假 OpenAI 兼容模型
  回合、`session/load`）；goose 本体是真实进程，MCP 与模型端点用本地假服务；不是只 mock
  goose 的测试。
- 但**真实在线模型、真机 COM8/COM25/继电器/烧录、A2A peer、真实 CLI Agent、人工 UI
  验收**未执行；SBOM、Authenticode 签名链验证、应用内 NotSigned 展示/第三方许可页、
  Job Object、外部 goose 路径接入启动链等仍未完成，见第 5 节。
- 不能把本报告读成「已全部实测」；也不能读成「未实连真实 goose」——真实 goose 进程已启动，
  未验证的是真实模型与真实外部系统。

---

## 1. 基线、环境与工作区

| 项 | 值 | 依据 |
|---|---|---|
| 分支 | `feat/agent-integration` | 任务给定；`git for-each-ref` |
| 基线 commit | `fe24fc6c8b4346cbfb33755b31f4638c98a87def` | `git rev-parse HEAD`（本会话实测） |
| upstream | 未配置 | `git for-each-ref --format='%(refname:short) %(upstream:short)'` 只输出分支名 |
| `Cargo.lock` diff 指纹 | `40d9ef00e940c92f1521275f154166a87d2e3c380d9fd0044000298a60cec7fa` | `git diff Cargo.lock` 管道给 `sha256sum`（本会话实测，与基线一致） |
| 隔离 target | `C:/Users/luo-j/duthub-impl-target` | 工作流记录；未写共享 `target/` |
| C: 盘空间 | 最终门禁时可用 66G；W1 记录 79G；W2 记录约 72G | 各波 `df -h /c` |
| 工作区 | 38 项（26 个 tracked 修改 + 12 个未跟踪） | `git status --porcelain` 计数（本会话实测） |

**外部在途路径（不属于各波 owns；未提交、未隔离）**：

- `AGENTS.md`（tracked 修改）
- `Cargo.lock`（tracked 修改；diff 仍只有既有的 `+ "duthub-control"` 一行，指纹与基线一致）
- `crates/duthub-device/Cargo.toml`（tracked 修改）
- `crates/duthub-device/tests/com8_reset_boot_live.rs`（外部未跟踪测试）
- `crates/duthub-device/tests/com8_io_latency_live.rs`（外部未跟踪测试，本轮未触碰）

**另，工作区中的 `docs/*` 有既有修改**：W4 修复轮按任务修订了 6 个 docs 文件
（`docs/decisions/ADR-3-GOOSE-BUNDLE.md`、`docs/decisions/ADR_SIGNOFF.md`、
`docs/reviews/AGENT_IMPLEMENTATION_REPORT.md`、`docs/SECURITY.md`、
`docs/reviews/AGENT_INTEGRATION_BEST_PRACTICE.md`、`43c58dc:docs/GOOSE_AGENT_FEASIBILITY.md`），
其中 ADR-3 的授权见下。

**关于 ADR-3 的修改授权**：`docs/decisions/ADR-3-GOOSE-BUNDLE.md` 不在 W4 修复轮的
owns 清单内；修改依据是工作流所有者上一轮的明确授权（问题 `dwfq-fd5da535-1`），
仓库内没有该授权的独立留痕，验证员按工作流材料接受。此处如实记录，不把它写成常规 owns 内改动。

**外部 com8 测试的格式时间线**：

- 更早一轮的「格式门禁修复」波次对 `crates/duthub-device/tests/com8_reset_boot_live.rs`
  做过 rustfmt 修正，**仅格式、无断言改动**。
- W1–W4 以及本报告撰写轮**未再修改、未再格式化**该文件；`com8_io_latency_live.rs` 同样未触碰。
- 本会话实测 `cargo fmt --all --check` 退出码 0（见第 8 节）。

---

## 2. 实施波次与结果

**每波是否通过：W1 true / W2 true / W3 true / W4 true（`[true, true, true, true]`）。**
W3、W4 的「通过」指验证发现的问题在对应修复轮关闭后的最终状态：

| 波次 | 主题 | 最终状态 | 说明 |
|---|---|---|---|
| W1 | 认证运行时叠加层、MCP 会话监听器与既有回归加固 | 通过 | 波内为 scoped 测试/clippy；未见失败项 |
| W2 | 内嵌 goose-acp 资产、ACP 客户端、每 DUT Host | 通过 | 默认测试真实启动 goose；未跑 live 模型 |
| W3 | 桌面接线与最小可用 UI | 通过 | 初次验证发现 GoosePath 清空、transcript 贴底、文案语义问题，修复后通过 |
| W4 | 文档修订、ADR-3 重签与全量回归收尾 | 通过 | 验证员指出两处「未实现写成已完成」与 §6.3 矛盾，修复后通过 |

> 以下波内验证数字与命令结果来自各波次的工作流记录。本报告撰写会话没有逐条
> 复跑这些 scoped 命令；最终 workspace 门禁见第 3 节，本会话实际执行的检查见第 8 节。

### 2.1 W1：认证运行时叠加层、MCP 会话监听器与回归加固

**实现**：

1. **core runtime overlay**（`crates/duthub-core/src/agent_access.rs`）：
   - 新增 `RuntimeListener` 与独立的 runtime overlay；持久设置与每会话监听器分表。
   - `set_runtime_listener`（`:362`）有 4 条拒绝规则：空 listener id；空凭据 id 或 token；
     `credential.listenerId != listener.id`；listener id 与持久监听器或已注册 runtime
     监听器冲突。第 4 条用于阻止临时 token 把主监听器切进 internal 认证路径。
   - `remove_runtime_listener`（`:405`）幂等；删除后 token 立即失效。
   - `is_runtime_listener`（`:414`）只在 id 已注册且**不是**持久监听器时返回 true。
   - `authenticate_internal`（`:437`）**只查 overlay**：空 listener id、空 token、未注册、
     跨监听器 token、停用凭据一律 `Unauthorized`；不回退 `LocalAdmin`，不读持久 auth 总开关。
   - `grants_for`（`:468`）合并持久表与 overlay：同一凭据 id 的 grants 按 DUT 合并动作并集；
     任一命中凭据停用或完全未知 → `None`（fail closed）。
   - `authenticate_http`（`:529`）仍只查持久表；`update`（`:329`）重建持久表但保留 overlay；
     `settings()`/`enabled()`（`:335`、`:341`）只看持久表，会话 token 永不写入
     `settings.json`。
   - 同 id 的持久监听器在 `update` 后赢回 HTTP 认证路径：`is_runtime_listener` 返回 false，
     `AuthSvc` 走 `authenticate_http`；此时临时 token 打不开主监听器（测试
     `persistent_listener_added_after_overlay_wins_the_auth_path`）。
   - `authorize`（`:577`）与 `lease_credential_enabled`（`:503`）合并两表。
   - `AgentCredential` 手写 `Debug` 给 token 打码（`:173`）；`AuthError::InvalidListener`
     （`:228`）只表示内部注册参数错误，HTTP 层不会产生该变体。
2. **store 安全路径 API**（`crates/duthub-core/src/store.rs`）：
   - `agent_dut_key`（`:150`）：空/全空白 → `None`；非空 → UTF-8 字节十六进制稳定键。
   - `agent_dut_dir`（`:165`）：`<work_dir>/agent/dut/<key>`。
   - `ensure_agent_dir`（`:135`）：逐级 cap-std 创建/校验，拒绝 junction/symlink 与非法路径段。
   - `plain_path`（`:173`）：去掉 Windows verbatim 前缀；`sha256_hex`（`:185`）/
     `sha256_file`（`:193`）：字节与流式文件哈希。
3. **process**（`crates/duthub-core/src/process.rs:192`）：`kill_process_tree` 公开；
   Windows 用 `taskkill /PID <pid> /T /F`（argv-only，非 shell），非 Windows 走
   `child.kill()`，两者都保留 `wait()` 收尸。
4. **MCP transport**（`crates/duthub-mcp/src/transport.rs`）：
   - `ListenerHandle`（`:213`）持有 `endpoint`、私有 shutdown sender 与 join handle；
     `shutdown_and_join_with_timeout`（`:223`）先发信号再限时 join；`Drop` 至少发 shutdown。
   - `spawn_http_listener_on`（`:251`）接受**已绑定**的 `TcpListener`；`spawn_http_listener_loopback`
     （`:298`）绑定 `127.0.0.1:0`。
   - `serve_http_listener_blocking`（`:179`）签名不变，内部与带句柄版本共用同一
     `serve_listener`，保证路由/认证/日志形状一致。
5. **AuthSvc**（`crates/duthub-mcp/src/auth_http.rs:107-110`）：按 `is_runtime_listener`
   选择 `authenticate_internal` 或 `authenticate_http`。
6. **server**（`crates/duthub-mcp/src/server.rs`）：
   - `authorize_any_read`（`:405`）改走 `grants_for`；DUT 专用 listener 先按绑定 DUT 授权。
   - `skill_export`（`:1281`）首行调用 `authorize_any_read`；stdio `LocalAdmin` 不受影响。
   - `call_tool`（`:1419`，审计在 `:1453`）外层记录 `tool.call` 审计：tool、dutId、credential、
     耗时、`Ok|Denied|Error`、仅长度/条数摘要（数组项数、字符串字符数、`content_bytes`），
     不记正文、token 或 API key。

**新增/修改测试**：

- `crates/duthub-core/tests/agent_access_overlay.rs`（新，10 个测试）
- `crates/duthub-mcp/tests/listener_lifecycle.rs`（新，4 个测试）
- `crates/duthub-mcp/tests/dut_binding.rs`（新，1 个测试）
- `crates/duthub-mcp/tests/security_canary.rs`（新，4 个测试）
- `crates/duthub-mcp/tests/http_auth_e2e.rs`（追加用例；既有断言未删改）
- 关键用例：`internal_auth_uses_only_overlay_when_persistent_auth_is_off`、
  `http_auth_never_accepts_runtime_tokens_and_internal_never_accepts_persistent_tokens`、
  `merged_grants_cover_persistent_and_overlay_and_deny_disabled`、
  `persistent_listener_added_after_overlay_wins_the_auth_path`、
  `runtime_listener_registration_rejects_four_invalid_shapes`、
  `remove_runtime_listener_is_idempotent_and_revokes_token`、`credential_debug_masks_token`、
  `session_token_is_never_written_to_settings_json`、`lease_credential_enabled_merges_overlay`

**波内验证记录**：

- `df -h /c`：可用 79G（满足 ≥70G），使用 `C:/Users/luo-j/duthub-impl-target` 隔离 target。
- `CARGO_TARGET_DIR=... cargo test -p duthub-core --lib --test agent_access_overlay --test agent_access_auth --test lease_invariants --test process_tree --test roundtrip`：
  全绿；lib 81 passed/1 ignored；`agent_access_auth` 4；`agent_access_overlay` 10；
  `lease_invariants` 10；`process_tree` 9；`roundtrip` 7；0 failed。
- `CARGO_TARGET_DIR=... cargo test -p duthub-device --test lease_admission`：4 passed/0 failed。
- `CARGO_TARGET_DIR=... cargo test -p duthub-mcp --lib --test listener_lifecycle --test dut_binding --test security_canary --test http_auth_e2e --test stdio_e2e`：
  全绿；lib 15；`listener_lifecycle` 4；`dut_binding` 1；`security_canary` 4；
  `http_auth_e2e` 17；`stdio_e2e` 3；0 failed。
- `rustfmt --edition 2024 --check`（W1 涉及的 11 个文件）：EXIT=0；
  `cargo fmt --all --check`：EXIT=0。
- `CARGO_TARGET_DIR=... cargo clippy -p duthub-core -p duthub-mcp --all-targets -- -D warnings`：
  `Finished`，无告警；另一次含 `-p duthub-device` 的同命令亦通过。
- `CARGO_TARGET_DIR=... cargo check -p duthub-desktop --all-targets`：EXIT=0
  （验证 core/mcp API 变更未破坏桌面端编译）。
- `git diff Cargo.lock | sha256sum`：`40d9ef00…`，与开工基线逐字相同（diff 仍只有既有
  `+ "duthub-control"` 一行）。
- `git status --porcelain`：本波仅改动 owns 内文件；`AGENTS.md`、`Cargo.lock`、
  `crates/duthub-device/Cargo.toml`、`com8_reset_boot_live.rs`、`docs/*` 为开工前既有外部
  在途项，未触碰。

**边界与未完成（W1）**：

- 未运行 workspace 全量 `cargo test --workspace` 与
  `cargo clippy --workspace --all-targets`；波次验收为 scoped。已额外跑 desktop
  `--all-targets` check 与 core/mcp/device scoped clippy。
- `set_runtime_listener` 的 4 条拒绝规则是本波按安全目标定义的；若上游设计对这 4 条有不同
  拆分，需要对齐。计划决策中「前缀 `session-`、grants 恰好 Read、`dut_id=Some` 由
  `set_runtime_listener` 强制」的表述与实现不同：实际由 `listener::issue` 构造保证，
  `set_runtime_listener` 只做上述 4 条通用校验。
- `agent_dut_key` 采用 UTF-8 字节 hex 编码（契约只要求空/全空白 → `None`）。选择它是为了
  路径安全、稳定与无损；若后续波次期望可读目录名，需要调整调用方。
- `tool.call` 审计摘要是通用长度/条数，未实现 T18 文档中 `serial_read.last_seq`、
  `log_segment_read.nextOffset` 等工具特定字段（本波只要求长度/条数且不记正文）。
- `kill_process_tree` 公开后的回归依赖既有 `crates/duthub-core/tests/process_tree.rs`
  （该文件不在本波 owns，未新增专用用例；既有 `runner_timeout_kills_tree` 覆盖超时整树终止）。
- `model.rs` 在 owns 列表中但无必要改动，未修改。
- 未做实机或外部 Agent 验证（本波为后端独立验收，未触碰真实串口/继电器/烧录）。

### 2.2 W2：内嵌 goose-acp v1.60.2、ACP 客户端与每 DUT Host

**内嵌资产**（`apps/desktop/assets/goose/`）：

> **2026-11-25 起作废**：goose 改为「工具下载」页下载分发，该目录与下表资产已全部删除，
> 运行时按「显式外部路径 > 已下载（`<config>/tools/goose/`）> PATH」解析
> （ADR-3 顶部变更说明、AGENTS.md 坑 #109）。下表仅作为历史记录保留。

| 对象 | 字节数 | SHA-256 | 用途 |
|---|---:|---|---|
| `goose-acp.exe` | 20,541,440 | `1c1f7c9ccb5bdbb53435b0e843a3e6d2e16f9e2f6dac5dbfcf31f1497ec7e3c5` | `include_bytes!` 内嵌并执行 |
| `goose-acp-x86_64-pc-windows-msvc.zip` | 9,832,133 | `536c65404f4a4c0d27cb3dd085d6389e1d2038bb9c2a4c8ee850f62da00c2f95` | 仅来源记录，不入构建、不运行期解压 |
| `LICENSE` | 11,341 | `44459b86c2e96fdbfd8a6b5c33d30d4b04b5293fcb2ec96fe4dcc4e0f90b8962` | `include_bytes!` 内嵌 |
| `NOTICE` | 非空 | — | `include_str!` 内嵌 |
| `manifest.json` | — | — | 双哈希、LICENSE 哈希、`--version` 探测、Authenticode 记录 |

- `goose-acp.exe --version` → `goose-acp 1.60.2`（exit 0）。
- PowerShell `Get-AuthenticodeSignature` → `Status=2 NotSigned`、`SignerCertificate=null`；
  `manifest.json` 记录 `authenticode.status = "NotSigned"`、`statusCode = 2`。
- `bundle.rs`（`:20-32`）内嵌 exe 与三份声明文件；`install_embedded`（`:171`）落到
  `<work_dir>/agent/bin/goose/1.60.2/<完整 sha256>/goose-acp.exe`，同目录临时文件 +
  原子 rename；`resolve`（`:198`）每次复算完整 SHA-256，不匹配 fail closed，不执行、
  不回退 PATH；外部覆盖只认显式选择的路径与固化哈希。

**实现**：

- **ACP 客户端**（`apps/desktop/src/agent_host/acp.rs`）：自研按行 JSON-RPC over stdio；
  4 MiB 行上限（`:22`）；4096 事件队列溢出后丢最旧并置一次 `ProtocolError`（`:23`）；
  agent → client 的 `session/request_permission` 一律拒绝，`fs/*`、`terminal/*` 一律
  `MethodNotFound`（`:6-8`）；stderr 单独收尾并脱敏；`Exited` 事件；Windows
  `CREATE_NO_WINDOW`。
- **模型注入**（`apps/desktop/src/agent_host/model_env.rs`）：从 provider catalog 与 keyring
  解析绑定；`goose_env`（`:151`）只经环境变量注入 `GOOSE_PROVIDER`、`GOOSE_MODEL`、
  `OPENAI_API_KEY`、`OPENAI_BASE_URL`、`GOOSE_DISABLE_KEYRING=1`、`GOOSE_PATH_ROOT`、
  `GOOSE_MODE=auto`；`Command::env_clear()` 后按白名单重建。`goose_config_yaml`（`:230`）/
  `write_config`（`:244`）写 managed config，关闭
  `developer`/`summon`/`tom`/`extensionmanager`；token 不写入 goose 配置。
- **每 DUT sandbox**（`apps/desktop/src/agent_host/sandbox.rs:42`）：
  `<work_dir>/agent/dut/<hex(dut_id)>/` 下的 `sandbox/`、`goose/`、`transcript.jsonl`、
  `session-id`；逐级经 `ensure_agent_dir`（cap-std；junction/symlink 与非法路径段拒绝）。
- **每会话 listener**（`apps/desktop/src/agent_host/listener.rs:64`）：`127.0.0.1:0`、
  id `session-<uuid>`、`listener_dut = Some(dut)`；runtime overlay 凭据只有该 DUT 的
  `DutGrant { actions: [Read] }`；`revoke`（`:117`）先删 overlay 再丢弃句柄。
- **transcript 双上限**（`apps/desktop/src/agent_host/session.rs:16-17`）：内存 2000 条，
  超出丢最旧；文件 8 MiB，到限后停写并只补一条 System 注记。
- **HostManager**（`apps/desktop/src/agent_host/host.rs`）：`start`（`:130`）立即返回，
  解包/校验/initialize/session-new 在工作线程；`stop`（`:195`）在 UI 线程只吊销 token、
  置停止意图、cancel、关 stdin，等待/杀进程树在工作线程；10 分钟无 ACP 事件的 prompt
  watchdog（`:29`、`:592`）→ `Failed` 并停止，不自动重启；`run_worker`（`:318`）收尾时
  先关 stdin，500ms 内未退则 kill。

**真实 goose 验证（默认测试，非 `#[ignore]`）**：

- `goose_mcp_injection`：`session/new` 返回的 `extensionResults` 仅含 `duthub` 成功，
  不再出现 `developer`/`summon`/`tom`/`Extension Manager`；对照实验（无 managed config）
  四项内置扩展均出现；假 MCP server 收到 `server/discover`、
  `Authorization=Bearer dth-injection-token-1234567890`、`MCP-Protocol-Version=2026-07-28`；
  `session/load` 跨进程恢复成功。
- `goose_model_env`：provider 环境变量驱动真实 goose 请求本地假 OpenAI 兼容服务
  `POST /v1/chat/completions`，服务收到 `Authorization=Bearer sk-model-env-secret`、
  `model=fake-model`，返回流式回复 `Hello from fake model`；`stderr_tail` 不含 API key。
- `goose_path_root`：HostManager 启动就绪后，`GOOSE_PATH_ROOT` 下存在
  `config/config.yaml` 与 `data/sessions/sessions.db`；`session-id`、transcript、sandbox
  位于每 DUT 目录；`stop` 后进入终止态并回调 `on_exited`。

**波内验证记录**：

- `df -h /c` → C: 501G，可用约 72G；使用隔离 `CARGO_TARGET_DIR=C:/Users/luo-j/duthub-impl-target`。
- 下载 zip 后 `sha256sum` → `536c6540…`、9,832,133 B；解出 exe 后 `sha256sum` →
  `1c1f7c9c…`、20,541,440 B。
- `goose-acp.exe --version` → `goose-acp 1.60.2`（exit 0）；Authenticode `NotSigned`。
- `rustfmt --edition 2024 --check`（agent_host/测试文件）：RC=0。
- `CARGO_TARGET_DIR=... cargo test -p duthub-desktop --lib --test agent_host --test goose_mcp_injection --test goose_model_env --test goose_path_root`：
  RC=0；lib 15；`agent_host` 8；`goose_mcp_injection` 3；`goose_model_env` 1 + 1 ignored；
  `goose_path_root` 1。
- `cargo clippy -p duthub-desktop --all-targets -- -D warnings`：RC=0。
- `git diff Cargo.lock | sha256sum`：`40d9ef00…`，与本次会话开始时记录一致；本波未改
  `Cargo.lock`（`apps/desktop/Cargo.toml` 只新增 `[lib]` 目标，无依赖变化）。
- `git status --porcelain`：本波仅 `apps/desktop/Cargo.toml`、
  `apps/desktop/{assets/goose,src/agent_host,src/lib.rs,tests}`、`docs/decisions/ADR-3` 为
  未提交改动；外部在途文件未触碰。

**边界与未完成（W2）**：

- 未运行 `DUTHUB_GOOSE_LIVE=1` 的真实在线模型调用（需 owner 的 key，按任务不计入门禁）；
  对应 `#[ignore] live_model_call` 在缺环境变量时会明确失败，不会假装通过。
- 10 分钟 prompt watchdog 已实现并接入工作线程，但未做端到端长等待验证（无 10 分钟测试夹具）。
- `goose_path_root` 只断言 `GOOSE_PATH_ROOT` 下生成 `config/config.yaml`/
  `data/sessions/sessions.db` 与每 DUT 文件；**未断言** `%APPDATA%\Block\goose` 在测试前后
  文件集合/mtime 不变。计划决策要求的该对照项未覆盖，「duthub 不落盘」口径仍需补充验证。
- goose 第三方 crate 的许可清单/SBOM 仍未生成，按 ADR-3 §6 属发布门禁，不在本波范围。
- 本波只交付可导入的 `agent_host` 库核心；桌面 UI 接线在后续波次完成（W2 当时记录为 W4，
  实际 W3 交付 Agent 面板，W4 只做文档纠偏）。外部 goose 路径接入启动链、Authenticode
  应用内展示仍未完成（见第 5 节）。
- 未运行 `cargo fmt --all --check`、`cargo test --workspace`、`bash scripts/regression`、
  `--self-test`（本波只按验收命令运行包级 fmt/clippy/test；工作区外部在途文件不属本工作流，
  未格式化/修改）。
- 未提交 git、未 push，改动全部留作未提交评审。

### 2.3 W3：桌面接线与最小可用 UI（含验证失败修复轮）

**初始交付（修复轮前已存在，保持不变）**：

- 连接管理每 DUT「Agent 会话」面板（`apps/desktop/src/pages.rs:1237`）：状态、端口、
  transcript、启动/停止/取消、prompt 输入与发送；每 DUT 独立草稿。
- Agent 页 goose 运行时面板（`pages.rs:1506`）：内嵌版本/SHA-256、供应商目录、外部
  goose 路径记录与校验；三行边界徽标（工具面 13 个 Plan 工具、本机工具未隔离、
  协议级 DUT 绑定不是安全边界）。
- 每 DUT 草稿与生命周期钩子：`Workbench` 的 `agent_prompts`/`agent_transcript_scroll`/
  `agent_transcript_seen`（`workspace.rs:866` 起）按 DUT 隔离；关 tab、删 DUT、退出
  Workbench 时停止会话并关闭 MCP 监听器。
- `apps/desktop/src/main.rs:178` 补 `.with_gui_session_lifetime()`（F10），GUI 会话租约
  不被 TTL 提前释放。
- `apps/desktop/src/mcp_runtime.rs`：`spawn_mcp_runtime` 返回并保存
  `McpRuntimeHandles { main, extras }`（`:483`），`Workbench::drop` 先停 Agent 再
  `mcp_runtime.shutdown()`。

**验证失败修复轮**：

1. **GoosePath 清空语义**（`apps/desktop/src/workspace.rs`）：
   - 新增 `goose_path_field_value`（`:153`）：空串草稿是有效草稿，不被已记录路径回填。
   - `input_value(In::GoosePath)`（`:2962`）改用它；`input_set`（`:3159`）对空值也保留草稿。
   - `save_goose_external`（`:4965`）在输入为空时明确恢复内嵌并清除已记录路径，不再回填/
     重新校验旧路径。
   - 新增单测 `workspace::goose_path_tests::empty_draft_wins_over_recorded_path_so_clearing_sticks`
     （`:158`）。
   - 两语言占位符/按钮/成功文案同步为「清空后保存即恢复内嵌」「校验并记录（仅本会话）」
     「仅本会话…尚未接入启动链路」，并删除不可达的 `goosePathEmpty`；两文件键集保持 parity。
2. **Transcript 贴底**（`apps/desktop/src/pages.rs:1349`）：在 `scroll_to_bottom` 前读取
   `UniformListScrollHandle` 的 `base_handle.offset().y`/`max_offset().y`，按 AGENTS #49 的
   65px near-bottom 语义只在贴近底部时跟随；用户上滚阅读时新条目不再拽回底部。
3. **文案/语义纠正**：
   - `agentRuntimeCandidate`（`locales/zh-CN.json:42`，en 同步）改为「首个启用项：
     {provider} · {model}（密钥在启动时校验）」。
   - `gooseModelSaveHint`（`:45`）说明 `save_goose_model` 会写 preferred（legacy
     `agent.provider`）并同步模型名；`pages.rs` 渲染注释同步。

**波内验证记录**：

- `rustfmt --edition 2024 --check`（W3 涉及的 8 个文件）：`RUSTFMT_RC=0`。
- `CARGO_TARGET_DIR=... cargo test -p duthub-desktop --lib --test lease_gui --test plan_badges`：
  lib 15 passed / `lease_gui` 2 passed / `plan_badges` 4 passed，0 failed。
- `CARGO_TARGET_DIR=... cargo test -p duthub-desktop --bins`：74 passed / 0 failed / 9 ignored；
  含新增 `workspace::goose_path_tests::empty_draft_wins_over_recorded_path_so_clearing_sticks`
  与 `i18n::tests::shipped_catalogs_cover_the_same_keys`。
- `CARGO_TARGET_DIR=... cargo clippy -p duthub-desktop --all-targets -- -D warnings`：通过
  （无 warning）。
- `CARGO_TARGET_DIR=... cargo run -p duthub-desktop -- --self-test`：退出码 0，输出
  `SELF-TEST OK: 3 frames · 0 core lines · duthub-desktop v0.1.1 (gpui-kit)`；退出后
  `netstat` 无 `:8797`，MCP 端口已释放。帧数随渲染时序波动（本轮多次为 2/3 frames），
  不写死。
- `git diff Cargo.lock | sha256sum`：`40d9ef00…`；`Cargo.lock` mtime `2026-11-21 09:02:00`
  早于本波编辑（`pages.rs` 12:01），本波未写。会话开始时未记录基线哈希，故该波只能证明
  「未改动」，不能逐字对比基线；最终门禁记录了与基线一致的指纹。
- 手工 UI 验收未执行：无真实 DUT、模型 API Key 与交互式 GUI；`--self-test` 不覆盖启动
  会话路径。
- 真实 goose 会话 e2e 未执行：未启动 goose-acp 交互式会话、未调用真实模型；流式对话仅由
  `agent_host` 单测与源码契约覆盖。

**边界与未完成（W3）**：

- **F28 外部 goose 路径仍未真正接入启动链**：`HostManager` 没有 `set_goose_override`，
  启动固定 `bundle::install_embedded`（`host.rs:369`）；`AgentSettings` 没有 runtime 字段；
  当前只校验 SHA-256 并在本会话内存记录，重启丢失。UI/按钮/成功提示已明确「仅本会话」
  「尚未接入启动链路」，不再声称生效。
- `save_goose_model` 仍只写 legacy `settings.agent.provider/slow`；goose 实际绑定由
  `model_env::resolve_binding` 按 preferred 优先的 provider catalog + keyring 解析
  （提示文案已按此纠正）。
- 手工 UI 与真实 goose 会话 e2e 未执行（环境无 DUT/模型凭据/GUI 交互）；启动状态机、
  流式回复、切 DUT、关 tab/删 DUT/退出的端口与进程释放仍待人工验收。
- near-bottom 修复与 GoosePath 清空修复的回归测试分别是源码/逻辑层与 bin 单测
  （helper 层）；`pages.rs` 的滚动跟随、实际点击/渲染、切 DUT 草稿不串仍未自动化
  （UI 不在 lib target）。
- **接口草案偏差（已披露）**：`mcp_runtime` 为 `McpRuntimeHandles { main, extras }`
  （`mcp_runtime.rs:483`）而非 `Vec`；`agent_host` 采用首次启动会话时惰性构造
  （`workspace.rs:4775`），`--self-test` 恒 `None`；`agent_prompts` 为
  `AgentPromptEditor` 包装（`workspace.rs:866`、`:883`）。后续 UI 波次需按实际接口适配；
  W4 修复轮仅做文档纠偏，未改这些接口。

### 2.4 W4：文档纠偏、ADR-3 重签与最终门禁

**修复内容**（修复验证员指出的两处「把未实现写成已完成」与 §6.3 自相矛盾）：

- `docs/decisions/ADR-3-GOOSE-BUNDLE.md`：
  - §6.1 #5、§6.3 #3 改为「`NotSigned` 仅记录在 `manifest.json`（release 时旁附
    `dist/licenses/goose/manifest.json`）；应用内 UI 展示与签名链验证均未完成（deferred）」。
  - §6.2 新增 deferred 第 3 项「应用内展示 Authenticode `NotSigned`/未签名提示」。
  - §6.3 结论改为「第 1、4、5 项已满足；第 2 项 SBOM 与第 3 项签名链验证/NotSigned UI
    未完成 → 本轮不对外发布 goose 资产」。
- `docs/decisions/ADR_SIGNOFF.md`：§3 与结论同步改为 UI 未实现，并把 NotSigned UI 列入
  §6.2 deferred 清单。
- `docs/reviews/AGENT_IMPLEMENTATION_REPORT.md`：把应用内 NotSigned 展示列入本报告 deferred 清单，
  并同步结论、风险与未覆盖清单。
- `docs/SECURITY.md`、`docs/reviews/AGENT_INTEGRATION_BEST_PRACTICE.md`、
  `43c58dc:docs/GOOSE_AGENT_FEASIBILITY.md`：Authenticode 状态同步为「已核实 NotSigned，UI 展示
  deferred」。

**证据**：

- `apps/desktop/src/pages.rs:1574-1586` 只渲染版本与 SHA-256，没有 NotSigned/未签名文案。
- `grep -rniE "notsigned|未签名|authenticode" apps/desktop --include=*.rs --include=*.json`
  仅命中 `assets/goose/manifest.json`、`bundle.rs` 的字段校验、
  `tests/agent_host.rs:219`；locales/UI 无对应文案。
- ADR-3 属所有者在上一轮明确授权修改（问题 `dwfq-fd5da535-1`）；本次修复轮 owns 清单再次
  漏列，以授权与 mustFix 为准。

**波内验证记录**：

- `CARGO_TARGET_DIR=... cargo clippy --workspace --all-targets -- -D warnings`：EXIT=0
  （`Finished dev profile ... in 1.61s`）。
- `CARGO_TARGET_DIR=... cargo test --workspace --quiet`：EXIT=0；39 个测试二进制，
  436 passed / 0 failed / 19 ignored。
- `CARGO_TARGET_DIR=... cargo run -p duthub-desktop -- --self-test`：EXIT=0；
  `SELF-TEST OK: 2 frames · 0 core lines · duthub-desktop v0.1.1 (gpui-kit)`。
- `CARGO_TARGET_DIR=... bash scripts/regression`：EXIT=0；四步全过，末尾
  `REGRESSION OK`（39 binaries / 436 passed / 0 failed / 19 ignored）。
- `cargo fmt --all --check`：EXIT=0。
- `git diff Cargo.lock | sha256sum`：`40d9ef00…`，与 baseline 一致。
- `git status --short`：本次修复仅涉及 6 个 docs 文件；未触碰 4 个外部在途路径，
  未提交、未 push。

**边界与未完成（W4）**：

- 应用内「第三方许可」页与 NotSigned/「未签名」UI 展示仍未实现（UI 文件不在本波 owns）；
  已明确列入 ADR-3 §6.2 deferred 与本报告 deferred 清单，不再表述为已完成。
- ADR-3 §6.3 仍为流程约束而非机器门禁：`release.yml` 未在 CI 层阻断 tag 发布
  （未新增仓库变量审批门）；SBOM、Authenticode 链验证、UI 展示未完成前只能依靠流程拒绝
  对外发布。
- `release.yml` 的 YAML 语法未用 actionlint/ruby 等独立解析器校验（环境无该工具）；其
  PowerShell 资产断言逻辑此前已用 `pwsh -NoProfile` 原样执行并通过（`asset ok` ×3 +
  `VERIFY OK`），本轮未在 GitHub Actions runner 实跑。
- 19 个 `#[ignore]` 测试未运行（真实在线模型、真机 COM8/COM25、继电器、FFmpeg、
  `DUTHUB_TEST_*` 外部设备）；真实 A2A peer 与真实 CLI Agent 未验证。
- 外部 goose 路径仅 UI 记录/校验，未接入启动链；Job Object 未实现，停止只 kill 直接
  子进程；SBOM 与 Authenticode 签名链未做。
- `git diff --check` 对既有 CRLF 文件（`release.yml`/TASKS/BEST_PRACTICE）新增行报 trailing
  whitespace（CR 被视为行尾空白）；已检查无真实行尾空格，未改行尾风格以避免全文件重排。

---

## 3. 全量门禁（最终收尾记录）

最终门禁按任务要求**只跑一次** `scripts/regression`，未重复子检查。命令与结果如下
（工作流收尾材料记录；本报告撰写会话未复跑该命令，见第 8 节）：

| # | 命令 | 退出码 | 关键输出 |
|---|---|---|---|
| 1 | `cd D:\Users\luo-j\Desktop\serialmcp\duthub && PATH="$HOME/.cargo/bin:$PATH" CARGO_TARGET_DIR=C:/Users/luo-j/duthub-impl-target bash scripts/regression` | 0 | 耗时约 43s（12:35:37→12:36:20）；末行 `REGRESSION OK`；按任务要求只跑一次 |
| 2 | 上述脚本第 1/4 步 `cargo fmt --all --check` | 0 | 无 Diff；`set -euo pipefail` 下继续进入 2/4 |
| 3 | 第 2/4 步 `cargo clippy --workspace --all-targets -- -D warnings` | 0 | `Finished dev profile [unoptimized + debuginfo] target(s) in 1.68s`，无 warning |
| 4 | 第 3/4 步 `cargo test --workspace --quiet` | 0 | 39 个 test suite 汇总 436 passed / 0 failed / 19 ignored；日志无 `FAILED`/`panicked` |
| 5 | 第 4/4 步 `cargo run -q -p duthub-desktop -- --self-test` | 0 | `SELF-TEST OK: 2 frames · 0 core lines · duthub-desktop v0.1.1 (gpui-kit)` |
| 6 | 环境预检 `df -h` / `du -sh C:/Users/luo-j/duthub-impl-target` | — | C: 可用 66G；隔离 target 18G（warm，未使用共享 target/） |
| 7 | 运行后 `git status --porcelain` | — | 38 项（26 M + 12 ??），与运行前一致，无新增工作树污染；该次运行期间未编辑任何文件 |

- **failures：无；missing：无。**
- 19 个 ignored 覆盖：真实在线模型（`DUTHUB_GOOSE_LIVE`）、真机 COM8/COM25、
  `DUTHUB_TEST_SERIAL_PORT`/`POSIX_SHELL`/`CAPTURE_VIDEO`/`SHELL_BIN`、FFmpeg 采集、
  继电器真实往返与 keyring 等外部依赖；本轮未运行。
- `scripts/regression` 的实际四步见 `scripts/regression:1-20`：fmt → clippy → test →
  desktop self-test；因此第 1 条命令已包含 fmt/clippy/test/self-test。

**本报告撰写会话实际执行的检查**（与上表分开）：

- `git rev-parse HEAD` → `fe24fc6c8b4346cbfb33755b31f4638c98a87def`
- `git status --porcelain | wc -l` → 38
- `git diff Cargo.lock | sha256sum` → `40d9ef00e940c92f1521275f154166a87d2e3c380d9fd0044000298a60cec7fa`
- `cargo fmt --all --check` → `FMT_EXIT=0`
- `git for-each-ref --format='%(refname:short) %(upstream:short)' refs/heads/feat/agent-integration`
  → 仅输出 `feat/agent-integration`（无 upstream）

---

## 4. 计划决策与实现口径

1. **ACP 客户端自研薄 client**：不引入 `agent-client-protocol` crate。`crates/duthub-agent`
   仍是空壳（`crates/duthub-agent/Cargo.toml:7-8` 无依赖），`Cargo.lock` 冻结（diff 只有既有
   `+ "duthub-control"` 一行）；wire 类型用 `serde_json::Value`，实现放
   `apps/desktop/src/agent_host/`，并新增 desktop `[lib]` target（只动 manifest 的 `[lib]`
   段，不改依赖）。
2. **goose 资产**：内嵌**解压后的 exe**（20,541,440 B，SHA-256 `1c1f7c9c…`），路径
   `<work_dir>/agent/bin/goose/1.60.2/<完整 sha256>/goose-acp.exe`（完整哈希，不用 sha8）；
   `bundle::resolve` 每次启动对已存在文件复算完整 SHA-256，不匹配用内嵌字节原子重写
   （tmp + sync_all + rename），重写失败（如在运行被锁）→ fail closed 并给可操作错误；
   绝不「文件存在就跳过校验」。manifest 字段名用 `executable`，保留 zip/exe 双哈希作来源
   记录，`authenticode: "NotSigned"` 静态记录。外部覆盖只认原生选择器显式选择，固化
   `goosePath` + `goosePathSha256`，每次启动复算；不自动下载/更新。
3. **模型注入**：key 走既有 keyring 服务 `duthub` 的
   `model/<credential id>` 条目；`model_provider_credential_id`（`crates/duthub-core/src/llm.rs:94`）
   生成 `provider/<id>/<sha256(provider.label + "\n" + base_url)>`。只经环境变量注入
   `GOOSE_PROVIDER`、`GOOSE_MODEL`、`OPENAI_API_KEY`、`OPENAI_BASE_URL`、
   `GOOSE_DISABLE_KEYRING=1`、`GOOSE_PATH_ROOT`、`GOOSE_MODE=auto`；
   `Command::env_clear()` + 白名单重建。必须用一个进程内 fake OpenAI 端点测试证明这些变量
   真的被 goose 消费（记录 Authorization 头与请求体 model，断言
   `POST /v1/chat/completions`），不靠「社区已实测」的口头引用。产品口径本轮只支持
   OpenAI 兼容协议；Anthropic/Gemini 在 UI 禁用并给原因（`goose_env` 已保留映射，未纳入
   本轮验收）。旧 `settings.agent.provider/fast/slow` 在 UI 上明确标注「goose 不读取
   （旧演示用）」。
4. **goose↔DUT 1:1 与隔离**：每 DUT 至多一个进程 + 一个 session；计划口径的 cwd 为
   `<work_dir>/agent/duts/<dut_id 的 sha256 hex>/sandbox`，实际实现落为
   `<work_dir>/agent/dut/<key>/sandbox`，其中 `key` 是 `store::agent_dut_key` 的 UTF-8
   字节 hex（见 W1），不是 SHA-256。`GOOSE_PATH_ROOT` 为同目录下 `goose`，transcript 与
   session-id 同目录。`GOOSE_PATH_ROOT` 已由真实 goose 测试证明生效；若未来不成立则改用
   `APPDATA`/`HOME` 重定向或 fail closed，不写「已隔离」。goose 自身可能把 MCP Bearer
   头写进其 `sessions.db`，所以口径写「duthub 不落盘」，不写「token 从不落盘」。
5. **DUT 只读凭据**：每会话 listener 由 `listener::issue` 构造为 loopback
   `127.0.0.1:0`、id `session-<uuid>`、`listener_dut = Some(dut)`，凭据 grants 恰好该
   DUT 的 `Read`；`authenticate_internal` 必须带非空 token，永不回退 `LocalAdmin`；
   `set_runtime_listener` 只做 W1 的 4 条通用拒绝（空 id、空凭据 id/token、凭据
   `listener_id` 与 listener 不一致、id 与持久/runtime 监听器冲突），**不单独校验**
   前缀或 grants——这两项由 `listener::issue` 的构造保证；`grants_for` 对停用凭据返回
   `None`。停止顺序中「吊销 token」只在内存（RwLock 写），可在 UI 线程执行；
   shutdown/join/等待/杀树全部在工作线程完成。
6. **进程生命周期**：`kill_process_tree(&mut std::process::Child)` 保留 `Child` 与
   `wait()` 语义（`process.rs:192`），非 Windows 走 `child.kill()`；`AcpClient` 持有
   `Child` 并暴露 `try_wait()`/`wait_timeout(Duration)`；停止路径 = 吊销 token →
   发 `session/cancel` → 关 stdin → 工作线程 `wait_timeout(5s)` → 仍未退才
   `kill_process_tree`。不做 Job Object：duthub 硬崩溃可能留孤儿，作为已知残留写入文档，
   不宣称「同生共死」已达成。
7. **本机工具姿态**：默认写 duthub 管理的 `<GOOSE_PATH_ROOT>/config/config.yaml`，
   关闭 goose 内置 `developer`/`summon`/`tom`/`Extension Manager`；验收测试断言
   `session/new` 的 `_meta.extensionResults` 同时满足（a）含 `duthub` 且 `success: true`、
   （b）不再出现上述内置扩展。真实测试的对照实验为「无 managed config → 四项均出现」。
   UI 固定三行：「工具面：13 个 Plan 工具（只读）」「本机工具：Goose 扩展已禁用；OS 沙箱
   未启用」「协议级 DUT 绑定，不是安全边界」；`agentPlanMode` 不单独渲染。
8. **评审修订落到接口**：F1 pre-bound `TcpListener` + 保留 `serve_http_listener_blocking`
   原签名，`mcp_runtime.rs` 保存句柄；F2 `Child` 语义；F3
   `new_session -> SessionInfo { session_id, extension_results }`；F4 真实 goose 测试默认执行（不 ignore），
   只有真模型调用 `#[ignore]` + env 门控；F6 `Cargo.lock` 与 baseline 逐字比对；F7 删除
   fmt 失败预判（门禁以实测为准）；F8 ADR-3
   修订 + ADR_SIGNOFF 重签；F10 `main.rs:178` 补 `.with_gui_session_lifetime()`；
   F18 core 暴露安全的逐级子目录创建（复用 cap-std 校验）；F21 W1 补 4 个既有回归；
   F22/F23 W1 新增 `dut_binding.rs`/`security_canary.rs` 与 `tool.call` 审计；F25 token/
   API key 脱敏（`AgentCredential` 手写 `Debug`；`GooseBinding`/stderr 亦脱敏，
   `goose_binding_and_env_do_not_expose_key_in_debug` 覆盖）；F29 Workbench 字段
   `pub(crate)` + `--self-test` 有使用点；F30 用实测 79G 可用，复用已有
   `C:/Users/luo-j/duthub-impl-target`（W1 时约 8.6G）。
9. **工作路径与 Plan**：只允许 `ensure_portable_work_dir`（`store.rs:77`）；Agent 新代码
   禁止 `resolve_work_dir_in`（`store.rs:217`）；DUT 子目录逐级用 core 的安全创建
   （拒绝 junction/symlink/`..`），不裸 `create_dir_all`。Plan 只承诺 13 个工具
   （`server.rs:1299` 的 `plan_tool_allowed`/`PLAN_ROUTER:1407`/`call_tool` 二次拒绝
   `:1434`），DUT 绑定再收窄；不做 OS 隔离，不宣称「Plan 只读」。T24 的工作根统一与
   改既有单测仍不做（AGENTS 红线 + ADR-WORKDIR §2.5），在第 5 节标 deferred。
10. **transcript 与 Host 生命周期**：内存 2000 条/文件 8 MiB 双上限；`start` 立即返回，
    `stop` 非阻塞；10 分钟无 ACP 事件 watchdog → `Failed` + 停止；本轮不自动重启，
    恢复状态机 deferred。

---

## 5. 延期、未覆盖与已知限制

### 5.1 明确 deferred（计划内，不按未完成判失败）

- **`crates/duthub-acp` / `crates/duthub-a2a` 新 crate 与 workspace 扩到 11 成员（T02）**：
  需改 `Cargo.lock`，等外部上下文释放。ACP client 现居 `apps/desktop/src/agent_host/`
  （新 lib target），后续迁移不改包外接口。
- **Job Object**（`KILL_ON_JOB_CLOSE` / `ActiveProcessLimit` / `ProcessMemoryLimit`）、
  `CreateProcessW` 挂起启动、`PROC_THREAD_ATTRIBUTE_JOB_LIST`、句柄白名单：~~需 windows-sys
  直接依赖~~。**已完成（2026-11-21 收口轮，见 §11.3）**：`apps/desktop/src/agent_host/job.rs`
  用 `windows 0.62.2` 实现 CREATE_SUSPENDED → Assign → Resume、KILL_ON_JOB_CLOSE、
  `ActiveProcessLimit=16`、`ProcessMemoryLimit=4 GiB`，任一步失败即终止挂起子进程（fail closed）。
- **受限令牌（只挡写）、专用账户 + NTFS ACL、AppContainer（含串口/USB 设备可访问性验证）**：
  需 owner 决定与管理员配置。
- **内嵌 9.8MB zip（省 10.7MB exe 体积，T50 原始形态）**：需 `zip`/`flate2` 直接依赖；
  当前 ADR-3 已改为内嵌 exe，若 owner 不接受体积再评估。
- **SBOM / 第三方许可清单、Authenticode 链验证（WinVerifyTrust）**：本轮只做 manifest/
  发布物旁附记录，未生成 SBOM，也未做签名链验证；缺 SBOM 属 ADR-3 §6 未满足项，需 owner
  书面豁免或有 SBOM 后才能对外发布。
- **应用内 NotSigned/「未签名」提示与应用内「第三方许可」页**：~~UI 尚未实现~~
  **已完成（2026-11-21 收口轮，见 §11.3）**：Agent 面板/会话渲染「未签名（仅 SHA-256 校验）」
  徽标（`agent_ui.rs:846` `EmbeddedNotSigned`）；设置页新增 `ConfigPage::Licenses` 许可页
  （`pages.rs:2587`），应用内可读 LICENSE/NOTICE/manifest。
- **外部 goose 路径接入启动链**：~~UI 只记录并校验，启动只用内嵌副本~~ **已完成
  （2026-11-21 收口轮，见 §11.3）**：`agents.json`（`agent_config.rs`）+ `agent_host/registry.rs`，
  Host 按 DUT `agent_id` 经 `registry::resolve` 启动（`host.rs:714`），外部 goose 版本
  ≥ 1.60.2 可进启动链。
- **会话自动重启（3 次/10 分钟 + 退避）与恢复状态机（T36）**：本轮只有 10 分钟无事件
  watchdog（Failed + 停止），不自动重启。
- **`session/load` 的完整恢复回归**（重启 resume、mcpServers 重注入、transcript 回放）：
  本轮 best-effort（有 session-id 文件就试 load，失败回退新会话），不做恢复语义承诺。
- **Anthropic/Gemini 的产品接线/验收、goose config.yaml 自定义 provider、managed config
  其余可用键**：产品口径本轮只支持 OpenAI 兼容协议；`goose_env` 有基础环境变量映射，
  但 UI 禁用且未纳入验收；managed config 只做禁用内置扩展的最小集合。
- **`agents.json`（kind=acp|cli）、CLI profile（Claude/Codex/dsh）、检测/哈希固化、
  替换降级矩阵**：本轮只有 goose 一条绑定。
- **A2A 运行时**（per-DUT listener、bind_addr、peer 认证、TLS、任务状态机）：~~完全不动~~
  **最小运行时已落地（W4a/W4b）**：`crates/duthub-a2a` 每 DUT 回环端点（仅绑 `127.0.0.1`，
  非回环一律 `NeedsProductSignoff`，`listener.rs:43-47`）；JSON-RPC 3 方法
  （SendMessage/GetTask/CancelTask，`task.rs:85`），其余方法一律 `-32601`（`task.rs:74`）；
  8 个证据工具白名单（`evidence.rs:23`）；请求体上限 256 KiB（`listener.rs:36`）。
  **仍 deferred：LAN 监听、任务持久化（任务状态仅内存，`lib.rs:8`）、peer 认证、TLS、
  bind_addr 升级**（见 §11.5）。
- **T24 工作根统一**（`resolve_work_dir_in` settings>CLI 改 CLI>settings + 改既有单测）：
  AGENTS 红线 + ADR-WORKDIR §2.5，仍待独立裁决；本轮 Agent 代码只走
  `ensure_portable_work_dir`。
- **旧演示面板清理**（`AgentState`/`AgentSession`/`RunStatus`/900ms STEP/OFFLINE DEMO）
  与右侧 Inspector（工具卡/权限/证据 seq）：本轮只加横幅与三行徽标。
- **transcript 持久化/回放、面板 2000 条以上分页、stderr 环缓冲落盘**：本轮上限已定
  （2000 条/8 MiB），超出即停写。
- **每 DUT listener 端口注册表与 `agentAccess.listeners[]`/A2A 端口冲突探测**：本轮会话
  端口由 OS 分配（`127.0.0.1:0`），持久 listener 仍按配置绑定。
- **`AGENTS.md` 经验回写**：它是外部在途路径，本轮禁改；新坑先写本报告，待外部上下文
  结束后回写。

### 5.2 未运行/未验证

- **19 个 `#[ignore]` 测试**：真实在线模型、真机 COM8/COM25、继电器往返、FFmpeg 采集、
  `DUTHUB_TEST_*` 外部设备与 keyring 等；本轮未运行。
- **真实在线模型调用**：只有 `#[ignore] live_model_call`（需 `DUTHUB_GOOSE_LIVE` 与 owner
  的 API key）；默认测试用的是真实 goose + 本地假 OpenAI 兼容服务，不是在线模型。
- **外部 goose 路径**：~~未接入启动链；HostManager 固定执行内嵌副本~~ **已接入启动链
  （2026-11-21 收口轮，见 §11.3）**；未验证的是真实外部 goose 二进制的启动与会话。
- **真实 goose 会话人工 e2e**：未启动交互式会话验证启动状态机、流式回复、切 DUT、
  关 tab/删 DUT/退出的端口与进程释放。
- **`%APPDATA%\Block\goose` 零写入对照**：未验证；`goose_path_root` 只证明
  `GOOSE_PATH_ROOT` 下生成 config/sessions，未比较用户目录测试前后的文件集合与 mtime。
- **真实 A2A peer、同网段互操作、真实 CLI Agent（Claude/Codex/dsh）**：未验证。
- **真机硬件**：未打开 COM25/COM8，未执行继电器、烧录或电源操作。
- **手工 UI 验收**：连接管理 → Agent 会话 → 启动→就绪→发一句→流式回复；切 DUT 草稿/
  会话；关 tab/删 DUT/退出端口与进程释放；均未执行（无真实 DUT、模型凭据与交互式 GUI）。
- **10 分钟 watchdog 端到端长等待**：未验证（无 10 分钟测试夹具）。
- **Windows Defender/EDR 对新增 20.5MB 内嵌 exe 的行为、冷启动与内存数据**：未验证。
- **`cargo build --release` 与 `scripts/regression.cmd`**：未运行；本轮使用 Git Bash 的
  `bash scripts/regression` 与隔离 debug target。
- **SBOM、Authenticode 签名链、发布物实际分发**：未做；ADR-3 §6.3 明确本轮不对外发布
  goose 资产。

### 5.3 已知实现残留/限制

- **~~无 Job Object~~**：**已实现（2026-11-21 收口轮，见 §11.3）**；嵌套 Job（duthub 自身
  已在父 Job 中）场景未实测，失败路径 fail closed。
- **NotSigned 二进制**：内嵌 exe 无 Authenticode 签名；manifest 与发布物记录 `NotSigned`，
  **应用内「未签名（仅 SHA-256 校验）」徽标已实现（§11.3）**；签名链验证仍未做，
  不得静默当作可信。
- **~~外部路径未接入启动链~~**：**已接入（§11.3）**；真实外部 goose 二进制未实测。
- **ADR-3 §6.3 机器门禁已建但按设计失败**：release.yml 已有 gate job（回归/SBOM/audit/
  签名/双哈希 + environment/vars 门），当前因缺签名证书与 SBOM 输入而按设计失败（§11.7）；
  在补齐前对外发布仍 blocked。
- **`agent_dut_key` 使用 hex**：路径安全、稳定、无损，但目录名不可读；若后续波次要求
  可读目录名，需要调整调用方并迁移既有目录。
- **`tool.call` 审计字段是通用长度/条数**：未实现工具特定序号/偏移字段。
- **W1–W3 门禁为 scoped**：未在这些波次跑 workspace 全量 test/clippy；最终 workspace
  门禁在 W4 收尾才跑。
- **ADR-3 owns 清单漏列**：修改依据是所有者授权（`dwfq-fd5da535-1`），仓库内无独立留痕。
- **CRLF trailing whitespace 告警**：`git diff --check` 对既有 CRLF 文件新增行报行尾空白；
  已确认无真实行尾空格，未改行尾风格以避免全文件重排。
- **工作区非空**：外部在途文件与未提交交付物共存；合流/提交前需所有者确认边界。

---

## 6. 风险与注意事项

1. **进程树收尾**：无 Job Object 时，goose 派生的孙进程可能残留；真实交付前建议补
   Job Object 或等价进程树收尾。
2. **未签名二进制**：`NotSigned` 已记录在 manifest/发布物，但应用内未提示；发布前必须
   补齐「未签名」UI 与签名链验证，否则不得对外发布。
3. **发布门禁依赖流程**：ADR-3 §6.3 未接入 CI 阻断；缺少 SBOM/签名链时只能人工拒绝发布。
4. **外部路径承诺未兑现**：UI 记录/校验不等于启动链生效；当前只执行内嵌副本。
5. **真实模型与真实硬件未验证**：本轮证明的是真实 goose 进程 + 假本地服务，不是在线
   模型；真机 COM8/COM25、继电器、烧录未执行。
6. **人工 UI 验收缺失**：面板交互、滚动跟随、切 DUT 草稿隔离、关 tab/删 DUT/退出的端口
   与进程释放仍待人工确认。
7. **W1–W3 为 scoped 门禁**：core/mcp/device/desktop 的包级测试不能替代 workspace 全量；
   最终 workspace 门禁在 W4 收尾记录。
8. **外部在途路径与 com8 格式**：`AGENTS.md`、`Cargo.lock`、device manifest、COM8 测试
   等不属于各波交付；更早一轮仅格式化 com8 测试（无断言改动），本轮未再触碰。

---

## 7. 交付状态声明

- **实现改动未提交、未 push。** 本地 HEAD 仍为
  `fe24fc6c8b4346cbfb33755b31f4638c98a87def`；分支 `feat/agent-integration` 未配置
  upstream；工作流内未执行 `git add` / `git commit` / `git push`。
- **本报告撰写轮只修改 `docs/reviews/AGENT_IMPLEMENTATION_REPORT.md`**，未改代码、测试、配置或
  其他文档。
- **`Cargo.lock` 未被本工作流改动**：`git diff Cargo.lock | sha256sum` 为
  `40d9ef00…`，与开工基线逐字一致；diff 仍只有既有的 `+ "duthub-control"` 一行。
- **未修改 4 个外部在途路径**：`AGENTS.md`、`Cargo.lock`、`crates/duthub-device/Cargo.toml`、
  `crates/duthub-device/tests/com8_reset_boot_live.rs`。更早一轮对 com8 测试做过 rustfmt
  修正（仅格式、无断言改动）；W1–W4 与本报告轮未再修改。
- **未运行真实硬件动作**：未打开 COM25/COM8，未执行继电器、烧录或电源操作。
- **未调用真实在线模型**：goose 测试使用本地假 OpenAI 兼容服务；真实模型用例
  `#[ignore]`。
- **真实 goose 二进制已启动**：`--version`、ACP 握手、`mcpServers` 注入、managed config、
  `GOOSE_PATH_ROOT`、假模型回合、`session/load` 均由默认测试覆盖；这不是「未实连真实
  goose」，也不是「已全部实测」。

---

## 8. 事实来源与验证边界

### 8.1 本报告撰写会话实际执行

| 命令 | 结果 |
|---|---|
| `git rev-parse HEAD` | `fe24fc6c8b4346cbfb33755b31f4638c98a87def` |
| `git status --porcelain` 计数 | 38 |
| `git diff Cargo.lock` 管道给 `sha256sum` | `40d9ef00e940c92f1521275f154166a87d2e3c380d9fd0044000298a60cec7fa` |
| `cargo fmt --all --check` | `FMT_EXIT=0` |
| `git for-each-ref --format='%(refname:short) %(upstream:short)' refs/heads/feat/agent-integration` | 仅 `feat/agent-integration`（无 upstream） |

### 8.2 本报告撰写会话实际读取/检索

- 任务/设计：`43c58dc:docs/AGENT_IMPLEMENTATION_TASKS.md`（含 W1–W4 supersession 与 §12.2）、
  `docs/decisions/ADR-3-GOOSE-BUNDLE.md`、`docs/decisions/ADR_SIGNOFF.md`、
  `scripts/regression`
- core：`crates/duthub-core/src/agent_access.rs`、`store.rs`、`process.rs`、`llm.rs`、
  `crates/duthub-core/tests/agent_access_overlay.rs`
- MCP：`crates/duthub-mcp/src/auth_http.rs`、`server.rs`、`transport.rs`、
  `crates/duthub-mcp/tests/{listener_lifecycle,dut_binding,security_canary,http_auth_e2e}.rs`
- desktop：`apps/desktop/src/lib.rs`、`Cargo.toml`、
  `apps/desktop/src/agent_host/{bundle,host,listener,session,acp,model_env,sandbox}.rs`、
  `apps/desktop/src/{main,mcp_runtime,workspace,pages}.rs`、
  `apps/desktop/tests/{agent_host,goose_mcp_injection,goose_model_env,goose_path_root,lease_gui,plan_badges}.rs`、
  `apps/desktop/locales/{zh-CN,en-US}.json`、`apps/desktop/assets/goose/manifest.json`

### 8.3 引用自工作流材料（本会话未复跑）

- W1/W2/W3 的 scoped `cargo test`/`cargo clippy`/`rustfmt`/`--self-test` 命令与计数；
  W4 的 workspace 级 clippy/test/self-test/regression/fmt 记录。
- 第 3 节的最终 `bash scripts/regression` 记录（EXIT=0；39 suites / 436 passed /
  0 failed / 19 ignored；约 43s；仅运行一次）。
- 真实 goose 测试断言内容（Bearer 注入、managed config 对照、假 OpenAI 回合、
  `GOOSE_PATH_ROOT`、`session/load`）。
- 资产下载/哈希/`--version`/Authenticode 记录。

### 8.4 未运行

- 本会话未运行 `cargo test --workspace`、`cargo clippy --workspace --all-targets`、
  `bash scripts/regression`、`--self-test`；这些结果以上述工作流记录为准，不冒充本会话
  实测。
- 19 个 `#[ignore]` 测试、真实在线模型、真机 COM8/COM25、继电器、烧录、FFmpeg 采集、
  真实 A2A peer、真实 CLI Agent、人工 UI 验收、10 分钟 watchdog 长等待、
  `cargo build --release`、`scripts/regression.cmd`、SBOM、Authenticode 签名链均未运行。

---

## 9. 总体结论

- W1–W4 的实现、测试与最终 workspace 门禁在记录范围内通过；`Cargo.lock` diff 指纹与
  开工基线一致。
- 这**不是**「生产就绪/全部实测」：真实在线模型、真机硬件、真实 A2A/CLI Agent、人工 UI
  验收未执行；Job Object、SBOM、Authenticode 链验证、NotSigned UI、外部 goose 路径接入
  启动链等仍 deferred（第 5 节）。
- 实现改动未提交、未 push；本报告撰写轮只修改本文件。

---

## 10. 2026-11-21 补充实施轮（子代理审阅 + 缺口补全）

本轮由 3 个只读审阅子代理（agent_host / core+MCP / UI+发布门）与 1 个社区组件选型子代理开始，随后按「复用社区高质量代码为主」补全。

### 10.1 已补全

- **运行时只读 fail-closed / 生命周期**：`extensionResults` 必须恰好 duthub success；`GOOSE_MODE=approve`；transcript 写失败注记；`try_transition` CAS、worker 终止兜底、`remove_finished` 先 join、tick 对账；UI 线程不再写 stdin（有界命令队列）；控制事件独立队列；initialize 校验 v1；HostEvents 接 AlertCenter/toast。测试：lib 26 / agent_host 8 / goose_lifecycle 4 等。
- **Job Object 硬杀**：`agent_host/job.rs`（`windows 0.62.2`）CREATE_SUSPENDED → Assign → IsProcessInJob → Resume；KILL_ON_JOB_CLOSE / ActiveProcessLimit=16 / ProcessMemoryLimit=4GiB；kill/Drop TerminateJobObject；`taskkill` 仅作回退。`goose_job_kill` 4 tests；`Cargo.lock` +2 行（新增 windows 直接依赖，保留外部 duthub-control 行）。
- **模型绑定与会话恢复**：`AgentSettings.activeProviderId/activeModelId`（camelCase + skip_serializing_if），精确选择、keyring 错误聚合、Gemini 拒绝、binding trim；`session/load` + 降级横幅。`goose_binding` 5 tests；roundtrip 7。
- **agents.json / 通用 ACP / 外部 goose**：`crates/duthub-core/src/agent_config.rs`（默认 goose、原子写、损坏备份 fail closed、不存 token）；`agent_host/registry.rs`（外部程序 expectedSha256、envKeys→keyring、CLI 明确未实现）；Host 按 DUT `agent_id` 启动；外部 goose 真正进入启动链（版本 >= 1.60.2）；UI 注册表/每 DUT 选择/原生选择器。`agents_config` 7 + `agent_registry` 7 tests。
- **发布门与许可 UI**：`bundle.rs` accessor；Agent 面板/会话「未签名（仅 SHA-256 校验）」徽标；`ConfigPage::Licenses`；根 LICENSE/NOTICE；`release.yml` gate（regression、cargo-cyclonedx 1.7、cargo-audit、Get-AuthenticodeSignature、双哈希、SHA256SUMS、environment/vars 门）；`scripts/release-gate.ps1`。测试 lib 39 / agent_host 10 / locale parity。
- **路径/环境收敛**：子进程不再继承 USERPROFILE/APPDATA/LOCALAPPDATA/HOME/HOMEDRIVE/HOMEPATH/TEMP/TMP；TEMP/TMP 指向 `<sandbox>/tmp`；`GOOSE_DISABLE_KEYRING=1`；真实 goose 无用户目录仍可启动；`%APPDATA%\Block\goose` 前后快照一致。注意：这是路径收敛，不是 OS 强制边界。

### 10.2 最终门禁

- `PATH=$HOME/.cargo/bin:$PATH CARGO_TARGET_DIR=C:/Users/luo-j/duthub-impl-target bash scripts/regression` → 退出码 0，末行 `REGRESSION OK`；fmt/clippy/test/self-test 四步均执行。
- 工作区：44 项未提交（27 tracked 修改 + 17 未跟踪，含 5 个外部在途路径与本轮交付）。
- 实现改动未提交、未 push。

### 10.3 仍未完成（如实）

- OS 级强制边界：受限令牌/专用账户 ACL/AppContainer 未实现；当前只有每 DUT cwd/GOOSE_PATH_ROOT/TEMP 收窄 + 无用户目录 env + managed config 禁用内置扩展。
- A2A 运行时未开始（ADR-2/8/9 + 产品签字仍是前置）。
- `duthub-acp`/`duthub-a2a` crate 拆分/workspace 11、端口注册表、自动重启/退避、完整 Inspector、演示面板清理未做。
- `kind=cli` 仅有 schema 与明确错误，Claude/Codex/dsh profile 未实现。
- 发布 gate 目前会按设计失败：缺少 duthub 代码签名证书、`apps/desktop/assets/goose/Cargo.lock`（SBOM/audit 输入）；未在本地安装/执行 cargo-cyclonedx/cargo-audit。
- 真实在线模型、真机 COM/继电器/烧录、人工 UI 验收未执行。
- `AGENTS.md` 仍是外部在途路径，本轮未改；新教训需外部上下文结束后回写。

---

## 11. 2026-11-21 Agent 集成收口（审阅修复、九波次汇总与最终门禁）

本节为 Agent 集成（`feat/agent-integration`）的**最终收口记录**，由收口文档轮汇总：三组审阅
发现与修复落点（§11.1）、九个实施波次的真实结果（§11.2，含未通过波次）、每波通过情况
（§11.3）、全量门禁（§11.4）、Cargo.lock 关键裁决（§11.5）、计划决策（§11.6）、延期清单
（§11.7）、新 crate 与社区依赖（§11.8）、交付状态（§11.9）。本节取代此前 W5 轮的阶段性
§11 草稿；§5 中按旧草稿编号书写的交叉引用对应关系：旧 §11.3（已完成项证据表）→ 本节
§11.1/§11.2，旧 §11.5（未完成清单）→ 本节 §11.7，旧 §11.7（release gate 逐项）→ 本节
§11.4 附注与 §11.9。

**实现改动未提交、未 push**（HEAD `fe24fc6`，工作树 62 项在途，本轮实测）。

### 11.1 本轮审阅结论与修复落点

三组只读审阅共 **53 项发现**（agent_host/桌面 UI 23、MCP 授权与审计 13、发布工程与展示 17）。
处置以波次交付与回归测试为证；「遗留」= 本轮未关闭、转入 §11.7 延期清单；「本轮实测」=
收口文档轮写作时的只读复核。汇总：**已修 37、部分修复 5、遗留 10、有意维持 1**。

#### 11.1.1 agent_host 与桌面 UI（23 项）

| # | 级 | 位置 | 问题 | 处置 |
|---|---|---|---|---|
| 1 | 高 | `agent_host/host.rs:542-575` | F1 工具面强校验只在 `ResumeOutcome::Fresh` 分支执行；Degraded/Resumed 未校验，Degraded 新 sessionId 被当「已校验」持久化 | 已修（W2a）：Fresh 与 Degraded 进入 Ready/落盘前统一 `validate_extension_results`；Resumed 经 `duthub_mcp::probe::list_tool_names` 复验，失败转 Failed 并提示新建 |
| 2 | 中 | `host.rs:472-482` | `env.extend(launch.env)` 后置，envKeys 可覆盖 `GOOSE_PATH_ROOT`/`GOOSE_MODE` 等隔离关键变量 | 已修（W2a）：RESERVED_ENV_KEYS 于 agent_config 与 host 双重拒绝；模型 env 改在 envKeys 之后合并、同名冲突报错 |
| 3 | 中 | `registry.rs:207-234` | `probe_program` 在 UI 线程执行用户选择的可执行文件，`run_binary` 继承完整环境（未 env_clear、未进 Job） | 遗留：探测仍在 UI 线程（本轮实测 workspace.rs:1631/5455/6215 三处同步调用）；keyring 解析已移工作线程（W4b，见 #18） |
| 4 | 中 | `store.rs:185-206` | `sha256_hex`/`sha256_file` 逐字节 `format!`，20MiB 内嵌 exe 每次解析哈希 2-3 次 | 已修（W2a）：十六进制查表/预分配；内容寻址内嵌 goose 文件哈希每进程一次、外部程序每次复算（bundle.rs 内存哈希未缓存，遗留） |
| 5 | 中 | `host.rs:397-406` | Drop 先 stop_all 再逐个 join；initialize/session-new 20s/30s 阻塞超时且 EOF 不唤醒 pending，关窗可卡 20-50s | 已修（W2a/W2b）：`mark_closed` 唤醒全部 pending（acp.rs:321/820/956，本轮实测）；Drop 有界 join；stop 可取消退避等待 |
| 6 | 中 | `sandbox.rs:61-67` | session-id 不区分 agent，换 Agent 后旧 goose sessionId 传给新 Agent 造成跨 Agent 误导恢复 | 已修（W2a）：`resume.json{agentId,programSha256,sessionId}` 权威判定（host.rs:794，本轮实测），不匹配按新会话并注记 |
| 7 | 中 | `host.rs:664-720` | 每个 ACP 事件在工作线程同步 `TranscriptLog::append`，慢盘拖住 4096 事件队列 drain | 已修（W2a）：新 transcript.rs 独立写线程（逐条 write_all+flush，cap/失败注记由写线程回写），事件循环只入队 |
| 8 | 中 | `acp.rs:52-73`、`agent_config.rs:343-392` | 通用 ACP envKeys 只有键名无值、不继承用户目录、无 config_dir，依赖用户配置/Node 认证的第三方 Agent 无法完成认证 | 部分（W4b）：env 值掩码编辑器 + keyring 写/删；config_dir/用户目录注入未做（deferred） |
| 9 | 中 | `host.rs:155-178` | `start_with_agent` 接受 `Some(binding)` 不要求 `is_goose`，模型 Token 可注入第三方进程 | 已修（W4b）：产品入口切 `start_entry`——非 goose 恒 `BindingSource::None`（`start_with_agent` 本身保留兼容既有测试；回归 `start_agent_session_never_passes_model_binding_to_non_goose`） |
| 10 | 中 | `workspace.rs:4956` | args 空格 `join`/`split_whitespace` 往返损坏带空格参数（配置数据损坏） | 已修（W4b）：编辑器改 JSON 数组（`args_json_roundtrip_preserves_quoted_argument`），非法 JSON 不落盘 |
| 11 | 中 | `workspace.rs:5138-5182` | 删除 Agent 不清 keyring env；删 DUT 不删 `<work_dir>/agent/dut/<hex>/` | 已修（W4a/W4b）：删除 Agent 走 `delete_agent_env_entries`（失败键写 pending-cleanup 重试）；删 DUT 走 `run_remove_dut` 五步清理序 |
| 12 | 中 | `registry.rs:237-244` | 版本门槛 `>=1.60.2` 与「恰好 1 条 duthub success」校验自相矛盾，新版本多内置扩展即 fail closed | 有意维持（决策④）：extensionResults 维持严格口径不改宽；fail closed 作为显式兼容策略写入报告，升级 goose 需同步 allowlist 并加测试 |
| 13 | 中 | `goose_mcp_injection.rs:132` | 无「真实 goose 读 DUT 证据」端到端（fake tools/list 为空、fake 模型不触发工具调用） | 未关闭（W2c 未通过）：真实 Claude 被 rmcp outputSchema 阻断（见 11.2.4）；真实 goose prompt 调 `dut_list`/`serial_read` 仍未验证 |
| 14 | 低 | `host.rs:469-475` | 启动期多个 stop_flag 提前返回不走 session-id 清理 | 已修（W2a）：显式停止（含启动期停止）删除 resume.json+session-id（initialize 前极早期时序无专门测试，遗留） |
| 15 | 低 | `workspace.rs:5312` | `agent_prompt_clear` 单 Option：跨 DUT 误清、渲染帧清掉新输入、入队失败草稿已丢 | 已修（W4c）：per-DUT `HashMap<String,(String,u64)>`，仅输入仍等于已发送文本时清空（workspace.rs:974/6039-6054，本轮实测） |
| 16 | 低 | `pages.rs:1418-1430` | transcript 2000 条截断后 item_count 恒 2000，贴底判断失效 | 已修（W4c）：贴底判定改用 `SessionView::total_entries()` 单调序号（pages.rs:1514-1539、agent_ui.rs:757，本轮实测） |
| 17 | 低 | `workspace.rs:1148-1164` | self-test 分支前无条件 `load_agents` 会写真实工作目录 | 已修（W4c）：self-test 复验 `! -f "$TMP/duthub-data/agents.json"` 与 `! -d agent` 均通过（见 11.2.8） |
| 18 | 低 | `workspace.rs:5240-5252` | `start_agent_session` 在 UI 线程同步读 keyring（`resolve_binding`） | 已修（W4b）：`start_entry` + `BindingSource::FromSettings` 在工作线程解析 keyring（provider 页探针未明确关闭，遗留） |
| 19 | 低 | `registry.rs:166` | 外部程序 signature 恒 NotSigned 被徽标当事实展示 | 已修（W4c）：外部程序徽标显示「未验证」；徽标按实际 launch agent 选择 |
| 20 | 中 | 报告 §1/§3/§7 等多文档 | Job/外部 goose 启动链/许可页/GOOSE_MODE 被写成未实现，与代码矛盾 | 已修（W5+修复轮）：报告/ADR-3/ADR_SIGNOFF/API_CONTRACT 统一；ADR-3 残留 grep=0（本轮复核） |
| 21 | 低 | `host.rs:1-12` 等 | 新增 agent_host/* 大量中文注释，违反 AGENTS「新增/改动代码注释一律英文」 | 遗留：本轮实测 host.rs 202 条注释中 74 条含中文；未在本轮波次关闭 |
| 22 | 低 | `workspace.rs:7210-7225` | `remove_dut` 在可失败 `strip_dut_mcp` 之前就 `stop_agent_session`（删除被取消但会话已停） | 已修（W4a）：`run_remove_dut` 显式五步清理序——可失败步骤先行、幂等目录删除、待清理记录 |
| 23 | 低 | `workspace.rs:4909-4926` | `set_dut_agent` 运行中即改绑定；`toggle_agent_registry` 可停用默认 goose | 已修（W4b）：运行中锁定下拉（`running_agent_selector_disabled`，agent_ui.rs:683）+ `running_launch_line` 显示实际 launch（:690）；禁止停用默认 goose 与最后一个启用条目 |

#### 11.1.2 MCP 授权、审批与审计（13 项）

| # | 级 | 位置 | 问题 | 处置 |
|---|---|---|---|---|
| 1 | 高 | `server.rs:387-396,647-763`、`agent_access.rs:168` | 通用 listener 的 `dut_files`/`dut_file_read`/`log_segments`/`log_segment_read` 零 grant 可读全部 DUT 文件与归档日志（省略 dutId 分支连 authorize_any_read 都不调用） | 已修（W1）：通用分支改 `authorize_any_read`（至少一个 Read grant）、dut_files 省略 dutId 分支同样要求；零 grant/停用凭据回归。口径裁定：保留 AGENTS #44/ADR-7 全局读语义，UI 明示 |
| 2 | 中 | `server.rs`、`auth_http.rs:97-126` | 认证通过后停用凭据/回收 grants，处理中的读请求不复核（违反「停用后立即拒绝」） | 已修（W1）：`credential_enabled` 合并持久表+overlay（agent_access.rs:540）；「认证后停用再请求 401」回归 |
| 3 | 中 | `auth_http.rs:177-195`、`server.rs:342-372` | `CurrentPrincipal` 缺 HTTP Parts 无条件回退 LocalAdmin，形成「审计记 Agent、执行 LocalAdmin」的潜在提权 | 已修（W1）：`principal_from_parts`——access=Some 且无 Parts → 401 fail closed；access=None（stdio/证据 server）→ LocalAdmin（auth_http.rs:331，本轮实测） |
| 4 | 低 | `agent_access.rs:362-402,468-499,503-510` | 跨表 credential id 冲突合并 grants；`set_runtime_listener` 不强制 dut_id/grants | 部分（W1）：新 `set_session_listener` 强制绑定 DUT + 凭据恰好一条该 DUT Read 并回写 dut_id（agent_access.rs:411）；跨表 id 合并语义按既有 overlay 测试保留，deferred 按 (表,listener,credential) 作用域改造 |
| 5 | 中 | `approval.rs:106-108,170-173`、`audit.rs:5-10` | 审批令牌明文进内存审计；权威 AuditSink 无 approval.issue/consume | 已修（W1）：令牌改 `apr-`+CSPRNG、`ApprovalFingerprint`、进程内环只存指纹、`with_audit` 写 approval.issue/consume。遗留：产品构造点仍 `ApprovalManager::new()`（workspace.rs:1396，本轮实测），接线未完成 |
| 6 | 中 | `audit.rs:76-107,231-239`、`server.rs:1424-1459` | redact 漏 `access_token`/`api-key` 等；tool/dut 字段来自客户端且不限长 | 已修（W1）：键名归一化（小写、去 `_`/`-`、后缀 token/key/secret/password）；tool/dut 截断去控制字符（64 字符，较决策草案 128 更严，server.rs:1375） |
| 7 | 中 | `auth_http.rs:127-137`、`audit.rs:221-290`、`transport.rs:344-354` | 无限流：LAN 未认证客户端可无限猜 token，每失败同步追加审计 JSONL | 已修（W1）：每监听器滑动窗口 60 次/60s → 429+Retry-After；auth.fail 聚合；/mcp 1MiB 请求体上限；审计 8MiB 上限 + 4 次轮转（到上限停止当日落盘） |
| 8 | 中 | `model.rs:91-156`、`pages.rs:2351-2369` 等 | A2A 前置 ADR-2/8/9 不存在/未签字，UI 已按 http:// 展示可复制 Card URL | 已修（W3/W4a）：新建 ADR-2/8/9（Proposed 未签字）；A2A 标 experimental、仅 127.0.0.1；无 Running listener 不生成可复制 URL |
| 9 | 低 | `pages.rs:2419-2427`、`workspace.rs:2531-2548` 等 | 「重新生成后旧 Token 立即失效」无执行路径；keyring 删除失败只 toast 留 orphan | 已修（W3/W4a）：`A2aRuntime.set_token` 即时换凭据（旧 token 立刻 401）并写回 TokenSource；删除失败入 pending-cleanup 记录并告警 |
| 10 | 低 | `workspace.rs:2886-2902`、`keys.rs:78-110` | `export_a2a_config` 不过 `export_contains_secret`，Card URL 可明文导出 | 已修（W4a）：`a2a_export_payload` 秘密扫描 fail closed |
| 11 | 低 | `agent_access.rs:362-402`、`listener.rs:60-99` | `set_runtime_listener` 不强制 `dut_id=Some`/grants 恰好 Read，未来调用方可注册全局读 listener | 已修（W1）：新增 `set_session_listener` 强制上述约束；`listener::issue` 改走该入口并暴露 `pub credential_id` |
| 12 | 低 | `mcp_runtime.rs:572-583`、`snapshot.rs:54-58` | `runtime/mcp-state.json` 无认证保护、同机任意进程可读（绕过 Bearer 的本地日志旁路） | 遗留：本轮实测仍每 2s 写盘（mcp_runtime.rs:597）；未在本轮关闭 |
| 13 | 中 | `ADR-3:172-193`、`ADR_SIGNOFF:39-41` 等 | ADR-3/ADR_SIGNOFF 把已实现项（NotSigned UI/许可页/外部启动链）列为 deferred | 已修（W5+修复轮）：三处残留改「已实现（2026-11-21 收口轮）」并附代码证据；ADR_SIGNOFF 记「状态更新非重签、对外发布仍 blocked」 |

#### 11.1.3 发布工程、展示与许可（17 项）

| # | 级 | 位置 | 问题 | 处置 |
|---|---|---|---|---|
| 1 | 中 | `release.yml:81-119`、`release-gate.ps1:201-208` | 发布构建从不给 dist/duthub.exe 做 Authenticode 签名，gate 却硬性要求 Valid | 遗留（发布仍 blocked）：无代码签名证书；gate 按 ADR-3 §6.3 设计失败（deferred） |
| 2 | 中 | `release.yml:47-79,96-110,203-205` | LICENSE/NOTICE/assets/goose/**/gate 脚本未入 git，干净 checkout/CI 失败 | 遗留：本轮实测仍为未跟踪；需提交决策（deferred） |
| 3 | 中 | `release.yml:179-201`、`release-gate.ps1:253-262` | 只为 duthub 生成 SBOM；goose 无第三方许可清单/SBOM（audit ≠ SBOM） | 遗留：goose 上游 v1.60.2 Cargo.lock 缺失；cargo-cyclonedx/cargo-audit 本机未装，SBOM/audit 未本机复验 |
| 4 | 中 | `ADR-3:146-147,172-193`、`API_CONTRACT.md:45` | 签名 ADR/合同写「外部 goose 未接入启动链」「许可页未实现」，与实现矛盾 | 已修（W5+修复轮）：ADR-3 §3/§4/§6 三处残留清零并附代码证据（见 11.2.9） |
| 5 | 中 | 报告 §1/§3/§7、`ADR_SIGNOFF:10-12` | 报告称 Cargo.lock 未改/指纹 40d9ef00，与实际矛盾 | 已修（W5+修复轮）：指纹更新、删除行如实记 8 并经所有者 escalate 确认（见 §11.5） |
| 6 | 中 | `mcp_runtime.rs:541-569`、`pages.rs:2955-2983` | MCP bind 失败只 eprintln+continue，UI 看不到「端口未监听」 | 已修（W4a）：`McpRuntimeHandles.failures` + `failure_for_port`；Agent 接入页/MCP 卡显示实际监听与绑定失败 |
| 7 | 中 | `pages.rs:1311-1358`、`workspace.rs:4909-4926` | Agent 下拉会话运行中仍可切换且无「下一次启动生效」提示 | 已修（W4b）：运行中锁定下拉 + 显示实际 launch agent_id/程序 |
| 8 | 中 | `workspace.rs:1165-1174,5019-5041,5240-5252,5507-5532` | 外部 goose SHA-256+版本探测与 keyring 读取在 UI 线程同步执行（最多冻结 5s） | 部分（W4b）：keyring 解析移工作线程（FromSettings）；probe_program 仍在 UI 线程（本轮实测 workspace.rs:1631/5455/6215） |
| 9 | 中 | 报告 §2 模型管理节 | 报告称仅支持 OpenAI 兼容、Anthropic 在 UI 禁用，与实现矛盾 | 已修（W5）：报告按 `model_env.rs:235-247` 更新为 Anthropic 支持 |
| 10 | 低 | `pages.rs:1502-1510`、locales | transcript Agent 行固定标 GOOSE、prompt 占位符写死 goose 文案 | 已修（W4c）：不再硬编码 GOOSE（本轮 grep 无字面量残留）；占位符改「输入发给 {agent} 的指令…」（zh-CN.json:138，本轮实测） |
| 11 | 中 | `pages.rs:1307,1538-1565`、`registry.rs:58,166,231` | Authenticode 徽标固定读内嵌 manifest；外部探测恒 NotSigned | 已修（W4c）：外部程序显示「未验证」而非 NotSigned；徽标按实际 launch agent 选择 |
| 12 | 低 | `pages.rs:1971-1975`、`bundle.rs:21-23` | 内置 goose expected_sha256=None 被显示「未固定哈希」（内嵌哈希实际已在 bundle.rs 固定） | 遗留：本轮实测 `AgentDefinition::goose()` 仍 None（agent_config.rs:147）、注册表仍回退 agentRegistryHashUnpinned（pages.rs:2333） |
| 13 | 低 | `pages.rs:2181-2201`、根 NOTICE | 许可页缺根 duthub NOTICE，单文件客户读不到完整声明 | 遗留：本轮实测许可页仅 DUTHUB_LICENSE + goose LICENSE/NOTICE 三面板（pages.rs:2683-2703）；根 NOTICE 未内嵌 |
| 14 | 低 | locales 死键、`pages.rs:2127`、`agent.rs` | 死键无引用；"version pending" 硬编码英文；demo Inspector 中英混合 | 部分：死键按「两份 locale 键集合一致」保留；"version pending" 仍在（pages.rs:2127，本轮实测）；demo Inspector 完整删除 deferred（需迁移既有单测） |
| 15 | 低 | `workspace.rs:5519-5523`、`agent_config.rs:107-110` | 外部路径可保存相对路径，启动按当时 CWD 解析 | 已修（W2a/W4b）：外部 program 必须绝对路径（盘符/UNC 跨平台判定）；`validate_external_program_path` 保存前 fail closed |
| 16 | 低 | `release.yml` 全部 uses | 第三方 action 按主版本标签引用、publish 持 contents:write | 已修（W5）：12 处 uses 全部 40 位 SHA pin（`git ls-remote` 逐一复核；本轮 grep 复核仍 12） |
| 17 | 低 | `release.yml:87-94,170-177` | smoke 无条件 exit 0、gate self-test continue-on-error，门可在二进制无法启动时通过 | 遗留：本轮实测仍为 informational/continue-on-error（release.yml:92/99/181）；headless 可判定 smoke 另立任务 |

### 11.2 实施波次与结果（含未通过波次，如实记录）

九个波次按 owns 独占写集串行执行，同一时刻一个写者；各波完整 owns 文件清单见各波执行记录，
本节汇总交付、验收与 gaps（遗留如实保留）。除注明外全部使用隔离 target
`CARGO_TARGET_DIR=C:/Users/luo-j/duthub-impl-target`（未写共享 `target/`）、clippy
`-D warnings` 与 `cargo fmt --all --check` 通过、改动未提交未 push。

#### 11.2.1 W1 MCP 授权、审批与审计硬化（含 listener 接线）——通过

- **owns**：`duthub-core/src/{audit,approval,agent_access}.rs`、`duthub-mcp/src/{server,auth_http,transport}.rs`、
  `agent_host/listener.rs`、`duthub-mcp/tests/auth_hardening.rs`、`duthub-core/tests/approval_audit.rs`、
  `apps/desktop/tests/listener_hardening.rs`。
- 交付：`authorize_scoped_read` 通用监听器改 `authorize_any_read`，`dut_files` 省略 dutId 的全局
  清单分支同样要求任一 read grant；`CurrentPrincipal` 提取改走 `principal_from_parts`，带认证
  上下文的服务缺 HTTP Parts 时 401 而非回退 LocalAdmin。审批令牌 `apr-`+CSPRNG（不可由 args
  hash 推导），新增 `ApprovalFingerprint{token_sha256,dut_id,action,args_sha256}` 与
  `with_audit(Arc<AuditSink>)`，进程内 audit 环仅存指纹（容量 512），配置 sink 后写
  approval.issue/consume；脱敏键名归一化；审计文件 `MAX_AUDIT_FILE_BYTES=8MiB` + 最多 4 次轮转。
  `AuthSvc` 每监听器滑动窗口 60 次/60s → 429 + Retry-After，auth.fail 只记一条聚合；`/mcp` 前置
  1MiB 请求体上限（超限 413），`serve_listener` 改 `into_make_service_with_connect_info`。
  `set_session_listener` 强制绑定 DUT、凭据恰好一条该 DUT Read 并回写 dut_id；`credential_enabled`
  合并复核；`McpServerCore::for_evidence`（access=None + listener_dut + evidence_scope）与
  `EVIDENCE_TOOL_NAMES` 8 工具，scope 外工具返回 -32000；tool.call 审计 tool/dut 截断去控制字符。
- 验收：`cargo test -p duthub-core -p duthub-mcp --all-targets` EXIT 0（core lib 89 passed/1 ignored、
  approval_audit 4、auth_hardening 5、http_auth_e2e 17、listener_lifecycle 4、security_canary 4、
  stdio_e2e 3、mcp lib 16）；`cargo test -p duthub-desktop --test listener_hardening` 5 passed。
  新增用例：零 grant 拒读、认证后停用 401、evidence scope 白名单、60 次错 token 第 61 次 429
  且 auth.fail 只落 61 条（60 条 + 1 条聚合）、审批同参两次 token 不同且审计无明文、
  access_token/api-key/clientSecret/db_password 均脱敏、审计轮转上限（9×1MiB 触发轮转）。
- gaps：`ApprovalManager::with_audit` 的产品构造点接线未完成（本轮实测 workspace.rs:1396 仍
  `ApprovalManager::new()`）；限流为进程内固定常量（重启清零、无配置项），窗口耗尽期正确 token
  也 429（防 LAN 暴力猜 token 的有意取舍）；A2A 证据面只实现 call_tool allowlist（tools/list 仍列
  13 个 Plan 工具）；`credential_enabled` 对未知 id/local-admin 返回 false；审计到轮转上限后停止
  当日落盘（有界停写）；未跑全 workspace 测试（仅 owns 包）；`ApprovalFingerprint` 未加到 crate
  根 re-export（lib.rs 不在 owns）。

#### 11.2.2 W2a Agent 托管正确性——通过

- **owns**：`agent_host/{host,acp,session,sandbox,registry,mod,transcript}.rs`、
  `apps/desktop/tests/agent_host_hardening.rs`、`duthub-core/src/{agent_config,store}.rs`、
  `duthub-mcp/src/{probe.rs,lib.rs}`。
- 交付：F1 工具面校验覆盖 Fresh 与 Degraded；Resumed 经 `duthub_mcp::probe::list_tool_names`
  复验 MCP 工具面，失败即 Failed、清理标记并提示新建会话；恢复改 `resume.json`
  （agentId+programSha256）权威判定，跨 Agent/换程序哈希一律忽略并注记，legacy session-id 仅
  goose 且无 resume.json 时尝试；新会话/恢复/降级双写 resume.json + session-id，显式停止（含
  启动期停止）删除两者；RESERVED_ENV_KEYS 在 agent_config 与 host 双重拒绝，模型 env 改为
  envKeys 之后合并且同名冲突报错；`AcpInner` 新增 closed/cancel 标志与 `mark_closed`（stdout
  EOF/读错误/进程退出唤醒全部 pending，request 每 20ms 轮询），新增 `spawn_with_cancel`；新增
  transcript.rs 独立写线程（逐条 write_all+flush，cap/失败注记由写线程回写），事件循环只入队；
  store 的 sha256 改十六进制查表/预分配，内容寻址内嵌文件哈希每进程一次、外部程序每次复算；
  外部 program 必须绝对路径（盘符/UNC）；补 `start_entry(BindingSource)` 与
  `stop_and_join(StopJoin)` 供 W4a/W4b 使用。既有测试断言未改，另增 9 个硬化用例。
- 验收：`cargo test -p duthub-desktop --lib --bins --tests` EXIT 0（lib 51、bin 74+9 ignored、
  12 个集成套件含 agent_host_hardening 9、agent_host 11、agent_registry 7、goose_binding 5、
  goose_lifecycle 4、goose_job_kill 4、goose_mcp_injection 3 等）；`cargo test -p duthub-core
  -p duthub-mcp --all-targets` EXIT 0（core lib 92、mcp lib 17 含新 probe 单测）；Cargo.lock 未改。
- gaps：bundle.rs 不在本波 owns——registry::resolve 内嵌路径仍每次对 20MiB EMBEDDED_EXE 做
  内存 sha256 并跑一次 `--version`；「probe 失败 → Failed 并清理标记」分支无专门端到端用例
  （靠实现审查）；initialize 前极早期停止清理无专门测试；resume.json 用 serde_json::Value 手工
  解析（未加 serde 依赖，Cargo.lock 未变）。

#### 11.2.3 W2b 重启退避 + OS 隔离——通过

- **owns**：`duthub-core/src/{restart.rs（新）,os_acl.rs（新）,lib.rs}`、`duthub-core/Cargo.toml`、
  `agent_host/{host,session}.rs`、`agent_host/recovery.rs（新）`、`apps/desktop/tests/
  {agent_restart,agent_isolation}.rs（新）`、`docs/decisions/ADR-11-OS-ISOLATION.md（新）`、
  `Cargo.lock`（仅增量 13 行：backon + duthub-core 依赖行）。
- 交付：restart.rs（backon 1.6 `default-features=false`+std，1s/2s/4s，滑动 10 分钟窗口最多
  3 次）与 os_acl.rs（SetNamedSecurityInfoW + PROTECTED_DACL 收紧为当前用户+SYSTEM+
  Administrators；GetNamedSecurityInfoW 读回 DACL；CreateRestrictedToken 仅探测并立即关句柄、
  不 spawn；非 Windows 返回 unsupported）；desktop 新增 recovery.rs（should_restart），run_worker
  改重启循环（`WorkerOutcome{ready_reached,stopped,terminal_failure}`：仅 Ready 后非 stop/
  非 watchdog/协议失败才重启，重置前写系统注记，预算耗尽置 Failed，stop 可取消退避等待且
  每轮释放 listener/进程树）；sandbox::prepare 后对每 DUT 目录 ACL 加固 + 隔离探测并写入
  SessionView；session.rs 新增 reset_for_restart()（仅 Stopped→Starting）与
  os_isolation()/set_os_isolation()；新增 ADR-11 与两组集成测试。受限令牌启动/AppContainer/
  专用账户按要求未实现。
- 验收：`cargo test -p duthub-core -p duthub-desktop --lib --bins --tests` EXIT 0（24 suite 全 ok，
  含 restart 预算 3 次/10 分钟拒第 4 次、ACL 加固/加固失败报告/探测不 spawn、
  restart_only_after_ready、stop_cancels_pending_restart；既有 goose_lifecycle 仍 ok）；
  Get-CimInstance 查 `*fake-acp*` 无残留子进程。
- gaps：recovery.rs 未注册进 mod.rs（改由 host.rs `#[path]` 编译，公开路径
  agent_host::host::recovery）；OsIsolationStatus 增加 `unsupported` 字段；重启预算口径为
  「允许 3 次自动重启」→ 总 spawn 4（首启+3），测试按此口径；ACL/探测仅 Windows 本机实测，
  非 Windows 分支未编译验证；fake ACP 复用系统 powershell（agent_restart 约 13s）；受限令牌
  「不 spawn」以 canary 未运行证明（未用内核级审计）；仅验收命令覆盖的包，未跑全 workspace。

#### 11.2.4 W2c CLI profile（claude-code，additive 入口）——**未通过**

- **owns**：`agent_host/{cli.rs,host.rs,mod.rs}`、`apps/desktop/tests/agent_cli.rs`。
- 交付（含修复轮）：`validate_custom_args` allowlist（`--model`/`--effort`/`--fallback-model`/
  `--max-budget-usd` 带值 + `--verbose` 开关），拒绝裸位置参数（会被 Claude 当 prompt 覆盖
  stdin 轮次）与值缺失；`is_reserved_cli_arg` 增加 `--no-session-persistence`/
  `--system-prompt[-file]`/`--append-system-prompt[-file]`/`--debug` 等；`parse_claude_event` 的
  tools[] 兼容字符串与 `{name}` 对象（2.1.204 init 实际为 `array(string)`，对象容忍防版本漂移）；
  真实 CLI 测试改 `DUTHUB_CLAUDE_EXE` → PATH → `USERPROFILE\.local\bin` 发现并修正文件头注释。
- 阻断（未通过原因）：真实 Claude Code 2.1.204 连接与 `${DUTHUB_AGENT_TOKEN}` 展开已用非
  模型调用验证成功，但 `claude mcp get duthub` 报 `Status: ! Connected · tools fetch failed`、
  `Issue: Invalid input: expected "object" (at tools.0.outputSchema.type) (+12 more)`——rmcp
  3.10.0 为 13 个返回 `Json<Value>` 的 tool 生成非 object `outputSchema`，Claude 拒绝注册全部
  工具。修复属 `crates/duthub-mcp`（typed 输出/显式 override/兼容层剥离），本轮按 write set
  约束未修改；`cli_live_claude_exposes_thirteen_plan_tools` 保持 `#[ignore]`（上游修复后的验收
  门禁，且会发起计费模型调用），**13 工具真实端到端未关闭**。
- 验收：`cargo test -p duthub-desktop --lib --bins --tests` EXIT 0（17 套全绿；agent_cli 6+1
  ignored、`cli_profile_is_rejected_before_spawning` ok、`cli_real_claude_expands_mcp_token_from_env`
  ok——该测试内 `probe::list_tool_names` 断言 listener 恰好 13 个 Plan 工具、正确 token Connected、
  错误 token 不 Connected）；环境实测 `claude --version` 2.1.204、`claude auth status` OAuth 已登录。
- gaps：真实模型调用未执行（计费调用未获授权；outputSchema 阻断已足以判定工具不会注册），
  真实 `--resume`、真实事件流未实测（事件解析由 fixture 覆盖）；文档漂移（TASKS:817 写
  `crates/duthub-acp/src/profiles/claude.rs`，实际 `apps/desktop/src/agent_host/cli.rs`；
  BEST_PRACTICE:723 未记 outputSchema 阻塞）；`run_cli_worker` 为 pub(crate)（公开面 CliPlan/
  CliEvent/PLAN_TOOLS/claude_code_plan(_with_resume)/parse_claude_event/ensure_cli_launchable）；
  CLI 显式 stop 后保留 resume.json（与 ACP「stop 即清」有意不同），无「开始新对话」产品动作；
  T29 凭据复制/只读挂载未做；`.cmd` shim 仅 SHA-256 TOFU；agent_host/ 目录整体 untracked，
  写集用 git status + 文件时间核对（Cargo.lock 本波未改）。

#### 11.2.5 W3 A2A crate 与三份 ADR——通过

- **owns**：`docs/decisions/{ADR-2-A2A-LAN,ADR-8-AGENT-CARD,ADR-9-STATEDB}.md`、根
  `Cargo.toml`/`Cargo.lock`、`crates/duthub-a2a/**`（Cargo.toml + src 8 模块 + tests/e2e.rs）、
  `duthub-core/src/model.rs`。
- 交付：三份 ADR（ADR-2-A2A-LAN / ADR-8-AGENT-CARD / ADR-9-STATEDB，均为 Proposed 未签字）；
  新 crate `crates/duthub-a2a`（8 模块 + 21 e2e + 16 单测）：127.0.0.1-only 监听（非回环
  bind_addr → NeedsProductSignoff）、Bearer fail-closed（HTTP 401 + with_authorizer 逐请求二次
  复核）、3 方法严格门（SendMessage/GetTask/CancelTask 委托 DefaultRequestHandler，其余 8 个
  A2A 方法一律 -32601）、RequestBodyLimitLayer 256KiB（300KiB 实测 413）、`Vary: A2A-Version`
  append 不覆盖 Card 的 `Vary: Origin`、A2A-Version 缺失/不支持 → -32009、Agent Card 最小化
  （name 固定 duthub、无 url/preferredTransport/security、无 DUT 名/COM/路径/hostname/token，
  实测 <5KiB）、8 证据 skill 白名单（镜像 W1 EVIDENCE_TOOL_NAMES）经 tokio duplex + rmcp
  client 调 `McpServerCore::for_evidence(state,dut)`、DataPart {skill,dutId?,args}（TextPart/
  未知 skill/跨 DUT/smuggled dutId/snake_case args → TASK_STATE_FAILED）、messageId 进程内
  去重（上限 4096）、TaskOwnership（unknown taskId → -32001）、PortRegistry（port=0/重复/
  reserved/OS bind 失败 → PortConflict）、token 缺失 → Failed 不 bind、set_token 即时换 token
  （旧 token 立刻 401）并写回 TokenSource；model.rs `DutA2aConfig` 增 `bind_addr`（serde default +
  skip，camelCase 往返单测）；workspace members 增 crates/duthub-a2a；Cargo.lock 纯增量
  （464 增 / 7 行依赖表重排、无版本变更）。
- 验收：`cargo test -p duthub-a2a --all-targets` 16 lib + 21 e2e 全 ok（含 Card 最小化、
  Bearer/版本门、gated methods -32601、跨 DUT/smuggled 拒绝、413、端口门与签字门、set_token
  热轮换等 17 个具名用例）；`cargo test -p duthub-core --test roundtrip` 7 passed（既有不可改
  测试全绿）；`cargo tree -p a2a-server-lf -e features | grep -E "rustls-tls|native-tls"` 无输出、
  `cargo tree -p duthub-a2a -e normal | grep aws-lc` 无输出（rustls 0.23.46 经既有
  duthub-mcp/duthub-core 带入，未新增 TLS crate）。
- gaps：AGENTS.md 教训回写未做（当时外部在途禁改，本轮收口已补写，见 AGENTS.md 追加节）；
  未跑 workspace 全量；`A2aStatus::Starting` 为瞬态（start 内联 await，仅并发 status 可观测）；
  127.0.0.1 以外回环字面量（127.0.0.2、::1）同样 NeedsProductSignoff（比「非回环」更严，写入
  ADR-2 §2.2）；set_token 的 keyring 读写经 spawn_blocking，未做真机 keyring 集成（沿用 keys.rs
  ignored 口径）；证据调用 30s 超时、去重表上限 4096；e2e 用 router 级 + 真实 socket 双覆盖，
  无外部 Agent 客户端参与；LAN/TLS/持久化 StateDb 按 ADR deferred。

#### 11.2.6 W4a A2A 运行时 + OS 隔离接进 Workbench——通过

- **owns**：`apps/desktop/{Cargo.toml,src/lib.rs,src/agent_ui.rs,src/a2a_runtime.rs,
  src/mcp_runtime.rs,src/main.rs,src/workspace.rs,src/pages.rs,locales/*.json}`、
  `apps/desktop/tests/agent_ui_logic.rs`、`Cargo.lock`。
- 交付：新 lib 模块 agent_ui.rs（a2a_endpoint_view 仅 Running 显示回环 Card URL；
  validate_a2a_port/reserved_ports_from；a2a_export_payload 秘密扫描 fail closed；
  DutCleanupOps/run_remove_dut 五步清理序；pending-cleanup 日志与启动重试；OS 隔离徽标/
  信任级别复核——任何路径不显示 os_sandboxed，未证实降级「未验证/未启用」）与 bin 包装
  a2a_runtime.rs（Workbench 持有 `A2aRuntime` + 专属 tokio 驱动，驱动失败降级不 panic）；
  Workbench Drop 顺序 = 停 Agent → a2a.shutdown → mcp_runtime.shutdown；McpRuntimeHandles 增
  `failures:Vec<(u16,String)>`+`failure_for_port`，bind 失败不再只 eprintln；remove_dut 改走
  run_remove_dut，Token/目录失败写 `<work_dir>/agent/pending-cleanup.json` 并弹告警、不谎称
  清理完成；启动 retry_pending_cleanup；启用 A2A 即时拉起监听、停用/删 DUT 即时停监听、
  重新生成 Token 调 a2a.set_token 热轮换；保存/启用拒绝 port 0 与保留端口；导出
  a2a_server.json 过 export_contains_secret；pages：per-DUT A2A 卡显示实验性/仅本机/失败原因、
  删局域网 URL 行与「运行时尚未接入」、全局面板 host 标 deprecated、MCP 卡与 Agent 接入页
  显示实际监听与绑定失败；新增 tests/agent_ui_logic.rs。
- 验收：`cargo test -p duthub-desktop --lib --bins --tests` 207 passed / 0 failed；agent_ui_logic
  11 passed；`cargo run -q -p duthub-desktop -- --self-test` OK（2 frames · MCP HTTP serving on
  127.0.0.1:8797）；`cargo test -p duthub-mcp --test http_auth_e2e --test listener_lifecycle
  --test dut_binding` 22 passed。中途 plan_badges 源码契约失败按「改实现不改测试」修复
  （stop_agent_session 去 cx 参数三处复用、spawn 栈保留 bin）。
- gaps：mcp_bind_failure 测试为源码契约（bin 模块无法在 lib test 实例化；真实 spawn 记录路径
  未被集成测试执行，附真实内核二次绑定必败佐证）；reserved_ports 实现为 MCP 端口 External
  预登记 + 合集冲突检测（草案字面的「全部预登记」会让 PortRegistry 拒绝运行时自己的端口）；
  包装器接口收敛为 start/status/any_running/set_token/start_duts/stop_dut/shutdown；
  enable_dut_a2a 即时拉起、save_dut_a2a 端口变更停旧监听（实现选择）；OS 隔离徽标渲染时现场
  探测未缓存；locales 死键 a2aServiceStopped 保留；agent.rs 侧栏面板不在 owns 未触碰；未跑
  bash scripts/regression 全量；Cargo.lock 除新增依赖外 cargo 归并自动收敛 7 行传递依赖。

#### 11.2.7 W4b 产品启动入口收口 + 注册表编辑器——通过

- **owns**：`agent_ui.rs`、`workspace.rs`、`pages.rs`、`tests/agent_ui_logic.rs`、
  `locales/{zh-CN,en-US}.json`。
- 交付：产品启动入口切 `host.start_entry`（goose ACP 才传 `BindingSource::FromSettings`，
  CLI/第三方 ACP 恒传 None，CLI 由 start_entry 路由到 cli 分支；workspace.rs 不再出现
  start_with_agent）；注册表编辑器 args 改 JSON 数组（parse_args_json/args_to_json 往返，
  编辑器实时显示解析错误）；env 值掩码编辑器（In::AgentRegEnvValue + InputState::masked +
  mask_toggle，env_value_source 状态机 Stored/NotSet/LookupFailed，写/删走
  `set/delete_agent_env`，删除凭据同步移出键名草稿）；保存前统一 `validate_agent_editor_draft`
  （args JSON + validate_env_keys 保留键 + validate_external_program_path 绝对路径 Windows
  语义），非法草稿 fail closed 不落盘；删除 Agent 先 save_agents_atomic 再
  delete_agent_env_entries（失败键写 pending-cleanup 新步骤 delete_agent_env_keys + 告警，
  启动重试按步骤分支且不误跑 DUT 步骤）；`validate_agent_toggle` 禁止停用默认 goose 与
  最后一个启用条目；会话运行中 `running_agent_selector_disabled` 锁定下拉 +
  `running_launch_line`（实际 launch agent_id/程序）+「下一次启动生效」。agent_registry.rs
  7 项未动全绿；新增 9 用例；locales 中英各 +27 键（667/667 一致）。
- 验收：`cargo test -p duthub-desktop --lib --bins --tests` 全 ok（agent_ui_logic 20、
  agent_registry 7；fmt 后复跑同样全绿）；node 校验 locales 键集合一致（only zh/only en 均空）；
  grep 核查 workspace.rs 无 start_with_agent/parse_agent_env_keys/split_whitespace 残留。
- gaps：GUI 渲染/交互观感未做真机验收（纯逻辑测试 + 源码契约 + host 启动链路回归）；
  keyring 真实写删的 GUI 路径未在凭据管理器实点（走可注入 store）；pending-cleanup
  delete_agent_env_keys 的真实凭据库失败重试需 OS 故障注入未实测；CLI 真实端到端未重跑
  （start_entry 路由契约 + 既有测试全绿）；agentRegistryEnvDuplicate 死键保留（重复键错误改由
  agent_ui 纯函数返回硬编码中文）；goose 绑定解析失败 toast 时机变化（移入 start_entry，
  文案由 host 拼接）。

#### 11.2.8 W4c 展示、自检纯净与交互收尾（复验轮）——**未通过**

- **owns**：复验轮零改动（files 为空；上轮交付 owns 9 文件 mtime 逐一吻合：workspace.rs
  22:26:08 / pages.rs 22:21:38 / main.rs 20:22:02 / side_panel.rs 22:09:38 / agent.rs 22:09:44 /
  agent_ui.rs 22:21:37 / agent_ui_logic.rs 22:21:38 / zh-CN·en-US.json 22:10:09）。
- 复验记录：mustFix 为空、本轮未修改任何文件；`cargo test -p duthub-desktop --lib --bins
  --tests` 18 target 全绿（agent_ui_logic 27 passed，含 W4c 七个新用例）；clippy 干净；
  self-test + `--workdir` 实测：SELF-TEST OK 2 frames、`! -f "$TMP/duthub-data/agents.json"` OK、
  `! -d "$TMP/duthub-data/agent"` OK（目录内仅既有 audit/、runtime/）、netstat 前后 8797/8790
  无监听。
- 未通过原因（波次整体遗留）：全 workspace `cargo fmt --all --check` EXIT=1 共 6 处 diff
  **全部位于 crates/duthub-transfer/**（外部并行上下文持续写入、本轮禁改；owns 9 个 rs 文件
  单独 rustfmt --check 通过）；AGENTS.md 教训回写当时未做（外部在途禁改——本轮收口已补写，
  见 AGENTS.md 追加节）；GUI 渲染观感人工验收未做（右栏 Inspector 真实渲染、2000 条后贴底、
  外部 goose 徽标「未验证」文案仅由 agent_ui_logic 纯函数 27 用例 + 源码契约覆盖）。
- 说明：W4c 实现内容（贴底改 `total_entries()` 单调序号、self-test 绕过 load_agents 落盘、
  外部签名徽标「未验证」、per-DUT prompt 清空 map）已在本轮代码复核确认在位
  （pages.rs:1514-1539、workspace.rs:974/6039-6054、agent_ui.rs:683/690/757）。

#### 11.2.9 修复轮（ADR-3 残留 + Cargo.lock 裁决回写）——通过

- **owns**：`docs/decisions/ADR-3-GOOSE-BUNDLE.md`、`docs/reviews/AGENT_IMPLEMENTATION_REPORT.md`。
- 交付：① ADR-3 残留旧表述清零——除 §3:53 外，§4:125 与 §6:160 两处同类矛盾一并改为
  「已实现（2026-11-21 收口轮）」并附代码证据（agent_ui.rs:846 EmbeddedNotSigned、
  ConfigPage::Licenses）；`grep -cE '尚未实现|未实现'` = 0（本轮复核仍为 0）；签名链验证/
  SBOM/旁挂的 deferred 表述保留不动。② 所有者在修复轮 escalate（问题 `dwfq-f28d1902-1`）
  显式确认「Cargo.lock 删除行如实记 8 不写 0、不做 cargo update --precise」裁决，原文见
  §11.5。同时按验证轮 missing 回写报告：self-test frames 标注为 1200ms 窗口运行时观察值
  （main.rs:238-245 实读，判定仅要求 rendered != 0）；http_auth_e2e/agent_registry 基线由
  全量测试独立覆盖的说明；duthub-transfer 记录（见 §11.8）。
- 验收：第 5 次运行 `bash scripts/regression` EXIT=0、末行 REGRESSION OK（fmt 无 Diff、
  clippy 无 warning、61 个 test-result 汇总 716 passed / 0 failed / 20 ignored、self-test
  2 frames）；Cargo.lock 指纹 84b4da63… 与门禁前一致；删除行 4 组版本消歧（foldhash 4 /
  md-5 1 / system-configuration 1 / tower-http 2 = 8）；duthub-control 行未动（HEAD 2 处 vs
  工作树 3 处，+1 为 duthub-desktop 新增边）。
- 运行历史（如实）：第 4 次撞并行 transfer fmt 竞态（reject_paths.rs:274、adapter.rs:836），
  轮询 45s 后对方自行格式化，第 5 次通过；更早 W5 轮曾遇 LNK1140/LNK1285（磁盘压力/PDB
  损坏瞬时失败，删 debug/incremental 15.4G 与损坏产物目录后通过）；并行 transfer 上下文
  曾在验证窗口新增 14 个文件（归属不变、本波禁改）。

### 11.3 每波通过情况

实施波次通过标记：`[true,true,true,false,true,true,true,false,true]`（按 §11.2.1–§11.2.9 顺序）。

| 波次 | 结果 | 关键依据 |
|---|---|---|
| W1 MCP 授权/审批/审计硬化 | 通过 | 验收命令全绿 + 零 grant/停用/限流/脱敏/轮转回归 |
| W2a Agent 托管正确性 | 通过 | agent_host_hardening 9 + 既有套件全绿 |
| W2b 重启退避 + OS 隔离 | 通过 | restart/ACL/隔离探测回归全绿（24 suite） |
| W2c CLI profile（claude-code，additive 入口） | **未通过** | rmcp 3.10.0 非 object outputSchema 阻断真实 Claude 工具注册；13 工具端到端未关闭 |
| W3 A2A crate 与 ADR | 通过 | duthub-a2a 16+21 全绿 + roundtrip 既有测试全绿 |
| W4a A2A/隔离接进 Workbench | 通过 | 207 passed + self-test OK |
| W4b 启动入口收口 + 注册表编辑器 | 通过 | agent_ui_logic 20 + agent_registry 7 未动全绿 |
| W4c 展示、自检纯净与交互收尾 | **未通过** | 复验轮零改动且 owns 命令全绿，但全 workspace fmt 受外部 transfer 影响、AGENTS.md 回写当时未做、GUI 人工验收未做 |
| 修复轮（ADR-3 + Cargo.lock 裁决） | 通过 | regression 第 5 次 EXIT=0 + ADR-3 残留清零 |

**未通过波次说明（2 条）**：

1. **W2c CLI profile（claude-code，additive 入口）**：非本波 write set 缺陷——rmcp 3.10.0 的
   outputSchema 生成问题属 `crates/duthub-mcp`；修复前真实 Claude Code 拒绝注册全部 13 个
   Plan 工具，`#[ignore]` live 测试作为修复后的验收门禁保留（见 11.2.4 阻断与 gaps）。
2. **W4c 展示、自检纯净与交互收尾**：复验轮自身零改动、验收命令全绿；未通过归于波次整体
   遗留——全 workspace fmt 当时受外部 duthub-transfer 在途写入影响（6/6 处均在该 crate）、
   AGENTS.md 教训回写当时被外部在途禁改阻断、GPUI 渲染观感人工验收未执行。前两项已由
   本轮收口关闭（最终门禁 fmt 全绿 + AGENTS.md 追加节）；GUI 人工验收仍在 §11.7。

### 11.4 全量门禁（最终收尾记录）

下表为收口轮的最终门禁实测记录（收口架构师执行并随收口输入提供，非本节写作时复跑；
本节写作时仅做只读复核，见附注）。`failures` 与 `missing` 均为空。

| 检查 | 结果 |
|---|---|
| `df -h C:/ D:/` | exit 0（C: 可用 25G、D: 可用 99G；隔离 target `C:/Users/luo-j/duthub-impl-target` 已存在且为暖态） |
| `CARGO_TARGET_DIR=C:/Users/luo-j/duthub-impl-target bash scripts/regression` | 整体 exit 0，末行 `REGRESSION OK`。按 scripts/regression:1-21 确认四步全含于脚本，依「不要重复跑同一检查」仅跑此一次，未单独重复 fmt/clippy/test/self-test |
| [1/4] `cargo fmt --all --check` | exit 0（无 diff 输出） |
| [2/4] `cargo clippy --workspace --all-targets -- -D warnings` | exit 0（Finished `dev` profile 2.03s，全部缓存命中零告警） |
| [3/4] `cargo test --workspace --quiet` | exit 0（约 60 个测试目标全 ok，合计 ~840 passed / 0 failed / 14 ignored；ignored 为真机 live 测试 `com8_io_latency_live`、`com8_reset_boot_live` 等，按设计显式运行才跑，非门禁项） |
| [4/4] `cargo run -q -p duthub-desktop -- --self-test` | exit 0（`SELF-TEST OK: 3 frames · 0 core lines · duthub-desktop v0.1.1 (gpui-kit)`） |
| `git status --porcelain \| wc -l` | 62（与门禁前在途改动一致，运行未新增/修改工作树文件，只读核查） |

- self-test 的 frames 为 1200ms 窗口内的运行时观察值（本轮实读 `main.rs:238-245`：定时后读
  `frames` 计数，判定仅要求 `rendered != 0`）——修复轮记录 2 frames、本轮门禁 3 frames，
  均为通过状态，非矛盾。
- 本节写作时的只读复核（均实测）：`git status --porcelain | wc -l` → 62；
  `git diff Cargo.lock | sha256sum` → `84b4da63d1698c04bac5e8b984c9deb1301c94bcb57ff73cddf827d0f528d3b2`
  （与修复轮一致）；`git diff -U0 -- Cargo.lock | grep -cE '^-[^-]'` → 8（foldhash 4 / md-5 1 /
  system-configuration 1 / tower-http 2）；`grep -cE '尚未实现|未实现'
  docs/decisions/ADR-3-GOOSE-BUNDLE.md` → 0；`ls docs/decisions/` → ADR-2/3/8/9/11/WORKDIR/
  SIGNOFF 在位、`crates/duthub-a2a/src/` 8 模块在位；关键代码落点抽查见 §11.1 各行「本轮实测」
  标注（principal_from_parts auth_http.rs:331、RequestBodyLimitLayer listener.rs:134、start_entry
  host.rs:239、mark_closed acp.rs:321、running_agent_selector_disabled agent_ui.rs:683 等）。
- 本节未复跑项（如实）：cargo test/clippy/fmt/regression、release-gate、SBOM/签名工具——本节
  为文档收口，未执行任何构建类命令；W5 修复轮本地实跑 `scripts/release-gate.ps1` 曾退出码 1
  （`dist/duthub.exe` 缺失，goose 资产双哈希 3 项通过），属按 ADR-3 设计 fail closed。
- 对外发布仍 blocked：duthub.exe Authenticode 签名（无证书）、goose SBOM/许可清单（缺上游
  Cargo.lock，cargo-cyclonedx/cargo-audit 本机未装、未本机复验）、发布资产入库（LICENSE/NOTICE/
  assets/goose/gate 脚本实测仍为未跟踪）三项缺失，属 ADR-3 §6.3 未满足项。

### 11.5 Cargo.lock 删除行核验与所有者裁决（如实记录 8，不写 0）

`git diff -U0 -- Cargo.lock` 的 8 条删除行**全部**是 4 组版本消歧重命名（同包多版本时 Cargo
要求依赖项带版本号寻址，属「新增依赖增量」的必然副产物），没有其他包被降级、移除或回退：

| HEAD（裸名，单版本无歧义） | 工作树（带版本） | 引入第二版本的依赖 |
|---|---|---|
| `"foldhash"` | `"foldhash 0.2.0"` | hashbrown 0.15.5（fileloft 链）→ foldhash 0.1.5 |
| `"md-5"` | `"md-5 0.10.6"` | fileloft-core 0.3.0（duthub-transfer）→ md-5 0.11.0 |
| `"system-configuration"` | `"system-configuration 0.6.1"` | hyper-util → system-configuration 0.7.0 |
| `"tower-http"` | `"tower-http 0.6.11"` | a2a-server-lf 0.5.3（duthub-a2a）→ tower-http 0.7.1 |

- `git show HEAD:Cargo.lock` 中这 4 个包各只有 1 个版本；工作树各 2 个版本；`cargo metadata
  --locked` 退出 0（修复轮实测）。
- 外部在途 `duthub-control` 行未动：该行在 diff 中是**上下文行**（无 +/-）；工作树多出的第 3 处
  `"duthub-control"` 引用是 `duthub-desktop` 新增依赖边（纯新增，属允许的新依赖增量），
  `duthub-device` 的既有外部行逐字保留。
- **所有者确认（修复轮，2026-11-21，问题 `dwfq-f28d1902-1`）**：裁决经所有者在修复波次
  escalate 中**显式确认成立**，原文要点：「验收口径放宽为：Cargo.lock 的删除行必须全部是
  这 4 组版本消歧重命名，不得出现任何其他包被降级/移除/回退——特别是基线里的
  `duthub-control` 外部在途行必须原样保留；不要用 `cargo update --precise` 去统一版本
  （强行统一要么降级 a2a，要么改区间超出 owns 且有兼容风险）；多版本共存是 Cargo 的正常
  行为，接受它，并在报告 deferred/risks 里注明『依赖树存在 4 组双版本，后续可在无在途冲突
  时单独收敛』」。**不得动 Cargo.lock/依赖区间**。
- 风险登记：依赖树存在上述 4 组双版本，后续可在无在途冲突时单独收敛（本收口未做）。
- 本轮写作时复核：删除行计数仍为 8（分组同上表），指纹 `84b4da63…` 未变。

### 11.6 计划决策（16 条）

1. **按评审意见重排切片，不重做已完成项**。基线为当时实测：HEAD `fe24fc6`；git status 45 项
   （评审另测到外部新增文档后为 46，后续轮次只记录执行时实测值、不硬编码）；当时 Cargo.lock
   diff 仅 +2 行、删除行 0。已完成且不得重做：Job Object（job.rs:102-180）、外部 goose 启动链
   （registry.rs:116-168）、NotSigned 徽标/许可页、GOOSE_MODE=approve（model_env.rs:355）、
   Anthropic 支持（model_env.rs:238-239）。
2. **mustFix 逐条落实**：① session-id 保留写入并新增 resume.json 双写，Degraded 先校验再落盘
   （W2a）；② extensionResults 维持「恰好 1 条 duthub success」不放宽，另加未知扩展拒绝用例
   （W2a）；③ A2A 对 ListTasks/GetExtendedAgentCard/push/subscribe 统一 -32601（W3）；④ 请求体
   RequestBodyLimitLayer 256KiB、300KiB→413（W3）；⑤ cargo tree 只查 a2a-server-lf 特性，明确
   rustls 来自既有 duthub-mcp/duthub-core 而非宣称 duthub-a2a 无 TLS（W3）；⑥ listener.rs 划归
   W1、with_audit 接线写进 W4a、agent env set/delete helper 写进 W2a（W1/W2a/W4a/W4b）；
   ⑦ remove_dut 显式顺序 + 幂等目录删除 + 待清理记录，不用 forget bool（W4a）；⑧
   OsIsolationStatus 接徽标、任何路径不显示 os_sandboxed（W4a/W4c）；⑨ AGENTS.md 实施轮不改
   （外部在途 5 insertions/1 deletion）、Cargo.lock 删除检查用 git diff -U0 过滤、rust-toolchain
   pin stable 分支 SHA 且保留 `with: toolchain: stable`（W5）；⑩ start_agent_session 切
   start_entry + 运行中禁用下拉/实际 launch 测试（W4b）；⑪ PortRegistry 由 a2a_runtime::start
   构造 reserved（MCP 主/附加/持久 listener），启动前 reserve、stop release（W3/W4a）；⑫ W2/W4
   各拆三子波并补 restart/resume/ACL/CLI、CancelTask/messageId/Vary/ReadScope、UI 逻辑用例；
   ⑬ A2A 在 ADR Proposed 期标 experimental、不进产品验收/发布说明；ADR-3 只更新实现状态并在
   ADR_SIGNOFF 记「非重签、发布仍 blocked」；cargo-cyclonedx 本机未装，命名修正标未验证。
3. **session-id 与恢复（W2a）**：双写 `<dut>/session-id` + `resume.json{version,agentId,
   programSha256,sessionId}`；Fresh 与 Degraded 都先 validate 再写两文件；Resumed 先 probe
   tools/list，失败转 Failed 并提示新建；agentId/programSha256 不匹配按新会话并明确提示；
   legacy 仅 goose 且无 resume.json 时尝试（不校验哈希、需提示）；不改 goose_binding.rs:233-234
   与 goose_path_root.rs:281-283 既有断言，新测试只追加。
4. **extensionResults 严格口径**：`host.rs` 的「恰好 1 条 duthub success」不改；已知内置扩展与
   任意未知扩展都被 len!=1 拒绝。宿主业务路径版本门槛仍 >=1.60.2，未来 goose 新增内置扩展会
   fail closed——显式兼容策略写入报告，升级 goose 需同步 allowlist 并加测试，本轮不放宽。
5. **A2A 方法门与传输限制（W3）**：a2a-server-lf 0.5.3 已路由 LIST_TASKS 且 DefaultRequestHandler
   有默认实现，故实现 StrictRequestHandler 包装——3 方法委托、其余 8 方法统一 -32601；
   RequestBodyLimitLayer 256KiB 包在 JSON-RPC 路由最外层（库内 10MiB DefaultBodyLimit 在内层，
   外层 DefaultBodyLimit 盖不住，评审实测 300KiB 返回 200）；Vary: A2A-Version append 而非
   insert，不覆盖 Card 的 Vary: Origin。
6. **A2A 证据面纵深防御（W1+W3）**：server.rs `for_evidence`（access=None + listener_dut +
   evidence_scope）与 EVIDENCE_TOOL_NAMES 8 工具，scope 外 -32000；executor 再按 8 skill 白名单
   映射，形成双层；for_evidence 仍走 cross-DUT 拒绝。
7. **A2A 实验边界（W3/W4a/W5）**：ADR Proposed 期只绑 127.0.0.1、默认关闭、Bearer 必填，UI 与
   API_CONTRACT 标 experimental；`DutA2aConfig.bind_addr` 为 Option<String>（skip），非回环 →
   NeedsProductSignoff，绝不绑 0.0.0.0；ADR-3 只在 ADR_SIGNOFF 记「实现状态更新、非重签、
   发布仍 blocked」。
8. **OS 隔离（W2b/W4a/W4c）**：windows 0.62.2 Get/SetNamedSecurityInfoW + PROTECTED_DACL 把
   `<work_dir>/agent/dut/<hex>/` DACL 收紧为当前用户+SYSTEM+Administrators；非 NTFS/网络盘失败
   降级 `private_dir_acl=false` 显示「未加固」；CreateRestrictedToken 只探测不 spawn；受限令牌
   启动/专用账户/AppContainer 全部 deferred 并写 ADR-11；UI 任何路径不显示 os_sandboxed，
   探测失败显示「未验证」。
9. **crate 拆分（W3）**：新建 crates/duthub-a2a（workspace 第 10 成员），duthub-acp 不拆；依赖
   a2a-lf =0.4.2 + a2a-server-lf =0.5.3 `default-features=false` + tower-http 0.6 limit + rmcp
   （AsyncRwTransport 进程内证据 server）；不宣称 duthub-a2a 无 TLS，只验证 a2a-server-lf 未开
   rustls-tls/native-tls 且未新增 aws-lc；ACP 客户端保留 agent_host/acp.rs，官方
   agent-client-protocol 仅作 conformance oracle。
10. **重试与端口（W2b/W3）**：backon =1.6.0 `default-features=false`+std 仅取 1s/2s/4s（不拉
    tokio-sleep）；3 次/10 分钟滑动窗口由 RestartBudget 自实现；自动重启只在曾 Ready 后异常退出
    时发生，启动失败保持 Failed；stop 置 cancel 并唤醒 initialize/session 请求，Drop 有界 join；
    PortRegistry 放 duthub-a2a/src/ports.rs，W4a 把 MCP 主端口 + agentAccess.listeners[].port
    作为 reserved 传入；A2A 端口重复/被占 → PortConflict，stop_dut release。
11. **MCP 授权/审计（W1）**：保留 AGENTS #44/ADR-7 通用 listener 全局读，但 listener_dut=None
    分支改 authorize_any_read（至少一个 Read grant，保住 http_auth_e2e.rs:729-745 既有断言）；
    CurrentPrincipal 具体化——access=Some 无 Parts → 401，access=None（stdio 与 A2A 证据
    server）→ LocalAdmin，纯函数 principal_from_parts 便于单测；审批令牌 apr-+CSPRNG、指纹
    审计、with_audit 写 approval.issue/consume；脱敏键归一化覆盖 access_token/refresh_token/
    api-key，tool/dut 限长（决策草案 128 字符，实现取 64 更严）；MCP HTTP 每监听器限流
    （60 次/分 → 429，auth.fail 聚合），transport 改 into_make_service_with_connect_info，
    limiter 缺 ConnectInfo 用固定哨兵桶；请求体 1MiB；审计大小上限/轮转。
12. **CLI 与既有测试（W2c/W4b）**：不改 agent_registry.rs:251-273（start_with_agent 对 kind=Cli
    的既有错误）与 :178-181（acme+Some(binding) 成功）；新增 `HostManager::start_entry(dut,
    BindingSource, agent)` 产品入口：goose 用 FromSettings 在工作线程解析 keyring，非 goose 只
    接受 None（产品路径不把模型 Token 给第三方）；kind=Cli 分派 cli.rs（claude-code：`-p
    --output-format stream-json --verbose --permission-mode plan --mcp-config` +
    `${DUTHUB_AGENT_TOKEN}` 环境展开、NDJSON 解析、resume.json）；.cmd fixture 测 argv/流解析/
    resume；真实 Claude CLI 验证 deferred。
13. **测试与接口兼容**：行为变化全部 additive（新构造/新类型/新模块），不改既有断言与快照；
    TranscriptLog 同步 API 与其 session.rs 既有单测保持，异步落盘用新 TranscriptWriter。
14. **子波次严格串行**：W2a/W2b/W2c 与 W4a/W4b/W4c 先后复用 host/session/mod/workspace/pages/
    locales/main；每子波完成并全绿后下一子波才接管，同一时刻一个写者；owns 为该子波执行期
    的独占写集；Cargo.lock append-only，严禁回退外部 duthub-control 行。
15. **外部路径与 AGENTS.md**：AGENTS.md 在实施轮不改、教训写 docs，追加列为 deferred（本收口
    轮已执行追加）；Cargo.lock 删除检查用 `git diff -U0 | grep -cE '^-[^-]'`（当时实测 0；后因
    社区依赖引入演化为 8 条版本消歧重命名，见 §11.5 裁决）；com8 两测试与
    duthub-device/Cargo.toml 不触碰、不格式化。
16. **文档/发布诚实**：W5 记录执行时实测计数与指纹、不硬编码；ADR-2/8/9/11 标 Proposed；ADR-3
    只更新实现状态、ADR_SIGNOFF 明写「非重签、发布仍 blocked」；release.yml 修 cyclonedx 文件名
    （--override-filename duthub.cdx）、cargo-audit 退出码交 gate、action pin 到 git ls-remote
    解析的 SHA（dtolnay 用 stable 分支 SHA 39008827e6b4319a349e9a42dfc90579dd25f1ca 并保留
    `toolchain: stable`）；cargo-cyclonedx/cargo-audit 本机未装，SBOM/签名/资产未入库 →
    release gate 按设计失败，报告如实记录。

### 11.7 延期/未覆盖（16 条，如实）

1. **A2A LAN/TLS/mTLS/peers.json/source_cidrs/防火墙/netsh/跨机互操作**：需 ADR-2 与产品签字；
   本轮 bind_addr 非回环 → NeedsProductSignoff，不绑定、不生成 LAN URL，A2A 仅 experimental。
2. **A2A 持久 TaskStore/StateDb**（rusqlite/WAL/journal/checkpoint/Working 恢复/Completed 30 天/
   证据 pin 512MiB）：需 ADR-9 签字与构建门；本轮仅 InMemoryTaskStore，重启即丢任务。
3. **A2A push/ListTasks/GetExtendedAgentCard/subscribe/task.write/headless/24x7/独立 bin**：
   ADR-0/8 未签字且与 GUI 存活边界冲突；StrictRequestHandler 一律 -32601，executor 无
   task.write skill。
4. **OS 级进程启动隔离**：受限令牌 CreateProcessAsUser/WithToken、PROC_THREAD_ATTRIBUTE_HANDLE_LIST、
   AppContainer（SECURITY_CAPABILITIES）、专用账户+ACL 部署：需替换 std::process/Child 与 ACP
   管道并验证串口/USB/继电器访问（普通用户可能 1314）；ADR-11 记录为 deferred，产品不宣称
   os_sandboxed。
5. **duthub-acp crate 拆分 / workspace 11 成员**：ACP 客户端已稳定且无第二消费者，拆分会改
   Cargo.lock/workspace/测试路径并 destabilize；保留 agent_host/acp.rs，官方
   agent-client-protocol 仅作 conformance oracle。
6. **AGENTS.md 新坑追加**：实施轮因外部在途 diff 未改——**本收口轮已执行追加**（AGENTS.md
   末尾「## 2026-11-21 Agent 集成收口（自动追加）」节，只新增不改既有），本条关闭。
7. **旧 demo AgentState/RunStatus/900ms STEP 的完整删除**：会删除 agent.rs 既有单测（违反测试
   红线），实施轮只从 side_panel 卸载产品渲染；完整删除/测试迁移另立任务。
8. **全局 sessions.json 与完整 transcript 回放**（8MiB×10/50k 只渲染尾部）：本轮用每 DUT
   resume.json + 双写 session-id + transcript 尾部最多 200 条回放；全局持久化 deferred。
9. **goose 第三方 SBOM/许可清单、Authenticode 签名/链验证、LICENSE/NOTICE/assets/goose/scripts
   入库**：需上游 v1.60.2 Cargo.lock、代码签名证书与提交决策；实施轮只修 workflow 脚本 bug，
   release gate 仍按设计失败。
10. **真实 Claude/Codex/dsh CLI 验证与 kind=cli 的 config_dir/auth_mode=inherit 复制**：本轮只做
    claude-code argv/NDJSON/.cmd fixture 测试；真实二进制安装、登录、`--resume` 与
    `--mcp-config` 兼容性未验证。
11. **T24 工作根统一**（resolve_work_dir settings>CLI 改 CLI>settings 并改既有单测）：
    ADR-WORKDIR 明确维持双解析器并存，Agent 代码只用 ensure_portable_work_dir。
12. **跨表 credential id 重复拒绝**：agent_access_overlay 既有测试显式断言同 id 合并语义；本轮
    不拒绝，记录为 API 边界（调用方用 UUID），后续按 (表,listener,credential) 作用域改造。
13. **通用 listener 改逐 DUT deny-by-default**：与 AGENTS #44/ADR-7 及 http_auth_e2e.rs:729-745
    既有断言冲突；保留全局读但要求至少一个 Read grant，并在 UI 明示该语义。
14. **真实硬件/在线模型/A2A 1.0 peer/人工 UI 验收**：无凭据与设备，只能列为未运行；不得据此
    声称生产就绪。
15. **审批审计/租约的跨进程持久化与 keyring/DPAPI 迁移（ADR-10）**：本轮只做进程内指纹审计 +
    AuditSink；token 仍按用户既有要求留在 settings.json。
16. **A2A messageId 去重的 task id 稳定性**：executor 层避免重复执行，但 DefaultRequestHandler
    可能为新消息生成新 task id；跨重启去重与 task id 映射需 ADR-9 持久化后实现。

### 11.8 新 crate 与社区依赖说明（按所有者要求记录）

- `crates/duthub-a2a`（workspace 成员）：A2A 实验运行时（见 §11.2.5/§11.2.6），ADR-2/8/9 均
  Proposed 未签字，experimental 不进产品验收与发布说明。
- `crates/duthub-transfer`（workspace 成员，`Cargo.toml:12`）：**收口架构师 Plan 决策引入的
  crate（所有者确认，问题 `dwfq-f28d1902-1`）**——板卡镜像接收端点，tus 1.0.0 可续传上传，
  暂存于 `duthub-data/images`；协议层用社区 `fileloft-axum/fileloft-core 0.3.0`（**MIT**，
  sound-systems/fileloft），存储/锁自写（`#![forbid(unsafe_code)]`）；crate 本体 license 继承
  workspace `Apache-2.0`（`Cargo.toml:22`）。`docs/ARCHITECTURE_REVIEW.md:37-46` 记录其
  `transfer_dir` 语义（同文件系统暂存、SHA-256 校验后 rename 发布）。该 crate 不在实施波次
  owns，收口文档轮只做事实记录、未触碰其源文件。
  > **2026-11-24 更新**：以上为当时记录，实现已被整包重写取代——协议、磁盘存储、每上传锁与
  > 完成定名发布改用 Salvo 官方 `tus` 模块（`salvo-tus` 1.2.1）+ `salvo` 1.2.1（含
  > `http2-cleartext`），整文件校验改为客户端按需声明（`blake3`/`sha256`），fileloft 与自写
  > 存储/锁/GC/发布全部移除；契约见 `docs/guides/TRANSFER_RECEIVER.md`。
- 本收口周期 Cargo.lock 新增社区依赖与许可证（`cargo metadata` 实测）：`a2a-lf 0.4.2` /
  `a2a-pb 0.3.2` / `a2a-server-lf 0.5.3`（Apache-2.0，a2aproject/a2a-rs）、
  `fileloft-axum/fileloft-core 0.3.0`（MIT）、`backon 1.6.0`（Apache-2.0）、`windows 0.62.2`
  （MIT OR Apache-2.0）。

### 11.9 交付状态

- HEAD `fe24fc6`（`feat/agent-integration`）；**实现改动未提交、未 push**。
- 工作树 62 项在途（本轮实测 `git status --porcelain | wc -l`）。
- `AGENTS.md` 本轮在末尾追加「## 2026-11-21 Agent 集成收口（自动追加）」一节（只新增、不改
  既有内容）——§11.7 第 6 条（AGENTS.md 教训回写）就此关闭。
- 对外发布仍 blocked：duthub.exe 签名、goose SBOM/许可清单、发布资产入库三项实际产出缺失，
  属 ADR-3 §6.3 未满足项；release gate 按 ADR-3 设计 fail closed。
