# 协议字段分析

存放 Cursor 协议（`protocols/cursor/*.proto`）的**字段用途分析**：某个字段是谁写的、谁读的、代表什么、有哪些取值、在哪条方法/帧上出现。

本目录只回答「**这段字节是什么**」。不回答「怎么抽出来的」（提取器）、「怎么抓到的」（抓包器）、「本仓库改了什么」（移植记录）。

| 文档 | 回答什么 |
|---|---|
| `crates/cursor-protocol-extractor/` + `apps/desktop/docs/PROTOCOL_EXTRACTOR.md` | 怎么从 Cursor bundle 抽出 proto |
| `crates/cursor-capture/` + `apps/desktop/docs/CURSOR_CAPTURE.md` | 怎么抓包、解码、重放 |
| `apps/desktop/docs/<步骤>.md` | 移植了什么、和上游哪一行不一样 |
| 本目录 | 抽出来/抓到的字段**到底是什么意思** |

## 目录约定

源文件是 `protocols/cursor/agent_v1.proto`、`protocols/cursor/aiserver_v1.proto`。本目录按包拆文件，包内再按方法或消息分节；单个包大到难以浏览时才拆成多个文件。

```text
docs/protocol/
├── README.md                          本文件：术语、范围、写法、证据要求
├── agent-v1.md                        agent.v1：AgentService 方法表，RunSSE 帧结构
├── aiserver-v1.md                     aiserver.v1：服务索引，BidiAppend / InferenceStream* 字段
├── cursor-inference-service.md        InferenceService 服务级：方法表、传输降级、帧顺序、local loop 开关
├── cursor-agent-loop-routing.md       local loop 与 backend loop 的取舍：谁跑循环、何时落到哪条、降级方向
└── cursor-prompt-model-metadata.md    RunInferencePromptModelMetadata 的模型身份位（is_opus55 等）
```

新增一个方法/消息的分析时，优先在对应包文件里加一节；只有新包才新建文件，并在上表登记。

**分层规则**：多个方法共用一条链路（同一个 `BidiRequestId`、同一套重映射、同一套消息类型）时，链路级结论单独成文（如 `cursor-inference-service.md`），包文件里只留索引与字段表。

## 传输形态

**先看形态，再看字段。** 同一消息在不同形态下的字段语义会变：同一条 `RunInferenceClientMessage`，在 http2 下走双向流，在 http1 下降级成「一条 server-stream 读流 + N 次 unary append」。

本目录术语统一如下，不要再用「SSE / 流式 / 长连接」这类模糊说法：

| 形态 | HTTP 形状 | 请求体 | 响应体 | 结束信号 | 本目录实例 |
|---|---|---|---|---|---|
| **bidi** | 一次 POST（HTTP/2），双向并发 | 多帧 | 多帧 | 双方各发一次 `end_stream` | `AgentService/Run`、`InferenceService/RunInference` |
| **server-stream** | 一次 POST，多帧响应 | 单条 | 多帧 | `end_stream` 帧 / 连接关闭 | `AgentService/RunSSE`、`InferenceService/RunInferenceSSE` |
| **unary** | 一次 POST，一问一答 | 单条裸 protobuf | 单条裸 protobuf | 响应体结束 | `BidiService/BidiAppend`、`GetUsableModels` |
| **poll** | 多次 POST | 每次一个 `BidiPollRequest` | 每次一条 `BidiPollResponse`（外层帧壳，内层 `data` 为 hex） | `BidiPollResponse.eof = true` | `InferenceService/RunInferencePoll` |
| **websocket** | 协议升级 | 帧 | 帧 | 关闭握手 | `AgentService/Run`、`InferenceService/RunInference` 的 websocket arm（是否启用未确认） |
| **REST** | 普通请求 | JSON | JSON | 响应体结束 | 非 proto 的杂项接口 |

### 帧壳

bidi 与 server-stream 的每一条流帧都包一层 5 字节头；unary 没有这层壳，整个 body 就是一条消息：

```text
┌──────────────┬──────────┬─────────────────────────┐
│ flags   u8   │ len u32  │ payload（len 字节）      │
│ bit0 = 压缩   │ 大端      │ protobuf 编码            │
│ bit1 = 结束流 │          │ end_stream 时是 EndStream│
└──────────────┴──────────┴─────────────────────────┘
```

- 帧可跨任意 chunk 边界，解码必须能拼接（`cursor-capture` 的 `ConnectFrameDecoder`）。
- `end_stream` 帧的 payload 是 `connect.error.v1.EndStreamResponse`，是流终止信号，不是业务消息。
- 本仓库证据：`crates/cursor-capture/src/decode.rs:71-91`（帧头）、`:116-117`（flags 位）、`:132-137`（end_stream）；上游编码器 `jsProject/cursor-byok/server/src/cursor/protocol/connect.rs:63-93`。
- 客户端实现：`daemon.cjs:9241` 的 `BidiSSETransport` 把读流交给连接库；连接库 `transformSplitEnvelope` 对响应体按同一套 5 字节头切帧（`daemon.cjs:332`）。

poll 是两层壳。外层仍是普通的 server-stream（同样的 5 字节头，每帧一条 `BidiPollResponse`）；内层 `BidiPollResponse.data` 是 hex 文本，客户端 `Buffer.from(message.data, "hex")` 直接解出单条 `RunInferenceServerMessage`，内层不再加壳（`daemon.cjs:9241`）。终止也换成 `eof = true`，而不是 `end_stream` 帧。

### 命名陷阱：`RunSSE` 里的 "SSE"

`RunSSE` / `RunInferenceSSE` 线上走的是 Connect 的 server-streaming，**不是浏览器的 `text/event-stream` 文本帧协议**。名字只表达「一个请求进来、多个响应出去」，与 bidi 相对。

#### 内容类型

客户端请求头写 `application/connect+proto`（连接库默认 binary）。响应侧 `parseContentType3` 把 `application/connect+proto` 与 `text/event-stream` 视为**等价**的「stream + binary」（`daemon.cjs:300`）：

```text
parseContentType3
├── "text/event-stream"                → { stream: true,  binary: true }   # 硬编码别名
└── /^application\/(connect\+)?(?:(json)(?:; ?charset=utf-?8)?|(proto))$/
     stream = !!match[1]   # connect+
     binary = !!match[3]   # proto（json 时为 false）
```

两种 Content-Type 的 body 都是二进制 Connect 帧序列（上面那层 5 字节头），**不是** `data: {...}\n\n`。所以本地 handler 返回哪种都能被消费：

| 观察点 | `Content-Type` | 说明 |
|---|---|---|
| 客户端请求 / 连接库校验 | `application/connect+proto` | 二进制格式，`binary = true` |
| 上游 `cursor-byok` 本地 handler 返回 | `text/event-stream` | `server/src/api/cursor/run_sse.rs:32`、`server/src/api/cursor/handlers.rs:266`；靠上面那条别名通过校验 |

响应头 `Connect-Protocol-Version` 不是客户端校验项：客户端的 `validateResponse` 只看 status 与 Content-Type/stream 标志；`requireProtocolVersionHeader` 只作用于服务端 handler，且默认关闭。上游返回它是为对齐官方字段惯例，本仓库保留无害，但不是必需。

写文档时统一写 **server-stream**，需要提客户端名字时写成「`RunInferenceSSE`（server-stream）」。

### Cursor 的三条推理路径

形态不是二选一，而是同一个逻辑流在三种传输下的落地方式：

```text
读模式          读方向                                    写方向
──────────────────────────────────────────────────────────────────────────────────
http2           RunInference（bidi）              ← 同一条 HTTP/2 连接的另一个方向
http1_sse       RunInferenceSSE（server-stream）  ← BidiService/BidiAppend（unary × N）
http1_poll      RunInferencePoll（poll）           ← BidiService/BidiAppend（unary × N）
```

`AgentService/Run` → `RunSSE` / `RunPoll` 在同一张映射表里，走完全相同的写法，但它属于另一条 loop（backend loop）：local loop 的模型调用不走 `AgentService`，见 [`cursor-agent-loop-routing.md`](cursor-agent-loop-routing.md)。

三条路径的逻辑消息完全一致（写 `RunInferenceClientMessage` / 读 `RunInferenceServerMessage`），只有外壳不同。两条 http1 路径靠同一个 `x-request-id` 把读流和 append 拼成一条逻辑流，且**是并发的**：读流先起，append 在后台发。详见 `cursor-inference-service.md` §2。

左侧的 `http2` / `http1_sse` / `http1_poll` 是客户端 `resolveHttpTransportMode` 的**模式标识**，不是形态名；对应关系是 模式 → （读形态，写形态）。

### 形态对应的解码路径

抓包与重放时，形态决定用哪段代码：

| 形态 | 入口 | 位置 |
|---|---|---|
| unary | `decode_unary_request` / `decode_unary_response`，body 直接 protobuf 解码 | `crates/cursor-capture/src/decode.rs:163` |
| server-stream、bidi | `ConnectFrameDecoder`，按 5 字节头逐帧切 | `crates/cursor-capture/src/decode.rs:40-152` |
| poll | 外层同样走 `ConnectFrameDecoder`（每帧一条 `BidiPollResponse`），内层 `data` 的 hex 尚未解码 | `streaming_response_message_type` 见 `decode.rs:785` |

poll 与 server-stream 在 `cursor-capture` 里共用同一个解码分支，区别只在消息类型和终止信号；内层 hex 载荷要等抓到真实 `RunInferencePoll` 流量后再补。

帧壳与客户端实现的行号按 Cursor `3.30.14` 计；升级后压缩变量名与行号会变，检索方式见 `cursor-inference-service.md` §8。

### 两条 bidi 链路分属两条 loop

`BIDI_ENDPOINT_TO_SSE_METHOD_MAP`（`daemon.cjs:9366`）把 8 条 bidi 方法映射到各自 server-stream 伴生方法，其中两条主链路落在**不同的 loop** 上：

```text
[InferenceService.methods.runInference.name]  → InferenceService.methods.runInferenceSSE   ← local loop（客户端持有循环）
[AgentService.methods.run.name]               → AgentService.methods.runSSE                ← backend loop（NAL）
[ChatService ×2 / HealthService ×1 / AiService ×3] → 各自的 *SSE 伴生方法（与 local loop 无关）
```

`BIDI_ENDPOINT_TO_POLL_METHOD_MAP` 在同一行，是同一批键，因此 poll 与 server-stream 的覆盖面完全一致。

一次 agent-host turn 只会在一条 loop 上跑：`createLocalLoopTurnRouter`（`daemon.cjs:3176`）决定 `runtime`，`turnRouteLoop`（`daemon.cjs:14775`）把 `managed-local` 记成 `"local_loop"`、`connect` 记成 `"nal"`。local loop 的模型调用只走 `RunInference*` + `BidiAppend`；`AgentService/Run*` 只出现在 `connect` 分支（`AgentClientHostAdapter`，`daemon.cjs:25979`）。取舍、拒绝条件与降级方向见 [`cursor-agent-loop-routing.md`](cursor-agent-loop-routing.md)。

| loop | 要实现的方法 | 依赖 | 不实现时的后果 |
|---|---|---|---|
| local loop（本仓库目标） | `InferenceService/RunInferenceSSE` + `BidiService/BidiAppend` | 只缺 handler，白名单已有 | 请求原样打到官方 |
| local loop，bidi 主通道 | `InferenceService/RunInference` | 代理链能承载 HTTP/2 双向流 | 客户端按 `http2_config` 走 http1 降级（现状） |
| local loop，poll | `InferenceService/RunInferencePoll` + 同一套 `BidiAppend` | 额外保证 `BidiPollResponse.seqno` 严格 +1 | 回退 `Unimplemented`，客户端整轮失败 |
| backend loop（不接管） | `AgentService/Run` / `RunSSE` / `RunPoll` | 需要本地 `RunEngine` 与 checkpoint，超出「只做 local loop」的边界 | 这些 turn 直通官方（现状）；若误加白名单而没有 handler，会变成 404 |
| server-stream | `InferenceService/Stream` | 无；业务层经适配器折进 `RunInference.invoke_model` | 不影响 local loop |

**读写编解码器按链路各自复用，不跨链路**：local loop 用 `RunInferenceClientMessage` / `RunInferenceServerMessage`，backend loop 用 `AgentClientMessage` / `AgentServerMessage`。因此 `http1_sse` 是唯一能只靠 unary + server-stream 跑通 local loop 的形态，也是 BYOK 现状的选择；`RunSSE` 的 handler 只有在同时做 engine 时才有意义。

两处重映射的键集完全相同（`ChatService` 2 条、`HealthService` 1 条、`AiService` 3 条、`AgentService/Run`、`InferenceService/RunInference`，共 8 条），所以 server-stream 覆盖到的方法，poll 也覆盖，不会出现「只有一种降级可用」的中间态。

本仓库落点（只固定「形态 → 方法 → 落点」的对应关系，不记进度，避免与 `AGENTS.md` 两处状态漂移）：

| 方法 | loop | 形态 | 白名单条目 | 本地 handler | 上游同名实现（路由 / handler） |
|---|---|---|---|---|---|
| `InferenceService/RunInferenceSSE` | local | server-stream | ✓ | 无 | `handlers.rs:53` / `:235` |
| `BidiService/BidiAppend` | local | unary | ✓ | 无 | `handlers.rs:56` / `:279` |
| `InferenceService/RunInference` | local | bidi | ✗ | 无 | 隐式经 `invoke_model`（`:25620`） |
| `InferenceService/RunInferencePoll` | local | poll | ✗ | 无 | 无 |
| `InferenceService/Stream` | local | server-stream | ✗ | 无（也不需要） | 适配器复用，无独立 handler |
| `AgentService/RunSSE` | backend | server-stream | ✗（刻意移除） | 无 | `handlers.rs:51` / `:198`（上游连 engine 一起移植才有） |
| `AgentService/Run` | backend | bidi | ✗ | 无 | 经 `BidiAppend` 写入 |
| `AgentService/RunPoll` | backend | poll | ✗ | 无 | 无 |

白名单只决定「命中就改写到 loopback」，不等于有 handler：`Routing.backend` 为空时命中白名单的请求仍原样转发官方（`proxy.rs` `Routing::routes` 的 `.then(|| self.backend())`）。所以「白名单 ✓ + handler 无」在运行时等于直通，这也是接管可以先开、后端后接的原因。

反过来，`backend` 一旦就绪，命中白名单而本地没有 handler 就是硬失败（`proxy.rs` 返回 502，不向上游旁路）。`AgentService/Run*` 因此必须留在白名单外：local loop 会拒绝一部分 turn（非 userMessage/executePlan、非 agent/plan/ask/debug/multitask 模式、带 harness 覆盖、`toolRestriction` 只允许 local loop 等），这些 turn 会落到 `connect`，若被改写到本地就成了「本来能用的 turn 变 404」。

WebSocket 不在上表：它不是 Cursor 协议里的 RPC，而是 bidi 链路的并行路由，各有独立门控；接管前都要确认未命中，否则请求走 `wss://` 绕过 HTTP 白名单：

| 路由 | loop | gate / 配置名 | 创建条件 |
|---|---|---|---|
| `AgentService/Run` | backend | `nal_websocket_client`（配套 `agent_websocket_cold_pool_dial`、`nal_websocket_client_pool`） | 独立于 http2 开关 |
| `RunInference` | local | `run_inference_websocket_client`；实验臂头 `x-cursor-run-inference-websocket-arm` | 仅 `isHttp2Disabled` 为真时创建 selection（`daemon.cjs:9400`） |

两处 gate 都未确认是否启用，因此「http2 禁用 → server-stream」这条推断仍带一个未验证分支。

进度与不变量归 `apps/desktop/docs/CURSOR_TAKEOVER.md` 与 `AGENTS.md` 的进度表所有，本表不重复记载。

## 每节写法

按「方法 → 消息 → 字段」自外向内展开，一节只写一个方法，消息按**出现顺序**写，不按 proto 声明顺序写。

方法一节必须写清三件事：传输形态（上表术语）、它与哪些方法共用一条链路、它在本仓库是否属于接管范围。字段表固定这几列：

| 列 | 内容 |
|---|---|
| `#` | 字段编号，与 proto 一致 |
| 类型 | proto 类型，repeated / oneof / map / optional 显式标出 |
| 含义 | 一句话说清业务含义，不转述字段名 |
| 方向 | `客户端→服务端` / `服务端→客户端` / `双向` |
| 取值 | 枚举值、哨兵值（`0`、空串、`-1`）、可选范围 |
| 证据 | 见下 |

oneof 类型每个分支单独一行，并额外标注它在客户端侧触发的行为（流式文本、工具调用、thinking、握手等）。

## 证据要求

结论必须能追到证据，优先级从高到低：

1. **抓包**：`cursor-capture` 解出的真实帧（附方法名、请求/响应摘要，不要贴 token）；
2. **Cursor JS bundle**：提取器抽到的工厂代码、compact schema、`enumType(...)`；
3. **上游实现**：`jsProject/cursor-byok`、`jsProject/CCursor` 的对应处理；
4. **推理**：只能写在「未确认」小节，并说明推理依据。

未确认的东西不要写成事实。字段含义变了（Cursor 版本升级）时，保留旧结论并标注适用版本，而不是直接删掉。

带 bundle 证据的文档开头写一行证据版本（应用版本 / commit / build 时间，以及 `daemon.cjs`、`workbench` 指哪个文件），因为压缩变量名与行号会随版本失效。见 `cursor-inference-service.md` 开头。

## 禁止

- 不贴真实 token、cookie、账号邮箱、企业域名。
- 不把 `protocols/cursor/*.proto` 的生成方式当成字段语义的证据——生成文件只说明字段存在。
- 不把「未确认」的推理写进字段表；字段表只写有证据的结论。

## 关联

- 抓包与解码：`apps/desktop/docs/CURSOR_CAPTURE.md`
- 协议提取：`apps/desktop/docs/PROTOCOL_EXTRACTOR.md`
- local loop 只依赖的两组接口：`InferenceService/RunInferenceSSE`（读）+ `BidiService/BidiAppend`（写）；bidi 与 poll 是它们的替代路径，见 `cursor-inference-service.md`
- `AgentService/Run*` 属于 backend loop，不接管：见 `cursor-agent-loop-routing.md`
- 形态 → 方法 → 是否需要 handler 的完整对应见上文「两条 bidi 链路分属两条 loop」；进度与不变量见 `AGENTS.md` 与 `apps/desktop/docs/CURSOR_TAKEOVER.md`