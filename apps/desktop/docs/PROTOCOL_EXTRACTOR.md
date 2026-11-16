# 移植记录：协议提取器（Go extractor → Rust + oxc）

上游实现：`cursor-byok/support/cursor-protocol-extractor/extractor/`（3698 行 Go，含单测）。
本移植：`crates/cursor-protocol-extractor/`（约 4450 行 Rust，含单测）。

范围只有一件事：**从已安装 Cursor 的 JS bundle 抽出 local loop 需要的 protobuf 协议**。不移植 Go 代码生成（`scripts/generate.sh` / `protoc-gen-go`），不提取 `origin.v1` / `git_forge.v1` / `anyrun.v1` 等无关包。

## 目录映射

```text
cursor-byok/support/cursor-protocol-extractor/
├── extractor/
│   ├── main.go            132 行  CLI
│   ├── types.go           273 行  领域类型 + 正则
│   ├── messages.go        344 行  makeMessageType / 类体字段
│   ├── compact.go         514 行  $() schema / enumType
│   ├── services.go        191 行  枚举与服务
│   ├── modules.go         539 行  模块边界、合并、校验
│   ├── resolver.go        446 行  符号 / 别名 / webpack 导出
│   ├── generator.go       341 行  跨包复制
│   ├── renderer.go        395 行  proto3 文本
│   └── extractor_test.go  523 行  语法与合并测试
└── scripts/extract.sh           扫描 Cursor.app，原子替换输出

crates/cursor-protocol-extractor/
├── src/
│   ├── main.rs             67 行  CLI（无 prettier）
│   ├── types.rs           448 行  领域类型 + 诊断
│   ├── ast_util.rs        194 行  oxc 节点取值
│   ├── scan.rs           1038 行  oxc Visit：消息 / 枚举 / 服务 / 模块
│   ├── compact.rs         426 行  $() 字符串 schema（仍按文本解析）
│   ├── resolver.rs        436 行  符号解析
│   ├── merge.rs           381 行  合并、归一化、包裁剪
│   ├── extract.rs         352 行  流水线 + 只生成 agent/aiserver + 原子写
│   ├── renderer.rs        397 行  proto3 文本
│   ├── validate.rs        109 行  Agent / Inference 形状（按块匹配）
│   └── tests.rs           552 行  对齐上游测试 + 包裁剪 + 诊断/写入
└── scripts/extract.sh           扫描 Cursor.app，原子替换输出
```

行数为 `wc -l` 实测，两侧都含测试。

## 差异清单

每一行都是**刻意**的，不是没抄完。

| # | 主题 | 上游 | nsh 版 | 原因 |
|---|---|---|---|---|
| 1 | 语言 | Go + 大量正则扫 bundle 文本 | Rust + oxc AST（`CallExpression` / `Class` / `ObjectExpression`） | 用户要求 Rust；AST 对工厂调用、类、赋值比正则稳 |
| 2 | compact schema | 正则找 `static $(){return[`，再切字符串 | 仍按字符串解析 `$()` 数组（`compact.rs`） | schema 本身是 `"Type\|1 name 9\|..."` 文本，不是 JS 对象；oxc 只能定位数组，字段编码还是上游规则 |
| 3 | 输出包 | 扫到的每个 `*.vN` 都写一份 proto | **只写 `agent.v1` 与 `aiserver.v1`** | local loop 编译的是 `agent_v1.proto` + `run_inference.proto`；生成文件上校验 Bidi / Agent 必要形状 |
| 4 | prettier | 默认把 Cursor bundle 拷到临时文件再 format | 不 format | 本仓库不改 Cursor 安装目录；oxc 直接吃压缩 JS；格式化不是提取正确性门槛 |
| 5 | proto 语法校验 | `jhump/protoreflect` 解析全部生成文件 | 不跑完整 proto parser；校验生成输出上的 Agent / Inference 必要形状 + 占位字段 | 本步没有 vendored protoc；形状检查按 message/service 花括号块匹配，避免跨声明误中 |
| 6 | Go 代码生成 | `scripts/generate.sh` → `protoc-gen-go` | 不做 | nsh 后续用 prost，不需要 Go 包 |
| 7 | 跨包复制 | 所有外部类型复制进当前包 | 只在 `agent.v1` / `aiserver.v1` 之间复制；Google well-known 走 import | 无关包被裁掉后没有复制来源；well-known 保持官方 proto |
| 8 | webpack 碎片输入 | 正则匹配 `1:(e,t,n)=>{...}` | 解析失败且看起来像模块表时包一层 `({ ... })` 再 parse；包装后声明变少或错误变多则回退 | oxc 不能把 `1:(e,t,n)=>{}` 当合法 Program；包装成对象字面量后数字键仍能当模块边界 |
| 9 | 后置包前缀 | 正则全局扫 `X="aiserver.v1."`，按距离回填 | 先暂存短名枚举，扫完再用最近前缀补全 | compact 测试里 `__protoPackage117="aiserver.v1."` 写在 `enumType(...)` 之后 |
| 10 | 工作区 | 独立 Go module | 仓库根 `[workspace]`：`apps/desktop` + 本 crate | extractor 不进 nsh 运行时；桌面 crate 不再自带 workspace |
| 11 | oxc 解析错误 | 无（正则扫文本） | 记入 `diagnostics.parse_errors`；`--strict` 时失败 | 部分解析不能伪装成成功 |
| 12 | `--no-strict` | 形状校验失败也进诊断，仅 strict 时退出 | 形状错误记诊断，不直接 abort；`--no-strict` 仍写出文件 | 对齐上游 `strictExtractionValidation` |
| 13 | 写盘 | 直接覆盖输出目录 | `write_protos` 先写临时目录再 rename 替换，并清掉旧文件 | CLI 与 `extract.sh` 都要原子替换，避免半成品 proto |

## 必须保持的语义（两侧一致）

| 不变量 | 上游 | nsh 版 |
|---|---|---|
| 字段 `T` 简写（`{kind:"message",T}`）解析为引用 `T` | `parseFieldObject` | `parse_field_object` |
| webpack `n.d(t,{KS:()=>T})` 导出别名可解析服务 I/O | `buildWebpackExportAliasIndex` | `handle_webpack_export` + `TypeResolver` |
| 类型解析：期望 kind 优先于当前包 | `pickBestDefinition` | 同 |
| `r.Empty` / `r.Value` → `google.protobuf.*` | `wellKnownTypeName` | 同 |
| 赋值别名 `ua=Timestamp` 可解析 | `buildAliasIndex` | 同 |
| 声明覆盖率忽略 `google.protobuf` | `declarationCoverage` | 同，且只统计 local-loop 包 |
| 匿名 `{typeName, methods}` 服务对象可提取 | `anonymousServiceRe` | `visit_object_expression` |
| 同名消息合并：字段更完整、全限定引用优先 | `mergeMessagesByTypeName` | 同 |
| compact `$()` 抽出 `agent_mode_configs=46`、`ask_question_config=48` | `TestCompactFactoryExtractsRunInferenceModeConfig` | 同名测试 |
| `InferenceService` 必须有 bidi `RunInference` 与 `RunInferenceSSE` | `validateRequiredAgentShapes` | 同，按 service 块匹配 |

## 输出契约

生成文件名与上游相同：`agent_v1.proto`、`aiserver_v1.proto`。
每个文件仍是 `syntax = "proto3"` + `package agent.v1` / `aiserver.v1`。
`option go_package` 保留上游路径，只为文本对照，nsh 运行时不消费它。

local loop 下一步（prost / ConnectRPC）只应依赖：

- `agent.v1.AgentService/RunSSE` 及 Agent 消息
- `aiserver.v1.InferenceService/RunInference` + `RunInferenceSSE`
- `aiserver.v1.BidiService/BidiAppend` 的请求形状（由生成文件 + 后续手写子集校验）

## 验证

```sh
cargo test -p cursor-protocol-extractor
cargo clippy -p cursor-protocol-extractor --all-targets -- -D warnings
```

覆盖到的行为（每条一个测试，对齐上游 `extractor_test.go`，另加包裁剪）：

| 测试 | 固定的是 |
|---|---|
| `parse_field_object_supports_shorthand_type` | `{T}` 简写 |
| `webpack_export_alias_resolves_service_message_type` | `r.KS` → 消息 |
| `resolver_prefers_expected_kind_over_current_package` | kind 优先于当前包 |
| `modern_factory_syntax_extracts_in_app_ad_service_types` | `makeMessageType` / `makeEnum` / 服务对象 |
| `resolver_maps_dotted_google_well_known_types` | `r.Empty` / `r.Value` |
| `assignment_alias_resolves_standard_protobuf_type` | 赋值别名 |
| `declaration_coverage_reports_unparsed_types_and_ignores_google_types` | 覆盖率忽略 Google 类型 |
| `extract_services_supports_anonymous_descriptors` | 匿名服务 |
| `merge_messages_prefers_*` | 合并策略 |
| `compact_factory_extracts_run_inference_mode_config` | compact `$()` / `enumType` / `QT` |
| `validate_required_agent_shapes_accepts_run_inference_mode_config` | Inference 形状 |
| `validate_required_agent_shapes_stays_inside_message_block` | 形状检查不跨 message 泄漏 |
| `local_loop_extract_drops_unrelated_packages` | 不输出 `origin.v1` |
| `scan_records_oxc_parse_errors` | oxc 语法错误进入诊断 |
| `no_strict_extract_keeps_files_when_required_shapes_are_missing` | `--no-strict` 仍写出文件 |
| `write_protos_replaces_directory_atomically` | 临时目录替换，清掉旧 proto |

**未做的验证**：没有对真实 `/Applications/Cursor.app` 的 `workbench.desktop.main.js` 做端到端提取，也没有用完整 proto parser 校验生成文件。那一步需要本机 Cursor，留给接入 prost 时一起跑 `scripts/extract.sh`。
