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
| `BidiService/BidiAppend` | 57256 | `BidiAppendRequest` | `BidiAppendResponse` | unary | 待分析 |
| `InferenceService/RunInference` | 58439 | `stream RunInferenceClientMessage` | `stream RunInferenceServerMessage` | bidi | 待分析 |
| `InferenceService/RunInferenceSSE` | 58440 | `BidiRequestId` | `stream RunInferenceServerMessage` | SSE，关联 `RunInference` | 待分析 |
| `InferenceService/RunInferencePoll` | 58441 | `BidiPollRequest` | `stream BidiPollResponse` | 轮询兜底 | 待分析 |
| `InferenceService/Stream` | 58436 | `InferenceStreamRequest` | `stream InferenceStreamResponse` | 服务端流 | 待分析（与 `RunInference` 的字段差异） |

`BidiAppend` 是 `Run*` 的前置：客户端先把首帧塞进 bidi 队列，再用 `BidiRequestId` 拉 SSE（待抓包确认这一顺序）。所以 `BidiRequestId` 的生成与生命周期要在 `BidiAppend` 一节先写清楚，再在 `RunSSE` / `RunInferenceSSE` 里引用。

## 节内格式

按 `README.md` 的字段表展开。`RunInferenceServerMessage` 是 oneof，每个分支单独一节，并额外标注它在客户端侧触发的 UI 行为（流式文本、工具调用、thinking 等）。

以下两节只列结构和待办：字段编号来自 proto，含义一栏等抓包补全。

### `BidiService/BidiAppend`

| # | 类型 | 含义 | 方向 | 取值 | 证据 |
|---|---|---|---|---|---|
| 1 | `string data` | 待确认：首帧的 JSON / proto 文本 | 客户端→服务端 | 待抓包 | proto 4269 |
| 2 | `BidiRequestId request_id` | 待确认：客户端生成还是服务端下发 | 客户端→服务端 | 见 `BidiRequestId` | proto 4270 |
| 3 | `int64 append_seqno` | 待确认：追加序号是否从 0 起、断号是否报错 | 客户端→服务端 | 待抓包 | proto 4271 |
| 4 | `bytes data_binary` | 待确认：与 `data` 二选一还是并存（line 1 与 4 的优先级） | 客户端→服务端 | 待抓包 | proto 4272 |

`BidiAppendResponse` 当前为空消息（proto 4276），因此 ack 语义只能体现在 HTTP 状态码上。

### `InferenceService/RunInference`

客户端 → 服务端 `RunInferenceClientMessage`（proto 41145），字段号有跳号（8 缺失）：

| # | 分支 | 分析状态 |
|---|---|---|
| 1 | `RunInferenceRunRequest` | 待分析（会话、模型、agent mode 的入口） |
| 2 | `RunInferenceInvokeModel` | 待分析（真正发起一次模型调用） |
| 3 | `RunInferenceCancelInvocation` | 待分析 |
| 4 | `RunInferenceFinishRun` | 待分析 |
| 5 | `RunInferenceClientHeartbeat` | 待分析（与 `run_ready.accepts_client_heartbeat` 配对） |
| 6 | `RunInferenceAgentEvent` | 待分析（工具事件回传） |
| 7 | `RunInferenceRunRequestLog` | 待分析 |
| 9 | `RunInferenceResolveSubagentModels` | 待分析 |
| 10 | `RunInferenceClientFirstCallTimings` | 待分析（遥测，可忽略） |
| 11 | `RunInferenceDeltaBaseSeed` | 待分析（prompt 前缀复用） |

服务端 → 客户端 `RunInferenceServerMessage`（proto 41503）：

| # | 分支 | 分析状态 |
|---|---|---|
| 1 | `RunInferenceHeartbeat` | 待分析 |
| 2 | `RunInferenceRunReady` | 待分析（握手：模型解析结果 + 能力开关） |
| 3 | `RunInferenceInvocationResponse` | 待分析（真正的 token / 工具调用流） |
| 4 | `RunInferenceInvocationEnd` | 待分析（结束或错误） |
| 5 | `RunInferenceSubagentModelsResolved` | 待分析 |

`RunInferenceRunReady` 里的 `accepts_*` 布尔位决定客户端后续发哪些帧，逐位含义必须和客户端行为对齐后再动，否则客户端会停发或错发。`RunInferenceInvocationResponse.response` 复用 `aiserver.v1.InferenceStreamResponse`，与 `AiService/StreamChat` 同一形状，需要判断 local loop 能否共用一套编码。

## 关联

- 抓包：`apps/desktop/docs/CURSOR_CAPTURE.md`
- 上游同语义实现：`jsProject/cursor-byok/server/src/handlers/llm/`、`jsProject/CCursor/Cursor++/src/server/handlers/llm/`
- 相邻包：`agent.v1` 的 `AgentService`（见 `agent-v1.md`）
- 模型 picker：`agent.v1.AgentService/GetUsableModels` 与 `aiserver.v1.AiService` 的模型列表方法，两者关系待确认