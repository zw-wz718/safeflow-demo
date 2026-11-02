# ExecutionPermit 存储与事务内条件消费（v1记录兼容门禁存储v2）

初版依据 `permit-stage-2026-10-05.md` 和公共合同 02/03/05/06/08/11；2026-10-07追加门禁表示兼容，当前 `PERMIT_STORAGE_CONTRACT_VERSION = 2`。实现仅在 `crates/storage`，目标 PostgreSQL 16。独立的 `storage_version` 区分旧记录1与新记录2；冻结的 `ExecutionPermit`、其数据库 `contract_version=1`、`OperationFingerprint`、三维状态及其版本均不改。本组件是底座自有存储/事务机制，未完成安全签发流水线或业务提交。

## 可信来源与未启用入口

`PermitStore::for_trusted_backend(repository, sessions, config)` 仅由可信服务端装配。身份及绑定版本来自 `SessionResolver::resolve_binding`，生产时间来自服务端 SystemTime；时钟注入仅限可信后端依赖与隔离测试。`PermitConfig::new(Duration)` 必填正整数毫秒 TTL，无默认产品期限。

正式请求 `request_issuance(session, task)` 在核对会话后始终返回 `PermitIssuanceUnavailable`。当前 SecurityKernel 的 READY_TO_EXECUTE 结论没有完整的可信认证、持久裁决及业务投影证据，不能独立作为签发真源。READY 状态、确认记录、AuthGrantRef/DecisionId 字符串存在都不会启用该入口，也不会自动改操作状态。

`save_verified(session, &VerifiedPermitSnapshot)` 是封闭的底层存储原语。VerifiedPermitSnapshot 的字段全部私有，没有 Deserialize、公开构造器或生产工厂；当前生产调用方无法生成它。只有 `#[cfg(test)]` crate 内子模块能依据公共合同构造隔离 fixture。没有测试插件、假认证提供器、业务投影器或 Tool 注册。以后接入真正的安全签发器，须由底座核实各来源及有效期、撤销规则后另行实现内部工厂，不把此类型改为客户端请求 DTO。

## 强验证门禁表示（存储v2）

明确确认与强验证是不同门禁。是否要求强验证由已确认Profile/Policy及可信金额、累计、风险等事实经实际Kernel决定，不由本存储根据转账金额猜等级，也不将全部写操作升级。写操作仍要求明确确认；必要策略/事实缺失不能记为“不要求”。本轮仅表示已核验来源的结果，不新增裁决器或启用资金。

封闭快照的私有 `required_gates` 和 `authentication` 必须一致。后者是封闭枚举 `NotRequired` 或 `Verified { grant_ref }`，不是可由HTTP/LLM传入的可选字符串/布尔值。不要求分支无认证引用；要求分支只有未来可信签发器实际核验本笔证据后才能构造Verified。当前没有该工厂，测试fixture不证明认证完成；grant字符串存在本身不是证据。

| 存储版本 / authentication_state | required_gates                                      | auth_grant_ref | 语义                                                                                    |
| ------------------------------- | --------------------------------------------------- | -------------- | --------------------------------------------------------------------------------------- |
| 1 / LEGACY_REQUIRED             | SQL NULL                                            | 必填           | 原有记录/旧写入保留原语义；没有补写虚构的历史门禁，不重新解释为无需验证或认证已重新核验 |
| 2 / NOT_REQUIRED                | `["EXPLICIT_CONFIRMATION"]`                         | SQL NULL       | 封闭核验快照确认该笔无需强验证；不能用缺省或缺证推导                                    |
| 2 / VERIFIED_REQUIRED           | `["EXPLICIT_CONFIRMATION","STRONG_AUTHENTICATION"]` | 必填           | 表示要求强验证且封闭来源已核验本笔证据；引用和门禁一起冻结                              |

门禁顺序采用现有Kernel的规范顺序。未知版本/状态、SQL NULL或JSON null门禁、畸形类型、缺确认、重复/未知/矛盾门禁、要求验证却缺引用、不要求却带引用均拒绝。数据库组合CHECK使用 `IS TRUE` 防止NULL绕过，Rust读回另行核验，不默认NotRequired。新v2记录及封闭快照拒绝合同13的 `sim_grant_` 隔离凭证引用；不改旧记录原语义。经批准、绑定本笔并进入正式安全链的项目模拟交易验证可在后续实际接入，不要求真实短信，但本轮没有接入该链，也不把合同13凭证转换为资金授权。

保存重放核对存储版本、门禁、验证状态及引用，与现有全部内容绑定一起比较；同operation/facts不能更换门禁、替换认证或改写旧v1为v2。读取/消费会解析上述组合；保存和消费的最终检查会重新读记录并逐字段比较，回调或审计触发器改成另一合法组合也整笔回滚。已有会话、版本、确认、有效期、消费唯一性和事务状态限制不降低。

新增 `0007_permit_authentication_gates.sql`，旧迁移1..6字节和校验和不变。它仅添加三列并允许认证引用按组合条件为空；旧记录/旧写入默认版本1与LEGACY_REQUIRED，仍必须有引用。迁移不会把旧记录改为无需验证、不删除或重发历史许可。新记录显式写版本2，`contract_version`仍为冻结许可版本1；旧客户端不能据此自动消费无需强验证的新记录，应升级存储解码器。此升级未放宽资金入口、本地结算终态或正式PRE_COMMIT合同。

既有 `OperationSecurityRequest::validate` 的PRE_COMMIT请求仍要求auth_grant_ref；本轮不改contracts/Runtime，也不以NOT_REQUIRED绕过它。该请求与未来签发器的条件性门禁装配仍待后续核对；存储可以表示无需强验证，不表示全链已兼容或可执行。

这不是对任意后端 SQL 的认证机制。数据库管理员、可信会话/时钟注入代码及事务回调仍在信任边界内。不可把这些服务注册为 HTTP 命令或 LLM Tool；许可及冻结类型化引用只供后端传递，调用方不得序列化到前端、LLM 或日志。

## 存储与绑定

`execution_permits` 包含冻结字段 permit_id、operation_id、decision_id、owner_id、tool_name、operation_fingerprint、facts_version、issued_at_ms、expires_at_ms、status、consumed_at_ms；额外保存 task_id、node_id、task_revision、plan_version、operation_revision、arguments_hash/binding_digest、完整 FingerprintPayloadV1、原会话摘要及绑定版本、confirmation_ref、auth_grant_ref。编号由服务端 UUID v4 随机源生成，时间为服务端整数毫秒。

完整指纹调用合同 03 的 `compute_operation_fingerprint`，包含 user/operation/tool/resource/normalized_arguments/plan/schema version；确认 arguments_hash 只用于快照一致性，不能替代指纹。normalized_arguments 与已存计划参数的摘要必须一致，浮点及零版本拒绝；本轮不规范化或猜测业务字段，也不证明资源的业务归属。投影可信性只能由未来已核验的内部签发器建立。

facts_version 从存储 u64 转冻结 u32 使用检查转换：1..=u32::MAX 可用，0/溢出拒绝。数据库的许可 facts/plan 版本也限制在相同范围。原会话仅保存域分隔 SHA256 摘要及正 binding_version，不存明文、不在返回或审计中输出摘要/会话版本。resolver 必须在撤销、续期、重绑定时递增版本，即使仍是同一用户；生产跨实例共享会话真源仍待接入。

保存/读取/消费均按 task-first 锁定当前任务，核对同一用户、原会话及版本、当前完整计划/节点/工具、任务修订、操作修订、事实及参数摘要，重新核验原 CONFIRMED 记录。仅 RUNNING 任务和 READY 操作可用；DRAFT、终态、SUBMITTED/PROCESSING、业务提交引用和需要原单对账的任务拒绝。原确认过期、撤销或变为 PREPARED 也拒绝。许可有效期为 `min(服务端签发时间 + 显式 TTL, 原确认到期时间)`；将来正式签发还需约束认证及裁决的到期时间，本轮不可取得封闭快照，不存在提前启用签发的路径。

读取返回 `StoredExecutionPermit`，没有 Serialize/Deserialize/Debug。其 `execution_permit()` 只读访问器复用冻结类型；内部 `ExecutionPermit::issue` 仅重建已存 ACTIVE 的类型化表示，不是安全裁决/签发入口。每次使用仍以数据库真源为准。

## 唯一性、撤销和过期

数据库采用永久 `UNIQUE(operation_id, facts_version)`，比“同版本最多一个 ACTIVE”更严格：历史墓碑保留，已消费、撤销、过期的同事实版本绝不另发；相同有效已核验快照重放返回原编号和期限，不重复审计。相同键的参数、投影、来源引用、会话等不同返回 PermitConflict；刷新或替换同事实版本的许可不在本切片范围。

当前事实/计划/参数/修订/会话变化会使旧许可在读取和消费时动态失效。没有后台自动将历史 ACTIVE 行改为 REVOKED；数据库状态本身不能省略最新真源核验。原会话仍有效且版本相同时可显式 revoke，更新状态和审计同事务；重复 REVOKED 幂等。已消费不能撤销，也不回滚已完成提交。原会话已失效后的后台清理接口待后续确定。

到期等号即拒绝。时间早于许可签发或原确认准备/确认时间拒绝；同一事务内观察到毫秒时间倒退也拒绝。没有跨请求的全局持久单调时钟，仍需可信部署时钟，不能声称检测全部集群时钟异常。动态到期行可仍为 ACTIVE；读/消费判定必然检查时间，EXPIRED 状态同样不可复活。

## 事务组合

```text
PermitStore::transaction(session, task, callback)
  BEGIN（仓库自身连接，设置自身 schema）
  锁定 task；重查会话
  callback: consume(permit_id, expected_binding)
    重查 schema/事务 ID、任务/操作/确认/会话/时间
    条件 UPDATE ACTIVE -> CONSUMED，受影响行必须恰好 1
    写 PERMIT_CONSUMED 原始审计
  callback: connection() 后组合同一事务 SQL
  重新读取数据库当前 task/operation/permit/confirmation
  复查关键内容、会话、时间、schema/事务 ID
  包装层 COMMIT
```

`PermitTransaction` 只能由 store 创建，没有外部 Transaction 注入、公开构造器或 commit。`connection()` 在 consume 成功前不可用；同一包装事务只消费一个许可。consume 任一失败立即使事务失效并回滚，即使回调吞掉错误，最终也不能提交。回调返回 Err、future 被取消、数据库写入/审计失败、提交时延迟约束失败，均回滚许可、哨兵或后续 SQL。没有单独提交消费的方法，不调用内存 consume 代替数据库更新。

回调 SQL 必须是受信任且经过审查的事务适配代码，只使用借出的同一连接。不得执行 BEGIN/COMMIT/ROLLBACK、切换数据库/schema、释放锁、改许可或改确认。最终检查 schema 与 txid_current 能拒绝可回滚的作用域变化，但无法撤销恶意可信代码自行执行 COMMIT 后已经完成的提交；该 API 不声称隔离任意恶意 SQL。不存在传入另一个 schema 的外部事务去消费本仓库许可的接口。

提交前重新读行，而不是使用开始时的 self.task 缓存：完整 AgentPlan（含全部节点参数）、owner/task/operation 关系、节点/工具、plan/facts 版本、参数摘要、已完成节点不可变，许可来源引用、payload、时间和实际消费时间也必须与消费时一致。回调同时改参数及 revision，或仅改操作 facts/hash，均拒绝整笔提交。

进度只接受以下两个形态：

1. task/operation 快照保持不变，Operation 仍 READY。用于底座事务隔离测试；许可已消费后不能再用，不代表业务已提交。
2. 严格遵守现有 SubmissionRecorded 形态：操作 READY -> SUBMITTED、operation_revision 恰好 +1、原业务引用非空；任务 RUNNING -> WAITING_EXTERNAL、task_revision 恰好 +1、恢复点是该 operation 的 ReconcileOperation。计划/事实/参数/已完成节点仍不变。这只定义公共状态形状，不证明真实银行提交、幂等、Outbox 或领域审计完成。

其他状态/任意修订变化拒绝。本地账务成功结果形态、业务事务适配、原单查单均待负责人交付合同后接入，不能为兼容任意 SQL 放宽内容绑定。当前没有业务回调，也没有自动继续执行。

回调必须自行遵守合同 05 的账户锁、幂等、账务、Operation、领域审计和 Outbox 同事务要求；本轮不提供这些业务接口，不将测试哨兵当账务替代。生产会话撤销源不与 PostgreSQL 共事务，提交前最后检查后的极短竞争窗口仍需后续真实源的组合设计，不能声称跨系统撤销绝对原子。

数据库明确拒绝 COMMIT（例如延迟约束失败）会全部回滚。提交时连接丢失/响应丢失可能已提交，返回 Database 不等于“肯定未消费”；须按原许可/操作核对，禁止重签重提。当前读取只返回有效 ACTIVE，其他状态返回 PermitNotActive，可结合已存任务、操作及原始审计核对；正式原单查单由业务负责，仍未接入。

## 迁移、审计与验证

初版新增 `0004_execution_permits.sql`，本次兼容仅追加迁移0007，旧迁移1..6不改。既有 audit kind、保存/消费/撤销及自身 PERMIT_SAVED/PERMIT_CONSUMED/PERMIT_REVOKED 同事务保持。拒绝回滚，不追加成功审计；无持久拒绝事件，不改变前序 PersistenceService 的拒绝语义。外键、有效期/状态/版本/摘要约束、唯一键都由 PostgreSQL 验证。原始审计不是 C-AUD、防 DBA 篡改或哈希链。

`crates/storage/src/permit_tests.rs` 是永久公共合同隔离测试。封闭快照仅 test 构造，泛化 resource 和 snapshot 节点不冒充逐风业务投影，测试哨兵只验证事务原子性。`scripts/verify-permits.ps1` 实际执行 ignored PostgreSQL 回归，缺配置直接失败。另有编译失败 doc 回归证明快照不能反序列化或外部修改。实测命令、结果、文件清单和待对接材料见[交接](../handoff/SafeFlow_ExecutionPermit存储与事务消费_v0.1.md)。

本轮不更改 Runtime/业务 crate/前端，普通 Runtime 继续拒绝资金写；前序 Mock 内存草案和授权也不会因此持久化。

门禁兼容的实测与边界见[本次短交接](../handoff/SafeFlow_许可强验证门禁存储兼容_v0.1.md)。
