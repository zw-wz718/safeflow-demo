# 付款意图的受检投影与 Agent 调度 v1

日期：2026-10-19。底座当前技术采用版，不表示成员新增业务合同已签字。复用合同02/03/06/17/28/29/31/32/34和已接逐风SQL，不变更成员原请求、备注或版本语义。

## 工具和能力边界

Agent生命周期装配明确登记 `execute_transfer`，Schema 1、Policy `payment-intent-boundary-v1`、最低等级Yellow、执行路径 `PAYMENT_INTENT`、投影 `payment-draft-locator-v1` 成对校验。只读插件注册仍只接受无副作用工具；新增路径只在受信任装配中启用。不得把此最低等级降低为Green；现有更高最低等级及中央风险计算/审计不改。

公开参数严格为 `{draft_id,draft_version}`：稳定业务草案号和正u32业务版本，无额外字段。定义唯一来源是 `safeflow_contracts::PaymentDraftLocator`。它不是完整转账DTO、Runtime计划版本、确认或执行指纹。所有旧direct-input比较保持；该投影明确不能供direct/lifecycle操作安全评估使用。

注册Tool提供规划目录与直调拒绝边界；真实消费者是统一Agent分发，调用 `PaymentPreparationService::prepare_for_agent`。注册Policy拒绝将定位参数直接作资金事实；已有真实 `FundsBusinessPolicy` 仍由付款确认/显式评估在同事务内读取完整可信事实后调用。没有虚拟成功来源或新的资金许可入口。

`GET /api/agent/status`、Tool说明和付款响应均区分 `payment_planning_enabled/planning_enabled=true` 与 `funds_execution_enabled=false`。这是一条可运行的付款准备、确认、实际预检和安全等待链，尚不能扣款或报告付款成功。

## 生命周期兼容和持久关联

现有Agent草案路径与付款准备都使用 `mock_draft_lifecycle_tasks` 及不可变历史计划。付款确认严格依赖原计划摘要和 `lifecycle_anchor`，因此不以新付款定位计划覆盖历史草案计划。

受检付款计划必须恰为一个无依赖的 `execute_transfer` 节点，任务与服务端选择上下文一致。规划版本为当前生命周期计划版本加一，只表示已检查的付款提案；不冒充已安装的生命周期计划。模型不能在同一计划混入读取、草案修改或其他写动作。

后端在原有准备事务中校验受检对象的Runtime实例、装配摘要、本人会话，以及当前任务revision/plan/facts/selection_ticket；从本人现存草案/成功回执读取完整 `PaymentSnapshot`。复用独立准备和确认记录，不创建第二套付款或确认表。

迁移0023仅增加不可变 `payment_agent_bindings`，关联原准备operation、完整受检计划及摘要、规划来源、装配摘要、公开定位参数、完整快照摘要、历史生命周期计划摘要和独立确认引用。关联、准备、确认准备审计、公共可靠事件一起提交；关联/审计/提交失败整笔回滚。查询重新核对其完整对应关系与审计，并继续原 `saved_payment` 全量历史校验。

同一选择重复请求复用同一准备与首次关联。新的候选计划仍通过当前注册、身份与参数约束；仅单节点的模型节点ID不同视为同内容重放，不改写首次计划、摘要或审计。其余受检计划/装配差异仍冲突，Agent准备中的真实幂等冲突返回HTTP 409。修改金额、收款人、备注或取消使旧确认失效；新版本可独立准备。关联不占 `public_execution_requests` 的业务号，不生成正式资金幂等键，不复用草案创建确认，不改参数hash或清空anchor。

## HTTP状态和下一步

`/api/agent/messages` 的受检模型提案和显式启用的 `/api/agent/samples` 样例 `EXECUTE_TRANSFER` 经过同一分发。必须提供原草案任务的当前context。返回 `kind=PAYMENT`、当前 `draft`、完整 `payment` 和 `payment_flow`，`can_start=false`，没有伪造的成功ToolResult。

旧READ/DRAFT/CLARIFY响应省略不存在的payment/payment_flow字段，保持原严格网页合同。status新增的两个能力布尔字段由网页显式校验为可选字段，未知字段仍拒绝。当前网页仅提供原查询和草案交互；新PAYMENT交互仅HTTP/脚本可用，未实现付款页面，不将其映射为DRAFT或付款成功。

| payment_flow.status | 实际含义与动作 |
| --- | --- |
| WAITING_PAYMENT_CONFIRMATION | 展示保存的完整付款；沿next_path调用原付款confirm，以本付款confirmation_ref/display_ticket明确CONFIRM或REJECT |
| WAITING_EXECUTION_CONDITIONS | 已确认但缺最终安全执行条件；可GET或明确调用原evaluate刷新真实最低等级/预检，不能自动重试付款 |
| ENDED_INVALIDATED | 已拒绝、过期、会话或草案变化；只读历史可恢复，不恢复确认权；按现有生命周期查看/选择当前草案 |
| QUERY_ORIGINAL | 原单/提交标记/公共原号已存在；响应original来自现有original-result服务，后续只按原号查询 |

`GET /api/agent/tasks/:id` 恢复最新已关联付款，并包含原付款GET一致的失效原因/最低等级/业务观察。`POST .../execute` 和 `POST .../continue` 对已关联生命周期付款只校验当前task_revision/plan_version/facts_version并读回同一付款；continue另比较响应中的waiting_node_id。它们不消耗Runtime步骤重试次数，不再次确认、不写观察、不签发或消费许可。

同owner的新有效会话可以查历史，旧会话仍无效；重启不恢复旧确认权。已有PROCESSING或未知原单只能查询，返回实际 `RECONCILE_REQUIRED` 等原协议状态；不改写为已成功，也不创建新的可执行单。

## 可运行例子与剩余条件

`crates/api/examples/payment-agent-http.ps1 -Url http://127.0.0.1:8787 -ConfirmDemonstrationActions` 使用已有服务的公开HTTP：固定计划建草案 -> Agent准备 -> 明确草案确认/保存 -> Agent付款提案 -> 展示完整付款/独立确认 -> 实际最低等级和业务预检 -> Agent继续安全等待 -> GET原任务恢复。需要已有专用模拟身份、账户数据及 `SAFEFLOW_AGENT_FIXED_PLAN_ENABLED=1`；原私密登录口令从环境读取，不写代码/输出。示例不是自然语言模型或扣款演示。

正式资金指纹/最终唯一裁决/封闭Permit/同事务消费仍待真实卡事务仓储及共同锁协议、已确认本笔验证政策等证据齐备后接入。当前业务观察不升级为最终授权，资金Operation仍DRAFT，不写正式执行键/余额/资金原单/流水。已有内部登记、原号查询、付款准备、累计加本笔1000元规则和真实业务预检均已存在，不再列为未开发。
