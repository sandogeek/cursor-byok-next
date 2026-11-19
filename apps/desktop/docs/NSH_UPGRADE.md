# 工具链记录：nsh 0.5.0 → 0.6.0

范围只有一件事：把本仓库的 NSH 工具链、crate 依赖与随仓库分发的技能文档升级到 0.6.0。
不改动 `src/harness/`、`src/plugins/` 的任何运行时行为。

## 改动对照

| 位置 | 0.5.0 | 0.6.0 | 原因 |
|---|---|---|---|
| 本机 CLI | `brew` tap 版本 | `nsh 0.6.0 (5541637)` | tap 公式落后一个版本，`brew update` 后升级 |
| `apps/desktop/Cargo.toml` | `nsh-core` / `nsh-base` = `0.5.0` | 两者 = `0.6.0` | NSH 要求 `nsh-*` 与所运行的 CLI 同版本 |
| `apps/desktop/nsh.json` | 无 `capabilities` | `application` / `statusBar` / `clipboard` | 0.6.0 新增能力门：声明后即为白名单 |
| `apps/desktop/test-fixtures/nsh.json` | 只有 `host` / `statusBar` | 追加 `application` / `clipboard` | 测试桩同样跑 `Cursor.tsx` 的窗口与剪贴板调用 |
| `.agents/skills/nsh*` | v0.5.0 的副本 | v0.6.0 的副本 | 技能文档必须与 CLI 同版本，否则示例 API 可能过时 |

本地技能与上游 `skills/` 目录逐字节一致，取自 `longbridge/nsh` 的 `v0.6.0` tag，而不是 `main`：
`main` 已包含尚未发布的 API 说明，会对不上本机 CLI。

## 能力声明为什么是这三项

0.6.0 把 `capabilities` 从“建议”变成“白名单”：一旦在 `nsh.json` 写这个键，未列出的能力一律拒绝。
因此只声明应用真正用到的部分：

- `application` — `src/screens/Cursor.tsx` 的 `desktop={{ windowDrag: true }}`（主窗口拖动）以及主窗口/菜单；
- `clipboard` — `src/screens/Cursor.tsx` 的 `Clipboard` 写入；
- `statusBar` — 宿主按 `nsh.json` 初始化窗口状态栏外观，不声明会每次启动记一条 `warn js`。

不声明 `network` / `fs` / `process`：TSX 不直接 fetch、不读写文件；Bun 插件 worker 由 Rust 侧拉起，
不属于 NSH 的沙箱范围。以后若 TSX 要发请求或开链接，再按需补 `network` / `openUrl`。

`test-fixtures/nsh.json` 额外保留 `host: ["cursor.*", "plugins.*"]`，这是测试桩调用 `host.call` 的既有授权，
0.6.0 起 `host` 即使不写 `capabilities` 也必须显式声明。

## 验证

```sh
cargo check --workspace --all-targets
cargo test --workspace
cd apps/desktop && nsh check && nsh test && nsh build
```

结果：workspace 68 个 Rust 测试通过；`nsh check`、`nsh build` 通过；`nsh test` 53 通过 / 1 跳过（原样）。
升级前 `nsh test` 每次都会打印 `warn js the application … is not granted`，补齐声明后消失。

## 不变量

- 依赖版本以本机 `nsh --version` 为准，升级 CLI 后必须同步 `Cargo.toml` 与 `.agents/skills`。
- 能力声明只增不减地对应真实使用；新增 TSX 平台调用时，先补声明再改代码。
- 技能文档只从对应版本的 tag 取，不从 `main` 取。