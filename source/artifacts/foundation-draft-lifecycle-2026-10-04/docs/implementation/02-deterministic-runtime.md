# 第二步：确定性 Runtime 内存闭环

## 目标

使用固定`PlanningResult`和`MockLlm`证明以下调用链可以在不接入真实模型、数据库和写业务的情况下确定性运行：

```text
用户输入
→ PlanningService
→ MockLlm（恰好一次）
→ PlanAcceptance
→ Runtime
→ Tool Registry
→ SecurityKernel
→ DomainPolicy Registry
→ Mock DomainPolicy（PRE_TOOL、PRE_AUTH）
→ Mock AgentTool（只读）
→ POST_EXECUTE核验
→ Audit
→ Task COMPLETED
```

## 已实现

- `safeflow-runtime`独立crate。
- `PlanningResult`、`Understanding`和`MockLlm`固定规划结果。
- Tool Registry与DomainPolicy Registry独立注册、查找和重名拒绝。
- `DeterministicSecurityKernel`执行Plan接收、服务端会话解析、Policy调用、Decision生成和结果核验。
- Runtime按依赖选择就绪节点，只通过SecurityKernel进入安全检查。
- `PRE_TOOL`和`PRE_AUTH`均通过后，只读Tool才允许调用。
- Tool返回后转换为`ExecutionObservation`，通过SecurityKernel完成`POST_EXECUTE`核验。
- 内存Audit按顺序记录Plan接收、安全评估、Tool调用和任务完成。
- 缺失DomainPolicy时Fail Closed；第三步已经把该检查前移到启动阶段。

## 信任边界

- Runtime执行入口只接收`session_ref`，不接收调用方指定的`user_id`。
- Runtime和SecurityKernel共享服务端`SessionResolver`，分别取得可信用户身份。
- Planning Service只向MockLlm提供Tool Registry中的`ToolDefinition`，不暴露DomainPolicy。
- Runtime不直接调用DomainPolicy Registry或Policy；只有SecurityKernel可以调用。
- 第二步拒绝执行所有有副作用Tool，不构造虚假的Permit或事务。

## 暂不实现

- 真实LLM API和结构修复。
- 写Tool、确认、强验证和ExecutionPermit签发。
- PostgreSQL、事务、幂等、任务持久化和重启恢复。
- PolicyCatalog、正式评分权重和生产DecisionEngine。
- PostgreSQL、持久化任务恢复和生产级策略目录。

## 完成定义

1. 固定PlanningResult由MockLlm返回，且每个请求只调用一次。
2. Planning Service提供当前ToolDefinition集合，不暴露DomainPolicy。
3. Runtime依次经过Plan接收、PRE_TOOL、PRE_AUTH、Tool执行和POST_EXECUTE。
4. Mock AgentTool只调用一次，并收到服务端会话解析出的用户身份。
5. Mock DomainPolicy记录两个执行前阶段和一次结果核验。
6. 审计序列完整，任务进入`COMPLETED`。
7. 缺失Policy时Fail Closed，Tool调用次数为0。
8. 全工作区格式检查和`-Dwarnings`测试通过。

权威集成测试位于`crates/runtime/tests/step_two_runtime.rs`。
