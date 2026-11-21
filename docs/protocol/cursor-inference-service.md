# aiserver.v1.InferenceService 与 local loop 推理链路

本文只做**服务级**分析：方法表、传输形态、方法之间的帧与 id 关联、本仓库差距。
逐字段含义见 [`aiserver-v1.md`](aiserver-v1.md)；`RunInference` 里 `prompt_model_metadata` 的模型身份位见 [`cursor-prompt-model-metadata.md`](cursor-prompt-model-metadata.md)。
传输形态术语（bidi / server-stream / unary / poll）见 [`README.md`](README.md#传输形态)；本文出现的 `http2` / `http1_sse` / `http1_poll` 是客户端 `resolveHttpTransportMode` 的**模式标识**，不是形态名。

证据版本：Cursor `3.30.14`，commit `5f54c8173e5552412beb40702adac16275677fa0`，build `2026-11-13T06:31:38Z`
（`/Applications/Cursor.app/Contents/Resources/app/extensions/cursor-agent-host/dist/agent-host-daemon/dist/bin/build-info.json`）。
下文 `daemon.cjs` 指 `extensions/cursor-agent-host/dist/agent-host-daemon/dist/bin/daemon.cjs`，`workbench` 指 `out/vs/workbench/workbench.desktop.main.js`。
两个 bundle 都是压缩后的长行文件，同一行可能包含多个声明，行号按 `Cursor 3.30.14` 计。

## 结论

1. `aiserver.v1.InferenceService` 有 **7 个方法**，5 个 bundle 共 6 处声明一致。本仓库 `protocols/cursor/aiserver_v1.proto:58435` 已是完整 7 方法版本（`// Source: … (var: T7S)`），**不再缺 service 块**。
2. local loop 的推理主链路是 **`RunInference`（bidi）**。业务代码里只有一处 `client.runInference(...)`；`RunInferenceSSE` / `RunInferencePoll` 没有具名调用点，是传输层按 HTTP/2 是否可用做的**方法重映射**。
3. 三条路径共用同一套客户端帧：**读方向**在 `RunInference` / `RunInferenceSSE` / `RunInferencePoll` 之间切换；**http1 降级路径的写方向统一折算成 `BidiService/BidiAppend`**，靠 `x-request-id` 与读流关联。http2 路径下写方向是真正的双向流，不经过 `BidiAppend`。
4. BYOK 只需实现 `RunInferenceSSE` + `BidiAppend`，是因为服务端返回 `GetServerConfigResponse.http2_config = FORCE_ALL_DISABLED`（`1`）：客户端因此不建 http2 local loop transport，`RunInference` 被重映射为 `RunInferenceSSE`。
5. 要走真正的 http2 bidi local loop，必须让 `GetServerConfigResponse.agent_url_config.local_loop_inference_url`（field 27 → field 3）指向本机，且客户端凭证是 **PROD**——非 PROD 凭证会被 workbench **清空**该字段。

---

## 1. 服务定义（本机实测）

```proto
// aiserver_v1.proto:58435-58443
service InferenceService {
  rpc Stream(InferenceStreamRequest) returns (stream InferenceStreamResponse) {}
  rpc RecordAgentFollowupClassification(AgentFollowupCategorizationRequest) returns (google.protobuf.Empty) {}
  rpc RecordAgentPostTurnLabeling(AgentPostTurnLabelingRequest) returns (google.protobuf.Empty) {}
  rpc RunInference(stream RunInferenceClientMessage) returns (stream RunInferenceServerMessage) {}
  rpc RunInferenceSSE(BidiRequestId) returns (stream RunInferenceServerMessage) {}
  rpc RunInferencePoll(BidiPollRequest) returns (stream BidiPollResponse) {}
  rpc StartRunInferenceImagePrecaption(StartRunInferenceImagePrecaptionRequest) returns (StartRunInferenceImagePrecaptionResponse) {}
}
```

| 方法 | 输入 | 输出 | 传输形态 | proto 行 | 接管状态 |
| --- | --- | --- | --- | --- | --- |
| `Stream` | `InferenceStreamRequest` | `InferenceStreamResponse` | server-stream | 58436 | 直接推理 RPC；local loop 通过适配器折成 `invoke_model`，无需另接 |
| `RecordAgentFollowupClassification` | `AgentFollowupCategorizationRequest` | `Empty` | unary | 58437 | 埋点，不接管 |
| `RecordAgentPostTurnLabeling` | `AgentPostTurnLabelingRequest` | `Empty` | unary | 58438 | 埋点，不接管 |
| `RunInference` | `RunInferenceClientMessage` | `RunInferenceServerMessage` | bidi | 58439 | http2 主通道，未接管 |
| `RunInferenceSSE` | `BidiRequestId` | `RunInferenceServerMessage` | server-stream | 58440 | 白名单已有、无 handler |
| `RunInferencePoll` | `BidiPollRequest` | `BidiPollResponse` | poll | 58441 | 未接管 |
| `StartRunInferenceImagePrecaption` | `StartRunInferenceImagePrecaptionRequest` | `StartRunInferenceImagePrecaptionResponse` | unary | 58442 | 当前 bundle 无调用点，优先级最低 |

`PrivateInferenceService`（`aiserver_v1.proto:58482`）在 workbench 路由表里也有一份声明（`workbench:17704`），`AdmitRun` / `ReportUsage` 属于私有部署配额，不是推理帧，不接管。

### 消息复用结构

`RunInference` 的主线是「建会话 → 发起调用」，一次模型调用的载荷其实是 `InferenceStreamRequest`，与直接推理 `Stream` 方法共用：

```text
InferenceService
├── Stream(stream InferenceStreamRequest) ─────────────► stream InferenceStreamResponse      # 直接推理 RPC
└── RunInference(stream RunInferenceClientMessage) ───► stream RunInferenceServerMessage
    ├── client  (oneof message, 41145-41158；8 缺失)
    │   ├── RunInferenceRunRequest            = 1   建会话：conversation_id / requested_model / agent_mode
    │   ├── RunInferenceInvokeModel           = 2   发起一次模型调用：request: InferenceStreamRequest   ← 复用
    │   ├── RunInferenceCancelInvocation      = 3
    │   ├── RunInferenceFinishRun             = 4
    │   ├── RunInferenceClientHeartbeat       = 5
    │   ├── RunInferenceAgentEvent            = 6
    │   ├── RunInferenceRunRequestLog         = 7
    │   ├── RunInferenceResolveSubagentModels = 9
    │   ├── RunInferenceClientFirstCallTimings= 10
    │   └── RunInferenceDeltaBaseSeed         = 11  request: InferenceStreamRequest   ← 复用
    └── server  (oneof message, 41503-41511)
        ├── RunInferenceHeartbeat             = 1
        ├── RunInferenceRunReady              = 2   握手，必须先于 invocation_response
        ├── RunInferenceInvocationResponse    = 3   response: InferenceStreamResponse  ← 复用
        ├── RunInferenceInvocationEnd         = 4
        └── RunInferenceSubagentModelsResolved= 5
```

三点结构结论：

- `InferenceStreamRequest`（`aiserver_v1.proto:26314`，30 field）才是“一次模型调用”的载荷；`RunInferenceRunRequest`（41471，23 field）只建会话，字段号有跳号（13/14/23/24 缺失）。
- `InferenceStreamResponse`（26348，12 field）是 token / 工具 / thinking 的通用容器，被 `Stream` 与 `RunInferenceInvocationResponse` 共用，因此只需要一套编码器。
- **本机 local loop 不直接调用 `/Stream`**：`createInvocationClient` 把业务层的 `.stream(InferenceStreamRequest)` 适配为 `RunInference.invoke_model`，再拆掉 `invocation_response` 外层返回给原调用方（`daemon.cjs:25620-25623`）。普通调用、上下文总结、thinking note 和 prompt-cache refill 都使用这层适配，区别是外层 `purpose`（`daemon.cjs:25626-25627`）。字段级对照见 `aiserver-v1.md` §`InferenceService/Stream`。

## 2. 传输模式与降级

本节三种模式名来自客户端 bundle，注意区分两个概念：`http1_sse` / `http1_poll` 是**客户端模式标识**；它们实际使用的传输形态分别是 server-stream 与 poll，与 `text/event-stream` 无关（术语见 [`README.md`](README.md#命名陷阱runsse-里的-sse)）。

`resolveHttpTransportMode`（`daemon.cjs:9216`）：

```text
模式标识        读方向                              写方向
http2 未禁用   → "http2"        bidi RunInference（HTTP/2 双向流，同一条连接）
http2 禁用+SSE 未禁 → "http1_sse"  server-stream RunInferenceSSE  + unary BidiAppend
http2 禁用+SSE 禁用 → "http1_poll" poll RunInferencePoll          + unary BidiAppend
```

- `AGENT_TRANSPORT_MODES = ["http2","http1_sse","http1_poll","websocket"]`（`daemon.cjs:13`），`websocket` 另有单独门控（见 2.5）。
- 回退步骤是 `HTTP_PROTOCOL_FALLBACK_STEPS`（`daemon.cjs:4339`）：`http2` → `http1_sse`；`http1_poll` 由 `isHttp1SseDisabled()` 决定。
- `classifyBidiStreamLoop`（`daemon.cjs:9217`）把 `InferenceService/RunInference` 标成 `"local_loop"`，只影响埋点标签。

### 2.1 http1 路径的写方向：`BidiAppend`

`BidiBaseTransport.startYieldingInputsToTheServer`（`daemon.cjs:9235`）把客户端帧逐条转成 append：

```text
requestId    = getRequestId(header)      # 读 x-request-id；缺失则生成 UUID 并写回 header（9230）
appendSeqno  = 0,1,2,…                   # 从 0 起、逐帧 +1（9236 / 9238）
编码         = data_binary(bytes) 或 data(hex string)，二选一（9238）
               └── 由 kill switch bidi_append_binary_encoding_disabled 控制（9232 / 9392）
在途窗口     = AppendWindow(32)          # 未 ack 的 append 最多 32 条（9232）
重试         = 退避 1s→30s，连续失败 5 次转致命（9232）
```

- `BidiAppendResponse` 是空消息（`aiserver_v1.proto:4276`），ack 只有 HTTP 状态码，序号连续性由客户端负责。
- 该路径只在 `useHttp2 === false` 时存在；http2 模式下写方向就是 bidi 流本身。

### 2.2 读方向：`RunInferenceSSE`

```text
BidiBaseTransport.stream(service, method, …)       # daemon.cjs:9234
├── requestId = getRequestId(header)
├── companion = bidiEndpointMap[method.name]       # 9366 的映射表
├── method    = {...companion, O: 原 method.O}     # RunInference → RunInferenceSSE
└── outbound  → BidiSSETransport.connectForStartOfStream          # 9241
                → transport.stream(SSE method, messages=[BidiRequestId{requestId}])
                   └── 请求内容类型 application/connect+proto；响应按 5 字节帧壳切帧
```

- `BIDI_ENDPOINT_TO_SSE_METHOD_MAP`（`daemon.cjs:9366`）有 `[runInference.name] → runInferenceSSE`；`BIDI_ENDPOINT_TO_POLL_METHOD_MAP` 同一行、同键，输出类型保持 `RunInferenceServerMessage`。同一张表还包括 `[AgentService.methods.run.name] → AgentService.methods.runSSE`，即 agent 主链路走同样的降级。
- 映射只在 `BidiTransportFactory.createTransport` 的 `useHttp2 === false` 分支安装（`daemon.cjs:9244-9245`）；`useHttp2` 来自 `!isHttp2Disabled`。
- 读流与 `BidiAppend` 用的是**同一个 request id**：读流的请求消息是 `BidiRequestId{requestId}`，append 帧也带 `BidiRequestId{requestId}`，而 `requestId` 就是读流请求头里的 `x-request-id`。服务端正是靠它把两条 HTTP 请求拼成一条逻辑流。
- 本地 handler 的响应 `Content-Type` 可以是 `text/event-stream` 或 `application/connect+proto`，客户端把两者都当「stream + binary」；body 仍是 5 字节帧壳，不是 SSE 文本帧。细节见 [`README.md`](README.md#命名陷阱runsse-里的-sse)。
- 具名调用点计数（`\.runInference(SSE|Poll)?\(` 与 `\.startRunInferenceImagePrecaption\(`）：

| bundle | `runInference` | `runInferenceSSE` | `runInferencePoll` | `startRunInferenceImagePrecaption` |
| --- | --- | --- | --- | --- |
| `workbench.desktop.main.js` | 0 | 0 | 0 | 0 |
| `api/node/extensionHostProcess.js` | 0 | 0 | 0 | 0 |
| `cursor-always-local/dist/main.js` | 0 | 0 | 0 | 0 |
| `cursor-agent-host/dist/main.js` | 0 | 0 | 0 | 0 |
| `agent-host-daemon/…/daemon.cjs` | 1（25608） | 0 | 0 | 0 |

### 2.3 读方向：`RunInferencePoll`

```text
首帧   BidiPollRequest{request_id, start_request: true}
后续   BidiPollRequest{request_id}
响应   BidiPollResponse{seqno, data(hex), eof}
        └── data 按 hex 解出 RunInferenceServerMessage
```

- `seqno` 必须严格 +1，否则客户端抛 `Unexpected seqno`（`Unavailable`，`daemon.cjs:9242-9243`）。
- `bidi_append_fix` 打开时空轮询退避 1s→30s（`daemon.cjs:9244`）。

### 2.4 local loop 的独立降级

`withLocalLoopInferenceFallback`（`daemon.cjs:9363`）只包住 **local loop 的 bidi 通道**：`RunInference` 返回 `Unimplemented` / `Unavailable` 时 abort 整条流，把 `RunInference` 与 `StartRunInferenceImagePrecaption` 改回后端 `agenticComposerTransport`（`daemon.cjs:9401`），**不经过 server-stream / poll 路径**。这是与 2.1 不同层级的降级。

### 2.5 websocket

两条 bidi 主链路各有一条 WebSocket 路由，门控彼此独立：

| 路由 | gate / 配置名 | 创建条件 |
|---|---|---|
| `AgentService/Run` | `nal_websocket_client`（配套 `agent_websocket_cold_pool_dial`、`nal_websocket_client_pool`） | 独立于 http2 开关 |
| `RunInference` | `run_inference_websocket_client`（实验臂头 `x-cursor-run-inference-websocket-arm`） | 仅 `isHttp2Disabled` 为真时创建 selection（`daemon.cjs:9400`），是否启用由动态配置与 gate 决定（`daemon.cjs:9404`） |

这与「http2 禁用 → server-stream」的假设冲突：**接管前必须确认这两个 arm 都没有接管**，否则请求会走 `wss://` 而绕过 HTTP 白名单。当前未确认。

## 3. local loop 的开关：`local_loop_inference_url`

```text
GetServerConfigResponse.agent_url_config = 27          # aiserver_v1.proto:21428
└── AgentUrlConfig.local_loop_inference_url = 3        # aiserver_v1.proto:1523

workbench（控制面）
├── updateAgentUrlsFromServerConfig                    # workbench:20787
│   ├── 仅 credentialsDisplayName === PROD 才采纳
│   └── 写入 cursorCreds.localLoopInferenceUrl
└── getEffectiveCredentials                            # workbench:1867
    └── 非 PROD 时把 localLoopInferenceUrl 清空

agent-host daemon（数据面）
└── createTransportsForLatestContext                   # daemon.cjs:9395
    ├── 条件：localLoopInferenceUrl 非空 && !isHttp2Disabled && 未回退过
    ├── 建 useHttp2:true 的 transport，包 withLocalLoopInferenceFallback
    └── 否则 RunInference 走 runInferenceBackendTransport ?? agenticComposerTransport（9369）
```

两个直接后果：

| 场景 | `local_loop_inference_url` | `http2_config` | `RunInference` 实际到达 |
| --- | --- | --- | --- |
| 官方云 + `FORCE_ALL_DISABLED`（BYOK 现状） | 空 | `1` | `api2.cursor.sh/aiserver.v1.InferenceService/RunInferenceSSE` + `BidiAppend` |
| 官方云 + 默认 | 服务端下发 | `0` | `local_loop_inference_url` 上的 bidi `RunInference` |
| 自托管 / 本地后端（非 PROD 凭证） | 被客户端清空 | — | 后端自己的 `RunInference`（http2，或 http1 降级） |

所以 **BYOK 现在能跑通，靠的是“保持 PROD 凭证 + 让 `GetServerConfig` 返回 `http2_config = 1`”**，而不是本地 loop URL。
若以后要接真 bidi，必须改 `GetServerConfig` 下发 `agent_url_config.local_loop_inference_url`，并接受“只对 PROD 凭证生效”这一约束。

⚠️ 反过来，若本地实现把 `http2_config` 改成 `0`（或完全不返回该字段），客户端会保留服务端下发的 `local_loop_inference_url`，`RunInference` 就会被发到那个 URL，而不是 `api2.cursor.sh`。`harness/proxy.rs` 的白名单是按 host 匹配（`is_cursor_host`），因此**这种流量不会命中本地白名单**，会直连出去。

## 4. 服务端帧顺序契约

客户端按状态机消费 `RunInference` / `RunInferenceSSE`，帧顺序错了会被判为不兼容：

| 规则 | 证据 | 违反后果 |
| --- | --- | --- |
| 非 speculative 路径下，`run_ready` 解析完才进入 invocation 写循环 | `daemon.cjs:25613-25614`（`await ready2` → `runReady`） | 提前写会与握手状态冲突 |
| speculative 路径：客户端在 `RunInferenceRunRequest` 带 `speculative_invocations=true` + `cached_config_fingerprint`，可在 `run_ready` 之前预写 `invoke_model`；放行条件是 server heartbeat 的 `accepts_speculative_invocations` | `daemon.cjs:25602-25603` / `25609` / `25613` | 不接受 speculative 时服务端就不能回该位，否则帧会在握手完成前到达 |
| `run_ready.resolved_model` 必须存在 | `daemon.cjs:25614`，缺失即 `Internal: did not produce a resolved model` | 整条 attempt 失败 |
| `accepts_client_heartbeat` 决定是否每 5s 发 `client_heartbeat` | `daemon.cjs:25614-25615`、`25628` | 不发不影响结果，长回合可能被服务端判死 |
| `accepts_delta_invocations` / `accepts_delta_base_seeds` 决定是否启用 delta 与 seed | `daemon.cjs:25610 / 25619 / 25626` | false 时发全量，结果正确但更慢 |
| `accepts_agent_events` / `accepts_tool_call_events` 决定回传哪些帧 | `daemon.cjs:25624 / 25627` | 少于服务端预期会丢工具事件 |
| `accepts_client_first_call_timings` 且模式非 `http1_sse/poll` 时才发 timings | `daemon.cjs:14680` | 只影响埋点 |
| `invocation_end.error.refusal === CONFIG_MISMATCH` 触发整条 attempt 失败 | `daemon.cjs:25611` | 不要返回该码，除非真要客户端整轮重试 |

`run_ready.accepts_*` **全为 false 是安全的最小实现**：客户端会退到全量请求、不发心跳/事件、不做 delta（每个 `if` 都是按 true 才启用）。BYOK 本地实现应保持这个默认，逐位开启前先对齐客户端行为。

`RunInferenceServerMessage.heartbeat` 的 `accepts_speculative_invocations` 只在 speculative 场景有意义（`daemon.cjs:25609`）；普通本地实现可返回空 heartbeat 或直接不发。

## 5. 证据位置

| bundle | 位置 | 内容 |
| --- | --- | --- |
| `workbench` | 1705 | `AgentUrlConfig` 的字段描述（`local_loop_inference_url=3`） |
| `workbench` | 1867 | `getEffectiveCredentials`：非 PROD 清空 `localLoopInferenceUrl` |
| `workbench` | 17704 | 路由表 `"aiserver.v1.InferenceService":{method:["RpcName","kind"]}` |
| `workbench` | 18768 | `var T7S={typeName:"aiserver.v1.InferenceService",methods:{…}}` |
| `workbench` | 20787 | `updateAgentUrlsFromServerConfig`：`agent_url_config` → `cursorCreds` |
| `api/node/extensionHostProcess.js` | 184 | 同格式路由表 |
| `cursor-always-local/dist/main.js` | 2 | `const Lar={typeName:"aiserver.v1.InferenceService",…}`（类型名已展开） |
| `cursor-agent-host/dist/main.js` | 8 | `const i={typeName:"aiserver.v1.InferenceService",…}` |
| `daemon.cjs` | 13 | `AGENT_TRANSPORT_MODE_RESPONSE_HEADER`、`AGENT_TRANSPORT_MODES` |
| `daemon.cjs` | 4339 | `HTTP_PROTOCOL_FALLBACK_STEPS` |
| `daemon.cjs` | 8719 | `InferenceService={typeName:…,methods:{…}}`（本节 proto 的写法依据） |
| `daemon.cjs` | 8734 | `PrivateInferenceService={typeName:…}` |
| `daemon.cjs` | 9216 / 9217 | `resolveHttpTransportMode` / `classifyBidiStreamLoop` |
| `daemon.cjs` | 9230 | `getRequestId`：读/补 `x-request-id` |
| `daemon.cjs` | 9232 | `BIDI_APPEND_WINDOW` / 退避与失败上限 / 二元编码 gate 名 |
| `daemon.cjs` | 9233 / 9234 | `BidiBaseTransport` / `bidiEndpointMap` 重映射 |
| `daemon.cjs` | 9235-9239 | `startYieldingInputsToTheServer`：`BidiAppend` 写循环 |
| `daemon.cjs` | 9241 | `BidiSSETransport` / `BidiPollTransport` 的起流帧 |
| `daemon.cjs` | 9242-9243 | poll 的 seqno 校验与空轮询退避 |
| `daemon.cjs` | 9244-9245 | `BidiTransportFactory`：`useHttp2` 分支与 SSE/Poll 安装 |
| `daemon.cjs` | 9363 | `withLocalLoopInferenceFallback` 定义 |
| `daemon.cjs` | 9366 | `BIDI_ENDPOINT_TO_SSE_METHOD_MAP` / `…_POLL_…`（同一行，8 条同键；含 `AgentService/Run`） |
| `daemon.cjs` | 9369 | `localLoopInferenceRoute` 选择 |
| `daemon.cjs` | 9395 | 创建 local loop transport 并包 fallback |
| `daemon.cjs` | 9400 | `RunInference` WebSocket selection（仅 http2 禁用时） |
| `daemon.cjs` | 9401 | `fallBackLocalLoopInference` |
| `daemon.cjs` | 25594 | `sanitizeInvokeRequest`：从 `invoke_model.request` 清除 run 级模型/会话字段 |
| `daemon.cjs` | 25608 | 唯一的 `client.runInference(...)` 调用点 |
| `daemon.cjs` | 25620-25623 | `createInvocationClient`：`.stream()` ↔ `invoke_model` / `invocation_response` 适配 |
| `daemon.cjs` | 25626-25627 | 普通、总结、thinking note、cache refill 四种 invocation 的 `purpose` |
| `daemon.cjs` | 25631 / 25635 | prompt-cache refill 复用同一 `.stream()` 适配器，不证明直接调用 `/Stream` |
| `daemon.cjs` | 25636 | `withFirstStreamObserved`：消费 `first_output_timing` |
| `daemon.cjs` | 27002 | `inferenceClient:createClient(InferenceService, backendTransport)` |

## 6. 本仓库现状与差距

```text
protocols/cursor/
├── aiserver_v1.proto   58592 行；40 service / 5910 message
│   ├── service InferenceService             58435-58443   ← 7 方法，已完整
│   ├── RunInferenceClientMessage            41145-41158   ← 10 分支 oneof
│   ├── RunInferenceServerMessage            41503-41511   ← 5 分支 oneof
│   ├── InferenceStreamRequest               26314-26345   ← 30 field
│   ├── InferenceStreamResponse              26348-26363   ← 12 field
│   ├── BidiAppendRequest / BidiRequestId    4268 / 4293
│   └── GetServerConfigResponse               21401
│       └── agent_url_config = 27             21428
└── agent_v1.proto      12026 行；本节链路只用到 `agent.v1.Bidi*` 的跨包副本（2565-2580）
```

| 项 | 本仓库 | 上游 `cursor-byok` | 本机 Cursor 3.30.14 |
| --- | --- | --- | --- |
| `service InferenceService` | 7 方法（58435） | `aiserver_v1.proto:44725` 仍是 3 方法旧快照（45021 行版本） | 7 方法 |
| `RunInference*` message | 全在 `aiserver_v1.proto`，字段号与客户端一致 | 另建 `run_inference.proto`（939 行，87 message / 12 enum / 0 service）承载同一批 message | — |
| 新字段 | `InferenceStreamRequest` 30 field（含 `automation_conversation_call=30`）、`RunInferenceRunRequest` 23 field（含 26/27） | 29 / 21 field，落后一个版本 | 与仓库一致 |
| proto 编译 | `crates/cursor-capture/build.rs` 用 protox 动态编译 `agent_v1` + `aiserver_v1`，未定义类型自动降级为 `bytes`；`apps/desktop` 尚未引入 prost | `server/build.rs:7-11` 编译 `agent_v1` + `run_inference`，`aiserver_v1` 只做字符串断言 | — |
| 路径白名单 | `apps/desktop/src/harness/proxy.rs:224-225` 有 `RunInferenceSSE` + `BidiAppend` | `server/src/local_app/proxy.rs:161` 同两条 | — |
| 本地 handler | 无（`local loop 服务端接入` 仍未做） | `server/src/api/cursor/handlers.rs:53` 有 SSE 路由，`235` 起是 handler | — |
| `GetServerConfig` | 只在白名单（`proxy.rs:230-231`），无本地实现 | `server/src/cursor/services/server_config.rs:36` 返回 `http2_config = FORCE_ALL_DISABLED` | — |
| extractor 形状校验 | `crates/cursor-protocol-extractor/src/validate.rs:41-47` 要求 bidi `RunInference` + `RunInferenceSSE` | `support/…/extractor/modules.go:492-499` 同语义 | — |
| 抓包解码 | `cursor-capture` 的 `rpc_method_descriptor` 可解 `RunInferenceSSE`（server-streaming），无需新增硬编码 | — | 字段完整性受 `build.rs` 的 undefined-type 降级影响，待抓包验证 |

结论：**本仓库不需要补 proto service 块**。与上游的差异只是“schema 放在哪个文件、用什么编译”，不是字段缺失。

## 7. 若要补齐（本次不做）

| 目标 | 需要的改动 |
| --- | --- |
| 走真 http2 bidi | `GetServerConfig` 返回 `agent_url_config.local_loop_inference_url`；代理链要能承载 HTTP/2 双向流（当前 `harness/proxy.rs` 按 path 转发，未区分流类型） |
| 接 `RunInference` | `proxy.rs:224` 已有的白名单列出 `RunInferenceSSE`，bidi 方法本身需另加；后端增加 bidi handler，按 §4 的帧顺序推流 |
| 接 `RunInferencePoll` | 同上，另需保证 `BidiPollResponse.seqno` 连续 |
| `StartRunInferenceImagePrecaption` | 当前 5 个 bundle 均无具名调用点，优先级最低 |
| websocket arm | 接管前先确认 `run_inference_websocket_client` 未命中（见 2.5） |

## 8. 验证方法

```bash
APP=/Applications/Cursor.app/Contents/Resources/app
D=$APP/extensions/cursor-agent-host/dist/agent-host-daemon/dist/bin/daemon.cjs
W=$APP/out/vs/workbench/workbench.desktop.main.js

# 1) 服务方法表
rg -n 'InferenceService=\{typeName:"aiserver\.v1\.InferenceService"' $D
rg -n 'var T7S=\{typeName:"aiserver\.v1\.InferenceService"' $W

# 2) 传输模式与 SSE/Poll 重映射
rg -n 'function resolveHttpTransportMode' $D
rg -n 'BIDI_ENDPOINT_TO_SSE_METHOD_MAP=\{' $D
rg -n 'InferenceService\.methods\.runInference\.name\]:InferenceService\.methods\.runInferenceSSE' $D

# 3) http1 写方向
rg -n 'this\.bidiClient\.bidiAppend\(' $D
rg -n 'BIDI_APPEND_WINDOW=32' $D

# 4) local loop 开关
rg -n 'updateAgentUrlsFromServerConfig' $W
rg -n 'localLoopInferenceUrl&&!isHttp2Disabled' $D
rg -n 'agent_url_config = 27' protocols/cursor/aiserver_v1.proto

# 5) 本仓库现状
rg -n '^service InferenceService' protocols/cursor/aiserver_v1.proto
rg -n 'RunInference' apps/desktop/src/harness/proxy.rs
cargo check -p cursor-capture        # protox 能否编译 aiserver_v1
```

要看压缩行内的上下文时，用 `rg -o '.{0,200}KEY.{0,200}'`，不要依赖列号。

证据版本：Cursor `3.30.14`。升级后压缩变量名（`T7S`、`k_f`、`B8S` 等）与行号会变，但方法名、流类型与 bundle 内所在模块（`inference_connect.js`、`transport/handler.js`）稳定，第 8 节的检索方式仍然有效。