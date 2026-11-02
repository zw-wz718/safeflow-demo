# 可信前置安全裁决存储 v1

2026-10-18 增补：中央评分已按 [计算/审计接入说明](30-central-risk-compute-audit-v1.md) 接入。下文历史“固定0分”和“拒绝全部观察值”限制仅保留背景；当前仅放行经过中央目录校验的有界离散事件计数。启用评分的 PRE_AUTH 按原操作/事实版本复用首条快照，配置变化拒绝复用和重新定价；其余既有请求号语义不变。

日期：2026-10-05。内部合同版本 `STORED_SECURITY_DECISION_CONTRACT_VERSION = 1`。冻结合同 02/03/05/06 的含义不变。

## 1. 来源与权限

`DecisionService::for_trusted_backend(repository, &Runtime, config)` 沿用 RuntimeRecoveryService 的可信后端装配方式。Runtime 只能由现有 RuntimeBootstrap 构建，实际使用 DeterministicSecurityKernel、启动绑定校验、已注册的 Tool、DomainPolicy 与安全 Profile。身份仅通过该 Runtime 的版本化 SessionResolver 取得，不额外接入另一个身份来源。

保存入口根据数据库里的 task/operation 构造请求，实际执行 Kernel 的 validate_plan/evaluate 和对应 Policy。Runtime 返回字段私有、不可反序列化的 VerifiedPreflightDecision，并绑定实际请求摘要及原会话版本；存储入口不接受外部 SecurityDecision、封闭结果、JSON、READY 布尔值或任意成功提供器。新请求摘要统一为 `serde_json::to_value(request)` 后的 JSON/SHA-256，旧持久化摘要规则不变。

所有记录固定标记 `evidence_kind=PREFLIGHT_ONLY`、`source_mode=TRUSTED_RUNTIME_V1`，仅支持 PRE_TOOL/PRE_AUTH。允许保存写工具的 READY_TO_CONFIRM、WAITING_AUTH、BLOCKED 等前置结论，但不执行 Tool、不推进 Operation READY，不修改 Task/facts_version，不等于已确认或已认证。ACTIVE/BLOCKED 仍是有效的拒绝评估记录。PRE_COMMIT 和 request_precommit_evidence 明确返回 DecisionSourcesUnavailable；正式 Permit 签发和资金入口不开放。

存储产品代码不依赖 MockBank，不按两个工具名白名单筛选。演示复用现有已确认银行查询；写工具回归复用已有 CreateMockDraftTool/Policy，不新增业务实现或成功插件。

## 2. 必填后端配置与直接输入合同

DecisionConfig 必填：正整数毫秒 TTL、1..80 位 ASCII 部署版本、1..256 个无重复 PreflightInputContract。部署版本、TTL、输入声明属于可信部署配置，不是客户端权限。TTL 无无限默认，示例 300 秒和测试 20 秒不是产品期限。

每个 PreflightInputContract 显式声明 tool_name、tool_schema_version、policy_version、projection_revision。必须与实际注册相符。它表示已确认的完整直接输入，不实现业务投影；参数摘要仍不是合同 03 的完整 OperationFingerprint。缺少输入声明、未注册工具/Policy/Profile、版本不符或未解析引用均拒绝。不得为了联调猜测未确认字段或登记一个虚假的合同版本。

v1 采用严格封闭的平面 JSON Schema 范围：根必须是 object，明确 properties、required 数组和 additionalProperties=false。字段支持 string/integer/boolean/null；支持 enum、字符串 minLength/maxLength/pattern、整数 minimum/maximum/exclusiveMinimum/exclusiveMaximum/multipleOf。所有已声明约束都校验，包括缺省的可选字段；类型错、未知关键字、无字段 type、空/重复或类型不符的 enum、畸形约束均拒绝。整数约束不得使用浮点，字符串长度按 Unicode 字符数计算。输入仍交由真实 Policy 验证业务规则。嵌套、组合、引用、浮点和未支持关键字留待明确合同，不忽略限制后继续。

pattern 使用 regex-syntax 成熟语法树解析并检查兼容范围，再由 regex 执行；不把 JSON Schema 改成 Rust 正则语义。支持的 `\d/\D` 固定为 ASCII 数字，`\w/\W` 固定为 ASCII 字母数字下划线；`\s/\S` 按 ECMAScript 空白及换行集合转换，包含 BOM/全角空格。字面量、范围、连接、选择、无标志的普通/非捕获分组和重复可用。点号、单词边界、标志、命名捕获、Unicode 属性、集合运算、八进制及未确认转义拒绝；含 pattern 的非 BMP 输入拒绝，避免 UTF-16 与 Unicode 标量差异。不是完整 ECMAScript 引擎。正则和自动机大小有限制，不能以全局关闭 Unicode 放宽现有银行输入。

大小限制：计划 JSON 64 KiB/256 节点，参数 4 KiB，schema/装配/裁决 JSON 各 16 KiB，字段 64 个、enum 64 项、pattern 1024 字节；配置、标识、版本、期限加法和 u32 facts_version 转换检查溢出。

## 3. 绑定和追溯

绑定原 user、原 session 摘要及 binding_version、task_id/task_revision/plan_version、operation_id/operation_revision/facts_version、node_id、tool_name、arguments_hash；完整 TaskRecord 与全部 OperationRecord 的摘要防止未增 revision 的内容变化和同级操作变化。任务/操作的处理中、终态及不一致恢复点不能产生新有效裁决。

保存冻结 SecurityDecision 的原 decision_id、contract_version、policy_version、outcome、minimum_level、risk_score/risk_tier、required_gates、findings、原 expires_at_ms 及摘要，另存真实计划审查摘要。持久 stored_decision_id 是随机 UUID；`(source_instance_id, source_decision_id)` 追溯原 Kernel 序号，服务/进程重建不会覆盖另一进程的 decision_N。

装配摘要包含实际注册的 ToolDefinition（含输入 schema）、DomainPolicyDefinition、安全 Profile，Kernel/启动/前置评估/输入校验代码 SHA-256、正则实现版本、TTL、输入投影声明及部署版本摘要。代码换行规范为 LF。每次提交前重新核对当前注册信息。存储不能反射 Policy 的内部依赖或银行数据；策略实现、事实源或配置发生语义变化时，可信部署方必须更新 policy_version 和/或 deployment_revision，并重新评估，不能保留旧版本宣称未变。底座 facts_version 也必须由事实更新方正确维护。

不保存原 session、原参数、密钥或凭据。仅保存安全的版本/原因/来源等元数据及必要摘要；包含 observed_value、任意 evidence_refs/event_ids/dedup/window 的裁决当前拒绝持久化，保留准确原输出而不静默脱敏成另一份裁决。finding 数量上限 16、门禁上限 2，标识有长度/控制字符校验。注册 schema/描述必须是可信公共元数据，不能把秘密写进注册配置。

## 4. 后端 API 与重试

| 方法 | 含义 |
|---|---|
| evaluate_and_save(session, task, operation, request, phase) | 从数据库构造并实际评估；相同原请求返回同一记录、不重复 Policy/审计、不续期 |
| read(session, task, id) | 读取并核验当前有效性；不会重新执行 Policy 或 Tool |
| verify(..., expected_binding) | 在 read 核验上逐字段比较期望值；期望值不是可信来源 |
| revoke(...) | 原会话后端主动撤销；幂等，已结束记录不会复活 |
| invalidate_if_stale(...) | 服务从当前完整快照、装配或时间计算原因；不接受调用方理由 |
| request_precommit_evidence(...) | 当前始终不可用 |

request_id 全局唯一。相同号的用户/会话/任务/操作/阶段/快照/装配冲突拒绝；同任务并发原请求串行返回同一结果。有效期始于实际评估开始，取配置期限与原 Kernel 期限较早者，重试不延长。到期等号拒绝；过期观察返回错误也持久推进时间围栏。REVOKED/INVALIDATED 不可复活。同号不能重放；新请求产生新的前置记录，可与其他未撤销记录共存，不自动替代、延长或恢复旧终态。新评估仍需完整身份和当前快照核验，不能绕过后续确认/认证。

失效原因只允许 SNAPSHOT_CHANGED、ASSEMBLY_CHANGED、EXPIRED。cleanup 可以结束旧记录，但绑定原用户/会话并检查本次事务开始时的当前快照，审计期间再次变化则回滚。旧快照已经终态或失配不会被恢复成有效状态。

## 5. 事务与时间边界

复用 task-first 锁序，事务内锁原任务与裁决，最终锁定当前操作集合并重读完整快照、裁决、装配。记录/撤销/失效及各自审计一起提交；SQL/审计立即错误、延迟 COMMIT 错误和提交前取消不会报告成功或保留半成品。异步评估、SQL/审计和最后时间观察后核验原会话版本，最终有效期检查覆盖最后观察。

schema 级持久时间高水位在独立短事务中更新。先等围栏行锁，再读取可信时间；倒退或时间存储失败拒绝。单独连接池不持任务锁，避免主事务占满连接后自锁。独立围栏只推进观察时间，不提交裁决/审计；因请求失败、过期、主事务回滚或进程恢复不能把时间退回。

这是同 PostgreSQL 16、可信后端配置及 SessionResolver 的本地公共机制，不是生产分布式时钟/身份服务。外部会话系统与数据库 COMMIT 没有跨系统原子事务，最后本地检查之后的撤销仍有生产协调边界；DBA 改写全部数据/摘要、备份回滚和伪造部署版本不在此机制的防护承诺内。COMMIT 已发出后的进程终止/网络响应丢失，结果可能未知，应按原 request/id 查记录，不能称为确定回滚或换号重试。审计为底座原始事件，不冒充风星龙的正式哈希链审计实现。

## 6. 未决依赖

现有 Kernel 使用固定 risk_score=0 和原 expires_at_ms=i64::MAX；本轮没有代写完整 DecisionEngine 的权重、规则或正式审批语义。外层记录 TTL 只限制存储有效性，不能修补 PRE_COMMIT 所缺的事实。

正式 PRE_COMMIT/Permit 真源仍依赖完整业务投影、可信正式 AuthGrant/认证策略、已确认门禁事实和业务事务/幂等/Outbox/查单合同。详见交接文档的待对接表。收到实际合同及来源并经确认后，才设计正式证据和接入；不得将本记录或模拟认证转换成 VerifiedPermitSnapshot。
