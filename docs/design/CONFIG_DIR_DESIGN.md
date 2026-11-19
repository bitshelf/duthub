# 配置目录迁移设计：`.duthub/` + 移除配置导出

日期：2026-11-20 · 状态：已实施（原「设计稿（待评审后实施）」；`config_dir_in` 已落地为 exe 同级 `.duthub/`，`initialize_config_dir` 逐文件补齐在位——store.rs 实测；§3 stdio 候选与实现存在偏差，见该节 2026-11-23 注记）

## 0. 需求原话与目标

> 配置文件也别导出了，如果 .duthub 不存在，就在 .duthub/ 创建默认的配置文件，如果存在，就使用里面的配置文件。

拆解为两条可实施的变更：

1. **移除配置文件导出**：设置页「配置文件」面板不再提供 settings.json / duts.json 的「导出」按钮（不再往 `<工作目录>/exports/` 复制副本）。
2. **配置引导改为 `.duthub/` 目录**：配置目录从 exe 同级 `duthub-data/` 迁移到 exe 同级 `.duthub/`；目录不存在 → 创建并写入默认配置文件；已存在 → 直接使用里面的配置文件。

与既有约束的关系：`43c58dc:docs/agent/TASKS.md:97` 记录的旧约束是「只写项目目录，**不写 ~/.duthub**（用户主目录）」。本设计把 `.duthub/` 锚定在 **exe 同级**（便携目录内），不触碰用户主目录，与旧约束一致。

## 1. 现状盘点（代码事实）

### 1.1 配置目录与加载

| 事实 | 位置 |
| --- | --- |
| 配置目录 = `config_dir_in(exe_dir)` = exe 同级 `duthub-data/`（与默认工作目录**同根混用**） | `crates/duthub-core/src/store.rs:23-35`（`config_dir_in` 直接别名 `default_work_dir_in`） |
| `load_settings`：文件缺失 → 返回内存 `default_settings()`，**不落盘**；损坏 → 改名 `settings.corrupt-<ts>.json` 备份后返回错误（不覆盖原文件） | `store.rs:186-216` |
| `load_duts`：文件缺失 → `Ok(None)`；损坏 → 改名备份后返回错误（调用方回退演示数据） | `store.rs:328-353`；AGENTS.md #32 |
| 缺省回退的演示数据源 | `apps/desktop/src/data.rs:149` `DutsState::demo()` |
| 出厂默认 settings（dark / zh-CN / windows 等） | `store.rs:153-184` `default_settings()` |
| 桌面 bootstrap：配置只加载一次（损坏备份有副作用，禁止读两遍），随后解析工作目录 | `apps/desktop/src/main.rs:61-103`（AGENTS.md 坑 #9） |
| 工作目录解析：CLI `--workdir` < settings `workspaceDir` < 默认 exe 同级 `duthub-data/`；`ensure_portable_work_dir` 在根下创建 `duthub-data` 子目录并校验 | `store.rs:39-89`、`main.rs:86-103` |
| 目录名安全校验经 `name_component` → `log_archive::dut_folder`（拒绝非法文件夹名） | `crates/duthub-core/src/dut_artifacts.rs:24-32`、`store.rs:67-89` |

### 1.2 设置页「配置文件」面板（导出入口）

- 面板：`apps/desktop/src/pages.rs:3219-3244`（标题 `configFiles`，副标题 `configExportHint`，两行：settings.json / duts.json）。
- 每行三个按钮：编辑JSON（toast 提示）、**导入**（`import_config`，保留）、**导出**（`export_config`，本次移除）。
- `export_config`（`apps/desktop/src/workspace.rs:1698-1715`）：序列化 settings 或 duts 写入 `<工作目录>/exports/<base>.json`。
- 其他「导出」不在本次范围：`export_a2a_config`（`workspace.rs:1717-1734` → `exports/a2a_server.json`）、`export_skill_md`（`workspace.rs:1738-1746` → `skill/duthub/SKILL.md`）、`export_agent_report`（`workspace.rs:1748+`）。

### 1.3 外部消费方

- **duthub-mcp stdio bin**（`crates/duthub-mcp/src/main.rs:13-64`）：从 `[config_dir_in(anchor), config_dir_in(cwd)]` 读 settings.json 的 `workspaceDir`；快照发现走 `default_work_dir_in(anchor/cwd)` 下的 `runtime/mcp-state.json`。
- **TUI**（`apps/tui/`）：grep 确认不引用 config 目录 → 不受影响。

### 1.4 受影响的既有测试

- `store.rs` 内单元测试（约 :495-673）大量以 `duthub-data` 作为配置目录断言，需同步更新。
- `crates/duthub-core/tests/roundtrip.rs`（examples 往返测试）不涉及目录路径 → 不受影响。
- `data.rs:392` `missing_connection_file_does_not_create_demo_duts` 等行为测试需复核语义。

## 2. 目标行为

### 2.1 目录职责划分（配置与工作文件分离）

```
<exe 目录>/
├── Duthub.exe
├── .duthub/                 ← 配置目录（新）：settings.json、duts.json
│   ├── settings.json
│   └── duts.json
└── duthub-data/             ← 工作文件根（不变）：audit/ runtime/ logs/ captures/
    ├── runtime/mcp-state.json    skill/ exports/ traces/ …
    └── …
```

- `.duthub/` 只放**用户可手工编辑的配置**；`duthub-data/` 只放**程序产出的工作文件**。两者彻底解耦后，`config_dir_in` 不再别名 `default_work_dir_in`（`store.rs:28-30` 拆开）。
- 工作目录解析链（`resolve_work_dir_in` / `ensure_portable_work_dir`，settings `workspaceDir` > CLI `--workdir` > 默认）**不变**；`workspaceDir` 指向的语义仍是「工作文件根」，不是配置目录。

### 2.2 `.duthub/` 引导语义（逐文件，幂等）

| `.duthub/settings.json` | 行为 |
| --- | --- |
| 目录不存在 | 创建 `.duthub/`（经 `ensure_portable_child` 校验普通目录），**写入默认 settings.json**（`default_settings()` 序列化落盘），再加载使用 |
| 目录存在、文件缺失 | 同上：写入默认文件后使用（**逐文件补齐**，两个文件互不依赖） |
| 文件存在 | 原样加载使用，**绝不覆盖**（运行期间保存仍走原子写） |
| 文件损坏 | 保持现行语义：改名 `*.corrupt-<ts>.json` 备份 → 返回错误 → 使用内存默认；**不回写覆盖** |

`duts.json` 同理：缺失 → 写入默认 duts.json 后使用（默认内容见 §8 问题 1）；存在 → 原样使用；损坏 → 备份 + 回退。

不变式（对应 AGENTS.md 坑 #9）：**settings.json 进程内仍只加载一次**。默认文件落盘发生在 `main.rs:61-84` 的单次加载路径内（bootstrap 阶段），不在 Workbench 或渲染层重复。

### 2.3 移除配置导出

- 删除设置页 settings.json / duts.json 两行的「导出」按钮（`pages.rs:3203-3216`）与 `export_config`（`workspace.rs:1698-1715`）。
- 「配置文件」面板改为**展示真实路径**：显示 `<exe>/.duthub/settings.json` 与 `<exe>/.duthub/duts.json`（只读文本 + 提示「可直接编辑」），替代 `configExportHint` 的导出引导文案——配置就在 `.duthub/` 里，无需导出副本。
- **导入按钮保留**（`import_config` 不动；见 §8 问题 2）。
- `export_a2a_config` / `export_skill_md` / `export_agent_report` 不动（见 §8 问题 3）。

## 3. 存量迁移与兼容

**问题**：已发布的安装里，配置在 `<exe>/duthub-data/settings.json|duts.json`。若首启直接在空 `.duthub/` 写默认值，会**丢用户配置**。

**迁移规则**（bootstrap 阶段，先于加载执行，幂等）：

1. `.duthub/<file>` 已存在 → 不迁移（用户在 .duthub 的配置优先）。
2. `.duthub/<file>` 缺失 且 `<exe>/duthub-data/<file>` 存在且可解析 → **复制**到 `.duthub/<file>`，旧文件改名 `<file>.migrated` 备份（可回滚；改名为幂等标记，下次启动不再重复迁移）。
3. `.duthub/<file>` 缺失 且旧文件不存在 → 按 §2.2 写默认文件。

**duthub-mcp stdio 发现链兼容**（`duthub-mcp/src/main.rs:38`，2026-11-23 偏差注记——与设计稿不符，以实现为准）：`load_workspace_setting` 的候选**维持两路径** `[config_dir_in(&anchor), config_dir_in(&cwd)]`，未按本节原设计扩为四路径——旧 `duthub-data` 根不参与 settings 读取，仅保留 **mcp-state.json 快照发现**（`default_work_dir_in(anchor/cwd)` 下 `runtime/mcp-state.json` 存在性探测）。

**向后兼容边界**：`.duthub/` 一旦建立即为准；`duthub-data/` 内残留的旧配置只作为迁移来源，迁移完成后不再被读取（stdio 兜底候选除外）。

## 4. 改动清单（按文件）

| 文件 | 改动 |
| --- | --- |
| `crates/duthub-core/src/store.rs` | ① `config_dir_in` 改为 `exe_dir.join(".duthub")`，与 `default_work_dir_in`（仍 `duthub-data`）拆开；② 新增 `bootstrap_config_dir(exe_dir) -> DeviceResult<()>`：创建目录 + 逐文件迁移/落默认（§2.2/§3 规则）；③ 新增 `write_default_settings_if_missing` / `write_default_duts_if_missing`（复用 `save_settings_atomic` / `save_duts_atomic`）；④ 同步更新模块文档注释（:1-6、:23、:32） |
| `crates/duthub-core/src/dut_artifacts.rs` | 验证 `name_component(".duthub")` 是否被 `log_archive::dut_folder` 放行；若拒绝前导点，为 bootstrap 常量单独放行（仅目录组件，不影响 DUT 名校验） |
| `apps/desktop/src/main.rs` | bootstrap（:61-84）加载前调用 `bootstrap_config_dir`；目录创建失败 → eprintln + exit(1)（与现行 :68-77 一致）；迁移/落默认失败 → 降级为内存默认 + toast 文案，不阻断启动 |
| `apps/desktop/src/pages.rs` | 「配置文件」面板（:3219-3244）：删导出按钮；行内增加真实路径展示；副标题改路径提示 |
| `apps/desktop/src/workspace.rs` | 删 `export_config`（:1698-1715）；`config_dir` 的真实路径经 `StartupContext`/Workbench 已有的 data_dir 传递（确认字段，勿新读一遍文件——坑 #9） |
| `apps/desktop/locales/zh-CN.json`、`en-US.json` | 删/改 `configExportHint`、新增路径提示文案（如 `configPathHint`） |
| `crates/duthub-mcp/src/main.rs` | `load_workspace_setting` 候选扩为 §3 的四路径（2026-11-23 注记：实际落地维持两路径，见 §3 偏差注记）；模块注释（:1-8）同步 |
| `crates/duthub-core/src/store.rs`（tests） | 既有 duthub-data 配置断言（:495-673）改为 `.duthub`；新增 §5 测试 |
| `AGENTS.md` | 坑 #9 与「构建与运行」节：配置目录描述 `duthub-data/` → `.duthub/`（bootstrap 配置目录语义不变） |
| `43c58dc:docs/agent/TASKS.md` | 追加一行：`.duthub/` 迁移落地，旧约束「不写 ~/.duthub」仍成立（exe 同级） |

## 5. 测试与验收

新增单元测试（`store.rs` tests 模块，沿用 `CARGO_MANIFEST_DIR` 运行时读取——坑 #4）：

1. **首启创建默认**：空 exe 沙箱 → bootstrap 后存在 `.duthub/settings.json` + `.duthub/duts.json`，内容等于 `default_settings()` / 默认 duts。
2. **存在即使用**：预写用户改过的 settings.json（如 theme=light）→ bootstrap 后文件内容不变、加载结果为 light。
3. **逐文件补齐**：只有 settings.json 存在 → duts.json 被补默认，settings.json 不动。
4. **损坏保护**：预写损坏 settings.json → 改名 `*.corrupt-*` 备份、返回错误、原文件未被覆盖。
5. **迁移幂等**：`duthub-data/settings.json`（workspaceDir=foo）+ 空 `.duthub/` → 迁移后 `.duthub/settings.json` 有 workspaceDir=foo，旧文件改名 `.migrated`；再跑一遍不再变化。

验收入口：

- `cargo fmt --all --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace`（并行期按坑 #10/#16 用 C 盘隔离 target）。
- `bash scripts/regression`（如有引用 `duthub-data` 作为配置目录的脚本，随迁移同步更新——实施时先 grep）。
- 手工端到端（release 前 taskkill 旧 exe——坑序言）：① 删 `.duthub/` 首启 → 出现两份默认文件且 UI 生效；② 改 theme 重启 → 不被覆盖；③ 旧安装（duthub-data 含配置）首启 → 配置迁移、DUT/主题不丢；④ stdio bin 在未迁移目录仍能发现快照。

## 6. 实施顺序（小步提交）

1. store.rs：路径拆分 + `bootstrap_config_dir` + 单测（纯核心层，先行）。
2. main.rs 接线 + 迁移逻辑 + 单测。
3. stdio 发现链兼容 + 其单测。
4. UI：删导出、加路径展示、i18n。
5. 文档回写（AGENTS.md / TASKS.md）+ `bash scripts/regression` 全绿。

每步 `cargo fmt + clippy + test` 全绿再进下一步；一次只变一个变量。

## 7. 风险

- **默认 duts.json 内容**若沿用演示数据，会把演示 DUT 固化为文件（见 §8 问题 1）。
- Windows 上 `.duthub` 前导点目录不会自动隐藏（非隐藏属性）；如需隐藏可在创建时设置 FILE_ATTRIBUTE_HIDDEN（可选，不阻塞）。

## 8. 开放问题（实施前需拍板）

1. **首次运行 duts.json 的默认内容**：A. 沿用现行缺省回退（`DutsState::demo()` 演示数据，行为零变化）；B. 改为空列表（更干净，但首次启动从演示磁贴变为空磁贴，改变首启体验）。默认建议：A。
2. **「导入」按钮**是否保留（本设计默认保留，只删导出）。
3. **a2a_server.json 导出**（`export_a2a_config`）是否也算「配置文件导出」一并移除？（本设计默认保留：它是给外部 A2A peer 的对接产物，且有文档/Skill 引用。）
4. **迁移采用复制+改名备份**（本设计）还是直接移动？（复制+改名可回滚，推荐。）
5. `.duthub/` 锚点确认为 **exe 同级**（便携，不写主目录）——如用户意图是其他锚点请指出。
