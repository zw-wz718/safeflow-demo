# PostgreSQL 恢复点持久化合同 v1

范围：底座第四步 A；`PERSISTENCE_CONTRACT_VERSION = 1`。仅保存/读取恢复点，复用 `AgentPlan`、`TaskState`、`OperationState` 和公共 ID。实现为 `crates/storage` 的 `PgRepository` 与 `PersistenceService`，目标数据库 PostgreSQL 16。

第四步 B 在这些存储接口之上增加独立可选的 [Runtime 只读等待点恢复 v1](./09-runtime-wait-recovery-v1.md)。旧的可信后端状态记录接口仍不是面向客户端的执行入口；手工写入 WAITING_USER 不等于通过了 Runtime 的查询和结果核验。

## 可信调用边界

`PersistenceService::for_trusted_backend` 只能由服务端装配，注入现有 `SessionResolver`。六个入口均从 `resolve_binding()` 取得同一 user_id + 非零 binding_version 快照，不回退 `resolve()`；事务等待后重复核验原绑定，不接受客户端 owner、初始状态、数据库修订号或任意 JSON 状态覆盖。新任务状态固定 RUNNING、新操作状态固定 DRAFT；后续用已有状态机事件计算下一状态。更改命令没有 `Deserialize`，不注册成 LLM Tool，也不是 HTTP 接口。

计划和进度来源必须是可信服务端已经核对的记录。storage 校验结构与一致性，但不代替 SecurityKernel 的计划/工具接受，也不能核实业务人员填入的“门禁已满足”“银行已提交/已成功”是否为真。这些操作事件仅允许可信业务适配器依据已验证事实调用，不能直接映射任意用户消息或客户端状态。单进程内有能力调用服务或直接 SQL 的受信任代码仍属信任边界；类型本身不是防恶意服务端代码的认证机制。

持久化 READY、SUBMITTED、PROCESSING 等状态不构成确认、认证、Permit 或执行许可。尤其不能通过本服务把 DRAFT 改成 READY 后调用资金 Tool。门禁、许可消费、银行幂等、账务与资金事务仍未实现。第四步 A 不包含 Runtime 集成；第四步 B 仅集成只读查询后等待点的保存与重新核验，不自动续跑，Runtime 的现有写拦截和 Mock 显式确认要求不变。

## 三张表

| 表 | 保存内容 |
|---|---|
| `tasks` | owner、TaskState、数据库 revision、完整 AgentPlan/plan_version、facts_version、恢复点、已完成节点、创建内容摘要 |
| `operations` | 原 operation_id、task_id/node_id 关联、OperationState、revision、plan_version、facts_version、参数 SHA256、原业务单号 |
| `audit_events` | 独立 UUID event_id、数据库 sequence、任务/操作关联、当前修订号、关键事件类型、参数摘要/状态信息、数据库时间 |

金额仍存放在计划 JSON 中的字符串，不转浮点；参数摘要由已存计划的 Tool 参数规范 JSON 派生，不接受调用方提供摘要。审计记录尽量仅保存摘要/状态/引用，不复制业务参数。SQL 和 migrations 在 storage 内，业务模块不执行 SQL。

首次创建可以使用服务端已有 TaskId/OperationId，也提供 UUID v4 生成函数；数据库唯一键确保重启后冲突 ID 不会覆盖原记录。相同 owner、任务 ID、初始计划和初始 facts_version 的重复创建返回当前记录，不重置进度，不追加创建审计；不同创建内容拒绝。一个任务/节点只能关联一个操作，不允许为了重试分配第二个操作。

## 更新和事务

所有写入先锁定 owner 对应的 task 行。客户端跨用户和不存在记录统一返回 NotFound，不读出计划/操作/审计；无效会话返回 Session。操作更新也先锁任务，保证任务、操作及恢复点遵循同一锁顺序。读取用 task 的 FOR SHARE 获取一致的任务/操作/审计快照。

`revision` 是数据库并发版本，不等于 `plan_version` 或 `facts_version`。checkpoint/replan/create-operation 要求当前 task revision，操作事件要求当前 operation revision；并发相同版本只有一个变更胜出。状态与进度更新、操作关联以及关键审计在同一事务提交；审计插入失败或数据库拒绝提交（例如延迟约束失败）则全部回滚。若提交时连接中断/响应丢失，则返回错误且可能已经提交，应按原 ID/revision 读回核对，不能直接当作失败重做。业务校验失败在锁内写 CHANGE_REJECTED，再返回错误；若拒绝审计本身失败，返回 Database，不报告已持久化成功。跨 owner/不存在任务不向他人的审计写入事件。

### 2026-10-06：存储会话复核补齐

创建、恢复读取、checkpoint、replace_plan、create_operation、record_operation_event 的公开参数保持不变；内部 repository 强制接收可信服务的同步 guard，包含 Runtime 共用的创建 / 读取 / checkpoint 路径。连接池获取、任务行锁、重复 ID 的 INSERT / 行锁等待、操作读取、更新和成功 / CHANGE_REJECTED 审计等数据库等待前后核验，包含 SQL 失败；所有 COMMIT 前以及 COMMIT 等待后返回前核验。相同内容创建、相同计划和相同操作的幂等返回也不能跳过 guard。

会话撤销、换用户、同用户续期 / 重绑均使旧请求失效；提交前检测到失效返回固定 Session，不向数据库保存当前事务的变化或拒绝审计。未提交的 sqlx 事务 drop 后回滚；请求取消同样丢弃事务，不能恢复该请求继续写入。跨用户 / 不存在统一 NotFound，无版本 / 零版本 / resolver 错误统一 Session，不回显会话或 resolver 诊断。

**兼容性收紧**：以前只实现 resolve 的服务端 resolver，即使能解析同一用户，现在六个入口也会拒绝。必须实现正版本 resolve_binding，用户和版本须来自同一一致快照；撤销 / 续期 / 重绑，包括撤销后绑定回同一用户，必须推进版本。StaticSessionResolver 现有固定版本 1 继续兼容。方法参数、数据库合同版本和 migrations / Cargo 锁文件不变，没有启用正式身份。

guard 与独立身份源不在同一数据库事务里，最后同步检查到 COMMIT 完成之间没有跨系统原子撤销保证。COMMIT 响应丢失或等待中身份变化，可能已经保存但返回 Session / Database；提交后检查不能撤销已完成事务。须从当前有效会话按原 ID / revision 读回核对，不自动重试、不换新 ID，不把持久状态当授权。永久 PG 回归明确检查提交前回滚及提交后失效的不同结果，见[本轮交接](../handoff/SafeFlow_持久化入口会话复核_v0.1.md)。本公共存储会话复核子项已[独立验收通过](../../../persistence-session-review-2026-10-06/REVIEW.md)，仅记入 BASE-05 子项；父项仍部分完成，正式身份、HTTP 与业务闭环未接通。

计划版本不能倒退，同版本不同 AgentPlan 拒绝，同版本相同内容且 facts_version 相同是无副作用读取。更高计划版本仅允许在尚无操作记录时替换；已完成节点定义不能改变，进度不能倒退。本切片不做业务草案改版、旧授权失效或完整多节点计划恢复。facts_version 必须为正且不倒退，它是可信事实版本引用，不是权限或事实正确性的证明。

非终态才允许按合同转换，终态不再接受变更。已完成节点不得重复、未知或缺依赖；存在操作的节点仅在操作 SUCCEEDED 后可记录完成；COMPLETED 要求全部节点完成且恢复点 FINISHED。恢复点必须指向当前计划里未完成且依赖完成的节点，WAITING_USER 必须指向用户决策节点。

新建操作在锁定任务后、INSERT 前校验包含该 DRAFT 的完整任务/操作恢复点，因此不能给已完成 Tool 节点新增 DRAFT。拒绝只追加 CHANGE_REJECTED，不改变原 task revision、进度或操作，原恢复点仍可读取。若该节点已有相同 operation_id 与参数摘要，仍在当前 task revision 校验通过后幂等返回原操作，包括已完成节点的原 SUCCEEDED 操作；不会用临时 DRAFT 覆盖它，也不追加创建审计。

## 未知结果的恢复

可信适配器记录 SUBMISSION_RECORDED 时必须带原业务单号。本服务在同事务把任务变为 WAITING_EXTERNAL、恢复点绑定原 operation_id。SUBMITTED/PROCESSING 时不允许创建新操作、重提交、改成普通节点恢复或把任务结束；原业务单号一旦记录不得更换。OutcomeUnknown 保持同一个 operation_id 和原单号。

恢复输出 `RecoveryDirective` 只有 AwaitUser、ReviewNode、ReconcileOriginal、ReviewRecordedResult、Finished，没有 Execute/Submit。SUBMITTED/PROCESSING 只能返回 ReconcileOriginal；可信查单结果写成终态后，先返回 ReviewRecordedResult，由后续阶段处理用户展示或下一节点。本轮没有查单适配器或自动执行。

## 迁移和限制

版本 1、2 的 SQL 随二进制嵌入，通过 sqlx Migrator 锁、校验和及迁移表依序应用；重复执行不破坏数据。迁移版本不等于公共 `PERSISTENCE_CONTRACT_VERSION`，合同仍为 v1。`0001_recovery_points.sql` 保持原内容及校验和，`0002_plan_identity_version.sql` 增加约束：plan 必须为 JSON 对象，task_id 必须为 JSON 字符串且匹配行身份，plan_version 必须为 JSON 数字且匹配行版本，整体 CHECK 必须为 TRUE，缺字段或 JSON null 不能借 SQL NULL 放行。原版本/整数范围约束继续生效。

升级也检查已有行。旧 schema 若已保存非法计划，版本 2 迁移事务失败，返回 Migration，不悄悄重写或删除旧数据，也不把版本 2 记为已应用。应由可信维护者先核查原记录再处理，不能绕过约束继续运行。永久 PostgreSQL 回归覆盖合法旧数据升级、版本 1 校验和保留、重复迁移及非法旧数据拒绝。

schema 名称只接受 63 字符内的小写 ASCII 名称，拒绝 public、pg_*、information_schema；所有值用绑定参数。连接字符串只从服务端配置加载，错误不回显 URL、密码或 SQL 参数。

这不是生产资金数据库：没有 Permit/认证/资金幂等表、账户账务或 Outbox；数据库管理员仍能改数据，尚未提供 RLS、最小权限分角色、审计哈希链或不可篡改存证。提交响应丢失时本服务也不提供完整命令幂等协议，调用方应先按原 ID/revision 读取核对，不把本轮事务回滚测试当资金幂等证明。上一轮 Mock 草案、授权和 Runtime 内存审计仍在内存，重启会丢失，不会因增加这三张表而持久化。

2026-10-05 后续增加独立的[服务端确认存储 v1](./11-server-confirmation-storage-v1.md)及迁移 3。该组件只确认已存底座操作快照，不修改本合同的状态记录语义，不启用资金执行；旧迁移 1/2 保持原内容与校验和。完整业务指纹与确认采集仍待对接。
