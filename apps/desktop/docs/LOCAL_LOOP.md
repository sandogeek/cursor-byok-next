# 移植记录：agent local loop

上游实现：`cursor-byok/server/src/cursor/inference/` + `server/src/api/cursor/` + `server/src/cursor/services/` + `server/src/plugin/{worker,registry,wire}.rs`。
本移植：`apps/desktop/src/{loop_service,backend,protocol,model,provider,plugins/{worker,host,wire}}.rs`。

范围只有一件事：**让 Cursor 的 local loop 在本机 Magpie 模型上跑起来**。读走 `InferenceService/RunInferenceSSE`，写走 `BidiService/BidiAppend`。不做 engine、checkpoint、工具执行，也不接管 `AgentService/Run*`。

## 目录映射

```text
cursor-byok/server/src/                         apps/desktop/src/
├── cursor/inference/service.rs                 loop_service/inference.rs   1212  按 request_id 路由 Local/Upstream
├── cursor/inference/project.rs                 loop_service/project.rs     537  InferenceStreamRequest → ModelRequest
├── cursor/inference/output.rs                  loop_service/output.rs       88  可重放的 Connect 帧广播
├── cursor/inference/overflow.rs                loop_service/overflow.rs     26  context overflow 分类
├── cursor/inference/subagent_models.rs         loop_service/subagent_models.rs 109  Task 模型解析
├── cursor/protocol/connect.rs                  protocol/connect.rs         209  Connect 5 字节帧
├── cursor/protocol/json.rs                     protocol/json.rs            104  prost JSON
├── cursor/protocol/proto.rs                    protocol/proto.rs            56  agent.v1 + run_inference
├── plugin/wire.rs                              plugins/wire.rs             320  按需 SDK JSON 投影 / ModelEvent
├── plugin/worker.rs                            plugins/worker.rs          1319  JSONL 进程传输（能力见 host_calls.rs）
├── plugin/registry.rs 的能力调用               plugins/host_calls.rs       486  fetch / stream / readRequest / 协议适配器
├── plugin/registry.rs live 目录                plugins/catalog_cache.rs    196  TTL 缓存、失败保留旧目录、按资源失效
├── plugin/registry.rs stream_model             plugins/host.rs            1712  Magpie invoke + 登记 Arc<ModelInvocation>
├── plugin/sdk/protocol/*.ts                    plugins/adapters/          3763  Chat / Responses / Anthropic
├── api/cursor/handlers.rs                      backend/mod.rs              548  路由、`PROVIDED_PATHS` 白名单、RunInferenceSSE、BidiAppend
├── api/cursor/proxy.rs                         backend/proxy.rs            276  只转发 *.cursor.sh HTTPS
├── cursor/services/account.rs                  backend/account.rs          351  本地 token → Ultra stub
├── cursor/services/analytics.rs                backend/analytics.rs        157  Statsig agent_host_local_loop
├── cursor/services/server_config.rs            backend/server_config.rs     40  http2_config=FORCE_ALL_DISABLED
├── cursor/services/compatibility.rs            backend/compatibility.rs     71  空 unary stub（三个元数据 RPC 合一）
├── cursor/services/model_catalog.rs            backend/catalog.rs          635  Magpie 并入 picker
└── cursor/services/knowledge/                  backend/knowledge.rs        532  本地 md CRUD，无 journal（读写走 spawn_blocking）
```

行数为 `wc -l` 实测，含测试和空行、不含生成代码。proto 由 `apps/desktop/build.rs` 从 `protocols/cursor/{agent_v1,run_inference}.proto` 编译；`aiserver_v1.proto` 不全量编译（重复消息名），Bidi / IsConnected 手写，schema 用字符串断言守住。

## 主路径

```text
Cursor agent-host（managed-local）
├── BidiAppend(RunInferenceClientMessage)
│   └── InferenceService.begin(request_id)
│       ├── plugin: / 已配置 Magpie → Local Run + serve()
│       └── 官方模型 → Upstream，原样转发
└── RunInferenceSSE(BidiRequestId)
    ├── Local → OutputHub 重放 Connect 帧
    └── Upstream → 官方 HTTPS

Local serve()
├── RunRequest → RunReady（vendor / summarization / subagent / 本地 rules；`accepts_client_heartbeat=true`）
├── 10s 定时 → `emit_live` 推送空 `RunInferenceHeartbeat`（不进重放历史），保活 Cursor `StallDetector`
├── InvokeModel → PluginHost.stream_model → 登记 Arc<ModelInvocation>
│                  → QuickJS 插件 VM 里的 Magpie provider.invoke（仅 providerId / model / resource）
│                  → protocol.stream → Rust 适配器从登记项读核心请求 → HTTP/SSE
│                  → ModelEvent → InferenceStreamResponse
├── CancelInvocation / FinishRun → CancellationToken
├── 最后读流断开 → 300s 重连宽限期；超时取消 Run / 插件 HTTP 并释放输出
└── ResolveSubagentModels → 已配置 Magpie hash
```

`Plugins::new` / `Harness::new` 共用一个 `PluginHost`。同步构造运行在引擎线程，没有 Tokio 上下文，不启动异步任务；首次 `snapshot()` / `status()` / `set_enabled()` 启动 backend。backend 绑 `127.0.0.1:0` 后 `set_backend_addr`。App.test.tsx 走 mock host，不启真实 backend。

## 差异清单

| # | 主题 | 上游 | nsh 版 | 原因 |
|---|---|---|---|---|
| 1 | Provider | 内置 OpenAI/Anthropic HTTP + Magpie | Magpie `provider.invoke` + 宿主协议适配器 | AGENTS.md：不做 engine；标准协议在 `plugins/adapters/`，见 `PLUGIN_PROTOCOL_ADAPTERS.md` |
| 2 | AgentService/Run* | 有 RunSSE handler + RunEngine | 不进白名单，无 handler | backend loop 需要 engine；local loop 拒绝的 turn 必须仍可达官方 |
| 3 | 知识库 | 上游优先 + journal 回放 | 只本地 md CRUD | 本仓库不做 journal 同步；读失败返回空列表，不阻断 RunInference |
| 4 | WriteGitCommitMessage | 可配本地模型生成 | 一律转发官方 | 没有 commit settings / Store |
| 5 | 模型目录 | SQLite 内置模型 + 插件模型 merge | 只 merge Magpie | 没有内置模型表 |
| 6 | 调用记录 | CallRecorder + SQLite | `calls/` + Magpie 首个 HTTP 观测 | 见 `MODEL_CALLS.md`；只记录本地 Magpie，不记录官方模型 |
| 7 | Free entitlement 缓存 | Stripe 失败时回落已确认的 free | 本地 token stub，官方 token 原样转发 | 没有 entitlement 缓存 |
| 8 | HTTP/2 bidi / Poll / WebSocket | 有传输层重映射 | 只实现 SSE + BidiAppend | `GetServerConfig.http2_config=1` 把 Cursor 钉在 HTTP/1.1 |
| 9 | 运行时 | Deno worker | 进程内独立 QuickJS VM | 沿用插件目录移植；无外部运行时 |
| 10 | 账号 stub | 本地 token 或已确认 free | 只认本应用签发的 token | 真实登录不覆盖（不变量 4） |
| 11 | 断流生命周期 | 只移除路由 | 最后读流断开保留 300s 重连；超时取消所有子 invocation、关闭输出并释放重放历史 | 客户端异常断开后不能继续消耗模型额度；短暂断线仍可恢复同一 Run |
| 12 | 文件附件 | 拒绝 `InferenceFilePart` | 保留 bytes / media type / filename；Responses 发 `input_file`，Chat 仅 PDF，Messages 仅 PDF 或 `text/*` | 按 provider 协议支持范围透传，不把未知二进制伪装成图片或文本；不支持的类型明确报错 |
| 13 | 标准协议请求 | invoke 时构造 SDK `LlmRequest` JSON 并塞进插件 | 标准路径不构造 SDK JSON；适配器直接读 `ModelInvocation`；自定义插件显式 `readRequest()` | 见 `LOCAL_LOOP_REQUEST_PRD.md`：长历史和附件不再为了选端点而序列化进插件 |
| 14 | 后端启动 | Tokio 应用启动阶段创建 | 同步构造只建共享状态；异步成员用当前 runtime 启动，`Idle → Starting → Running`；失败或 panic 回 `Idle` | nsh 引擎线程不在 Tokio 上下文中；直接 `spawn` 会 panic 并毒化锁，失败必须能被后续状态刷新重试 |
| 15 | 后端任务所有权 | 服务端持有长生命周期 `Backend` | `BackendWatch.backend: OnceLock<Backend>` 持有；`Backend` 的 `JoinHandle` 随进程存活 | 原先用 `std::mem::forget` 泄漏句柄；放进 `OnceLock` 表达“只启动一次、活到进程结束”，也让重复启动能被记录 |
| 16 | 状态锁 | `std::sync::Mutex`，poison 时清毒继续 | `parking_lot::Mutex`，无 poison 状态 | 状态锁中毒后继续用是未定义的状态；换成无 poison 的锁后不需要恢复分支，也不需要额外测试 |
| 17 | 重放历史上限 | 重放缓冲不设上限，随 Run 增长 | `OutputHub` 只保留最近 `MAX_REPLAY_FRAMES`（4096）帧，更早的帧计入 `dropped_frames`；服务端 10s `Heartbeat` 走 `emit_live` 不入历史 | 订阅者迟到时重放整条流是常态；无上限的历史让一个没人读的 Run 一直占内存，且心跳不应挤占重放窗口 |
| 18 | 适配器工具累积 | 每个适配器各自维护 `ToolState` 与 `update_tool`，`Chat` 用 `String`、`Responses` 用 `Option<String>` + `ended` | 统一为 `adapters/tools.rs` 的 `ToolState`：`merge_call_id`/`merge_name` 处理片段，`take_start`/`take_arguments_delta`/`take_end` 各自只在未发出时返回事件 | 两种协议对同一件事（何时发 start / 参数增量 / end）的簿记只写一次；`String` 与 `Option` 的差异由空值判定吸收，事件序列不变 |
| 19 | local loop 白名单 | 白名单与路由表各一份 | `backend::PROVIDED_PATHS` 是唯一来源，`harness::proxy::is_local_path` 查它 | 漂移时请求直通官方且无任何报错；合成一份后，加路由就自动可被改写 |
| 20 | 知识库读写的执行线程 | axum handler 里直接同步读目录与文件 | 每次操作一次 `spawn_blocking`，锁也在阻塞线程内获取 | `read_dir` + 逐文件 `read_to_string` 在 async worker 上会占住运行时线程；锁改 `blocking_lock` 后不再与 async 任务争用 |
| 21 | 转发请求的公共准备 | `forward_request` 与 `forward_buffered` 各写一遍路径/头部处理 | 抽 `PreparedUpstream::new` | 两份代码一旦有一边漏删 hop-by-hop 头，同一个字段会在一条路径上泄漏到上游 |
| 22 | 计数饱和转换 | 五处私有 `as_i64` / `i32_tokens` / `count_i32` / `saturating_u32` | 统一 `src/saturating.rs` | 同名函数五份，其中一份把超限值变成 `None`、其余饱和到上限；“超限时消失还是饱和”必须是一个决定 |
| 23 | 数据目录 | `harness::data_dir()` 被 backend / plugins / calls 反向依赖 | 独立 `src/storage.rs` | 取一个路径不该让四个模块依赖接管层；harness 只是它的一个调用方 |
| 24 | `subscribe` 的所有权 | `run: &Arc<Run>`，内部再 `Arc::clone` | `run: Arc<Run>`，直接移入订阅 | 调用点本来就持有 `Arc`；接受 `&Arc` 等于要求调用方保留一份所有权义务，却无法省掉克隆 |
| 25 | 魔法数字 | `mpsc::channel(64)`、错误码 `13` / `3` | `RUN_INPUT_QUEUE_FRAMES` / `INVOCATION_CODE_*` | 同一个 `13` 在别处可能是任意常量；命名后 `code: 13` 不再需要读者去查 gRPC 文档 |
| 26 | 模块名 | `service.rs`（与 `output` / `project` 并列时没有指向任何东西） | `inference.rs`，与主类型 `InferenceService` 同名 | AGENTS 约定“叶子模块以主条目命名”；`service` 在有多个 service 的仓库里不是名字 |
| 27 | 输出回放与订阅 | 无上限，回放只有帧数上限 | 历史同时受 4096 帧与 64 MiB 限制；每个订阅者是有界队列（1024 帧），满了就断开该订阅者并记 `lagged` | `mpsc::unbounded_channel` 让慢读者无限积压 `Bytes`，历史裁剪也释放不了它们。有滞后订阅者时不发结束帧，调用不会被记成完整成功 |
| 28 | 后端 serve 的生命周期 | 只保存 `JoinHandle`，没有停止语义 | 显式 `Backend::shutdown()` 发 graceful 信号并等待；`Drop` 只发信号 + `abort`，不阻塞 | 丢弃 `JoinHandle` 是 detach 不是取消；端口会在“已停止”的对象后面继续监听 |
| 29 | 后端地址的读取 | `if let Some(addr) = backend.borrow().clone()`，`watch::Ref` 临时值活在分支里 | 先 `let addr = { backend.borrow().clone() };` 再进分支 | 分支内的 await 会让 `watch` 读锁跨越 `.await`，阻塞其他发送方 |
| 30 | 后端启动的失败重试 | 启动失败后状态停在不可再试 | 失败后回到 `Idle`，下一次 `status()` / `setEnabled` 会重试 | 应用可以在引擎尚未就绪时被调用；一次失败不该让 local loop 永久关着 |

## 必须保持的不变量

1. CA Ready 前不写 Cursor 文件。
2. 停用：先清 `settings.json`，再停代理。
3. 只 MITM `*.cursor.sh`；转发上游只允许 `*.cursor.sh` HTTPS。
4. 白名单命中无 handler → 502，不旁路。接线后白名单每条都有 handler。
5. `AgentService/Run*` 不进白名单。
6. 官方模型的 BidiAppend / RunInference 原样转发。
7. `plugin:` 模型 id 永不转发官方。
8. 凭证不进 UI / 日志 / 测试快照。
9. 测试用临时目录，不读 `~/Library/Application Support/Cursor` 或 `~/.cursor-byok-next`。
10. Statsig bootstrap 必须把 `agent_host_local_loop=true`，否则 Cursor 不会走 local loop。
11. `NetworkService/IsConnected` 必须空 proto 成功，否则 Cursor 会当断网并取消在途生成。
12. 多个读流订阅并存时不能互相取消；只有最后一个订阅断开且超过重连宽限期，才允许取消整个 Run。
13. Run 正常结束仍保留末帧供迟到的读流重放；超时断开则关闭输出并释放历史，不能留下可重连的空 Run。
14. backend 只在 Tokio 上下文内启动，并发调用只能有一个启动任务；启动失败或任务 panic 必须回到 `Idle` 并唤醒地址等待者，不能永久卡住后续重试。

## 刻意不做

- `RunEngine` / checkpoint / 工具执行
- backend-loop fail-fast 草案（`docs/protocol/cursor-backend-loop-interception.md`）
- 真 HTTP/2 bidi `RunInference`、Poll、WebSocket
- 内置 OpenAI/Anthropic HTTP provider
- 知识库上游回放
- 官方 Cursor 追踪、外部 API 调用记录、首页用量概览（Magpie local loop 记录见 `MODEL_CALLS.md`）

## 验证

```sh
cargo test -p nsh-app-cursor-byok-next --lib
cd apps/desktop && nsh check && nsh test
```

`plugin_model_starts_a_local_inference_run` 覆盖：`plugin:` id 的 BidiAppend 开本地 Run，RunInferenceSSE 立刻给出 `RunReady`。`the_last_subscription_cancels_a_disconnected_run_after_its_grace_period` 用虚拟时钟覆盖重连与超时清理；`magpie_invocation_serializes_files_per_protocol` 与 `magpie_invocation_propagates_cancellation` 走真实 QuickJS 插件 + 本地假网关，验证 Responses / Chat 的 PDF、Messages 的纯文本文档，以及取消后关闭上游 HTTP。`magpie_invocation_marks_resource_invalid_on_http_401` 覆盖鉴权失败把资源标为 `invalid`。`magpie_invoke_frame_does_not_include_or_grow_with_the_model_body` 固定模型 / 资源 / 等长 ID，比较短历史、长历史和 1 MiB 附件的 `provider.invoke` 帧：帧大小不变，且无正文、工具定义或附件标记，标准路径 SDK 投影次数为 0。`magpie_catalog_through_quickjs` 继续覆盖模型目录 worker 路径；协议请求体与 SSE 等价性见 `PLUGIN_PROTOCOL_ADAPTERS.md`。真实 Cursor + 已配置 Magpie 网关的端到端 turn 仍需单独验收。
