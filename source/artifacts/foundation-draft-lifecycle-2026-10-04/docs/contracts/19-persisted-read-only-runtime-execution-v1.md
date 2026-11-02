# 已存任务只读 Runtime 执行合同 v1

本合同属于 BASE-03/BASE-05/BASE-06 公共底座子项，版本常量为 `READ_ONLY_EXECUTION_CONTRACT_VERSION = 1`，迁移为 `0009_read_only_runtime_claims.sql`。依据原 PRD BASE-R02/R03/R04/R06 与当前用户授权，只接已有确认 Tool/Policy/直接参数合同。沿用合同 09 的执行器和恢复核验，旧 `execute_until_wait`、`review_read_only_plan` 保持等待点限定；不开放通用写、扣款或等待后的自动续跑。

2026-10-08：本合同产生的合法 WAITING_USER 可通过[合同 20 的显式继续入口](./20-persisted-read-only-runtime-continuation-v1.md)按原任务续跑；本合同 schedule/recover 和迁移 9 保持。续跑后的收据链由合同 20 recover 读取，不自动复用本合同的首次执行恢复入口。

## 创建与调度

`PlanningTaskCreationService` 仍只接受模型提案并创建初始 RUNNING 任务，不自动执行、不授权执行。新增 `TASK_CREATED.details.planning_origin` 保存会话摘要、绑定版本及当次注册/批准输入合同摘要；不保存原会话引用。这是可选元数据扩展。旧记录缺少该来源时，新调度明确返回 `RuntimeIntakeRequired`，不猜来源、不回填历史执行授权，旧创建/查询/恢复入口继续使用原合同。

可信后端以已装配 Runtime 和 `Vec<PreflightInputContract>` 构造 `RuntimeTaskExecutionService`。`schedule(session, task_id, RuntimeTaskVersion)` 的三个版本仅作比较，owner/完整计划/facts/completed/state 从数据库读取。外部不能提供结果、已完成节点、目标状态、许可或 Runtime 凭据。仅初始 RUNNING、无已完成节点和无业务 Operation 的任务可调度，原会话摘要及版本、创建时的注册摘要必须匹配。

数据库计划通过现有 PlanningIntake 严格有界 JSON、直接参数 Schema、工具白名单和计划门禁重新核验，然后扫描全计划 Tool/Policy/Profile，包括 UserDecision 之后的节点。任一写工具拒绝，Tool 调用为 0。实际执行沿用既有 Runtime PRE_TOOL/PRE_AUTH、银行资源归属、ToolResult 身份/成功/版本检查、POST_EXECUTE 及 Runtime 审计；可信公共状态输入不当作执行授权。

## 持久领取与结果

`runtime_read_claims.task_id` 是持久唯一键。task-first 锁内比较完整任务并插入 CLAIMED，领取审计同事务提交；提交响应失败或未知均不能进入 Tool。领取审计后复读任务、Operation、领取记录，成功提交后再次核对。重复/并发领取返回拒绝，不等待共享内存执行、不重新调用 Tool。

封闭 `VerifiedReadOnlyExecution` 只能由 Runtime 在真实结果及审计通过后生成，不能反序列化或外部构造。Storage 比较原任务快照、原会话、注册、领取号后，将任务 COMPLETED 或 WAITING_USER、已完成节点、领取 SETTLED、结果引用/摘要及检查点审计写入同一事务。只保存既有 `tool_name/operation_id/result_ref` 与结果摘要，不新增结果正文/脱敏语义。审计后重读任务、Operation、领取/回执及审计，提交后再次核对；失败不得返回内存中的成功。

明确 Runtime 错误在上下文仍有效时持久化 FAILED + 失败回执，调用者仍收到错误。计划/版本变化、会话重绑、注册变化或数据库/审计无法保存时保留原号 CLAIMED，需要核对，不能伪造失败或完成。提交响应未知可能已落库，调用者先查询原号，不能推导事务一定回滚。

领取不是业务幂等，也不声称 Tool 恰好执行一次：进程可能在领取后、Tool 中或落库前中断。CLAIMED 永不自动过期/重试；没有可靠证据的部分结果不补成成功。重复调用不再次执行，恢复只核对原 task_id。

## 查询与恢复

原 `TaskQueryService` 继续返回本人持久状态和受限恢复提示。新 `recover(session, task_id, RuntimeTaskVersion)` 使用当前合法本人会话，允许同一用户重新登录，并在所有数据库/策略等待前后检查本次绑定；不会要求用户持有旧会话。已完成/等待回执必须匹配任务摘要与同版本检查点审计；注册及参数重新核对，等待点复查当前 Policy，最后再次读库。全过程 Tool 调用为 0，不恢复 Permit、不执行用户决定后的节点。

尚未结算的领取返回 `RequiresOriginalTaskReview`，它区分不了仍在执行与进程中断。FAILED 返回已记录失败；历史来源缺失、回执缺失/不一致拒绝。新视图只供可信后端，含内部 TaskRecord；不能直接作为公开 HTTP/前端合同，公开展示继续使用合同 15 的受限视图。

## 边界与依赖

底座后续工作：公共 HTTP/TaskResponse、项目模拟环境的服务端会话与 Runtime 装配（负责人不高兴）。LLM 具体配置待用户提供，涉及业务语义的字段单独向对应负责人确认。资金、锁卡、账单分析、订阅与审计业务由各负责人提供；相关接入恢复条件为可核对的 Tool+Policy+字段/失败样例/调用例及明确合同版本。会话与 PostgreSQL 不构成跨系统原子撤销，最后同步复核到数据库提交之间存在原合同已说明的窗口；提交后检查可拒绝过时响应，不宣称消除该窗口。

永久测试位于 `crates/storage/tests/runtime_execution_cases/mod.rs`，复用既有模拟银行余额工具；底座隔离测试不代表其他成员业务联调通过。
