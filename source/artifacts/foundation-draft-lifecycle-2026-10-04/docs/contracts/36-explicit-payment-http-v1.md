# 显式付款 HTTP 与安全原单投影 v1

2026-10-20。底座本地技术采用，接续合同17/28/29/31/32/35及已经独立验收的最终付款组合器。它不批准共享实例的卡目录、真实银行或真实LLM。旧合同的历史交付结论保留，本增量以当前代码及本轮独立审查为准。

## 后端装配与能力

`ApiConfig.payment_execution` 默认 `None`。部署代码可以用 `PaymentExecutionConfig::for_reviewed_project_simulation(source, profile, permit, behavior)` 显式构造，再交给 `ApiState::for_project_simulation_with_bank` 或 `for_project_simulation_with_read_only_tools`；bank由调用方明确提供。带付款配置时，隐式创建MockBank的 `for_project_simulation` 拒绝启动。

构造器复用 `payment_final_registration_for_backend` 校验固定tool `execute_confirmed_payment` / schema1 / policy `transfer-funds-business-2026-10-18-v1`。生命周期Runtime安装既有真实最终Policy和FundsBusinessPolicy，与API的版本化SessionResolver、Kernel及准备服务共用。最终实现仍为 `locked-funds-policy-2026-10-20-v1`。本轮不重造资金、许可、锁和Outbox协议。

配置只有受信后端代码入口，没有反序列化、HTTP注册、任意JSON/环境provider loader。模型及请求不能选择provider、owner、Policy、风险等级、业务结果或成功callback。`MockBehavior`明确表示项目模拟服务行为，不是用户可填结果。

只有付款配置存在、source已显式绑定且生命周期Runtime存在，`funds_execution_enabled`与`execution.enabled`才为true。这表示入口已装配；来源当前授权/revision、卡状态、资金规则等仍在明确执行时复查，不保证一定获准。未装配时false，POST失败关闭；来源失效时执行拒绝。

准备/确认/评估及Agent PAYMENT响应保留原付款DTO，新增：

```json
{
  "execution": {
    "enabled": false,
    "reason": "PAYMENT_DEPENDENCY_NOT_ADOPTED",
    "execute_path": "/api/sim/payment-preparations/<task>/<preparation>/execute",
    "result_path": "/api/sim/payment-preparations/<task>/<preparation>/funds-result",
    "automatic_retry": false
  }
}
```

启用时reason为`RECHECKED_AT_EXPLICIT_EXECUTION`，`execution_conditions=[]`；未启用保留原缺项。旧响应缺少execution或新增能力字段时，网页按未启用处理；其余字段继续严格校验。

## 明确执行命令

`POST /api/sim/payment-preparations/:task_id/:operation_id/execute`

operation_id是原付款准备操作，不是资金执行号。本人在已显示完整付款信息并独立确认后，才可明确调用：

```json
{
  "contract_version": 1,
  "scope": "PAYMENT_EXECUTION_ONLY",
  "confirmation_ref": "原付款确认引用",
  "display_ticket": "原付款显示票据"
}
```

仅这四个字段，不接受金额、owner、账户替换、幂等键或授权声明。沿既有Cookie会话、精确Host/同源Origin、`X-SafeFlow-Request: 1`及application/json保护；拒绝query参数、未知/重复JSON键、错误scope/version、非POST及超过8192字节的body。继续使用请求deadline和既有错误信封。

调用已验收 `PaymentExecutionService.execute`，检查原确认/快照/任务与草案锚点及最终安全条件。同号并发和重放复用实际原单；不能增加许可或第二笔资金效果。原准备DRAFT、历史计划和快照保持，普通任务/原锚点校验未放宽。

成功HTTP响应也重新查询持久原单后输出下述安全DTO，不直接序列化 `PaymentExecutionResult`。HTTP超时或断线可能发生在COMMIT之后，不能据此判定失败，前端不得自动重发POST。

## 安全原单查询

`GET /api/sim/payment-preparations/:task_id/:operation_id/funds-result`

只有路径选择器，拒绝query参数。当前有效同owner会话可查询；原确认到期/新会话、source关闭、付款配置撤除、甚至没有draft Runtime时仍可读取历史。不会恢复执行权限。

`PaymentOriginalReader`只持有PgRepository和API使用的SessionResolver，在`REPEATABLE READ READ ONLY`中校验owner、任务模式/历史、原准备与完整快照及公共/业务原单关联，并在等待与提交前后检查版本化会话。查询不初始化目录、评估付款、消费许可或写账。

POST与GET均输出严格九字段：

| 字段                     | 来源/含义                                                               |
| ------------------------ | ----------------------------------------------------------------------- |
| contract_version / scope | `1` / `PAYMENT_FUNDS_RESULT_ONLY`                                       |
| task_id                  | 原任务                                                                  |
| preparation_operation_id | 原准备operation                                                         |
| snapshot_digest          | 保存的完整付款快照摘要                                                  |
| transfer                 | 原保存的完整MockTransferRequest；金额/收款人/备注/版本/业务原号均不重建 |
| state                    | 原单核验状态                                                            |
| submitted                | 已发现公共提交或业务原单，不等同成功                                    |
| automatic_retry          | 恒false                                                                 |

没有内部idempotency_key、fingerprint、permit、authority、provider/source或完整business-store记录。`transfer`仍包含合同约定的业务task/draft/owner/account/operation标识，用于完整关联核验；不是脱敏的银行回执。

状态保持原协议：`UNSUBMITTED`、`REGISTERED_UNSUBMITTED`、`PROCESSING`、`SUCCEEDED`、`FAILED`、`RECONCILE_REQUIRED`、`SOURCE_CONFLICT`。只在原单证据满足时报告成功/失败；不以超时或客户端缓存猜终态。

合同32的`original-result`及Agent的legacy `payment_flow.original`为兼容仍保留旧完整协议（其中原有内部键字段未在本轮删除）；它们不能被描述为新的九字段安全DTO。旧GET现也复用独立reader，不依赖draft Runtime。客户的新执行/原号查询使用funds-result；Agent恢复增加安全`payment_flow.funds_result`。

## Agent与网页

2026-10-20第四次返修：所有异步恢复写入须在写盘前核对发起组件的生命周期、当前会话代次和认证状态；卸载、StrictMode effect重放或会话失效后，即使旧请求仍成功/失败返回，也不能提交旧集合或结束新组件的待查询。任务GET失败分支在同一校验内；取消、超时或失效会话不清除中断查询入口。旧请求的finally也不能清除新查询的busy状态。同步入口在未挂载/未认证时拒绝写入。

同一存储对象跨会话时只清内存核验结论，并将遗留任务查询标记为需重放原单读取；此步骤不写盘、不清日志或原号。重新挂载继续从三份持久记录合并。新页查询B并保留A/B后，旧页任何回调不得改动这些持久值。逐笔核清后的正常新办理、未提交原单读完后的解除路径、容量和部分提交保护仍沿用下文规则；不新增资金或后端会话规则。

准备后的信息确认仍调用原`confirm`，按钮为“确认付款信息”；启用时确认后的flow为`WAITING_EXPLICIT_PAYMENT / EXECUTE_PAYMENT`，next_path指向显式execute。未启用保持`WAITING_EXECUTION_CONDITIONS`。已有原单转`QUERY_ORIGINAL`。Agent execute/continue、付款evaluate及所有GET不执行资金；模型`execute_transfer`仍只是准备付款。

银行转账页“在线模拟付款”直接复用后端Agent会话、草案、确认、执行和原号查询；“本机草案”继续原本地适配器。服务失败不回退本地余额或成功记录。客户模式隐藏研发导航和内部检查，保留独立模拟账户/固定办理选项说明。

网页在POST前只将原号恢复选择器（task、准备operation、snapshot_digest、原transfer）写入sessionStorage；不保存confirmation_ref、display_ticket、许可、内部执行键或整份付款DTO。发现尝试或原单后关闭旧确认/修改/执行，结果不明或PROCESSING禁止换任务重付；明确终态后允许新办理。未提交GET也不解除已发POST的尝试锁。后续GET及刷新只接受完整匹配的原号/金额/收款人/备注/版本，旧会话响应丢弃。

2026-10-20首审P1返修：未核清付款A的恢复选择器和核验结果独立于当前查看的任务。允许只读查询其他任务B及其原单；B的未提交、确认、处理中、成功或失败响应、刷新和页面重载都不能解除A、覆盖A的恢复选择器或允许另一笔付款。跨任务时始终提供“查询保留的付款原单”读取A，查询A也不覆盖B当前展示的原单。

只有当前有效会话响应中与A的task、准备operation、snapshot_digest及完整transfer逐字段相符且submitted=true的SUCCEEDED/FAILED才能解除A的未核清限制。UNSUBMITTED/REGISTERED_UNSUBMITTED、404/读取失败、关联不匹配或旧会话响应不能解除；会话代次变化清除内存核验结论并保留原号，重新查询后才能取得当前会话可信终态。发现legacy处理中原单也建立同样保护，查询本身不产生付款。确认、草案写入与明确付款处理函数继续检查未核清状态及同步busy；匹配终态后可正常新办理。

2026-10-20第二次返修接续：上述保护逐笔适用于所有本页已发现的原单。POST开始前和safe/legacy原单发现路径均按完整选择器登记；重复读取同一选择器不新增记录，不按最后查看任务覆盖。A的匹配终态只核清A，B仍为PROCESSING/RECONCILE_REQUIRED/SOURCE_CONFLICT、尝试后未提交、不明或读取失败时继续保留B及全局写入限制；全部记录各自得到可信终态后才允许新办理。B首次被发现即为可信终态时可直接核清，不形成永久锁定。同会话较旧非终态投影不抹去已核验终态，旧会话响应仍被代次检查丢弃。

sessionStorage的`payment-attempts.v2`保存严格`{contract_version:2,scope:PAYMENT_RECOVERY_REFERENCES_ONLY,references:[...]}`集合，元素仍只有原有task/准备operation/hash/transfer选择器，不保存票据、许可或核验结果。兼容读取旧`payment-attempt.v1`单条记录并合并去重；旧key作为优先指向尚未核清原单的兼容指针，不代表完整集合或付款授权。新办理只能在全部核清后主动清集合。重新加载或会话变化清所有内存核验结论，保留全部选择器并逐笔重新查询；当前B的终态不能代替A的查询。损坏/不可读取或保存的本地恢复记录明确提示、阻断新办理，保留可读取原号与原损坏记录，不以格式升级或默认空集合冒充全部核清。

## 本轮采用边界

2026-10-20第三次返修：恢复集合提交采用本页sessionStorage写前日志`payment-recovery-journal.v1`，不新增后端跨订单锁。日志严格保存版本1、scope=`PAYMENT_RECOVERY_JOURNAL_ONLY`、PENDING/COMMITTED阶段、完整恢复选择器及待完成只读查询入口；不保存核验结论、付款票据、许可或整份付款DTO。初始预留131072字符；新发现查询发出前，按既有35000字节响应上限再确保日志有70000字符余量。预留不足或存储不可用时，拒绝发出无法可靠保留恢复入口的新发现查询；已有完整选择器的只读查询仍可用。不靠删除旧原号腾出空间。

任务GET及原单GET在请求前先登记PENDING入口；收到原单后，先将完整集合写入PENDING日志，再依次提交v2集合、v1兼容指针，最后将无待查询项的日志标记COMMITTED。任何一步失败均不清除之前的日志；即使v2仍是可解析的旧A集合，重载仍合并日志中的B/待查询入口并保持完整性故障。初始化旧格式迁移及已全部核清后的新任务清理也采用相同顺序。日志只记录集合/查询完整性，COMMITTED不表示任何付款成功或已核清。

重载读取三处原号并合并去重，全部outcome仍置空；PENDING或格式错误继续阻断新写。重载遗留的精确原单查询只能由对应完整选择器的funds-result完成，旧的Agent页面不能替代；遗留的付款任务发现也需原单读取，不能由一次无原单的旧付款投影直接清除。非付款任务可由其已校验的任务响应完成查询。会话变化只清核验结论，保留集合、查询入口及故障。

界面提供待查询入口与“重试保存付款恢复记录”。空间恢复后可重提完整集合，不能用重试按钮清空原号、清除未完成查询或伪造终态；解除每笔付款限制仍要求当前会话完整匹配、submitted=true的SUCCEEDED/FAILED。只有全部原单逐笔核清、没有待查询或完整性故障，且清理事务成功后，才进入新任务。损坏原始记录仍保留，不提供默认清空/自动放行入口；客观无法持久化时明确拒绝新写。

共享main仍`source=None`、payment_execution=None；用户决定先由结夏核对真实目录，未采用U1/A与K/1234候选。受控注入路径及隔离HTTP/PG成功不等于共享实例100元已完成。

PROCESSING原确认到期/新会话后的后端终结授权仍待采用；本轮没有resolve_success、续期旧确认、释放占用或猜成功接口。最小权限部署角色、Red逐笔凭据、线下解锁、外部审计ACK与真实LLM/银行沿原缺项保留。
