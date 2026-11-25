# PRD：local loop 标准协议路径不再构造 `LlmRequest`

状态：已实现。适用范围：Cursor-BYOK-Next 的本地 `InvokeModel`。前置工作见 [local loop](LOCAL_LOOP.md) 和 [Rust 协议适配器](PLUGIN_PROTOCOL_ADAPTERS.md)。文中「发给 Bun」的传输后来由 [插件运行时 VM](PLUGIN_RUNTIME_VM.md) 换成进程内会话队列，投影边界与所有权结论不变。

## 决策与收益

保留 `InferenceStreamRequest → ModelInvocation` 的 Cursor 语义转换。删除的是**标准协议调用中**的第二份消息表示：Chat、Responses、Messages 直接借用同一份 Rust 调用数据，构造各自的最终 HTTP body。长历史和附件不再为了让 Magpie 选端点而序列化进 Bun。不是把 Cursor protobuf 直接发给模型，也不是承诺整个请求零拷贝。

[插件运行时 PRD](PLUGIN_RUNTIME_PRD.md) 允许自定义 TS 协议自行构造请求并 `output.emit`。保留这项能力，但改为显式按需读取正文；不保留旧 `input.request` 属性、兼容别名或自动回退。只改 Next，不改上游 `cursor-byok`、官方模型转发、engine、数据库 schema 或 UI。

## 当前成本在哪里

1. `loop_service/project.rs` 已完成 Cursor → 核心模型的投影。`plugins/host.rs::stream_model` 随后调用 `wire::llm_request`，再次遍历历史，构造 JSON，并提前将每张图片、每份文件编码为 base64。
2. 宿主执行 `serde_json::from_value(request.clone())`，得到登记给 Rust adapter 的 `LlmRequest`；同时又把完整 `request` 放进发给 Bun 的 `provider.invoke` 参数。`sdk/worker.ts` 把它作为 `input.request` 交给插件。
3. Magpie `provider.ts::invoke` 实际只读 `input.model` 和 `input.resource`。回调 `protocol.stream` 的参数里已经没有正文；**多余传输发生在第一次 Rust → Bun，而不是回调时**。

## 目标路径与所有权

```text
Cursor InvokeModel → project.rs → ModelInvocation（Rust 拥有）
  → host.rs：解析插件 / 模型 / 资源，按本次 worker request ID 登记调用
  → Bun provider.invoke：仅发送 providerId、model、resource
      ├─ Magpie：protocol.stream({protocol, url, model, headers})
      │    → Rust 按当前调用取核心请求 → 所选 adapter → 最终 HTTP body
      │    → 假如已启用记录，沿用 CallRecorder 的现行记录策略
      │    → 上游 SSE → ModelEvent → Cursor 响应帧
      └─ 自定义 TS 协议：显式 await input.readRequest()
           → 宿主才投影 SDK JSON → 插件构造协议请求并 emit
```

- `plugins/worker.rs`：在现有 invocation 登记项中持有 `ModelInvocation`；跨异步任务需要共享时使用 `Arc`，只克隆句柄，不克隆历史。`Start` 使用原 call ID。并发调用不能互读，会话重连不创建第二次模型调用。完成、失败、取消及 worker 退出均沿用 invocation 清理，不延长到整个 Run 生命周期。
- `plugins/adapters/{mod,chat,responses,anthropic}.rs`：入口从登记项取借用；`build_body` 直接读核心 prompt、history、model 和会话 ID。协议无关的消息判定或校验可用借用式小函数复用，不新建包含完整消息列表的 DTO。附件保持原始字节直到所选协议需要编码；纯文本文件按该协议现行规则处理。
- `plugins/{host,wire}.rs`：删除标准路径上的 JSON 构造、深拷贝和反序列化。删除仅用于这一往返的 Rust `LlmRequest` 类型族；保留自定义插件按需导出所需的投影和既有事件解码。不要误删 `calls/` 中记录最终请求的同名方法。
- `plugins/sdk/{provider,worker}.ts`：`ProviderInvokeInput.request` 替换为 `readRequest(): Promise<LlmRequest>`。由 worker 注入函数，仅首次调用才发送 host call；同一次 invoke 内复用 Promise，不做跨调用缓存。宿主只认现有 host-call 信封绑定的在途 provider invocation，不接受插件指定目标 ID；目录调用、已结束或已取消的调用不可读取。Magpie 不调用此方法。

## 等价性要求

- 消息顺序和内容不变：纯文本 assistant 的合并、thinking、按 provider 匹配的 replay state、工具调用顺序与 call ID、工具结果的错误位和富内容优先级均沿用现行行为。非法 tool 消息不能因删除 `wire_message` 校验而变成成功调用；不得向上游发送请求。
- 逐协议保持 instructions、工具 schema、reasoning、latency、输出上限及空值省略规则。缓存键仍来自 `invocation.conversation_id`，不是本轮 call ID。附件保留 MIME、文件名、内容及现有不支持类型的错误；不以空文本替代。
- 保留 Magpie 端点与鉴权策略、origin 限制、重定向限制、`401 → invalid`、流事件次序、取消关闭上游、首个请求认领 recorder 及脱敏规则。详细记录取实际发出的 provider body，不以 SDK 投影代替。

## 实施顺序与验收

1. **先固定基线。** 用同一组核心调用 fixture 保存三个现行适配器的最终 body 和失败用例。覆盖多轮工具、回放、空可选字段、图片、PDF 和纯文本文件。fixture 只用合成内容；保留既有 SSE golden。
2. **切换宿主和插件边界。** 一并改登记项、三个 adapter 和 SDK；移除标准路径旧实现。自定义 TS 插件测试验证显式读取返回原 SDK 形状，第二次读取不重复传输；并发两次调用及取消后读取验证隔离和清理。
3. **验证优化确实生效。** 真实 Bun + 本地假网关分别运行短历史、长历史及含 1 MiB 合成附件的请求。固定模型、资源及等长 ID，比较 `provider.invoke` 帧字节数：目标是正文增大时帧大小不变，且无正文、工具定义或附件标记；标准路径 SDK 投影次数和 `readRequest` 调用次数均为 0。不要记录真实帧或鉴权头。对比改动前后本地构造耗时作为观测，不以公网首 token 延迟衡量此优化。
4. **行为门槛。** 三协议最终 body 与基线 JSON 等价（保留数组顺序），错误与 SSE、取消、401、记录测试通过；运行 `cargo test -p nsh-app-cursor-byok-next --lib`，在 `apps/desktop` 运行 `nsh check` 和 `nsh test`。测试不读取真实 Cursor 配置或网关密钥。
5. **落地后更新记录。** 更新 `LOCAL_LOOP.md` 与 `PLUGIN_PROTOCOL_ADAPTERS.md` 的目录映射、相对上游差异及原因、调用路径和测试对应关系。本 PRD 替代旧运行时 PRD 中「标准 adapter 输入必须与 SDK 同 schema」的要求，其余运行时约束继续有效。
