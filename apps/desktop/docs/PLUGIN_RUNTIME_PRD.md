# PRD：插件协议适配器下沉 Rust

状态：已实现  
产品：Cursor-BYOK-Next 桌面端

## 1. 目标

把 Bun SDK 里随应用分发的三个 TS 协议 helper（`cursor-byok-next:protocol/openai-chat`、`openai-responses`、`anthropic`，共约 1200 行）改为宿主的 Rust 实现：构建请求体、发起 HTTP、解析 SSE、产出归一化事件。

插件其余逻辑全部留在 TS：插件注册契约、资源（表单 / 展示 / 移除）、实时目录拉取与解析、`native_endpoints` 协议选择、鉴权头、模型 ID、错误分类（如 `401` → 资源失效）。宿主只增加一个完成上述迁移所必需的接口 `PluginContext.protocol.stream(...)`；现有 `:plugin` / `:provider` / `:model` / `:resource` 契约与 `network.fetch` / `network.stream` 保持不变。

`cursor-byok-next:protocol/*` 直接删除，不保留别名、适配层或双路径开关。旧 SDK 调用方不在兼容范围内。

## 2. 用户行为

- **Magpie 用户**：行为不变。在插件页连接 / 删除 / 刷新网关，从 Cursor picker 选择已发现的模型并发起调用；流式输出、thinking、工具调用、用量、取消与调用记录与迁移前一致。
- **未安装 Bun 的用户**：与现状一致，Bun 插件显示自身运行时不可用；Magpie 仍是 Bun 插件，因此同样不可用。本期不引入“无 Bun 也能跑 Magpie”的路径。
- **插件作者**：不再引用 `:protocol/*`。标准 OpenAI Chat / OpenAI Responses / Anthropic Messages 转发改为调用 `context.protocol.stream`；需要自定义上游协议的项目继续用 `context.network.stream` 自行实现并 `output.emit`。

## 3. 范围外

- 不引入 NSH 插件类型、注册项或执行派发；Magpie 仍是随应用分发的 Bun 插件，不变为 Rust 插件。
- 不迁移资源创建、资源展示、目录拉取 / 解析、协议选择、鉴权头、错误分类到 Rust。
- 不移植上游 Rust provider 的模型路由、`ProviderConfig`、重试、`extraParams` 合并、body allowlist 等非协议逻辑。
- 不兼容旧版插件 SDK、旧模型 ID 或旧资源存储格式；不做数据迁移或双写。
- 不新增第三方插件扫描、安装、更新或市场；不交付 `.nshpkg` 配置页。
- 不改变 Cursor 接管、代理白名单、`AgentService/Run*`、engine / checkpoint / 工具执行与官方转发（官方转发仍不落库）。
- 不把协议 helper 做成 Rust ↔ TS 双向桥以外的通用 IPC；宿主接口只有本节第 5 条定义的调用。

## 4. 目标结构与职责

现状：宿主加载内嵌 Magpie 的 Bun descriptor，经 worker 调用；`provider.ts` 调用三个 `:protocol/*` helper 完成请求体与 SSE 归一化。目标结构如下；现有模块优先在原位置拆分职责。

```text
apps/desktop/
├── src/
│   ├── plugins/
│   │   ├── protocol.rs    # 现有 Bun JSONL 帧；HostError 增加可选 status
│   │   ├── adapters/      # 新增：只服务插件的协议适配器（不与 protocol.rs 的帧定义混用）
│   │   │   ├── mod.rs     # 协议枚举、统一入口、URL 校验、取消/超时/上限、CallRecorder 接入
│   │   │   ├── chat.rs    # Chat Completions：请求体 + SSE → ModelEvent
│   │   │   ├── responses.rs # Responses：请求体 + SSE → ModelEvent
│   │   │   └── anthropic.rs # Messages：请求体 + SSE → ModelEvent
│   │   ├── worker.rs      # Bun 检测、JSONL、network 服务；新增 protocol.stream 宿主调用
│   │   ├── host.rs        # 现有编排；provider.invoke 时登记 Arc<ModelInvocation>
│   │   ├── wire.rs        # 按需 core → SDK JSON 投影；标准 adapter 不走此路径
│   │   └── sdk/
│   │       ├── plugin.ts  # 增 protocol 能力与 ProtocolHttpError；删除 protocol/ 目录
│   │       ├── worker.ts  # 转接 protocol.stream 宿主调用；host_error 带 status 时抛 ProtocolHttpError
│   │       ├── provider.ts / model.ts / resource.ts / collect.ts  # 不变
│   │       └── tsconfig.json # 只保留 :plugin :provider :model :resource 映射
│   └── provider.rs        # 统一 ModelEvent（适配器直接产出，不再回 JSON）
└── plugins/build-in/magpie/ # 保留 Bun 插件；provider.ts 改为调 context.protocol.stream
```

```text
目录：Magpie TS models.list → context.network.fetch → TS 解析 /v1/models → 宿主 live 缓存
推理：Cursor local loop → PluginHost.stream_model
     → Bun Magpie provider.invoke（仅 providerId / model / resource）
       ├─ TS：选协议、选端点、组鉴权头、401/协议错误分类
       └─ hostCall "protocol.stream" {protocol, url, model, headers}
          → Rust 适配器：宿主登记的 ModelInvocation → 请求体 → 网关 HTTP/SSE → ModelEvent
          → 事件直写本次 invocation 流；HTTP 状态/错误回给 TS
     自定义 TS 协议：显式 await input.readRequest() 才投影 SDK JSON
     → local loop 生成 Cursor 响应帧
观测：适配器的首个请求 claim CallRecorder → calls.db → 调用记录页
```

## 5. 接口契约

### 5.1 SDK（`sdk/plugin.ts`）

```ts
export type ProtocolStreamInput = {
  protocol: "openai-chat" | "openai-responses" | "anthropic";
  /** 上游固定端点；origin 必须是本次绑定资源或 plugin.json 声明的 HTTPS 主机。 */
  url: string;
  /** 发给网关的裸上游模型 ID。 */
  model: string;
  headers?: Record<string, string>;
};

export class ProtocolHttpError extends Error {
  readonly status: number;
  readonly body: string;
}

export type PluginContext = {
  network: { fetch(...): ...; stream(...): ... }; // 不变
  protocol: {
    /** 宿主构建请求体、发起 HTTP、解析 SSE；事件写入本次 invocation 流。 */
    stream(input: ProtocolStreamInput): Promise<void>;
  };
  signal: AbortSignal;
};
```

- 事件由宿主直接写入本次 invocation 的流，TS 不中转；`ProviderOutput.emit` 仅保留给自行实现协议的插件。
- 非 2xx 抛 `ProtocolHttpError(status, body)`（`message` 为 `HTTP <status>: <body>`，与原 helper 一致）；流内失败抛 `Error`。
- invocation 取消时 `stream` 以 reject 结束，并立即停止读取上游。

### 5.2 JSONL

`host_call` 使用 `method: "protocol.stream"`，`params` 即 `ProtocolStreamInput`。`host_error` 增加可选 `status`，worker 据此构造 `ProtocolHttpError`。适配器事件不经过 worker/JSONL，避免事件二次编解码。

### 5.3 请求体来源

已被 [local loop 请求 PRD](LOCAL_LOOP_REQUEST_PRD.md) 替代：标准协议路径不再构造 SDK `LlmRequest`。适配器**只**使用宿主在 `invoke` 时登记的 `Arc<ModelInvocation>`；`protocol.stream` 参数里不含消息体或工具定义。自定义 TS 协议显式 `await input.readRequest()` 才投影 SDK JSON。理由：请求投影只有一个所有者（宿主），长历史和附件不再为了选端点而序列化进 Bun。

### 5.4 执行语义（沿用现有 worker 网络调用）

- **URL 校验**：与 `network.fetch/stream` 同一套白名单（资源 origin 或 `plugin.json` 的 HTTPS 主机），拒绝带凭证的 URL，不跟随重定向。
- **记录**：调用内第一个协议请求 claim `CallRecorder`，请求头按现有规则脱敏，请求体与响应块落库；记录失败不阻断推理，官方转发仍不落库。
- **取消与断连**：取消信号贯穿连接等待与响应读取；取消或宽限期结束后停止读取并关闭上游连接；缺少协议终止事件的断流按失败处理，不静默成功。
- **上限与超时**：连接与整体超时沿用现有网络服务常量；错误响应体读取设上限（64 KiB，超出截断）后交给插件分类。

## 6. 功能要求与不变量

1. **策略在 TS，协议在 Rust。** 协议选择、端点拼装、鉴权头、资源有效性（`401` → `invalid`）、目录解析一律留在 TS；Rust 只做请求体、HTTP/SSE 与事件归一化。两边都不引入第三条路径。
2. **同一份输入产出同一份输出。** 对同一核心 `ModelInvocation` 与上游响应，Rust 适配器的请求体与事件序列必须与迁移前 TS helper 等价；等价性由 golden fixture（请求体 + 事件序列，脱敏、无凭证）锁定。标准路径不经过 SDK JSON；「adapter 输入必须与 SDK 同 schema」已由 [local loop 请求 PRD](LOCAL_LOOP_REQUEST_PRD.md) 替代。
3. **事件帧边界不变。** 文本 / 思考 / 每个工具的 start → delta → end 顺序、回放状态在流结束前一次发出、`usage` 与 `done` 的先后都与现行为一致。
4. **网络与凭证边界不放宽。** Magpie 只请求本次绑定的网关 origin 和固定路径；密钥不出现在 UI、日志、测试快照与插件目录错误里；目录错误不展示网关响应原文。
5. **生命周期归宿主。** 一次 `InvokeModel` 建立一条调用记录；配置页关闭不取消推理；短暂断线在现有宽限期内重连同一 Run 且不重发上游请求。
6. **观测不改变推理结果。** `completed` / `error` / `cancelled` 不互相覆盖；观测存储失败只记日志。
7. **删除即完成。** 生产代码、tsconfig 映射与 `definition.rs` 内嵌列表都不再出现 `cursor-byok-next:protocol/`；Bun 核心 SDK 与 worker 仍可独立运行。
8. **UI 不变。** 插件页只镜像宿主状态；用户可见文案仍走 `locales/`，不展示内部协议错误或凭证。

## 7. 协议行为基线（逐项保留）

| 协议 | 必须保留的行为 |
|---|---|
| OpenAI Chat | 纯文本消息压成字符串、混合图片展开为 `image_url`；非 PDF 附件报错；assistant 无可见内容且无工具调用时跳过；`reasoning_content` / `reasoning` 增量驱动 thinking 边界；工具 `id` / `name` 片段去重后拼接，`callId + name` 齐备才发 start；流末补未开始工具的 start + 参数 + end；`stream_options.include_usage`；`max_completion_tokens`、`reasoning_effort`、`service_tier: "fast"`、`prompt_cache_key`；`[DONE]` 结束且无 `finish_reason` 时按是否见工具推断；两者都没有时报错；回放 `{ providerKind: "openai_chat", value: { reasoning_content } }` |
| OpenAI Responses | `instructions`、`include: ["reasoning.encrypted_content"]`、工具 `strict: false`、`service_tier: "priority"`、`prompt_cache_key`；assistant 回放只接受 reasoning 项，缺失或异类报错；`function_call_arguments.delta` 与最终 `arguments` 快照不一致时报错；`output_text.delta` 与 `output_text.done` 文本 reconcile 补后缀；`response.completed` / `incomplete` / `failed` 三种终止；`sawCompletedItem` 兜底；无终止事件报错 |
| Anthropic Messages | `system` 数组 + `cache_control`；最后一个 user 内容块打 cache breakpoint；最后一个工具打 `cache_control`；连续同角色消息合并；thinking `adaptive` / `summarized` 与 `output_config.effort`；PDF 走 base64 `document`，`text/*` 解码为 `plain_text` 附件，其余附件报错；`redacted_thinking` 直接进回放；thinking 文本与 signature 分别累积，只有带 signature 的块进回放；usage 逐字段取最大值并重算 total；`message_stop` 终止，`stop_reason` 映射 `stop` / `length` / `tool-use`，缺失终止事件按现状报错 |

错误文案与新旧差异：

- 请求体字段名、附件编码与错误消息保持原 helper 文本（如 “OpenAI Chat Completions only supports PDF file attachments”“Anthropic stream ended without message_stop”）。
- 唯一有意的差异：`ProtocolHttpError.body` 有 64 KiB 截断上限，避免超长响应体进入调用记录与错误链。差异写入移植记录。

## 8. 交付顺序

1. **接口与桥**：新增 `context.protocol.stream`、invocation 请求登记、`host_error.status`、适配器事件直通；用本地假网关只跑通 Chat，确认 Magpie `provider.ts` 不再引用任何 helper。
2. **迁移 Responses 与 Anthropic**：逐协议把 body 构建与 SSE 解析写成 Rust，附上从旧 helper 抓取的 golden fixture 与假网关端到端测试。此步保留旧 helper 仅作对照，不接入调用链。
3. **删除**：删 `sdk/protocol/`、specifier 映射、`definition.rs` 内嵌列表、旧 helper 与对照代码；跑全量测试，确认无残留引用。

## 9. 验收清单

- [x] **接口**：Magpie 仅通过 `context.protocol.stream` 调上游；插件与 SDK 中不再出现 `:protocol/*`。
- [x] **行为等价**：适配器 golden + 假网关覆盖三类协议端点、裸模型 ID、鉴权头、附件、流式工具、thinking / 回放、usage、结束原因、`401` → 资源 `invalid`、非 2xx 与流内失败。
- [x] **边界**：非白名单 origin 被拒、带凭证 URL 被拒、不跟随重定向；取消后上游连接实际关闭；断开流判定为失败。
- [x] **观测**：完成 / 失败 / 取消 / 详细模式与脱敏测试继续通过；详细快照来自首个协议请求。
- [x] **目录未受影响**：`/v1/models` 拉取与解析仍在 TS；失败保留旧缓存，无缓存不上假模型，目录错误不含响应原文。
- [x] **无 Bun**：插件显示自身运行时不可用，宿主与其余功能不受影响。
- [x] **删除完成**：生产代码与 SDK 中无 `cursor-byok-next:protocol`；Bun 核心 SDK 与 worker 可独立运行。
- [ ] **发布验证**：自动化 `cargo test` / `nsh check` / `nsh test`；真实 Cursor + Magpie 端到端 turn 仍需单独验收。

## 10. 文档同步
实现时在本目录新增移植对照记录 `PLUGIN_PROTOCOL_ADAPTERS.md`：目录映射、逐项差异及原因、不变量与测试映射、golden fixture 来源。同步更新 [插件目录](PLUGIN_CATALOG.md) 的 SDK specifier 与行数清单、[local loop](LOCAL_LOOP.md) 的调用路径、[调用记录](MODEL_CALLS.md) 的记录接入点，以及根目录 `AGENTS.md` 中的 SDK specifier 与插件目录结构。协议字段证据仍归 `docs/protocol/`，不写进本 PRD。
