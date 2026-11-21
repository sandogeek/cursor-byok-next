# backend loop：非官方模型 fail fast 与调用明细

状态：**设计草案，尚未实现**。

本文只定义一种保护行为：Cursor 已选择 backend loop，且本次 turn 使用非官方模型时，本应用**立即拒绝该 turn，并记录到调用明细**。不实现 backend loop，不移植 backend engine，不在本地执行 agent 循环，也不把该 turn 改派给 local loop。

官方模型的 backend turn 仍原样直通 Cursor 云端。现有“不做 engine，只做 agent local loop”的仓库边界不变。

链路选择与降级证据见 [`cursor-agent-loop-routing.md`](cursor-agent-loop-routing.md)；协议形状见 [`agent-v1.md`](agent-v1.md) 与 [`aiserver-v1.md`](aiserver-v1.md)。

## 1. 目标行为

```text
Cursor turn router 已选择 backend loop
└── BidiAppend 携带 AgentClientMessage.run_request
    ├── 非官方模型（命中本应用已配置模型）
    │   └── fail fast
    │       ├── 拒绝该 backend turn
    │       ├── 写入一条 rejected 调用明细
    │       └── 不向 Cursor 官方或任何 provider 发起模型调用
    └── 官方模型（未命中本应用配置）
        └── 原始请求与响应直通 Cursor 官方
            ├── 不创建本地 agent loop
            └── 不创建本地 provider 调用明细
```

这里的“拦截”只表示**识别并拒绝**，不表示接管执行。拒绝必须发生在首个可识别模型的客户端帧上，不能等官方已经收到 `run_request` 后再报错。

必须保持：

1. **不实现 backend loop。** 不增加 `RunEngine`、checkpoint、工具编排或本地 `AgentServerMessage` 生成器。
2. **官方模型直通。** 未命中本应用配置的模型仍走 Cursor 官方，行为与现状一致。
3. **非官方模型不落到官方。** 已配置模型 id 只在本地有意义；一旦识别，原样转发也属于路由错误。
4. **失败关闭。** 模型无法识别、protobuf 无效或路由状态异常时，不得猜测为官方并转发。
5. **同一 `request_id` 路由固定。** 首个 `run_request` 决定该流是 `RejectedNonOfficial` 还是 `Official`，后续帧沿用，不逐帧重新判断。
6. **不自动切换 loop。** 拒绝后不伪造 local-loop 响应，不尝试让 Cursor 在同一个 turn 内改走 local loop。用户若要继续，应按客户端能力重新选择可用路径并发起新 turn。
7. **不以模型名猜“官方”。** “非官方模型”指能通过稳定模型 id 解析到本应用已配置模型，不是按厂商名或模型名前缀判断。

## 2. 检测点与失败时序

backend loop 在 http1 下与 local loop 共用 `aiserver.v1.BidiService/BidiAppend` 写入口；读入口 `agent.v1.AgentService/RunSSE` 只有 `BidiRequestId`，没有模型 id。因此必须在首个包含 `run_request` 的 `BidiAppend` 上判定。

```text
BidiAppend(request_id, append_seqno, AgentClientMessage)
└── 解码 run_request
    ├── requested_model.model_id → 查本应用模型配置
    ├── 否则 model_details.model_id → 查本应用模型配置
    └── 无有效模型 id → 协议错误并拒绝

模型解析结果 + request_id + conversation_id
└── 原子登记路由
    ├── RejectedNonOfficial(model)
    │   ├── 先持久化 rejected 调用明细
    │   ├── 当前 BidiAppend 返回明确错误
    │   └── 关联 RunSSE 以明确错误结束，不等待官方
    └── Official
        └── 原样转发当前 BidiAppend；RunSSE 同样原样转发
```

模型字段候选顺序参照上游 `server/src/api/cursor/bidi.rs:23-41`。两个字段能否同时出现及语义仍需真实抓包确认；在确认前，两者非空但不一致时按协议错误拒绝，不任选其一直通。

`RunSSE` 与首个 append 可能并发到达：

- `RunSSE` 先到达：按 `request_id` 短暂等待首个 append 的路由结果；判为拒绝后立即以错误结束。等待必须有超时，超时同样失败关闭，不能默认转发官方。
- `BidiAppend` 先到达：登记路由、写明细并返回错误；后续 `RunSSE` 读取同一路由并结束。
- 后续 append：已拒绝的流统一返回同一类错误，不转发、不重复创建 turn 明细。
- 官方流：首个模型请求及后续 append 均保持原样转发，不能因模型配置中途变化切换路由。

错误应通过当前 RPC 的 Connect 错误语义返回；具体 error code、HTTP 状态与 `RunSSE` 的终止帧形状需按 Cursor 客户端对失败的实际处理确定。不能返回一个伪造成功的空流，否则会把“模型未执行”表现成正常结束。

## 3. 协议范围

最小实现只保护当前接管配置强制使用的 `http1_sse` 形态：

| 方法 | 非官方模型 | 官方模型 |
|---|---|---|
| `aiserver.v1.BidiService/BidiAppend` | 解码、拒绝并记录 | 原样转发 |
| `agent.v1.AgentService/RunSSE` | 复用拒绝路由，以错误结束 | 原样转发 |
| `agent.v1.AgentService/Run` | 本阶段不支持；必须拒绝或确保不可达 | 可保持现状直通 |
| `agent.v1.AgentService/RunPoll` | 本阶段不支持；必须拒绝或确保不可达 | 可保持现状直通 |
| WebSocket 上的 `AgentService/Run` | 本阶段不支持；必须确认不可达或补保护 | 不经本功能处理 |

`RunSSE` 要进入本地服务做路由判断，但不增加本地 backend handler 的业务实现：它只在官方分支透传，在拒绝分支返回错误。`Run` / `RunPoll` / WebSocket 不能因为“没有本地 handler”就自动落到官方；非官方模型在这些路径上同样必须失败关闭。

现有 local loop 的 `InferenceService/RunInferenceSSE + BidiAppend` 行为不变。本功能只给共用 `BidiAppend` 入口增加 backend 消息识别和拒绝分支。

## 4. 调用明细语义

**本功能记录的是一次被拒绝的 backend turn，不是一次实际 provider 调用。**

一个 `request_id` 只创建一条 turn 级明细：

| 分组 | 内容 |
|---|---|
| 身份 | `call_id`、`run_id = request_id`、`conversation_id`（能解析时） |
| 路由 | `loop = backend`、`route = rejected_non_official` |
| 模型 | 请求的模型 id；能解析时同时记录稳定模型 id、显示名与 provider 类型 |
| 状态 | 固定终态 `rejected`；不经历 `running`，不显示 `completed` |
| 错误 | `backend_loop_non_official_model`，附不含敏感信息的说明 |
| 时间 | 检测时间、拒绝时间、耗时；首字节、首段正文与 provider 结束时间为空 |
| 网络 | 本地 RPC 方法与 append 序号；provider URL、HTTP 状态和 token 用量为空 |
| 正文 | 默认不保存完整会话内容；详细日志开启也只能保存脱敏后的协议元数据 |

语义约束：

- `rejected` 与 provider 的 `error` / `cancelled` 分开，便于筛选“因不支持 backend loop 而未执行”的记录。
- `provider_call_index`、token、费用、provider HTTP 状态均为空，不填 `0` 冒充真实测量。
- 不向 provider 发请求，因此没有重试 attempt、响应 chunk 或首 token 延迟。
- 同一 turn 的后续 append、客户端重试同一逻辑流或 `RunSSE` 重连不得重复插入明细；以 `request_id` 幂等。
- 客户端另起一个 `request_id` 重试，则是一次新的拒绝事件，可以另记一条。
- 官方模型直通不创建本地 provider 调用明细。如未来需要展示官方路由，应另设路由观测，不伪造模型用量。

### 4.1 写入与拒绝的顺序

```text
识别非官方模型
└── 持久化 rejected 明细（幂等键 request_id）
    ├── 成功 → 返回明确错误
    └── 失败 → 仍拒绝并记录本地错误日志；不得因记账失败转而直通官方
```

即使明细存储失败，防泄漏优先级仍高于可观测性。UI 可显示存储故障，但不能把该 turn 标记成已执行或已转发。

### 4.2 无法分类的输入

缺少模型 id、字段冲突或消息解码失败时，没有可靠证据判定为官方模型，同样失败关闭。可记录为 `route = rejected_invalid_request` 的 turn 级明细；无法取得 `request_id` 时无法建立幂等关联，只记录脱敏错误日志，不编造会话或模型信息。

## 5. 状态所有权

```text
Rust
├── 模型配置与稳定 id 的唯一解析结果
├── request_id → RejectedNonOfficial / Official 的固定路由
├── rejected 明细的幂等写入与查询
└── 官方请求 / 响应的原始透传

TSX
├── 镜像调用列表与拒绝原因
└── 只通过 native 方法查询、筛选或清理明细
```

明细由 Rust 持久化，不能只存在插件 worker 或 UI 内存中。应用重启后仍可查询；路由判定所需的临时状态可在流结束后回收，不影响历史明细。

## 6. 验收场景

| 场景 | 必须结果 |
|---|---|
| backend loop + 已配置非官方模型 | 首个模型 append 即拒绝；只有一条 `rejected_non_official` 明细；官方与 provider 均未收到模型请求 |
| backend loop + 官方模型 | append 与 RunSSE 原样直通；不创建本地 provider 明细 |
| 同会话交替使用官方 / 非官方模型 | 每个新 `request_id` 独立判定，不沿用上一 turn |
| RunSSE 先于首个 append | 有界等待；拒绝或超时均失败关闭，不提前转发 |
| 同一流收到后续 append / RunSSE 重连 | 保持拒绝结果，不重复写明细，不转官方 |
| 客户端用新 request_id 重试 | 新事件单独记录；旧记录不变 |
| 模型配置在流中途变化 | 当前流路由不变；非官方流仍拒绝，官方流仍直通 |
| 首帧缺模型、字段冲突或 protobuf 无效 | 明确拒绝；不默认直通官方 |
| `Run` / `RunPoll` / WebSocket 非官方模型 | 显式拒绝或确保路径不可达，绝不转发官方 |
| 明细存储失败 | 仍拒绝并记录本地错误；不显示为成功执行 |
| local loop + 非官方模型 | 维持现有 local-loop 目标，不被 backend 拒绝规则误伤 |

## 7. 实施前置

1. 用真实 backend-loop 抓包确认 `run_request` 的模型字段、会话字段和首帧序号。
2. 确认 `nal_websocket_client`、`RunPoll` 在当前配置下不可达；否则先补非官方模型防泄漏保护。
3. 明确客户端可接受的 Connect 错误形状，避免把拒绝显示为正常空响应或无界等待。
4. 确定 turn 级调用明细的持久化结构，确保 `rejected` 不与真实 provider 调用混淆。
5. 实现时保留 `AGENTS.md` 与 `apps/desktop/docs/CURSOR_TAKEOVER.md` 的“不实现 backend engine”边界，只修订“非官方模型也一律直通”的旧假设。

## 关联

- backend 与 local loop 的选择：[`cursor-agent-loop-routing.md`](cursor-agent-loop-routing.md)
- `AgentClientMessage` / `AgentServerMessage`：[`agent-v1.md`](agent-v1.md)
- `BidiAppend` 与传输形态：[`aiserver-v1.md`](aiserver-v1.md)、[`cursor-inference-service.md`](cursor-inference-service.md)
- 上游模型字段解析：`cursor-byok/server/src/api/cursor/bidi.rs:23-41`
- 上游按模型分流（仅作路由证据，不移植其 engine）：`cursor-byok/server/src/api/cursor/handlers.rs:345-411`
