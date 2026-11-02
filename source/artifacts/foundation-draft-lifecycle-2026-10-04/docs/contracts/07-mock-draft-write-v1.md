# Mock 内存草案授权合同 v1

日期：2026-10-04。`MOCK_DRAFT_AUTHORIZATION_CONTRACT_VERSION = 1`。

本文保留创建 v1 的合同与交付记录。当前修改、取消使用独立 [生命周期授权 v2](./10-mock-draft-lifecycle-v2.md)，不改变这里的创建输入和合同版本；文中“仅创建/尚未接入”描述适用于 v1 入口。

这是用户授权增加的单进程模拟合同，仅允许创建未提交的内存转账草案。它与资金确认、强认证、资金提交及生产 `ExecutionPermit v1` 隔离，不替代数据库条件消费或业务事务。

## 唯一允许的副作用

工具名 `create_mock_transfer_draft`，动作 `CREATE_MOCK_DRAFT`，Schema 版本 1，`has_side_effect=true`。只创建，不注册修改、取消、扣款或资金执行入口。当前仅支持一个节点的保存计划；查询仍走原只读入口。

`RuntimeBootstrap::build()` 和普通 `Runtime::execute()` 保持拒绝写工具。应用必须显式使用 `build_mock_draft()`，并经 `execute_mock_draft()` 执行；不能通过给计划补 JSON 字段打开写权限。

## 可信确认与许可

1. 服务端生成请求，包含合同版本、请求编号、会话、任务、节点、工具、完整 JSON 参数、Runtime 计划版本及到期时间。请求不作为 LLM 的工具参数或公共 HTTP 请求开放。
2. 模拟服务端在用户明确确认“仅保存这些参数的模拟草案”后，调用 `record_explicit_confirmation()`。这是可信 UI/测试装配边界，不是模型推理、普通业务 Policy 或资金确认。示例直接模拟这次用户动作，不实现生产 UI 或认证。
3. `issue()` 只能接受服务端记录的确认对象。确认和 `MockDraftPermit` 均不实现 JSON 序列化/反序列化；私有对象身份必须与内存 Store 中的实际记录一致。`unregistered()` 构造的对象没有权限，字符串引用也不能替代记录。
4. 重复登记同一个未失效的请求返回原确认；重复签发返回同一许可对象，不延长有效期，不重置消费状态。

中央安全装配仍注册 Tool、同名 Policy 和 Profile，启动校验沿用原合同。SecurityKernel 在 PRE_TOOL/PRE_AUTH 仍返回 `READY_TO_CONFIRM` 与 `ExplicitConfirmation`。Mock Runtime 只接受 Green、该单一门禁且无额外风险/强验证要求的决定，再核验上述真实确认。Yellow、Red、Critical、HardDeny 与额外门禁均不能靠 Mock 确认放行。

本合同不执行 PRE_COMMIT，也不伪造 `confirmation_ref` 或 `auth_grant_ref` 来满足生产合同。生产请求的三项必需引用、Permit v1 和原门禁逻辑没有修改。新增非序列化调用分支 `ToolExecutionAuthorization::MockDraftWriteV1` 由本合同单独覆盖；原 `ReadOnly`/`Write` 语义及 Tool JSON 版本不变。原 `permits_side_effects()` 不将 Mock 许可视为通用写权限。

## 绑定、版本与撤销

- 执行和 Tool 使用均核验用户、原会话、任务、节点、工具、参数、Runtime 计划版本、请求编号、合同版本、到期及撤销状态；会话须仍有效且仍对应原用户。
- 当前确认版本的排他范围是 `(user, task)`。一个锁内检查版本并失效旧授权，新版本即使更换节点也覆盖旧版本。拒绝版本倒退，以及同版本的不同请求、参数或节点。许可本身仍精确绑定节点。
- 新确认会撤销该用户任务内所有旧确认/许可，包括正在等待银行的执行。银行查询结束后、同步业务保存前重新核验；核验与同步保存持同一个授权锁，避免撤销竞态。
- 撤销与保存有明确先后：撤销先取得锁则禁止保存；保存先完成则不会被撤销回滚。已经保存的草案也不会因新确认自动修改。本入口仍只创建，新请求再次创建已保存任务会失败。
- `revoke()` 撤销单个许可；`revoke_task()` 用于服务端取消/重规划任务并撤销该任务全部 Mock 授权，不代表调用业务取消接口。许可、会话失效也会拒绝旧缓存结果的访问。

## 一次执行与结果

授权 Store 在锁内将请求从 Pending 改为 Running。Tool 只有在 Runtime 持有该执行记录时才能声明一次使用；直接调用 Tool、重复 Tool 调用及新请求复用旧许可均拒绝。

同一已授权请求的并发调用等待同一次执行，完成后取得相同缓存结果。成功和失败都缓存为终态，不会自动重试银行或业务保存。中断执行记录为 `EXECUTION_INTERRUPTED_RESULT_UNKNOWN`，只允许读回核对，不静默重新保存。

Tool 调用逐风原服务保存，再从该服务读回核验；随后登记可信完整回执。Runtime 检查状态、引用、操作编号、合同版本及完整结果正文，POST_EXECUTE 的业务 Policy 再按可信回执读回实际草案。Failed、Processing、篡改回执、核验失败和审计失败都不能报告完成。

业务可能已保存而结果/审计阶段失败：保留草案，返回错误，后续同请求只返回缓存错误；`read_created_for_task()` 用于服务端核对。它不重提业务、不扣款，不是假装具备资金查单能力。

## 审计与模拟边界

原公共审计增加 `WriteRejected`、`WriteFailed`、`WriteReplayed`。正常写沿用计划、安全评估、ToolStarted、ToolCompleted、TaskCompleted；确认与许可记录保存在授权内存 Store。日志不记录会话或完整业务参数。审计不可用时返回错误；若连失败事件也无法保存，不承诺已经留下持久化审计。

全部记录仅存在当前进程，重启丢失；不保证多实例、防崩溃恢复、跨进程幂等或数据库事务。本授权不能带到资金工具、真实银行或生产系统。上线需要另行实现可信用户确认采集、生产认证、持久化授权/幂等/审计以及业务事务合同。

本 Store 不提供银行超时或后台到期定时器；到期在调用及保存边界核验。银行查询长期等待时，需查询完成或调用方取消才能结束原执行，不能声称已实现生产任务恢复系统。

测试入口：`crates/transfer/tests/runtime_draft_write.rs`，包含真实草案保存、并发、失败、回执核验、版本跨节点失效及银行等待时撤销的回归测试。
