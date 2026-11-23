# 移植记录：GitHub 配置与发布线

上游实现：`cursor-byok/.github/`（`workflows/ci.yml`、`workflows/release.yml`、`scripts/*.mjs`、`dependabot.yml`、`CODEOWNERS`、`ISSUE_TEMPLATE/`、`pull_request_template.md`、`release-notes/`）+ 上游仓库根的 `LICENSE`、`THIRD-PARTY-LICENSES.md`、`SECURITY.md`。
本移植：本仓库的 `.github/`、同三个根文件，以及 `apps/desktop/src/screens/updates.ts` 的应用内更新检查。

范围：**把上游的检查与发布流水线换成 nsh 的打包模型**。上游发 Tauri 应用（npm + tauri-action + updater-json），本仓库发 NSH 应用（`nsh check/test/build/package` + `nsh update-manifest`），因此 workflow 的每一步都要重写，脚本与模板只迁移规则。

## 目录映射

```text
cursor-byok/.github/                          cursor-byok-nsh/.github/
├── workflows/ci.yml            127  4 个 job   workflows/ci.yml       168  3 个 job（repository / rust / package）
├── workflows/release.yml       355  Tauri     workflows/release.yml  328  三平台 nsh package + 签名清单
├── scripts/check-release.mjs    55             scripts/check-release.mjs            77  版本/密钥/nsh 版本三方对齐
├── scripts/*.test.mjs           41             scripts/check-updater-manifest.test.mjs 168  manifest 校验规则
├── scripts/generate-portable-update.mjs   71   ─   Windows 便携包，nsh 没有对应概念
├── scripts/normalize-tauri-update.mjs    107   ─   Tauri latest.json 的 API URL 改写，nsh 自己写
├── dependabot.yml               75             dependabot.yml   77  github-actions + cargo 两组（无 npm）
├── CODEOWNERS                    1             CODEOWNERS        1  换 owner
├── ISSUE_TEMPLATE/*             30             ISSUE_TEMPLATE/* 28  文案改名 + 安全链接
├── pull_request_template.md      6             pull_request_template.md 11
└── release-notes/1.0.1、1.1.0      ─            release-notes/0.0.1.md 23  本仓库首个版本

新增（上游没有的对应物）：
  .github/actions/install-nsh/action.yml       68  按 runner 平台下载 nsh release + 校验 SHA-256
  .github/scripts/check-updater-manifest.mjs  132  清单 ↔ 归档 的一致性（上游由 Tauri 保证）
  .github/scripts/local-release-probe.sh       54  本机跑 finalize 的清单半步

应用侧（上游是 Tauri updater 命令 + `shared/store/updateStore.ts`）：
  apps/desktop/src/screens/updates.ts        83  更新状态镜像 + 检查/下载，`nsh/updater`
  apps/desktop/src/screens/updates.test.ts   47  四种状态的纯逻辑测试
  apps/desktop/src/screens/SystemSettings.tsx 291 → 367  新增“更新”卡片
```

行数为 `wc -l` 实测（`@` 号省略），含注释与空行。

根文件：

```text
cursor-byok/                    cursor-byok-nsh/
├── LICENSE            21 MIT     LICENSE             20 MIT（版权行改为 sandogeek）
├── THIRD-PARTY-LICENSES.md 29    THIRD-PARTY-LICENSES.md 33（保留 leehainuo + leookun 两条 MIT）
└── SECURITY.md        13         SECURITY.md         17（改为本仓库实际能力：CA、代理、密钥、签名清单）
```

## 主路径

```text
push / PR
└── ci.yml
    ├── repository（ubuntu，5 min → 10 min）
    │   ├── node .github/scripts/check-release.mjs
    │   │   ├── nsh.json version 是稳定 SemVer，build 是正整数
    │   │   ├── .github/release-notes/<version>.md 存在且首行是 "# Cursor-BYOK-Next v<version>"
    │   │   ├── nsh.json name / id 固定；updater.github / updater.publicKey 形状正确
    │   │   ├── apps/desktop/Cargo.toml 版本 == Cargo.lock 里 nsh-app-cursor-byok-next 的版本
    │   │   ├── install-nsh action 默认版本 == apps/desktop/Cargo.toml 的 nsh-core 版本
    │   │   │                              == ci.yml / release.yml 的 NSH_VERSION
    │   │   └── protocols/cursor/{agent_v1,aiserver_v1}.proto 非空
    │   └── node --test .github/scripts/*.test.mjs
    ├── rust（ubuntu-24.04 / macos-15 / windows-latest，45 min）
    │   ├── cargo fmt --all -- --check
    │   ├── cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
    │   ├── cargo test --locked --workspace --all-targets        （仅 Linux）
    │   └── cd apps/desktop && nsh check / nsh build / nsh test  （test 仅 Linux）
    └── package（macos-15 / ubuntu-24.04 / windows-latest，60 min）
        └── nsh package --target <os>  →  upload-artifact
```

```text
tag v<version> / workflow_dispatch(tag)
└── release.yml
    ├── prepare（ubuntu）
    │   ├── 只允许 repository owner 触发
    │   ├── NSH_UPDATE_KEY 存在且是 44 字符 base64（nsh keygen .secret 的形状）
    │   ├── check-release.mjs --tag v<version>
    │   ├── tag 指向的 commit 在 origin/main 上
    │   └── 该 tag 尚未 published（draft 允许复用）
    ├── checks → ci.yml（workflow_call，ref = tag 的 commit）
    ├── publish（3 个 OS，各 90 min）
    │   ├── linux/macos: 装 GTK4 + WebKitGTK；macos 额外装 x86_64-apple-darwin
    │   ├── nsh package --target <os>   （nsh.json 有 updater ⇒ 同时写 <name>-<version>-<platform>.<ext>）
    │   └── upload-artifact release-<os>
    └── finalize（ubuntu，15 min）
        ├── 下载全部 artifact → dist/
        ├── gh release create --draft（已存在 draft 则复用）
        ├── 按 RELEASE_PLATFORMS 逐个上传归档（每个平台必须恰好 1 个文件）
        ├── nsh update-manifest dist/*.tar.gz dist/*.zip --notes ... --key ... -o dist
        ├── node check-updater-manifest.mjs（id、version、每平台 url/size/sha256，且不多不少）
        ├── 先传 latest.json.sig，再传 latest.json
        ├── 写下载表（五个平台）→ gh release edit --notes-file
        └── gh release edit --draft=false --latest
```

## 逐项差异与原因

| 文件 / 步骤 | 上游 | 本仓库 | 原因 |
|---|---|---|---|
| `ci.yml` 的 frontend job | `npm ci` + `npm audit` + `npm run check` | 删除，改为 `nsh check` / `nsh build` | 本仓库没有 `package.json`：TSX 的编译器由 nsh 提供，`nsh check` 是唯一的类型检查（`nsh build` 才能看到 View 文本、capabilities 拼写、缺资源）。 |
| `ci.yml` 的 rust job | Linux 只跑 `cargo clippy/test` | 三平台 clippy + Linux 测试，并加 `nsh check/build/test` | 桌面 crate 有 `cfg(windows)` / `cfg(unix)` 分支，只在自身平台编译；`nsh test` 同时跑 crate 的 `cargo test` 与 `*.test.tsx`。 |
| `ci.yml` 新增 package job | 无 | 三平台 `nsh package` 各一次 | 上游由 release 一次性验证；这里 PR 阶段就证明 `nsh.json` + 入口仍能打包，失败在合并前暴露。 |
| `ci.yml` 的 release 依赖 | release 调 `ci.yml` | 同 | 保留：发布必须跑同一套检查。 |
| nsh 版本 | Tauri 版本由 `Cargo.toml` 决定 | `NSH_VERSION` 一处 + `install-nsh` action | nsh 会按 CLI 版本重写 `.nsh/rust` 的 `nsh-*` 依赖；CLI 与 Cargo 不一致时行为在同一 runner 上静默改变，所以 `check-release.mjs` 三方对齐。 |
| `check-release.mjs` | 校验 `package.json` / `package-lock.json` / `tauri.conf.json` / `src-tauri/Cargo.toml` / updater endpoints / `bundle.resources` | 校验 `nsh.json`（version/build/name/id/updater）+ crate 版本 + nsh 版本对齐 + proto 存在 + release-notes | Tauri 的版本载体与 updater 地址在本仓库都不存在；版本唯一来源变成 `nsh.json`，release-notes 由 workflow 直接读文件。 |
| `generate-portable-update.mjs` | 为 Windows 便携包签名并写 `portable-latest.json` | 删除 | NSH 的 updater 用一份 `latest.json` 覆盖所有平台，`nsh update-manifest` 自己算 sha256/size；没有“便携包另一条清单”这个第二来源。 |
| `normalize-tauri-update.mjs` | 把 Tauri 写的 API asset URL 改写成公开下载 URL | 删除 | nsh 写清单时直接使用归档文件名（相对 `latest.json`），不存在需要改写的 API URL。 |
| 新增 `check-updater-manifest.mjs` | 由 Tauri 保证清单与产物一致 | 逐平台比对 url / size / sha256，且拒绝多出来的平台 | `nsh update-manifest` 只读它拿到的文件；workflow 若漏上传或多上传，必须在发布前失败，而不是让客户端下载到错误产物。 |
| `release.yml` 的平台矩阵 | 4 项（linux-x86_64 / windows-x86_64 / macos-aarch64 / macos-x86_64）× 各自 runner | 3 项（macos / linux / windows） | `nsh package --target macos` 一次产出两种 Mac 架构的归档，`linux` 同理；可打包平台只有自身 OS。 |
| `RELEASE_PLATFORMS` | 分散在矩阵 `platform` 字段 | 一处 env | 归档、清单校验、下载表共用一份列表，避免三处漂移。 |
| macOS DMG 单独重命名上传 | 有 | 无 | `nsh package --target macos` 产出 `.app` 与 updater 归档；没有 DMG，也没有第二套命名。 |
| 更新签名 | `TAURI_SIGNING_PRIVATE_KEY`（minisign） | `NSH_UPDATE_KEY`（Ed25519，`nsh keygen`） | nsh updater 的密钥模型；公钥写进 `nsh.json` 的 `updater.publicKey`。 |
| 上传顺序 | tauri-action 一次性上传 | 先归档，再 `latest.json.sig`，最后 `latest.json` | 客户端读 `releases/latest/download/latest.json`；清单先到、归档未到会让客户端校验失败。 |
| dependabot | github-actions + npm(apps/desktop) + cargo | github-actions + cargo | 没有 npm 工程；cargo 新增 `nsh` 分组（`nsh` / `nsh-*`），因为 CLI 版本必须跟着一起动。 |
| release-notes | 每个版本一份，标题 `# Cursor BYOK v<version>` | 同，标题 `# Cursor-BYOK-Next v<version>` | `check-release.mjs` 用标题对齐 `nsh.json` 版本。 |
| 应用内更新 | 前端 `updateStore.ts` + Tauri updater 命令 | `src/screens/updates.ts` + `nsh/updater` + 系统设置“更新”卡片 | Rust 侧拥有更新状态（`nsh/updater` 一次只跑一个检查/下载），TSX 只镜像 signal；字段名沿用 nsh 的 `AppUpdate`。 |

## 必须保持的不变量

上游的检查与发布规则里，本仓库仍然成立的：

1. **版本只有一个来源。** `nsh.json` 的 `version` 决定归档名、清单、release tag 和 release-notes 文件名；`check-release.mjs` 在任何不一致时失败。
2. **发布只由 repository owner 触发，且 tag 必须已在 `main` 的历史里。**
3. **同一版本不重复发布。** 已 published 则失败；draft 允许复用。
4. **发布前跑完整 CI。** `checks` job 复用 `ci.yml`，ref 固定为 tag 的 commit。
5. **清单与产物必须一致。** `check-updater-manifest.mjs` 拒绝缺平台、多平台、url 改名、size / sha256 不符。
6. **签名公钥只出现在仓库里，私钥只在 secret 里。** `nsh.json` 的 `updater.publicKey`；`NSH_UPDATE_KEY` 只进 `$RUNNER_TEMP`，用完即删，不落仓库、不进日志。
7. **归档在清单之前上传。** 顺序见 `finalize`。
8. **draft → 补全 → 发布。** 只有全部资产到位才 `--draft=false --latest`。

## 与上游不同、且本仓库特有的约束

- **`NSH_UPDATE_KEY` 需要用户自己配。** 本机密钥在 `~/.nsh-keys/cursor-byok-nsh/update.{pub,secret}`；`.pub` 已写进 `apps/desktop/nsh.json`，`.secret` 通过 `gh secret set NSH_UPDATE_KEY < ~/.nsh-keys/cursor-byok-nsh/update.secret` 配到仓库。丢失 `.secret` 只能换钥并重新发布一个应用构建（公钥写在已安装的应用里）。
- **macOS 归档未公证。** `nsh package --target macos` 是 ad hoc 签名；Gatekeeper 可能要求用户确认首次启动，`SECURITY.md` 里写明。
- **Linux 与 Windows 的归档在同一 workflow 里跨 runner 汇总。** 因此 `finalize` 必须在 ubuntu 上跑 `nsh`（CLI 的 update-manifest 是跨平台的），不能用发布机本地的 CLI 版本。

## 本机验证（不依赖 GitHub）

```sh
node .github/scripts/check-release.mjs
node --test .github/scripts/*.test.mjs

# 清单半步：用 nsh package 的产物跑 finalize 的核心
nsh package --target macos -o "$TMPDIR/out"     # 本机会写 macos-aarch64 与 macos-x86_64 两个归档
# 其余平台的归档可以先用占位文件，只验证清单/校验逻辑
.github/scripts/local-release-probe.sh ~/.nsh-keys/cursor-byok-nsh/update.secret "$TMPDIR/out"
```

`nsh package` 与 `nsh update-manifest` 的结果都已在本机跑通：归档名是 `Cursor-BYOK-Next-<version>-<platform>.<ext>`，清单 `platforms` 的 key 是 `nsh_updater::platform()` 的 `<os>-<arch>`，五个平台齐全。