# Runtime 与安全公共合同 v1

合同版本：

- `AGENT_PLAN_CONTRACT_VERSION = 1`
- `PLANNING_RESULT_CONTRACT_VERSION = 1`
- `AGENT_TOOL_CONTRACT_VERSION = 1`
- `SECURITY_CONTRACT_VERSION = 1`

本合同冻结 Runtime、业务 Tool、SecurityKernel 和 DomainPolicy 之间的公共边界。具体 Registry、Runtime、Kernel、评分和业务实现属于后续阶段，不能反向修改这里的类型语义；需要修改时必须提升相应合同版本并更新合同测试。

`PlanningResult`冻结理解结果、可选`AgentPlan`和可选澄清问题的组合：`PASS`必须携带Plan且不能携带澄清；`CLARIFY`必须携带非空澄清且不能携带Plan。

## AgentTool

`AgentTool`是 Runtime 调用业务功能的唯一接口：

```rust
trait AgentTool: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    fn execute<'a>(
        &'a self,
        context: &'a ExecutionContext,
        authorization: ToolExecutionAuthorization<'a>,
        arguments: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>>;
}
```

`ToolDefinition`必须带合同版本、规范工具名、用途、输入JSON Schema、Schema版本、是否有副作用和业务动作。写Tool必须收到`ToolExecutionAuthorization::Write(&ExecutionPermit)`；类型化引用不能代替Permit Store和业务事务内的验证、条件消费。

2026-10-04 增加独立可选的 [Mock 内存草案授权 v1](./07-mock-draft-write-v1.md)。仅其单进程草案 Tool 使用非序列化的 `MockDraftWriteV1` 调用分支；上面的生产 Write/Permit v1 语义不变，不能将 Mock 授权用于资金 Tool 或通用写操作。

`ExecutionContext.authenticated_user_id`只来自服务端可信会话。Tool参数不是身份或授权真源。

## DomainPolicy

`DomainPolicy`与`AgentTool`使用同一个`tool_name`绑定，但职责独立：

```rust
trait DomainPolicy: Send + Sync {
    fn tool_name(&self) -> &ToolName;
    fn evaluate(...) -> ContractFuture<Result<Vec<SecurityFinding>, SecurityError>>;
    fn verify_execution(...) -> ContractFuture<Result<Vec<SecurityFinding>, SecurityError>>;
}
```

Policy只能读取受信事实并返回Finding；不得执行业务副作用、调用Tool、签发凭证、提交权重或生成最终Decision。Tool和Policy接口保持对象安全，供后续两个Registry保存实例。

## Plan接收合同

`PlanAcceptanceRequest`包含：

```text
contract_version、task_id、plan_version、plan
```

外层`task_id`和`plan_version`必须与类型化`AgentPlan`完全一致。`AgentPlan`及节点只表达计划结构；工具白名单、Schema、依赖图、上游引用、禁止身份字段和Tool/Policy绑定由后续`validate_plan()`确定性检查。

`PlanAcceptanceDecision`包含：

```text
contract_version、decision_id、task_id、plan_version、accepted、
policy_version、findings
```

Plan接收不创建`operation_id`，不读取余额或风险历史，不产生操作级风险分数。

## 操作安全请求与可信上下文

`OperationSecurityRequest`包含：

```text
contract_version、task_id、node_id、operation_id、facts_version、phase、
tool_name、plan_version、session_ref、normalized_arguments、arguments_hash、
可选idempotency_key、confirmation_ref、auth_grant_ref
```

允许的执行前阶段只有`PRE_TOOL`、`PRE_AUTH`和`PRE_COMMIT`。`facts_version`和`plan_version`必须大于0。`arguments_hash`为64位小写十六进制摘要。

请求中没有`user_id`字段，且反序列化拒绝未知字段。SecurityKernel必须使用`session_ref`解析服务端身份，再通过`SecurityContext::from_request()`构建Policy上下文。构建后：

- `SecurityContext`只包含`authenticated_user_id`，不再包含`session_ref`。
- 原始会话令牌、验证码和密钥不得进入Context。
- `PRE_COMMIT`必须带当前幂等、确认和强验证引用，缺少任一项即拒绝。

## Finding与Decision

`SecurityFinding`由CommonGuards或DomainPolicy产生，包含规则编号、策略版本、来源、类型、原因、证据、原始观察值、事件ID、去重键和可选时间窗口。Finding不能携带权重或最终分数。

Finding类型冻结为：

- `HARD_DENY`
- `NEEDS_INPUT`
- `REQUIRE_GATE`
- `RISK_SIGNAL`
- `INFORMATIONAL`

只有`EvaluatedFinding`可以由DecisionEngine附加`applied_weight`和`score_delta`。`SecurityDecision`是Runtime唯一可执行的操作级安全结论，包含：

```text
contract_version、decision_id、operation_id、facts_version、policy_version、
outcome、minimum_level、risk_score、risk_tier、required_gates、findings、expires_at_ms
```

`risk_score`范围固定为`0..=100`。`SecurityOutcome`继续使用三维状态合同中的权威定义；业务成功、失败和处理中状态不得放入安全Decision。

## POST_EXECUTE衔接

Tool调用后使用`PostExecuteRequest`和`ExecutionObservation`，由SecurityKernel返回`PostExecuteAssessment`。SecurityKernel解析可信身份后构建不含`session_ref`的`PostExecuteContext`交给DomainPolicy核验。

## 第一步完成定义

1. 三组公共合同均带版本常量并从`safeflow-contracts`根导出。
2. `AgentTool`、`DomainPolicy`和`SecurityKernel`接口可作为trait object使用。
3. Plan接收信封与类型化Plan不一致时拒绝。
4. 操作安全请求不能携带模型或客户端指定的用户身份。
5. `SecurityContext`只使用服务端解析的可信用户，且不暴露会话引用。
6. `PRE_COMMIT`缺少幂等、确认或认证引用时拒绝。
7. 合同格式检查和全部正反测试通过。
