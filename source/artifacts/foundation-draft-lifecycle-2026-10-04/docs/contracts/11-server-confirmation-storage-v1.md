# 服务端确认记录持久化与核验 v1

`SERVER_CONFIRMATION_CONTRACT_VERSION = 1`。实现位于 `crates/storage`，依赖 PostgreSQL 16、现有任务/操作存储和版本化 `SessionResolver`。这是第 4 步剩余部分及第 5 步前置中的独立组件，不表示第 4/5 步整体完成。

依据：本次授权的 `confirmation-stage-2026-10-05.md`；本仓库总 PRD（README）6.2/6.3 的明确确认与改参数失效要求；底座 PRD `BASE-R03/R04`（来源为 `SafeFlow_完整项目直拷版_2026-10-02/docs/collaboration/prd/02_底座PRD.md`，该文档仍标待评审，只采用本次任务明确授权的范围）；公共合同 02/03/05/06/08。没有替其他负责人冻结业务接口。

## 接口与可信边界

`ConfirmationService::for_trusted_backend(repository, sessions, config)` 只供可信后端明确确认处理器装配。生产时钟来自服务端 `SystemTime`；`for_trusted_backend_with_clock` 供可信依赖注入和隔离测试，时间不在调用命令里。`ConfirmationConfig::new(Duration)` 必填正有效期，精度为微秒，没有产品默认值。

```rust
prepare(session, task_id, operation_id) -> ConfirmationRecord
read(session, task_id, confirmation_ref) -> ConfirmationRecord
record_explicit_confirmation(session, task_id, confirmation_ref, displayed_binding)
    -> ConfirmationRecord
verify(session, task_id, confirmation_ref, expected_binding) -> ConfirmationRecord
revoke(session, task_id, confirmation_ref) -> ConfirmationRecord
```

这些是 Rust 服务方法，不是 HTTP 接口或 LLM Tool。输入没有 owner、confirmed 布尔值、可信时间或任意状态字段；`ConfirmationBinding` / `ConfirmationRecord` 只有 Serialize，不提供输入反序列化。引用和回显绑定只是定位与比较数据，不能证明用户明确确认。可信处理器必须先确保这份快照已展示给用户，并核实针对该快照的明确动作，才调用 `record_explicit_confirmation`。不得把模型输出、客户端 `confirmed=true` 或“引用存在”直接映射成该调用。能调用服务或直接写 SQL 的代码仍属于信任边界，类型不是防恶意后端代码的认证机制。

准备时从已存 task/operation 生成绑定及 UUID v4 `confirmation_ref`，返回 PREPARED。调用方用同一快照展示参数，不能悄悄刷新内容后沿用用户旧动作。明确确认必须比较用户看到的原 binding、确认行 binding 与锁内当前 binding；任一不同都拒绝，不自动确认新版。读取 PREPARED 不等于已确认；verify 只接受当前有效的 CONFIRMED 行。

## 绑定与核验

绑定包含可信 user_id、task_id、operation_id、node_id、tool_name、task_revision、plan_version、operation_revision、facts_version、arguments_hash。原会话通过内部域分隔 SHA256 摘要及 binding_version 绑定，原 SessionRef 不保存。会话摘要和绑定版本不在返回或审计详情中输出；摘要不是公共授权。服务端应使用高熵、不复用的会话引用；摘要不提供低熵口令保护或生产认证。

每次调用先由 `resolve_binding` 获取用户和版本，锁定 task 后、写前、审计后及提交前再次比较同一会话。版本必须为正；旧 resolver 的默认实现拒绝。可变 resolver 在撤销、续期或重新绑定时必须递增版本，包括同用户重新绑定；多实例须共享可信、持久且不倒退的会话真源。本轮没有生产会话库。

准备、读取、确认、核验均检查：

- 当前 task 属于会话用户且存在；原确认属于同一原会话及绑定版本。同用户另一个会话也不能使用原确认。
- task 仅可为 RUNNING/WAITING_USER；operation 仅可为 DRAFT/READY，无业务提交引用。终态、SUBMITTED/PROCESSING、WAITING_EXTERNAL 或需原单对账的任务拒绝。
- 计划结构和恢复点一致，operation 对应当前 Tool 节点；操作 plan_version 等于当前计划版本，操作 facts_version 等于当前任务事实版本。
- 从当前计划 Tool 参数重新计算 SHA256，与 operations.arguments_hash 一致。嵌套浮点也拒绝；金额字符串不转换数值。
- 原 binding 精确等于当前 binding。task 数据库修订号也绑定，因此合法进度/状态变化同样要求重新准备与展示。不隐式修改计划或同步旧操作的事实版本。
- 服务端时间必须小于 expires_at，等号即失效；REVOKED 拒绝。检测到时间早于 prepared_at/confirmed_at 也拒绝。时间按 PostgreSQL 微秒精度保存和比较。

过时/过期是每次核验的动态拒绝，不由后台改写历史状态；即使历史行仍是 CONFIRMED，也不代表它当前可用。此服务没有可缓存或消费的执行权限。核验后数据库仍可能变化，后续执行须在其事务中再次核验，不能把本方法当业务写入锁。

## 重放与事务

使用既有 task-first 行锁，再锁确认行；既有计划、进度、操作更新也先锁 task。并发确认、撤销及状态改变按此顺序串行。确认不更新 task/operation，不使操作 READY。

自然唯一键为 task/operation/原会话摘要/会话版本/binding 摘要。同一快照重复准备返回原有效 PREPARED 或 CONFIRMED 行，不新建、不续期、不重落审计。已过期/撤销的同一准备拒绝，不能靠重新准备复活；刷新同一快照期限的产品规则不在本轮范围。

明确确认重放返回原 confirmed_at/expires_at，不重复审计；其后过期、撤销、会话或快照变化，重放也拒绝。重复撤销返回原 REVOKED 行及 revoked_at，不重复审计。

撤销仅是清理：原会话仍有效且版本一致时，可撤销已过期、绑定过时或 task/operation 已终止的历史确认。会话本身已撤销/换版则不能借此清理历史行，维护接口不在本轮范围。撤销不回滚已完成业务。

确认状态与底座自身原始关键审计同事务：CONFIRMATION_PREPARED、EXPLICIT_CONFIRMATION_RECORDED、CONFIRMATION_REVOKED。失败不产生成功事件；拒绝事务直接回滚，不另落持久拒绝事件，既有 PersistenceService 的 CHANGE_REJECTED 语义不变。写入、立即审计、提交时延迟审计失败全部回滚。提交连接中断/响应丢失可能已经提交，应按原 task/operation/ref 读取或原命令重放核对，不能认定“肯定未保存”。

新增迁移 `0003_server_confirmations.sql`，旧 0001/0002 内容及校验和不改；扩展既有 audit kind 约束、新增确认表、外键、状态/时间/摘要/版本约束及唯一键。`PERSISTENCE_CONTRACT_VERSION` 仍为 v1，确认组件另有独立版本；重复迁移保留旧数据。

## 错误与范围

跨用户、原会话不匹配、换版后的历史确认、错误 task/ref 或不存在记录统一 NotFound；会话解析失败、缺少版本绑定或调用期间变化返回 Session。过期、撤销、尚未确认分别为 ConfirmationExpired、ConfirmationRevoked、ConfirmationRequired；绑定不符为 ConfirmationBindingMismatch，也可能先返回 StalePlan、FactsConflict、Terminal、ReconcileRequired。数据库错误统一 Database，不输出 URL、SQL 参数或凭据。

这里只证明对已存底座操作快照的明确确认。arguments_hash 是存储摘要，**不是**带 resource_id / normalized_arguments / schema_version 的完整 OperationFingerprint v1，不证明银行事实、资源归属或业务规则正确。业务投影待负责人交付并确认，不能造资源补齐。

本确认组件不提供 AuthGrant、ExecutionPermit Store、许可消费、业务提交、扣款、查单或资金事务；不向 SecurityKernel 注入证据，不接通 Runtime 资金路径。2026-10-05 后续增加独立的[许可存储与事务内消费 v1](./12-execution-permit-storage-v1.md)及迁移 4，消费时在同一事务重新核验本组件已存确认；正式签发仍未启用，不改变本确认合同语义。普通 Runtime 写拦截、前序单进程 Mock 草案授权和内存存储保持原样，本组件不使它们持久化。没有生产确认 UI/HTTP 采集、角色权限/RLS、防 DBA 篡改、审计链或 Outbox。时钟、SessionResolver 和数据库代码必须可信；会话撤销不与 PostgreSQL 共享事务，本轮验证锁等待和写入等待后的重核验及回滚，不能冒充完整生产认证事务。

## 验证入口

`crates/storage/tests/confirmations.rs` 为永久真实 PostgreSQL 回归，默认 ignored；实际执行 `scripts/verify-confirmations.ps1` 才算运行。示例 `confirmation_record` 是公共合同快照的隔离测试，未注册或执行业务 Tool，明确动作由测试后端演示，不是用户界面已经接入。命令、实测结果及待对接清单见[本步交接](../handoff/SafeFlow_服务端确认记录持久化与核验_v0.1.md)。

## 2026-10-24 账单v2专用清理边界

[合同38](./38-billing-prepare-recovery-v2-candidate.md)的闭合abandon允许当前同owner会话清理自己的零尝试或有显式回滚证明的准备，即使原确认session已变化。它从持久根核原绑定，撤确认及ACTIVE许可并取消task/O、提交永久fence；不是通用revoke权限扩展，不返回旧ref/失权参数、不续期，不清理ADMITTED未知或撤回已保存反馈。原通用服务仍要求原session/版本。
