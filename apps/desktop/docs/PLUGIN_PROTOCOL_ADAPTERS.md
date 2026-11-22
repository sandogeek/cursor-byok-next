# 移植记录：插件协议适配器下沉 Rust

上游实现：`cursor-byok/server/src/plugin/sdk/protocol/{openai_chat,openai_responses,anthropic}.ts`。
本移植：`apps/desktop/src/plugins/adapters/` + `sdk/{plugin,worker}.ts` + Magpie `provider.ts`。

范围只有一件事：**把三个 TS 协议 helper 改成宿主 Rust 适配器**。插件仍用 Bun 选协议、拼端点、组鉴权头并分类错误；请求体、HTTP/SSE 与事件归一化归 Rust。不引入 NSH 插件类型，不迁移目录拉取。

## 目录映射

```text
cursor-byok/server/src/plugin/sdk/protocol/     apps/desktop/src/plugins/
├── openai_chat.ts      Chat 请求体 + SSE      adapters/chat.rs       713
├── openai_responses.ts Responses 请求体 + SSE adapters/responses.rs  855
├── anthropic.ts        Messages 请求体 + SSE  adapters/anthropic.rs  918
└── anthropic_test.ts   Deno 对照              adapters/* 内 #[cfg(test)]

cursor-byok/server/src/plugin/                  apps/desktop/src/plugins/
├── worker.rs  network.stream + JSONL Event     worker.rs             protocol.stream、readRequest、HostError.status、事件直通
├── registry.rs stream_model                    host.rs               invoke 时登记 Arc<ModelInvocation>
├── wire.rs    SDK JSON 投影                    wire.rs               按需 SDK 投影 + 事件解码；无 Rust LlmRequest 类型族
└── sdk/plugin.ts / worker.ts / tsconfig        sdk/                  context.protocol.stream；input.readRequest()

cursor-byok/server/plugins/build-in/magpie/
└── provider.ts  调 :protocol/* helper          plugins/build-in/magpie/provider.ts  调 context.protocol.stream
```

行数为 `wc -l` 实测，含测试和空行。适配器合计约 2892 行；SDK 核心/worker 约 787 行；Magpie 插件 TS 312 行。golden fixture 内嵌在适配器单测里（脱敏、无凭证），来源是迁移前三个 TS helper 的请求体字段与事件序列。标准路径不再经过 SDK JSON。

## 主路径

```text
目录：Magpie TS models.list → context.network.fetch → TS 解析 /v1/models → 宿主 live 缓存
推理：Cursor local loop → PluginHost.stream_model
     → 登记 Arc<ModelInvocation> → Bun Magpie provider.invoke（无正文）
       ├─ TS：选协议、选端点、组鉴权头、401/协议错误分类
       └─ hostCall "protocol.stream" {protocol, url, model, headers}
          → Rust 适配器：从登记项借用核心请求 → 请求体 → 网关 HTTP/SSE → ModelEvent
          → 事件直写本次 invocation 流；HTTP 状态/错误回给 TS
     自定义 TS 协议：显式 await input.readRequest() 才投影 SDK JSON
     → local loop 生成 Cursor 响应帧
观测：适配器的首个请求 claim CallRecorder → calls.db → 调用记录页
```

## 差异清单

| # | 主题 | 上游 TS helper | nsh 版 | 原因 |
|---|---|---|---|---|
| 1 | 协议实现位置 | Bun SDK `:protocol/*` | 宿主 `plugins/adapters/` | PRD：协议在 Rust，策略在 TS |
| 2 | 请求体来源 | helper 参数带完整 `LlmRequest` | invoke 时登记 `Arc<ModelInvocation>`；适配器直接读核心请求；`protocol.stream` 不含消息 | 会话投影只有一个所有者；长历史和附件不再为了选端点而序列化进 Bun |
| 3 | 事件通道 | worker JSONL `event` | 适配器直写 `WorkerStreamItem::Event(ModelEvent)` | 避免事件二次编解码 |
| 4 | 非 2xx | 三个 `HttpError` | 统一 `ProtocolHttpError`，`host_error.status` | 删除 helper 后 Magpie 只需一种分类 |
| 5 | 错误体上限 | 读完整响应体 | 64 KiB 截断 + 短 deadline | 避免超长响应进入调用记录与错误链，且不让半开的 body 拖住状态码 |
| 6 | extraBody / extraParams | helper 可合并额外字段 | `ModelSpec.stop_sequences` + 合并 Cursor `temperature` / `top_p`；禁止覆盖核心字段 | 对齐上游 Rust `merge_extra_params`，不移植任意 extraBody allowlist |
| 7 | Anthropic 系统图 | 上游测试文案 “only accepts images” | 现 helper 与 Rust 均为 “only accepts attachments” | 对齐本仓库迁移前 helper，不回退上游旧文案 |
| 8 | SDK specifier | `:protocol/openai-chat` 等 | 删除，无别名 | PRD：删除即完成 |
| 9 | 标准路径 JSON | invoke 时构造 SDK `LlmRequest` 并 `from_value` | 标准路径不构造 SDK JSON；Rust `LlmRequest` 类型族已删；自定义插件显式 `readRequest()` | `LOCAL_LOOP_REQUEST_PRD.md` 替代「adapter 输入必须与 SDK 同 schema」 |
| 10 | 停止序列 | 上游 helper 无该字段（Cursor `InferenceModelConfig.stop_sequences` 未投影） | `project.rs` 只把 `stop_sequences` 放进 `ModelSpec`，由适配器按协议写：Chat `stop`、Anthropic `stop_sequences`、Responses 无停止字段故丢弃 | 三个协议字段名不同；原样合并 `stop` 会让 Anthropic/Responses 在严格网关 400 |
| 11 | OpenAI 工具名 | helper 直接使用 `tool.name` | `ToolNames` 每次调用建无冲突双向映射：请求体用别名（`a.b` → `a_b`，撞名加 `_2`），回给 Cursor 的 `ToolCallStart` 还原原名；Anthropic 不改写 | 保留 Cursor 注册名，避免 `a.b` 与 `a_b` 撞成同一名字 |
| 12 | 非 2xx 错误体 | 读完整响应体，等待 EOF | 64 KiB 上限 + 10s deadline；先记录 HTTP 状态，再读错误体，超时/读失败只截断文本，状态码照常回给 Magpie | 网关可能发完 401 头却不结束 body；状态码不能因为错误体延迟而丢失 |

## 必须保持的不变量

1. 策略在 TS，协议在 Rust：协议选择、端点、鉴权头、`401 → invalid`、目录解析留在 Magpie TS。
2. 同一核心 `ModelInvocation` 与上游 SSE，请求体字段和事件序列与迁移前 helper 等价（适配器 golden 锁定）。标准路径不经过 SDK JSON。
3. 文本 / 思考 / 每个工具的 start → delta → end、回放在流结束前一次发出、`usage` 与 `done` 先后不变。Anthropic 必须发出 `ThinkingSignature` / `RedactedThinking`，并把缓存 token 计入 `context_input_tokens`。
4. Magpie 只请求本次绑定的网关 origin；密钥不出现在 UI、日志、测试快照。
5. 一次 `InvokeModel` 认领首个协议请求写入 `CallRecorder`；观测失败不阻断推理。
6. URL 白名单、拒绝凭证、不跟随重定向与 `network.fetch/stream` 相同。
7. 生产代码与 SDK 中不再出现 `cursor-byok-next:protocol/`。
8. 测试用临时目录和假网关，不读真实 `~/.cursor-byok-next`。
9. 工具名只在 provider 请求体内改写；`ToolCallStart`、`tool_calls` 与 `tool_use` 回放一律是 Cursor 的原名。
10. 停止序列按协议字段名落地，未知字段不写入请求体；非 2xx 先留住 HTTP 状态，错误体读取不阻塞状态回传。

## 测试映射

| 覆盖 | 位置 |
|---|---|
| Chat / Responses / Anthropic 请求体与 SSE golden | `plugins/adapters/{chat,responses,anthropic}.rs` |
| 停止序列按协议映射（Chat `stop`、Anthropic `stop_sequences`、Responses 丢弃） | `plugins/adapters::tests::maps_stop_sequences_per_protocol`、`loop_service::project` |
| OpenAI 工具名别名去冲突并还原原名（Chat + Responses），Anthropic 不改写 | `plugins/adapters::tests::restores_original_tool_names_after_openai_aliasing`、`keeps_anthropic_tool_names_unaliased` |
| 非 2xx 错误体读到 deadline/字节上限即返回，HTTP 状态仍可回传 | `plugins/adapters::tests::error_body_read_keeps_the_status_and_never_waits_for_eof` |
| 同一核心 fixture 的三协议 body、空可选字段、非法 tool 消息、extra params、SSE 分帧 | `plugins/adapters/mod.rs` |
| 按需 SDK 投影与非法 tool 消息 | `plugins/wire.rs` |
| `host_error.status` 序列化 | `plugins/protocol.rs` |
| 附件、取消关上游、完成/取消落库、目录 max tokens | `host.rs` 既有 Magpie 假网关测试 |
| Magpie invoke 帧不含正文且不随历史/附件增大 | `magpie_invoke_frame_does_not_include_or_grow_with_the_model_body` |
| 自定义插件 `readRequest` 复用、并发隔离、取消后不可读、invoke 句柄先于发送取得 | `plugins/worker.rs` |
| `401` → 资源 `invalid` | `magpie_invocation_marks_resource_invalid_on_http_401` |
| 目录仍走 TS `network.fetch` | `magpie_catalog_through_real_bun` |

```sh
cargo test -p nsh-app-cursor-byok-next --lib
cd apps/desktop && nsh check && nsh test
```
