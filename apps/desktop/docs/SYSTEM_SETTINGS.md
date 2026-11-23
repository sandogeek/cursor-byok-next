# 移植记录：系统设置

上游实现：`cursor-byok/apps/desktop/src/features/settings/`（`SettingsPage.tsx` + 7 张卡片）+ `server/src/store/settings.rs` + `server/src/control/settings.rs`。
本移植：`apps/desktop/src/screens/SystemSettings.tsx` + `src/ui/{SettingRow,NumberField,Select,ChoiceGroup,appearance}` + `src/harness/{state,mod}.rs` + `src/lib.rs` 的 `Harness`。

范围只有一件事：**给本仓库真正拥有的状态提供设置界面**。端口、调用记录、外观、界面语言、数据目录位置；不做上游那些本仓库没有后端的功能。

## 目录映射

```text
cursor-byok/apps/desktop/src/features/settings/   apps/desktop/src/
├── SettingsPage.tsx               319  页面编排   screens/SystemSettings.tsx  277  分区与操作
├── AppLifecycleSettingsCard.tsx   166  开机/静默  ─                          ─
├── CommitSettingsCard.tsx         231  commit 模型 ─                          ─
├── ExternalApiSettingsCard.tsx     62  外部 API    ─                          ─
├── PricingSettingsCard.tsx        165  Token 单价  ─                          ─
├── ProxySettingsCard.tsx           81  出站代理    ─                          ─
└── TabSettingsCard.tsx             90  TAB 地址    ─                          ─

cursor-byok/server/src/                           apps/desktop/src/
├── store/settings.rs              714  KV 设置     harness/state.rs  130  JSON 意图 + 端口偏好
├── control/settings.rs            188  HTTP 读写   lib.rs Harness    native 方法
└── local_app/settings.rs          111  Cursor 配置 harness/settings.rs  不改（已有）

新增本仓库自有控件（上游用 SCSS 模块手写）：
src/ui/SettingRow.tsx    20  设置行：标题 / 说明 / 尾部控件
src/ui/NumberField.tsx   38  数字框（nsh/base Input 的应用层样式）
src/ui/Select.tsx        42  下拉框（表单字段外观，系统设置页的语言 / 选项复用）
src/ui/ChoiceGroup.tsx   35  分段选择（RadioGroup，三类外观选项）
src/ui/appearance.ts     28  外观持久化（Storage 键）
```

行数为 `wc -l` 实测，含测试与空行。

## 主路径

```text
系统设置（TSX）
├── 代理端口
│   ├── resource(() => takeover.status())       读 preferredProxyPort / proxyPort / dataDir
│   ├── 编辑 → signal 草稿（null = 未编辑，空串 = 清掉选择）
│   └── 保存 → Harness.setPreferredPort(number | null) → HarnessStatus → signal 镜像
│                 └── harness.json preferredPort（下次 enable 时优先尝试）
├── 调用记录
│   ├── resource(() => calls.snapshot())        detailed / storage 计数
│   ├── 详细记录开关 → Calls.setDetailed → CallsSnapshot
│   └── 清空详情 / 全部 → 确认弹窗 → Calls.clearDetails / clearAll
├── 外观 → chooseAppearance → setColorScheme + Storage
├── 界面语言 → chooseLocale → setLocale + Storage（`src/screens/Language.tsx` 的 `languageItems` / `localePreference`）
└── 关于 → dataDir（只读）+ 复制 → Clipboard.writeText

持久化
├── ~/.cursor-byok-next/harness.json   enabled / proxyPort / preferredPort
├── ~/.cursor-byok-next/calls.db       service_settings.llm_detailed_logging + 调用表
└── NSH Storage                        cursor-byok-next:appearance / :locale
```

`preferredPort` 与 `proxyPort` 分开存：前者是用户意图（`null` 表示没选过，`0` 表示这次由系统挑），后者是上次实际绑定的端口。传入 `null` 表示清掉选择，下次沿用上次绑定；传入 `0` 则即使上次绑过固定端口，下次也会重新让 OS 挑——两者不会互相顶掉。

## 差异清单

| # | 主题 | 上游 | nsh 版 | 原因 |
|---|---|---|---|---|
| 1 | 设置载体 | SQLite `service_settings` KV + HTTP API | `harness.json`（serde + temp/rename）+ native 方法 | 本仓库没有服务端 control API；`harness.json` 已是端口与意图的唯一所有者，加一份 KV 会造出第二个所有者 |
| 2 | 端口语义 | `network_ports.proxy_port`/`service_port`，重启生效 | 只有 `preferredPort`（`null` = 没选过，`0` = 系统挑），下次开启接管时生效 | 本应用没有独立的服务端口；代理端口在 enable 时绑定，这个时机本来就是现有的重启点 |
| 3 | 实际端口 | 与配置值分开存 DB | `proxyPort` 只存在于 status（运行时），`harness.json` 保留上次绑定值 | 状态只读接口不返回错误（不变量 7）；端口被占用时的回落要能被界面看见 |
| 4 | 出站代理 | `outbound_proxy` + 所有 reqwest client 重建 | 不做 | 本仓库没有内置 HTTP provider，出站只有 Magpie worker，代理由插件网关自身决定 |
| 5 | TAB / commit 模型 | 可配置公益服务地址与 commit 模型 | 不做 | `LOCAL_LOOP.md` 刻意不做清单：没有 commit settings / TAB 直连需求 |
| 6 | 外部 API | 本地 OpenAI/Anthropic 端点开关 + 密钥 | 不做 | 本仓库不对外提供模型 API，模型只经 Cursor 的 local loop |
| 7 | Token 定价 | 四个单价字段，前端换算首页用量 | 不做 | 没有首页用量概览（`MODEL_CALLS.md` 刻意不做） |
| 8 | 应用生命周期 | 开机启动 / 静默启动 / Dock 图标 / 更新 | 不做 | 这些是 Tauri 插件能力，NSH 侧要另做一套宿主能力，超出本步 |
| 9 | 调用观测 | 独立「调用观测」卡片 | 合并进「调用记录」，与调用详细页共享 `Calls` class | 详细模式只有一个所有者；两页各写一份会让开关抢写 |
| 10 | 存储清理范围 | `details` / `all` 两档，清理后刷新全应用 store | 同样两档，只刷新本页 `CallsSnapshot` | 没有全局 store；调用详细页下次进入会自己读 |
| 11 | 语言 | 自建 i18n store + 系统语言标签 | `nsh/i18n` + `nsh` Storage 键 | `I18N.md` 已定；设置页复用侧栏同一实现，不复制第二份 |
| 12 | 主题 | `themeIds` + localStorage + `data-theme` | `setColorScheme("system"|"light"|"dark")` + Storage | NSH 原生外观：`dark:` 类、组件主题和平台控件一起跟随，不需要 DOM dataset |
| 13 | 数据目录 | 无此信息 | 「关于」显示 `dataDir` 并可复制 | 用户排查接管 / 插件问题需要知道 `~/.cursor-byok-next` 在哪；该值由 Rust 提供，TSX 不拼路径 |
| 14 | 页面标题 | `PageContent` 内部滚动 | 应用壳统一标题栏 + 唯一滚动区 | `CURSOR_UI.md`：内容区是唯一滚动所有者 |
| 15 | 保存反馈 | toast | 卡片内行内 notice（成功）/ 页尾 alert（失败） | 本仓库没有 toast 组件；设置项的反馈贴近控件更明确 |
| 16 | 端口意图的存储形态 | 上游把端口配置当普通数字，缺省即默认端口 | `PortChoice { Unset, OsAssigned, Fixed(u16) }`；JSON 与 TSX 边界仍是 `null / 0 / number` | “没选过”和“这次让 OS 挑”是两种意图，只用一个数字表示会让重启后行为分叉；自定义 `Serialize`/`Deserialize` 保持线上格式 |
| 17 | 原子写 | 每处各写一遍 temp + rename（`harness.json`、`settings.json`、CA 文件、规则库） | 统一到 `src/atomic.rs` 的 `write` / `write_with_mode`：同目录临时文件、随机后缀、Windows 先删再 rename、失败时清理临时文件 | 四份实现里只有一份处理了 Windows 的 rename 覆盖和临时文件清理；写 Cursor 文件与 CA 的原子性是不变量 5，不能各写各的 |

## 必须保持的不变量

1. **CA 未 `Ready` 不影响设置页。** 设置页只读 status，不改接管；端口保存在 `harness.json`，写它不需要 CA。
2. **端口只改意图，不改在途代理。** `setPreferredPort` 不重启、不写 Cursor 文件；界面文案明确「下次开启接管时生效」。它与其他意图写入（开关、状态自愈）共用同一个 `update_intent`，且都在 `operation` 锁内，避免读-改-写互相覆盖。
3. **端口范围 0–65535**；`0` 表示由系统选择，空串表示清掉选择（沿用上次绑定）。越界不发起调用，只显示行内错误。
4. **调用记录只有一个所有者。** 详细模式与清理都走 `Calls`（`calls.db`），设置页和调用详细页共享同一份实现。
5. **清理是不可撤销操作**，必须经确认弹窗，文案写明保留什么、删什么。
6. **外观与语言立即生效**，写入 `Storage` 后重启仍保留；「跟随系统」是删除键值，不是写特殊值。
7. **文案只放 `locales/`**，TSX 只调 `t()`；Rust 不新增用户可见文案。
8. **设置页读取失败不抛错给界面**（`resource` 保留最后值），也不写假数据。
9. **TSX 不拼路径、不猜端口。** `dataDir`、`preferredProxyPort`、`proxyPort` 全部来自 `HarnessState`。
10. **测试走临时目录替身**，不读 `~/.cursor-byok-next`，不碰真实 Cursor 配置。
11. **`harness.json` 读失败一律降级为默认值**；字段缺失不做迁移（beta 不写兼容层），`preferredPort` 缺失由 serde 的 `Option` 自然读成"没选过"。
12. **`proxyPort` 与 `preferredPort` 不互相覆盖**：绑定成功只写 `proxyPort`，用户选择只写 `preferredPort`；两者经同一个 `update_intent` 在 `operation` 锁内写入。
13. **`data_dir()` 每进程解析一次**（`OnceLock`），所有存储共用同一个目录值。

## 验证

```sh
cargo test -p nsh-app-cursor-byok-next --lib
cd apps/desktop && nsh check && nsh test
nsh test test-fixtures/Visual.test.tsx --target macos
```

Rust 覆盖：`a_file_without_a_choice_field_reads_as_no_choice`、`a_state_without_a_choice_round_trips`（无选择在文件里是显式 `null`）、`a_chosen_port_is_remembered_for_the_next_run`、`a_saved_zero_asks_for_a_fresh_os_pick_even_after_a_bind`、`the_port_to_bind_prefers_the_choice_over_the_last_bind`（纯函数，覆盖"用户选择 / 沿用上次绑定 / 平台默认"三条分支），以及原有的 harness 状态机测试。

TSX 覆盖：
- `系统设置页保存端口与清理存储都走真实边界`（默认禁用保存 → 输入后启用 → 只调用一次 `setPreferredPort` → 清理确认）
- `系统设置页的外观与语言选择立即生效并记住`（Storage 键两条 + 英文文案切换）
- `系统设置页在端口越界时给出错误文案`（越界不禁用保存但仍显示错误）
- `清空端口字段表示沿用上次绑定的端口`（清空后保存发送 `null`，而不是 `0`）

`nsh test` 当前 169 项：167 通过，1 项显式跳过（`Visual.test.tsx` 需 `--target macos`）。

### 设计完成清单

- 圆角：卡片 `--radius-lg`，按钮 / 输入 / 下拉 / 代码块 `--radius`，状态标签胶囊，分段选择内部同心。
- 字体：分区标题 `text-sm font-semibold`，行标题 `text-sm`，说明 `text-xs muted-foreground`，数据目录与端口号用等宽 / `tabular-nums`。
- 间距：卡片 `p-4`、卡片之间 `gap-4`，设置行上下 `py-3` 并有分隔线，控件与标题 `gap-4`；行内控件 `gap-2`。
- 对齐：所有设置行共用左右两条内容脊线；尾部控件固定右缘；端口行「当前端口」与错误提示共用底行。
- 颜色：只用主题 token；主按钮仅「保存端口」一处；错误用 `destructive`，成功用普通 muted 文本而非绿色横幅。
- hover / pressed / focus：按钮缩放 0.97，分段项颜色过渡，`focus-visible` / `focus-within` ring；禁用无反馈。
- 状态：端口（未编辑 / 已编辑 / 越界 / 未绑定 / 已保存）、调用记录（0 条时清理禁用）、读取失败（页尾 alert）、清理确认（两档文案不同）全覆盖。
- 动效：卡片内容首次进入淡入上移 8pt（`motion.base`），开关与分段选择颜色 / 位移过渡，全部带 `motion-reduce:` 或 `createMotion` 默认策略。

### 未完成验证

- `nsh test test-fixtures/Visual.test.tsx --target macos` 已执行并通过（2026-11-23）；设置页已加进该用例（截图 `/tmp/cursor-byok-next-{light,dark}-settings.png`），明暗两态均检查过圆角、字体、间距、对齐与颜色。
- VoiceOver、Windows / Linux、大字号与窄窗口未在真实机器逐项核对。
- `cargo clippy --all-targets -- -D warnings` 在 `calls/recorder.rs`、`plugins/{adapters/anthropic,worker}.rs`、`lib.rs::Calls::new`、`calls/store.rs` 有既有告警（本步未引入，属于之前步骤的存量），未在本步处理。
- `cargo test --workspace` 中 `cursor-capture` 的 4 项 TLS 用例在未改动的工作树上同样失败（rustls 未安装进程级 CryptoProvider），与本步无关；单独 `cargo test -p cursor-capture --lib` 可通过。