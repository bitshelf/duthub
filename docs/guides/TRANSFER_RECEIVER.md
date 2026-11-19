# 镜像传输接收端（duthub-transfer）

> 2026-11-24 重写。上一版（fileloft 0.3.0 + 自写存储/锁/GC/发布）已被替换：
> 协议、磁盘存储、每上传锁、完成即发布全部交给上游 crate，本 crate 只保留
> 鉴权、名称校验、磁盘预算与「客户端要求的校验」。

## 1. 依赖与分工

| 关注点 | 归属 |
| --- | --- |
| HTTP/1.1 与**明文 HTTP/2（h2c）** | `salvo` 1.2.1，`default-features = false` + `http1`/`http2`/`http2-cleartext`/`server`/`server-handle`/`fix-http1-request-uri`/`tus` |
| tus 1.0.0 协议、磁盘存储、每上传锁、完成定名发布 | `salvo` 官方 `tus` 模块（crate `salvo-tus`） |
| 整文件摘要 | `blake3` 1.8（`mmap` + `rayon`）；`sha2` 仅为兼容 ztus 的 SHA-256 |
| 鉴权 401/403、名称白名单、磁盘预算 507、可选校验 | 本 crate（`authorize`/`names`/`router`/`checksum`/`store`） |

不引入 TLS 栈：局域网监听按产品口径是明文 HTTP，绑定 `0.0.0.0` 必须配鉴权（fail closed）。

## 2. 客户端

任何 tus 1.0.0 客户端可用。两条明确支持的路径：

```bash
# ztus：可续传、自适应分块、h2c（prior-knowledge HTTP/2）
ztus upload boot.img http://<host>:<port>/transfer \
  --header "Authorization:Bearer <token>" --metadata board:rk3576
ztus upload boot.img http://<host>:<port>/transfer \
  --header "Authorization:Bearer <token>" --checksum sha256      # 可选校验
ztus upload boot.img http://<host>:<port>/transfer \
  --header "Authorization:Bearer <token>" --no-checksum           # 局域网省时间
ztus resume boot.img http://<host>:<port>/transfer \
  --header "Authorization:Bearer <token>"                         # 中断后续传

# curl：创建时声明一次摘要（BLAKE3 用官方 b3sum 计算）
curl -si -X POST http://<host>:<port>/transfer \
  -H "Authorization:Bearer <token>" -H "Tus-Resumable: 1.0.0" \
  -H "Upload-Length: $(stat -c %s boot.img)" \
  -H "Upload-Metadata: filename $(printf boot.img | base64 -w0),board $(printf rk3576 | base64 -w0)" \
  -H "Upload-Checksum: blake3 $(b3sum boot.img | cut -d' ' -f1)"
curl -si -X PATCH http://<host>:<port>/transfer/<id> \
  -H "Authorization:Bearer <token>" -H "Tus-Resumable: 1.0.0" \
  -H "Upload-Offset: 0" -H "Content-Type: application/offset+octet-stream" \
  --data-binary @boot.img
```

### ztus 兼容要点（源码核实，2026-11-23 提交 `87292e8`）

- 用 `http2_prior_knowledge()` 通信 → 服务端**必须**开 `http2-cleartext`，否则连不上。
- 鉴权只能经 `--header "Authorization:Bearer …"`；无独立 token 参数。
- 元数据 `--metadata key:value`（可重复）；`filename` 会在缺省时按文件路径自动补上。
- `--checksum sha1|sha256|none`（默认 sha256）：摘要**在创建请求**随 `Upload-Checksum` 发出。
  服务端拒绝 sha1 时必须在报文里出现 `unsupported checksum algorithm`，ztus 才会回退到 sha256。
- `resume` 只接受 `--header`：**没有状态文件时会回退成新建上传**，而它此时不带 `filename`
  → 服务端按契约 400。正常续传（状态文件存在）只做 HEAD + PATCH，不受影响。
- 每个 PATCH 成功必须回 `Upload-Offset`；续传前的 HEAD 必须是 **200**（204 会让它中止）。

## 3. 校验模型：由客户端决定

- 创建请求带 `Upload-Checksum: <algorithm> <hex>` 才校验；算法支持 `blake3`、`sha256`（大小写不敏感，hex 大小写归一）。
- 不带该头 → **零哈希开销**（局域网直传不读第二遍盘）。
- 带该头 → 最后一字节提交后整文件单遍哈希；通过则回执带 `X-Duthub-Checksum: <algo>:<digest>`，并把结果写入
  `metadata/<id>.verified`（重启后仍然有效，不重复计算）。
- 不通过 → 上传被丢弃，回 `460` + JSON（`error=checksum_mismatch`、`expected`、`actual`、`algorithm`）；
  这是终态，重发同样字节只会再失败。
- 丢弃采用「**先写失败标记、再删除**」：`metadata/<id>.failed` 由周期清扫重试，避免 Windows 持有句柄时
  坏文件留在 `data/` 看起来像已发布。

## 4. 磁盘布局（上游 `DiskStore` 拥有）

```text
<transfer_dir>/
  data/<filename>          # 完成的上传，已定名发布
  data/<id>.bin            # 仍在传输
  metadata/<id>.json       # size / offset / metadata（每 PATCH 原子提交）
  metadata/<id>.verified   # 校验通过的摘要（本 crate 写）
  metadata/<id>.failed     # 待清扫的失败标记（本 crate 写）
```

- 定名规则：`filename` 元数据（缺失不再可能——创建时必填）；重名**绝不覆盖**，后到的落成
  `<stem>-<id>.<ext>`。
- 默认根目录 `<工作根>/duthub-data/images`；`transferDir` 必须在工作根之内（绝对路径同样校验包含关系），并拒绝 `..`、`D:images`、`\images`、UNC 与 reparse point。2026-11-25 起绝对路径不再原样放行。
- 每个 PATCH 提交后 offset 可跨进程崩溃恢复；上游**不做 fsync**，**没有断电一致性保证**——声明摘要只证明「进程读到的字节与声明一致」（经页缓存），不证明已落盘。需要更强保证时在上传侧或文件系统层解决。

## 5. 状态码

| 码 | 场景（谁答） |
| --- | --- |
| 200 | HEAD 进度（上游，ztus 续传前提） |
| 201 | 创建成功（`Location: <base>/<id>`，相对路径） |
| 204 | PATCH/DELETE/OPTIONS 成功 |
| 400 | 名称/元数据非法、`Upload-Checksum` 非法、非规范上传 ID、非规范路径（本 crate） |
| 401 | 无/未知令牌（本 crate，带 `WWW-Authenticate: Bearer`） |
| 403 | 令牌有效但超出作用域（板型/长度/过期/非本人上传；本 crate） |
| 404 | 未知或已清扫上传；不匹配任何 tus 路由的路径（如 `/%2Ftransfer`） |
| 405 | 接收端不提供下载（GET）与其他方法（本 crate） |
| 409 | offset 不匹配（上游） |
| 410 | 已过期未清扫（上游 expiration） |
| 412 | tus 版本不支持（上游） |
| 413 | 超过 `Tus-Max-Size`（上游） |
| 460 | 摘要不匹配（本 crate，终态） |
| 501 | 延迟长度 / 创建即上传（未宣告，上游） |
| 503 | 上传锁等待超时（上游 `LockTimeout`） |
| 507 | 磁盘预算不足（本 crate，JSON 带 `required`/`available`） |

路径形态门禁：只接受以单个 `/` 开头、段非空且非 `.`/`..`、无反斜杠、无百分号编码、至多一个尾斜杠的路径。`//transfer`、`/./transfer`、`/foo/../transfer` 这类写法会在鉴权之前被 400 拒绝（`invalid_path`）；挂载点之下的非规范上传 ID 报 `invalid_upload_id`。规则来自审核 F-01：路由层会归一化这些写法并把请求交给 tus handler，守卫必须与路由层按同一形态判定。

## 6. 过期与清扫

- `uploadExpiresSecs` 是唯一 TTL：既是上游 `expiration` 扩展（410 + `Upload-Expires`），也是清扫依据（清扫后 404）。
- 只清**未完成**的上传；已完成文件不按 TTL 删除。
- **容量口径（2026-11-25 用户拍板）**：已完成镜像计入每 DUT 容量（`dutStorageBytes`，只算 `data/` 载荷，不含元数据开销）；配额不足时按修改时间从**最旧**的已完成制品开始淘汰（`evict_completed`），**在途上传永不淘汰**；淘汰后仍不足返回 507。TTL 清扫与配额淘汰是两条独立路径。
- 清扫与写者共用同一把每上传锁；锁被占用就留到下一轮。
- `serve` 内启动周期清扫（启动即扫一轮）；自行嵌入 router 的宿主需自行调用 `gc_now`。

## 7. 待接线（宿主侧，不在本 crate）

- 端口加入全局唯一性/占用探测（AGENTS.md 坑 #45）并对齐 `PortRegistry`（`crates/duthub-a2a/src/ports.rs`）。
- `TransferConfig` 落 `SettingsFile`（serde 形状已按 workspace 约定：默认值段序列化为 `{}`）。
- keyring 令牌实现 `UploadAuthorizer`。
- 桌面/MCP 入口（上传状态查询、制品列表）另立任务。

## 8. 2026-11-25 审核修复记录

> 对应 [TRANSFER_SECURITY_REVIEW.md](../reviews/TRANSFER_SECURITY_REVIEW.md) 的 F-01..F-19；全部 crate 级项已落地，测试 103 条。

- **路径**：最外层规范形态门禁（`//`、`.`/`..` 段、反斜杠、百分号、非法尾斜杠 → 400）；传输根必须位于宿主传入的工作根内（`TransferServer::new_in`：词法归一 + canonicalize 包含 + 拒绝 reparse）；`for_dut` 用 `<dut 目录>/images` 并校验 `.duthub-dut-id`；marker 路径对 id 做校验。
- **每 DUT**：`TransferServer::for_dut(dut_id, dut_dir, …)`，上传创建时服务端写入 `dutId` 元数据，后续请求跨 DUT 一律 403；`dutStorageBytes` 为每 DUT 配额（`used_bytes` + 声明长度）。
- **认证**：`Tus-Resumable` 缺失/不支持在鉴权前返回 412；上传记录读失败或元数据解码失败返回 500，不再按「未知上传」放行。
- **资源**：创建串行化（预算检查与 tus 落记录原子化）；零长度上传默认 400（`allowZeroLengthUploads` 显式开启）；清扫回收 `data/` 孤儿、`metadata-tmp/`、损坏记录、无主 `.verified`、人工删文件后的墓碑；同一传输根独占写者（`.duthub-transfer.lock`）；整文件哈希并发由 `verifyConcurrency` 限流（默认 2）。
- **回执**：`x-duthub-artifact` 改为相对传输根的路径；校验失败/拒绝响应不带该头。
- **文档更正**：断电一致性承诺删除（见 §4）。
- **仍待接线（宿主侧）**：`TransferConfig` 落 `SettingsFile`、端口登记进 `PortRegistry`、keyring `UploadAuthorizer`、桌面/MCP 入口。容量口径已按 2026-11-25 用户拍板落定：已完成镜像计入每 DUT 容量并按修改时间淘汰最旧（在途不淘汰），与 `API_CONTRACT.md:123` 一致。
