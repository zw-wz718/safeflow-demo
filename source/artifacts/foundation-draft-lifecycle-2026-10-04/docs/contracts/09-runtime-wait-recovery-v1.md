# Runtime 只读等待点恢复合同 v1

范围：底座第四步 B；`READ_ONLY_WAIT_CONTRACT_VERSION = 1`。使用第四步 A 的三张表和迁移 1、2，不增加资金表或生产许可。`crates/runtime` 定义可选的只读检查与不可由外部构造的执行凭据；`crates/storage::RuntimeRecoveryService` 做数据库编排。依赖仍为 storage -> runtime -> contracts，没有 runtime -> storage 循环。

## 保存、恢复和继续执行

`execute_until_wait(session, plan, facts_version)` 只开始一个新 task：检查计划及所有工具声明为只读，保存 RUNNING 创建记录，复用现有 Runtime 执行器逐工具经过 PRE_TOOL/PRE_AUTH、实际 ToolResult 包络和 POST_EXECUTE 核验，遇到 UserDecision 后保存 WAITING_USER。已经存在的 task_id 统一拒绝再次初始执行，包括先前失败留下的 RUNNING、WAITING_USER、终态和有未知操作的任务；不隐式重试查询。

`recover_wait(session, task_id, expected_versions)` 只读取并重新核验，返回同一 WAITING_USER 任务与本次核验使用的真实 facts_version。它不调用 AgentTool::execute、不重做 POST_EXECUTE、不追加持久化审计、不推进节点。Policy 可以按现有只读 BankGateway 重新核查当前归属和可用性，因此“工具调用 0 次”不等于没有 Policy 查询。

本合同入口不实现 UserResponded 后继续执行。2026-10-08 新增的[合同 20](./20-persisted-read-only-runtime-continuation-v1.md)仅接受具备合同 19 持久认领证据的等待点；本合同历史任务不能凭旧等待摘要补出执行来源。恢复等待不等于继续执行，更不等于资金确认、认证、许可或提交。

## 可信输入和执行凭据

服务只能由可信后端用 `PgRepository` 和已通过 `RuntimeBootstrap` 装配的 Runtime 构造。身份只从该 Runtime 的会话解析器取得，没有第二个可独立注入的 resolver。后端决定 task_id、计划和事实版本；面向用户的输入不得直接映射第四步 A 的任意状态记录方法。

初始入口不接收 owner、TaskState、completed_nodes、工具结果、Permit。期望的 revision/plan_version/facts_version 只是恢复时的精确比对值，不是权威状态。计划的节点输入没有完成标记；范围为 1..256 个结构有效节点，包含 UserDecision。写工具或混合读写计划在任何查询前整体拒绝，包括用户等待节点之后的写工具。

`ReadOnlyPlanReview` 绑定当前计划、会话身份快照、事实版本和工具注册摘要，只是检查记录，不是执行许可。`VerifiedReadOnlyWait` 的字段仅 Runtime crate 可构造，没有 Deserialize/公共构造函数。完成节点仅从现有执行器在 ToolResult 验证、POST_EXECUTE 与 ToolCompleted 审计成功之后的实际执行集合产生。已验证结果只保存 SHA256 摘要，不保存可重放结果或旧授权。compile-fail 文档测试验证外部 JSON 无法构造执行凭据。

## 会话、版本和注册

本次可选入口要求 `SessionResolver::resolve_binding()` 返回一致的 `SessionBinding { user_id, binding_version }`，版本为正。可变 resolver 必须在失效、续期、重绑时递增版本，包括失效后又绑定回同一用户；读取用户和版本必须为一致快照。默认实现拒绝可选入口，不能退化成仅比较用户 ID。StaticSessionResolver 的共享映射不可修改，模拟绑定版本固定为 1。现有 resolve() 和普通 execute()/execute_mock_draft() 语义保留。

检查在所有安全/工具异步调用之后、等待时、数据库任务锁取得之后、写入/审计/提交前及提交后重复进行。同一用户的会话绑定版本变化也关闭本次成功路径。重新登录或重启后恢复从当前有效会话重新开始核验；不会把此前绑定快照恢复成旧确认或许可。这不是生产认证实现，真实服务端仍必须正确提供可撤销的版本化会话源。

2026-10-06 补齐创建事务：execute_until_wait 在 review 后将既有 `check_read_only_review` guard 直接传入 create_runtime_task；连接池、初次 INSERT（含重复 ID 等待）、锁、创建 / 拒绝审计和 COMMIT 等待前后均核验 review。数据库等待期间撤销或重绑在提交前被发现时，创建记录与审计回滚，不能仅在已提交后报错，更不会执行 Tool。checkpoint 使用同一 guard；内部 recover 路径也在各 SQL / 提交等待前后核验原绑定。已有执行 /恢复流程和公开参数保持，不新增资金、续跑或任务创建幂等执行权限。

复用[合同08存储会话复核](./08-recovery-storage-v1.md#2026-10-06存储会话复核补齐)的限制：guard 与外部身份源没有跨系统原子事务，提交等待中失效或响应丢失仍可能已经保存。提交后检查只拒绝当前返回，不能回滚已保存任务；必须按原 task_id / revision 核对，不自动重试初始执行。实现与永久真实 PG 证据见[本轮交接](../handoff/SafeFlow_持久化入口会话复核_v0.1.md)；本公共子项已[独立验收通过](../../../persistence-session-review-2026-10-06/REVIEW.md)，BASE-05 父项仍部分完成，正式身份、HTTP 与业务闭环未接通。

存储 facts_version 是 u64，当前安全合同是 u32；新入口只接受 1..u32::MAX，并用检查转换把实际版本传入每次安全请求，绝不截断或硬编码 1。事实版本由可信后端提供，仅是事实引用；不证明银行数据未变化，恢复的 Policy 会重新核查当前数据。

计划涉及的完整 ToolDefinition（含 action、schema、只读声明）、Policy 版本/兼容 schema、Security Profile 版本/最低级别一同求注册摘要。保存和恢复都重新校验当前工具/Policy/Profile 绑定及计划接受规则。缺注册、工具转写、schema/action/Profile/Policy 版本改变、当前 Policy 阻断均拒绝或要求重新审查。相同版本的新实现仍须遵守可信组件版本约定，摘要不检测未声明的实现代码变化。

## 数据库事务和恢复证据

保存从执行前创建记录取得原 task revision，不在查询结束后读取新 revision 来覆盖别人。锁内精确核对计划、事实版本、归属、RUNNING/空进度/无操作，并复用原状态机及整个恢复点校验。查询期间不持有数据库长事务；其他可信服务可以改计划/事实，旧执行保存时因版本冲突拒绝。

状态、进度和 CHECKPOINT_SAVED 审计同事务提交。审计的 `runtime_wait` 子对象保存合同版本、注册摘要、已验证结果摘要、完整 TaskRecord 摘要；它由封闭执行凭据生成。恢复必须找到与当前 task revision 精确对应的这条证据，检查合同版本、task 摘要和当前注册摘要。旧的手工 WAITING_USER 或计划改版后的等待点没有匹配证据，不能伪装为 Runtime 已执行进度。该证据用于一致性及来源边界检查，不是签名、防数据库管理员篡改的证明或生产资金回执。

恢复先按当前 owner 获取一致快照，再核验期望版本、计划/证据和当前 Policy；Policy 的异步核验结束后再次读回并比对 task、operations 和证据。并发计划/事实/进度改变关闭成功路径。返回的是当时已核对的等待快照，不是永久有效的执行授权。

只有无操作记录的 WAITING_USER/用户决策等待点适用此入口。RUNNING、终态及 WAITING_EXTERNAL 返回 NotWaiting，保留第四步 A 的恢复指令；SUBMITTED/PROCESSING 仍只能指向原 operation_id/原单号核对，不重提交。

## 失败和限制

工具失败、失败/错配结果、POST_EXECUTE 失败、Runtime 审计失败均不会保存成功等待进度。数据库更新、关键审计或可确认的提交拒绝导致事务回滚，返回错误。Runtime 内存 TaskPaused 只是内存执行记录，不表示数据库保存成功。

数据库提交响应丢失，或成功提交后身份/版本在最终检查中改变，可能已经保存但本次返回错误；应按原 task_id 读取核对，不能把错误当作可以重新初始执行的许可。查询与保存之间没有跨进程恰好一次协议；崩溃前查询可能执行过但仅留 RUNNING，后续需要人工/后续合同审查，不自动重跑。

本轮没有资金、自动续跑、生产 Permit/认证、账务、Outbox 或真实银行。业务草案、Mock Permit 和 Runtime 的一般内存审计仍未持久化；只把本次关键等待点及其验证证据写入既有数据库审计。转账业务目录保持原样。
