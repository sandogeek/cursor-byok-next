# 协议字段分析

存放 Cursor 协议（`protocols/cursor/*.proto`）的**字段用途分析**：某个字段是谁写的、谁读的、代表什么、有哪些取值、在哪条方法/帧上出现。

与既有文档的分工：

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
├── README.md           本文件：范围、写法和证据要求
├── agent-v1.md         agent.v1：AgentService / RunSSE 及其消息
├── aiserver-v1.md      aiserver.v1：InferenceService、BidiService、模型列表等
├── cursor-inference-service.md        InferenceService 服务级分析：方法表、传输模式、local loop 开关
└── cursor-prompt-model-metadata.md    RunInferencePromptModelMetadata 的模型身份位（is_opus55 等）
```

新增一个方法/消息的分析时，优先在对应包文件里加一节；只有新包才新建文件，并在上表登记。
`cursor-inference-service.md` 是 `aiserver-v1.md` 里 `InferenceService` 小节的服务级展开（方法表、传输模式、帧顺序、local loop 开关）；字段级内容仍写回 `aiserver-v1.md`。

## 每节写法

按「方法 → 消息 → 字段」自外向内展开。方法一节要写清传输形态（ConnectRPC unary / SSE / WebSocket / REST），因为同一消息在不同形态下的字段语义会变。
多个方法共用一条链路（同一个 `BidiRequestId`、同一套重映射）时，把链路级结论单独成文（如 `cursor-inference-service.md`），包文件里只留索引与字段表。

字段表固定这几列：

| 列 | 内容 |
|---|---|
| `#` | 字段编号，与 proto 一致 |
| 类型 | proto 类型，repeated / oneof / map / optional 显式标出 |
| 含义 | 一句话说清业务含义，不转述字段名 |
| 方向 | `客户端→服务端` / `服务端→客户端` / `双向` |
| 取值 | 枚举值、哨兵值（`0`、空串、`-1`）、可选范围 |
| 证据 | 见下 |

## 证据要求

结论必须能追到证据，优先级从高到低：

1. **抓包**：`cursor-capture` 解出的真实帧（附方法名、请求/响应摘要，不要贴 token）；
2. **Cursor JS bundle**：提取器抽到的工厂代码、compact schema、`enumType(...)`；
3. **上游实现**：`jsProject/cursor-byok`、`jsProject/CCursor` 的对应处理；
4. **推理**：只能写在「未确认」小节，并说明推理依据。

未确认的东西不要写成事实。字段含义变了（Cursor 版本升级）时，保留旧结论并标注适用版本，而不是直接删掉。

## 禁止

- 不贴真实 token、cookie、账号邮箱、企业域名。
- 不把 `protocols/cursor/*.proto` 的生成方式当成字段语义的证据——生成文件只说明字段存在。

## 关联

- 抓包与解码：`apps/desktop/docs/CURSOR_CAPTURE.md`
- 协议提取：`apps/desktop/docs/PROTOCOL_EXTRACTOR.md`
- local loop 只依赖的三组接口：`AgentService/RunSSE`、`InferenceService/RunInference` + `RunInferenceSSE`、`BidiService/BidiAppend`