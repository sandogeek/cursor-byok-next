# agent.v1 字段分析

源文件：`protocols/cursor/agent_v1.proto`（约 35.7 万字符）。
本包有四个服务，只有 `AgentService` 在 local loop 的链路上（且只用到 unary）；其余三个是 Cursor 本机 agent-host 的接口，仅登记不展开。

| 服务 | proto 行 | 用途 |
|---|---|---|
| `AgentService` | 12004 | 客户端↔服务端的 agent 主链路；`Run*` 属于 backend loop，local loop 只用其中的 unary（见下节） |
| `AgentHostService` | 11933 | Cursor 本机 agent-host 与 IDE 之间的会话、turn、工具执行管理，不经服务端 |
| `AgentHostDebugShellHooks` | 11928 | 调试钩子（`SpawnDemoShell`），不实现 |
| `AgentHostSurfaceTestHooks` | 11996 | 测试钩子（auth refresh、subagent 调试），不实现 |

## AgentService 方法清单

传输形态栏是本目录首先要确认的事情：同一消息在不同形态下字段语义不同，术语见 [`README.md`](README.md#传输形态)。

本服务的 `Run*` 三条是 **backend loop（NAL）** 的读方向，不是 local loop。Cursor 的 agent-host 在同一 turn 里只会选一条 loop（`turnRouteLoop`），而 local loop 的模型调用走 `aiserver.v1.InferenceService/RunInference*`，不经过这里；`RunSSE` 在上游 `cursor-byok` 有 handler，是因为上游连 `RunEngine` 一起移植了。本仓库不做 engine，因此这三条不接管（证据见 [`cursor-agent-loop-routing.md`](cursor-agent-loop-routing.md)）。

```text
AgentService/Run       （bidi）  ──► RunSSE（server-stream） / RunPoll（poll）
AgentService/RunSSE 只吃 BidiRequestId；写方向在 http1 下折算成 BidiService/BidiAppend
```

| rpc | proto 行 | 请求 | 响应 | 传输形态 | 分析状态 |
|---|---|---|---|---|---|
| `Run` | 12005 | `stream AgentClientMessage` | `stream AgentServerMessage` | bidi | 待分析（backend loop，不接管） |
| `RunSSE` | 12006 | `BidiRequestId` | `stream AgentServerMessage` | server-stream，靠 `BidiRequestId` 关联 `Run` / `BidiAppend` | 待分析（backend loop，不接管） |
| `RunPoll` | 12007 | `BidiPollRequest` | `stream BidiPollResponse` | poll | 待分析（backend loop，不接管） |
| `NameAgent` | 12008 | `NameAgentRequest` | `NameAgentResponse` | unary | 待分析 |
| `UpdateConversationMetadata` | 12009 | `UpdateConversationMetadataRequest` | `UpdateConversationMetadataResponse` | unary | 待分析 |
| `CreateTranscriptOverview` | 12010 | `CreateTranscriptOverviewRequest` | `CreateTranscriptOverviewResponse` | unary | 待分析 |
| `GetUsableModels` | 12011 | `GetUsableModelsRequest` | `GetUsableModelsResponse` | unary | 待分析（picker 广告模型要改这里） |
| `GetDefaultModelForCli` | 12012 | `GetDefaultModelForCliRequest` | `GetDefaultModelForCliResponse` | unary | 待分析 |
| `GetAllowedModelIntents` | 12013 | `GetAllowedModelIntentsRequest` | `GetAllowedModelIntentsResponse` | unary | 待分析 |
| `UploadConversationBlobs` | 12014 | `UploadConversationBlobsRequest` | `UploadConversationBlobsResponse` | unary | 待分析（local loop 的 blob 上传走这条） |
| `UploadLocalAgentRunToPromptQuality` | 12015 | `UploadLocalAgentRunToPromptQualityRequest` | `UploadLocalAgentRunToPromptQualityResponse` | unary | 不实现（遥测） |
| `GetSignedUrlForAttachedMedia` | 12016 | `GetSignedUrlForAttachedMediaRequest` | `GetSignedUrlForAttachedMediaResponse` | unary | 待分析 |
| `NotifyConversationClone` | 12017 | `NotifyConversationCloneRequest` | `NotifyConversationCloneResponse` | unary | 待分析 |
| `WarmUp` | 12018 | `WarmUpRequest` | `WarmUpResponse` | unary | 待分析（`warmUpNetwork` 的预热） |
| `GetNewChatNudgeLegacyModelPicker` | 12019 | `GetNewChatNudgeLegacyModelPickerRequest` | `GetNewChatNudgeLegacyModelPickerResponse` | unary | 待分析（新会话提示，可能影响 picker） |
| `GetNewChatNudgeParameterizedModelPicker` | 12020 | `GetNewChatNudgeParameterizedModelPickerRequest` | `GetNewChatNudgeParameterizedModelPickerResponse` | unary | 待分析 |
| `GetPromptContextUsage` | 12021 | `GetPromptContextUsageRequest` | `GetPromptContextUsageResponse` | unary | 待分析 |
| `ListLocalSubscriptionTools` | 12022 | `ListLocalSubscriptionToolsRequest` | `ListLocalSubscriptionToolsResponse` | unary | 待分析 |
| `CallLocalSubscriptionTool` | 12023 | `CallLocalSubscriptionToolRequest` | `CallLocalSubscriptionToolResponse` | unary | 待分析 |
| `StreamLocalAgentMailbox` | 12024 | `StreamLocalAgentMailboxRequest` | `stream StreamLocalAgentMailboxResponse` | server-stream | 待分析 |

## 方法形状（待逐字段展开）

按 `README.md` 的字段表展开，一节只写一个方法，消息按出现顺序写，不按 proto 声明顺序写。

以下只列结构，字段含义待抓包补全。

### `AgentService/RunSSE`

传输：server-stream。响应体是带 5 字节帧壳的 Connect 帧序列，每帧一条 `AgentServerMessage`（帧壳见 [`README.md`](README.md#帧壳)）。

`RunSSE` 只吃一个 `BidiRequestId`，所以必然先由 `Run`（bidi）或 `aiserver.v1.BidiService/BidiAppend`（unary）建立一条 bidi 流；两条路径的先后与 id 复用规则待抓包确认。

这条流只承载 backend loop 的 turn；local loop 的 turn 从不出现 `AgentService/Run*`。两种形态的取舍与拒绝条件见 [`cursor-agent-loop-routing.md`](cursor-agent-loop-routing.md)。

请求 `BidiRequestId`（proto 2578，`aiserver.v1` 同形）：

| # | 类型 | 含义 | 方向 | 取值 | 证据 |
|---|---|---|---|---|---|
| 1 | `string request_id` | bidi 流标识 | 客户端→服务端 | 待确认（格式 / 是否 UUID） | 待抓包 |

响应 `AgentServerMessage`（proto 1924）：`ttft_breakdown` 在 oneof 之外，因此每条帧都可以带首字延迟拆解（`TtftBreakdown` 见 proto 10940）。

| # | 分支 | 分析状态 |
|---|---|---|
| 1 | `InteractionUpdate` | 待分析（思考 / 文本增量） |
| 2 | `ExecServerMessage` | 待分析（工具调用下发，与客户端 `exec_client_message` 成对） |
| 5 | `ExecServerControlMessage` | 待分析 |
| 3 | `ConversationStateStructure conversation_checkpoint_update` | 待分析 |
| 4 | `KvServerMessage` | 待分析 |
| 7 | `InteractionQuery` | 待分析（需要用户回答的提问） |

客户端 → 服务端 `AgentClientMessage`（proto 75）：

| # | 分支 | 分析状态 |
|---|---|---|
| 1 | `AgentRunRequest` | 待分析（一轮对话的入口） |
| 2 | `ExecClientMessage` | 待分析（工具调用结果） |
| 5 | `ExecClientControlMessage` | 待分析 |
| 3 | `KvClientMessage` | 待分析 |
| 4 | `ConversationAction` | 待分析 |
| 6 | `InteractionResponse` | 待分析（回答 `InteractionQuery`） |
| 7 | `ClientHeartbeat` | 待分析 |
| 8 | `PrewarmRequest` | 待分析（对应 `WarmUp`） |

帧顺序（`partialToolCall` → delta → `toolCallStarted`）以抓包为准，不按 proto 字段号推断。

## 关联

- 抓包：`apps/desktop/docs/CURSOR_CAPTURE.md`
- 两条 loop 的取舍：`cursor-agent-loop-routing.md`
- 上游实现（backend loop，需要 `RunEngine`）：`jsProject/cursor-byok/server/src/api/cursor/run_sse.rs`、`bidi.rs`
- 上游实现（client-owned loop，对应本仓库目标）：`jsProject/cursor-byok/server/src/cursor/inference/`；`jsProject/CCursor/Cursor++/src/server/handlers/agent/` 是更早的 server-owned loop（CCursor 自己跑 agent 编排），不是本仓库的参照对象
- 相邻包：`aiserver.v1` 的 `InferenceService`（见 `aiserver-v1.md`）