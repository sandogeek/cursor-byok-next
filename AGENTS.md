# AGENTS.md

当前目标：
- 逐步移植 cursor-byok 到 nsh 版本，产物名为 **Cursor-BYOK-Next**
- 不做 engine 部分，只做 agent local loop
- 当前仍是 beta：不考虑向后兼容。破坏性改动直接改，不写兼容层、旧路径回退、数据迁移或双份默认值；旧的配置/CA/数据库需要重建就重建。

## 仓库结构

```text
cursor-byok-nsh/
├── Cargo.toml                     # workspace：apps/desktop + crates/*
├── AGENTS.md                      # 本文件：移植边界与约定
├── .agents/skills/                # nsh / nsh-design-guides 技能（只读参考）
├── docs/
│   └── protocol/                  # 协议分析：README / agent-v1 / aiserver-v1 / cursor-inference-service / cursor-agent-loop-routing
├── protocols/cursor/              # 提取器产物：agent_v1.proto、aiserver_v1.proto
├── crates/
│   ├── cursor-protocol-extractor/ # 从 Cursor JS bundle 提取 agent.v1 / aiserver.v1
│   └── cursor-capture/            # Cursor API 抓包调试器（Rust 逻辑 + 原版 web）
└── apps/desktop/                  # nsh mixed 应用：App.tsx + 同目录 Rust crate
    ├── nsh.json                   # "rust": "."，无 "root" ⇒ App.tsx 是入口
    ├── App.tsx                    # 入口：先 restoreLocale() 再 mount
    ├── App.test.tsx               # nsh test
    ├── Cargo.toml                 # crate nsh-app-cursor-byok-next
    ├── locales/                   # zh-CN.yml / en-US.yml（nsh/i18n 的文案源）
    ├── docs/
    │   ├── CURSOR_TAKEOVER.md     # 接管（local_app）移植记录
    │   ├── PROTOCOL_EXTRACTOR.md  # 协议提取器移植记录
    │   ├── CURSOR_CAPTURE.md      # 协议调试器移植记录
    │   ├── CURSOR_UI.md           # 接管页 UI
    │   ├── I18N.md                # 界面语言（nsh/i18n）
    │   ├── PLUGIN_CATALOG.md      # Magpie 插件目录
    │   ├── PLUGIN_RUNTIME_VM.md   # 独立 QuickJS VM、oxc 编译、native 能力与验收
    │   ├── PLUGIN_PROTOCOL_ADAPTERS.md # 协议适配器下沉 Rust
    │   ├── LOCAL_LOOP.md          # local loop 服务端接入
    │   ├── MODEL_CALLS.md         # Magpie 调用记录
    │   └── SYSTEM_SETTINGS.md     # 系统设置：端口 / 调用记录 / 外观 / 语言 / 数据目录
    └── src/
        ├── lib.rs                 # #[native_module("cursor-byok-next/native")]：Harness + Plugins + Calls
        ├── harness/               # 上游 server/src/local_app 的移植
        ├── plugins/               # 插件宿主、QuickJS VM、Magpie 静态模块、协议适配器
        │   ├── vm/               # 每插件独立线程/堆、表内 loader、native Promise、timers
        │   └── sdk/host.ts       # cursor-byok-next:host 六能力桥（仅插件 VM）
        ├── calls/                 # 调用记录 SQLite 与生命周期
        ├── screens/               # Cursor / Plugins / Calls / CallDetails / SystemSettings / Language
        └── ui/                    # 样式控件（Button / Field / NumberField / Select / ChoiceGroup / SettingRow / Surface …）
```

上游参照：`../cursor-byok/server/src/local_app/`（同一工作区里的 `jsProject/cursor-byok`）。
每一步移植都要在 `apps/desktop/docs/` 留下该步的对照文档：目录映射、逐行差异及原因、必须保持的不变量。
协议字段的用途分析（谁写、谁读、取值、证据）统一放 `docs/protocol/`，不写进移植记录。
移植记录不写 Motion inventory（状态变化 / 工具与令牌 / 减少动态效果 表格）；动效只在代码与设计清单里体现，不再单独成文。

## 分工

| 层 | 语言 | 放什么 |
|---|---|---|
| 协议、文件、套接字、数据库、代理 | Rust（`src/harness/` 及后续模块） | 需要精确控制或已在 Rust 实现的能力 |
| 屏幕、表单、列表、状态、文案 | TSX（`App.tsx` / 后续 `src/screens/`、`src/ui/`） | 变化频繁、纯展示与交互 |
| 边界 | `src/lib.rs` 的 `#[native_module]` | `#[class]` / `#[module]` / `#[component]`，TS 类型由此生成 |

规则：
- 一份状态只有一个所有者。Rust 拥有代理、CA、Cursor 文件的真实状态；TSX 只持有镜像 signal，写入一律调方法。
- TSX 读取 Rust 的 getter 不会随变化重绘，必须在 signal 里镜像。
- 不要为了“整齐”新建平行抽象；优先改现有文件。

## 移植不变量（沿用上游，不得违反）

1. **启用前 CA 必须 `Ready`**，否则不写任何 Cursor 文件。
2. **停用顺序：先清 `settings.json`，再停代理。** 反过来会让 Cursor 打到已关闭的端口。
3. **清理残留只认“托管签名 + loopback”**，绝不删用户或企业配置的代理。
4. **真实登录不覆盖。** 只有“本来就是本应用签发的 token”才允许修复会员缓存。
5. **设置文件按 JSONC 读、原子写（temp + rename）**，因为 Cursor 可能正开着。
6. **只 MITM `*.cursor.sh`**，其他主机纯隧道；命中白名单才改写到本地。
7. **状态只读接口不返回错误。** 自愈失败记日志，让 UI 显示 `degraded`，不要抛错。
8. **路径与端口发生变化时必须持久化**，否则 Cursor 的 `http.proxy` 会指向死端口。
9. 不提交任何真实密钥；测试一律指向临时目录，不读 `~/Library/Application Support/Cursor`。
10. **用户可见文案只放 `locales/`**，界面与 Rust 都走 `t()` / `t!`；插件自带的 locale map 由 `localized()` 运行时解析，不在 Rust 边界提前压平。

## 与上游不同时的处理

允许差异，但必须同时满足：

- 差异写在 `apps/desktop/docs/<步骤>.md` 的差异表里，说明**为什么**，而不只是“不一样”；
- 差异不能削弱上面的不变量；
- 上游有测试覆盖的行为，本仓库要有对应测试。

## 命名（Cursor-BYOK-Next）

| 位置 | 值 |
|---|---|
| 应用名 / 窗口标题 | `Cursor-BYOK-Next`（`nsh.json` `name`） |
| Bundle id | `dev.cursorbyoknext.app`（`nsh.json` `id`） |
| Crate | `nsh-app-cursor-byok-next` |
| TSX 导入 | `cursor-byok-next/native` |
| 应用数据目录 | `~/.cursor-byok-next`（`harness.json`、`ca/`、`plugins/`） |
| CA 主题 | `Cursor-BYOK-Next Local CA` / `Cursor-BYOK-Next` |
| Linux 锚文件 | `cursor-byok-next-local-ca.crt` |
| Magpie 插件 id | `dev.cursorbyoknext.plugins.magpie` |
| 插件 SDK specifier | `cursor-byok-next:plugin` / `:provider` / `:model` / `:resource` / `:host` |
| descriptor 前缀 | `CURSOR_BYOK_NEXT_PLUGIN_DEFINITION:` |

上游 `cursor-byok` 的名字只作为移植来源出现在对照表里，不作为本应用的标识。

改名带来的唯一运行时后果：数据目录从 `~/.cursor-byok-nsh` 变成 `~/.cursor-byok-next`，旧 CA 与 `harness.json` 不再被读取。升级后需要在接管页重新初始化并信任 CA（系统里旧的 `Cursor BYOK Local CA` 可手动删除），接管开关回到“关”。

## 常用命令

```sh
# 仓库根
cargo check --workspace --all-targets
cargo test --workspace
cargo test -p cursor-protocol-extractor
cargo test -p cursor-capture
cargo clippy --workspace --all-targets --all-features # BeautifulRust lint 表，见下方
cargo fmt --all

# 桌面 mixed 应用
cd apps/desktop
nsh check        # crate + TSX 类型检查
nsh test         # crate 单测 + *.test.tsx
nsh run          # 看真实界面
nsh package --target macos
```

改了 Rust 导出签名后 `nsh check` 会重写 `.nsh/types/cursor-byok-next/native.d.ts`，该文件不要手改。

## Lint 配置

仓库根 `Cargo.toml` 的 `[workspace.lints]` 与 `clippy.toml` 是 BeautifulRust 的 lint 表，每个成员用 `[lints] workspace = true` 继承。

- 目标形态已经达成：`cargo clippy --workspace --all-targets --all-features -- -D warnings` 全绿，改动后必须保持全绿。
- 关掉一条要写原因：`#[expect(clippy::unwrap_used, reason = "…")]`；`#[allow]` 会被 `allow_attributes` 拦下。`#[expect]` 只在真正还会触发时写，多余的 expectation 本身也是一条告警。
- 生成代码（prost 输出）在 `src/protocol/proto.rs` 的 `include!` 外层统一豁免，不在 crate 根放宽。
- 三个成员都是应用（桌面 app + 两个 CLI），所以 BeautifulRust 标注 `[lib]` 的 lint（`missing_docs`、`missing_errors_doc`、`missing_panics_doc`、`unreachable_pub`、`exhaustive_enums`）已按表内说明去掉；哪天真出库 crate 再加回来。
- CLI 入口用 `#![expect(clippy::print_stdout, clippy::print_stderr, reason = …)]` 显式豁免打印。
- `cargo fmt --all` 之后再提交；本仓库没有自定义 `rustfmt.toml`，用默认风格。

## 当前进度

- [x] 让 Cursor 走本地：`harness/`（见 `apps/desktop/docs/CURSOR_TAKEOVER.md`）
- [x] 协议提取器：`crates/cursor-protocol-extractor/`（见 `apps/desktop/docs/PROTOCOL_EXTRACTOR.md`）
- [x] 协议调试器：`crates/cursor-capture/`（见 `apps/desktop/docs/CURSOR_CAPTURE.md`）
- [x] 接管页 UI（`src/ui/` + `src/screens/Cursor.tsx`）
- [x] Magpie 插件目录（`src/plugins/` + `src/screens/Plugins.tsx`，见 `apps/desktop/docs/PLUGIN_CATALOG.md`）
- [x] 界面语言（`locales/` + `nsh/i18n`，见 `apps/desktop/docs/I18N.md`）
- [ ] 协议字段分析（`docs/protocol/`）
- [x] local loop 服务端接入（`set_backend_addr`）：只做 `InferenceService/RunInferenceSSE` + `BidiService/BidiAppend`；`AgentService/Run*` 属 backend loop，不接管（见 `docs/protocol/cursor-agent-loop-routing.md`、`apps/desktop/docs/LOCAL_LOOP.md`）
- [x] 把 Magpie 模型广告给 Cursor picker
- [x] 模型调用 / 调用记录（见 `apps/desktop/docs/MODEL_CALLS.md`）
- [x] 插件协议适配器下沉 Rust（见 `apps/desktop/docs/PLUGIN_PROTOCOL_ADAPTERS.md`）
- [x] 系统设置页（见 `apps/desktop/docs/SYSTEM_SETTINGS.md`）
- [x] GitHub 配置与发布线：`.github/`（CI / 发布 / 脚本 / 模板 / dependabot）+ 应用内更新检查（见 `apps/desktop/docs/GITHUB_CI.md`）
- [x] 内嵌插件进程内 QuickJS VM：`plugins/vm/`（loader/globals/timers/native/collector/runtime），构建期 oxc 去 TS，无外部运行时、子进程或临时模块目录（见 `apps/desktop/docs/PLUGIN_RUNTIME_VM.md`）。
