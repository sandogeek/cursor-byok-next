# 移植记录：系统设置

上游实现：`cursor-byok/apps/desktop/src/features/settings/`（`SettingsPage.tsx` + 7 张卡片）+ `server/src/store/settings.rs` + `server/src/control/settings.rs`。
本移植：`apps/desktop/src/screens/SystemSettings.tsx` + `src/ui/{SettingRow,NumberField,Select,ChoiceGroup,appearance}` + `src/harness/{state,mod}.rs` + `src/desktop.rs` + `src/lib.rs` 的 `Harness` / `Desktop`。

范围只有一件事：**给本仓库真正拥有的状态提供设置界面**。端口、调用记录、开机自启、静默启动、外观、界面语言、Dock 图标、数据目录位置；不做上游那些本仓库没有后端的功能。

## 目录映射

```text
cursor-byok/apps/desktop/src/features/settings/   apps/desktop/src/
├── SettingsPage.tsx               319  页面编排   screens/SystemSettings.tsx  394  分区与操作
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

apps/desktop/src-tauri/src/desktop.rs  窗口 / 外观   apps/desktop/src/
├── create_main_window: Overlay 标题栏 + background_color  desktop.rs 255  Dock 设置 + 原生外观（set_app_appearance）
└── （WebView 前端自己画顶栏）                              App.tsx    25  启动顺序：外观 → 原生 → 语言 → 挂载

新增本仓库自有控件（上游用 SCSS 模块手写）：
src/ui/SettingRow.tsx    20  设置行：标题 / 说明 / 尾部控件
src/ui/NumberField.tsx   38  数字框（nsh/base Input 的应用层样式）
src/ui/Select.tsx        42  下拉框（表单字段外观，系统设置页的语言 / 选项复用）
src/ui/ChoiceGroup.tsx   35  分段选择（RadioGroup，三类外观选项）
src/ui/appearance.ts     34  外观持久化（Storage 键）+ 启动返回值
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
├── 启动与窗口
│   ├── 开机自启
│   │   ├── appInfo().development === true    禁用开关 + 开发模式提示
│   │   ├── Application.launchAtLogin()       读取系统真实登录项（页面加载 / 窗口聚焦刷新）
│   │   └── Application.setLaunchAtLogin      切换后回读，若仍为 false 显示待批准引导
│   ├── 静默启动
│   │   ├── Desktop.snapshot().silentStart    读 desktop.json，启动决定是否 window.focus()
│   │   └── Desktop.setSilentStart            原子写 desktop.json（保留 dockIcon）
│   └── Dock（仅 macOS）
│       ├── Desktop.snapshot()                读 desktop.json，无副作用
│       ├── Desktop.setDockIconVisible        AppKit 主线程切换 → 原子保存 → 失败回滚
│       └── App.tsx 启动挂载后 Desktop.restore 静默启动时初始 WindowPresence::Closed（隐藏 Dock）
├── 托盘与单实例（macOS / Windows / Linux 统一关闭隐藏到托盘）
│   ├── nsh.json singleInstance + defaultVisible: false
│   ├── Cursor 根组件 Tray.set                单色模板图标 / 打开 / 退出
│   ├── 关闭按钮 → closeOnPress: false → WindowRef.hide()
│   └── 托盘点击 / 菜单「打开」/ Windows·Linux 二次启动 → currentWindow().focus()
│       └── 同一主窗口 / 引擎 / 接管生命周期，不重建应用
├── 外观 → chooseAppearance（setColorScheme + Storage）
│              └── Desktop.setAppearance（原生标题栏，本次新增）
├── 界面语言 → chooseLocale → setLocale + Storage（`src/screens/Language.tsx` 的 `languageItems` / `localePreference`）
└── 关于 → dataDir（只读）+ 复制 → Clipboard.writeText

持久化
├── ~/.cursor-byok-next/harness.json   enabled / proxyPort / preferredPort
├── ~/.cursor-byok-next/desktop.json   dockIcon（Visible / Hidden，默认 Visible）+ silentStart（默认 false）
├── ~/.cursor-byok-next/calls.db       service_settings.llm_detailed_logging + 调用表
├── 系统登录项                         macOS SMAppService.mainApp / Windows HKCU Run / Linux XDG autostart
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
| 8 | 应用生命周期 | 开机启动 / 静默启动 / Dock 图标 / 托盘 | 接入全部四项：开机自启走 NSH 原生 `Application.launchAtLogin` / `setLaunchAtLogin`；静默启动走 `nsh.json` `window.defaultVisible: false` + `desktop.json` `silentStart`；Dock 通过 `objc2-app-kit` 在 NSH 主线程切换；托盘与关闭进托盘扩展到全桌面平台并开启 `singleInstance: true` | NSH 原生登录项（`SMAppService.mainApp` / `Run` / XDG `.desktop`）不注入自定义 `--autostart` 参数，因此不再自建 LaunchAgent/注册表并行实现，静默启动改为控制所有启动是否直接进入托盘 |
| 8a | 桌面壳设置载体 | SQLite `desktop_settings`（`show_dock_icon` / `silent_start`） | 独立 `desktop.json`（`dockIcon` + `silentStart`），由 `Desktop` 独占；开机自启不写本地配置 | 不混进 `harness.json` 的 Cursor 接管状态机；开机自启由系统登录项作为唯一所有者，避免应用内缓存与系统设置脱节 |
| 8b | Dock 窗口联动 | 无窗口时先 `app.hide()` 再切 Accessory；托盘可重开窗口 | `WindowPresence::dock_icon(preference)` 与 `WindowPresence::at_launch(silent_start)` 统一决定当前有效策略；静默启动或关闭窗口隐藏后自动切 `Accessory`（不改写 `desktop.json` 的 `dockIcon` 偏好），最小化保留 Dock 图标；托盘 `onClick` 与菜单「打开」恢复 `Regular`（若偏好开启）并经 `currentWindow().focus()` 聚焦（NSH 的 `Window.get("main")` 不返回该窗口） | NSH 的 `WindowRef` 已负责取消最小化、显示和激活，监听 `window.state()`（`visible \|\| minimized`）也自然覆盖全屏退出后再隐藏与静默启动后首次打开的时序；保留同一引擎，不复制上游的窗口销毁与重建逻辑 |
| 8c | 生命周期失败与开发态语义 | AppKit 切换后保存，失败尝试回滚；开机启动绑定自启折叠静默开关 | Dock 与静默启动保持独立原子保存；开机自启与静默启动独立显示；`appInfo().development` 下禁用开机自启并提示，打包版开启后回读仍为 `false` 时提示待系统批准 | 静默启动对手动启动同样有效，无需强制依赖开机自启；`SMAppService.mainApp` 在 macOS 登录项未批准或 `nsh run` 开发模式下不可直接置为 `true`，需向用户明确状态 |
| 9 | 调用观测 | 独立「调用观测」卡片 | 合并进「调用记录」，与调用详细页共享 `Calls` class | 详细模式只有一个所有者；两页各写一份会让开关抢写 |
| 10 | 存储清理范围 | `details` / `all` 两档，清理后刷新全应用 store | 同样两档，只刷新本页 `CallsSnapshot` | 没有全局 store；调用详细页下次进入会自己读 |
| 11 | 语言 | 自建 i18n store + 系统语言标签 | `nsh/i18n` + `nsh` Storage 键 | `I18N.md` 已定；设置页复用侧栏同一实现，不复制第二份 |
| 12 | 主题 | `themeIds` + localStorage + `data-theme` | `setColorScheme("system"|"light"|"dark")` + Storage，另经 `Desktop.setAppearance` 把同一选择交给原生标题栏 | NSH 原生外观：`dark:` 类、组件主题和平台控件一起跟随，不需要 DOM dataset；但 NSH 的 `setColorScheme` 只设置根视图与它自己开的窗口，不设置主窗口所属的 `NSApplication`，系统标题栏会停在系统外观（见 12a） |
| 12a | 原生标题栏外观 | Tauri `titleBarStyle: Overlay` + 窗口 `background_color`，标题栏跟窗口背景走 | 标题栏本身已隐藏（`nsh.json` 的 `"window"."titleBar"."style": "hidden"`，见 `CURSOR_UI.md` 差异表）；`src/desktop.rs` 的 `set_app_appearance` 只在 macOS 设 `NSApplication.appearance`（`nil` = 跟随系统），其他平台 no-op | 标题栏隐藏后不再有「标题栏跟不跟窗口背景」的问题，剩下的是交通灯与窗口边框的外观：`NSApplication` 的外观是它各窗口的默认值，主窗口没有自己的外观，所以跟随它；这是 NSH 边界之外的唯一缺口，只能在 Rust 侧补。失败只记日志，不撤销已应用的外观选择 |
| 13 | 数据目录 | 无此信息 | 「关于」显示 `dataDir` 并可复制 | 用户排查接管 / 插件问题需要知道 `~/.cursor-byok-next` 在哪；该值由 Rust 提供，TSX 不拼路径 |
| 14 | 页面标题 | `PageContent` 内部滚动 | 应用壳统一标题栏 + 唯一滚动区 | `CURSOR_UI.md`：内容区是唯一滚动所有者 |
| 15 | 保存反馈 | toast | 卡片内行内 notice（成功）/ 页尾 alert（失败） | 本仓库没有 toast 组件；设置项的反馈贴近控件更明确 |
| 16 | 端口意图的存储形态 | 上游把端口配置当普通数字，缺省即默认端口 | `PortChoice { Unset, OsAssigned, Fixed(NonZeroU16) }`；JSON 与 TSX 边界仍是 `null / 0 / number` | “没选过”和“这次让 OS 挑”是两种意图，只用一个数字表示会让重启后行为分叉；`NonZeroU16` 让 `Fixed(0)` 这种非法状态无法构造，自定义 `Serialize`/`Deserialize` 保持线上格式 |
| 17 | 原子写 | 每处各写一遍 temp + rename（`harness.json`、`settings.json`、CA 文件、规则库） | 统一到 `src/atomic.rs` 的 `write` / `write_with_mode`：同目录临时文件、`create_new`、随机后缀、失败时由 RAII guard 清理临时文件 | 四份实现里只有一份处理了临时文件清理；写 Cursor 文件与 CA 的原子性是不变量 5，不能各写各的 |
| 18 | 覆盖目标的差异 | 未区分平台 | `replace` 直接 `fs::rename`（Windows 的 `MoveFileEx` 会替换目标），不再先删目标 | 先删再 rename 会留出一个“目标不存在”的窗口，崩溃后既没有旧文件也没有新文件 |
| 19 | 端口选择的值域 | 普通 `u16` | `PortChoice::Fixed(NonZeroU16)` | `Fixed(0)` 与 `OsAssigned` 序列化后无法区分，读回来会变体漂移；零只能表示“让 OS 挑” |
| 20 | 写入的文件权限 | OS 默认 | 未指定 mode 的写保留原文件 mode；指定 mode 时在建文件那一刻设置 | 重写 `ca.key`（0600）不能让权限退回 0644 |

## 必须保持的不变量

1. **CA 未 `Ready` 不影响设置页。** 设置页只读 status，不改接管；端口保存在 `harness.json`，写它不需要 CA。
2. **端口只改意图，不改在途代理。** `setPreferredPort` 不重启、不写 Cursor 文件；界面文案明确「下次开启接管时生效」。它与其他意图写入（开关、状态自愈）共用同一个 `update_intent`，且都在 `operation` 锁内，避免读-改-写互相覆盖。
3. **端口范围 0–65535**；`0` 表示由系统选择，空串表示清掉选择（沿用上次绑定）。越界不发起调用，只显示行内错误。
4. **调用记录只有一个所有者。** 详细模式与清理都走 `Calls`（`calls.db`），设置页和调用详细页共享同一份实现。
5. **清理是不可撤销操作**，必须经确认弹窗，文案写明保留什么、删什么。
6. **外观与语言立即生效**，写入 `Storage` 后重启仍保留；「跟随系统」是删除键值，不是写特殊值。
6a. **外观有两个落点，一个所有者。** 选择只存在 TSX（Storage + `signal`）；`chooseAppearance` 写 NSH 的 `setColorScheme`，同一个值经 `Desktop.setAppearance` 交给 `NSApplication`（隐藏标题栏后的交通灯与窗口边框跟着它）。Rust 不存这份选择，也不从 `Storage` 读它。
6b. **原生外观失败不阻断界面。** `Desktop.setAppearance` 的错误只 `console.warn`，屏幕外观与 Storage 保持已选值；`Desktop` 记住最后一次成功应用的值，重复选择不再过主线程。
6c. **启动时只在有固定选择时调原生。** `restoreAppearance()` 返回 `null` 表示跟随系统，此时设置 `NSApplication.appearance = nil` 由 AppKit 默认值完成，不必跨线程调用。
6d. **只有 macOS 需要这次原生同步。** Windows 的标题按钮由 NSH 按应用配色自绘，Linux 的 GTK 头部跟随环境；两者对 `set_app_appearance` 是 no-op，不引入平台分支到 TSX。
6e. **标题栏的隐藏是窗口启动属性，不是设置项。** `nsh.json` 的 `"window"."titleBar"` 声明；`Desktop.setAppearance` 只改外观，不改标题栏样式，也不在运行时 `set` 它。
7. **文案只放 `locales/`**，TSX 只调 `t()`；Rust 不新增用户可见文案。
8. **设置页读取失败不抛错给界面**（`resource` 保留最后值），也不写假数据。
9. **TSX 不拼路径、不猜端口。** `dataDir`、`preferredProxyPort`、`proxyPort` 全部来自 `HarnessState`。
10. **测试走临时目录替身**，不读 `~/.cursor-byok-next`，不碰真实 Cursor 配置。
11. **`harness.json` 读失败一律降级为默认值**；字段缺失不做迁移（beta 不写兼容层），`preferredPort` 缺失由 serde 的 `Option` 自然读成"没选过"。
12. **`proxyPort` 与 `preferredPort` 不互相覆盖**：绑定成功只写 `proxyPort`，用户选择只写 `preferredPort`；两者经同一个 `update_intent` 在 `operation` 锁内写入。
13. **`data_dir()` 每进程解析一次**（`OnceLock`），所有存储共用同一个目录值。
14. **Dock 只在 macOS 暴露并立即生效**，默认 `Visible`；`desktop.json` 用原子写，成功后才更新 Rust 的已确认状态。
15. **AppKit 只经 NSH 的 `run_on_main_thread` 调用**。先切策略再保存；保存失败必须尝试恢复旧策略，界面不更新为新值。
16. **Dock 隐藏不销毁或隐藏当前窗口**，也不阻止接管后台运行。关闭最后一个窗口后自动切为 `Accessory` 隐藏 Dock 图标但不改写 `desktop.json`；最小化到 Dock 仍视为窗口开启，不隐藏 Dock 图标。托盘点击或「打开」必须按偏好恢复并聚焦同一主窗口；退出走系统 `quit`，不能仅隐藏窗口。
17. **读取与恢复分开**：设置页 `snapshot()` 只读，启动 `restore()` 才应用保存的策略；失败记录日志而不阻断界面。
18. **全平台关闭按钮只隐藏窗口**，不能销毁引擎、接管代理或调用存储；仅托盘「退出」与系统退出真正结束进程；`nsh.json` 开启 `singleInstance: true` 防止 Windows / Linux 后台常驻时重复拉起多实例。托盘点击与菜单「打开」共用同一 `currentWindow()` 引用的 `focus()`；不经 `Window.get("main")`（该 API 不返回本应用主窗口）。
19. **开机自启只有系统一个所有者。** 不在 `desktop.json` 保存 `autostart` 布尔值，每次进入设置页及窗口重新获焦时通过 `Application.launchAtLogin()` 读取系统状态；`nsh run` 开发模式下禁用开关；打包版开启后回读仍为 `false` 时展示待系统批准提示。
20. **静默启动决定首次启动是否显示主窗口。** `nsh.json` 以 `window.defaultVisible: false` 启动隐藏主窗口，由 `App.tsx` 读取 `desktop.snapshot().silentStart` 后决定是否调用 `focus()`；为 `true` 时不弹出主窗口，`Desktop` 初始 `WindowPresence` 为 `Closed`，macOS 同步隐藏 Dock 图标直到用户从托盘或菜单打开窗口。

## 验证

```sh
cargo test -p nsh-app-cursor-byok-next --lib
cd apps/desktop && nsh check && nsh test
nsh test test-fixtures/Visual.test.tsx --target macos
```

Rust 覆盖：`a_file_without_a_choice_field_reads_as_no_choice`、`a_state_without_a_choice_round_trips`（无选择在文件里是显式 `null`）、`a_chosen_port_is_remembered_for_the_next_run`、`a_saved_zero_asks_for_a_fresh_os_pick_even_after_a_bind`、`the_port_to_bind_prefers_the_choice_over_the_last_bind`（纯函数，覆盖"用户选择 / 沿用上次绑定 / 平台默认"三条分支），以及原有的 harness 状态机测试。

TSX 覆盖：
- `系统设置页保存端口与清理存储都走真实边界`（默认禁用保存 → 输入后启用 → 只调用一次 `setPreferredPort` → 清理确认）
- `系统设置页的外观与语言选择立即生效并记住`（Storage 键两条 + 英文文案切换；跟随系统启动不调原生，选暗色 / 亮色 / 跟随系统各外发一次 `appearance:*`）
- `原生外观失败时仍保留外观选择`（`desktop.setAppearance` 抛错后 Storage 与分段选择仍是所选值）
- `系统设置页在端口越界时给出错误文案`（越界不禁用保存但仍显示错误）
- `清空端口字段表示沿用上次绑定的端口`（清空后保存发送 `null`，而不是 `0`）
- `系统设置页的 Dock 开关立即切换并保留真实返回值`（关闭与重新开启各调用一次真实边界）
- `Dock 设置保存失败时显示错误并保留已确认的状态`（失败后可重试，不把开关改成未保存值）
- `非 macOS 平台不显示 Dock 设置`
- 真实 macOS 窗口：关闭后主窗口隐藏，测试宿主的 `tray.open` 经同一 `WindowRef.focus()` 恢复为可见且聚焦；生产侧托盘 `onClick` / 菜单「打开」走同一路径；菜单含「打开 Cursor-BYOK-Next」与系统退出。

Rust 的 `desktop.rs` 覆盖默认显示、保存并重读、不可读文件回默认，以及缺失字段读成显示；全部使用临时目录。

本步新增 4 项 Dock 界面测试与 5 项 Rust 设置测试；外观原生同步后续补入 1 项 TSX 失败路径测试与真机双向验收。

本轮验证（2026-11-24）：`nsh check`、`nsh build` 通过；`nsh test` 200/201 通过、1 跳过（含 Dock 三项与外观两项）；`nsh test test-fixtures/Visual.test.tsx --target macos` 通过（关闭隐藏 → `tray.open` → 同一 `WindowRef` 可见且聚焦）；`cargo test -p nsh-app-cursor-byok-next --lib -- desktop::tests` 5/5；`cargo clippy -p nsh-app-cursor-byok-next --all-targets --all-features -- -D warnings` 通过。

外观的原生标题栏在真实 `nsh run` 窗口里双向核对（2026-11-24，系统外观为浅色）：选「暗色」后交通灯与标题栏条变深，选「跟随系统」后随系统回到浅色，再选「暗色」又变深；三次切换中内容与标题栏始终同色。

### 设计完成清单

- 圆角：卡片 `--radius-lg`，按钮 / 输入 / 下拉 / 代码块 `--radius`，状态标签胶囊，分段选择内部同心。
- 字体：分区标题 `text-sm font-semibold`，行标题 `text-sm`，说明 `text-xs muted-foreground`，数据目录与端口号用等宽 / `tabular-nums`。
- 间距：卡片 `p-4`、卡片之间 `gap-4`，设置行上下 `py-3` 并有分隔线，控件与标题 `gap-4`；行内控件 `gap-2`。
- 对齐：所有设置行共用左右两条内容脊线；尾部控件固定右缘；端口行「当前端口」与错误提示共用底行。
- 颜色：只用主题 token；主按钮仅「保存端口」一处；错误用 `destructive`，成功用普通 muted 文本而非绿色横幅。
- hover / pressed / focus：按钮缩放 0.97，分段项颜色过渡，`focus-visible` / `focus-within` ring；禁用无反馈。
- 状态：端口（未编辑 / 已编辑 / 越界 / 未绑定 / 已保存）、调用记录（0 条时清理禁用）、读取失败（页尾 alert）、清理确认（两档文案不同）全覆盖。
- 动效：卡片内容首次进入淡入上移 8pt（`motion.base`），开关与分段选择颜色 / 位移过渡，全部带 `motion-reduce:` 或 `createMotion` 默认策略。

### 其他验证范围

- `nsh test test-fixtures/Visual.test.tsx --target macos` 本轮（2026-11-24）通过；`/tmp/cursor-byok-next-{light,dark}-settings.png` 已检查 Dock 行的圆角、字体、间距、对齐、颜色与开关状态。
- 设置页截图只画窗口内容，不含系统标题栏；原生标题栏的外观因此另用 `nsh run` 的真实窗口截图核对（见上）。
- VoiceOver、Windows / Linux、大字号与窄窗口未在真实机器逐项核对。