# aiserver.v1 字段分析

源文件：`protocols/cursor/aiserver_v1.proto`（约 190 万字符，40 个服务）。
本包绝大多数方法属于 Cursor 云端产品功能，local loop 只需要 `BidiService`、`InferenceService` 两组，外加 `AiService` 里少量模型能力方法；其余按服务登记，需要时再单独开节。

## 服务索引

`local loop` 列标 ✓ 的是本仓库要接管的服务。

| 服务 | proto 行 | local loop | 用途 |
|---|---|---|---|
| `AiService` | 56606 | 部分 | 老一代 chat / edit / 模型能力接口；`AvailableModels` 与 `agent.v1.AgentService/GetUsableModels` 的关系待确认 |
| `BidiService` | 57255 | ✓ | `BidiAppend`：`Run` / `RunSSE` 的 bidi 帧入口 |
| `InferenceService` | 58435 | ✓ | `RunInference` / `RunInferenceSSE` / `RunInferencePoll`：模型推理主链路 |
| `PrivateInferenceService` | 58482 | 不接管 | 私有部署配额：`AdmitRun` + `ReportUsage`，不是推理帧 |
| `ServerConfigService` | 58542 | 待定 | `GetServerConfig`：服务端功能开关，local loop 可能也要返回一份 |
| `AuthService` | 56877 | 不接管 | 登录、邮箱、隐私模式、订阅身份（任意改写会打断真实登录） |
| `ConversationsService` | 57324 | 不接管 | commit metrics 查询，与会话 checkpoint 无关 |
| `DashboardService` | 57350 | 不接管 | 用量 / 订阅面板 |
| `McpToolService` / `MCPRegistryService` | 58451 / 58446 | 不接管 | MCP 工具注册与执行 |
| 其余 30 个服务 | — | 不接管 | 产品功能（automations、bugbot、metrics、upload 等） |

跳过的服务不写字段分析；它们的消息仍会出现在 `RunInference` 的字段里时，在那个字段下就地解释。

## local loop 三组接口

| rpc | proto 行 | 请求 | 响应 | 传输形态 | 分析状态 |
|---|---|---|---|---|---|
| `BidiService/BidiAppend` | 57256 | `BidiAppendRequest` | `BidiAppendResponse` | unary | 已确认（见下，链路见 `cursor-inference-service.md` §2.1） |
| `InferenceService/RunInference` | 58439 | `stream RunInferenceClientMessage` | `stream RunInferenceServerMessage` | bidi | 已确认（`cursor-inference-service.md` §1-§2） |
| `InferenceService/RunInferenceSSE` | 58440 | `BidiRequestId` | `stream RunInferenceServerMessage` | SSE，关联 `RunInference` 的写流 | 已确认（同上 §2.2） |
| `InferenceService/RunInferencePoll` | 58441 | `BidiPollRequest` | `stream BidiPollResponse` | 轮询兜底 | 已确认（同上 §2.3） |
| `InferenceService/Stream` | 58436 | `InferenceStreamRequest` | `stream InferenceStreamResponse` | 服务端流 | 待分析（与 `RunInference` 的字段差异） |

`BidiAppend` 与读流是**并发**的两条 HTTP 请求，共用同一个 `x-request-id`：`BidiBaseTransport.stream()` 先起读流，同时在后台把客户端帧逐条 `BidiAppend`（`daemon.cjs:9234-9235`）。因此不能假设“先 append 再拉 SSE”。`BidiRequestId` 的生成与生命周期见 `cursor-inference-service.md` §2.1。

## 节内格式

按 `README.md` 的字段表展开。`RunInferenceServerMessage` 是 oneof，每个分支单独一节，并额外标注它在客户端侧触发的 UI 行为（流式文本、工具调用、thinking 等）。

以下两节只列结构和待办：字段编号来自 proto，含义一栏等抓包补全。

### `BidiService/BidiAppend`

| # | 类型 | 含义 | 方向 | 取值 | 证据 |
|---|---|---|---|---|---|
| 1 | `string data` | 客户端帧的 hex 文本；与 `data_binary` 二选一 | 客户端→服务端 | 空串表示改用 `data_binary` | proto 4269；`daemon.cjs:9238` |
| 2 | `BidiRequestId request_id` | 逻辑流 id，等于读流请求头里的 `x-request-id` | 客户端→服务端 | UUID 字符串 | proto 4270；`daemon.cjs:9230 / 9238` |
| 3 | `int64 append_seqno` | 追加序号，从 0 起逐帧 +1，服务端需按序拼接 | 客户端→服务端 | 0,1,2,… | proto 4271；`daemon.cjs:9236 / 9238` |
| 4 | `bytes data_binary` | 客户端帧的二进制编码；kill switch `bidi_append_binary_encoding_disabled` 打开时改用 field 1 | 客户端→服务端 | 未压缩 protobuf 字节 | proto 4272；`daemon.cjs:9232 / 9392` |

`BidiAppendResponse` 当前为空消息（proto 4276），因此 ack 语义只能体现在 HTTP 状态码上。

### `InferenceService/RunInference`

客户端 → 服务端 `RunInferenceClientMessage`（proto 41145），字段号有跳号（8 缺失）：

| # | 分支 | 分析状态 |
|---|---|---|
| 1 | `RunInferenceRunRequest` | 建会话；字段见 `cursor-inference-service.md` §1 |
| 2 | `RunInferenceInvokeModel` | 真正发起一次模型调用，载荷是 `InferenceStreamRequest`（`request=2`） |
| 3 | `RunInferenceCancelInvocation` | 仅 `invocation_id` |
| 4 | `RunInferenceFinishRun` | `aborted` / `turn_completed` / `tool_call_batches`；是否带 `aborted` 取决于 `accepts_aborted_finish` |
| 5 | `RunInferenceClientHeartbeat` | 与 `run_ready.accepts_client_heartbeat` 配对，客户端默认 5s 一次 |
| 6 | `RunInferenceAgentEvent` | 工具事件回传，受 `accepts_agent_events` 控制 |
| 7 | `RunInferenceRunRequestLog` | `agent_run_request` 原样字节，受 `agent_run_request_log_requested` 控制 |
| 9 | `RunInferenceResolveSubagentModels` | 子代理模型解析请求，响应在 server 分支 5 |
| 10 | `RunInferenceClientFirstCallTimings` | 遥测；`http1_sse/poll` 下改用 client heartbeat 携带 |
| 11 | `RunInferenceDeltaBaseSeed` | prompt 前缀复用，受 `accepts_delta_invocations` + `accepts_delta_base_seeds` 双重控制 |

服务端 → 客户端 `RunInferenceServerMessage`（proto 41503）：

| # | 分支 | 分析状态 |
|---|---|---|
| 1 | `RunInferenceHeartbeat` | `accepts_speculative_invocations` 等预写协商，非 speculative 场景可空 |
| 2 | `RunInferenceRunReady` | 握手；`resolved_model` 必须存在，`accepts_*` 位见 `cursor-inference-service.md` §4 |
| 3 | `RunInferenceInvocationResponse` | `response: InferenceStreamResponse`，真正的 token / 工具调用流 |
| 4 | `RunInferenceInvocationEnd` | `error` / `delta_refused`；`refusal=CONFIG_MISMATCH` 会使客户端整轮失败 |
| 5 | `RunInferenceSubagentModelsResolved` | 与 client 分支 9 的 `resolution_id` 配对 |

`RunInferenceRunReady` 里的 `accepts_*` 布尔位决定客户端后续发哪些帧，逐位含义必须和客户端行为对齐后再动，否则客户端会停发或错发。逐位契约见 `cursor-inference-service.md` §4。
`RunInferenceInvocationResponse.response` 就是 `aiserver.v1.InferenceStreamResponse`，与 `Stream` 方法共用同一形状，因此 local loop 只需要一套 `InferenceStreamResponse` 编码器。

## 关联

- 抓包：`apps/desktop/docs/CURSOR_CAPTURE.md`
- 上游同语义实现：`jsProject/cursor-byok/server/src/handlers/llm/`、`jsProject/CCursor/Cursor++/src/server/handlers/llm/`
- 相邻包：`agent.v1` 的 `AgentService`（见 `agent-v1.md`）
- 模型 picker：`agent.v1.AgentService/GetUsableModels` 与 `aiserver.v1.AiService` 的模型列表方法，两者关系待确认