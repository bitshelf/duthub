# ADR-3：内嵌 goose ACP 资产（v1.60.2）与 Apache-2.0 合规

> **2026-11-25 状态变更：本 ADR 的「内嵌」部分已被取代（Superseded）。**
> 用户拍板 goose 改为与 ffmpeg / adb 相同的下载分发：`apps/desktop/assets/goose/`
> 全部删除，`agent_host/bundle.rs` 不再 `include_bytes!`/`include_str!`，运行时按
> 「显式外部路径（含 sha256 校验）> 已下载安装（`<config>/tools/goose/`，`duthub-tooldown`
> 内核 manifest）> PATH 裸名」解析，未安装经 `registry::goose_install_state()` 上报。
> 仍在生效的部分：不改 goose 源码、不自动下载/不自动更新（由用户在「工具下载」页触发）、
> 版本门槛 >= 1.60.2、不宣称签名链验证。下文关于内嵌资产、zip/exe 字节数与 SBOM 的条款
> 只作为历史记录保留；`scripts/release-gate.ps1` 与 `.github/workflows/release.yml` 的内嵌
> 资产断言已按新口径改写（2026-11-25 发布门禁同步轮），新发布契约见 §9。

- 状态：~~已接受（Accepted）~~ → 已被「工具下载分发」取代（2026-11-25，见 §9）；2026-11-21 按
  「内嵌解压后的 exe、zip 仅来源记录」修订并重签，签字见 `docs/decisions/ADR_SIGNOFF.md`
- 日期：2026-11-21（初版）；2026-11-21 修订（W4）
- 阶段：W0（P0 前置门）签字；本轮 W2 起内嵌资产合入（内嵌已于 2026-11-25 取消）；对外
  发布门禁见 §9（SBOM 与 Authenticode 链验证未完成前不对外发布）
- 取代：`docs/reviews/AGENT_INTEGRATION_BEST_PRACTICE.md` §4.7 的 ADR-3 行、§9/P7 与
  `43c58dc:docs/AGENT_IMPLEMENTATION_TASKS.md` T50 中「可选捆绑、默认不捆绑、仅引导安装」的旧口径；
  「不自动下载 / 不自动更新 / `duthub.exe` 单文件分发」的约束继续有效。
- 相关：`43c58dc:docs/GOOSE_AGENT_FEASIBILITY.md` §2、§3.6；T50、T26、T24

## 1. 背景与用户决定

用户（仓库所有者）于 2026-11-21 拍板：

> 内嵌 goose 二进制：使用上游官方 lean goose-acp（v1.60.2 Windows x64，约 9.4MB zip，
> Apache-2.0），不改 goose 源码；duthub.exe 内嵌，首次解包到 duthub 管理的版本化目录，
> SHA-256 校验，支持用户外部路径覆盖以独立升级。

本 ADR 把该决定固化为可验收的发布契约。这里的「内嵌」指 **随 duthub 发布物携带 ACP
sidecar 资产，并在首次使用时解包为独立进程**，不是把 goose 作为 Rust 库链接进
`duthub.exe`。GDK / oi-kit 库嵌入仍按 `43c58dc:docs/GOOSE_AGENT_FEASIBILITY.md` §3.6 的结论不采用
（alpha、改名中、API 不稳定）；本 ADR 不改变该结论。

## 2. 来源与版本

| 项 | 值 |
|---|---|
| 上游仓库 | `aaif-goose/goose`（原 `block/goose`） |
| 版本 | `v1.60.2`，release 发布时间 `2026-11-20T06:18:55Z` |
| Release 页 | <https://github.com/aaif-goose/goose/releases/tag/v1.60.2> |
| Windows x64 资产 | `goose-acp-x86_64-pc-windows-msvc.zip` |
| 下载 URL | <https://github.com/aaif-goose/goose/releases/download/v1.60.2/goose-acp-x86_64-pc-windows-msvc.zip> |
| ACP 入口 | `goose-acp.exe`（stdio ACP server；无子命令；支持 `--with-builtin <NAME,...>`） |

## 3. 双哈希与资产事实（2026-11-21 实测）

| 对象 | 字节数 | SHA-256 |
|---|---:|---|
| zip | 9,832,133 | `536c65404f4a4c0d27cb3dd085d6389e1d2038bb9c2a4c8ee850f62da00c2f95` |
| zip 内 `goose-acp.exe` | 20,541,440 | `1c1f7c9ccb5bdbb53435b0e843a3e6d2e16f9e2f6dac5dbfcf31f1497ec7e3c5` |

核验方式与边界：

- GitHub release API 的 asset `digest` 与本机 `sha256sum` 对 zip 的结果一致；
- `unzip -l` 显示 zip **只有** `goose-acp.exe`，不含 LICENSE/NOTICE/manifest；
- 内层 exe 哈希由
  `unzip -p goose-acp-x86_64-pc-windows-msvc.zip goose-acp.exe | sha256sum` 计算；
- Authenticode：2026-11-21 在 Windows 10.0.26300 上以 `Get-AuthenticodeSignature` 复核，
  结果为 `NotSigned`（`Status = 2`，`SignerCertificate = null`）；`manifest.json` 的
  `authenticode` 段记录该结果，release 时另旁附到 `dist/licenses/goose/manifest.json`。
  **应用内 UI 展示「未签名」已实现（2026-11-21 收口轮）**：Agent 面板/会话渲染
  「未签名（仅 SHA-256 校验）」徽标（`apps/desktop/src/agent_ui.rs:846` `EmbeddedNotSigned`）；
  不得把该二进制静默当作可信；签名链验证仍属 §6.2 deferred；
- 版本字符串：以 release tag `v1.60.2` 为准；二进制内嵌 usage 字符串实测为
  `Usage: goose-acp [--with-builtin <NAME,...>]`（2026-11-21 用 `rg -a` 读取），说明 ACP
  入口无 `acp` 子命令；2026-11-21 实跑 `goose-acp.exe --version` 输出 `goose-acp 1.60.2`
  （退出码 0），已记入 `manifest.json.versionProbe`。

## 4. Apache-2.0 资产清单

发布物内嵌**解压后的 exe（20,541,440 B）+ LICENSE + NOTICE + manifest.json**；构建期统一放
`apps/desktop/assets/goose/`，`duthub.exe` 用 `include_bytes!` / `include_str!` 内嵌 exe 与
三份声明文件，客户仍只分发单个 `duthub.exe`。zip **仅作来源记录**保留在资产目录，不进入构建、
不在运行期解压；`agents/goose/` 旁挂交付形态本轮不做（见 §6 deferred）。

| 文件 | 来源 / 责任 | 本轮实测或状态 |
|---|---|---|
| `goose-acp.exe` | 从官方 zip 解出后入库；构建期 `include_bytes!` 内嵌 | 20,541,440 B；sha256 见 §3；bundle 测试校验内嵌字节数与完整 SHA-256 |
| `LICENSE` | 上游 `v1.60.2` 仓库根 `LICENSE`（Apache-2.0 全文） | 11,341 B；sha256 `44459b86c2e96fdbfd8a6b5c33d30d4b04b5293fcb2ec96fe4dcc4e0f90b8962`；内嵌并可由应用读取 |
| `NOTICE` | duthub 自拟；上游 `v1.60.2` tag 全树无 NOTICE（GitHub trees API recursive 列表 2026-11-21 未见） | 已落盘并内嵌：声明 `goose-acp v1.60.2`、Apache-2.0、来源 URL、未修改、版权归 AAIF/goose 贡献者 |
| `manifest.json` | duthub 维护（字段见下） | 已落盘并内嵌；含 zip 与 exe 双哈希、LICENSE 哈希、`--version` 探测与 Authenticode 记录 |
| `goose-acp-x86_64-pc-windows-msvc.zip` | 上游 release，未修改 | 9,832,133 B；sha256 见 §3；**仅来源记录，不入构建、不作为发布物** |

`LICENSE` 取自上游 tag 的
<https://raw.githubusercontent.com/aaif-goose/goose/v1.60.2/LICENSE>（2026-11-21 拉取，
11,341 B），不是从 zip 内取得。

`manifest.json` 最小字段：

```json
{
  "id": "goose-acp",
  "version": "1.60.2",
  "target": "x86_64-pc-windows-msvc",
  "source": "https://github.com/aaif-goose/goose/releases/tag/v1.60.2",
  "archive": {
    "name": "goose-acp-x86_64-pc-windows-msvc.zip",
    "bytes": 9832133,
    "sha256": "536c65404f4a4c0d27cb3dd085d6389e1d2038bb9c2a4c8ee850f62da00c2f95"
  },
  "executable": {
    "name": "goose-acp.exe",
    "bytes": 20541440,
    "sha256": "1c1f7c9ccb5bdbb53435b0e843a3e6d2e16f9e2f6dac5dbfcf31f1497ec7e3c5"
  },
  "command": "goose-acp.exe",
  "args": [],
  "license": {
    "name": "LICENSE",
    "bytes": 11341,
    "sha256": "44459b86c2e96fdbfd8a6b5c33d30d4b04b5293fcb2ec96fe4dcc4e0f90b8962"
  },
  "notice": "NOTICE",
  "versionProbe": {
    "command": ["--version"],
    "expected": "goose-acp 1.60.2",
    "checkedAt": "2026-11-21"
  },
  "authenticode": {
    "status": "NotSigned",
    "statusCode": 2,
    "checkedAt": "2026-11-21",
    "method": "Get-AuthenticodeSignature (PowerShell 7, Windows 10.0.26300)",
    "signer": null
  }
}
```

合规义务（Apache-2.0）：

- 随发布物提供 `LICENSE` 全文，保留版权、专利、商标与免责声明；本轮实现为「内嵌 + release
  旁附 `dist/licenses/goose/`」；单文件客户需要的应用内第三方许可入口**已实现
  （2026-11-21 收口轮）**：设置页 `ConfigPage::Licenses` 展示同一份声明（见 §6.2）；
- 上游有 `NOTICE` 时必须原样保留；当前上游 tag 全树无 NOTICE，duthub 仍提供自拟 NOTICE，
  避免「无声明」的歧义；
- 不修改 goose 源码；如未来打补丁，必须在修改文件加显著变更说明，并重新计算 zip/exe
  哈希、更新 manifest、重新签字；
- 二进制内含的第三方 crate 的许可 / NOTICE 义务，需在发布前按上游 `Cargo.lock` 生成第三方
  许可清单或 SBOM；本轮未生成，列为发布门禁（见 §6）；
- `duthub.exe` 仍按单文件分发；goose 资产不改变 duthub 自身许可证。

## 5. 运行期布局与校验

- 构建期：`apps/desktop/assets/goose/` 放 zip（仅来源记录）、解出的 `goose-acp.exe`、
  `LICENSE`、`NOTICE`、`manifest.json`；构建只内嵌 exe 与三份声明文件，不内嵌 zip，
  **运行期不解压**；
- 首次使用：把构建期内嵌的 exe 落到
  `<work_dir>/agent/bin/goose/1.60.2/<完整 sha256>/goose-acp.exe`（版本化 + 内容寻址
  目录，避免覆盖正在运行的 exe）；`<work_dir>` 由 `ensure_portable_work_dir` 解析（见
  ADR-WORKDIR）。内嵌与落盘的都是**已解出的 exe**，zip 不参与运行期；
- 校验：写入前校验内嵌 exe 的字节数与 sha256，写入后再次复算；每次 `resolve` 都重新复算
  完整 sha256，任一不匹配 fail closed，不执行、不回退到 PATH；写入使用同目录临时文件 +
  原子 rename；
- 外部路径覆盖：`apps/desktop/src/agent_host/bundle.rs` 的
  `resolve(work_dir, Some((path, sha256)))` 会在执行前复算完整 SHA-256，不匹配拒绝执行；
  外部路径**已接入启动链**（2026-11-21 收口轮：`agents.json` + `agent_host/registry.rs`，
  Host 按 DUT `agent_id` 经 `registry::resolve` 启动，外部 goose 版本 ≥ 1.60.2）；
  真实外部 goose 二进制未实测；
- 不自动下载、不自动更新、不静默替换；版本升级必须更新 manifest 与两份哈希并重新签字。

## 6. 发布与签字责任

| 角色 | 责任 |
|---|---|
| 产品负责人 | 批准「内嵌 goose」与对外分发；在 `ADR_SIGNOFF.md` 签字；决定随包内嵌还是仅旁挂（本轮裁决：内嵌；旁挂 deferred） |
| 维护者 | 维护 manifest / 哈希 / LICENSE / NOTICE；每次发布前复核 exe 与 LICENSE 的字节数/SHA-256、`--version` 探测、Authenticode 状态；生成第三方许可清单或 SBOM（deferred）；缺任一必需资产即拒绝发布 |
| 打包 / 发布执行人 | 按 manifest 放置资产；release.yml 构建前断言资产存在且哈希匹配，构建后把 LICENSE/NOTICE/manifest 另存到 `dist/licenses/goose/`；运行 bundle 测试与 `--self-test`；在 release notes 记录版本与哈希 |
| 安全评审 | 复核 hash pin、无自动更新、外部路径信任流程；确认 Authenticode 状态（NotSigned）在 manifest/发布物与应用内 UI（「未签名（仅 SHA-256 校验）」徽标）可见；签名链验证仍 deferred |

### 6.1 本轮已交付/已记录项（2026-11-21 实测）

1. 内嵌资产齐全：`goose-acp.exe` 20,541,440 B / `1c1f7c9c…`；`LICENSE` 11,341 B /
   `44459b86…`；`NOTICE` 非空；`manifest.json` 字段完整。
   `manifest_matches_checked_in_zip_and_exe_assets` 覆盖 manifest ↔ 签入 zip/exe/LICENSE 的
   交叉校验；`install_resolve_and_recompute_hash` 覆盖安装目录、每次复算与篡改后 fail closed。
2. `--version` 探测已由 `probe_version_runs_real_goose` 覆盖（真实执行内嵌 exe，期望
   `goose-acp 1.60.2`），不是仅记录字段。
3. `duthub.exe` 仍单文件分发；release.yml 构建前按 manifest 断言 exe/zip/LICENSE 的字节数与
   SHA-256，构建后把 `LICENSE`/`NOTICE`/`manifest.json` 另存到 `dist/licenses/goose/`。
4. 无自动下载 / 自动更新 / 静默替换。
5. Authenticode 状态已记录为 `NotSigned`（`manifest.json` 内嵌；release 时旁附到 `dist/licenses/goose/manifest.json`）；**应用内 UI 明示已实现（2026-11-21 收口轮）**：Agent 面板/会话渲染「未签名（仅 SHA-256 校验）」徽标（`agent_ui.rs:846`）。

### 6.2 deferred（对外发布前仍需完成）

1. 第三方 crate 许可清单或 SBOM（按上游 `Cargo.lock` 生成）。**仍未完成**：缺 goose 上游
   Cargo.lock，`scripts/release-gate.ps1` 以明确原因失败，不伪造 SBOM。
2. Authenticode 签名链验证（当前只有 `Get-AuthenticodeSignature` 的状态记录，未验证签名者/链）。
   **仍未完成**；且 duthub.exe 自身无代码签名证书，release gate 在 Authenticode 步按设计失败。
3. ~~应用内展示 Authenticode 状态（`NotSigned`）与「未签名」提示~~ **已实现
   （2026-11-21 收口轮）**：`agent_ui.rs` `EmbeddedNotSigned` 徽标；签名链验证仍按第 2 项 deferred。
4. ~~应用内「第三方许可」页~~ **已实现（2026-11-21 收口轮）**：设置页 `ConfigPage::Licenses`
   展示 duthub 与内嵌 goose 的 LICENSE/NOTICE/manifest。
5. ~~外部 goose 路径接入启动链~~ **已实现（2026-11-21 收口轮）**：`agents.json` +
   `agent_host/registry.rs`，Host 经 `registry::resolve` 启动（外部 goose 版本 ≥ 1.60.2）；
   真实外部 goose 二进制未实测。
6. `agents/goose/` 旁挂交付形态（本轮改为内嵌解压，不做旁挂）。**维持 deferred。**

### 6.3 发布门禁

> **2026-11-25 起本节被 §9 取代**：内嵌资产已删除，发布门禁改为「不得内嵌 + 下载分发契约」
> 断言；下面 1–5 条只保留为历史记录，不再是发布门禁的验收依据。

全部满足才可对外。第 1、4、5 项已满足，第 3 项的应用内 UI 部分已实现（签名链验证部分
仍未完成）；第 2 项（SBOM）与签名链验证未完成，故**本轮不对外发布 goose 资产**，只允许
内部实施与构建验证。release.yml 已建机器门（gate job：回归/SBOM/audit/Authenticode/双哈希
+ `RELEASE_GATE=true` 仓库变量与 release environment 门），当前因缺签名证书与 SBOM 输入
按设计失败（见实施报告 §11.7）：

1. exe 与 LICENSE 的字节数/SHA-256 匹配 manifest，manifest 与签入文件一致；
2. LICENSE / NOTICE / manifest 齐全；第三方许可清单或 SBOM 已生成，或有书面记录的豁免理由
   （当前：deferred，无豁免记录）；
3. `--version` 探测通过（bundle 测试覆盖）；Authenticode 状态已记录在 `manifest.json`
   （release 时旁附）；应用内 UI 显示「未签名」已实现；签名链验证**仍未完成**（deferred）；
4. `duthub.exe` 单文件分发不变；无自动下载 / 自动更新；
5. ADR-3 与 ADR-WORKDIR 已签字且为 2026-11-21 修订版；未重签则内嵌资产不得对外发布。

## 7. 不做什么

- 不把 goose 作为 Rust 库（GDK / oi-kit）链接进桌面进程；
- 不修改 goose 源码，不 fork；
- 不自动下载、不自动更新、不把用户 Token 写入 goose 配置或日志；
- 不把 goose 的 `load_skill` 等只读本机工具宣称为「已沙箱」；UI 只写工具面 13 个 Plan
  工具（只读）与本机工具未隔离；
- 不承诺未实测的 Defender / EDR 行为与冷启动数据。

## 8. 参考

- `docs/reviews/AGENT_INTEGRATION_BEST_PRACTICE.md` §2（goose 资产与版本）、§4.7（ADR 表）、§9/P7；
- `43c58dc:docs/AGENT_IMPLEMENTATION_TASKS.md` T50、T26、T24；
- `43c58dc:docs/GOOSE_AGENT_FEASIBILITY.md` §2、§3.6；
- 上游 release：<https://github.com/aaif-goose/goose/releases/tag/v1.60.2>；
- Apache License 2.0：<https://www.apache.org/licenses/LICENSE-2.0>。

## 9. 2026-11-25 改为下载分发（取代 §2–§6.3 的资产口径）

**状态：已实施。** 用户于 2026-11-25 拍板 goose 与 ffmpeg / adb 一样改为按需下载安装；
本节取代 §2–§6.3 中「内嵌 goose-acp / zip / exe / manifest 资产」的全部发布契约。
§3、§4、§5、§6.1–§6.3 只作为历史记录保留，不再是发布门禁的验收依据。

### 9.1 现状（代码事实）

- 仓库与构建不再携带任何 goose 字节：`apps/desktop/assets/goose/` 已删除，
  `apps/desktop/src/agent_host/bundle.rs` 不再 `include_bytes!`/`include_str!`。
  Rust 契约测试 `apps/desktop/tests/agent_host.rs::embedded_goose_assets_are_gone`
  锁定「目录不存在 + 源码不再内嵌」；发布门另有独立复验（§9.3 第 1 条）。
- 分发方式：用户在「工具下载」页触发 `duthub-tooldown`。spec 在
  `crates/duthub-tooldown/src/spec.rs` 的 `TOOL_GOOSE`：
  URL `https://github.com/aaif-goose/goose/releases/latest/download/goose-x86_64-pc-windows-msvc.zip`，
  安装到 `<config dir>/tools/goose/`，从 zip 中取 `goose.exe`。
- **没有固定 pin（刻意）**：滚动 `latest` 资产每次上游发布都会改变字节，pin 会让每次
  更新都以 `ChecksumMismatch` 失败（`install.rs` 的 pin 强制仍然保留，未来重新 pin 时
  自动生效）。安装内核把实测 SHA-256 写进安装 manifest，并标
  `checksumVerified: false`（未验证下载，如实记录而不是假装已校验）；
  `agent_host/registry.rs` 在启动前按该记录复算已安装 `goose.exe` 的字节，不符则落空到
  PATH（`verify_downloaded_goose`），三者都没有时 `goose_install_state()` 返回
  `NotInstalled`，UI 引导去「工具下载」页。
- 版本门槛 >= 1.60.2（`bundle::GOOSE_VERSION`，与 `registry::MIN_GOOSE_VERSION` 同源）；
  无自动下载、无自动更新、无静默替换。
- duthub 不验证签名链；对未内置的程序不宣称 `NotSigned`——`bundle::authenticode_status()`
  返回「未内置（随上游发行包）」。

### 9.2 许可、来源与合规

duthub 不再再分发 goose，Apache-2.0 的再分发义务随内嵌资产一并消失；但来源、许可、
版本门槛与校验口径必须继续随发布物可见：

| 载体 | 内容 | 维护位置 |
|---|---|---|
| 根 `NOTICE` | goose 段改为「不捆绑 / 不复制 / 从上游下载」的事实陈述 + 来源 URL + Apache-2.0 + `checksumVerified: false` 语义 | 仓库根，随 `dist/licenses/duthub/NOTICE` 发布 |
| `goose-distribution.md` | 机器可核对的来源说明：下载 URL、许可、版本门槛、安装位置、未固定校验与运行时复算 | `apps/desktop/assets/goose-distribution.md`，发布时旁附为 `dist/licenses/goose/PROVENANCE.md` |
| 应用内「第三方许可」页 | 与上面同一事实（`bundle::license_text()/notice_text()/manifest_json()`） | `apps/desktop/src/pages.rs`（只读，不在本 ADR 修改范围） |

上游 release 包自带的 LICENSE/NOTICE 随用户安装的那份程序提供；duthub 不内嵌、不转发
其全文，也不再在发布物里放一份「内嵌 goose 的 LICENSE/NOTICE/manifest」——那会与
「不捆绑」的事实矛盾。

### 9.3 发布门禁新口径（fail closed）

`scripts/release-gate.ps1` 与 `.github/workflows/release.yml` 已按下列断言改写；每条失败
都 `exit 1`，没有 warn-only 分支：

1. **不得内嵌**：`apps/desktop/assets/goose/` 及其旧文件名（exe/zip/LICENSE/NOTICE/
   manifest.json/Cargo.lock）不得存在；`apps/desktop/assets` 下不得有 `goose*.exe`；
   `bundle.rs` 不得含 `include_bytes!`/`include_str!`/`assets/goose`；`dist/` 不得出现
   goose 可执行文件或 zip。防的是内嵌资产悄悄回归，绕过下载安装的记录与复算链路。
2. **下载分发契约**：`TOOL_GOOSE` 必须存在并登记在 `builtin_specs()`；URL 必须是
   `https://github.com/aaif-goose/goose/releases/.../goose-x86_64-pc-windows-msvc.zip`；
   `pinned_sha256` 必须显式写成 `Some("<64 hex>")` 或 `None`，且 spec 内必须有 64 位
   sha256 记录（固定 pin 或审计备注）；`None` 时必须写明「滚动资产不固定」。
   安装内核必须保留 `ChecksumMismatch` 强制与
   `checksum_verified: spec.pinned_sha256.is_some()`；`registry.rs` 必须保留
   `verify_downloaded_goose` 与 `goose_install_state`。防的是来源漂移、pin 失效与
   「未验证却记录为已验证」。
3. **许可与来源说明**：根 `NOTICE`、`goose-distribution.md`、本 ADR 的本章节必须存在且
   含来源 URL / Apache-2.0 / 不捆绑 / `checksumVerified: false` / 版本门槛；发布物旁附的
   `dist/licenses/goose/PROVENANCE.md` 必须与仓库源文件逐字节一致。防的是「删除说明
   让门禁变绿」。
4. **duthub.exe**：Authenticode 必须 `Valid`（与旧口径一致，未变）。
5. **SBOM / audit**：duthub 的 CycloneDX 1.7 SBOM 与 cargo-audit JSON/退出码检查不变。
   goose 运行时依赖审计降级为**显式未验证占位**：workflow 写
   `dist/sbom/goose-audit-status.txt`（以 `UNVERIFIED` 开头），gate 断言该文件存在；若
   owner 另行提供上游 Cargo.lock 的审计结果，则必须干净且退出码 0。若内嵌资产回归
   （`apps/desktop/assets/goose/Cargo.lock` 出现），workflow 立即回到旧的 fail-closed
   上游 Cargo.lock 审计。
6. **SHA256SUMS**：覆盖 `duthub.exe`、`licenses/duthub/{LICENSE,NOTICE}`、
   `licenses/goose/PROVENANCE.md`、`sbom/duthub.cdx.json`、`sbom/audit.json`、
   `sbom/audit-exit.txt`、`sbom/goose-audit-status.txt`。不再覆盖已不存在的 goose
   LICENSE/NOTICE/manifest。
7. **发布说明**：只声明 `duthub.exe` 的哈希与签名状态；goose 段写明未捆绑、来源 URL、
   Apache-2.0、版本门槛、未固定校验（`checksumVerified: false`）与运行时复算，不得再写
   goose 的 SHA-256 或 `NotSigned`。

### 9.4 仍未完成 / 需人工确认

- `goose.exe` 是否直接以 ACP 模式启动尚未实测（AGENTS.md 坑 #109 的待核对项：下载安装
  的是通用 `goose.exe`，ACP 入口按文档是 `goose acp` 子命令）。发布说明不得声称已用
  下载版跑通 ACP。
- `duthub.exe` 仍无代码签名证书，Authenticode 门禁按设计失败（继承 §6.2 第 2 项）。
- goose 运行时依赖的 SBOM/许可清单由上游 release 负责；duthub 不做机器校验，本门禁的
  占位明确标注 `UNVERIFIED`（不再伪造上游 SBOM）。
- 应用内「第三方许可」页的文案由 `bundle.rs` 提供（本轮未改 Rust 源码）；发布门只校验
  它的分发记录字段 `"embedded": false`，不校验 UI 渲染。
