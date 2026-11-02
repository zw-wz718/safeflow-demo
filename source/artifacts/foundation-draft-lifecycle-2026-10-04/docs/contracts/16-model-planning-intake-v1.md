# 合同 16：模型计划安全接收 v1

2026-10-06，BASE-R06 / BASE-06 的内部规划接收子项。实现位于 `crates/runtime/src/planning_intake.rs`，接收版本 `PLANNING_INTAKE_CONTRACT_VERSION = 1`；模型结果仍使用冻结 PlanningResult / AgentPlan v1，不改变合同 06 / 14 的语义。本公共子项已[独立验收通过](../../../planning-intake-review-2026-10-06/REVIEW.md)，不代表真实模型、页面业务闭环或 BASE-06 完成。

## 来源与接口

`PlanningIntakeService::for_trusted_backend(runtime, gateway, contracts)` 只接受既有 RuntimeBootstrap 启动成功的 Runtime，复用同一个版本化 SessionResolver、Tool / Policy / Profile 注册和实际 Kernel。`contracts` 复用合同 14 的 `PreflightInputContract`：后端明确批准的工具、Schema 版本、Policy 版本和直接输入投影修订；不是由模型 JSON 指定的许可。重复、缺失、版本不匹配或非法修订配置拒绝。未配置 Gateway 时 `plan` 明确返回 `GatewayNotConfigured`；仍可调用原始接收入口。

两条内部入口：

```rust
service.accept_json(&session_ref, &expected_task_id, expected_plan_version, raw_json).await?;
service.plan(&session_ref, &expected_task_id, expected_plan_version, user_input).await?;
```

身份只来自该 Runtime 的版本化会话；预期 task_id / plan_version 来自可信后端。**本期不查询数据库归属、不创建任务、不证明旧任务可改版**，后端不得把客户端或模型任意任务号当成可信预期。PASS 计划必须准确匹配预期；CLARIFY 没有计划，也绑定返回的后端任务上下文。零版本、非法标识和无版本会话源拒绝。

Gateway 仍使用现有 `LlmGateway::generate(&PlanningInput)`，只收到用户文字与当次批准的 ToolDefinition，不发送服务端身份、会话、预期任务号、Policy 实例、审计、凭证或数据库。旧 PlanningService / MockLlm / Runtime 接口保留。当前冻结 PlanningInput 没有任务上下文字段，本轮不擅自改接口或把任务号塞入用户文字；演示 MockLlm 预设匹配任务结果。正式模型的任务上下文协议、传输、模型名称、地址、凭据和超时策略仍待确认。

## 同一接收核心

原始入口用 Serde 的有界 `DeserializeSeed` / `Visitor` 解析为 Value，在任意对象中对**解码后的键**去重，再反序列化冻结强类型；包含嵌套 entities / arguments 和 `a` / `\u0061` 别名。不提取 Markdown / 自由文本 JSON，不截断或修复。畸形 JSON、非法 UTF-8、尾随第二个值、未知结构字段、浮点或过量内容拒绝。

强类型入口至多调用一次 Gateway，随后与原始入口共用结果、参数、注册和 Kernel 校验。强类型 Value 已无法表示重复键，不能恢复一个上游先行宽松解析所丢弃的重复键；未来真实 Gateway 必须在解析原始响应时采用本入口的严格路径，不得以普通 `from_value` / Map 去重后伪称原始校验通过。这里的大小限制验证的是已返回的强类型结果，不能限制外部 Gateway 自己在返回之前的分配或网络读取，正式适配器须另行落实传输限制。

结果必须满足既有 PASS / CLARIFY 组合及信封版本。理解 intent 非空，entities 为有界对象；PASS 不得同时声明 missing_fields。澄清问题非空且有界，missing_fields 至多 64 个不同的有界名称。未确认的 context_refs / 变量替换 / 结果引用不启用，非空 context_refs 和参数 `${...}`、`{{...}}`、`$ref` 拒绝。理解与澄清自由文本始终是**模型提案**，不作为可信身份、安全事实或成功；调用方只能按不可信文本显示，不解释为 HTML / 指令。

计划节点唯一、有界且非空。依赖必须存在，重复依赖、自依赖、循环拒绝；复用 Runtime 当前 Kernel 的 PLAN_ACCEPTANCE 结构校验，不运行 DomainPolicy。只接收当次批准且仍成对注册的工具。每个 Tool 参数复用 **同一份未放宽的 `decision_input::validate_direct_object`**：仅支持合同 14 已确认的封闭直接对象参数能力，包含类型、required / additionalProperties、enum、字符串长度 / 兼容 pattern、整数约束。畸形或不支持的 Schema 即拒绝，即使违规约束位于缺省可选属性；不忽略约束、不猜业务字段。

金额在已确认参数中沿用十进制字符串，没有浮点运算。本轮保守拒绝结果任意层级的浮点，包括 entities；不为未知业务补金额限额、收款人归属或资金规则。通过结构 / 参数检查不证明账户归属、余额可用或业务可执行，仍由 Runtime 后续的安全门禁核验。

## 技术边界

以下是解析和资源边界，不是业务限额或产品 TTL；超限整体拒绝，不截断。

| 内容 | 上限 / 约束 |
| --- | --- |
| 用户输入 | 非空白，4096 UTF-8 字节 |
| 原始 JSON / 强类型结果的紧凑序列化 | 65536 字节；原始含空白与转义，强类型另校验真实编码大小 |
| 计划 | 1..256 节点；整个结果已限 64 KiB，计划不会超出现有 64 KiB 边界 |
| 单节点依赖 / 任意 JSON 数组或对象成员 | 至多 256，依赖不重复 |
| JSON 深度 | 根为深度 0，最多 16 个边；在继续构造子值前拒绝 |
| 累计 JSON 值数量 / 解码字符串字节 | 单个结果至多 4096 个值 / 65536 字节，含对象键；强类型在复制序列化前也检查借用字段的累计预算 |
| 单个字符串 / 对象键和标识 | 至多 4096 / 200 UTF-8 字节；标识无首尾空白或控制字符，结构文字非空且无控制字符 |
| 单 Tool 参数 / Schema | 沿用合同 14 的 4096 / 16384 编码字节、至多 64 属性及对应封闭限制 |
| 批量工具定义 | 至多 256 项、累计编码 65536 字节，description 4096 字节、action 200 字节 |

原始与强类型最终都检查相同结果编码、深度、计数及参数。受限 Schema 原本不支持复杂嵌套业务参数、远程 `$ref`、任意 JSON Schema 或不保证 ECMAScript 兼容的正则；本轮不因此扩大合同。

## 会话、错误与执行边界

请求前解析 user_id + 非零 binding_version，Gateway 等待后（包括错误）及 Kernel 等待后复核；返回前再次复核，最后同步检查之后不再 await。同用户续期 / 重绑、撤销、换用户或无版本 resolver 均拒绝。批准工具的当次完整定义在等待前后及返回前比较，注册 / Schema 漂移拒绝。没有分布式会话事务，最后同步检查后的外部撤销需要正式身份源协调；本提案不能被缓存成执行授权。

`AcceptedPlanningProposal` 无公共构造或 Deserialize，只保存受检提案和预期任务版本，提供只读 getter 和显式取出 PlanningResult；不包含身份、会话、Permit、SecurityDecision 或成功布尔值。返回仅表示 **PROPOSAL_ACCEPTED**，CLARIFY 仍需等待用户。取出的计划进入 Runtime 仍重新经过原 PLAN_ACCEPTANCE / 所有执行前门禁；普通 Runtime 的写工具拦截、显式确认、正式资金及 PRE_COMMIT 关闭边界不变。

不执行 Tool / DomainPolicy、不调用银行、数据库或 LLM 第二次；不保存任务 / 操作 / 凭证 / 审计，也不自动恢复。Kernel 结构检查只产生进程内结构检查，不输出可用安全裁决或授权。请求取消即丢弃当前 future，没有自动重试；已经触发的外部模型请求是否真正终止取决于未来 Gateway 的取消合同，本轮不声称远端已取消。

错误是无动态原文的固定 `PlanningIntakeError` 枚举及 `PLANNING_INTAKE_*` 文本：Session、InvalidInput、Limit、InvalidJson、InvalidResult、TaskBinding、Registration、InputContract、GatewayNotConfigured、GatewayFailed。模型原文、原始输入、Parser 诊断、Gateway / SessionResolver 原始错误、令牌或连接信息均不进入错误；不定义 HTTP 错误码。

## 兼容与待对接

仅新增 Runtime 接收模块及存储 crate 中使用既有依赖的隔离测试 / 演示；将已验收 `decision_input.rs` 作为同一 crate 模块供两个入口复用，**文件校验语义及内容未改**。decision_review 的模块引用和 lib 导出兼容更新，冻结合同、业务 crates/features、存储 / 任务查询、迁移、清单及锁文件不改。前置裁决既有代码源码摘要随模块引用变化，旧记录按既有来源版本规则重新核验，不能忽略来源变化。

| 负责人 | 缺少材料 | 影响 / 恢复条件 |
| --- | --- | --- |
| 底座，模型服务负责人待确认 | 模型 URL / 名称 / 凭据、原始响应与任务上下文传输、读取 / 超时 / 取消合同 | 确认后才做真实 Gateway；当前仅 MockLlm 和原始 JSON 内部入口 |
| 底座 / 页面承接人，正式身份负责人待确认 | C-HTTP、正式登录、可撤销版本化会话真源及任务归属协议 | 确认后装配外部接收 / 持久任务入口，不能让客户端自定可信任务上下文 |
| 各业务负责人及底座 | 真实 Tool / Policy / 完整参数投影、资源及事实版本合同 | 实际交付并确认后登记相应直接输入；本轮只复用已存在工具，不代写业务 |
| 公共安全与对应业务 | 正式认证、当前裁决、PRE_COMMIT / Permit 真源和已确认规则 | 齐备后另行接通正式执行；接收提案不解决这些依赖 |

运行、真实结果及保全说明见[本轮交接](../handoff/SafeFlow_模型计划安全接收_v0.1.md)。

## 限定验收记录（2026-10-06）

[检查方最终报告](../../../planning-intake-review-2026-10-06/REVIEW.md)确认固定 Rust 1.98.1 严格检查：199 普通、178 真实 PG、9 doc、18 独立边界、四种示例及最后 19 项永久接收回归均通过，副作用计数为零。本段引用独立结果，生成方自测另见交接；不累加重复运行。仅公共提案接收通过，真实模型、正式身份 / HTTP、任务归属 / 持久化创建、正式裁决 / Permit 和成员业务仍按上表待对接，BASE-06 父项仍部分完成。台账与网页同步由检查方另行复核。
