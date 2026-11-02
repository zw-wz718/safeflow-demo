# 本人任务进度查询与安全范围视图 v1

日期：2026-10-06。`TASK_QUERY_CONTRACT_VERSION = 1`。依据本轮 `task-query-stage-2026-10-06.md`、BASE-R04 / BASE-05 和既有合同 02/03/05/06/08/09/11..14。这是底座内部后端查询合同，未冻结全团队 `TaskResponse`、C-HTTP、外部登录或业务响应。

## 入口和可信来源

```rust
let service = TaskQueryService::for_trusted_backend(repository, assembly.runtime());
let view = service.query(&session_ref, &task_id).await?;
```

实现为 `crates/storage/src/task_query.rs`，依赖现有 PostgreSQL 16 `PgRepository` 和 `RuntimeBootstrap` 构建的 Runtime。身份只使用该 Runtime 的 `session_binding_for_backend` / 版本化 `SessionResolver`，不独立注入第二个身份或成功提供器。请求仅给 `SessionRef` / `TaskId`，无 owner、可信状态、确认 / 验证布尔值、回执或任意筛选。会话引用是内部后端引用，不是本轮选择的外部认证传输协议。

每次查询使用当前有效会话重新开始核验，不要求旧创建会话仍存在；同用户的新有效会话可以查询本人历史任务，但不能由本方法续用旧确认或凭证。本次调用一旦开始，则必须保持原 `user_id + binding_version`；撤销、续期、同用户重绑也使本次成功路径失效。resolver 须在这些动作上递增且不复用版本，读取身份 / 版本为同一快照；默认无版本 resolver 和零版本均拒绝。生产持久身份与跨实例会话源仍待对接，StaticSessionResolver 仅是演示装配。

`PgRepository::connect_existing` 是兼容新增的连接方式：只打开已存在且名称合法的 namespace，不创建 schema 或执行迁移；不存在统一 NotFound。原 `connect` 的可信初始化语义保持不变。查询服务自身不建表、不迁移、不更新数据；生产装配应先完成可信迁移并采用只读权限。

## 输出及三维边界

`TaskProgressView` / `OperationProgressView` / `TaskRecoveryHint` / `TaskQuerySecurityScope` 只有输出 Serialize，不提供 Deserialize。根字段恰为：

| 字段 | 来源 / 语义 |
| --- | --- |
| contract_version | 独立内部查询合同版本 1 |
| task_id / task_state | 已存本人任务及原 TaskState |
| revision | 任务数据库记录版本；不是计划版本 |
| plan_version / facts_version | 已存计划版本 / 事实引用版本，不证明业务事实仍新鲜 |
| operations | 完整且稳定排序的关联操作摘要，每项为 operation_id、node_id、operation_state、revision、plan_version、facts_version |
| recovery | 从既有恢复点校验及 RecoveryDirective 投影的受限提示 |
| security | 固定 `CURRENT_DECISION_NOT_PROVIDED`，明确本视图没有当前有效安全裁决 |

恢复提示为 `AWAIT_USER { node_id }`、`REVIEW_NODE { node_id }`、`RECONCILE_ORIGINAL { operation_id }`、`REVIEW_RECORDED_RESULT { operation_id }`、`FINISHED { task_state }`。没有 Execute / Submit / 自动下一步。SUBMITTED / PROCESSING 保留原 operation_id，只提示需要另行核查原操作，不声称银行已经查单；可信已存结果仍只是要求复核记录，不附加业务回执。

TaskState、OperationState 原样分开，不使用一个通用 status，也不派生业务 SUCCESS。任务 COMPLETED 不等于扣款成功；操作 READY 不构成确认 / 认证 / Permit。安全范围字段不是 SecurityOutcome，不能将它或空操作列表解释成无需安全检查。本轮不读取 / 重评估已有前置裁决，不把历史 ALLOW / ACTIVE 变成当前授权；后续需要正式有效性核验来源才能增加安全结论。

不输出 owner、会话、验证码、确认引用、AuthGrant / Permit、内部注册配置、AgentPlan、原参数、arguments_hash、审计明细或业务外部单号。业务结果、正式审计状态 / 证据未接入，不猜字段 / 枚举或返回假结果。操作 facts_version 可低于任务 facts_version：既有公共存储只更新本次变化的操作，其他操作可保留历史事实版本；逐项如实输出，不标成当前已获安全核验。零或高于任务事实版本、计划版本不匹配等拒绝。

## 一致性、校验与错误

查询使用 PostgreSQL `REPEATABLE READ READ ONLY` 事务，先按 task_id + 当前 owner 获取任务，再取操作集合。MVCC 快照在首个任务 SELECT 建立，同一响应的 task / operations / 版本来自同一数据库提交视图，跨进程写入同样受约束；不使用内存锁，也不要求写方遵守某个查询锁。另一个事务在两次 SELECT 之间完成合法改版 / 操作迁移，当前响应仍为完整旧快照，下次查询取得完整新快照。响应不是永远最新的授权快照，不可据此执行业务写入。

连接池 / BEGIN、设置隔离、任务 SELECT、操作 SELECT、只读事务 COMMIT 每次 await 后都重新比较原会话；服务返回前再核验，最后核验后没有异步等待。锁等待期间撤销 / 换版同样拒绝，包含底层 await 返回错误的情形。外部会话源与 SQL 没有分布式原子事务，最后同步检查之后或响应传输期间的撤销仍需生产身份协调；此查询不产生写权限。

复用现有 `validate_plan`、`directive` / `validate_checkpoint`、记录解码和参数摘要算法。校验计划身份 / 信封 / 有界依赖图、正记录 / 事实版本、恢复点 / 完成节点、操作 task / Tool 节点 / 计划版本 / 事实版本关联、重复操作 / 节点、当前计划参数摘要、引用与操作状态、单一未明外部操作。嵌套浮点拒绝，金额字符串原样保留在内部校验，不转换为数值或输出。查询不运行 Tool / Policy / Kernel 评估 / LLM，不恢复执行、不签发、读取、撤销或消费凭证，也不追加审计。

技术限制沿用现有 1..256 节点结构和前置存储 64 KiB 计划边界。计划 / 完成节点 JSON 各限 64 KiB、checkpoint JSON 1 KiB，SQL 在传输和解码前检查大小；标识和业务原引用限 200 UTF-8 字节，无首尾空格或控制字符。操作最多 256 项（既有每节点一项约束），读取最多 257 项以识别超限，超限整体失败，不静默截断；按 PostgreSQL C 字节排序的 operation_id 稳定返回。坏 JSON、状态、节点、关联、版本、摘要或过量数据整体 CorruptRecord，不跳过坏项。

不存在 / 他人任务相同 `NotFound`，无效 / 无版本会话或本次绑定变化相同 `Session`；请求标识非法为固定 `Invalid`，数据库失败为 `Database`。错误不带 SQL、URL、原参数或凭据。没有任意 SQL / 路径 / 列表筛选入口；未来 HTTP 的状态码及身份协议待共同确认。

## 验证、迁移与未决项

永久测试在 `crates/storage/tests/task_query.rs`（真实 PostgreSQL，默认 ignored，须显式执行）及 `src/task_query.rs` 单元 / compile-fail 文档回归。并发用例使用真实数据库表锁：任务读取后阻塞操作读取，同时提交计划或 task / operation 状态 / 事实版本，再核对响应只出现旧 / 新完整组合。存储样本仅为既有公共状态合同的隔离记录，没有构造扣款、银行结果提供器或组员业务。

`task_progress_query` 示例 prepare 模式复用已有真实 Runtime 只读账户 Tool / Policy 保存合法 WAITING_USER；query 模式连接已有 schema，在独立进程读回同一内部视图，Tool 执行 / BankGateway 调用均 0。prepare 负责明确的初始化 / 等待点写入，不能把这部分说成零写入；query 本身零数据写入。脚本 `verify-task-query.ps1` 比较两个新查询进程，保留随机演示 schema 供复验。

无新依赖 / 迁移，旧迁移和已冻结状态、恢复及凭证合同不改。数据库管理员或受信任后端仍可改写所有相关内容；结构一致性不是防 DBA 篡改 / 真实业务证据验证。取消查询最多结束只读事务，不改变任务；网络响应丢失可重新查询原 task_id，不产生新操作。

| 负责人 | 待对接材料 | 受影响步骤 / 恢复条件 |
| --- | --- | --- |
| 底座与页面承接人，正式身份服务负责人待确认 | C-HTTP / TaskResponse、身份传输与可撤销版本化会话真源 | 确认合同后装配 HTTP；本轮只有内部后端服务 |
| 逐风及各业务负责人 | 原 operation_id 查单、业务结果 / 资源映射、事实版本与结果核验合同 | 收到实际接口 / 代码并确认后接入；本轮仅保留原号提示 |
| 底座公共安全与对应业务，认证策略负责人待确认 | 正式有效裁决、完整投影、认证真源、失效规则 | 来源齐备后单独设计安全查询；本轮明确不提供当前裁决 |
| 风星龙与底座 | 正式审计查询、事件 / 补写 / 存证合同 | 收到材料后增加已确认投影；底座 SQL 事件不能冒充正式审计 |

运行命令、实测与文件清单见[交接文档](../handoff/SafeFlow_任务进度查询与安全范围视图_v0.1.md)。本子项已于 2026-10-06 [独立验收通过](../../../task-query-review-2026-10-06/REVIEW.md)，不能把本公共查询子项标成 BASE-05 / 全项目完成。
