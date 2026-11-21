# 移植记录：agent local loop

上游实现：`cursor-byok/server/src/cursor/inference/` + `server/src/api/cursor/` + `server/src/cursor/services/` + `server/src/plugin/{worker,registry,wire}.rs`。
本移植：`apps/desktop/src/{loop_service,backend,protocol,model,provider,plugins/{worker,host,wire}}.rs`。

范围只有一件事：**让 Cursor 的 local loop 在本机 Magpie 模型上跑起来**。读走 `InferenceService/RunInferenceSSE`，写走 `BidiService/BidiAppend`。不做 engine、checkpoint、工具执行，也不接管 `AgentService/Run*`。

## 目录映射

```text
cursor-byok/server/src/                         apps/desktop/src/
├── cursor/inference/service.rs                 loop_service/service.rs     859  按 request_id 路由 Local/Upstream
├── cursor/inference/project.rs                 loop_service/project.rs     374  InferenceStreamRequest → ModelRequest
├── cursor/inference/output.rs                  loop_service/output.rs       53  可重放的 Connect 帧广播
├── cursor/inference/overflow.rs                loop_service/overflow.rs     23  context overflow 分类
├── cursor/inference/subagent_models.rs         loop_service/subagent_models.rs 77  Task 模型解析
├── cursor/protocol/connect.rs                  protocol/connect.rs          55  Connect 5 字节帧
├── cursor/protocol/json.rs                     protocol/json.rs             73  prost JSON
├── cursor/protocol/proto.rs                    protocol/proto.rs            44  agent.v1 + run_inference
├── plugin/wire.rs                              plugins/wire.rs             281  LlmRequest / ModelEvent
├── plugin/worker.rs                            plugins/worker.rs           875  JSONL + Event + SSE 流
├── plugin/registry.rs stream_model             plugins/host.rs            1081  Magpie invoke
├── api/cursor/handlers.rs                      backend/mod.rs              468  路由、RunInferenceSSE、BidiAppend
├── api/cursor/proxy.rs                         backend/proxy.rs            208  只转发 *.cursor.sh HTTPS
├── cursor/services/account.rs                  backend/account.rs          336  本地 token → Ultra stub
├── cursor/services/analytics.rs                backend/analytics.rs        200  Statsig agent_host_local_loop
├── cursor/services/server_config.rs            backend/server_config.rs     52  http2_config=FORCE_ALL_DISABLED
├── cursor/services/compatibility.rs            backend/compatibility.rs     86  空 unary stub
├── cursor/services/model_catalog.rs            backend/catalog.rs          656  Magpie 并入 picker
└── cursor/services/knowledge/                  backend/knowledge.rs        484  本地 md CRUD，无 journal
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
├── RunRequest → RunReady（vendor / summarization / subagent / 本地 rules）
├── InvokeModel → PluginHost.stream_model → Magpie provider.invoke
│                  → network.stream.open/read → ModelEvent → InferenceStreamResponse
├── CancelInvocation / FinishRun → CancellationToken
└── ResolveSubagentModels → 已配置 Magpie hash
```

`Plugins::new` / `Harness::new` 共用一个 `PluginHost`。backend 绑 `127.0.0.1:0` 后 `set_backend_addr`。App.test.tsx 走 mock host，不启真实 backend。

## 差异清单

| # | 主题 | 上游 | nsh 版 | 原因 |
|---|---|---|---|---|
| 1 | Provider | 内置 OpenAI/Anthropic HTTP + Magpie | 只 Magpie `provider.invoke` | AGENTS.md：不做 engine，也不引入内置 HTTP provider |
| 2 | AgentService/Run* | 有 RunSSE handler + RunEngine | 不进白名单，无 handler | backend loop 需要 engine；local loop 拒绝的 turn 必须仍可达官方 |
| 3 | 知识库 | 上游优先 + journal 回放 | 只本地 md CRUD | 本仓库不做 journal 同步；读失败返回空列表，不阻断 RunInference |
| 4 | WriteGitCommitMessage | 可配本地模型生成 | 一律转发官方 | 没有 commit settings / Store |
| 5 | 模型目录 | SQLite 内置模型 + 插件模型 merge | 只 merge Magpie | 没有内置模型表 |
| 6 | 调用记录 | CallRecorder + SQLite | 不记录 | AGENTS.md 下一项 |
| 7 | Free entitlement 缓存 | Stripe 失败时回落已确认的 free | 本地 token stub，官方 token 原样转发 | 没有 entitlement 缓存 |
| 8 | HTTP/2 bidi / Poll / WebSocket | 有传输层重映射 | 只实现 SSE + BidiAppend | `GetServerConfig.http2_config=1` 把 Cursor 钉在 HTTP/1.1 |
| 9 | 运行时 | Deno worker | PATH 上的 Bun | 沿用插件目录移植 |
| 10 | 账号 stub | 本地 token 或已确认 free | 只认本应用签发的 token | 真实登录不覆盖（不变量 4） |

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

## 刻意不做

- `RunEngine` / checkpoint / 工具执行
- backend-loop fail-fast 草案（`docs/protocol/cursor-backend-loop-interception.md`）
- 真 HTTP/2 bidi `RunInference`、Poll、WebSocket
- 内置 OpenAI/Anthropic HTTP provider
- 知识库上游回放
- 模型调用明细

## 验证

```sh
cargo test -p nsh-app-cursor-byok-next --lib
cd apps/desktop && nsh check && nsh test
```

`plugin_model_starts_a_local_inference_run` 覆盖：`plugin:` id 的 BidiAppend 开本地 Run，RunInferenceSSE 立刻给出 `RunReady`。Magpie 真调用仍走 `magpie_catalog_through_real_bun` 的 worker 路径；invoke 需要已配置网关。
