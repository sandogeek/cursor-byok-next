# cursor-capture

单独的调试进程：把 Cursor 的 API 流量按固定上游 `https://api2.cursor.sh` 转发，边转发边抓包，再用原版调试页查看 Connect 帧、protobuf 消息和原始正文。

它**不属于 nsh 桌面应用运行时**，不碰 `harness/` 的代理与 CA，也不改 Cursor 的任何文件。移植来源与逐行差异见 [`apps/desktop/docs/CURSOR_CAPTURE.md`](../../apps/desktop/docs/CURSOR_CAPTURE.md)。

## 快速开始

```sh
# 仓库根
cargo run -p cursor-capture -- --addr 127.0.0.1:9090

# 调试页：http://127.0.0.1:9090/__debuger__/
```

启动时会打印服务地址、固定上游、调试界面与 SQLite 路径，并默认打开浏览器。

| 参数 | 默认值 | 说明 |
|---|---|---|
| `--addr` / `-addr` | `127.0.0.1:9090` | 监听地址，只接受回环（`localhost` / `127.0.0.0/8` / `::1`） |
| `--max-exchanges` | `200` | 内存窗口保留的请求数，只影响内存，不裁剪磁盘 |
| `--db` | `~/.cursor-byok-next/cursor-proxy-debugger.db` | SQLite 路径，目录不存在会自动创建 |
| `--no-open` | 关闭 | 不自动打开浏览器（默认是 `--open`） |
| `-h` / `--help` | | 打印用法 |

要抓真实流量，需要把 Cursor 发往 `api2.cursor.sh` 的请求改到这里。本服务是**明文 HTTP 反向代理**，不做 TLS 终止也不处理 `CONNECT`，因此前面需要有能改写目标地址的一层（例如抓包机上的 hosts 加本地 TLS 终止，或调试用客户端）。对真实 Cursor 安装的端到端抓包尚未验证。

## 目录

```text
crates/cursor-capture/
├── Cargo.toml          依赖；二进制名 cursor-capture
├── README.md           本文件
├── build.rs        188 行  清洗 protocols/cursor 的 proto，protox 编成 FileDescriptorSet
├── src/
│   ├── main.rs      90 行  CLI 解析、监听、优雅退出
│   ├── lib.rs       16 行  模块入口与公开导出
│   ├── config.rs   121 行  回环校验、默认值、浏览器地址
│   ├── types.rs    129 行  Exchange / Payload / FrameView 的 JSON 契约（camelCase）
│   ├── capture.rs  138 行  正文限长拷贝包装器（边转发边取样）
│   ├── decode.rs   937 行  Connect 帧切分、解压、prost-reflect 动态解码、历史回填
│   ├── store.rs    601 行  内存窗口 + SQLite 持久化 + SSE 广播、会话关联
│   ├── proxy.rs    591 行  axum 路由 + hyper 转发 + 请求/响应收尾
│   ├── web.rs      165 行  调试 API、SSE、内嵌静态页
│   └── tests.rs    431 行  解码 / 存储 / 流式转发 / 调试 API 测试
└── web/           2179 行  原版调试界面（HTML + Alpine，原样拷贝上游）
```

行数为 `wc -l` 实测，含注释与空行，不含构建期生成的 `cursor.bin`。

## 数据流

```text
Cursor / 调试客户端
        │  HTTP（任意路径）
        ▼
 axum Router ── /__debuger__/* ──► web::router ──► rust-embed 静态页 + JSON API + SSE
        │
        └─ fallback: proxy()
              │
              ├─ capture_request()      新建 Exchange，编号入内存窗口并落盘，广播 created
              │     └─ CaptureBody<Body> 包住请求正文
              │           ├─ 每 chunk：限长拷贝一份 + 增量喂给 ConnectFrameDecoder
              │           └─ 结束时：解压 → 反序列化 → 写 rawHex/decodedJson/kind/requestId
              │
              ├─ rewrite_upstream()     路径与 query 拼到固定上游，剥 hop-by-hop，重写 Host
              │
              ├─ hyper-rustls 客户端    HTTPS → https://api2.cursor.sh
              │
              └─ 响应体同样包一层 CaptureBody<Incoming>
                    ├─ 每帧：追加 FrameView、更新 responseKind / frameCount（仅内存）
                    └─ 结束时：解码、计时、state=completed（读中断记 error）并落盘
                                        │
                                        ▼
                        SSE /__debuger__/api/events ──► 调试页自动刷新
```

转发在正文被轮询时同步完成，抓包不额外缓冲：客户端没读完，上游就不会被提前读完；调试页看到的是同一份字节。

## 解码能力

`build.rs` 只做编译期准备，运行时全部走 `prost-reflect` 动态解码，因此不需要为每个 RPC 生成 Rust 类型。

| 阶段 | 内容 |
|---|---|
| 描述符 | `protocols/cursor/agent_v1.proto` + `aiserver_v1.proto` → `cursor.bin` |
| 清洗 | 拼接反斜杠续行；`Parent_Child` 改写为 `Parent.Child`；编译报“类型未定义”时把该名字替换为 `bytes`（最多 500 轮，仍失败即构建失败） |
| 帧 | 5 字节头（1 字节 flags + 4 字节大端长度），`0x01` 压缩、`0x02` end-stream；单帧上限 64 MiB，收满 `max_frames` 后停止解码 |
| 解压 | `gzip` / `deflate` / `br` / `identity`；流式读 `connect-content-encoding`，普通响应读 `content-encoding` |
| 流式类型 | `RunSSE` 硬编码请求 `aiserver.v1.BidiRequestId`、响应 `agent.v1.AgentServerMessage`；其余按 proto 的 streaming 方法取 input/output |
| 一元类型 | 白名单内 RPC 直接映射消息名（BidiAppend、AvailableModels、GetServerConfig、GetKnownServers…），未登记的一元方法从 `MethodDescriptor` 取 input/output |
| BidiAppend | 解 `BidiAppendRequest`，再解 hex / binary 内层 `AgentClientMessage`，抽出 `conversation_id`，输出外层与内层合并视图 |
| 非 proto | JSON 美化、文本按 MIME 标语言，其余给截断 hex |

JSON 字段名统一 camelCase（`startedAt` / `decodedLanguage` / `rawHex` / `rawTruncated` / `frameCount` / `requestId` / `conversationId`），与上游 Go struct tag 一致。

## 调试 API

全部挂在 `/__debuger__/` 下。原版前端请求的是相对路径 `api/...`，所以这个前缀不能改；拼写 `__debuger__` 沿用上游。

| 方法 | 路径 | 返回 |
|---|---|---|
| GET | `/__debuger__/api/status` | 监听地址、上游、数据库路径与最近一次数据库错误 |
| GET | `/__debuger__/api/exchanges[?conversation_id=]` | `ExchangeSummary` 列表 |
| GET | `/__debuger__/api/exchanges/{id}` | 单个 `Exchange`（无则 404） |
| GET | `/__debuger__/api/conversations` | 按会话聚合的条数、最后时间、收发字节 |
| DELETE | `/__debuger__/api/exchanges` | 清空，204 |
| GET | `/__debuger__/api/events` | SSE：一次 `ready`，之后 `update` 与 15s 心跳 |

响应统一加 `nosniff`、`no-referrer` 与 CSP（允许 jsDelivr 的脚本/字体，`connect-src 'self'`）。

## 存储

| 层 | 行为 |
|---|---|
| 内存窗口 | 最新 `max_exchanges` 条，`order` 维护插入序，超出即淘汰最旧 |
| SQLite | `~/.cursor-byok-next/cursor-proxy-debugger.db`，WAL、`secure_delete`、5s busy timeout |
| 表 | `exchanges(id, started_at_ms, conversation_id, request_id, state, request_bytes, response_bytes, payload_json, updated_at_ms)` 加会话/起止索引 |
| 写入 | 新建、请求收尾、响应收尾各写一次；流式帧只更新内存（`update_transient`） |
| 启动 | 按 `MAX(CAST(id AS INTEGER))` 续编号；先回填历史记录的解码字段，再载入最近 `max_exchanges` 条 |
| 关联 | 同 `request_id` 的记录互相互补 `conversation_id`，内存与磁盘都补 |
| 清空 | 同一把锁内删表、清窗口、递增捕获代次、编号回到 1；旧代次回调直接丢弃，不覆盖新记录，也不中断已在转发的旧流 |

敏感头（`authorization`、`cookie`、`set-cookie`、`proxy-authorization`、`x-api-key`）的值统一显示为 `[已隐藏]`，头名按字母序输出。

## 测试

```sh
cargo test -p cursor-capture
cargo clippy -p cursor-capture --all-targets -- -D warnings
```
## 已知边界

- 每方向只保留 2 MiB 原始正文副本（溢出时 `rawTruncated=true`），一元解码会明确报“超过抓取上限”；这 2 MiB 是正文副本上限，不是解码后帧的总内存。
- `--max-exchanges` 只管内存窗口，磁盘不做淘汰。
- 只转发，不缓存、不重试、不改写正文；上游不可达时返回 502 并把 `state` 记为 `error`。
- 未对真实 Cursor 安装做端到端抓包验证。