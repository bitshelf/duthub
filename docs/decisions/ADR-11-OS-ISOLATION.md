# ADR-11：每 DUT Agent 目录 ACL 加固与受限令牌能力探测（W2b）

- 状态：Proposed（未经产品签字；权威口径见 `docs/decisions/ADR_SIGNOFF.md` §7——最终措辞待 owner 复核）
- 能力注记：能力已在位——`crates/duthub-core/src/os_acl.rs:78` `harden_private_dir`、
  `crates/duthub-core/src/restart.rs`（`RestartBudget` 1s/2s/4s 退避）均已实现，
  但能力实现 ≠ 产品签字；§2.2 deferred 项仍未实现
- 日期：2026-11-21
- 阶段：W2b（P2 收口）
- 相关：`43c58dc:docs/AGENT_IMPLEMENTATION_TASKS.md` T24/T39；`docs/reviews/AGENT_IMPLEMENTATION_REPORT.md` §5.1；
  `docs/SECURITY.md`
- 代码：`crates/duthub-core/src/os_acl.rs`、`crates/duthub-core/src/restart.rs`、
  `apps/desktop/src/agent_host/{host,session,recovery}.rs`

## 1. 背景

W2a 已把每个 Agent 的 cwd、`GOOSE_PATH_ROOT` 与 `TEMP`/`TMP` 收敛到
`<work_dir>/agent/dut/<hex(dut_id)>/`，并清除了用户目录环境变量。这属于**路径收敛**：
子进程仍可能用绝对路径访问用户目录，因此不是 OS 级强制边界（报告 §5.1 明确列为 deferred）。

本 ADR 收紧其中可低成本实现的一部分，并把「能做什么」如实暴露给 UI：

1. **目录 ACL 加固**：让每 DUT 目录的 DACL 只允许当前用户、SYSTEM 与 Administrators，
   并设为 protected（不继承父目录的 ACE），从而阻止同机其他普通用户读写该目录；
2. **受限令牌能力探测**：确认当前进程能否创建 `WRITE_RESTRICTED` 受限令牌，
   为后续「受限令牌启动 Agent」提供事实依据；
3. **仍不实现**：用受限令牌启动 Agent、专用账户运行 Agent、AppContainer。

## 2. 决定

### 2.1 已实现

| 能力 | 实现 | 失败语义 |
|---|---|---|
| 目录 ACL 加固 | `duthub_core::os_acl::harden_private_dir`：`SetNamedSecurityInfoW(SE_FILE_OBJECT, DACL_SECURITY_INFORMATION \| PROTECTED_DACL_SECURITY_INFORMATION)`，DACL 仅含当前用户、`WinLocalSystemSid`、`WinBuiltinAdministratorsSid` 三个允许 ACE，带 `OBJECT_INHERIT_ACE \| CONTAINER_INHERIT_ACE` | 只在会话 transcript 里写一条系统注记，**不阻断 Agent 启动** |
| 状态探测 | `probe_os_isolation`：读回目录 DACL（protected + 是否含 Everyone/Users）并用 `CreateRestrictedToken` 创建、立即关闭一个受限令牌句柄 | 返回 `private_dir_acl=false` / `restricted_token=false` 与 `detail` 原因，不报错 |
| 状态展示 | `SessionView::set_os_isolation` / `os_isolation()`；host 在 `sandbox::prepare` 后写入 | UI 可读；本轮不新增页面 |
| 非 Windows | `harden_private_dir` 返回 `AclError::Unsupported`；`probe_os_isolation` 返回 `unsupported=true` 的状态 | 不误报为已隔离 |

`CreateRestrictedToken` 探测**只创建令牌并关闭句柄，不调用 `CreateProcess`**；
`restricted_token_probe_never_spawns` 用可执行 canary 断言探测后没有任何进程/文件副作用。

### 2.2 明确不实现（deferred）

- **受限令牌启动 Agent**：只挡写、不挡读；且需要为串口/USB/视频采集重新验证句柄可访问性，
  AppContainer/受限令牌的兼容性属于 T39/二期；
- **专用账户 + 每账户 ACL**：需要管理员配置账户、凭据与串口设备 ACL，不能由应用静默完成；
- **AppContainer**：需要按 capability 重新授权文件/串口/USB，属于二期；
- **子进程降权后的能力矩阵实测**（COM/USB/FFmpeg/DirectShow）。

因此 `OsIsolationStatus.app_container` 恒为 `false`，UI/文档不得把目录 ACL 加固写成
「Agent 已沙箱」；外部 Agent 在未上 OS 隔离前仍按 experimental 处理。

## 3. ACL 细节与边界

- 目标路径：`sandbox::prepare` 返回的 `<work_dir>/agent/dut/<hex(dut_id)>/`，在创建
  `sandbox/`、`goose/` 之前加固；后续创建的文件通过可继承 ACE 继承同一限制；
- 拒绝目标：普通文件、缺失路径、junction/symlink（`symlink_metadata` + `FILE_ATTRIBUTE_REPARSE_POINT`）
  返回 `AclError::InvalidPath`/`Io`，**不会**把文件误报为已加固；
- 只替换 DACL：owner/group/SACL 保持原样；`PROTECTED_DACL_SECURITY_INFORMATION` 会移除
  从父目录继承的 ACE，因此 Everyone/BUILTIN\Users 不再通过继承进入；
- 这是**纵深防御**，不是针对「已以当前用户运行的 Agent」的沙箱：Agent 仍可访问当前用户
  有权访问的其他路径。

## 4. 与重启策略的关系

本波同时实现 Ready 后异常退出的自动重启（`duthub-core::restart::RestartBudget`）：

- 仅当会话至少一次到达 `Ready`、且不是显式 stop、也不是 watchdog/协议 fail-closed 时重启；
- 滑动 10 分钟窗口内最多 3 次，退避 1s/2s/4s（`backon` 仅提供延迟序列，窗口自实现）；
- 每次重启前 `SessionView::reset_for_restart()` 只做 `Stopped -> Starting`；
  `Failed`/`Stopping` 不会被复活；
- 预算耗尽后会话进入 `Failed`，必须人工重新启动；
- 重启前释放上一轮的 listener 与进程树，避免端口/进程残留；
- 每轮 `sandbox::prepare` 后都会重新执行 ACL 加固与能力探测，状态写回 `SessionView`。

启动失败（从未 Ready）不重启；既有
`apps/desktop/tests/goose_lifecycle.rs::failed_session_can_restart_after_work_dir_is_fixed`
仍以 `Failed` 结束，行为不变。

## 5. 验证

- `restart_budget_allows_three_in_ten_minutes_and_denies_fourth`（`duthub-core` 单测）：
  3 次准入、4 次拒绝、窗口滑动后可再次准入；
- `restart_delays_are_one_two_four_seconds_then_exhausted`：退避 1s/2s/4s；
- `apps/desktop/tests/agent_restart.rs::restart_only_after_ready`：fake ACP 未应答
  initialize → 只 spawn 1 次且 `Failed`；Ready 后自退出 → spawn 4 次
  （首启 + 3 次重启）后 `Failed`，每次重启有系统注记；
- `stop_cancels_pending_restart_without_new_spawn`：退避窗口内 stop → `Stopped`，
  不再 spawn；
- `apps/desktop/tests/agent_isolation.rs::acl_hardening_applies_owner_only_dacl`：
  读回 DACL，protected=true，含 SYSTEM/Administrators，无 `S-1-1-0`/`S-1-5-32-545`；
- `harden_failure_reports_unguarded`：文件/缺失路径加固失败且探测不宣称私有；
- `restricted_token_probe_never_spawns`：探测成功且无子进程/文件副作用。

验收命令（隔离 target，不写共享 `target/`）：

```bash
CARGO_TARGET_DIR=C:/Users/luo-j/duthub-impl-target cargo test -p duthub-core -p duthub-desktop --lib --bins --tests
CARGO_TARGET_DIR=C:/Users/luo-j/duthub-impl-target cargo clippy -p duthub-core -p duthub-desktop --all-targets -- -D warnings
cargo fmt --all --check
```

## 6. 风险与后续

| 风险 | 说明 | 缓解 |
|---|---|---|
| 权限不足 | 加固需要目标目录的 WRITE_DAC；受限令牌需要 `TOKEN_DUPLICATE` | 失败只注记；探测如实返回 false |
| 误读为沙箱 | 目录 ACL 不阻止当前用户权限内的读取 | 文档/UI 只写「目录 ACL 已加固」，不写「已隔离」 |
| 设备可访问性 | 未来受限令牌/AppContainer 下串口、USB、DirectShow 可能不可用 | 二期逐项 capability 实测；未实测前不启用降权启动 |
| 继承语义 | 目录级 ACE 通过 CI/OI 继承；已存在的旧文件保持原 ACL | 需要时后续补递归加固；本轮只保证新文件 |
