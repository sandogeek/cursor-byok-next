# 移植记录：插件模型目录（Magpie）

上游实现：`cursor-byok/server/src/plugin/` + `server/plugins/build-in/magpie/`。
本移植：`apps/desktop/src/plugins/` + `apps/desktop/plugins/build-in/magpie/` + `src/screens/Plugins.tsx`。

范围只有一件事：**让 Magpie 网关的实时模型目录出现在本应用的插件配置页**。不接入 Cursor `AvailableModels` / `GetUsableModels`，不实现 `provider.invoke` 调用，不在核心 UI 内置通用模型增删改。

## 目录映射

```text
cursor-byok/server/src/plugin/              apps/desktop/src/plugins/
├── registry.rs     插件能力编排              ├── mod.rs            PluginHost 生命周期
├── catalog.rs      扫描 plugin.json          ├── catalog.rs        扫描内置 / 安装目录
├── definition.rs   Deno 收集 descriptor      ├── definition.rs    Bun 收集 descriptor
├── worker.rs       Deno JSONL IPC            ├── worker.rs        Bun JSONL IPC
├── state.rs        资源与 snapshot 目录      ├── state.rs         资源与 live 目录缓存
├── data.rs         原子 JSON 文件            ├── data.rs          原子 JSON 文件
├── origin.rs       origin 校验               ├── origin.rs        origin 校验
├── descriptor.rs   稳定 ID / 投影            ├── descriptor.rs    稳定 ID / 投影
├── manifest.rs     plugin.json               ├── manifest.rs      plugin.json
├── protocol.rs     JSONL 消息                ├── protocol.rs      JSONL 消息
├── builtin.rs      预装内置插件              ├── builtin.rs       预装 Magpie
└── sdk/            Deno import map           └── sdk/             Bun import map
    ├── plugin.ts / model.ts / resource.ts
    ├── collect.ts / worker.ts
    └── protocol/*.ts                         （首期不调用，仅保持 SDK 完整）

cursor-byok/server/plugins/build-in/magpie/ apps/desktop/plugins/build-in/magpie/
├── plugin.json / main.ts / catalog.ts / resources.ts / provider.ts
└── magpie_test.ts                            解析与表单语义由 Rust 单测覆盖
```

行数为 `wc -l` 实测，含测试：Rust 宿主约 3400 行，Magpie 插件源约 310 行，SDK/worker 约 740 行。nsh 侧新增 `src/screens/Plugins.tsx`：NSH mixed 应用当前没有 `.nshpkg` 加载 API，插件配置页先做在宿主 TSX 里；`PluginHost` 的 native 导出仍是后续签名包可复用的边界。

## 本步边界

- Rust 拥有资源凭证、live 目录缓存、worker 生命周期；TSX 只镜像 `PluginHost.snapshot()`。
- Magpie `catalog: "live"`：目录不落盘，按资源短缓存 `models.list`。
- 连接表单属于插件自有资源配置，不是核心模型 CRUD。
- Cursor picker / local loop 不改；已广告给 Cursor 的模型必须可路由，本步因此不广告 Magpie 模型。

## 差异清单

| # | 主题 | 上游 | nsh 版 | 原因 |
|---|---|---|---|---|
| 1 | 运行时 | Deno 子进程 | PATH 上的 `bun` | 用户指定 Bun；首期不捆绑运行时二进制 |
| 2 | UI 交付 | React 插件页 | 宿主 TSX 插件页，native API 预留给 `.nshpkg` | NSH 签名包是 TSX-only，当前没有 headless package runner |
| 3 | 能力范围 | `models.list` + `provider.invoke` + OAuth/import | 只实现 form 添加、present、remove、`models.list` | 本期不调用模型 |
| 4 | 模型广告 | 追加到 Cursor AvailableModels | 只在插件页展示 | 避免 Cursor 选到不可调用的模型 |
| 5 | 运行时安装 | 下载/校验 Deno | 检测 PATH 上的 bun | 不引入运行时分发 |
| 6 | SDK 协议 helper | Deno import map 含 openai/anthropic | 文件保留，首期 worker 不调用 | 后续 invoke 阶段再接线 |

## 必须保持的不变量

1. 凭证只出现在 Rust 私有资源存储和对应 worker 调用里；`PluginResourceView` / 日志 / 测试快照不含 API key。
2. Magpie URL 只保留 origin；拒绝凭证、非 http(s)。
3. live catalog 按 `(plugin, provider, resource)` 缓存，TTL 15s；资源变更使缓存失效。
4. 稳定模型 ID：`plugin:<plugin>/<provider>/<resource>/<upstream>`；发给 Magpie 的是裸上游 ID。
5. 目录刷新失败时保留旧缓存；无缓存则向 UI 报告错误，不抛给 Cursor。
6. 测试使用临时目录，不读真实 `~/.cursor-byok-nsh` 或真实 Magpie。
7. 不改 Cursor 代理白名单，不写 Cursor 文件。

## Motion inventory

| 状态变化 | 工具与令牌 | 减少动态效果 |
|---|---|---|
| 按钮 / 导航 hover | `transition-colors`，`instant` 120ms | 立即切换 |
| 按钮按下 | `transition`，`fast` 180ms，缩放 0.97 | 不缩放 |
| 插件页内容到达 | `createMotion` 淡入上移 8pt，`base` 240ms | 直接显示最终帧 |
| 移除确认弹窗 | 原生 Dialog fade | 平台减少动态效果优先 |
| 目录刷新 | 状态文案原地位换，按钮显示忙碌 | 无循环动画 |

## 验证

```sh
cargo test -p nsh-app-cursor-byok
cd apps/desktop && nsh check && nsh test
```
