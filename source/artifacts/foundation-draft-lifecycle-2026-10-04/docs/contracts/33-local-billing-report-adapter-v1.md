# 本地基础账单报告只读适配 v1

## 2026-10-21 公共反馈组合增补

当前采用成员固定`8ff8c6657d724d4f69e7a3d69266b12ec148e0ea`，65文件原件与SHA见`docs/sources/jonathan-8ff8c665.json`；V2公开结构保留，member_revision更新为该来源。完整来源页仅通过不出现在wire的后端附件进入原只读结算事务内归档，依据[合同37](./37-billing-feedback-public-composition-v1.md)恢复特定原报告及显式HTTP反馈。旧无附件历史继续显示，不具备反馈证据。下文保留V1/V2历史来源说明；反馈/保存点/公共结果的当前接法以合同37为准，正式业务/卡映射仍未采用。

## 2026-10-19 V2兼容扩展：本地重复消费候选

当前成员来源为固定 `d822709a69dcc73c522adc711e23329758315c0c`，40份Git原字节文件及blob/SHA256见 `docs/sources/jonathan-d822709.json`；原7007ce2来源清单保留为历史。依据原PRD BILL-R04采用成员 `duplicates::detect_duplicates`，不批准D-10历史偏离阈值，不表示完整异常分析。

工具名、action、输入版本及可信范围接法保持下文原合同。当前返回 `adapter_contract=LOCAL_BILLING_REPORT_ADAPTER_V2`，保留原source_revision、source_mode、production_source_connected=false、report，新增 `member_revision=d822709a69dcc73c522adc711e23329758315c0c` 和 `duplicate_candidates`。基础report及其 `capabilities.anomalies.executed=false` 原语义保持。

同一次spawn_blocking中，先由成员analyze_spending读取来源并生成内存报告，再立即以相同TrustedScope和该报告调用detect_duplicates。只使用成员私有证据，不从展示JSON反序列化可信报告，不第二次取数拼快照，不补造merchant_id。该输出随同一个ToolResult在原公共只读结算事务内保存；无新表、新执行HTTP服务或独立候选写入。

`duplicate_candidates`保留成员Result的JSON形状，且只允许以下两个互斥分支：

- `{"Ok": {...}}`：完整原DuplicateReport，包括contract、rule_version、report_id、snapshot_id、source_id、production_registered=false、state、pairs、missing_merchant、risk_action_authorized=false。
- `{"Err":"REPORT_NOT_COMPLETE"}`或`{"Err":"RESOURCE_UNAVAILABLE"}`：无法完成候选判断，绝不是没有重复。绑定在同一包装的report和可信公共操作下，不生成假空列表。

成功分支contract固定 `LOCAL_DUPLICATE_CANDIDATES_V1`，规则固定 `LOCAL_SAME_MERCHANT_CNY_AMOUNT_0_600S_V1`。成员仅对可信相同merchant_id、币种、定点金额且0..600秒闭区间产生候选；同秒不同交易不自动去重。`COMPLETE`允许空pairs，含义仅为本规则未发现候选；`MERCHANT_EVIDENCE_INCOMPLETE`保留缺商户的SourceKey及已有候选，不能称全部判断完成；`PAIR_LIMIT_EXCEEDED`必须pairs=null，不能截断为完整列表或解释为零候选。

公共report_id仍为 `billing_report_{operation_id}`，不是成员独立示例的虚构报告号；候选report_id、source_id、snapshot_id必须分别等于同一report.metadata.report_id、read.source_id、read.snapshot_id。每个交易引用由source_id/account_id/transaction_id共同定位到该report.evidence；网页严格核对这些关联和当前输出版本。成员原JSON/原例期望保持原件，测试单独核对公共报告号映射，其余成员候选字段逐项相等。

网页保留旧V1报告原有展示，V2新增只供核对的候选、原交易引用及缺失状态。没有反馈保存、锁卡按钮、自动处置或完整异常通过提示。反馈prepare_feedback仍为PREPARED_NOT_SAVED，不注册写工具，不把纯比较当数据库幂等。

当前implementation_revision使用 `local-billing-v2:` 前缀；摘要包括当前成员提交、lib/query/spending/duplicates原字节、适配源码、实际工具定义及原受信来源版本/限制。新增参与计算的duplicates文件已纳入；反馈文件不参与本工具计算，仍经成员来源清单核对。旧已计划execute/continue必须拒绝不匹配的实现，不继承旧批准。

Agent GET使用 `RuntimeTaskContinuationService::query_saved_history` 对原保存链作独立只读投影。该方法仍核验当前会话、owner、期望版本、完整回执链/结果摘要/审计一致性；不运行当前Tool/Policy、不重读来源、不重建私有证据，也不授予执行或继续权。旧V1及已完成V2历史在工具升级/未装配后仍可GET读取，未执行旧计划不能据此运行；原execute/continue及原recover方法的当前注册校验保持。旧返回正文不追加新候选，不随升级改写。

以下为V1原合同及历史验证边界；当前执行采用上述V2增量。

2026-10-18，依据BASE-R02/R03/R06、合同27及本轮账单任务，采用Jonathan固定 `7007ce2be971b4e4ad2c257e21ec8a830d651397`。这是底座本地只读接入方案，不冻结正式C-TXN，不包含反馈写入、订阅、净额、异常、预测或周期规则。

## 受信启动装配

`LocalBillingSource::for_reviewed_local_bounded_reader(reader, revision)` 接收成员既有 `PagedSource + Send + Sync` 和非空来源版本。调用者必须审查并明确批准该来源只做有限本地读取；本版本不支持同步网络/生产消费来源。没有默认消费源，不从转账流水制造消费记录，HTTP及模型不能上传来源实现或原始页JSON。

`billing_report_registration_for_backend(bank, Some(source))` 返回现有 `ReadOnlyToolRegistration`；None拒绝。必须传入与ApiState装配相同的可信BankGateway。随后调用原 `ApiState::for_project_simulation_with_read_only_tools`；启动前仍经过Tool/Policy/Profile、输入投影和恢复兼容校验。默认服务未调用此工厂，不会自动启用账单。

本轮唯一批准项：工具 `analyze_local_basic_spending`、action `READ_LOCAL_BASIC_SPENDING`、schema/input_version=1、Policy `local-billing-scope-policy-v1`、最低等级Green。该等级仅适用于此本地只读注册，不给其他工具默认批准。Policy仅校验公共会话上下文、请求范围、账户读取及来源身份，不代写未决账单业务规则。

成员31个Git原字节文件位于 `members/jonathan`，包括原独立Cargo.toml/Cargo.lock及JSON；根workspace排除该独立包并以path依赖引用，成员manifest无需修改。源文件、Git blob和SHA256见 `docs/sources/jonathan-7007ce2.json`。

## 参数投影

使用原 `bounded-direct-input-v2` 及恢复所需的扁平直接对象语法，不修改全局校验器。原数组与嵌套Window不能直接注册，因此批准以下最小扁平投影：

```json
{
  "input_version": 1,
  "account_ids_json": "[\"account_001\"]",
  "start_inclusive": "2026-10-01T00:00:00+08:00",
  "end_exclusive": "2026-10-16T12:00:00+08:00",
  "timezone": "Asia/Shanghai"
}
```

account_ids_json是JSON字符串，只解码为1..16个唯一账户ID，最多2048字节；不是TrustedScope或来源注入。起止时间各最多64字符，必须为RFC3339且start<end<=后端as_of；时区固定Asia/Shanghai。保留成员半开窗口语义，不自动换成四个月或改写所选时间。拒绝额外字段、重复账户、空范围、未来窗口和不匹配版本。

成员 `Request.account_ids` 来自该数组，`Window.start_inclusive/end_exclusive/timezone` 原值映射，as_of来自后端系统时间。report_id为 `billing_report_{公共operation_id}`，calculated_at使用本次后端时间；这些值不能由模型或请求提供。报告ID不是新的业务写入授权。

## 归属与实际执行

owner来自Runtime实际SessionResolver，不读取fixture中的user字段。Policy和Tool都以该owner逐项调用已装配BankGateway.get_account_balance，并核对返回账户ID及原响应合同；任一失败停止，不能因账户列表有一项成功就授权其他项。

BankGateway响应没有owner字段，不能声称验证了不存在的字段：归属依据是其“以已验证用户读取本人账户”的既有合同。仅这些实际成功读取的账户构造成员TrustedScope，不声称枚举本人全部账户。成员collect继续逐页核验owner_user_id、账户集合、行归属、窗口、快照、游标和完整性证据。user_demo与user_001严格分开，不做别名。

Tool实际调用成员 `spending::analyze_spending`，由它调用collect和原汇总逻辑。报告rule_version保留 `LOCAL_BASIC_SPENDING_2026_10_16_V1`。返回data包装包含 `adapter_contract=LOCAL_BILLING_REPORT_ADAPTER_V1`、启动代码提供的source_revision、`source_mode=LOCAL_BOUNDED_IN_PROCESS_V1`、`production_source_connected=false` 和完整原成员SpendingReport的report字段，不接受客户端提供该对象。

ToolResult.SUCCEEDED仅表示产生了该报告对象，不能据此宣称消费读取完整；消费者必须检查report.read、calculation_status及summary。分页失败、来源不可用或证据不足沿成员状态返回READ_BLOCKED/null，不转换成0元成功；只有已证完整的空消费范围才可为0。退款独立列出，缺原单引用保持缺失，不推断净额或商户/服务/卡关联。

同步成员接口通过spawn_blocking执行，单个来源实例最多两个后台工作，槽位用尽直接拒绝。HTTP/Runtime可取消等待，取消后结果不会被该请求保存；已运行的同步页读取不能强制中断，槽位持续占用到工作实际结束。因此调用方必须保证本地读有界，本版本不能接无法取消的同步网络实现。生产异步消费来源另待交付与合同确认。

## 保存与恢复

直接复用 `/api/agent/messages` 受检规划、原execute/continue及 `GET /api/agent/tasks/:task_id`。模型只看到上述工具定义，不得到owner或来源授权。登记不提供单独账单HTTP直执行路由、动态插件注册或反馈写入口。

报告正文和原ToolResult摘要在现有只读结算事务中保存，查询原号不重跑成员库或取数。进程重建/合法新会话沿原owner恢复。已完成任务的旧版本execute按现有防重合同返回冲突；不为报告重试放宽永久领取。来源/批准变化后旧计划execute/continue拒绝，历史不被改写或补查。

来源ID、来源版本、模式、固定成员提交、实际lib/query/spending字节摘要、适配源码摘要、输入版本及分页限制共同生成implementation_revision，并进入原持久注册摘要。来源实现发生变化，启动方必须更新其显式revision；接口不能自动证明任意trait实现未改变。

## 验证和未接项

底座隔离测试原样回放basic_spending的mixed、empty、page-failed、unknown-refund。原业务字段逐项对比成员expected；后端报告ID单独验证，未改原JSON。仅测试构造器使用原user_demo和冻结后端时钟，账户来源复用已交MockTransferBank::demo；默认公开模拟登录仍为user_001。无生产来源，因此这些证据不等于账单正式接通。

真实消费PagedSource及其异步取消/分页协议、正式C-TXN映射仍待团队对接，不能用余额读取或transfer_ledger_v1替代。BILL-04反馈仓储由Jonathan提供；未来仓储应借用底座传入的 `&mut sqlx::PgConnection`（SQLx0.7.4），不另建连接池或自己提交。底座收到业务模型/迁移建议后纳入唯一公共迁移链，并在统一事务中保存业务结果、公共记录、审计/Outbox；修改/撤销等未定D-20规则仍待确认。本只读工具不能保存反馈或授权锁卡。
