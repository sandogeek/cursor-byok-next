# 移植记录：Cursor 接管（`local_app` → `harness`）

上游实现：`cursor-byok/server/src/local_app/`（1149 行，含 `ca/`，含单测）。
本移植：`apps/desktop/src/harness/`（1754 行，含单测）。

范围只有一件事：**让 Cursor 愿意走本地**——把它的网络栈指向本机代理、让它信任代理签发的证书、让它以为已登录。代理之后把请求改写给谁，是下一步（local loop 的服务端）的事。

## 目录映射

```text
cursor-byok/server/src/local_app/          apps/desktop/src/harness/
├── mod.rs             235 行  CursorHarness  ├── mod.rs              423 行
├── proxy.rs           223 行  代理与路由      ├── proxy.rs            311 行
├── settings.rs        111 行  settings.json  ├── settings.rs         222 行
├── account.rs         175 行  state.vscdb    ├── account.rs          255 行
├── process.rs          91 行  结束 Cursor    ├── process.rs           97 行
└── ca/
    ├── mod.rs         239 行  CA 生成与信任   ├── ca.rs               283 行（目录收成单文件）
    └── windows.rs      75 行  Win32 ROOT 枚举 └── windows.rs           70 行
                                               └── state.rs            93 行（新增：意图与端口持久化）
```

行数为 `wc -l` 实测，两侧都含 `#[cfg(test)]`。nsh 侧多出 39 行，主要来自 `state.rs` 与新增的 `ProxyRuntime` 生命周期测试。

上游划掉的 `pub(crate)` 辅助（`proxy_host_allowed`、`request_uses_local_cursor_token`、`local_cursor_authorization`）在本仓库没有消费方（`api/cursor/proxy.rs`、`cursor/services/{account,entitlement,compatibility}.rs` 还没移植），因此以公开导出保留：

```40:42:apps/desktop/src/harness/mod.rs
pub use self::account::{is_local_cursor_authorization, local_token};
pub use self::process::terminate_cursor as quit_cursor;
pub use self::proxy::{is_cursor_host, Routing as ProxyRouting, UPSTREAM_URL_HEADER};
```

`proxy.rs` 的 32 条本地路径白名单逐条对齐（`is_local_path`，脚本比对结果：两侧完全相同）。

## 差异清单

每一行都是**刻意**的，不是没抄完。

| # | 主题 | 上游 | nsh 版 | 原因 |
|---|---|---|---|---|
| 1 | 后端未就绪时 | 一律把命中白名单的请求改写到 `127.0.0.1:<service_port>` | `Routing.backend == None` 时**原样转发**给 Cursor | 本仓库还没有本地服务；此时改写到空端口会让 Cursor 全面报错。接管可以先开，路由随后接 |
| 2 | Tab / FileSync 路由 | `is_local_path(path) \|\| (is_tab_path(path) && tab_mode != Direct)`，含 `set_tab_settings` | 不在白名单，也没有 `TabMode` | 那是独立的 tab 中转功能（16 条 `TAB_PATHS`），不属于“让 Cursor 走本地”。加它等于同时承诺实现 tab 服务 |
| 3 | 端口记忆 | 存 SQLite `network_ports`，启动读 `proxy_port` | 存 `~/.cursor-byok-nsh/harness.json`，重启**复用上次实际绑定的端口** | 本仓库还没有数据库。端口变了 Cursor 的 `http.proxy` 就失效，所以实际端口必须回写并复用 |
| 4 | 端口被占 | 回退随机端口，警告 | 同样回退，但把实际端口写回 `harness.json` | 见 #3；上游重启会退回“请求的端口”，nsh 重启会退回“上次真的跑通的端口” |
| 5 | `status()` 的自愈 | `if takeover_enabled && ca ready && backend_addr.is_some() { self.enable().await? }` —— 失败即返回错误 | 拆成 `restore()` + `report()`：自愈条件不再要求 `backend`，失败只 `warn` | UI 会轮询 `status()`。错误从只读接口冒出去会把“状态页刷新”变成“应用错误态”。恢复失败应该表现为 `degraded`，不是异常 |
| 6 | 自愈触发条件 | 必须有 `backend_addr` 才自愈 | 有 CA + 有意愿就自愈 | 配合 #1：代理先跑起来接管流量，后端后接 |
| 7 | 状态结构 | `CursorHarnessStatus` 含 `configured_models` / `enabled_models` | `HarnessStatus` 去掉模型计数，加 `proxy_port` | 模型计数依赖 `Store`，本步没有。`proxy_port` 是端口回退后 UI 必须能看到的 |
| 8 | 启停入口 | HTTP `PUT /__byok-api__/api/harness/cursor/enabled` + `SetEnabled { enabled }` | `#[class] Harness` 的 `setEnabled(enabled)` / `status()` / `initializeCa()` | 这个仓库没有 axum 管理面；nsh 的类型边界是 `#[class]` |
| 9 | 回滚语义 | `start` 后写端口失败 → `stop()`；写设置失败 → `stop()` | 只有“写设置失败 → `stop()`” | 本仓库不写端口到数据库，`harness.json` 与设置写入放在同一次成功后提交（#3） |
| 10 | 锁 | 全程持有 `proxy` 锁；`apply_cursor_configuration` 是同步函数，不需要 await | `apply()` 是 `async`（账号注入要 await），因此写文件前先 `drop(proxy)`，失败再重新取锁回滚 | `tokio::sync::Mutex` 不能跨 await 持锁写文件；否则 UI 的 `status()` 会排队等整次接管 |
| 11 | 账号注入的串行化 | 每次 `enable` 直接写 | 同；额外显式 `.busy_timeout(5s)` | 与 sqlx 默认值一致（`sqlx-sqlite 0.8.6` 默认 5s），写出来是为了让“Cursor 正开着这个库”这件事在代码里可见 |
| 12 | 代理 URL 解析 | `reqwest::Url` | 自带 `loopback_host()`（约 10 行） | 只用来判断“是不是 loopback”，不值得为此引入 `reqwest`（它在本仓库会额外拉 native-tls） |
| 13 | `clear_stale_managed_settings` | `Result<()>` | `Result<bool>`（是否真的清了） | UI 需要区分“没有残留”与“清掉了”，否则每次启动都要弹一次提示 |
| 14 | `terminate_cursor` | `Result<()>` | `Result<bool>`（Cursor 是否在运行） | 调用方据此决定日志措辞；也让“没装 Cursor”不被当成失败 |
| 15 | 意图持久化 | SQLite 键 `cursor_takeover_enabled` | `harness.json` 的 `{ enabled, proxyPort }` | 同上，本仓库还没有数据库；文件损坏/缺失一律读成“关”，绝不凭读不懂的文件接管 |
| 16 | 数据目录 | `~/.cursor-byok-v3`（CA 在 `ca/`） | `~/.cursor-byok-nsh`（CA 在 `ca/`） | 两个客户端要能并存，证书也不能互相覆盖 |
| 17 | Linux 锚文件名 | `cursor-byok-local-ca.crt` | `cursor-byok-nsh-local-ca.crt` | 同上 |
| 18 | 桩数据 | 无 | 测试全部指向临时目录（`SettingsStore::at` / `AccountStore::at` / `CaManager::at` / `HarnessStateStore::at`） | 单测绝不读真实 `~/Library/Application Support/Cursor` |

### 顺带修掉的上游隐患

- **`status()` 是自愈入口，同时是轮询接口**：见 #5，nsh 把它拆开。
- **`enable()` 里的失败路径没有覆盖 `harness.json`**：上游“先记住意图再动作”，崩在中间会留下“想开但没开”的持久状态；nsh 只在成功后提交 `{ enabled: true, proxyPort: actual }`，失败时同时把内存意图复位。

## 必须保持的语义（两侧一致）

| 不变量 | 上游 | nsh 版 |
|---|---|---|
| 启用前 CA 必须 `Ready`，否则**不写任何 Cursor 文件** | `mod.rs:172-177` | `mod.rs:253-258`（有单测 `enabling_without_a_trusted_ca_fails_before_touching_cursor`） |
| 停用顺序：先清 `settings.json`，再停代理 | `mod.rs:225-229` | `mod.rs:317-328` |
| 写设置前先落意图，`enable` 幂等 | `mod.rs:155-164` | `mod.rs:186-197` + `enable` 内 `running()` 早返回 |
| 只在设置**尚未生效**时结束 Cursor，失败不阻断 | `mod.rs:190-196` | `mod.rs:266-275` |
| 清理残留只认“托管签名 + loopback”，绝不删用户/企业代理 | `settings.rs:96-111` | `settings.rs:94-109` |
| 已有真实登录不覆盖；只修“本来就是本地 token”的会员缓存 | `account.rs:56-65` | `account.rs:72-81` |
| 设置文件读 JSONC、写原子替换（temp + rename） | `settings.rs:36-60` | `settings.rs:111-137` |
| 只 MITM `*.cursor.sh`，其余主机纯隧道 | `proxy.rs:132-155` | `proxy.rs:159-181` |
| 改写的上游 URL 要能被下游校验（`x-server-upstream-url`） | `proxy.rs:117-120` | `proxy.rs:118-127` |

## 三个前置条件与状态矩阵

```text
             proxy 未运行        proxy 运行
settings 未匹配  Disabled          Degraded   ← 跑着但 Cursor 没吃配置
settings 匹配    Degraded          Enabled    ← 残留设置 / 端口不符
```

| 条件 | 谁负责 | 缺了的表现 |
|---|---|---|
| Cursor 指向本机代理 | `settings.rs` | Cursor 直连 `api2.cursor.sh`，接管完全不可见 |
| Cursor 信任站点证书 | `ca.rs` + `proxy.rs` | 所有 `*.cursor.sh` TLS 失败，Cursor 表现为离线 |
| Cursor 认为已登录 | `account.rs` | 登录与额度 UI 失效，进不了 Agent 面板 |

`process.rs` 只是加速器：改完设置不重启 Cursor 也会在下次启动生效。

## Cursor 侧文件落点（对照）

| 目标 | 上游 | nsh 版 |
|---|---|---|
| `settings.json` | `~/Library/Application Support/Cursor/User/settings.json`（同） | 同 |
| `state.vscdb` | `…/User/globalStorage/state.vscdb`（同） | 同 |
| CA 证书/私钥 | `~/.cursor-byok-v3/ca/{ca.crt,ca.key}` | `~/.cursor-byok-nsh/ca/{ca.crt,ca.key}` |
| 意图与端口 | SQLite `cursor_takeover_enabled` + `network_ports` | `~/.cursor-byok-nsh/harness.json` |

写入的 5 个托管键两侧完全一致，并删除 `http.noProxy`：

```text
http.proxy                            = http://127.0.0.1:<port>
http.proxyKerberosServicePrincipal    = 同上
http.proxySupport                     = "on"
cursor.general.disableHttp2           = true
http.experimental.systemCertificatesV2 = true
```

## 对 TSX 的边界

```ts
new Harness()                               // 读 harness.json，恢复端口与开关
await harness.status()                      // HarnessState，见下
await harness.setEnabled(enabled)           // 切接管；CA 未信任时抛错且不动 Cursor
await harness.initializeCa()                // 生成缺失的 CA（阻塞工作，跑在 blocking 线程）
await harness.cleanupStaleSettings()        // 清崩溃残留，返回是否清掉了
await Harness.quitCursor()                  // 结束 Cursor，返回它是否在运行
```

```ts
type CaState = "missing" | "untrusted" | "ready" | "invalid";
type IntegrationState = "disabled" | "enabled" | "degraded";
interface HarnessState {
  platform: string;
  ca: CaState;
  integration: IntegrationState;
  settingsApplied: boolean;
  proxyUrl?: string | null;      // 仅在运行时存在
  proxyPort?: number | null;
  caInstallCommand?: string | null;  // sudo 命令，只展示不执行
}
```

类型由 Rust 生成（`.nsh/types/cursor-byok/native.d.ts`），Rust 侧的 `CaState` / `IntegrationState` 分别镜像 `harness::CaState` / `harness::IntegrationState`，映射在 `src/lib.rs` 的 `From` 实现里——只此一处翻译。

## 验证

```sh
cargo check --all-targets   # 无 warning
cargo test                  # 20 passed
nsh check                   # crate + TSX 类型检查
nsh test                    # 21 passed（含 App.test.tsx）
```

覆盖到的行为（每条一个测试）：

| 测试 | 固定的是 |
|---|---|
| `generates_once_then_loads_the_same_issuer` | CA 生成幂等 |
| `a_key_that_does_not_match_the_certificate_is_invalid` | 密钥与证书不匹配 → `Invalid` |
| `writes_every_managed_key_and_removes_no_proxy` | 5 键写全 + 删 `noProxy` + 保留非托管键 |
| `clearing_keeps_unmanaged_keys` | 停用只删托管键 |
| `stale_cleanup_only_removes_a_loopback_managed_proxy` | 企业代理与残缺签名都不动 |
| `injects_the_local_account_and_repairs_its_membership` | 本地账号注入 + 会员字段自愈 |
| `leaves_a_real_signed_in_account_alone` | 真实登录不覆盖 |
| `recognises_only_the_injected_cursor_token` | token 判定边界 |
| `a_missing_file_reads_as_off` / `a_save_state` / `a_corrupt_file_reads_as_off` | 意图持久化与降级 |
| `only_cursor_hosts_are_cursor_hosts` | 域名判定（含大小写与尾点） |
| `a_backendless_takeover_routes_nothing` | #1：无后端不改写 |
| `a_configured_backend_routes_only_allowlisted_cursor_paths` | 白名单边界 |
| `starting_twice_returns_the_same_port_and_stopping_clears_it` | 代理启停幂等 |
| `a_taken_port_falls_back_to_a_free_one` | 端口被占回退 |
| `status_reports_disabled_before_anything_is_enabled` | 初始状态矩阵 |
| `enabling_without_a_trusted_ca_fails_before_touching_cursor` | 第一道门 |
| `disable_clears_settings_and_persists_the_intent` | 停用顺序 |
| `stale_cleanup_reports_whether_it_removed_anything` | #13 |

**未做的验证**：没有在真机 Cursor 上跑通完整接管（需要 sudo 信任 CA、会结束 Cursor 进程）。这一步的验收是单测与类型检查，端到端留给接管页一起做。

## 后续

1. **接管页 UI**：`src/ui/` 基础层 + `src/screens/Cursor.tsx`，按 `ca → 复制安装命令 → 初始化 → 启用 → 状态回读` 的顺序；`caInstallCommand` 只复制不执行。
2. **备份与还原**：两侧目前都直接改用户的 `settings.json` 与 `state.vscdb`。要支持多机/多账号，应在首次写入前把原始键值存到 `harness.json` 同级，`disable()` 时还原而不是删除。
3. **后端接线**：local loop 服务端进仓库后调用 `harness.set_backend_addr(addr)`，`Routing.backend` 一变，白名单内请求才开始改写到本地。
4. **tab 中转**：需要时再加 `TabMode` 与 `TAB_PATHS`（差异 #2），届时要同时实现 tab 服务。