# PRD：插件运行时从 Bun 子进程换成进程内 QuickJS

状态：待实现
产品：Cursor-BYOK-Next 桌面端
关联：[插件目录](PLUGIN_CATALOG.md)、[协议适配器](PLUGIN_PROTOCOL_ADAPTERS.md)、[插件协议适配器 PRD](PLUGIN_RUNTIME_PRD.md)

## 1. 目标

把插件宿主的外部依赖从「用户 PATH 上的 `bun`」换成「应用内已链接的 QuickJS」，这样：

- **用户不需要装 Bun**，Magpie 开箱可用；删除 `plugin.error.bun_missing` 这一整类失败态。
- **插件的能力面由宿主决定**，而不是由运行时决定：Bun 里插件能 `Bun.spawn`、`node:fs`、裸开 socket；进程内 QuickJS 里只有宿主装进去的 native，`manifest.permissions` 第一次真正成为边界。
- **少一层进程、少一套管道协议解析、少一个临时工作目录和它的清理线程**。

TypeScript 编译在**构建期**完成（方案 A）：`build.rs` 用 `oxc` 把 `plugins/**/*.ts` 与 `src/plugins/sdk/*.ts` 降级为 ES2022 JavaScript，产物用 `include_str!` 编进二进制。运行时不再解析 TS。

**能力调用不走会话帧（方案 D）。** Bun 只有一根 stdin/stdout，所以今天 `network.fetch` / `protocol.stream` 都被压成 `host_call` 帧往返。进进程后这个前提消失：能力调用改成 VM 线程上直接可调的 native，tokio 任务完成后再把 Promise 结算；会话帧只承载 `request` / `cancel` / `result` / `event` 四类。

对外契约不变：`PluginContext`（`network.fetch` / `network.stream` / `protocol.stream` / `readRequest`）、`ProviderResult`、`host_calls.rs` 的白名单与上限、`adapters/` 的三条协议、`descriptor.rs` 的 `PluginModuleDefinition` 反序列化全部保持原样。**换的是传输与运行时，不是插件作者看见的契约。**

## 2. 用户行为

- **所有用户**：插件页不再要求 Bun；内置 Magpie 在首次打开插件页时即可加载，连接 / 删除 / 刷新网关、模型广告与推理行为不变。
- **曾装过 Bun 的用户**：应用不再启动 `bun` 进程，也不再在临时目录留 `cursor-byok-next-plugin-*` 目录；升级后旧残留由系统临时目录回收，应用不主动删除。
- **插件作者**：`PluginContext` / `ProviderResult` 等契约不变，但运行环境变了——见 §5.7 的「可用的宿主对象」清单。宿主另外提供 `cursor-byok-next:host` 模块给 `sdk/worker.ts` 内部转发能力调用；插件作者不需要、也不应该直接 import 它。Magpie 作者源仍是 `.ts`，编辑器与 `tsc` 仍能查类型。

## 3. 范围外

- 不引入第三方插件扫描、安装、更新、市场；仍然只跑应用内嵌的 Magpie。
- 不把插件 VM 变成通用 JS 沙箱服务；不提供模块表之外的代码加载入口。
- 不改 `adapters/` 的协议行为、`wire.rs` 的投影形状、`host_calls.rs` 的网络策略。
- 不改 `nsh` 侧任何东西：插件 VM 是 `plugins/` 自己的 `rquickjs` 实例，**不复用** `nsh_js::Host` 的 VM、线程或 `harden` 设置（理由见 §5.1）。
- 不做 `import` / `actions` 能力（仍按 [PLUGIN_CATALOG.md](PLUGIN_CATALOG.md) 差异 #10 删除）。
- 不改 Cursor 接管、代理白名单、`AgentService/Run*`、调用记录表结构。

## 4. 目标结构

```text
apps/desktop/
├── build.rs                          # 现有 proto 生成 + 新增：oxc 把插件 TS 编译成 JS，
│                                     #   写入 OUT_DIR/plugins/<module id>.js（≈+120 行）
├── src/plugins/
│   ├── mod.rs                        # 只剩 pub use 与 text()；删 find_bun / which / detach_console（75 → ≈40）
│   ├── manifest.rs  data.rs  state.rs  origin.rs  protocol.rs  descriptor.rs  catalog_cache.rs  wire.rs
│   │                                 # 全部不动（帧枚举只把 &'a str 换成 owned String）
│   ├── host.rs                       # 删 executable 字段与 runtime()/executable() 两个探测函数；
│   │                                 # 其余编排、快照、stream_model 不动（1705 → ≈1620）
│   ├── host_calls.rs                 # 不动（570）
│   ├── adapters/                     # 不动（3861）
│   ├── catalog.rs                    # 不再有临时工作目录；PluginEntry 携带模块表（432 → ≈330）
│   ├── builtin.rs                    # 内嵌编译产物清单代替写盘（69 → ≈45）
│   ├── worker.rs                     # 会话帧与线程；spawn/停止/超时改造（1654 → ≈1150）
│   ├── vm/                           # 新增 ≈1180
│   │   ├── mod.rs                    #   线程生命周期、事件循环、fatal 与重建（≈240）
│   │   ├── runtime.rs                #   Runtime/Context、allocator 堆上限、栈上限、CPU 预算中断（≈200）
│   │   ├── loader.rs                 #   模块表 resolver/loader，动态 import（≈130）
│   │   ├── globals.rs                #   URL、AbortController/Signal、console（≈230）
│   │   ├── timers.rs                 #   setTimeout/setInterval（≈90）
│   │   ├── natives.rs                #   会话帧入口 __recv / __send / __entry（≈90）
│   │   ├── host_module.rs            #   cursor-byok-next:host 的六个能力 native（≈200）
│   │   └── collect.rs                #   在 VM 内收集 descriptor（≈70）
│   ├── definition.rs                 # 删除（313 行：Bun 启动参数、临时工作目录、stdout 解析）
│   └── sdk/                          # 只保留类型定义 + 两个入口，全部经 build.rs 编译
│       ├── plugin.ts  provider.ts  model.ts  resource.ts   # 类型 only，运行期导出为空
│       ├── host.ts                   # 新增：六个能力的类型 + 转发到 native（≈60）
│       ├── worker.ts                 # 改 4 处（见 §5.6），逻辑不变（236 行）
│       ├── collect.ts                # 保留，改为读 __entry()（7 行）
│       └── bunfig.toml               # 删除
└── plugins/build-in/magpie/          # 作者源仍是 .ts，不再写盘、不再被 Bun 执行
    ├── plugin.json                   # entry 由路径改为模块 id
    └── main.ts  provider.ts  catalog.ts  resources.ts
```

```text
目录：Magpie models.list
  PluginHost → PluginWorker.invoke("models.list")
    → 会话帧 {type:"request"} → VM 线程 → JS dispatch → ctx.network.fetch
      → native host.fetch（VM 线程，不排队）
        → tokio 任务 → HostContext::call（白名单 + reqwest，不变）
        → 完成队列 → VM loop 下一拍 resolve Promise
      → JS 拿到 {status, headers, body} → 结果帧 {type:"result"} → catalog_cache

推理：Cursor local loop → PluginHost.stream_model
  → JS provider.invoke → ctx.protocol.stream
    → native host.protocolStream（同上，只等 HTTP 状态）
  → 事件由 Rust 适配器直写 invocation 流，不过 VM
    （同 [PLUGIN_PROTOCOL_ADAPTERS.md](PLUGIN_PROTOCOL_ADAPTERS.md)）

观测：适配器首个请求 claim CallRecorder → calls.db（不变）
```

**帧与 native 的分工**

| 谁 | 走什么 | 为什么 |
|---|---|---|
| `request` / `cancel` / `result` / `event` | 会话帧（mpsc） | 生命周期必须相对 invocation 有序；`event` 上限定为 4096 |
| `network.fetch` / `stream.*` / `protocol.stream` | `cursor-byok-next:host` native → Promise | 真异步 I/O；结果不必和会话帧抢队列 |
| `readRequest` | 同一个 native 模块，**同步**返回 | Rust 侧本来就是同步的 `wire::llm_request` |
| `__entry` / descriptor | native | 一次性的启动数据 |

## 5. 关键设计

### 5.1 为什么是独立 VM，而不是复用 `nsh_js::Host`

| 问题 | 后果 |
|---|---|
| 与 UI 同一个 engine 线程 | 插件一次长循环或一次同步长请求会冻住 60 Hz 渲染 |
| VM 里装了 `nsh` 全套模块 | `fetch`、`fs/promises`、`storage`、`clipboard` 全部可达 → `manifest.permissions` 形同虚设 |
| `harden: true` 冻结原型、删 `eval` | 与插件作者在 Bun 下的行为不一致 |
| 单 runtime 单堆上限 | 一个插件吃满内存会 `stop()` 整个 UI 的 VM |

因此：**一个插件一条 `std::thread`、一个 `rquickjs::Runtime`、一个 100% 由我们定义的模块表与全局对象**。

依赖必须与 `nsh-js` 精确同版本，否则 cargo 会编两份 `quickjs-jit-sys`，`JS_*` 符号重复：

```toml
rquickjs = { package = "quickjs-jit", version = "=0.14.2", default-features = false, features = ["std", "loader"] }
```

### 5.2 传输替换（`worker.rs`）

会话帧的两个端点从 `ChildStdin` / `ChildStdout` 换成 `mpsc`，**方向与今天一一对应**：

| 今天 | 目标 | 说明 |
|---|---|---|
| `Writer = mpsc::Sender<Vec<u8>>` → `write_stdin(ChildStdin)` | `to_worker: mpsc::Sender<String>` → VM 线程 `recv_timeout` | 会话帧仍是 JSON 文本 |
| `read_stdout(ChildStdout)` → `LineReader` | `read_frames(to_host: Receiver<String>)` → 逐帧 | `deliver_result` / `deliver_event` 原样保留 |
| `child.wait()` / `kill_on_drop` | VM 线程 `join` + `stop: AtomicBool` | 见 §5.4 |
| `read_stdout` 中的 16 MiB 行上限 | `__send` 单帧上限 16 MiB（超限记日志并终止本代 VM） | 上限值不变 |

`read_frames` 依旧跑在 tokio 任务上，所以 tokio 侧的 `tokio::spawn` 不需要传 `Handle`；VM 线程完全不碰 tokio。

`protocol.rs` 的帧枚举把 `&'a str` 换成 `String`（帧要进队列），字段名、tag、`requestId`、可选 `status` 全部不变。**`HostCall` 变体保留在枚举里**，用来读旧格式的测试样本与日志；生产路径不再产生它（见 §5.6）。

被删掉的是 `spawn_host_call` 及其 invocation 槽：能力调用不再从帧进入，`MAX_HOST_CALLS_PER_INVOCATION = 8` 移到 native 侧计数（§5.6）。

### 5.3 VM 线程循环

```text
loop {
  if stop { break }
  let frame = to_worker.recv_timeout(next_deadline)      // 会话帧：request / cancel
  if let Ok(frame) = frame { deliver_frame(frame) }       // __recv → JS
  drain_host_done()                                       // tokio 侧的 Promise 结果
  drain_jobs()                                            // JS_ExecutePendingJob 到空
  run_timers(now)                                         // setTimeout/setInterval
  if fatal { break }
}
```

- `deliver_frame` = 调用 JS 侧 `__recv(json)`；`__recv` 是同步分发，内部按 `type` 走 `request` / `cancel`。
- `drain_host_done` 从能力调用的完成队列（`mpsc::Receiver<HostDone>`）取走所有已完成项，逐个调用 `resolve` / `reject`。tokio 任务只往这条队列丢结果，**不直接碰 VM**。
- `drain_jobs` 是 `Runtime::execute_pending_job` 循环；一个 `request` 引发的 async 链在这里推进到下一次能力调用或终止帧。
- 停止顺序：`stop` 置位 → `to_worker` 关闭 → `drain` 后 `join`；遗留的 `HostDone` 与 Promise capability 一起丢弃。

### 5.4 取消与超时（没有 `kill` 的替代）

| 层 | 机制 | 触发后 |
|---|---|---|
| 一次进 JS | interrupt handler：`stop` 置位或**线程 CPU 时间**超预算（默认 5 s，`CLOCK_THREAD_CPUTIME_ID`，与 `nsh-js` `sandbox::Budget` 同法） | 抛不可 `catch` 异常结束本次进入 |
| 一次 invocation | 现有 `INVOCATION_TIMEOUT = 60 s`（tokio 侧，不改） | 丢弃 `Pending`，worker 标记死亡 |
| 单次 host call | 现有 `NETWORK_REQUEST_TIMEOUT` 等常量（不改） | 同今天 |
| VM fatal / interrupt 命中 | **销毁整个 VM**，下次 `connection()` 重建 | 与今天「进程退出 → `pending.close` → 下次 `spawn`」完全同构，`WorkerProcess` 的 `Drop` 语义不变 |

关键不变量：**被 interrupt 打断的 JS 状态不作修复，直接丢弃**。半个 `__recv`、悬空的 promise capability 都比重建便宜。

### 5.5 模块加载

`loader.rs` 自己实现 `rquickjs::loader::{Resolver, Loader}`（`nsh-js` 的 `modules.rs` 是同款参考实现）：

```text
"cursor-byok-next:plugin"              → "sdk/plugin.js"
"cursor-byok-next:provider"            → "sdk/provider.js"（空导出）
"cursor-byok-next:model" | ":resource" → 同上
"cursor-byok-next:host"                → "sdk/host.js"（六个能力，转发到 vm/host_module.rs 的 native）
"./provider.ts"  from "magpie/main.js" → "magpie/provider.js"   ← build.rs 改写扩展名
其他裸名                                → Err（"could not load module"）
```

`sdk/host.ts` 是新增的类型 + 转发层：它只是把 `PluginContext` 的调用转成 native，不含任何策略；插件作者仍然只 import `cursor-byok-next:plugin` 拿类型。

- 一张 `&'static [(&'static str, &'static str)]` 表（id → 源码），全部来自 `include_str!`。**不写盘、不留临时目录**，因此 `RuntimeDirectory`、清理线程、`0700`/`0600` 权限那一套整体删除。
- **不用 bytecode**：312 行插件的解析是微秒级；bytecode 会把插件钉死在 `BC_VERSION`（`bc28`）上，且动态 `import()` 需要保留 debug 元数据。
- `Context::full()`：拿到 `JSON`、`Promise`、async generator（QuickJS 0.16.2 已确认支持 `for await`、`async *`、TLA）。
- 入口用 `Module::eval()` 拿到模块 promise，再用 `Promise::state()` 判定：`Rejected` ⇒ 读 `Promise::result()` 当作启动失败（已在本仓库 `libquickjs.a` 上验证：import 期 throw 与 TLA reject 都在此可观测）。

### 5.6 宿主调用：能力 native + 会话帧（方案 D）

**为什么不沿用帧。** Bun 只有一根 stdin/stdout，所有回宿主的能力调用只能复用 `host_call` / `host_result` 帧。进进程后这条约束消失，继续复用只剩成本：

- 一次 `fetch` 要经历 `stringify` → 帧信封 → mpsc → `read_frames` → `tokio::spawn` → 结果帧 → 再 `parse`；
- `protocol.stream` 只等一个 HTTP 状态，却要占满一轮帧协议（事件由适配器直写，本就不过 VM）；
- 能力调用和 `result` / `event` 抢同一个写队列（`WRITE_QUEUE_FRAMES = 32`）。

**为什么不做成同步 native。** 插件 VM 只有一条线程，上面同时跑着最多 `MAX_ACTIVE_INVOCATIONS = 16` 个 invocation、`setTimeout` 和 `cancel`。同步堵在 HTTP 上，`AbortSignal` 要等请求自己结束才能被读到，同 VM 的其它 invocation 也一起停。所以凡是真 I/O，一律「调用即返回 Promise，完成走队列」。

**为什么不复用 `nsh_js::Host` 的那套绑定。** `CallHost` / `Op::Rust` / `#[class]` 假定「同步调用里直接转 JSValue」并把活排进 UI 响应图；插件这边是 tokio 上的 HTTP，且必须与 UI 的 engine 线程完全隔离（§5.1）。

```text
能力调用（不占会话帧）

  JS  ctx.network.fetch(url, init)
    → native host.fetch(cx, this, argc, argv)
    → 参数校验（url / headers / body 是本模块的小 JSON）
    → ctx.requestId → 查 InvocationState（取消 token、白名单、预算、recorder）
    → tokio Handle::spawn(HostContext::call(...))
    → 立刻返回 Promise；capability 存在 VM 的 pending 表
    → tokio 侧完成 → host_done 队列 → drain_host_done → resolve / reject
```

`cursor-byok-next:host` 的导出（由 `vm/host_module.rs` 装，走 §5.5 的模块表）：

| 导出 | 签名 | Rust 侧 | 形态 |
|---|---|---|---|
| `fetch(requestId, url, initJson)` | `Promise<string>` | `HostContext::call("network.fetch")` | 异步 |
| `streamOpen(requestId, url, initJson)` | `Promise<string>` | `network.stream.open` | 异步 |
| `streamRead(requestId, streamId)` | `Promise<string>` | `network.stream.read` | 异步 |
| `streamClose(requestId, streamId)` | `Promise<null>` | `network.stream.close` | 异步 |
| `protocolStream(requestId, inputJson)` | `Promise<string>` | `adapters::stream` | 异步 |
| `readRequest(requestId)` | `string` | `wire::llm_request(pending.model)` | **同步** |

- **参数与结果仍是 JSON 字符串**。这六个方法的输入输出本来就是 JSON（url、headers、body、`{lines, done}`、HTTP 状态）。方案 D 省掉的是帧信封与会话写队列的争用，不是 `JSON.stringify` 本身；不为 `undefined` / `Map` / 循环引用写 QuickJS 值双向转换器。
- **`readRequest` 同步**。`wire::llm_request` 只读 `ModelInvocation` 的不可变数据，`InvocationState.model` 是 `Arc`，不需要 tokio。Magpie 不调用它；自定义协议插件少一次排队。
- **策略全在 `host_calls.rs`。** 白名单、拒绝带凭证 URL、不跟随重定向、`NETWORK_REQUEST_TIMEOUT`、`MAX_STREAMS_PER_INVOCATION`、`CallRecorder` 一个都不搬。native 只做三件事：把 JS 参数交过去、把结果 Promise 掉、记一次预算。
- **上限照旧。** `MAX_HOST_CALLS_PER_INVOCATION = 8` 从 `spawn_host_call` 移到 native 的 pending 表计数，超限的 Promise 直接 reject（`"too many concurrent host calls"`）。
- **取消照旧。** 每个待决 Promise 记录它属于哪个 `requestId`；`cancel` 帧或 invocation 结束时，VM 把这一组 reject 掉，tokio 任务靠 `CancellationToken` 自行退出。

`sdk/worker.ts` 的改动（相对今天的 Bun 版）：

1. `argv.slice(2)` → `__entry()`；`console.log(DEFINITION:…)` → `__send` 一帧。
2. `hostCall()` 的实现从 `send({type:"host_call"…})` 改成调 `cursor-byok-next:host` 的 native；参数仍是 `requestId` + 方法 + JSON。
3. 删除 `pendingHost` 表与 `host_result` / `host_error` 分支——Promise capability 现在在 Rust 侧，`worker.ts` 只需要 `await native`。
4. `dispatch()` / `contextFor()` / 各 method 分支保持不变；`readRequest` 由 `async` 变为直接返回（调用方已经 `await`，语义不变）。

会话帧随之缩小到四类：

```text
host → VM : {type:"request", id, method, params}
            {type:"cancel", id}
VM → host : {type:"result", id, result | error}
            {type:"event", id, event}
```

`__recv` / `__send` / `__entry` 留在 `natives.rs`：

| native | 签名 | 说明 |
|---|---|---|
| `__entry()` | `() -> string` | 取代 `argv.slice(2)`；`worker.ts` 用它 `await import()` |
| `__recv(frame)` | `(string) -> void` | 会话帧入口，只认 `request` / `cancel` |
| `__send(frame)` | `(string) -> void` | 会话帧出口，只发 `result` / `event` |

### 5.7 需要宿主补的全局（`globals.rs`）

已在 `libquickjs.a`（quickjs 0.16.2）实测：`URL` 与 `AbortController` **不存在**；`queueMicrotask`、`WeakRef`、`atob/btoa`、`Promise`、`Symbol.asyncIterator` 存在；`import.meta` 是对象但 **`import.meta.url` 为 `undefined`**。

| 全局 | 需要它的地方 | 实现 |
|---|---|---|
| `URL` | `resources.ts` 的 `gatewayOrigin`（取 `protocol` / `username` / `password` / `hostname` / `origin`） | 原生类，底层用 `url` crate；**宿主侧 `origin::normalize_origin` 仍是唯一权威**，JS 这份只是过滤器 |
| `AbortController` / `AbortSignal` | SDK 的 `context.signal.aborted` | 最小实现（`aborted` + `addEventListener("abort")`），≈80 行，不实现 `EventTarget` 继承 |
| `setTimeout` / `setInterval` | 插件重试逻辑 | `timers.rs`，由 §5.3 的 `recv_timeout` 驱动 |
| `console.*` | 调试 | 原生 → `tracing`，target `plugin`（今天 stderr 的行等价物） |

**不提供**：`node:*`、`Bun.*`、`fetch`、`WebSocket`、`fs`、`process`、`structuredClone`。出网只能走 `context.network.*`，由 `host_calls.rs` 校验——这是本方案最大的安全收益。

### 5.8 TypeScript 编译（方案 A 的具体形态）

`build.rs` 新增一段（与现有 proto 生成并列）：

```text
输入：src/plugins/sdk/{plugin,worker}.ts
      plugins/build-in/magpie/{main,provider,catalog,resources}.ts
步骤：Parser::new(&allocator, src, SourceType::from_path(path))
      SemanticBuilder::new().with_enum_eval(true)        ← 见下方「已验证的坑」
      Transformer::new(&allocator, path, &TransformOptions {
          env: EnvOptions::default(),                    ← 只剥 TS，不做语法降级
          ..Default::default()
      })
      Codegen::new().build(&program).code
输出：OUT_DIR/plugins/<module id>.js，并生成一张 id → path 的表由 include_str! 收进 builtin.rs
改写：相对 import 的 ".ts" 后缀改为 ".js"
fail-closed：解析或转换有任何 diagnostic 立即 panic，构建失败
```

**已验证的两个坑**（在 `oxc 0.159` + 本仓库 `libquickjs.a` 上实跑）：

1. `TransformOptions::default()` 的 `EnvOptions` **全关闭**，只剥类型：`#private`、`static {}`、`async *`、`||=`、`??=`、`import.meta` 原样保留，QuickJS 都能执行。一旦设 `EnvOptions::from_target("es2020")`，oxc 会把私有字段降级成 `require("@oxc-project/runtime/helpers/…")`——ESM 里没有 `require`，插件直接崩。**因此目标必须是「不降级」。**
2. TS `enum` 需要 `SemanticBuilder::with_enum_eval(true)`，否则 transformer 直接 panic（`enum.rs:180`）。Magpie 目前没用 `enum`，但第三方插件迟早会用，这条必须写进 `build.rs`。

`tsconfig.json` 的 `cursor-byok-next:*` 映射与服务端 `include/exclude` 保持不变——作者体验不变，只是运行期不再需要它。

### 5.9 descriptor 收集（`vm/collect.rs`）

今天：`bun collect.ts <entry url>` → stdout `CURSOR_BYOK_NEXT_PLUGIN_DEFINITION:<json>`。

目标：同一份 VM 里先 `globalThis.__entry = "<module id>"`，再求值内建模块 `sdk/collect.js`（`collect.ts` 的编译产物：`await import(__entry())` → `__descriptor(__getRegisteredPlugin())` → `__send` 一帧 descriptor），Rust 侧读该帧即可。

`PluginModuleDefinition` 的 `serde_json` 反序列化与 `deny_unknown_fields` 校验**原样复用**，`descriptor.rs` 一行不改。`definition.rs` 整体删除（Bun 命令、临时工作目录、stdout 解析、`file_url`）。collector 与 worker 各自一份独立 VM：collector 求值后立即丢弃，命名空间不泄漏进 worker。

## 6. 功能要求与不变量

1. **会话帧与能力调用分工固定。** 会话帧只有 `request` / `cancel` / `result` / `event`；`HostMessage` / `WorkerMessage` 的 tag、`requestId`、可选 `status`、16 MiB 帧上限、`WRITE_QUEUE_FRAMES = 32`、`EVENT_QUEUE_ITEMS = 4096`、`MAX_ACTIVE_INVOCATIONS = 16`、`MAX_STREAMS_PER_INVOCATION = 16` 沿用现值。`MAX_HOST_CALLS_PER_INVOCATION = 8` 改由 native 的待决表强制。
2. **能力面只由宿主决定。** 插件 VM 里除 `natives.rs` 的三个入口、`host_module.rs` 的六个能力、`globals.rs` 装的对象、模块表里的模块之外，没有任何可达宿主的路径；没有 `fs`、`process`、`fetch`、`Bun`、`require`，`import()` 只认表内名字。
3. **出网只经 `host_calls.rs`。** `manifest.permissions.network` + `resourceOrigin` 的白名单、拒绝带凭证 URL、不跟随重定向、调用级取消与上限，全部照旧；native 不复制任何一条检查。
4. **单线程、单堆、单预算。** 一个插件一条线程；堆上限由自有 allocator 强制（`set_memory_limit` 对自定义 allocator 无效，沿用 `nsh-js::sandbox::HeapAllocator` 的做法）；每次进 JS 受 CPU 预算约束。超限 **销毁 VM 并重建**，不带病继续。
5. **VM 线程不碰 tokio。** VM 线程只发起 `Handle::spawn`、从 `host_done` 队列取结果；tokio 任务只往队列丢结果，不持有任何 QuickJS 值。
6. **构建期编译，运行期零 TS。** 二进制里没有任何 `.ts` 源码；解析或转换失败让构建失败，绝不静默跳过。
7. **依赖版本锁死。** `rquickjs = { package = "quickjs-jit", version = "=0.14.2" }`，与 `nsh-js` 相同；**不得开启 `jit-abi`**（会改 C ABI 并要求后端 vtable）。加一条测试断言锁文件里 `quickjs-jit-sys` 只有一个版本。
8. **失败语义不变。** 目录刷新失败保留旧缓存、无缓存只报宿主观察到的 HTTP 状态、不透传网关响应原文、不回显密钥——这些都在 TS 与 `host_calls.rs` 里，本轮不动。
9. **UI 结构不变。** `PluginSnapshot { runtime_ready, runtime_error, plugins }` 字段形状不变，只是语义从「Bun 在不在」变成「插件 VM 起没起来」。
10. **删除即完成。** 生产代码、`build.rs`、CI、`tsconfig.json`、`Cargo.toml` 里不再出现 `bun`；`definition.rs`、`bunfig.toml`、`collect.ts`、清理线程全部消失。

## 7. 交付顺序

1. **探针与骨架**：`vm/runtime.rs` + `vm/loader.rs` + `vm/natives.rs`，用一个两模块玩具插件跑通 `import` → `defineProviderPlugin` → `__send` descriptor。验收：descriptor 帧完整；随后在 `vm/host_module.rs` 里用一条 `fetch` 走通 native → tokio → Promise 的 resolve / reject 两分支。
2. **构建期编译**：`build.rs` 接 oxc，产物进 `OUT_DIR`；`builtin.rs` 换成编译产物清单；`vm/collect.rs` 替掉 `definition.rs`。验收：插件页在**没有 Bun** 的机器上列出 Magpie。
3. **会话帧替换**：`worker.rs` 进程 → 线程，`read_frames` 换输入源；先只搬 `request` / `cancel` / `result` / `event`。验收：`magpie_catalog_through_quickjs`（由今天的 `..._real_bun` 改名）逐断言通过，`LAST_INVOKE_FRAME` 里的 `request` / `result` 帧逐字节一致。
4. **能力调用切到 native**：`sdk/worker.ts` 的 `hostCall()` 改为调 `cursor-byok-next:host`；删 `spawn_host_call`，把预算与取消搬进 VM 的 pending 表；`readRequest` 改同步。验收：`fetch` / `stream` / `protocol.stream` 三条路径的行为与第 3 步一致；沙箱测试里插件无法自造 `host_call` 帧。
5. **全局补齐**：`vm/globals.rs` + `vm/timers.rs`；`sdk/worker.ts` 的既有三处改动。验收：表单添加路径（`new URL`）与 `AbortSignal` 取消路径通过。
6. **删除 Bun**：`mod.rs` / `host.rs` 的探测、CI 的 `setup-bun`、`tsconfig.json` 注释、`Cargo.toml` 依赖复查。验收：`cargo test` / `nsh check` / `nsh test` 全绿，`rg -i bun` 只剩文档。
7. **真实网关**：`nsh run` 接一台真实 Magpie，跑一次 Cursor turn（流式文本、thinking、工具调用、用量、取消），与迁移前对比。

第 3 步与第 4 步分开，是为了让「换运行时」与「换调用方式」各自可回滚：第 3 步只动进程，第 4 步只动能力调用的传输。

## 8. 验收清单

- [ ] **无 Bun 可用**：干净环境（PATH 无 `bun`、无 `~/.bun`）下插件页列出 Magpie 并可连接网关。
- [ ] **会话帧等价**：`provider.invoke` 的 `request` / `result` / `event` 帧与今天逐字节一致（黄金文件取自 `LAST_INVOKE_FRAME` 与 Magpie 集成测试）。`host_call` 帧只允许出现在旧样本的解析测试里。
- [ ] **能力调用无帧**：抓一次 `models.list`，会话帧数量等于「1 request + 1 result」；`network.fetch` 不出现在任何帧里。
- [ ] **目录行为**：去重、媒体过滤、稳定 ID、503 回退与脱敏、按资源失效、移除、损坏存储，与原 `magpie_catalog_through_real_bun` 断言一一对应。
- [ ] **沙箱边界**：插件内 `typeof fetch` / `typeof process` / `typeof require` / `import("node:fs")` / `import("fs")` 全部失败；`context.network.fetch` 到非白名单 origin 得到 `ERR_ACCESS_DENIED`。
- [ ] **预算与重建**：`while(true)` 插件在 CPU 预算内被打断、VM 被销毁、下一次 invoke 成功；超大分配得到 `RangeError` 而非宿主崩溃。
- [ ] **取消**：`CancellationToken` → `AbortSignal.aborted`；取消时待决的能力 Promise 全部 reject，tokio 任务退出，不产生半个 result。
- [ ] **并发上限**：一个 invocation 同时发起 9 个 `fetch`，第 9 个 Promise reject `"too many concurrent host calls"`；其余完成。
- [ ] **启动失败可观测**：插件 import 期 throw / TLA reject / 缺模块 / 编译失败，四种都归到 `plugin.error.vm_failed` 并带原因（不把插件源码路径或响应体写进 UI）。
- [ ] **TS 编译门禁**：故意写一个类型错误的插件，构建失败并打印诊断；空跑一次确认 `build.rs` 不产生 `require(...)`。
- [ ] **无残留进程**：`ps` 里没有插件子进程，临时目录没有 `cursor-byok-next-plugin-*`。
- [ ] **发布验证**：`cargo test --workspace`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、`nsh check`、`nsh test`；真实 Cursor + Magpie 端到端 turn 单独验收。

## 9. 测试

| 类型 | 内容 |
|---|---|
| 回归 | 由 `magpie_catalog_through_real_bun` 改名的 `magpie_catalog_through_quickjs`，断言逐项保留（假网关、临时数据目录、两资源去重） |
| 黄金帧 | `LAST_INVOKE_FRAME` 快照，只比 `request` / `result` / `event`；`host_call` 相关的断言改为「不出现」 |
| 能力调用 | 一个假 `HostContext`：`fetch` resolve / reject / 带 `status` 的 reject 三分支各一条；`readRequest` 同步返回；`streamRead` 的 `{lines, done}` 循环到 `done` |
| 并发与取消 | 9 路并发拒绝第 9 个；`cancel` 后待决 Promise 全部 reject 且 tokio 任务数归零 |
| 沙箱 | §8 的边界与预算两组 |
| 加载器 | 未知 specifier 拒绝、`./x` 越界拒绝、动态 `import()` 命中同一张表；`cursor-byok-next:host` 的六个导出可 import，其余 native 只在 `globalThis` 上 |
| 全局 | `new URL` 与 `url` crate 对 Magpie 用到的 5 个字段取值一致；`AbortController` 语义 |
| 构建 | `build.rs` 的 oxc 步骤单测：一段含 `enum` / `import type` / 相对 `.ts` 的输入 → 期望输出 |

## 10. 文档同步

- 新增移植对照记录 `PLUGIN_RUNTIME_VM.md`：目录映射、逐项差异（运行时、TS 编译时机、能力面、能力调用传输、停止机制）、不变量与测试映射、验证过的 QuickJS 能力清单。
- 更新 [PLUGIN_CATALOG.md](PLUGIN_CATALOG.md) 差异表：#1（Deno → Bun）改为「进程内 QuickJS VM，无外部运行时」；#7（内置插件写临时目录）改为「构建期编译为模块表，`include_str!` 内嵌」；#18（清理线程）整条删除；「验证」段删掉「需有 Bun」。
- 更新 [PLUGIN_PROTOCOL_ADAPTERS.md](PLUGIN_PROTOCOL_ADAPTERS.md)：传输描述改为「`context.protocol.stream` → native → 适配器」，注明事件不过 VM、只回 HTTP 状态。
- 更新根 `AGENTS.md`：插件目录树（`vm/`、`vm/host_module.rs`）、SDK specifier 说明（多一条 `cursor-byok-next:host`）、当前进度。
- `.github/release-notes/0.0.1.md` 的「Bun 插件宿主」改为「进程内 QuickJS 插件宿主」。
- 协议字段证据仍归 `docs/protocol/`，不进本 PRD。

## 附录 A：为什么不是其它三种

| 方案 | 做法 | 结论 |
|---|---|---|
| A（原 PRD） | 会话帧加 `__hostCall` / `host_result` | 移植最快，但进程内多两跳，且能力结果与会话帧抢同一个写队列 |
| B | 能力做成同步 native，JS 线程等 tokio | 会冻住 `cancel`、timer 和同 VM 的其它 invocation；`MAX_ACTIVE_INVOCATIONS = 16` 的前提被破坏 |
| C | 复用 `nsh_js::CallHost` / `Op::Rust` 绑定 | 把插件绑进 UI 响应图，且 `#[class]` 的同步转换模型对不上 HTTP |
| **D** | 能力 = native Promise + 完成队列；会话 = 四类帧 | 策略面（`host_calls.rs`）一动不动，作者面（`PluginContext`）不变 |

B 看起来最像普通 FFI，但它把「一次调用 = 一次 JS 线程阻塞」当成了常态；`AbortSignal` 恰恰要求 HTTP 期间 VM 线程仍然能转。

## 附录 B：已验证事实（本轮实测）

在 `apps/desktop/target/debug/build/quickjs-jit-sys-*/out/libquickjs.a` 上编了一个 C 探针（`JS_SetModuleLoaderFunc` + `JS_ExecutePendingJob`）：

| 项 | 结果 |
|---|---|
| 引擎版本 | `JS_GetVersion() == "0.16.2"` |
| ESM 图 | 三角形 import（`main → provider → resources`）+ 裸名 `cursor-byok-next:plugin` 全部解析成功 |
| Top-level await | 求值后 `jobs=2` 才排空；TLA 之后的赋值可见 |
| import 期 throw / TLA reject | 模块 promise 停在 `JS_PROMISE_REJECTED`，`JS_PromiseResult` 拿到原因 |
| 缺模块 / 语法错 | loader 返回 NULL，`JS_Eval` 的异常可读 |
| 类语法 | `#private`、`static {}`、`async *`、`extends Error` + 声明字段赋值均正常 |
| `import.meta` | 是对象；**`import.meta.url` 为 `undefined`**（插件不得依赖它） |
| `URL` / `AbortController` | **均 `undefined`**，必须由宿主补 |
| oxc 默认转换 | 只剥 TS：`import type` 消失、`readonly` 构造参数变成字段赋值、`enum` 变 IIFE、其余语法不动 |
| oxc 降级到 ES2020 | 私有字段变 `require("@oxc-project/runtime/…")`，**不可用** |

未验证、列为交付第 1 步验收项：native 的 Promise capability 往返（resolve / reject / 带 `status` 的 reject 三分支）、`tokio::Handle` 从 VM 线程 `spawn` 后经完成队列回 VM 的时序。