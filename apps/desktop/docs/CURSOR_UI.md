# 移植记录：Cursor 接管页 UI 壳子

上游参照：

- `apps/desktop/src/shell/AppLayout.tsx` + `AppLayout.module.scss`：固定侧栏、右侧操作栏、接管状态；
- `apps/desktop/src/features/models/CursorSettingsPage.tsx`：接管开关与 CA 安装命令弹窗；
- `apps/desktop/src/features/models/CursorGates.tsx` + `CursorSettings.module.scss`：初始化 / 信任 CA 的居中空态。

## 目录映射

```text
Cursor-BYOK-Next/apps/desktop/src/            Cursor-BYOK-Next/apps/desktop/
├── shell/AppLayout.tsx        应用壳          ├── App.tsx                  只负责挂载 Cursor 页
├── features/models/           模型与门禁      ├── src/screens/Cursor.tsx   接管页、状态镜像、操作编排
│   ├── CursorSettingsPage.tsx                 ├── src/ui/
│   └── CursorGates.tsx                        │   ├── Button.tsx          按钮、图标按钮
├── shared/ui/Switch.tsx       开关            │   ├── Switch.tsx         完整轨道 / 滑块样式
├── shared/ui/ConfirmDialog.tsx 确认弹窗       │   ├── Dialog.tsx         原生键盘 / 焦点 / Escape
└── shared/ui/Controls.module.scss             │   ├── Surface.tsx        卡片、状态徽章、导航项
                                               │   ├── Icon.tsx           单色线型图标
                                               │   └── motion.ts         动效令牌
                                               ├── test-fixtures/        测试清单、隔离接管与纯视觉夹具
                                               └── nsh.json               语义颜色 / 圆角 / 缓动令牌
```

## 本步边界

复刻桌面壳、导航分组、Cursor 当前项及状态标签、右侧接管开关和刷新、CA 门禁、安装命令弹窗。数据概览、调用详细、插件配置、系统设置、使用教程保留为禁用导航占位；模型列表、添加模型、local loop 均未实现。禁用项不假装有页面，不跳路由。

接管开关、CA 初始化、刷新接 `cursor-byok-next/native` 的真实 `Harness`，不是演示状态。CA 未 `ready` 时不能启用；停用先弹确认。`caInstallCommand` 只能由用户点击复制，应用不打开终端、不执行命令、不申请 sudo。TSX 的 `HarnessState` 是 Rust 真实状态的镜像，写入只走 Rust 方法。

## 视觉与交互对照

| 主题 | 上游 | nsh 版 | 原因 |
|---|---|---|---|
| 应用壳 | 浮动卡片侧栏（距窗口左/底 8px）+ 独立浮动操作条 | 侧栏贴紧窗口左缘并顶底拉满，右侧细线分隔；操作条仍浮动 | 根布局 `p-4` 把导航从左缘推开 16pt；贴边侧栏符合桌面 sidebar workspace，不再用外层 padding 冒充浮动卡片 |
| 导航 | 数据概览 / 调用详细 / 模型配置 / Cursor / 设置分组 | 同样顺序与中文文案；未移植项禁用 | 复刻壳子，不承诺未实现功能 |
| Cursor 行 | 图标 + Cursor + 已接管 / 未接管胶囊 | 保留，状态取 `integration` | 同时表达代理与设置不一致的降级状态 |
| 操作栏 | 状态 + 开关 + 刷新；添加模型按钮 | 保留前三项；不加无功能的添加按钮 | 本步只做接管页 |
| CA 门禁 | 虚线圆角区，居中标题 / 描述 / 主按钮 | 保留；`missing`、`untrusted`、`invalid` 和读取失败均有文案 | NSH 状态还包含证书无效 |
| 安装 CA | 自动复制后弹窗，可打开终端 | 弹窗内显式“复制命令”；刷新状态 | 当前边界只允许复制，不执行安装命令 |
| 测试入口 | 无独立入口 | `test-fixtures/` 声明独立宿主能力与同一套主题，页面通过窄接口接内存替身 | 避免读取 / 改写真实 Cursor；正式入口不增加宿主权限，清单不继承主题故显式同步 |
| 状态读取 | 全应用 `refresh()` 轮询 | `resource` 初始读取 + 用户主动刷新；失败保留最后状态 | 本步只有单页，无需引入全局 store 或额外定时请求 |
| 关闭接管 | 二次确认，说明会清代理设置 | 保留确认；精简说明 | 真实写入仍完全由 Rust 控制 |
| 样式 | VS Code CSS 变量与 SCSS | NSH 语义 token、Tailwind 尺度 | 统一明暗主题，不在屏幕写颜色 |
| 图标 | Iconify / Cursor SVG | `ui/Icon` 内联 16pt 单色线型 SVG，`tintColor` 跟主题 token | 文字符号大小和笔画不齐；内联 data URI 才能让正式入口和 `test-fixtures/` 共用同一套图标，不越过 NSH 的资源目录边界 |

## 目标与设计基准

当前目标仅为桌面（macOS / Windows / Linux），鼠标与键盘优先。控件使用紧凑尺寸；不将桌面布局直接缩小为手机界面。页面内容区唯一拥有纵向滚动，侧栏不随内容移动。键盘 Tab 顺序为接管开关、刷新、CA 操作；弹窗支持 Escape，按钮有可见焦点。

视觉沿用上游的编辑器式灰色层次：中性背景、略亮侧栏、细分隔线、紧凑导航、少量蓝色主操作。绿色只表示接管有效，橙色只表示降级 / 错误。文字同时表达状态，不靠颜色区分。

## 必须保持的不变量

1. CA 非 `ready` 时开关禁用；是否写 Cursor 文件仍由 Rust 最终把关。
2. 开关镜像 `integration === "enabled"`，另显示 `degraded`，不只读 `settingsApplied`。
3. 操作串行化，同一操作完成后只接收它返回的状态；失败显示在操作附近。
4. 状态刷新失败不覆盖最后可用状态，不用假数据填空。
5. 证书命令只在弹窗里复制，绝不执行。
6. 不接触模型配置、数据库、后端地址；不扩大本步移植范围。

## 验证

- `nsh check`、`nsh build`、`cargo clippy --all-targets -- -D warnings` 通过。
- `nsh test`：30 项通过，1 项显式跳过（27 个 Rust harness 测试 + 3 个接管交互）；测试替身只使用临时状态，不读写真实 Cursor。
- `nsh test test-fixtures/Visual.test.tsx --target macos`：真实 AppKit 渲染明暗截图通过；弹窗可通过 Escape 关闭。此用例只作为显式真实平台验收运行，不进入默认 headless 集合。
- 原生真实 `Harness` 预览：当前 CA 为 `untrusted`；打开安装命令弹窗、复制命令、显示“命令已复制”、关闭弹窗均通过。没有执行安装命令、没有启用或停用接管、没有写 Cursor 设置。
- 动效实现与截图核对：hover / active 使用统一颜色与缩放过渡；开关滑块过渡；CA 内容初次淡入；弹窗原生 fade；状态文案不做无意义移动；`motion-reduce:` 与 `createMotion` 默认策略关闭对应动画。截图覆盖门禁与命令弹窗的明暗两态。

### 设计完成清单

- 圆角：贴边侧栏无外圆角；门禁与弹窗为大圆角，按钮与代码块为控制级圆角，状态标签为胶囊；嵌套表面边界一致。
- 字体：页面标题 / 正文 / 辅助 / 代码四类角色，等宽字体只用于代理地址和命令；状态不靠字号单独表达。
- 间距：侧栏贴紧窗口左缘，内侧仍为 6pt；主区 16pt 内边距；导航行、状态标签和刷新共享中线。
- 对齐：侧栏贴窗口左/顶/底边，与主区共享顶安全区；操作栏与标题共享中线，内容区为唯一滚动所有者，虚线边界贴合主区。
- 颜色：展示代码不含原始颜色或调色板类；明暗截图中前景、辅助文本、按钮和代码块可辨识，成功 / 警告同时有文字。
- hover / pressed / focus：控件具有语义按钮或开关、原生 focus-visible ring、颜色过渡与按压反馈；禁用项无操作反馈。
- 状态：缺失、未信任、就绪、无效、降级、加载、错误、复制反馈均有可见文案；未实现导航明确禁用。

### 未完成验证 / 已知限制

- macOS 的 Tab → Enter / Space 激活路径未通过自动化验收；换用前台运行、Space、先点击相邻按钮再 Tab 均未打开弹窗。按钮指针操作与弹窗 Escape 已通过。需进一步确认 NSH 原生按钮键盘焦点行为，不能记为键盘全流程通过。
- 尚未验证 VoiceOver、Windows / Linux、减少动态效果、大字号和窄窗口。当前只声明 macOS 桌面壳的视觉验收。
- `Visual.test.tsx` 使用纯展示夹具，仅证明布局与 Escape；真实操作行为由 `App.test.tsx` 的隔离宿主测试覆盖，不把视觉夹具当作接管成功。
