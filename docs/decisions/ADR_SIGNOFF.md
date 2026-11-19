# ADR 签字记录（ADR_SIGNOFF）

- 仓库：duthub
- 分支：`feat/agent-integration`
- 基线：HEAD `fe24fc6`（提交 Agent 集成设计、文档与在途改动）
- 记录日期：2026-11-21（W0 初签；W4 按 ADR-3 修订重签）
- 记录人：实施者-1（W0）；实施者-4（W4 重签记录）
- 签字依据：用户（仓库所有者）在本轮工作流指令中已拍板的两项决定；本文件是工作流内的
  电子签字记录，不是密码学签名。
- 外部在途口径（2026-11-21，工作流所有者）：`AGENTS.md`、`Cargo.lock`、
  `crates/duthub-device/Cargo.toml`、`crates/duthub-device/tests/com8_reset_boot_live.rs`
  不提交 / 不隔离 / 不格式化 / 不修改；允许 `git status` 非空，不设 git-clean 门。

## 1. 签字人

| 角色 | 姓名 / 身份 | 签字 | 日期 | 依据 |
|---|---|---|---|---|
| 产品负责人 | JZ Loh（仓库所有者） | 已签（电子记录，含 ADR-3 修订版重签） | 2026-11-21 | 本轮指令：内嵌 goose v1.60.2；工作根 CLI > settings；Agent 只用 `ensure_portable_work_dir`；本轮内嵌解压后的 exe，zip 仅来源记录 |
| 维护者 | JZ Loh（仓库所有者兼维护者） | 已签（电子记录，含 ADR-3 修订版重签） | 2026-11-21 | 同上；负责 ADR-3 资产清单、manifest 与发布门禁 |
| 记录人 | 实施者-1（W0）/ 实施者-4（W4） | 记录，不代签 | 2026-11-21 | 本文件 |

说明：姓名取自本仓库 git 配置 `user.name = JZ Loh`。若组织要求独立审批系统或手写签字，
须由仓库所有者在对外发布前补签；补签前 ADR-3 仅对内部实施有效，**不得对外发布 goose 资产**。
若维护者另有其人，需由该维护者补签，不得以本记录代替。

## 2. 已签字 ADR

| ADR | 标题 | 阶段 | 产品签字 | 维护者签字 | 日期 | 状态 |
|---|---|---|---|---|---|---|
| ADR-3 | 内嵌 goose ACP 资产（v1.60.2）与 Apache-2.0 合规 | W0 初签；W4 按 2026-11-21 修订重签；发布门禁见 §6.3 | 已签 | 已签 | 2026-11-21（重签） | 接受；内部实施与构建可用，对外发布待 §6.2 deferred 完成 |
| ADR-WORKDIR | 工作根双解析器并存与 Agent 路径约束 | W0 签字；W3 Agent 接线起生效；T24 收口 | 已签 | 已签 | 2026-11-21 | 接受；T24 维持 deferred，本轮不统一 |

## 3. ADR-3 修订与重签（2026-11-21 W4）

重签对象是 `docs/decisions/ADR-3-GOOSE-BUNDLE.md` 的修订版，修订点：

| 节 | 修订前 | 修订后 |
|---|---|---|
| §4 资产表 | zip 与 exe 并列作为发布资产；`agents/goose/` 旁挂与构建期目录并列 | 发布物 = **内嵌解压后的 exe（20,541,440 B）+ LICENSE + NOTICE + manifest**；zip **仅作来源记录，不入构建**；旁挂 deferred |
| §5 路径/构建 | 构建期放 zip 并在需要旁挂时复制到 `agents/goose/`；未写外部路径的接线现状 | 路径固定 `<work_dir>/agent/bin/goose/1.60.2/<完整 sha256>/goose-acp.exe`；构建只内嵌 exe 与三份声明文件，运行期不解压；外部路径 `resolve(...)` 已实现校验，但**尚未接入启动链**（deferred） |
| §6 发布门 | 五项门禁，未区分本轮可交付与 deferred | 区分 §6.1 本轮可交付（资产/hash、`--version` 由 bundle 测试覆盖、release 旁附 licenses、无自动更新、Authenticode 状态记录在 manifest/发布物）与 §6.2 deferred（SBOM、签名链验证、NotSigned 的应用内 UI 展示、应用内第三方许可页、外部路径接入、旁挂形态）；§6.3 明确第 1/4/5 项已满足、第 2/3 项未完成，本轮**不对外发布** goose 资产 |

资产事实（2026-11-21 实测，未变）：

| 对象 | 字节数 | SHA-256 |
|---|---:|---|
| `goose-acp.exe`（内嵌） | 20,541,440 | `1c1f7c9ccb5bdbb53435b0e843a3e6d2e16f9e2f6dac5dbfcf31f1497ec7e3c5` |
| `LICENSE`（内嵌） | 11,341 | `44459b86c2e96fdbfd8a6b5c33d30d4b04b5293fcb2ec96fe4dcc4e0f90b8962` |
| `goose-acp-x86_64-pc-windows-msvc.zip`（仅来源记录） | 9,832,133 | `536c65404f4a4c0d27cb3dd085d6389e1d2038bb9c2a4c8ee850f62da00c2f95` |
| `NOTICE` | — | 非空；声明版本、来源、Apache-2.0、未修改、版权 |

`--version` 探测已由 `apps/desktop/tests/agent_host.rs` 的 `probe_version_runs_real_goose`
覆盖（真实执行内嵌 exe，期望 `goose-acp 1.60.2`）；Authenticode 仍为 `NotSigned`，状态只记录在
`manifest.json`（release 时旁附到 `dist/licenses/goose/manifest.json`），**应用内 UI 展示
「未签名」尚未实现**（ADR-3 §6.2 deferred）；签名链验证同样属于 deferred。

## 4. 门禁实测

### 4.1 W0 初签记录（2026-11-21，保留原文结论）

| # | 命令 | 结果 |
|---|---|---|
| 1 | `test -z "$(git status --porcelain)"` | **失败**。4 个外部在途路径（本波开始前已存在，非本波产生，未触碰）；本波新增 `?? docs/decisions/`。按所有者口径不提交 / 不隔离。 |
| 2 | `C:/Users/luo-j/.cargo/bin/cargo.exe fmt --all --check` | **失败（EXIT=1）**。仅 `crates/duthub-device/tests/com8_reset_boot_live.rs:82`、`:91` 两处 Diff；该文件是外部在途测试，W0 未修改、未格式化。 |
| 3 | `test -f docs/decisions/ADR_SIGNOFF.md` | EXIT=0（本文件存在）。 |
| 4 | `grep -n "ADR-3\|ADR-WORKDIR" docs/decisions/ADR_SIGNOFF.md` | EXIT=0（覆盖第 1–5 节）。 |

### 4.2 W4 复测（2026-11-21，本轮实际执行）

| # | 命令 | 结果 |
|---|---|---|
| 1 | `CARGO_TARGET_DIR=C:/Users/luo-j/duthub-impl-target cargo clippy --workspace --all-targets -- -D warnings` | EXIT=0（`Finished dev profile ... in 4.81s`）。 |
| 2 | `CARGO_TARGET_DIR=C:/Users/luo-j/duthub-impl-target cargo test --workspace --quiet` | EXIT=0；39 个测试二进制，436 passed / 0 failed / 19 ignored。 |
| 3 | `CARGO_TARGET_DIR=C:/Users/luo-j/duthub-impl-target cargo run -p duthub-desktop -- --self-test` | EXIT=0；`SELF-TEST OK: 2–3 frames · 0 core lines · duthub-desktop v0.1.1 (gpui-kit)`（两次实测 2 / 3 帧）。 |
| 4 | `CARGO_TARGET_DIR=C:/Users/luo-j/duthub-impl-target bash scripts/regression` | EXIT=0；四步全部通过，末尾 `REGRESSION OK`。 |
| 5 | `/c/Users/luo-j/.cargo/bin/cargo.exe fmt --all --check` | EXIT=0（无 Diff）。 |
| 6 | `git diff Cargo.lock \| sha256sum` | `40d9ef00e940c92f1521275f154166a87d2e3c380d9fd0044000298a60cec7fa`，与开工 baseline 一致。 |
| 7 | `df -h /c` / `du -sh C:/Users/luo-j/duthub-impl-target` | 开工可用 67G；收尾可用 66G；复用隔离 target（17G → 18G），未新建 target、未写共享 `target/`。 |
| 8 | release.yml 资产断言脚本（`pwsh -NoProfile` 原样执行） | EXIT=0；`asset ok` × 3 + `VERIFY OK`（exe/zip/LICENSE 字节数与 SHA-256 匹配 manifest）。 |

### 4.3 外部在途改动（非本波产生，未触碰）

| 路径 | 状态 | 本波动作 |
|---|---|---|
| `AGENTS.md` | 外部 tracked 修改（COM8 / COM25 说明） | 未触碰 |
| `Cargo.lock` | 外部 tracked 修改（duthub-control dev 依赖） | 未触碰；diff 哈希与 baseline 一致 |
| `crates/duthub-device/Cargo.toml` | 外部 tracked 修改（duthub-control dev 依赖） | 未触碰 |
| `crates/duthub-device/tests/com8_reset_boot_live.rs` | 外部未跟踪测试 | 未修改、未格式化 |
| `docs/decisions/` | ADR 交付物；`ADR-3` 经所有者授权在本轮修订 | 新增/修订未跟踪；按所有者口径不提交 / 不隔离 |

**com8 格式时间线（消除矛盾）**：该文件的两处 rustfmt 差异是在 2026-11-21 更早一轮的
「格式门禁修复」波次中修正的（仅格式，无断言改动）；W0 记录中的「未修改、未格式化」指
W0 波内动作，与更早修复波不矛盾；本轮 W4 未再触碰该文件，`cargo fmt --all --check` 为
EXIT=0。

## 5. 结论

- **ADR-3 已按 2026-11-21 修订重签；ADR-WORKDIR 签字继续有效。** 重签后允许内部实施与
  构建验证；**对外发布 goose 资产仍需完成 ADR-3 §6.2 的 deferred 项**（SBOM、Authenticode
  链验证、NotSigned 的应用内 UI 展示、应用内第三方许可页、外部路径接入启动链、旁挂形态）。
- **W4 全量门禁实测全部 EXIT=0**（clippy / test / self-test / regression / fmt），
  `Cargo.lock` 未被改动。门禁不再被外部 com8 格式差异阻塞；工作区非空来自外部在途路径与
  本波未提交交付物，属预期。
- **未做**：未提交、未 push；未修改 / 格式化 4 个外部路径；未运行 19 个 ignored 测试
  （真实模型、真机 COM8/COM25、FFmpeg 采集、外部设备依赖）；未生成 SBOM；未做 Authenticode
  链验证；未实现应用内 NotSigned 展示与第三方许可页；外部 goose 路径未接入启动链。

## 6. 后续 ADR 状态

本文件只记录 ADR-3 与 ADR-WORKDIR。ADR 编号分两组，勿混用：

- **旧 v5 长周期计划（T01/P0）**：ADR-0/2/5/6/7/8/9/10 属于该轮计划，本轮未签字，
  已被 W4 的 supersession 块取代；
- **现行 `docs/decisions/` ADR**：ADR-2-A2A-LAN、ADR-8-AGENT-CARD、ADR-9-STATEDB、
  ADR-11-OS-ISOLATION 为 Proposed（见 §7），均未经产品签字。

不得以本文件的 ADR-3 / ADR-WORKDIR 签字推断任何其他 ADR 已签。ADR-3 的发布门禁见
`docs/decisions/ADR-3-GOOSE-BUNDLE.md` §6；ADR-WORKDIR 的 T24 收口条件见
`docs/decisions/ADR-WORKDIR.md` §2。

## 7. 2026-11-21 收口轮记录（无新签字）

- **本轮只更新实现状态，无新签字**：ADR-3 §6.2/§6.3 与本文件的修订仅把「已实现」与
  「仍 deferred」对齐到代码事实（NotSigned 应用内徽标、应用内第三方许可页、外部 goose
  接入启动链已实现；SBOM、签名链验证仍 deferred）。这不构成对 §6.3 任何未满足项的豁免，
  也不改变签字状态。
- **ADR-2/8/9/11 为 Proposed**：A2A 监听绑定拓扑（LAN/TLS 延后签字，ADR-2）、
  Agent Card 内容最小化与发现面（A2A v1.0 规范 §8，ADR-8）、A2A 任务状态库
  （W3 仅内存；持久化 StateDB 为 ADR-9 §2.2 明确 deferred，并非把状态库迁入 core）、
  OS 隔离（ADR-11）均未经产品签字；`crates/duthub-a2a` 的现有回环实验面按「未经签字不得进产品验收」
  口径对待，对外发布与产品承诺均不得引用。
- **对外发布仍 blocked**：ADR-3 §6.3 的 SBOM 与 Authenticode 签名链验证未完成；
  release gate 机器门已建（release.yml gate job + `scripts/release-gate.ps1`）但当前因缺
  duthub 代码签名证书与 goose 上游 Cargo.lock 按设计失败。在 owner 补齐或书面豁免前，
  不得发布 goose 资产与正式 release。
