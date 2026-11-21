# Cursor `RunInferencePromptModelMetadata` 与 `is_opus55`

## 结论

`is_opus55`(field 50) 是 **server → client 的模型身份标志**，唯一作用是在客户端本地拼装的 system prompt 中**条件插入 3 段 Opus 5.5 专用指令**。它不是 provider 请求参数，不会发往 Anthropic/OpenAI。

```text
server 声明模型身份 ──► client 决定 prompt 形态 ──► provider 收到的 system prefix
      (field 50)              (本地拼接)                (被这三段改变)
```

BYOK 当前把该字段留在 `..Default::default()`(`false`)，因此这 3 段不会出现；同时它们位于 system prompt 前缀中部，开关必须由**稳定的模型配置**推导，不能按请求/回合变化，否则 prefix cache 失效。

---

## 1. 字段位置

```text
protocols/cursor/run_inference.proto                    # 938 行；run_inference wire schema 全集
└── message RunInferencePromptModelMetadata              # 681–743 行；模型身份标志集合
    ├── string vendor = 1                                # "anthropic" / "openai"
    ├── string prompt_version = 2                        # "latest" / "gpt5-codex" / "dsv3-*"
    ├── bool is_opus45/46/48 = 17/18/19                  # 各代 Opus 专属 prompt 分支
    ├── bool is_opus5 = 20                               # Opus 5 家族（子串匹配，含 5.5）
    ├── bool is_opus55 = 50                              # ← 本节主题；Opus 5.5 独立位
    ├── bool is_fable5/51 = 21/43
    ├── bool is_gpt5*/is_codex_family = ...              # GPT 家族位
    └── bool is_claude_4x = 16, is_sonnet4 = 10, ...
```

field 号乱序（50 排在 21 之后）是因为**增量插入**：旧客户端按 protobuf 默认值 `false` 忽略未知 field，新客户端读新位。

携带位置：

```text
RunInferenceRunReady                                   # 778–805 行
└── prompt_model_metadata = 4                          # 783 行；每个 run 只生成一次
```

服务端生成点（BYOK 本地 loop）：

```text
server/src/cursor/inference/service.rs                   # 1175 行；单次 RunInference 流的所有者
├── run_ready()                     # 425–468 行；构造 RunInferenceRunReady
│   └── prompt_model_metadata       # 445–462 行；只设置 5 个位 + 结构字段
└── serve()                         # 294 行；run_request → run_ready 只发一次
```

---

## 2. 客户端消费链路

以 Cursor **3.30.14** 安装包为证据（`/Applications/Cursor.app/Contents/Resources/app`）：

```text
RunInferenceRunReady.prompt_model_metadata
        │
        ▼  agent-host: module 26814
   f(metadata, modelId) ──► { promptModelInfo: b(metadata, modelId), ... }
        │                        │
        │                        └── b() 逐字段拷贝；无名字兜底、无 OR 合并
        ▼
   ae(): const A = E.promptModelInfo
         if (A === undefined) throw F("unrecognized-model-family", modelId)
        │
        ▼
   prompt 渲染器
   ├── computer_use 段:     !0===e.modelInfo.isOpus55 && <p>…</p>
   └── 沟通段:              !0===e.modelInfo.isOpus55 && <p>…</p>
```

关键事实：

| 事实 | 位置 | 含义 |
| --- | --- | --- |
| `b()` 是纯赋值 | `extensions/cursor-agent-host/dist/3891.js` | `isOpus55: e.isOpus55`，**不会**从 model 名反推 |
| 准入白名单 | 同上，`b()` 开头 | `vendor ∈ {anthropic,openai,google,xai,zai,cursor,unknown}` 且 `prompt_version ∈ {dsv3-1018,dsv3-1205,cursor-0226,latest,gpt5-codex,codex-cloud,haiku}`，否则返回 `undefined` |
| 失败是抛错 | 同上，`ae()` | `metadata-unavailable` / `unrecognized-model-family`，不是“静默无补丁” |
| 名字启发式另有用途 | `cursor-agent-exec` / `cursor-local-agent-runtime` 的 `bLe()` | 仅服务 Task 子代理配置、埋点标签等，**不参与**主 prompt 的这三段 |
| 经典 workbench 不消费 | `out/vs/workbench/workbench.desktop.main.js` | 只有 protobuf field 描述符，无 `.isOpus55` 读取点 |

结论：**在本地 loop 路径上，`is_opus55` 是这三段 prompt 的唯一开关，没有第三态。**

---

## 3. 三段 prompt 的原文与用途

### 3.1 `computer_use` 段（挂载条件：computer-use 能力开启）

```text
section: computer_use > Tool Usage
base:    "The `computer` tool accepts a list of actions to execute sequentially.
          Each action will be performed in order, and a screenshot will be captured
          after all actions complete."
+ isOpus55:
         "Here `computer` is a plain custom tool with the schema in its definition,
          not the provider computer toolset: there are no per-action member tools,
          no `toolset_name`, and each call returns its own screenshot. Follow the
          schema exactly, and read the returned screenshot before deciding the next
          action instead of assuming a batch landed."
```

**纠正的偏差**：Opus 5.5 倾向把 `computer` 当成 Anthropic 原生 computer-use toolset（原生是 “toolset + 每个 action 一个成员工具”）。这里强制说明：只有 1 个普通自定义工具、action 是列表参数、截图逐次返回、**必须读截图再决定下一步**。

### 3.2 进度汇报段（挂载条件：`!isFable5 && isOpus5` 且 `isOpus55`）

```text
section: communicating_with_the_user
+ isOpus55:
         "A progress note only helps if the user can see it. Keep each one to a
          sentence or two of plain text: a longer aside tends to become reasoning
          the user never sees, and from their side the turn looks silent. Say what
          you did, what you found, or what you need next, not a restatement of the
          plan. When a check fails or a step is blocked, put that in the note
          instead of routing around it quietly."
```

**纠正的偏差**：Opus 5.5 倾向把过长旁白写进 thinking/reasoning，而客户端默认不展示 thinking → 用户侧看到长时间静默。要求：1–2 句可见纯文本，说明“做了什么 / 发现了什么 / 下一步要什么”，失败与阻塞必须显式写出。

### 3.3 原文转发段（挂载条件：`!isFable5 && isOpus5 && isOpus55` 且 `SEND_TO_USER` 存在、`SEND_MESSAGE` 不存在）

```text
+ isOpus55 && SEND_TO_USER:
         "When the user must see something exactly as written before the turn ends,
          such as a partial result, a command for them to run, or a question you are
          not blocking on, call `SEND_TO_USER` with that content instead of folding
          it into surrounding text; it reaches the user verbatim."
```

**纠正的偏差**：防止把“必须逐字看到”的内容（部分结果、要用户执行的命令、不阻塞的提问）混进正文——正文可能被折叠/摘要/被 reasoning 吸收。强制走发送工具。

### 3.4 触发条件矩阵（易错点）

```text
                      is_opus55
                     false   true
                   ┌───────┬──────────────────────────────────────┐
isOpus5 && !isFable5│  无   │ 沟通段 #2、#3                        │
                   ├───────┼──────────────────────────────────────┤
否则                │  无   │ computer_use 段 #1（若有 computer）  │
                   └───────┴──────────────────────────────────────┘
```

即：**#2/#3 实际要求 `is_opus5 = true` 与 `is_opus55 = true` 同时成立**（mount gate 是 `!isFable5 && isOpus5`），`is_opus55` 只是段内条件。只开 `is_opus55` 不开 `is_opus5` 不会得到沟通段。

---

## 4. 为什么单独开 `is_opus55` 而不复用 `is_opus5`

```text
client 名字启发式（bLe）,仅用于非 prompt 场景
  isOpus5  = id.includes("opus-5")   || includes("opus 5")     ← "opus-5-5" 也命中
  isOpus55 = includes("opus-5-5") || includes("opus-5.5") || includes("opus 5.5")
```

- `is_opus5` 是子串匹配，`opus-5-5` 天然落在里面，无法区分 5 与 5.5。
- Cursor 的 prompt 按版本调优：已对 base 5 调好的文本不动，5.5 的新偏差用新增位单独打补丁。
- 分工上：`is_opus5`/`is_opus48`/`is_fable5`/`is_fruitcake` 还参与 `reasoningEffort` 后缀解析（`-xhigh`/`-max`/`-high`…）；`is_opus55` 在本地 loop 中**只影响 prompt 拼装**。

---

## 5. BYOK 现状

```text
server/src/cursor/inference/service.rs:445
    prompt_model_metadata: Some(pb::RunInferencePromptModelMetadata {
        vendor: if anthropic { "anthropic" } else { "openai" }.into(),   # 白名单内, OK
        prompt_version: "latest".into(),                                  # 白名单内, OK
        is_claude_4x: claude,                                             # display_name 含 "claude"
        is_sonnet4: claude && name.contains("sonnet"),
        is_gpt5_family: gpt5,                                             # name.starts_with("gpt-5")
        is_gpt5: gpt5,
        persona: "auto".into(),
        self_identity_name: "Cursor".into(),
        agent_token_limit: ...,
        agent_mode_configs: agent_mode_configs(),
        ask_question_config: ...,
        subagent_model_config: ...,
        ..Default::default()          # ← is_opus5 / is_opus55 / is_sonnet45 / is_fable* 全为 false
    }),
```

推论：

| 配置的模型 | 客户端行为 | 与官方差异 |
| --- | --- | --- |
| Claude Opus 5.5 | `is_opus5 = false`, `is_opus55 = false` | 三段全无：可能把 `computer` 当原生 toolset、长旁白落进 reasoning、逐字内容被折叠 |
| Claude Sonnet | `is_claude_4x = true`, `is_sonnet4 = true` | 只命中这两位的分支，`is_sonnet45 = false` |
| GPT-5.x | `is_gpt5 = true`、`is_gpt5_family = true` | `is_gpt51/52/54/55/56`、`is_codex_family` 全 false，GPT 细分分支不出现 |

即：当前 BYOK 只声明“粗粒度家族”，所有**代际细分位**均为 false。

---

## 6. 若要让 BYOK 命中这些分支

改动只集中在一个纯函数（当前 `run_ready` 每次 run 只执行一次，天然满足“稳定输入”）：

```text
server/src/cursor/inference/service.rs
└── run_ready()  # 425–468 行
    派生规则（按顺序判定，先具体后笼统）：
      name      = display_name.to_ascii_lowercase()
      anthropic = model_type == ModelType::Anthropic
      is_opus55 = anthropic && (name.contains("opus-5.5") || name.contains("opus-5-5") || name.contains("opus 5.5"))
      is_opus5  = anthropic && (is_opus55 || name.contains("opus-5") || name.contains("opus 5"))
      ...
```

前置条件与约束：

| 项 | 要求 |
| --- | --- |
| `vendor` | 必须留在白名单（`anthropic`/`openai`），否则客户端抛 `unrecognized-model-family` |
| `prompt_version` | 必须留在白名单（当前 `latest` 合法） |
| 稳定性 | 只能依赖 `ModelConfig`（display_name/model_type），不能依赖请求内容、回合数、工具开关 —— 这三段在 system prefix 中部，翻转即 cache 失效 |
| 一致位 | 只设 `is_opus55` 而不设 `is_opus5`，沟通段仍然不出现（见 3.4） |
| 测试 | `server/src/api/cursor/handlers/run_inference_tests.rs`（892 行）已有 `prompt_model_metadata` 断言范式，可加“Opus 5.5 名称 → 两个位都为 true”的用例 |

不建议做的：用 `feature_flags` 或按回合动态开关来控制这三段；也不建议让 provider 侧补写等价 prompt（那是另一条会破坏 provider 中立性的路径）。

---

## 7. 验证方法

```bash
# 1) 字段与生成点
rg -n "is_opus55" protocols/cursor/run_inference.proto
rg -n "prompt_model_metadata" server/src/cursor/inference/service.rs

# 2) 客户端是否有名字兜底（在本地 loop 路径上应只看到纯赋值）
rg -o '.{0,120}isOpus55:.{0,120}' /Applications/Cursor.app/Contents/Resources/app/extensions/cursor-agent-host/dist/3891.js

# 3) 三段 prompt 的实际挂载点
rg -o '.{0,200}isOpus55&&.{0,200}' /Applications/Cursor.app/Contents/Resources/app/extensions/cursor-agent-exec/dist/main.js

# 4) 运行现有测试
cargo test -p cursor-server run_ready
```

证据版本：Cursor `3.30.14`（`/Applications/Cursor.app/Contents/Resources/app/package.json`）。升级 Cursor 后上述 bundle 中的函数名与段名可能变化，结论的验证方式不变。