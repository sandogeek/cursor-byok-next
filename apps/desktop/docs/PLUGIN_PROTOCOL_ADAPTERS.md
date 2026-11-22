# 移植记录：插件协议适配器下沉 Rust

上游实现：`cursor-byok/server/src/plugin/sdk/protocol/{openai_chat,openai_responses,anthropic}.ts`。
本移植：`apps/desktop/src/plugins/adapters/` + `sdk/{plugin,worker}.ts` + Magpie `provider.ts`。

范围只有一件事：**把三个 TS 协议 helper 改成宿主 Rust 适配器**。插件仍用 Bun 选协议、拼端点、组鉴权头并分类错误；请求体、HTTP/SSE 与事件归一化归 Rust。不引入 NSH 插件类型，不迁移目录拉取。

## 目录映射

```text
cursor-byok/server/src/plugin/sdk/protocol/     apps/desktop/src/plugins/
├── openai_chat.ts      Chat 请求体 + SSE      adapters/chat.rs       661
├── openai_responses.ts Responses 请求体 + SSE adapters/responses.rs  805
├── anthropic.ts        Messages 请求体 + SSE  adapters/anthropic.rs  867
└── anthropic_test.ts   Deno 对照              adapters/* 内 #[cfg(test)]

cursor-byok/server/src/plugin/                  apps/desktop/src/plugins/
├── worker.rs  network.stream + JSONL Event     worker.rs             protocol.stream 宿主调用、HostError.status、事件直通
├── registry.rs stream_model                    host.rs               invoke 时登记 LlmRequest
├── wire.rs    SDK JSON 投影                    wire.rs               LlmRequest 类型 + round-trip
└── sdk/plugin.ts / worker.ts / tsconfig        sdk/                  context.protocol.stream；删除 :protocol/*

cursor-byok/server/plugins/build-in/magpie/
└── provider.ts  调 :protocol/* helper          plugins/build-in/magpie/provider.ts  调 context.protocol.stream
```

行数为 `wc -l` 实测，含测试和空行。适配器合计约 2658 行；SDK 核心/worker 约 779 行；Magpie 插件 TS 312 行。golden fixture 内嵌在适配器单测里（脱敏、无凭证），来源是迁移前三个 TS helper 的请求体字段与事件序列。

## 主路径

```text
目录：Magpie TS models.list → context.network.fetch → TS 解析 /v1/models → 宿主 live 缓存
推理：Cursor local loop → PluginHost.stream_model
     → 登记 wire::llm_request → Bun Magpie provider.invoke
       ├─ TS：选协议、选端点、组鉴权头、401/协议错误分类
       └─ hostCall "protocol.stream" {protocol, url, model, headers}
          → Rust 适配器：登记的规范请求 → 请求体 → 网关 HTTP/SSE → ModelEvent
          → 事件直写本次 invocation 流；HTTP 状态/错误回给 TS
     → local loop 生成 Cursor 响应帧
观测：适配器的首个请求 claim CallRecorder → calls.db → 调用记录页
```

## 差异清单

| # | 主题 | 上游 TS helper | nsh 版 | 原因 |
|---|---|---|---|---|
| 1 | 协议实现位置 | Bun SDK `:protocol/*` | 宿主 `plugins/adapters/` | PRD：协议在 Rust，策略在 TS |
| 2 | 请求体来源 | helper 参数带完整 `LlmRequest` | invoke 时宿主登记，`protocol.stream` 不含消息 | 会话投影只有一个所有者；避免附件 base64 二次走 JSONL |
| 3 | 事件通道 | worker JSONL `event` | 适配器直写 `WorkerStreamItem::Event(ModelEvent)` | 避免事件二次编解码 |
| 4 | 非 2xx | 三个 `HttpError` | 统一 `ProtocolHttpError`，`host_error.status` | 删除 helper 后 Magpie 只需一种分类 |
| 5 | 错误体上限 | 读完整响应体 | 64 KiB 截断 | 避免超长响应进入调用记录与错误链 |
| 6 | extraBody | helper 可合并额外字段 | 不实现 | PRD：不移植 extraParams / body allowlist |
| 7 | Anthropic 系统图 | 上游测试文案 “only accepts images” | 现 helper 与 Rust 均为 “only accepts attachments” | 对齐本仓库迁移前 helper，不回退上游旧文案 |
| 8 | SDK specifier | `:protocol/openai-chat` 等 | 删除，无别名 | PRD：删除即完成 |

## 必须保持的不变量

1. 策略在 TS，协议在 Rust：协议选择、端点、鉴权头、`401 → invalid`、目录解析留在 Magpie TS。
2. 同一 `LlmRequest` 与上游 SSE，请求体字段和事件序列与迁移前 helper 等价（适配器 golden 锁定）。
3. 文本 / 思考 / 每个工具的 start → delta → end、回放在流结束前一次发出、`usage` 与 `done` 先后不变。
4. Magpie 只请求本次绑定的网关 origin；密钥不出现在 UI、日志、测试快照。
5. 一次 `InvokeModel` 认领首个协议请求写入 `CallRecorder`；观测失败不阻断推理。
6. URL 白名单、拒绝凭证、不跟随重定向与 `network.fetch/stream` 相同。
7. 生产代码与 SDK 中不再出现 `cursor-byok-next:protocol/`。
8. 测试用临时目录和假网关，不读真实 `~/.cursor-byok-next`。

## 测试映射

| 覆盖 | 位置 |
|---|---|
| Chat / Responses / Anthropic 请求体与 SSE golden | `plugins/adapters/{chat,responses,anthropic}.rs` |
| `LlmRequest` JSON round-trip | `plugins/wire.rs` |
| `host_error.status` 序列化 | `plugins/protocol.rs` |
| 附件、取消关上游、完成/取消落库 | `host.rs` 既有 Magpie 假网关测试 |
| `401` → 资源 `invalid` | `magpie_invocation_marks_resource_invalid_on_http_401` |
| 目录仍走 TS `network.fetch` | `magpie_catalog_through_real_bun` |

```sh
cargo test -p nsh-app-cursor-byok-next --lib
cd apps/desktop && nsh check && nsh test
```
