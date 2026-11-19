# `.rec` 结构化记录日志格式（v1）

会话日志的第三条流。与 `.log`（utf8-lossy 文本行）/ `.bin`（原始字节，审计权威）同源同线程，逐条 `write_all + flush`，零丢失语义一致（AGENTS.md 坑 #47）。

- 实现与零拷贝读取：`crates/duthub-device/src/rec.rs`（zerocopy derive，写入免手工打包、解析为借用切片）
- 写入接线：`apply_log_msg`（crates/duthub-device/src/demo.rs）每条 `LogMsg` 伴随一条记录；`log_control_boundary` 直写兜底路径同样落记录
- 回归红线：LOG-7（drain 顺序）/ LOG-8（逐条 flush）/ LOG-9（轮换与分段）/ LOG-10（开关跟随），另有 rec.rs 九个单测

## 文件布局

每会话三条流同目录轮换：

| 流 | 主段 | 轮换段 | 阈值 |
|---|---|---|---|
| 文本 | `<session>.log` | `.log.1` … `.log.20` | 2 MiB |
| 原始 | `<session>.bin` | `.bin.1` … `.bin.20` | 8 MiB |
| 记录 | `<session>.rec` | `.rec.1` … `.rec.20` | 8 MiB（与 .bin 同阈值） |

`.rec`、文本日志和 `.bin` 随串口会话写入。「连接管理 → 按启动分割日志」按 DUT 保存在 `serial.splitLogs`，保存连接后生效。开启时，便携模式的三个文件共用 `NAME-NNN_时间戳`。关闭时，后续会话持续追加到 `logs/serial.txt`、`logs/serial.bin` 和 `logs/serial.rec`。连续日志和已有分段都可通过 MCP 读取。

### 按启动周期分段

- 「启动日志参考文件」按 DUT 保存到 `.duthub/duts.json` 的 `serial.referenceLog`。可填写路径或浏览选择。相对路径从 duthub 工作目录解析。首次配置时，如果该 DUT 已有 `references/reference-boot.log`，界面会自动显示其路径。
- 「学习启动日志」经用户确认后，通过已绑定的电源控制通道采集 3 轮。采集开头部分，最多保留 100 行，并在进入内核时结束。比较前 50 行时归一化时间戳、地址和计数值。参考文件只在所有采样两两相似度都达到该 DUT 的最低相似度时更新。默认 95%，可在连接管理中调整。
- 参考文件启用时，从匹配到的早期启动行开始新段，下一次启动行之前的内容留在本段。继电器动作和 Agent 标记继续记录。首次在已运行设备上打开串口时，第一段可能只有部分启动周期。手动改路径保存后在下次打开连接时加载。
- 分段标记存入会话日志。写入失败通过应用诊断记录，串口终端继续显示设备输出。


**每段独立成文件**：段内字节连续，段之间无顺序耦合；每段以文件头开始（轮换段若复用既有文件则不重复写头）。段列表按「主段 → .1 → .2 …（数值序）」拼接即完整时间序。

## 字节格式（全部小端）

### 文件头（24 字节，仅段首、仅空文件时写）

| 偏移 | 大小 | 字段 | 说明 |
|---|---|---|---|
| 0 | 8 | magic | `DUTHUBR1` |
| 8 | 4 | version | `1`（LE u32） |
| 12 | 4 | header_len | `24`（前向扩展用） |
| 16 | 8 | created_unix_ns | 段创建时刻 |

### 记录头（24 字节，定长，`#[repr(C)]` + Unaligned）

| 偏移 | 大小 | 字段 | 说明 |
|---|---|---|---|
| 0 | 4 | magic | `REC1`（撕尾部再同步/拒绝垃圾用） |
| 4 | 4 | kind | 见下表 |
| 8 | 8 | ts_unix_ns | 写线程消费序打点（单消费者 ⇒ 每会话单调） |
| 16 | 4 | payload_len | 载荷字节数（> 16 MiB 拒绝） |
| 20 | 4 | crc32 | 仅覆盖载荷（IEEE 0xEDB88320 反射，与 zlib 一致） |

### 记录类型（kind）

| 值 | 类型 | 载荷 |
|---|---|---|
| 1 | Raw | RX 线上字节（与 .bin 同段字节） |
| 2 | Tx | TX 线上字节 |
| 3 | Keystrokes | 键入审计字节 |
| 4 | Note | 注记行全文（含 `[note]`/`[serial]` 前缀） |
| 5 | Mark | `[mark] {label}` 全文 |

0 为非法 kind。单条记录 = 记录头 + 载荷，**单次 `write_all` + flush**（进程内撕裂窗口消除；OS 级崩溃仍可能截断尾部）。

## 写入规则

- `LogMsg::Raw` → Raw(字节)；`Tx` → Tx(线上字节)；`Keystrokes` → Keystrokes(字节)；`Note` → Note(行文本)；`Mark`/`MarkSync` → Mark(`[mark] …` 全文)。
- **Mark 记录写在 `mark()` 之前**：普通模式 `mark()` 先 append 再 split，因此标记记录与 `.log` 标记行同落旧段；`.rec` 失败不影响 mark 成败判定（供电边界 LOG-5 语义只看 `.log`/`.bin`）。
- `.rec` 载荷超过 16 MiB 截断（防御性上限，串口块 ≤4 KiB 实际不可达）；`.bin` 永远完整。
- `.rec` 写失败不回滚审计流；apply 层与 Raw/Tx 同形状静默（`let _`）。

## 读取规则（`parse_records` / `RecTailReader`）

1. 首 24 字节 magic 为 `DUTHUBR1` 则跳过文件头；否则从字节 0 开始（续读缓冲/无头旧段均可解析）。
2. 逐条取记录头：magic 不符、kind 非法、`payload_len` 越界或超上限、CRC 不符 → **停在最后一条完整记录**（撕尾容忍，绝不 panic、不吞好记录）。
3. 增量追读：`RecTailReader` 记字节 offset，`poll` 读到 EOF 后解析，不完整尾记录留在缓冲等后续字节；offset 只推进完整记录。
4. 多段文件必须**按段逐个解析**——直接拼接原始字节会被后续段的文件头卡停（rec_rotates_and_splits_with_pair 红线锁定此行为）。

## 与既有契约的关系

- MCP `log_segments`/`log_segment_read` 只认 `.log`/`.txt`（`matching_files` 后缀过滤、`log_sequence` 只认 `.txt`），`.rec` 不进入 MCP 工具面。
- `logs::list_segments` 只列 `.log`/`.bin` 家族（既有测试断言精确集合）；`.rec` 段列表走 `rec::list_rec_segments` / `DeviceService::session_rec_segments`。
- 时间戳是写线程消费时打点（≈入队时刻 + 写线程延迟），同会话严格单调不减、顺序即入队序。
