# aiserver.v1 字段分析

源文件：`protocols/cursor/aiserver_v1.proto`（约 190 万字符，40 个服务）。
本包绝大多数方法属于 Cursor 云端产品功能，local loop 只需要 `BidiService`、`InferenceService` 两组，外加 `AiService` 里少量模型能力方法；其余按服务登记，需要时再单独开节。

## 服务索引

`local loop` 列标 ✓ 的是本仓库要接管的服务。

| 服务 | proto 行 | local loop | 用途 |
|---|---|---|---|
| `AiService` | 56606 | 部分 | 老一代 chat / edit / 模型能力接口；`AvailableModels` 与 `agent.v1.AgentService/GetUsableModels` 的关系待确认 |
| `BidiService` | 57255 | ✓ | `BidiAppend`：local loop 的写方向（读方向是 `InferenceService/RunInference*`） |
| `InferenceService` | 58435 | ✓ | `RunInference` / `RunInferenceSSE` / `RunInferencePoll`：模型推理主链路 |
| `PrivateInferenceService` | 58482 | 不接管 | 私有部署配额：`AdmitRun` + `ReportUsage`，不是推理帧 |
| `ServerConfigService` | 58542 | 待定 | `GetServerConfig`：服务端功能开关，local loop 可能也要返回一份 |
| `AuthService` | 56877 | 不接管 | 登录、邮箱、隐私模式、订阅身份（任意改写会打断真实登录） |
| `ConversationsService` | 57324 | 不接管 | commit metrics 查询，与会话 checkpoint 无关 |
| `DashboardService` | 57350 | 不接管 | 用量 / 订阅面板 |
| `McpToolService` / `MCPRegistryService` | 58451 / 58446 | 不接管 | MCP 工具注册与执行 |
| 其余 30 个服务 | — | 不接管 | 产品功能（automations、bugbot、metrics、upload 等） |

跳过的服务不写字段分析；它们的消息仍会出现在 `RunInference` 的字段里时，在那个字段下就地解释。

## local loop 两组接口（推理链）

术语见 [`README.md`](README.md#传输形态)。`RunInference` / `RunInferenceSSE` / `RunInferencePoll` 是同一个逻辑流在 http2 / http1+SSE / http1+poll 下的三条落地路径，选择规则见 `cursor-inference-service.md` §2。

这四条加 `Stream` 全部属于 local loop；`AgentService/Run*` 属于 backend loop，不在本表（见 `cursor-agent-loop-routing.md`）。

| rpc | proto 行 | 请求 | 响应 | 传输形态 | 分析状态 |
|---|---|---|---|---|---|
| `BidiService/BidiAppend` | 57256 | `BidiAppendRequest` | `BidiAppendResponse` | unary | 已确认（见下，链路见 `cursor-inference-service.md` §2.1） |
| `InferenceService/RunInference` | 58439 | `stream RunInferenceClientMessage` | `stream RunInferenceServerMessage` | bidi | 已确认（`cursor-inference-service.md` §1-§2） |
| `InferenceService/RunInferenceSSE` | 58440 | `BidiRequestId` | `stream RunInferenceServerMessage` | server-stream，关联 `RunInference` 的写流 | 已确认（同上 §2.2） |
| `InferenceService/RunInferencePoll` | 58441 | `BidiPollRequest` | `stream BidiPollResponse` | poll | 已确认（同上 §2.3） |
| `InferenceService/Stream` | 58436 | `InferenceStreamRequest` | `stream InferenceStreamResponse` | server-stream | 已确认：直接推理 RPC；local loop 将其适配成 `RunInference.invoke_model`，见下节 |

`BidiAppend` 与读流是**并发**的两条 HTTP 请求，共用同一个 `x-request-id`：`BidiBaseTransport.stream()` 先起读流，同时在后台把客户端帧逐条 `BidiAppend`（`daemon.cjs:9234-9235`）。因此不能假设「先 append 再拉读流」。`BidiRequestId` 的生成与生命周期见 `cursor-inference-service.md` §2.1。

## 节内格式

按 `README.md` 的字段表展开。`RunInferenceServerMessage` 是 oneof，每个分支单独一节，并额外标注它在客户端侧触发的 UI 行为（流式文本、工具调用、thinking 等）。

以下两节列已确认帧结构。

### `BidiService/BidiAppend`

| # | 类型 | 含义 | 方向 | 取值 | 证据 |
|---|---|---|---|---|---|
| 1 | `string data` | 客户端帧的 hex 文本；与 `data_binary` 二选一 | 客户端→服务端 | 空串表示改用 `data_binary` | proto 4269；`daemon.cjs:9238` |
| 2 | `BidiRequestId request_id` | 逻辑流 id，等于读流请求头里的 `x-request-id` | 客户端→服务端 | UUID 字符串 | proto 4270；`daemon.cjs:9230 / 9238` |
| 3 | `int64 append_seqno` | 追加序号，从 0 起逐帧 +1，服务端需按序拼接 | 客户端→服务端 | 0,1,2,… | proto 4271；`daemon.cjs:9236 / 9238` |
| 4 | `bytes data_binary` | 客户端帧的二进制编码；kill switch `bidi_append_binary_encoding_disabled` 打开时改用 field 1 | 客户端→服务端 | 未压缩 protobuf 字节 | proto 4272；`daemon.cjs:9232 / 9392` |

`BidiAppendResponse` 当前为空消息（proto 4276），因此 ack 语义只能体现在 HTTP 状态码上。

### `InferenceService/Stream`

**本机证据版本**：Cursor `3.30.14`，commit `5f54c8173e5552412beb40702adac16275677fa0`。下文 `daemon.cjs` 均指本机 `extensions/cursor-agent-host/dist/agent-host-daemon/dist/bin/daemon.cjs`。

**结论：`Stream` 与 `RunInference` 共用一次模型调用的内层载荷，但 local loop 的普通模型调用并不直接发 `/Stream`。** agent-host 业务层只持有带 `.stream()` 方法的推理 client；`createInvocationClient` 把该方法适配成外层 `RunInferenceClientMessage.invoke_model`，响应则从 `invocation_response.response` 拆回来（`daemon.cjs:25620-25623 / 25627`）。因此 `InferenceStreamRequest` 和 `InferenceStreamResponse` 是两条链路共用的模型协议，不等于两条 RPC 都要接管。

```text
ProtoPromptExecutor.stream                         daemon.cjs:25534-25535
└── client.stream(InferenceStreamRequest)
    ├── local loop 普通/总结/thinking-note/cache-refill 调用
    │   └── createInvocationClient                 daemon.cjs:25620-25623
    │       ├── C→S  RunInference.invoke_model
    │       │        ├── invocation_id = 1        ← 外层生成/保留
    │       │        └── request = 2              ← InferenceStreamRequest
    │       └── S→C  invocation_response.response ← InferenceStreamResponse
    └── 直接 RPC 形态（供同一 client 使用）
        └── InferenceService/Stream
            ├── C→S  InferenceStreamRequest
            └── S→C  stream InferenceStreamResponse
```

本机普通调用与后台 cache refill 都是通过这套适配器进入 `RunInference`，不能仅凭 `.stream()` 调用点断定网络上出现 `/Stream`。官方后端内部的实际 `/Stream` 流量仍未抓包确认；BYOK 最小接管集合仍是 `RunInferenceSSE + BidiAppend`，不需要另加 `/Stream` handler。

#### `InferenceStreamRequest`：模型调用载荷

`proto:26314-26345`，30 个字段。除特别说明外均由本机 `buildStreamRequest` 写入（`daemon.cjs:25524-25527`），方向为客户端→服务端。下面「保留」指 `sanitizeInvokeRequest` 是否把它留在 `invoke_model.request` 内（`daemon.cjs:25594`）。

| # | 类型 | 含义 | 取值 / `invoke_model` 中的处理 | 证据 |
|---|---|---|---|---|
| 1 | `repeated InferenceCoreMessage messages` | 完整对话上下文，不是仅增量消息 | system/user/assistant/tool 为 4/1/2/3；assistant 的 text、reasoning、tool_calls 分开保存；**保留** | `daemon.cjs:25508-25518 / 25524` |
| 2 | `repeated InferenceAgentTool tools` | 客户端工具目录 | name/description/parameters/custom_tool_format；parameters 是完整 JSON Schema Struct，不需要外包 `jsonSchema`；**保留** | `daemon.cjs:25521-25524` |
| 3 | `repeated InferenceNamedProviderDefinedTool provider_defined_tools` | 不属于普通函数工具的 provider 原生工具 | name/id/type/options；与 field 2 分组发送；**保留** | `daemon.cjs:25522-25524 / 25533` |
| 4 | `optional InferenceModelConfig model_config` | 本次生成参数 | max_tokens/temperature/top_p/stop_sequences/top_k/chat_template_kwargs/continue_final_message；**保留** | `daemon.cjs:25527`；proto 26180-26188 |
| 5 | `optional string model_id` | 旧/直接 RPC 的模型 id | 本机 local-loop 构造器不写；**清除**，以 run 的 `requested_model` 和 `run_ready.resolved_model` 为准 | `daemon.cjs:25524-25527 / 25594` |
| 6 | `optional string invocation_id` | 一次模型调用的关联 id | 构造时可能写入；适配器取用或生成 UUID，放到外层 field 1，**内层清除** | `daemon.cjs:25525 / 25620` |
| 7 | `optional InferenceRequestedModel requested_model` | 调用侧选择的模型与 reasoning/context 参数 | model_id/max_mode/parameters/built_in_model；**清除**，以 run 握手为准 | `daemon.cjs:25525 / 25594` |
| 8 | `optional string conversation_id` | 会话 id | **清除**，以 `RunInferenceRunRequest.conversation_id` 为准 | `daemon.cjs:25525 / 25594` |
| 9 | `repeated string accepted_unadvertised_tool_names` | 允许模型使用、但本轮未在工具目录广告的名字 | 原样字符串集合；**保留** | `daemon.cjs:25525 / 25533` |
| 10 | `optional string automation_id` | 自动化任务 id | **清除**；local-loop run 级关联，不在 invocation 内重复发送 | `daemon.cjs:25525 / 25594` |
| 11 | `optional InferenceReason inference_reason` | 特殊推理用途 | 0 未指定；1 Gemini video 子代理；2 Grok bot proactive 子代理；3 prompt cache keepalive；**清除**，改由外层 `purpose` 表达特殊调用 | `daemon.cjs:25526 / 25594`；proto 26231-26236 |
| 12 | `optional string conversation_group_id` | 会话分组 id | **清除**，以 run 级字段为准 | `daemon.cjs:25525 / 25594` |
| 13 | `optional string parent_request_id` | 父请求追踪 id | 原样；**保留** | `daemon.cjs:25526` |
| 14 | `optional string root_parent_request_id` | 调用树根部追踪 id | 原样；**保留** | `daemon.cjs:25526` |
| 15 | `optional string parent_agent_tool_call_id` | 触发子代理的父工具调用 id | 原样；**保留** | `daemon.cjs:25526` |
| 16 | `optional string subagent_type` | 子代理类型 | 原样；**保留** | `daemon.cjs:25526` |
| 17 | `optional int32 compaction_epoch` | 上下文压缩代次 | 客户端上下文回调提供的整数；**保留** | `daemon.cjs:25526 / 25535` |
| 18 | `optional string turn_unit_id` | 本地 agent loop 的 turn 单元 id | 原样；**保留** | `daemon.cjs:25526` |
| 19 | `optional string turn_unit_type` | turn 单元类别 | 原样字符串；**保留** | `daemon.cjs:25526` |
| 20 | `optional string originating_flow` | 调用来源流程标记 | 原样字符串；**保留** | `daemon.cjs:25526` |
| 21 | `optional string routing_reason` | 调用方给出的路由原因 | 原样字符串；**保留** | `daemon.cjs:25526` |
| 22 | `optional int32 summarized_message_count` | 已被摘要覆盖的消息数 | 客户端上下文回调提供的整数；**保留** | `daemon.cjs:25526 / 25535` |
| 23 | `optional bool is_simulated_msg` | 当前用户消息是否由程序模拟 | 仅明确取得布尔值时写入；**保留** | `daemon.cjs:25526 / 25533-25535` |
| 24 | `optional int32 simulated_msg_reason` | 模拟消息的原因码 | 原样整数；枚举语义未在本机 schema 中展开；**保留** | `daemon.cjs:25526 / 25535` |
| 25 | `optional bool parent_conversation_call` | 是否父会话发起的调用 | 仅 true 时写入；**保留** | `daemon.cjs:25526` |
| 26 | `optional int32 system_static_prefix_length` | 首条 system 消息中稳定前缀的长度 | 客户端按 system 内容计算；**保留**；字符数还是 token 数需抓包确认 | `daemon.cjs:25526 / 25533-25535` |
| 27 | `optional InferenceCompactionTrigger compaction_trigger` | 触发压缩时的上下文用量 | used_tokens/reserved_tokens/window_fraction；**保留** | `daemon.cjs:25523 / 25526` |
| 28 | `optional bool automation_next_run_within_hour` | 自动化下一次运行是否在一小时内 | 仅 true 时写入；**保留** | `daemon.cjs:25525` |
| 29 | `optional bool automation_previous_run_within_hour` | 自动化上一次运行是否在一小时内 | 仅 true 时写入；**保留** | `daemon.cjs:25525` |
| 30 | `optional bool automation_conversation_call` | 是否自动化会话调用 | 仅 true 时写入；**保留** | `daemon.cjs:25525` |

#### `InferenceStreamResponse`：两条链路共用的输出帧

`proto:26348-26363`；方向均为服务端→客户端。local-loop 客户端按 `invocation_id` 将 `response` 投递给对应 `.stream()` 调用方（`daemon.cjs:25610`），`invocation_end` 才是这次调用的终止帧，不能拿 `is_final` 当成整个 invocation 结束。

| # | 类型 / oneof 分支 | 含义 | 取值 / 客户端行为 | 证据 |
|---|---|---|---|---|
| 12 | `InferenceFirstOutputTiming first_output_timing` | 首次输出计时，独立于 response oneof | provider_ttft_ms/server_first_output_ms/server_dispatch_ms；可与其他分支同帧 | `daemon.cjs:25636`；proto 25983-25988 |
| 1 | `InferenceTextStreamPart text_part` | 正文增量 | text 为增量；is_final=true 表示正文段结束 | proto 26371-26375；上游 `inference/service.rs:698-705` |
| 2 | `InferenceToolCallStreamPart tool_call_part` | 工具调用及参数增量 | tool_call_id/tool_name/args/is_complete/tool_index/call_signature；完成且带签名时通知客户端保存签名 | `daemon.cjs:25610`；proto 26405-26413 |
| 3 | `InferenceUsageInfo usage` | 基础 token 用量 | prompt_tokens/completion_tokens/total_tokens | proto 26431-26436；上游 `inference/service.rs:792-812` |
| 4 | `InferenceResponseInfo response_info` | 响应 id、实际模型及规范化输出消息 | id/model/created_at/messages/error_message 等；客户端转换为最终 response；created_at 按毫秒构造 `Date` | `daemon.cjs:25523`；proto 26256-26267 |
| 5 | `InferenceExtendedUsageInfo extended_usage` | 缓存、reasoning、上下文窗口等扩展用量 | input/output/cache_read/cache_write/max_tokens/reasoning_tokens/context_input_tokens | proto 25954-25963；上游 `inference/service.rs:813-820` |
| 6 | `InferenceProviderMetadataInfo provider_metadata` | provider 私有元数据 | Struct，按 provider 名分组；本机转成 JS metadata | `daemon.cjs:25529-25530` |
| 7 | `InferenceInvocationIdInfo invocation_id` | provider 侧模型调用 id | 与外层路由 invocation_id 是不同层级，不能用来路由 RunInference 帧 | proto 26357；上游 `inference/service.rs:776-779` |
| 8 | `InferenceStreamError error` | 模型输出流内错误 | message/code/token-limit 标志/error_type；输入与输出 token limit 会映射为客户端专用错误 | `daemon.cjs:25523`；proto 26291-26310 |
| 9 | `InferenceThinkingStreamPart thinking_part` | reasoning 增量与签名 | text/signature/is_final/is_already_summarized/model_name/redacted_data | proto 26377-26384；上游 `inference/service.rs:706-733` |
| 10 | `InferenceImageDescriptionsInfo image_descriptions` | 输入图片的描述，回写到后续请求的图片 part | message_index/part_index/exp_content_index/description；不是生成图片的输出 | `daemon.cjs:25528-25529 / 25532` |
| 11 | `InferenceLineageTagsInfo lineage_tags` | 服务端为已有请求消息及 reasoning 分配的 lineage 标记 | 客户端按 message_index 写回消息，供后续请求保留；delta 模式下还更新缓存副本 | `daemon.cjs:25592-25593 / 25610` |

#### `RunInferenceInvokeModel` 相对 `Stream` 的外层增量

`proto:41274-41283`。这些是 `RunInference` 的外层 envelope 字段，**不是** `InferenceStreamRequest` 的额外版本。

| # | 类型 | 含义 | 取值 / 与 `Stream` 的差异 | 证据 |
|---|---|---|---|---|
| 1 | `string invocation_id` | 同 run 内模型调用的路由 id | 从内层 field 6 取得或生成 UUID；所有 response/end/cancel 都用它配对 | `daemon.cjs:25620-25623` |
| 2 | `InferenceStreamRequest request` | 完整模型调用载荷 | 与 `/Stream` 请求同型；上表 7 个字段已清除 | `daemon.cjs:25594 / 25622` |
| 3 | `RunInferenceInvocationPurpose purpose` | 特殊 invocation 用途 | 0 普通；1 上下文总结；2 thinking note；3 prompt cache refill；各使用独立的 `.stream()` 适配器 | `daemon.cjs:25626-25627`；proto 41253-41259 |
| 4 | `repeated RunInferenceToolCallBatch tool_call_batches` | 与调用一起上报的工具执行批次 | 仅 `run_ready.accepts_tool_call_events=true` 时积累并随下一次 invocation 发送 | `daemon.cjs:25622 / 25627` |
| 5 | `optional RunInferenceDeltaBase delta_base` | 相对哪次 invocation 做增量请求 | 只有握手同时接受 delta 才启用；最小实现不广告即可始终全量 | `daemon.cjs:25606 / 25610` |
| 6 | `optional bytes config_fingerprint` | speculative run 的配置指纹 | speculative 写流标签解析后附带；普通路径可为空 | `daemon.cjs:25621-25622` |
| 7 | `RunInferenceCompressedRequest compressed_request` | 压缩后的 `InferenceStreamRequest` | 与 field 2 二选一；只有服务端通过 `request_compression` 广告且抽到 treatment 才使用 | `daemon.cjs:25612-25614 / 25622` |
| 8 | `RunInferenceRequestEncodingArm request_encoding_arm` | 请求压缩实验臂 | 0 未指定、1 control、2 treatment；control 不发送压缩体 | `daemon.cjs:25612 / 25622` |

#### 对 local loop 实现的约束

1. **模型以 `RunInferenceRunRequest.requested_model` / `run_ready.resolved_model` 为准，会话以 `RunInferenceRunRequest.conversation_id` 为准**，不要依赖已被清空的 `invoke_model.request.requested_model`。
2. **复用一套 `InferenceStreamRequest → provider` 投影与 `provider event → InferenceStreamResponse` 编码器**；`RunInference` 只额外负责 run、invocation id、cancel/end 与能力协商。
3. `tools.parameters` 直接就是 JSON Schema；上游 `cursor-byok` 当前 `project.rs` 对外包 `jsonSchema` 的兼容输入仍可取内层，但不应把它误当本机唯一合法形状。
4. 不广告 `accepts_delta_invocations`、`request_compression`、`prompt_cache_refill` 时，可只处理未压缩全量请求；仍需实现每次 invocation 的 `invocation_end`，包括无错误结束。
5. `/Stream` 的独立 server-stream handler 当前不是最小接管项；若未来启用，应复用同一编码器，而不是返回 `RunInferenceServerMessage` 或在 `RunInferenceSSE` 上模拟它。

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
- 上游同语义实现：`jsProject/cursor-byok/server/src/cursor/inference/`（请求投影、调用生命周期、响应编码）
- 相邻包：`agent.v1` 的 `AgentService`（见 `agent-v1.md`；其中 `Run*` 属 backend loop，不接管）
- 两条 loop 的取舍：`cursor-agent-loop-routing.md`
- 模型 picker：`agent.v1.AgentService/GetUsableModels` 与 `aiserver.v1.AiService` 的模型列表方法，两者关系待确认