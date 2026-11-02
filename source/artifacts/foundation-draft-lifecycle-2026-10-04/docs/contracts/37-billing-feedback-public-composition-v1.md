# 账单反馈公共组合 v1（本地候选，待独立审查）

2026-10-21。依据账单公共反馈接入任务、合同05/11/12/19/20/28/33与Jonathan固定提交 `8ff8c6657d724d4f69e7a3d69266b12ec148e0ea`。本合同采用公共技术接法，不冻结未确认卡接口、D08/D19/D10/D20或正式消费供数规则。

## 原报告的可信保存与恢复

成员原DDL只有报告头与所选交易，展示SpendingReport不包含私有VerifiedReport。因此现有报告JSON不能作为反馈写凭据。原采用只读Adapter实际调用成员算法时，旁路记录PagedSource每次真实调用的游标、快照、Result<Page,PageError>，原owner、全部所选账户/Request、来源ID/显式版本、成员固定版本、后端ReportMetadata、原报告全文与重复候选全文。8MiB是公共归档安全上限，超限明确失败，不截断。

附件通过ToolResult的`#[serde(skip)] backend_evidence`内存字段进入原密封Runtime结果；不接受客户端上传，不序列化到API、历史或审计正文。只有完整且可计算报告可归档。失败/不完整报告继续保存原展示状态、分页错误和null语义，不能进入反馈准备。旧无归档历史仍可显示，不能凭旧JSON补建可信报告。

归档写在原ROOT/CONTINUATION只读结算事务中：与原任务进度、SETTLED回执、公共结果、审计/Outbox及最终会话守卫同提交。保存原task/O/result_ref/claim/kind，及原回执、单个公开ToolResult、归档各自摘要。不先独立保存可选报告；原结算失败/取消/最终复核失败，归档一起回滚。

恢复从受信主库/namespace取特定归档，核原SETTLED回执和原操作/结果摘要；以当前会话owner重新核全部原账户权限。再用捕获的真实页严格按原调用序列/游标/快照重放成员analyze_spending，重新获得成员私有VerifiedReport，核原全文与重复候选相等。绝不调用当前最新reader，不从展示JSON反序列化密封证据。DB及受审后端属于信任边界，不防恶意DB管理员伪造整库。

## API / Runtime实际受信入口

`ApiState::billing_feedback_for_backend(ConfirmationConfig, PermitConfig)`返回闭合`BillingFeedbackService`，使用该ApiState的同一PgRepository、SessionResolver、BankGateway和Runtime。可信显式用户动作处理器可在原perimeter/CSRF检查后用`billing_feedback_session_for_backend(&HeaderMap)`解析现有登录Cookie。没有客户端owner/scope/schema参数。现有公开启动工厂与报告注册保持兼容，正常共享启动不装配测试source。

可调用顺序：

```rust,ignore
let session = state.billing_feedback_session_for_backend(&request_headers)?;
let feedback = state.billing_feedback_for_backend(confirmation_config, permit_config)?;
let original = feedback.original_report(&session, &report_id).await?;
// 选择来自这份原报告，FeedbackIntent仅包含report_id/复合交易引用/成员choice。
let prepared = feedback.prepare(&session, intent).await?;
// 将prepared.parameters展示给当前用户；只有明确确认动作才调用confirm。
feedback.confirm(&session, &prepared).await?;
let saved = feedback.execute(&session, &prepared.task.task_id,
    &prepared.operation.operation_id, &prepared.confirmation.confirmation_ref).await?;
// 超时、提交不明和重启只查同一个原号；不要重新prepare/execute。
let recovered = feedback.query(&session, &prepared.operation.operation_id).await?;
```

这是实际受信Rust后端调用接口。完整工厂测试为`billing_feedback_api_factory_complete_confirmation_execute_query_and_concurrency`，输出原HTTP结果/准备/查询对照，保留工厂供后端组合；同时新增下述实际显式HTTP处理器。模型不能调用confirm/execute，反馈NOT_MINE也不触发锁卡。

受信启动方显式设置`ApiConfig.billing_feedback=Some(BillingFeedbackConfig::for_reviewed_backend(confirmation_config,permit_config))`才启用HTTP动作。两个配置沿原类型由启动方明确选择TTL，无客户端/环境开关或默认期限。`ApiConfig::local_simulation`为None，正常共享启动未改、未装配原消费源或卡目录。公开启动仍使用原`ApiState::for_project_simulation_with_read_only_tools(repository,config,bank,registrations)`，同一BankGateway/会话/Runtime进入反馈；没有新增模型工具、卡provider或资金能力。

| HTTP动作 | 最小请求与输出 |
|---|---|
| `POST /api/billing-feedback/prepare` | `{input_version:1,report_id,transaction:{source_id,account_id,transaction_id},choice}`，choice沿成员MINE/NOT_MINE/UNSURE。返回服务端task_id/operation_id、原parameters、confirmation引用/status/期限、lock_authorized=false |
| `POST /api/billing-feedback/tasks/:task_id/operations/:operation_id/confirm` | 仅`{confirmation_ref}`；独立显式用户确认动作，从当前会话读取服务端原确认/绑定并核原O，不能上传完整Binding |
| `POST /api/billing-feedback/tasks/:task_id/operations/:operation_id/execute` | 仅`{confirmation_ref}`；服务端完整复核，不接受scope/owner/原页面/密封证据/许可/结果。成功返回提交后独立读回的原成员SavedFeedback及最小公共操作投影 |
| `GET /api/billing-feedback/operations/:operation_id` | 无查询参数；仅只读原号。缺失/无权404，数据库/完整性故障503，绝不以Err代None或补写/执行 |

所有路由复用现有perimeter的Host/Origin、JSON内容类型及`x-safeflow-request: 1`，Cookie会话、no-store、严格路径/请求体未知字段/重复字段检查，8KiB请求上限和原配置超时。关闭配置时认证后501；没有新增可选owner/scope/schema。confirm只能由展示prepare原parameters后的明确用户动作发起，不是登录自动确认。没有页面视觉改版或自动调用这些动作的按钮。

执行HTTP超时取消整个composer，返回409 `RECONCILE_REQUIRED`并保留原task/O路径语义；COMMIT可能不明，只GET原号，不自动retry/prepare。未确认/绑定冲突409，额外认证依赖403，跨主体404，失效会话401。测试`billing_feedback_http_original_report_prepare_confirm_execute_get_and_atomic_rejections`从真实HTTP报告生成开始，经prepare/confirm/execute/GET核原结果、错误拒绝和三处联合回滚；`billing_feedback_http_timeout_requires_original_get_and_rolls_back_waiting_stage`实际观察成员stage等待后触发HTTP超时，确认整体回滚且无自动重执行。这些仍是本轮独立库/本地协议与成员虚构JSON的证据，不是共享服务上线。

公共非资金工具身份`save_billing_feedback`仅用于后端绑定，policy=`billing-feedback-original-scope-v1`、schema=1，最低Yellow；不进入LLM工具表。使用同一个DeterministicSecurityKernel的最终复核，保留已有更高Policy/Profile门槛、风险评分、会话/上下文校验；不伪造金额、不借付款阈值放行。当前composer只支持ExplicitConfirmation；Red/Critical等额外认证门槛明确拒绝，不默认通过。公共参数包括原归档摘要、原report_id、复合交易引用和choice，参与完整确认/原O/指纹/版本绑定。

## 同事务与保存点所有权

唯一公共迁移链追加0025，原1..24不变；原成员三表/不可变触发器保留，另加public_billing_reports_v1及public_billing_feedback_results_v1，不另建仓储池或迁移器。

闭合composer持有PermitTransaction：锁本人任务→恢复原报告并核全部原账户→Kernel最终决策→核原持久确认→公共许可保存及消费→**同连接**成员stage_feedback→复核账户/会话→原record_submission→公共反馈结果与严格SUCCEEDED操作形态→公共审计及既有触发Outbox→最终绑定/schema/事务ID/确认/许可/会话守卫→唯一COMMIT→独立连接load_feedback/原号查询后才返回成功。

仅该闭合调用拥有成员固定私有保存点`billing_feedback_stage_v1`；禁止外层业务callback重入、同名嵌套、BEGIN/COMMIT、任意事务/schema控制。成员仅SAVEPOINT/ROLLBACK TO/RELEASE，不接管外层。成员错误（包括保存点清理失败/OutcomeUnknown）直接传播，不能吞错后继续。取消整个composer future时丢弃外层SQLx Transaction并排队ROLLBACK，禁止取消后复用该事务提交；联合测试实际观察成员事件INSERT等待advisory锁后取消，核许可/成员/公共结果/审计/Outbox均无部分提交。

结果守卫只接受闭合composer产生的原请求、成员反馈行/本地事件、公共不可变结果及操作SUCCEEDED。这个任务只有一次显式非资金动作，本地反馈结果已核清后，在**原事务内**按既有状态机从SubmissionRecorded产生的WAITING_EXTERNAL，经ExternalResolved→RUNNING，再Complete→COMPLETED/Finished，两个节点都登记完成；operation revision从原绑定+2，task revision+3，并分别写原始操作结果审计、恢复检查点及完成检查点/Outbox。不依赖不存在的外部worker，也不把未注册的模型工具交普通Runtime重跑。

`BillingCompletion`为只有闭合composer能构造的私有完整task/op证明，单独`verify_completion`从原锁定task重建上述精确终态，比较完整计划/owner/facts/版本/完成节点、原O/业务引用/摘要、原首条SUBMITTED回执、成员/公共结果后，仍经过共享schema/事务ID、许可/确认/会话/期限最终守卫。没有简单增加通用callback可接受的终态；普通READY/SUBMITTED与转账专用分支保持原限制。终态审计失败或审计触发器篡改completed_nodes的联合测试均整笔回滚。保存行中的staged始终是STAGED_AWAITING_PUBLIC_COMMIT，不当作提交回执。

执行及专用原号GET明确返回task_state=COMPLETED；普通`GET /api/tasks/:task_id`读回COMPLETED、FINISHED恢复点与FINISHED下一步，不需要模型工具注册。报告历史仍走原Agent只读历史；显式反馈历史使用本合同专用原号GET，不冒充新的Agent规划任务。原GET不写入、不恢复业务或消费许可。跨进程查询也核任务已完成；不存在需要将来补完的WAITING_EXTERNAL中间态。

原始公共OPERATION_TRANSITIONED携带本地成员stage事实、原摘要、lock_authorized=false及domain_audit_mapping_adopted=false，经迁移17同事务进入公共Outbox。它不是正式C-AUD事件映射、外部送达或ACK。未启用正常sender worker。

## 原号语义与剩余采用差异

同O同原绑定重放原已提交结果、不消费新许可；同O换task/确认/会话绑定拒绝，不提供同O改参入口；不同O重复原交易按成员D20停止。查询以当前有效同owner会话和全部原账户授权为准，使用独立主库连接调用成员load_feedback并复核公共绑定及会话；跨主体/缺失None与数据库/完整性Err保持区分。合法新会话可读历史，不能消费旧会话确认。成员SavedFeedback只作为原opaque序列化结果返回，不私取字段构造卡provider/feedback_id/revision。

COMMIT尝试发生错误统一保守返回ReconcileRequired；提交成功后读回失败仍是查询错误，不能推断未保存。调用方保留原O，只调用query；不自动创建新操作或重执行。真实COMMIT断网本轮未验证，不能把并发重放/模拟丢返回称为断网通过。

正式消费源与可信卡目录、SavedFeedback的成员可访问卡投影、公共feedback_id/namespace/revision、卡侧provider、正式C-TXN/C-AUD及D08/D19/D10/D20业务采用继续具体待对接。SQLx安全处置另见本轮交付的统一方案，当前仍0.7.4，不能宣称漏洞关闭。新代码仅本地候选，显式HTTP接法待指定独立审查，不是正常共享站点反馈/锁卡上线；无提交/推送/发布/真实模型/共享付款/锁卡授权。

## 2026-10-24 隔离恢复候选补充

本候选新增nonce原子准备、持久入场、只读反查及明确放弃，详见[合同38](./38-billing-prepare-recovery-v2-candidate.md)。原v1业务语义和历史说明保留；受信安装后旧账单writer拒绝绕行，原已保存GET保留。本说明不是共享采用或启用授权。
