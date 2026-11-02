# 付款公共绑定与原单查询 v1

2026-10-18登记增量：已实现独立内部关联及以下实际HTTP路径。登记不写 `public_execution_requests`，不预占其唯一业务号，不生成资金幂等键或资金执行操作。下文最终执行分支/资金Tool提案仍未启用；本增量不是对该提案的批准。

2026-10-18，底座内部登记与只读查询合同；不是资金 Tool/Profile 批准、许可或扣款入口。复用合同02/03/05/17/28/29/31，成员代码为逐风固定 `f0997ec1e0ebf1bfa88f7a44ffff25cac3cd0cc6`，业务SQL源自 `1f0e46db7d08051140d42ac195af5f207f2d7062`。不修改成员DTO或原JSON。

## 实际入口

`GET /api/sim/payment-preparations/:task_id/:operation_id/original-result`

operation_id是原付款**准备**操作。原付款准备/查询响应新增 `original_result_path`，可直接GET。只接受原任务/准备操作选择，拒绝查询参数，不接收owner、业务号、幂等键、结果或授权声明。沿既有Cookie、Host/Origin、会话和超时保护；POST此路径为405。

`PaymentPreparationService::original_result`在数据库 `REPEATABLE READ READ ONLY` 事务中，从当前版本化会话取得owner，核验任务模式/生命周期历史及持久意愿、原确认、原计划和完整快照摘要。普通付款query也已改为只读投影，所有付款GET不持久撤销确认或判断。当前合法同owner新会话、原确认过期/撤销均可读历史；没有恢复执行权。等待及事务提交前后复核当前会话绑定。

## 内部持久登记

`POST /api/sim/payment-preparations/:task_id/:operation_id/execution-link`，请求严格为：

```json
{"contract_version":1,"scope":"PAYMENT_REGISTRATION_ONLY","confirmation_ref":"原准备确认号","display_ticket":"同份准备显示票据"}
```

拒绝查询参数和额外字段。POST沿用登录Cookie、Host/Origin及 `x-safeflow-request: 1` 防护。GET同路径是原结果查询别名，返回完整原结果及 `execution_link`，不是再次登记。原付款单笔/任务数组响应也提供该关联和路径。

服务端由原保存准备生成 `PaymentRegistrationBinding`，明确绑定原完整确认摘要、准备操作、独立登记操作、任务修订/计划/事实版本及完整成员请求摘要。原确认和lifecycle_anchor保持原样。迁移21将关联与完整原确认binding存入 `payment_execution_links`；登记操作在公共operations中保持不可变DRAFT，单独保存审计/Outbox，四者同事务。锁顺序owner→task→业务原号→草案，保存前及保存后核验当前会话、原确认已确认且有效、计划/事实和完整当前草案。失败全部回滚。

`registration_key` 仅是内部登记身份，`registration_operation` 仅是元数据操作。它们不是 `idempotency_key` / `execution_operation_id`，不能传入Permit或FundsStore。登记响应显式包含 `confirmation_migration_authorized=false`、`funds_execution_enabled=false`。未产生正式请求时，原结果binding中的执行号、指纹和资金原键仍为null。

每份准备最多一条不可变关联；同请求、并发和重启返回原登记。合法同owner的新会话可以重放已存历史，不恢复旧确认。跨准备版本允许新登记，所有版本沿原草案保留同一个稳定业务号，各自完整快照和确认独立；旧登记不覆盖、不解除正式执行表的唯一约束。登记后继续修改/取消、新版准备确认均走原生命周期接口。正式公共原请求或成员已提交/未知原单存在时，不新建登记，沿原结果读取；已存在登记的重放只返回历史。

真实资金请求与登记的查询关联通过同owner、稳定业务号及完整成员请求核验，资金原键和实际执行号只取真实公共记录/成员原单。后续正式资金请求可以沿同一业务号登记，内部关联不占其位置；最终资金Tool/Profile、确认迁移与专用许可装配未确认，当前不提供该正式资金写入口。数据库共存回归只证明唯一位置未被占用，不代表最终执行装配通过。

## 实际绑定适配

`PaymentExecutionBinding`是响应/比较结果，仅Serialize，不是可输入或签发的凭证。它在上述实际HTTP调用链使用。

| 字段 | 可信来源/含义 |
| --- | --- |
| preparation_operation_id / preparation_plan_version | 已保存独立付款准备操作及当时Runtime计划版本 |
| preparation_arguments_hash | 原完整PaymentSnapshot摘要，原确认的lifecycle_anchor不改 |
| business_operation_id / business_draft_version | 保存的完整成员MockTransferRequest稳定原号和业务草案版本 |
| normalized_arguments / transfer_arguments_hash | 原样完整成员请求及其独立摘要；不同于完整PaymentSnapshot摘要 |
| execution_operation_id / execution_plan_version / execution_tool_name | 仅从实际已存公共ExecutionRequest读取；没有公共请求则null |
| idempotency_key | 实际公共请求原键或已核验成员原单original_key；尚无原请求则null |
| operation_fingerprint | 仅实际公共请求的完整指纹；没有则null，不猜工具名/键/执行号 |

有公共记录时复用 `execution_request::stored_row`、`ExecutionRequest::validate`、`transfer_result::bound_request`、`execution_request::view_locked`。重算整个payload摘要/指纹、业务版本/resource、owner/task、原始及当前公共操作，并逐项比较**完整**成员请求（含备注）。准备操作不能冒充执行操作，内部意愿Tool不能冒充资金Tool。适配只关联和核对历史，不创建ExecutionRequest，不将旧确认清除anchor或重散列后转换成执行许可。

## 业务结果与状态

复用 `FundsStore::query` 与从 `transfer_result::verified_record` 提取的共同只读 `verified_business_record`，检查原键、完整请求、资金状态及成功的双边不可变流水；写适配仍先锁原单和资金记录，再调用相同核验器，原事务/审计/Outbox不变。

| state | 含义 |
| --- | --- |
| UNSUBMITTED | 此一致性快照无公共/业务/资金/流水记录且草案存在、无submitted标记；不是付款许可或可换号证明 |
| REGISTERED_UNSUBMITTED | 有已核验的内部关联，但没有正式公共请求/业务/资金/流水及submitted标记；登记不是资金提交，原登记可作为历史读取 |
| PROCESSING / SUCCEEDED / FAILED | 完整公共原请求/首次提交与实际业务结果核验通过后的业务状态，不按公共SUBMITTED猜成功 |
| RECONCILE_REQUIRED | 单侧来源、只有submitted标记、草案/原单缺失等；不可认为未扣款，不允许补提/换号 |
| SOURCE_CONFLICT | 原请求、公共映射、幂等键、当前公共终态或双边流水不一致；不能当作业务FAILED |

响应保留 `first_public_submission` / `public_submitted_at` 和 `verified_business_result`（成员原request/first_receipt/latest_receipt）两种来源。只有业务一侧时，即使核验到真实SUCCEEDED，整体仍为RECONCILE_REQUIRED，并明确缺公共原单。冲突内容不作为已核验结果返回。数据库故障仍为503，不吞成空原单。

GET不调用submit/resolve/reconcile、不新增请求/许可、不改公共状态、确认、判断或审计/Outbox。`funds_execution_enabled=false`、`public_sync_enabled=false`恒定表达本入口权限。原通用显式 `reconcile_transfer` 仍只服务已兼容的普通执行任务；不会从本GET调用。

## 最终执行仍关闭的具体原因

当前实际付款准备是生命周期任务中的独立DRAFT分支，已有 `lock_task` 拒绝生命周期模式，`reserve_locked/current_binding` 要求普通RUNNING/READY及当前计划节点；`view_locked/settle`要求通用checkpoint。付款准备确认还绑定完整PaymentSnapshot与lifecycle_anchor，而资金公共请求必须绑定完整MockTransferRequest且anchor为空。不能为接上它而放宽这些通用检查。

因此本轮HTTP真实成功/失败读回验证使用成员既有隔离SQL结算方法，整体缺公共原单，明确标待核对；公共+业务双侧状态映射另在既有sealed公共隔离夹具验证。没有声称生命周期任务已经能登记/提交资金，或普通公共夹具已完成真实用户扣款流程。

底座承担下一段技术方案（以下为**未启用的设计提案**，不是新增注册或悬空签发器）：

- 资金Tool候选沿既有提议 `execute_transfer`，输入Schema采用 `draft_id` + `draft_version`，严格拒绝多余字段。后者明确为业务版本，不能沿用旧提议的歧义plan_version。由底座形成正式Schema/Profile，逐风只核对业务语义；内部可信投影仍为完整MockTransferRequest，不能散列两字段后给资金适配器。
- 同一task保存新的资金执行分支和独立执行Operation；从本轮已实现的内部登记建立正式执行关联，显式绑定原确认、完整PaymentSnapshot摘要、完整成员请求、B、资金原键、执行计划/事实版本和执行payload指纹。内部登记尚未分配这些资金字段。由专用模式命令验证生命周期历史和新分支，不清除旧anchor、不伪造旧Operation READY，不修改通用lock_task/current_binding。正式执行状态/关联迁移及最终命令归底座实施，未通过合同与必要来源验证前不创建新资金号。
- 最终Profile使用已有Kernel硬规则：当前owner全账户日累计加未提交本笔、独立确认、仅在实际门禁要求时核验本笔认证、真实业务Policy事实、卡锁revision及同一锁覆盖COMMIT。软评分只计算/审计。最低等级结果READY_TO_CONFIRM/WAITING_AUTH不能成为READY_TO_EXECUTE。
- 专用后端最终装配器核验上述所有来源后，才可创建封闭VerifiedPermitSnapshot；许可条件消费、成员FundsStore提交、原单/双边流水、公共结果、关键审计/Outbox必须仍在现有同一连接和同一事务中。`permit::request_issuance`当前仍拒绝，PRE_COMMIT缺可信核验时仍拒绝，不添加固定拒绝的execute HTTP充数。

真实缺项：逐风提供/确认资金DomainPolicy业务事实（现有只有草案Policy，无需重交已交SQL/JSON）；结夏提供正式PG卡锁来源、revision及锁至COMMIT协议；本笔模拟交易验证的来源与撤销/版本绑定由底座装配并与政策负责人确认；风星龙与底座采用正式外部审计映射/ACK。底座不将Schema/Profile、分支关联、Kernel/许可装配推回成员。

自测与独立验收分开；原单查询历史交付见工作区 `artifacts/payment-execution-bridge-2026-10-18/DELIVERY.md`，本次登记及独立检查设计返修的本地候选见 `artifacts/payment-execution-link-2026-10-18/DELIVERY.md`。
