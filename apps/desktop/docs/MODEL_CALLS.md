# 移植记录：模型调用记录

上游实现：`cursor-byok/server/src/{provider/recorder,store/llm_calls,plugin/worker}.rs` + `apps/desktop/src/features/calls/`。
本移植：`apps/desktop/src/calls/` + `plugins/{host,worker}.rs` + `loop_service/service.rs` + `src/screens/{Calls,CallDetails}.tsx`。

范围只有一件事：**记录 Cursor local loop 里每次 Magpie `InvokeModel` 的生命周期、用量和可选原始请求/响应**。不记录官方模型、不移植首页统计图表、不引入 HTTP 控制 API。

## 目录映射

```text
cursor-byok/server/src/                         apps/desktop/src/
├── store/llm_calls.rs            523  调用落库      calls/store.rs          SQLite、查询、两种清理
├── store/storage.rs              111  清理计数      calls/store.rs          无官方 trace
├── provider/recorder.rs          463  调用生命周期  calls/recorder.rs       观测失败不阻断模型
├── provider/event.rs              55  TTFR 判定     provider.rs             is_valid_response_event
├── plugin/worker.rs              首个 HTTP 认领     plugins/worker.rs       脱敏头 + 原始 chunk；protocol.stream 共用 prepare_request
├── plugin/registry.rs            传入 recorder      plugins/host.rs         stream_model(..., recorder)
├── cursor/inference/service.rs   InvokeModel        loop_service/service.rs 开始 / 事件 / 终态
└── control/service.rs            HTTP API           lib.rs Calls class      native 查询与命令

cursor-byok/apps/desktop/src/features/calls/    apps/desktop/src/screens/
├── CallsPage.tsx / CallTable.tsx               Calls.tsx            虚拟列表、详细模式、清理
└── CallDetails.tsx / CallDetailsPage.tsx       CallDetails.tsx      字段、请求 JSON、响应流
```

行数为上游 `wc -l` 对照；数据库文件在 `~/.cursor-byok-next/calls.db`。

## 主路径

```text
InvokeModel
  → CallRecorder::start（运行中，快照详细模式）
  → PluginHost.stream_model(recorder)
  → 插件首个 protocol.stream（或仍走 network.fetch / network.stream.open 的自定义协议）
      → 脱敏请求头 + 请求体、HTTP 状态、原始响应字节
  → 每个 ModelEvent → TTFR / TTFT / Usage
  → 网络分块进入有界观测队列，独立 writer 批量落库
  → completed | error | cancelled（关闭队列、排空后写终态）
  → Calls.snapshot / detail
  → TSX signal 镜像
```

## 差异清单

| # | 主题 | 上游 | nsh 版 | 原因 |
|---|---|---|---|---|
| 1 | 来源 | 内置 Provider + Magpie + 外部 API + 官方 trace | 只 Magpie local loop | AGENTS.md：只做 agent local loop |
| 2 | 查询边界 | `/__byok-api__/api/llm-calls` | `Calls` native class | mixed 应用状态走 Rust 方法，不另开 HTTP |
| 3 | 首页概览 | `overview` 图表 | 不做 | 本步只做调用记录 |
| 4 | 数据库 | 主库 migrations 0001/0002/0005/0007 | 独立 `calls.db`，`0001_calls.sql` 一次建最终 schema | 本仓库没有上游主库；当前不兼容开发期旧库，删除旧 `calls.db` 后重建 |
| 5 | 观测失败 | 部分路径可失败上抛 | 记日志并 `degraded`，不阻断模型 | 不变量 7：状态只读不抛；调用本身仍要完成 |
| 6 | 详细模式 | 调用开始快照 | 相同 | 在途调用不被后来的开关改写 |
| 7 | 首个 HTTP | `claim_recorder` | 相同 | 模型发现 / 后续请求不污染该次调用 |
| 8 | 列表同步 | 控制 API 拉取 | 版本化 signal 镜像；操作期间暂停轮询 | 旧查询不能覆盖较新的开关或清理结果 |
| 9 | 数据保留 | 无自动上限 | 新调用启动时只保留最近 1000 次；请求体、响应原文各最多 1 MiB | 防止详细记录无限增长；完整网络字节数和用量仍保留 |
| 10 | 详情不存在 | HTTP 404 | `detail()` 返回 `null` | 已清理调用与数据库读取失败是不同界面状态 |
| 11 | 观测写入 | 网络流内批量等待 SQLite | 有界队列 + 独立 writer，终态前关闭并排空 | 数据库批量写入不占用模型事件处理；终态后响应计数不再变化 |
| 12 | 指标名称 | TTFB / stream event | 响应头耗时 / 原始网络分块数 | 名称与实现一致；首块时间取到达偏移，而非 flush 时刻 |
| 13 | 调用记录的模型坐标 | 解构 `plugin_request_url` 的三元组 | `PluginRequestUrl { model, provider, url }` | 具名字段让“模型 id / provider 族 / 请求 URL”在写入 `NewLlmCall` 时不会错位 |
| 14 | 记录暂停时钟 | 各自 `SystemTime` 调用 | 统一 `crate::time::now_ms()`，饱和到 `i64::MAX` | 三处（calls/plugins/knowledge）曾各有一份 `now_ms`，行为已一致，合并后只留一份 |


## 必须保持的不变量

1. 只记录本地 Magpie `InvokeModel`，官方 `Upstream` 不落库。
2. 终态只允许 `running → completed/error/cancelled`；存储层的所有生命周期更新都带 `status = 'running'`，迟到写入不能覆盖已取消。
3. 详细模式在 `start()` 快照；关闭时仍累计网络响应字节数和原始分块数，不落 payload。请求与响应原文各最多保存 1 MiB，截断只影响调试原文，不影响模型流或完整指标。
4. `authorization` / `proxy-authorization` / `x-api-key` / `api-key` / `cookie` / `set-cookie` 永不落库。
5. 一次插件调用只认领首个上游 HTTP 请求。
6. `details` 清理只删请求和响应块并复位截断标志；`all` 删全部调用摘要。
7. 读取接口失败返回空列表/`degraded`，不向 UI 抛状态查询错误；详情不存在返回 `null`。
8. 测试用临时目录，不读 `~/.cursor-byok-next` 或真实 Cursor 路径。
9. 凭证、`privateData` 不进 UI、日志和测试快照。
10. 列表只在最新版本仍有效时应用查询结果；手动操作使旧轮询失效，页面卸载后不再写 signal。
11. 新调用启动时按 `created_at_ms DESC, rowid DESC` 清理 1000 名之外的旧调用，外键级联删除其详细内容。
12. 网络分块发送端到终态期间只关闭一次；writer 排空后才允许写终态。响应头耗时取响应头首次到达时间，首网络块时间取接收偏移，二者不能用 flush 时间替代。

## 验证

```sh
cargo test -p nsh-app-cursor-byok-next --lib
cd apps/desktop && nsh check && nsh test
nsh test test-fixtures/Visual.test.tsx --target macos
```

Rust 覆盖：最终 schema 与重复打开、往返、详细模式快照、敏感头、首个请求认领、TTFR/TTFT first-wins、完成/失败/取消幂等、终态后所有字段只读、drop 收尾、观测队列排空、两种清理、payload 上限与截断标志、保留策略、官方路由不落库。`magpie_invocation_records_the_completed_call` 与 `magpie_invocation_records_cancellation` 走真实 Bun + 假网关。TSX 覆盖列表、详情、清理确认、空状态、详情不存在和中英文；`callSync.test.ts` 覆盖旧轮询与卸载后的结果失效。测试夹具 `capabilities.host` 含 `calls.*`。

### 设计完成清单（调用列表 / 详情）

- 圆角：卡片与弹窗用 `--radius-lg`，按钮与代码块用 `--radius`，状态标签为胶囊。
- 字体：标题 / 正文 / 辅助 / 等宽标识四类角色；用量数字 `tabular-nums`。
- 间距：主区 16pt，卡片 16pt，行内 8pt；清理按钮与刷新、开关同一工具条。
- 对齐：状态 / 模型 / HTTP / 耗时 / Token / 错误共用固定车道；数字右对齐。
- 颜色：只用主题 token；成功 / 失败同时有文字。
- hover / pressed / focus：行加深背景不缩放；按钮缩放 0.97；`focus-visible` ring。
- 状态：空、加载、读取失败、降级、无详情、调用不存在、请求/响应截断、清理确认均有文案；空列表时清理按钮禁用。

### 未完成验证

- `nsh check`、`nsh test`、macOS 明暗主题 `Visual.test.tsx` 已执行；列表与详情截图已检查圆角、字体、间距、对齐、颜色及状态。
- 真实 Cursor Magpie turn 的端到端验收未做（需接管后真实调用）。
- VoiceOver、Windows / Linux、减少动态效果、大字号和窄窗口未在真实机器上逐项核对；`createMotion` 默认跟随 `reduceMotion`，控件有 `motion-reduce:`。
