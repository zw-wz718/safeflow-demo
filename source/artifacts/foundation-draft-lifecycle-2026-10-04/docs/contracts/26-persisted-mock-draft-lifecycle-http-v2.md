# 同任务持久模拟草案生命周期 HTTP v2（合同26）

日期：2026-10-09。实施依据为工作区 `docs/handoff/NEXT_LIFECYCLE_IMPLEMENTATION_TASK.md`
和 `artifacts/persisted-lifecycle-design-review-2026-10-09/REVIEW.md`，受审设计 SHA-256：
`BF6D0BFD99D862E4FB571D8D5D5933E01FB63CB88CABDB748BA8B841AA7B4239`。
这是底座技术实现合同，生成交付待独立验收；不声称组员签字或新增业务批准。
业务三动作、参数、状态、归属和业务版本沿用合同10及实际成员代码，未修改成员实现。

## 版本与启用

成功响应 `contract_version=2`、`mode=PROJECT_SIMULATION`、
`scope=MOCK_DRAFT_LIFECYCLE_ONLY`。只支持显式创建的新任务，不转换合同22/24旧任务。
默认关闭；可信装配调用 `DraftConfirmationConfig::with_lifecycle_v2()`，可执行服务设置
`SAFEFLOW_SIM_DRAFT_LIFECYCLE_V2_ENABLED=1`，并提供已有的明确草案配置。
只适用于单个进程共享的真实模拟 Store；PostgreSQL 保存公共任务、计划、确认、动作和历史，
不保存或恢复成员业务草案。不是资金转账、正式身份、Permit、PRE_COMMIT或C-AUD。

## 严格请求

所有 POST 保持既有 Cookie、Host/Origin、`x-safeflow-request:1`、JSON Content-Type、
5秒正文读取期限和8KiB正文上限。每个 DTO 必填下述字段，未知字段、递归重复解码键、
浮点、超深输入拒绝；转义后的同名键也视为重复。用户、可信计划、confirmed、许可、
业务成功、TTL和任意服务器来源不能混入。每项均包含：

```json
{"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY"}
```

路径根为 `/api/sim/draft-lifecycles`，T为原任务号、O为原公共动作号。

| 方法与路径 | 完整 DTO 除公共版本/scope外的字段 | 结果 |
|---|---|---|
| POST 根 | 无 | 固定本人账户account_001、候选payee_008、200.00 CNY、空备注；仅 checked Create任务/P1/O1，未准备、未写Store |
| GET/HEAD `/:T` | 无正文、无query | 当前活动动作及真实公共历史投影，不准备或执行 |
| POST `/:T/actions` | `selection_ticket`, `task_revision`, `plan_version`, `facts_version`, `action`, `parameters` | 检查当前等待点或替换未领取尾部，产生P+1与新O；同ticket完全相同请求返回原O |
| GET/HEAD `/:T/operations/:O` | 无正文、无query | 原O动作与历史；任务版本/状态可已推进，动作版本另列 |
| POST `/:T/operations/:O/prepare` | 无 | 核验当前真实来源，冻结预检、确认与展示票据，不调用写工具 |
| POST `/:T/operations/:O/confirm` | `confirmation_ref`, `display_ticket`, `action`为CONFIRM/REJECT | 记录真实展示对应的明确动作；不执行，REJECT不业务取消 |
| POST `/:T/operations/:O/execute` | `confirmation_ref`, `task_revision`, `operation_revision`, `plan_version`, `facts_version` | 原确认与原版本比较，逐动作永久领取后执行一个实际Tool |
| GET/HEAD `/:T/operations/:O/current-draft` | 无正文、无query | 另行读取真实Store，独立scope，不结算未知领取或赋予执行权限 |

actions的 `action` 是 `CREATE_MOCK_DRAFT` / `MODIFY_MOCK_DRAFT` / `CANCEL_MOCK_DRAFT`。
Create仅允许显式替换未领取Create，parameters必须等于该任务最初的固定参数，包含
`replaces:null`；任何Create领取永久禁止第二Create。Modify/Cancel须读到原U/T的真实
完整READY_TO_CONFIRM草案，ID、业务版本、详情和期限必须匹配最近已核验成功回执。
选择参数是待检查数据，不是可信计划或确认。示例：

```json
{"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY","selection_ticket":"<server ticket>","task_revision":2,"plan_version":1,"facts_version":7,"action":"MODIFY_MOCK_DRAFT","parameters":{"draft_id":"<actual draft id>","expected_version":1,"changes":{"amount":"300.00"}}}
```

`task_revision` / `operation_revision`：正整数且不超过9007199254740991。
`plan_version` / `facts_version` / `expected_version`：正u32整数。
selection和展示票据为服务端产生的 `lifecycle_<32 lower hex>`，引用只定位与比较。
同ticket不同动作、参数或比较字段冲突；ticket消耗后的重放不会生成新O或续期。
首次创建/选择回包丢失且尚未取得原号时停止新增，人工核对；未提供跨任务恢复幂等协议。

## 响应

生命周期响应完整字段：`contract_version`, `mode`, `scope`, `task_id`, `task_state`,
`task_revision`, `plan_version`, `facts_version`, `operation_id`, `operation_state`,
`operation_revision`, `operation_plan_version`, `action`, `is_current_action`, `parameters`,
`observed_draft`, `target_parameters`, `selection_ticket`, `display_ticket`, `confirmation`,
`execution_status`, `unknown_operation_id`, `receipt_scope`, `receipt`, `current_draft_verified`,
`next_action`, `replayed`。不返回owner/session、数据库来源、裁决对象、授权对象或完整计划。

`observed_draft`是选择Modify/Cancel时冻结的Store观察，不是每次GET的新观察；
`target_parameters`为该冻结详情按实际changes合并后的目标展示。Create首次无观察，
parameters本身是创建展示。确认字段为 `confirmation_ref`, `status`, `prepared_at`,
`expires_at`，无当前可用确认时为null。同来源prepare返回原引用/票据和原期限，不续期。
过期/拒绝后需明确选择新P重新准备，不能用同P重做。

| execution_status | 真实含义 | next_action.kind |
|---|---|---|
| NOT_CLAIMED | 未永久领取 | PREPARE_OR_REPLACE_ACTION / CONFIRM / EXECUTE |
| SUCCEEDED | 本动作完整核验并已结算 | Create/Modify后SELECT_ACTION；真实Cancel后FINISHED |
| RESULT_UNKNOWN | 存在CLAIMED未结算，无可用成功回执 | QUERY_ORIGINAL_OPERATION，携带原O |

非当前历史动作通常为REVIEW_ORIGINAL_TASK；未知屏障优先于任务/操作外观状态。
Create/Modify成功后task为WAITING_USER，业务B分别1、+1；Cancel成功后task为CANCELLED，
业务B不增。每个动作新P、新O、新确认；公共O与业务operation_id分别保存，不互相替代。
最终Cancel对应公共Operation仍为SUCCEEDED，表示取消动作已成功。

历史 `receipt_scope=HISTORICAL_EXECUTION_RECEIPT`、`current_draft_verified=false`。
receipt字段：`action`, `operation_id`, `plan_version`, `expected_draft_version`（Create为null），
`draft_id`, `business_operation_id`, `draft_version`, `tool_result_ref`, `executed_at`。
真实Store观察响应为 `contract_version`, `mode`,
`scope=MOCK_DRAFT_LIFECYCLE_CURRENT_ONLY`, `task_id`, `operation_id`,
`current_draft_verified=true`, `draft`。draft字段为 `draft_id`, `business_operation_id`,
`draft_version`, `status`, `expires_at`, `parameters`（实际五项业务详情）。
观察true只表示本次实际读取，既不授权，也不代表历史仍是当前版本。
HEAD执行相同鉴权与读取、响应正文为空；其他方法拒绝。响应均no-store。

## 永久领取、来源与失败

任务先锁，核对不可变计划链、动作原始审计、所有操作、当前R/P/F、原会话与绑定版本、
两次真实预检、确认与完整摘要、装配代码/部署版本及微秒时间围栏。
v2使用 `TRUSTED_RUNTIME_LIFECYCLE_V2` 与 `lifecycle_anchor`，不放宽旧V1来源。
Store观察适用于Modify/Cancel；首次Create须核验无既存草案及Create领取，不能伪造Store。

CLAIMED事务明确提交后才调用一个真实工具；随后真实回读、可信回执和POST_EXECUTE通过，
才在同一公共结算事务落Operation三迁移、动作检查点、历史回执、WAITING_USER/CANCELLED
及审计。Runtime发MockDraftActionVerified，Create/Modify不发TaskCompleted。
永久唯一约束覆盖O、T/node、T/P、一项未知领取和一项Create领取。
领取前失败无Tool；领取COMMIT不明、调用中断、业务保存后核验/审计/结算失败都不重领，
无完整结算就保留CLAIMED。没有TTL解锁、管理解锁、自动重试或未知取消后门。
同execute的原引用/比较字段重复只查询原领取，异参冲突；新会话可按当前归属读历史，
不能因此新增执行。提交回包丢失后只GET原T/O，不能自动POST或换号。

旧版本历史按当时冻结来源验证，包含准备/明确确认、裁决、领取、三迁移及结算审计；
历史读取不要求旧P等于新P，不重新评估Policy、不新增确认、领取、许可、工具或时间围栏。
成功历史在真实进程重启后可读；Store丢失时观察和后续写明确503，不从历史重建/重跑Create。
未结算CLAIMED重启后永久阻断全部后续动作，仍按原O查。

使用已有错误信封 `contract_version=1` 与原因码：400 VALIDATION_ERROR、401
NOT_AUTHENTICATED、403 FORBIDDEN、404 NOT_FOUND、409 IDEMPOTENCY_CONFLICT或
STALE_PLAN、503 DEPENDENCY_UNAVAILABLE、501未启用。错误携带已知原T并指导查询原任务；
不泄漏数据库诊断或私有配置。不同scope/version、disabled配置拒绝，新旧路由及通用
仓储/确认/裁决/续跑互不越权；普通任务GET对新模式只投影实际历史，不给CONTINUE。

## 输入与迁移

v2直接验证实际Tool Schema，显式projection_revision为bounded-direct-input-v2。
只新增一层封闭嵌套object及minProperties；嵌套required可缺省，所有可选Schema预先校验。
输入4KiB、Schema16KiB、全树64字段；空changes/null/未知字段、第二层对象、array/ref/
组合/未知关键字拒绝，标量与金额规则保持。旧V1仍拒绝嵌套changes。
计划最多256节点、64KiB，P/R增长溢出拒绝，历史不自动剪裁。
新装配来源绑定嵌套验证、动作Runtime/authority及生命周期存储三个源文件摘要。

仅追加迁移0013，旧SQL及Cargo.lock不变；新表为空，不回填旧模式、不迁移旧领取或终态。
合法尚未应用0013的旧Schema可恢复和查询旧任务；没有新模式表时拒绝生命周期模式。
0013已记录却缺少模式表、表存在但迁移未成功记录时，模式识别明确拒绝，不当作旧任务。
所有可信入口升级后方可启用v2，不能回滚到不认识新模式的旧二进制继续写服务。
停用v2须保留其历史与领取；回退版本必须识别新任务、拒绝写，并提供明确的只读能力。
合同24旧Create仍COMPLETED；合同25仅旧模式观察，未启用修改/取消。

## 运行与边界

可信HTTP完整例子：`crates/api/examples/lifecycle-http.ps1`，操作者必须明确启用
`-ConfirmDemonstrationActions`，脚本显示待确认动作和参数，再发送独立confirm/execute。
失败只报告已知T/O，不自动重试。永久PG/HTTP与真实进程测试位于
`crates/api/src/http_lifecycle_tests.rs`及同名子目录。

没有自然语言改口合同，因此本次提供固定可信HTTP流程，未接入普通客户页面/模型的
修改或取消输入。业务跨进程恢复、未知结果可信查单/解封、正式身份、资金和成员Outbox
仍待其各自合同与交付；本合同不替这些负责人补写业务能力。
