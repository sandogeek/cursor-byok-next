# 移植记录：Cursor 接管（`local_app` → `harness`）

上游实现：`cursor-byok/server/src/local_app/`（1149 行，含 `ca/`，含单测）。
本移植：`apps/desktop/src/harness/`（2018 行，含单测）。

范围只有一件事：**让 Cursor 愿意走本地**——把它的网络栈指向本机代理、让它信任代理签发的证书、让它以为已登录。代理之后把请求改写给谁，是下一步（local loop 的服务端）的事。

## 目录映射

```text
cursor-byok/server/src/local_app/          apps/desktop/src/harness/
├── mod.rs             235 行  CursorHarness  ├── mod.rs              507 行
├── proxy.rs           223 行  代理与路由      ├── proxy.rs            405 行
├── settings.rs        111 行  settings.json  ├── settings.rs         253 行
├── account.rs         175 行  state.vscdb    ├── account.rs          305 行
├── process.rs          91 行  结束 Cursor    ├── process.rs           99 行
└── ca/
    ├── mod.rs         239 行  CA 生成与信任   ├── ca.rs               286 行（目录收成单文件）
    └── windows.rs      75 行  Win32 ROOT 枚举 └── windows.rs           79 行
                                               └── state.rs            94 行（新增：意图与端口持久化）
```

行数为 `wc -l` 实测，两侧都含 `#[cfg(test)]`。nsh 侧新增 `state.rs`、操作串行化、后端热接线、代理生命周期与状态降级测试。

`request_uses_local_cursor_token` 现由 `backend/` 账号与兼容 stub 消费。`is_cursor_host` / `UPSTREAM_URL_HEADER` 由转发层读取：


```40:42:apps/desktop/src/harness/mod.rs
pub use self::account::{is_local_cursor_authorization, local_token};
pub use self::process::terminate_cursor as quit_cursor;
pub use self::proxy::{is_cursor_host, Routing as ProxyRouting, UPSTREAM_URL_HEADER};
```

`proxy.rs` 的本地路径白名单（`is_local_path`）与上游逐条比对后只**少一条**：上游 32 条，这里 31 条，少的正是 `/agent.v1.AgentService/RunSSE`（差异 #24）。

## 差异清单

每一行都是**刻意**的，不是没抄完。

| # | 主题 | 上游 | nsh 版 | 原因 |
|---|---|---|---|---|
| 1 | 后端未就绪时 | 一律把命中白名单的请求改写到 `127.0.0.1:<service_port>` | `Routing.backend == None` 时**原样转发**给 Cursor | 本仓库还没有本地服务；此时改写到空端口会让 Cursor 全面报错。接管可以先开，路由随后接 |
| 2 | Tab / FileSync 路由 | `is_local_path(path) \|\| (is_tab_path(path) && tab_mode != Direct)`，含 `set_tab_settings` | 不在白名单，也没有 `TabMode` | 那是独立的 tab 中转功能（16 条 `TAB_PATHS`），不属于“让 Cursor 走本地”。加它等于同时承诺实现 tab 服务 |
| 3 | 端口记忆 | 存 SQLite `network_ports`，启动读 `proxy_port` | 存 `~/.cursor-byok-next/harness.json`，重启**复用上次实际绑定的端口** | 本仓库还没有数据库。端口变了 Cursor 的 `http.proxy` 就失效，所以实际端口必须回写并复用 |
| 4 | 端口被占 | 回退随机端口，警告 | 同样回退，但把实际端口写回 `harness.json` | 见 #3；上游重启会退回“请求的端口”，nsh 重启会退回“上次真的跑通的端口” |
| 5 | `status()` 的自愈 | `if takeover_enabled && ca ready && backend_addr.is_some() { self.enable().await? }` —— 失败即返回错误 | 拆成 `restore()` + `report()`：自愈条件不再要求 `backend`，失败只 `warn` | UI 会轮询 `status()`。错误从只读接口冒出去会把“状态页刷新”变成“应用错误态”。恢复失败应该表现为 `degraded`，不是异常 |
| 6 | 自愈触发条件 | 必须有 `backend_addr` 才自愈 | 有 CA + 有意愿就自愈 | 配合 #1：代理先跑起来接管流量，后端后接 |
| 7 | 状态结构 | `CursorHarnessStatus` 含 `configured_models` / `enabled_models` | `HarnessStatus` 去掉模型计数，加 `proxy_port` | 模型计数依赖 `Store`，本步没有。`proxy_port` 是端口回退后 UI 必须能看到的 |
| 8 | 启停入口 | HTTP `PUT /__byok-api__/api/harness/cursor/enabled` + `SetEnabled { enabled }` | `#[class] Harness` 的 `setEnabled(enabled)` / `status()` / `initializeCa()` | 这个仓库没有 axum 管理面；nsh 的类型边界是 `#[class]` |
| 9 | 回滚语义 | `start` 后写端口失败 → `stop()`；写设置失败 → `stop()` | 写设置失败会尝试清除完整托管设置；端口保存失败先清设置再停代理 | 停用不变量同样适用于失败回滚，不能把 Cursor 留在死端口 |
| 10 | 锁 | 全程持有 `proxy` 锁；`apply_cursor_configuration` 是同步函数，不需要 await | `operation` 串行化 enable/disable/restore/后端接线；`proxy` 锁只在需要时持有 | `apply()` 要 await 账号注入；仅有 `proxy` 锁无法阻止启停在释放锁后交错 |
| 11 | 账号注入的串行化 | 每次 `enable` 直接写 | `operation` 串行化；仅后端就绪时注入，显式 `.busy_timeout(5s)` | 后端未就绪时请求直通官方，不能注入会被直接转发的伪 token |
| 12 | 代理 URL 解析 | `reqwest::Url` | 自带 `loopback_host()`（约 10 行） | 只用来判断“是不是 loopback”，不值得为此引入 `reqwest`（它在本仓库会额外拉 native-tls） |
| 13 | `clear_stale_managed_settings` | `Result<()>` | `Result<bool>`（是否真的清了） | UI 需要区分“没有残留”与“清掉了”，否则每次启动都要弹一次提示 |
| 14 | `terminate_cursor` | `Result<()>` | `Result<bool>`（Cursor 是否在运行） | 调用方据此决定日志措辞；也让“没装 Cursor”不被当成失败 |
| 15 | 意图持久化 | SQLite 键 `cursor_takeover_enabled` | `harness.json` 的 `{ enabled, proxyPort }` | 同上，本仓库还没有数据库；文件损坏/缺失一律读成“关”，绝不凭读不懂的文件接管 |
| 16 | 数据目录 | `~/.cursor-byok-v3`（CA 在 `ca/`） | `~/.cursor-byok-next`（CA 在 `ca/`） | 两个客户端要能并存，证书也不能互相覆盖 |
| 17 | Linux 锚文件名 | `cursor-byok-local-ca.crt` | `cursor-byok-next-local-ca.crt` | 同上 |
| 18 | 桩数据 | 无 | 测试全部指向临时目录（`SettingsStore::at` / `AccountStore::at` / `CaManager::at` / `HarnessStateStore::at`） | 单测绝不读真实 `~/Library/Application Support/Cursor` |
| 19 | 后端热接线 | 启动时复制 `backend_addr`，之后不可变 | `Routing` 持有共享 loopback 地址；`setBackendAddr` 更新已运行代理并注入账号 | 接管可以先启动，local loop 后启动；不应靠重启代理切换路由 |
| 20 | 代理任务回收 | 停止超时后丢弃 `JoinHandle` | 超时后 `abort` 并等待；`Drop` 也取消任务 | 丢弃句柄不取消 Tokio 任务，会遗留监听端口 |
| 21 | 本地 token 判定 | 比较完整序列化 token | 验证 JWT `alg=HS256`、本地 `sub` 与 marker 段 | 不依赖 JSON 字段顺序，payload 演进仍识别旧本地账号 |
| 22 | 停用 / 残留清理 | 完整通用签名 + 任意 loopback 地址即可删除 | 只清理完整匹配本 harness 代理 URL 的 5 个键 | 不删除用户后来改回的代理，也不清理其他本地工具 |
| 23 | 路由改写失败 | URI 构造失败则静默发往上游 | 返回 502，不向上游旁路 | 命中本地白名单后必须失败关闭 |
| 24 | `AgentService/Run*` | `RunSSE` 在白名单，且有本地 handler（配 `RunEngine`） | 白名单不含 `Run` / `RunSSE` / `RunPoll`，一律直通官方 | 这三条只服务 backend loop；本仓库不做 engine，而白名单命中后没有 handler 就是 502。local loop 会拒绝一部分 turn（模式、action、harness 覆盖、tool restriction），这些 turn 必须还能打到官方。见 [`docs/protocol/cursor-agent-loop-routing.md`](../../../docs/protocol/cursor-agent-loop-routing.md) |
| 25 | native 实例所有权 | 服务端拥有长生命周期 harness | `App.tsx` 模块级单例持有 `Harness` / `Plugins` / `Calls`，不在 `mount` 回调内临时构造 | NSH 的 JS 对象被 GC 时会 drop 其 Rust 实例。临时 `new Harness()` 在 `status()` 返回后触发 `ProxyRuntime::drop`，代理被 abort，但 TSX 镜像仍显示已接管，Cursor 对已配置端口反复 `ECONNREFUSED`。应用级所有者必须与 socket 生命周期一致 |

### 顺带修掉的上游隐患

- **`status()` 是自愈入口，同时是轮询接口**：见 #5，nsh 把它拆开。
- **`enable()` 里的失败路径没有覆盖 `harness.json`**：`{ enabled }` 先作为用户意图持久化，修改 Cursor 后保存实际端口；任一步失败都按真实文件状态报告 `degraded`，不伪报运行成功。

## 必须保持的语义（两侧一致）

| 不变量 | 上游 | nsh 版 |
|---|---|---|
| 启用前 CA 必须 `Ready`，否则**不写任何 Cursor 文件** | `mod.rs:172-177` | `mod.rs:253-258`（有单测 `enabling_without_a_trusted_ca_fails_before_touching_cursor`） |
| 停用顺序：先清 `settings.json`，再停代理 | `mod.rs:225-229` | `mod.rs:317-328` |
| 写设置前先落意图，`enable` 幂等 | `mod.rs:155-164` | `mod.rs:186-197` + `enable` 内 `running()` 早返回 |
| 只在设置**尚未生效**时结束 Cursor，失败不阻断 | `mod.rs:190-196` | `mod.rs:266-275` |
| 清理残留只认“完整托管签名 + loopback + 本 harness URL”，绝不删用户/企业代理 | `settings.rs:96-111` | `settings.rs` 的 `clear_proxy_settings` |
| 已有真实登录不覆盖；只修“本来就是本地 token”的会员缓存 | `account.rs:56-65` | `account.rs:72-81` |
| 设置文件读 JSONC、写原子替换（temp + rename） | `settings.rs:36-60` | `settings.rs:111-137` |
| 只 MITM `*.cursor.sh`，其余主机纯隧道 | `proxy.rs:132-155` | `proxy.rs:159-181` |
| 改写的上游 URL 要能被下游校验（`x-server-upstream-url`） | `proxy.rs:117-120` | `proxy.rs:118-127` |
| 白名单只列有本地 handler 的路径；命中后无 handler 就失败关闭，不向上游旁路 | `is_local_path` + `fallback`（`handlers.rs:167` 是显式列出） | `is_local_path`（差异 #24 去掉了 `RunSSE`） |

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

### 2026-11-23：假接管与持续重连排查

真实运行中，`~/.cursor-byok-next/harness.json` 为启用且 CA 已信任，`settings.json` 指向 `127.0.0.1:53922`，但 Cursor 对该端口持续报 `ECONNREFUSED`。临时生命周期探针确认代理已完整执行 `ProxyRuntime::start`，随后 `ProxyRuntime::drop` 又将其取消。入口在 `mount` 回调内构造 `Harness`，NSH 回收对应 JS 对象时也回收 Rust 所有者；TSX 的状态镜像却不会因此失效，所以出现“已接管”的假象。模块级单例修复后，同一端口保持监听，账号、Statsig、模型目录等 Cursor RPC 已进入本地 backend。

验证边界：已验证真实 Cursor 的连接恢复和本地 RPC 路由；完整 BYOK 生成轮次仍需在 Cursor 中发送消息后验收。

## Cursor 侧文件落点（对照）

| 目标 | 上游 | nsh 版 |
|---|---|---|
| `settings.json` | `~/Library/Application Support/Cursor/User/settings.json`（同） | 同 |
| `state.vscdb` | `…/User/globalStorage/state.vscdb`（同） | 同 |
| CA 证书/私钥 | `~/.cursor-byok-v3/ca/{ca.crt,ca.key}` | `~/.cursor-byok-next/ca/{ca.crt,ca.key}` |
| 意图与端口 | SQLite `cursor_takeover_enabled` + `network_ports` | `~/.cursor-byok-next/harness.json` |

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
await harness.setBackendAddr("127.0.0.1:3000") // local loop 监听后接线，可晚于启用
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

类型由 Rust 生成（`.nsh/types/cursor-byok-next/native.d.ts`），Rust 侧的 `CaState` / `IntegrationState` 分别镜像 `harness::CaState` / `harness::IntegrationState`，映射在 `src/lib.rs` 的 `From` 实现里——只此一处翻译。

## 验证

```sh
cargo check --all-targets   # crate + TSX 类型检查
cargo test                  # 27 passed
cargo clippy --all-targets -- -D warnings
nsh check                   # crate + TSX 类型检查
nsh test                    # 28 passed（含 App.test.tsx）
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
| `backend_loop_rpcs_stay_with_cursor_upstream` | #24：`AgentService/Run*` 不接管 |
| `the_local_loop_pair_is_routed_locally` | local loop 的读/写两条命中白名单 |
| `a_configured_backend_routes_only_allowlisted_cursor_paths` | 白名单边界 |
| `starting_twice_returns_the_same_port_and_stopping_clears_it` | 代理启停幂等 |
| `a_taken_port_falls_back_to_a_free_one` | 端口被占回退 |
| `status_reports_disabled_before_anything_is_enabled` | 初始状态矩阵 |
| `enabling_without_a_trusted_ca_fails_before_touching_cursor` | 第一道门 |
| `disable_clears_settings_and_persists_the_intent` | 停用顺序 |
| `stale_cleanup_reports_whether_it_removed_anything` | #13 |
| `a_backend_arriving_after_start_updates_the_shared_route` | 已运行代理读取新后端 |
| `a_non_loopback_backend_is_not_used` | 后端只允许 loopback |
| `dropping_releases_the_listening_port` | 代理任务回收后端口可复绑 |
| `recognises_a_local_token_with_changed_payload` | token 识别不依赖序列化 |
| `a_stale_proxy_settings_file_is_degraded` / `unreadable_settings_still_return_a_degraded_status` | 状态矩阵与只读降级 |

**未做的验证**：没有在真机 Cursor 上跑通完整接管（需要 sudo 信任 CA、会结束 Cursor 进程）。这一步的验收是单测与类型检查，端到端留给接管页一起做。

## 后续

1. **接管页 UI**：`src/ui/` 基础层 + `src/screens/Cursor.tsx`，按 `ca → 复制安装命令 → 初始化 → 启用 → 状态回读` 的顺序；`caInstallCommand` 只复制不执行。
2. **备份与还原**：两侧目前都直接改用户的 `settings.json` 与 `state.vscdb`。要支持多机/多账号，应在首次写入前把原始键值存到 `harness.json` 同级，`disable()` 时还原而不是删除。
3. **后端接线**：`harness.setBackendAddr("127.0.0.1:<port>")` 已接入类型边界；local loop 服务端进仓库后调用它，已运行的代理会立即更新白名单路由并补齐本地账号。
4. **tab 中转**：需要时再加 `TabMode` 与 `TAB_PATHS`（差异 #2），届时要同时实现 tab 服务。