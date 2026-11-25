# Cursor-BYOK-Next

把 Cursor 接到你自己的模型 API：接管层用 Rust 写，界面是 NSH（Native Shell）桌面应用。

> Beta。配置格式、数据目录与本地 CA 在版本之间不保证稳定，见[升级](#升级)。

<p align="center">
  <img src="apps/desktop/assets/icon.svg" width="128" alt="Cursor-BYOK-Next">
</p>

[English](./README.md) · 中文

## 做什么

Cursor-BYOK-Next 让 Cursor 不使用内置模型，而是走你配置的模型 API：

```text
Cursor agent-host
      │  经本地代理的 *.cursor.sh
      ▼
harness（Rust）           本地 CA、只 MITM *.cursor.sh、托管 settings.json
      │
      ▼
backend（Rust）           InferenceService/RunInferenceSSE + BidiService/BidiAppend
      │
      ▼
Magpie 插件（QuickJS）    按网关选择模型并转发响应
      │
      ▼
你的 OpenAI / Anthropic 兼容 API
```

| 模块 | 位置 | 拥有什么 |
|---|---|---|
| 接管 | `apps/desktop/src/harness/` | 代理生命周期、本地 CA、`settings.json`、本地账号缓存 |
| local loop | `apps/desktop/src/loop_service/`、`backend/` | Cursor 推理流、模型目录、server config |
| 插件宿主 | `apps/desktop/src/plugins/` | 独立 QuickJS VM、Magpie 目录、Chat Completions / Responses / Anthropic 适配器 |
| 调用记录 | `apps/desktop/src/calls/` | 每次 Magpie `InvokeModel` 的 SQLite 记录 |
| 界面 | `apps/desktop/App.tsx`、`src/screens/`、`src/ui/` | 镜像 Rust 状态的 TSX 屏幕 |
| 调试工具 | `crates/cursor-capture/`、`crates/cursor-protocol-extractor/` | 抓包调试器与 proto 提取器 |

Rust 是代理、CA、Cursor 设置与数据库的唯一写者；TSX 只持有镜像并调用方法。

## 环境要求

- macOS 14+、Windows 10 1809+，或 GTK 4.14+ 与 glibc 2.39 的 Linux（Ubuntu 24.04 及以后）
- 从源码构建需要 Rust 1.97+ 与 `nsh` CLI 0.6.0

内置 Magpie 插件跑在编进应用的进程内 QuickJS VM 里，不需要额外的 JS 运行时。

## 安装

发布包由本仓库的 Releases 提供。macOS 包是 ad hoc 签名、未公证，首次启动 Gatekeeper 会要求确认。

## 构建

```sh
curl -fsSL https://nsh.dev/install.sh | sh    # NSH CLI；Windows：irm https://nsh.dev/install.ps1 | iex

cargo check --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all

cd apps/desktop
nsh check        # crate + TSX 类型检查
nsh test         # crate 单测 + *.test.tsx
nsh run          # 真实窗口
nsh package --target macos
```

## 使用

1. 打开 **Cursor** 页，初始化本地 CA。
2. 从弹窗复制安装命令，在终端执行并信任证书。应用只复制命令，不执行、不申请密码。
3. 打开接管开关。这会向 Cursor 的 `settings.json` 写入一条托管代理；Cursor 正在运行的话需要重启。
4. 在 **Plugins** 页填 Magpie 网关地址与 API Key，刷新模型目录。
5. 在 Cursor 的模型选择器里选这些模型。

关闭接管时先清 `settings.json` 再停代理，Cursor 不会指向已关闭的端口；清理只删本应用写过的那一条。

## 数据目录

```text
~/.cursor-byok-next/
├── harness.json            接管意图、首选端口、上次实际端口
├── ca/                     本地 CA 证书与私钥
├── calls.db                调用记录与详细日志开关
└── plugins/                Magpie 网关与目录缓存
```

CA 私钥不出本机。API Key 只存在插件数据目录；调用记录不保存鉴权头。

## 升级

本仓库仍是 beta，破坏性改动手改到底，不留兼容层。改名到 Cursor-BYOK-Next 时数据目录换了，
所以升级等于重新开始：重新初始化并信任 CA，接管开关回到关闭。系统里旧的
`Cursor BYOK Local CA` 可以手动删除。

## 文档

- `docs/protocol/`：Cursor 协议字段的含义与证据
- `apps/desktop/docs/`：每一步的移植记录（目录映射、与上游的每一处刻意差异及原因、必须保持的不变量）

## 致谢

Cursor-BYOK-Next 移植 [cursor-byok](https://github.com/hainuoli/cursor-byok) 的本地接管，后者基于
[leookun/cursor-byok](https://github.com/leookun/cursor-byok)。Magpie 插件作者是
[@leookun](https://github.com/leookun)。

以 [MIT License](./LICENSE) 发布；上游版权与许可声明保留在 [Third-Party Licenses](./THIRD-PARTY-LICENSES.md)。

## 安全

请私下报告漏洞，见 [SECURITY.md](./SECURITY.md)。