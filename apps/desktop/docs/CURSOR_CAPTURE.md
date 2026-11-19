# 移植记录：Cursor 协议调试器（Go cursor-capture → Rust）

上游实现：`cursor-byok/support/cursor-capture/`（2338 行 Go + 2179 行原版 web，无 Go 单测）。
本移植：`crates/cursor-capture/`（3219 行 Rust，含单测 + 188 行 build.rs；web 原样拷贝 2179 行）。

范围只有一件事：**独立进程把 Cursor API 流量转到 `api2.cursor.sh`，边转发边抓包，并用原版调试页查看**。不接入 nsh 桌面应用，不改 harness 代理，不重写 web。

## 目录映射

```text
cursor-byok/support/cursor-capture/
├── main.go              50 行  CLI
├── types.go            189 行  配置 / Exchange / Payload / Frame
├── capture.go           95 行  限长拷贝读取器
├── capture_pipeline.go 345 行  请求响应捕获与解码收尾
├── decode.go           380 行  Connect 帧 + protobuf 视图
├── decode_stored.go    397 行  历史回填、HTTP 解压、动态类型
├── server.go           144 行  单端口反向代理
├── store.go            243 行  内存窗口 + SQLite 打开
├── store_queries.go    354 行  查询 / 关联会话 / SSE
├── web.go              141 行  调试 API + 内嵌静态页
└── web/               2179 行  原版调试界面

crates/cursor-capture/
├── src/
│   ├── main.rs          90 行  CLI（`--no-open`）
│   ├── lib.rs           16 行  模块入口
│   ├── config.rs       121 行  回环地址与默认值
│   ├── types.rs        129 行  JSON 契约
│   ├── capture.rs      138 行  http_body 限长拷贝与收尾状态
│   ├── decode.rs       937 行  Connect 帧 + prost-reflect
│   ├── store.rs        601 行  捕获编号、内存窗口 + rusqlite
│   ├── proxy.rs        591 行  axum 转发 + 捕获
│   ├── web.rs          165 行  原版 API / SSE / rust-embed
│   └── tests.rs        431 行  解码 / 存储 / 流式转发 / 调试 API
├── build.rs            188 行  清洗提取 proto 再编 FileDescriptorSet
└── web/               2179 行  原样拷贝上游页面
```

行数为 `wc -l` 实测。web 两侧字节级相同，不计入 Rust 差异。

## 差异清单

每一行都是**刻意**的，不是没抄完。

| # | 主题 | 上游 | nsh 版 | 原因 |
|---|---|---|---|---|
| 1 | 语言 / 进程 | 独立 Go module `cursor-proxy-debugger` | workspace crate `cursor-capture`，二进制同名 | 用户要求 Rust 写逻辑；不进 nsh 运行时 |
| 2 | web | `//go:embed web/*` | `rust-embed` 嵌同一份 `web/` | 用户要求 web 直接用原版，不重写成 NSH TSX |
| 3 | protobuf | `protoc-gen-go` 生成静态类型 + `protoregistry.GlobalFiles` | `protox` 编 FileDescriptorSet，`prost-reflect` 动态解码 | nsh 的 aiserver proto 太大，prost 生成代码不适合调试器；动态解码才能按 RPC 路径选消息 |
| 4 | 提取 proto 折行 | Go 生成器会写 `BUILT\` + 换行 | `build.rs` 先拼回 `BUILT_IN` | 标准 proto 词法不认反斜杠续行 |
| 5 | 嵌套类型名 | 字段写成 `Parent_Child` | `build.rs` 改成 `Parent.Child` | proto3 嵌套类型用点号；下划线名不存在 |
| 6 | 未解析压缩名 | Go 生成代码里没有 `K0` / `u0` | 编译失败时把未定义类型改成 `bytes` | 当前提取文件仍有未解析短名；改成 bytes 才能编过描述符，未知字段仍可当原始载荷看 |
| 7 | 反向代理 | `httputil.ReverseProxy`，`FlushInterval = -1` | axum + hyper 客户端，响应体边读边写 | Rust 标准库没有等价 ReverseProxy；http_body 流式拷贝保持 SSE / Connect 不缓冲 |
| 8 | 上游地址 | 写死 `https://api2.cursor.sh` | 默认相同，`Config.upstream_url` 可覆盖 | 单测必须打本地 echo，不能打外网 |
| 9 | SQLite | `modernc.org/sqlite` | `rusqlite` bundled | 纯 Rust 绑定，测试指向临时文件 |
| 10 | 数据目录 | `UserConfigDir()/cursor-byok/cursor-proxy-debugger.db` | `~/.cursor-byok-next/cursor-proxy-debugger.db` | 本应用标识是 Cursor-BYOK-Next；库文件名保持上游，方便对照 |
| 11 | 浏览器 | `github.com/pkg/browser`，`-open` 默认开 | `open` crate，`--no-open` 关掉 | 无头测试和 CLI 脚本不能弹浏览器 |
| 12 | SSE | `http.Flusher` + 15s `: heartbeat` | axum `Sse` + comment heartbeat | 契约仍是 `event: ready` / `event: update` / 心跳，原版 `EventSource` 不用改 |
| 13 | 调试路径 | `/__debuger__`（上游拼写） | 同 | web 的相对 `api/` 请求依赖这个前缀 |
| 14 | 清空期间的在途流 | 清库后独立重置编号，回调只带编号 | 存储层在同一把锁内清库、切换捕获代次、重置编号；拒绝旧代次更新 | 保留公开编号从 1 重启的行为，不让旧流污染新记录，也不取消旧流的转发 |
| 15 | 单帧更新 | Go 原地更新 | Rust 原地更新，帧所有权直接交给记录 | 避免每帧深拷贝已累计的 `Exchange`，保留实时可见和按需落盘的语义 |
| 16 | 正文收尾 | 以 `Read` / `Close` 返回值收尾 | 区分正常结束、读取错误、未读完即丢弃；取消信息沿用 `error` 字段 | 不把中断误记为 `completed`，同时保持现有 Web 状态契约 |

转发仍在正文被轮询时同步执行诊断回调，解码与 SQLite 写入尚未移出热路径。2 MiB 限制的是每方向原始正文副本，不是解码后帧的总内存；`max_exchanges` 也仅限制内存窗口，不限制磁盘保留量。

## 必须保持的语义（两侧一致）

| 不变量 | 上游 | nsh 版 |
|---|---|---|
| 只监听回环 | `validateLoopbackAddress` | `validate_loopback_address` |
| 调试页在 `/__debuger__/`，其余路径转发上游 | `server.go` mux | `proxy.rs` nest + fallback |
| 敏感头脱敏为 `[已隐藏]` | `isSensitiveHeader` | 同名单 |
| 单向载荷上限 2MiB，帧上限 2000 | `types.go` 默认值 | 同 |
| RunSSE 请求帧是 `aiserver.v1.BidiRequestId`，响应帧是 `agent.v1.AgentServerMessage` | `decode.go` | 同，有单测 |
| BidiAppend 解 hex/binary `AgentClientMessage`，抽出 `conversation_id` | `decodeBidiClientMessage` | `decode_bidi_append_request` |
| 同 `request_id` 的捕获补齐 `conversation_id` | `associateConversationLocked` | `associate_conversation_locked` |
| JSON 字段名 camelCase：`startedAt` / `decodedLanguage` / `rawHex` | Go struct tag | serde `rename_all = "camelCase"` |
| 清空捕获后编号从 0 再加 | `clearExchanges` | `clear_exchanges` |

## 输出契约

调试 API 路径与上游相同：

- `GET /__debuger__/api/status`
- `GET /__debuger__/api/exchanges[?conversation_id=]`
- `GET /__debuger__/api/exchanges/{id}`
- `GET /__debuger__/api/conversations`
- `DELETE /__debuger__/api/exchanges`
- `GET /__debuger__/api/events`（SSE）

前端仍请求相对路径 `api/status`，因此必须挂在 `/__debuger__/` 下。

## 验证

```sh
cargo test -p cursor-capture
cargo clippy -p cursor-capture --all-targets -- -D warnings
```

覆盖到的行为：

| 测试 | 固定的是 |
|---|---|
| `loopback_address_is_required` | 非回环地址拒绝启动 |
| `run_sse_uses_hardcoded_stream_types` | RunSSE 的请求/响应 protobuf 类型 |
| `connect_frames_decode_across_chunk_boundaries` | 5 字节 Connect 帧可跨 chunk 拼 |
| `gzip_json_content_is_pretty_printed` | gzip JSON 正文可展示 |
| `bidi_append_hex_client_message_extracts_conversation_id` | BidiAppend hex 内层 Agent 消息 |
| `store_associates_conversation_by_request_id` | 同 request_id 补会话 |
| `debug_api_serves_original_ui_and_status` | 原版 HTML、status JSON、echo 正文与完成状态 |
| `stream_completes_and_persists_both_chunks` | 第二段发送前能读到第一段，重开 SQLite 后能读到完整捕获 |
| `clear_during_stream_does_not_overwrite_reused_id` | 清空后旧流继续转发，但不能覆盖复用编号的新记录 |
| `dropping_partial_stream_records_an_error_but_empty_body_completes` | 未读完丢弃记为错误，合法空体正常完成 |

**未做的验证**：没有对真实 Cursor 安装做端到端抓包。那一步需要本机把 Cursor 指到 `127.0.0.1:9090`。

## 使用

```sh
cargo run -p cursor-capture -- --addr 127.0.0.1:9090
# 调试页：http://127.0.0.1:9090/__debuger__/
```

把 Cursor 的 API 流量指到该地址后，页面会列出 RunSSE / BidiAppend 等捕获。
