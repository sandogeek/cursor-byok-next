# AGENTS.md

当前目标：
- 逐步移植 cursor-byok 到 nsh 版本
- 不做 engine 部分，只做 agent local loop

## 仓库结构

```text
cursor-byok-nsh/
├── Cargo.toml                     # workspace：apps/desktop + crates/cursor-protocol-extractor
├── AGENTS.md                      # 本文件：移植边界与约定
├── .agents/skills/                # nsh / nsh-design-guides 技能（只读参考）
├── crates/
│   └── cursor-protocol-extractor/ # 从 Cursor JS bundle 提取 agent.v1 / aiserver.v1
└── apps/desktop/                  # nsh mixed 应用：App.tsx + 同目录 Rust crate
    ├── nsh.json                   # "rust": "."，无 "root" ⇒ App.tsx 是入口
    ├── App.tsx                    # 入口（目前仍是模板）
    ├── App.test.tsx               # nsh test
    ├── Cargo.toml                 # crate nsh-app-cursor-byok
    ├── docs/
    │   ├── CURSOR_TAKEOVER.md     # 接管（local_app）移植记录
    │   └── PROTOCOL_EXTRACTOR.md  # 协议提取器移植记录
    └── src/
        ├── lib.rs                 # #[native_module("cursor-byok/native")]：目前导出 Badge/Format/Counter/Harness
        └── harness/               # 上游 server/src/local_app 的移植
            ├── mod.rs             # CursorHarness 生命周期 = enable/disable/status/initialize_ca/cleanup
            ├── proxy.rs           # hudsucker MITM + RcgenAuthority + 路由判定
            ├── ca.rs              # 本地 CA 生成/加载/信任检测
            ├── settings.rs        # Cursor settings.json 的 5 个托管键
            ├── account.rs         # state.vscdb 注入本地 ultra 账号
            ├── state.rs           # ~/.cursor-byok-nsh/harness.json：开关与端口
            ├── process.rs         # 结束 Cursor（可选加速器）
            └── windows.rs         # Win32 ROOT 枚举
```

上游参照：`../cursor-byok/server/src/local_app/`（同一工作区里的 `jsProject/cursor-byok`）。
每一步移植都要在 `apps/desktop/docs/` 留下该步的对照文档：目录映射、逐行差异及原因、必须保持的不变量。

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

## 与上游不同时的处理

允许差异，但必须同时满足：

- 差异写在 `apps/desktop/docs/<步骤>.md` 的差异表里，说明**为什么**，而不只是“不一样”；
- 差异不能削弱上面的不变量；
- 上游有测试覆盖的行为，本仓库要有对应测试。

## 常用命令

```sh
# 仓库根
cargo check --workspace --all-targets
cargo test --workspace
cargo test -p cursor-protocol-extractor

# 桌面 mixed 应用
cd apps/desktop
nsh check        # crate + TSX 类型检查
nsh test         # crate 单测 + *.test.tsx
nsh run          # 看真实界面
nsh package --target macos
```

改了 Rust 导出签名后 `nsh check` 会重写 `.nsh/types/cursor-byok/native.d.ts`，该文件不要手改。

## 当前进度

- [x] 让 Cursor 走本地：`harness/`（见 `apps/desktop/docs/CURSOR_TAKEOVER.md`）
- [x] 协议提取器：`crates/cursor-protocol-extractor/`（见 `apps/desktop/docs/PROTOCOL_EXTRACTOR.md`）
- [ ] 接管页 UI（`src/ui/` + `src/screens/Cursor.tsx`）
- [ ] local loop 服务端接入（`set_backend_addr`）
- [ ] 模型配置 / 调用记录