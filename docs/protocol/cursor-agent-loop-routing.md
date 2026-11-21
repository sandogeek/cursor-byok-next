# local loop 与 backend loop：谁跑循环、何时落到哪条

本文只做**链路选择**层面的分析：一次 agent turn 由谁持有循环、local loop 需要哪些方法、两条 loop 如何互斥与降级。
逐字段含义见 [`agent-v1.md`](agent-v1.md) / [`aiserver-v1.md`](aiserver-v1.md)；推理链路的形态与降级见 [`cursor-inference-service.md`](cursor-inference-service.md)。

证据版本：Cursor `3.30.14`，commit `5f54c8173e5552412beb40702adac16275677fa0`，build `2026-11-13T06:31:38Z`
（`/Applications/Cursor.app/Contents/Resources/app/extensions/cursor-agent-host/dist/agent-host-daemon/dist/bin/build-info.json`）。
下文 `daemon.cjs` 指 `extensions/cursor-agent-host/dist/agent-host-daemon/dist/bin/daemon.cjs`，`workbench` 指 `out/vs/workbench/workbench.desktop.main.js`。

## 结论

1. Cursor 3.30 的 agent turn 有**两条互斥的 loop**，由 agent-host 的 turn router 选定：

   | loop | 谁跑循环 | 模型调用 | 本仓库 |
   |---|---|---|---|
   | local loop（`managed-local`） | 本机 agent-host 进程内的 `LocalLoopAgentClient` | `InferenceService/RunInference`（→ `RunInferenceSSE` + `BidiAppend`） | **目标** |
   | backend loop（`connect`，埋点名 `nal`） | Cursor 云端 | `AgentService/Run`（→ `RunSSE` / `RunPoll`） | 不接管 |

2. **`AgentService/RunSSE` 不是 local loop 的路径**，它只承载 backend loop 的 turn。上游 `cursor-byok` 之所以有它的 handler，是因为上游连 `RunEngine`（服务端自跑循环）一起移植了；本仓库只做 local loop，因此不需要。
3. local loop 除了推理读写两条，还会用 `AgentService` 的**两条 unary**：`UploadConversationBlobs`（blob 上传）与 `WarmUp`（预热）；工具调用走后端 `AiService` / `RepositoryService`，同样只涉及 unary。
4. 降级是**单向**的：local loop 失败只会回落到 backend loop；backend loop 不会被改派给 local loop。
5. 因为 local loop 会拒绝一部分 turn，这些 turn 必须继续可达官方——所以代理白名单里**不能**放 `AgentService/Run*`。

---

## 1. 一次 turn 的 loop 选择

```text
agent-host daemon 处理一个 turn
└── createLocalLoopTurnRouter                       daemon.cjs:3176
    ├── privateInference 分支（私有推理，另议）
    ├── managedLocalAvailable == false
    │   └── connectDecision("managed-local-client-absent" / "managed-local-unavailable")
    └── gate 读取
        ├── runOptions.localLoopGateEnabled（客户端下发）优先
        └── 否则 capabilities.checkFeatureGate("agent_host_local_loop")   daemon.cjs:3182
    └── 资格判断 getIneligibilityReason                daemon.cjs:3181
        ├── 通过 → runtime:"managed-local"
        └── 不通过 → connectDecision(<reason>)
```

`turnRouteLoop`（`daemon.cjs:14775`）是整个仓库里唯一把 `runtime` 翻译成 loop 名字的地方：

```text
runtime:"managed-local" → "local_loop"
runtime:"connect"       → "nal"
runtime:"fail"          → 无 loop（拒绝 turn）
```

两个 loop 是 `switch` 的不同分支，不存在「先本地再后端」的同 turn 串联。

埋点口径与之独立但一致：`classifyBidiStreamLoop`（`daemon.cjs:9217`）按服务名把 `InferenceService/RunInference` 标成 `"local_loop"`、`AgentService/Run` 标成 `"nal"`。

## 2. local loop 的资格条件

`getIneligibilityReason`（`daemon.cjs:3181`）拒绝的 turn 会直接走 backend loop。已确认的拒绝原因：

| 原因 | 条件 |
|---|---|
| `action-not-supported` | action 不是 `userMessageAction` / `executePlanAction`，也不是 conversation continuation（`backgroundTaskCompletionAction` / `resumeAction` / `summarizeAction` / `goalContinuationAction` / `asyncAskQuestionCompletionAction`） |
| `mode-not-supported` | 模式不在 `AGENT / PLAN / ASK / DEBUG / MULTITASK`（`LOCAL_LOOP_SUPPORTED_MODES`，`daemon.cjs:3176`） |
| `harness-not-supported` | turn 带 `runOptions.harness` 覆盖 |
| `exclude-workspace-context-not-supported` | `runOptions.excludeWorkspaceContext === true` |
| `direct-meta-subagent-not-supported` | `runOptions.directMetaParentChildSubagent === true` |
| `custom-system-prompt-not-supported` | subagent turn 自带 custom system prompt |
| `model-not-supported` | turn 解析不出 model id |
| `private-inference-*` | 私有推理配置存在但不可用 |

另有一条反向约束：`localLoopOnly`（`isGrokBuildCli(network)`）的构建如果被判到 `connect`，会用 `LocalLoopOnlyBackendTurnError` **直接拒绝 turn**，而不是跑 backend loop；同理，带 `toolRestriction` 且后端无法强制该限制的 turn 会被拒绝（`ToolRestrictedBackendTurnError`）。这两条说明「两条 loop 不可互换」，不是可选项。

## 3. local loop 用什么

`createManagedLocalAgentClient`（`daemon.cjs:26963`）与宿主的装配处决定 local loop 只拿到这些 client：

```text
LocalLoopAgentClient
├── inferenceClient       = createClient(InferenceService, inference.transport)
│   └── RunInference →（http1 下）RunInferenceSSE 读 + BidiAppend 写
├── agentToolsClient      = managedAgentToolsClient
│   ├── AiService.runWebSearch / runWebFetch / runGenerateImage / runGenerateVideo / runXSearch
│   └── RepositoryService（codebaseSearch）
├── conversationBlobClient = createClient(AgentService, inference.transport)   daemon.cjs:14729
│   └── 仅用于 UploadConversationBlobs（createManagedLocalConversationBlobUploader，daemon.cjs:26960）
└── warmUp                 = AgentServiceWarmUp → AgentService/WarmUp
```

对照 backend loop 的装配（`createAgentHostBackendClients`，`daemon.cjs:14738`）：它是 `new AgentClientHostAdapter(...)`（`daemon.cjs:25979`）持有通用 `AgentService` client，`AgentService/Run` 走 `agentBidiTransport`；`RunSSE` / `RunPoll` 只是 http1 不可用时的传输层重映射（映射表见 [`README.md`](README.md#两条-bidi-链路分属两条-loop)）。

一句话：**同一个进程里两个 loop 用的是不同的 client 集合**。local loop 里不存在 `AgentService/Run*` 的调用点。

## 4. 降级方向

```text
local loop turn
├── 资格不足 → 选 backend loop（进入本次 turn 之前就决定）
└── 运行期失败 → "Local loop fell back to the backend loop for this turn"
                  （daemon.cjs:14778，metric agent_host.local_loop.fallback）

backend loop turn
├── 若宿主 localLoopOnly 或该 turn 依赖本地工具限制 → 拒绝，不跑
└── 不会再改派给 local loop
```

`LocalLoopDeniedMidTurnError`（gate 在 turn 运行中被关掉）给出的是「恢复后继续在 backend loop 上跑」，也不是切回 local loop。

## 5. 对本仓库的约束

```text
apps/desktop/src/harness/proxy.rs  is_local_path
├── 已在白名单（local loop 的读写两条）
│   ├── InferenceService/RunInferenceSSE
│   └── BidiService/BidiAppend
├── local loop 会调用、但本地暂无 handler 的 unary（接后端时再决定是否加入）
│   ├── AgentService/UploadConversationBlobs（blob 上传）
│   └── AgentService/WarmUp（预热，可选）
└── 必须留在白名单外（backend loop）
    ├── AgentService/Run
    ├── AgentService/RunSSE
    └── AgentService/RunPoll
```

理由：

- `Routing.routes` 命中即改写，没有「本地无 handler 就回落上游」这层；`backend` 就绪后拿着没有 handler 的路径是 502，不是直通。因此第二组（尚未加入白名单的 unary）在实现 handler 之前不要加进去。
- §2 的拒绝条件保证**同一次会话的不同 turn 会在两条 loop 之间切换**。把 `Run*` 抢到本地，会让「本地 loop 不支持的 turn」从可用变成失败。
- 要做 backend loop 才需要 `Run*`，而那需要本地 `RunEngine`、checkpoint、工具执行与输出终态——即 `AGENTS.md` 明确排除的 engine 部分。

## 6. 未确认

| 项 | 现状 | 需要什么证据 |
|---|---|---|
| 本机是否启用 `agent_host_local_loop` | workbench 侧两个 gate 默认 `false`（`workbench:1647`），`readLocalLoopGate` 在 `workbench:2039`；上游 `analytics.rs` 用本地 Statsig bootstrap 把它置为 true | 打开接管后看 agent-host 日志的 `agent_host.local_loop.route` 埋点 |
| local loop 是否一定不复用 `AgentService/Run*` | 现有 bundle 的 local client 装配里没有 `AgentService` 的 `Run*` 调用点 | 抓一次 local loop turn 的全量 RPC 列表 |
| `nal_websocket_client` / `run_inference_websocket_client` 是否启用 | 均未确认 | 见 [`cursor-inference-service.md`](cursor-inference-service.md) §2.5 |

## 7. 验证方法

```bash
APP=/Applications/Cursor.app/Contents/Resources/app
D=$APP/extensions/cursor-agent-host/dist/agent-host-daemon/dist/bin/daemon.cjs
W=$APP/out/vs/workbench/workbench.desktop.main.js

# 1) 两个 loop 的名字与互斥分支
rg -n 'function turnRouteLoop' $D
rg -n 'runtime:"managed-local"|runtime:"connect"' $D

# 2) local loop 的 gate 与资格条件
rg -n 'AGENT_HOST_LOCAL_LOOP_GATE=' $D
rg -n 'function getIneligibilityReason' $D
rg -n 'LOCAL_LOOP_SUPPORTED_MODES' $D
rg -n 'readLocalLoopGate' $W

# 3) local loop 的 client 集合（无 AgentService/Run*）
rg -n 'function createManagedLocalAgentClient' $D
rg -n 'conversationBlobClient' $D

# 4) 降级方向
rg -n 'Local loop fell back to the backend loop' $D
rg -n 'LocalLoopOnlyBackendTurnError' $D

# 5) 本仓库白名单
rg -n 'AgentService/Run' apps/desktop/src/harness/proxy.rs
cargo test -p nsh-app-cursor-byok-next harness::proxy
```

证据版本：Cursor `3.30.14`。升级后压缩变量名与行号会变，方法名、gate 名与模块名稳定，第 7 节的检索方式仍然有效。