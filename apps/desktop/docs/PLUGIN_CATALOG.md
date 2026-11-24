# 移植记录：插件模型目录（Magpie）

上游实现：`cursor-byok/server/src/plugin/` + `server/plugins/build-in/magpie/`。
本移植：`apps/desktop/src/plugins/` + `apps/desktop/plugins/build-in/magpie/` + `src/screens/Plugins.tsx`。

范围只有一件事：**让 Magpie 网关的实时模型目录出现在本应用的插件配置页**。不接入 Cursor `AvailableModels` / `GetUsableModels`，不实现 `provider.invoke` 调用，不在核心 UI 内置通用模型增删改。

## 目录映射

```text
cursor-byok/server/src/plugin/              apps/desktop/src/plugins/
├── registry.rs     插件能力编排              ├── host.rs           生命周期、资源操作与 worker 复用
├── catalog.rs      扫描 plugin.json          ├── catalog.rs        只加载内置 Magpie
├── definition.rs   Deno 收集 descriptor      ├── definition.rs    共用 Bun 启动参数，收集 descriptor
├── worker.rs       Deno JSONL IPC            ├── worker.rs        Bun JSONL IPC，按进程管理调用
│                                             ├── host_calls.rs    worker 可用的能力：fetch / stream / readRequest / 协议适配器
│                                             ├── catalog_cache.rs live 目录的 TTL 缓存与失效
├── state.rs        资源与 snapshot 目录      ├── state.rs         资源记录与持久化
├── data.rs         原子 JSON 文件            ├── data.rs          原子 JSON 文件
├── origin.rs       origin 校验               ├── origin.rs        origin 校验
├── descriptor.rs   稳定 ID / 投影            ├── descriptor.rs    稳定 ID / 投影
├── manifest.rs     plugin.json               ├── manifest.rs      plugin.json
├── protocol.rs     JSONL 消息                ├── protocol.rs      JSONL 消息
├── builtin.rs      预装内置插件              ├── builtin.rs       从应用内嵌内容写入私有临时目录
└── sdk/            Deno import map           └── sdk/             Bun `tsconfig` 路径映射
    ├── plugin.ts / model.ts / resource.ts
    ├── collect.ts / worker.ts
    （`:protocol/*` 已删除，见 PLUGIN_PROTOCOL_ADAPTERS.md）

cursor-byok/server/plugins/build-in/magpie/ apps/desktop/plugins/build-in/magpie/
├── plugin.json / main.ts / catalog.ts / resources.ts / provider.ts
└── magpie_test.ts                            生产解析与表单由真实 Bun 集成测试覆盖
```

按 `wc -l` 统计，含测试和空行、不含生成代码：Rust 宿主（不含 adapters）约 5000 行，协议适配器约 2892 行，Magpie 插件 TS 312 行，SDK 核心/worker TS 787 行。nsh 侧新增 `src/screens/Plugins.tsx`：NSH mixed 应用当前没有 `.nshpkg` 加载 API，插件配置页先做在宿主 TSX 里；`PluginHost` 的 native 导出仍是后续签名包可复用的边界。协议转发见 `PLUGIN_PROTOCOL_ADAPTERS.md`。

## 本步边界

- Rust 拥有资源凭证、live 目录缓存、worker 生命周期；TSX 只镜像 `PluginHost.snapshot()`。
- Magpie `catalog: "live"`：目录不落盘，按资源短缓存 `models.list`。缓存同时保留旧模型与最近一次刷新错误；资源变更仅失效对应条目。
- 首期只执行随应用分发的 Magpie，不扫描任意第三方插件。Bun 是同用户子进程，不是 OS 沙箱；origin 权限检查只约束 Rust 提供的 SDK 网络接口。
- 连接表单属于插件自有资源配置，不是核心模型 CRUD。
- Cursor picker 与 local loop 在后续步骤接入：见 `LOCAL_LOOP.md`。本步只保证目录页能列出 Magpie 模型。

## 差异清单

| # | 主题 | 上游 | nsh 版 | 原因 |
|---|---|---|---|---|
| 1 | 运行时 | Deno 子进程 | PATH 上的 `bun` | 用户指定 Bun；首期不捆绑运行时二进制 |
| 2 | UI 交付 | React 插件页 | 宿主 TSX 插件页，native API 预留给 `.nshpkg` | NSH 签名包是 TSX-only，当前没有 headless package runner |
| 3 | 能力范围 | `models.list` + `provider.invoke` + OAuth/import | form 添加、present、remove、`models.list`、invoke | invoke 接到 `LOCAL_LOOP.md`；协议适配器见 `PLUGIN_PROTOCOL_ADAPTERS.md` |
| 4 | 模型广告 | 追加到 Cursor AvailableModels | 本步只在插件页展示 | 广告接到 `LOCAL_LOOP.md` |
| 5 | 运行时安装 | 下载/校验 Deno | 检测 PATH 上的 bun | 不引入运行时分发 |
| 6 | SDK 协议 helper | Deno import map 含 openai/anthropic | 已删除；`context.protocol.stream` 走 Rust 适配器 | 见 `PLUGIN_PROTOCOL_ADAPTERS.md` |
| 7 | 内置插件加载 | 从安装目录发现插件 | 内嵌 Magpie，collector 与 worker 共用 `--tsconfig-override` | 修复 Bun specifier 解析，不执行任意安装目录中的代码 |
| 8 | 失败反馈 | 通用插件错误 | 按资源保留旧目录和失败信息；存储读失败单独提示；不回显网关响应体 | 让用户区分离线、未配置与本地存储故障 |
| 9 | 插件源类型检查 | 桌面端单独 `tsconfig.node.json`，`include: ["plugins/**/*.ts"]` | 应用 `tsconfig.json` 纳入 `plugins/**` 并映射四个 SDK specifier | NSH 应用只有一份 tsconfig；插件源不再落在任何 TS 工程外，编辑器与 `tsc` 才能解析 `cursor-byok-next:*` |
| 10 | `import` / `actions` 能力 | 资源定义携带导入与动作描述，SDK worker 有 `import.parse` / `resource.action` | 三处一起删除：`ResourceDefinition` 字段、`sdk/plugin.ts` 的 `__descriptor` 输出、`sdk/worker.ts` 的两个 method | beta 不做向后兼容：宿主没有执行路径，保留类型与字段只会让插件以为导入和动作可用。重新移植时按 `resource.ts`/`plugin.ts`/`worker.ts`/`descriptor.rs` 四层一起补 |
| 11 | 宿主能力与进程隔离 | `registry.rs` 兼办进程管理与 SDK 能力 | `worker.rs` 只管进程与 JSONL，能力在 `host_calls.rs`；live 目录缓存在 `catalog_cache.rs` | 进程传输不应决定插件权限：越权检查集中在 `host_calls.rs`，改权限只改一处；`worker.rs` 从 1760 行降到 ~1240 行，缓存单测不再需要起 Bun |
| 12 | catalog 与表单字段类型 | `catalog` / `methodType` / `fieldType` 是自由字符串，校验时手写 `matches!` | `CatalogMode`、`AddMethodKind`、`FormFieldKind` 枚举，serde 按 `snake_case` 解析，未知值在反序列化时就失败 | 插件声明里只有这三种取值；枚举让“不在列表里”变成解析错误，不用在每个使用点重复校验。边界仍输出字符串，插件协议不变 |

## 必须保持的不变量

1. 凭证只出现在 Rust 私有资源存储和对应 worker 调用里；`PluginResourceView` / 日志 / 测试快照不含 API key。
2. Magpie URL 只保留 origin；拒绝凭证、非 http(s)。
3. live catalog 按 `(plugin, provider, resource)` 缓存，TTL 15s；资源变更使缓存失效。缓存实现在 `catalog_cache.rs`，`host.rs` 只决定何时读、何时强制刷新。
4. 稳定模型 ID：`plugin:<plugin>/<provider>/<resource>/<upstream>`；发给 Magpie 的是裸上游 ID。
5. 目录刷新失败时保留旧缓存；无缓存则向 UI 报告错误，不抛给 Cursor。
6. 测试使用临时目录，不读真实 `~/.cursor-byok-next` 或真实 Magpie。
7. 不改 Cursor 代理白名单，不写 Cursor 文件。
8. origin 授权只覆盖当前 worker 请求；结果、取消、超时、停止和进程退出都会回收授权。网关失败只显示宿主观察到的 HTTP 状态（如有），不透传任意 worker 错误或响应体。

## 验证

需有 Bun（实测 1.3.14）。`magpie_catalog_through_real_bun` 使用临时数据目录、实际 collector/worker 和两个本地假网关，覆盖添加、去重与媒体过滤、稳定 ID、HTTP 503 回退与脱敏、按资源失效、移除及损坏存储。NSH 交互测试另覆盖插件加载失败重试和存储异常。

```sh
cargo test -p nsh-app-cursor-byok-next
cd apps/desktop && nsh check && nsh test
nsh test test-fixtures/Visual.test.tsx --target macos
```
