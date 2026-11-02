# 第三步：启动绑定校验与绕过防护

## 目标

应用对外提供服务前，必须证明每个业务Tool、DomainPolicy和中央最低安全配置形成唯一且兼容的绑定。任何缺失、孤立、重复或版本冲突都必须导致启动失败，不能留到用户请求期间再猜测或降级。

## 三份独立真源

启动校验同时比较：

1. `ToolRegistry`中的`ToolDefinition`：`tool_name`和`schema_version`。
2. `DomainPolicyRegistry`中的注册元数据：`tool_name`、`policy_version`和兼容的Tool Schema版本。
3. `ToolSecurityCatalog`中的中央配置：`tool_name`、批准的Tool Schema版本、批准的Policy版本和最低安全等级。

功能Policy不能自行降低中央最低等级，也不能自报未经批准的版本。

## 启动失败条件

- Tool缺少同名DomainPolicy。
- Tool缺少中央安全Profile。
- DomainPolicy指向未注册Tool。
- 中央安全Profile指向未注册Tool。
- Tool、Policy或安全Profile同名重复注册。
- Policy声明兼容的Schema版本与Tool不一致。
- 中央批准的Schema版本与Tool不一致。
- Policy注册版本与中央批准版本不一致。

`RuntimeBootstrap::build()`是Runtime的唯一公开装配入口。`Runtime::new()`和`DeterministicSecurityKernel::new()`仅在crate内部可见，因此外部应用代码无法通过直接构造Runtime绕过启动校验。

## 运行时防护

启动校验不替代每次执行时的SecurityKernel：

- Runtime仍然在固定阶段调用SecurityKernel。
- Plan没有安全节点也不能跳过Policy。
- DomainPolicy返回`HardDeny`时，Decision为`BLOCKED`，Tool调用次数为0。
- 中央最低等级高于Policy结果时采用中央等级；测试中的Red最低等级强制进入`WAITING_AUTH`，Tool调用次数为0。
- 运行中意外缺少Tool、Policy或安全Profile仍然Fail Closed。

## 完成定义

1. 所有Runtime实例只能由通过校验的`RuntimeBootstrap`创建。
2. Tool、Policy和安全Profile形成一一对应集合。
3. Tool Schema版本、Policy兼容Schema版本和中央批准Schema版本一致。
4. Policy注册版本与中央批准版本一致。
5. 每个Tool存在中央最低安全等级，且运行时不能被Policy降低。
6. 缺失、孤立、重复和版本冲突测试全部导致启动失败。
7. 绕过测试证明HardDeny时Tool调用次数为0。
8. 全工作区格式检查和`-Dwarnings`测试通过。

权威测试位于`crates/runtime/tests/step_three_bindings.rs`。

## 2026-10-06：本人任务公共查询装配

新增 `crates/storage::TaskQueryService::for_trusted_backend(repository, assembly.runtime())`，复用已经装配的 Runtime 的版本化会话源。它不执行注册的 Tool / Policy，不进行安全评估或自动恢复，只在 PostgreSQL 只读一致快照中投影本人任务 / 操作状态和记录版本。当前会话绑定在每次数据库异步等待后及返回前复核；未知任务与他人任务均 NotFound。

实现及生成方测试见[内部查询合同15](../contracts/15-task-progress-query-v1.md)和[本轮交接](../handoff/SafeFlow_任务进度查询与安全范围视图_v0.1.md)。本子项已[独立验收通过](../../../task-query-review-2026-10-06/REVIEW.md)，本段仍仅为实现说明，独立证据以检查报告为准。BASE-05 父任务仍部分完成，正式 HTTP、身份传输、当前安全裁决、业务原号查单和审计视图待对接；返回 `CURRENT_DECISION_NOT_PROVIDED` 不构成放行。

## 2026-10-06：模型计划安全接收装配

`PlanningIntakeService::for_trusted_backend(assembly.runtime(), gateway, approved_contracts)` 复用同一 Runtime、版本化会话及三份绑定，批准合同沿用 `PreflightInputContract` 的版本和直接投影定义。原始 `accept_json` 与强类型 `plan` 走同一完整结果 / 参数 / Kernel 结构校验；模型调用至多一次。已验收 `decision_input.rs` 内容不改，从 crate 级同一模块同时供前置裁决和接收使用，不复制或放宽参数校验。

期待的 task_id / plan_version 必须由可信后端提供；本期不查询数据库任务归属或授权旧任务修改。返回提案不构成安全裁决或 Permit，不保存 / 执行业务。会话等待后及返回前复核，Schema / 注册漂移拒绝。旧 PlanningService、MockLlm 和 Runtime 执行接口保持原样，现有写拦截、确认和 PRE_COMMIT 边界不变。合同及示例见[内部合同 16](../contracts/16-model-planning-intake-v1.md)和[交接](../handoff/SafeFlow_模型计划安全接收_v0.1.md)，本公共子项已[独立验收通过](../../../planning-intake-review-2026-10-06/REVIEW.md)；BASE-06 父项仍部分完成。

## 2026-10-06：持久化会话装配补齐

PersistenceService 原六入口参数保持，但 resolver 必须提供非零、可撤销的版本化绑定快照，默认 resolve-only 实现拒绝。服务把同一用户 / 绑定版本的同步 guard 注入真实 repository 事务，覆盖连接池 / SQL / 幂等 / 拒绝审计和 COMMIT 的等待前后。RuntimeRecoveryService 新建任务直接复用已装配 Runtime 的 review guard，后续 checkpoint 和读取同样守护；没有第二个身份源或业务执行器。

参见[合同08](../contracts/08-recovery-storage-v1.md)、[合同09](../contracts/09-runtime-wait-recovery-v1.md)与[交接](../handoff/SafeFlow_持久化入口会话复核_v0.1.md)。仅公共存储会话安全子项已[独立验收通过](../../../persistence-session-review-2026-10-06/REVIEW.md)，BASE-05 父项仍部分完成；最后同步检查与外部撤销并非跨系统原子事务，提交窗口失效或响应丢失仍须按原编号核对。不启用正式身份 / HTTP、资金、Outbox、自动续跑或成员业务。

