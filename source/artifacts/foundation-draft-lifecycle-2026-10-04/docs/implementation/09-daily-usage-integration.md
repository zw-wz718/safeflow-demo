# 日累计公共绑定与显式演示初始化

2026-10-18，生成侧本地候选。采用逐风固定收件 f0997ec1e0ebf1bfa88f7a44ffff25cac3cd0cc6，SQL读取实现41518bd0379b50efedbd9096a04f6860383f4704；规则 transfer-daily-usage-2026-10-17-v1 和 transfer-shared-demo-2026-10-17-v1。此文更新07中“日累计业务读取/统计规则待交”和“无显式启动初始化”的历史状态，不表示资金入口开放。

## 实际消费路径

`PermitStore::submission_transaction`及旧`transaction`统一在`open`取得用户级事务锁，再取任务锁；`consume`核验当前会话、完整绑定与许可后，**仅对封闭资金范围许可**在任何可写连接交出前调用公共`read_locked`。只读返回`daily_usage_snapshot()`，不能传入调用者的complete或零累计代替。普通许可不读转账统计，快照为None，提交前也不重读转账统计；相同owner的转账孤立记录或转账表不可用不再阻断普通许可。通用会话、归属、确认、防重放、任务及最终绑定检查继续执行。正式签发仍封闭。

### 2026-10-18 独立检查返修：范围依据

范围来自`VerifiedPermitSnapshot`的私有`execution_scope`，不是模型字段、客户端开关或仅凭`payer_account_id`存在与否推断。该对象没有生产构造器或Deserialize；本批只用既有子模块隔离夹具验证，不新增签发接口。新增迁移0018将范围保存为COMMON/TRANSFER，旧记录标LEGACY；触发器禁止修改范围，重复保存也比较范围，重连后从原记录恢复。历史迁移校验和不变。

TRANSFER消费必须读到同owner、同Runtime operation的不可变公共预留，核对完整许可绑定和payload，再复用`transfer_result::bound_request`严格解析已收到的完整成员MockTransferRequest，核对task/draft/业务号/版本。预留缺失或投影字段缺失返回DependencyUnavailable，绑定不一致返回PermitBindingMismatch；缺payer也不能退回COMMON。完整投影通过后才读取全账户统计、校验付款账户归属，最终提交前再次校验完整快照与统计日。

COMMON和LEGACY不读取资金统计；若其已封闭参数呈现资金结构，消费拒绝DependencyUnavailable，等待可信适配重新核验，不能由通用入口降级放行。LEGACY表示历史记录无领域证明，不从参数自动授予新资金范围；其参数已与原确认、指纹绑定，客户端无法通过删字段改变旧许可。真实资金签发仍未开放，不能将历史通用许可用于正式资金执行。

未来采用正式Tool/Profile后，由可信后端适配在核验领域时设置该私有范围，并沿用相同持久化、消费和最终检查；资金Profile必须选择TRANSFER并交付完整资金投影。此处不是新的工具合同批准，也未将字段启发式作为资金范围的正向授权来源。

`ExecutionRequestStore::daily_usage(session)`提供后端只读查询，相同锁与相同校验；owner来自版本化服务端会话，无客户端owner/account范围。它返回全部本人SQL账户、数据库观察时刻、绑定事实与成员计算结果，不作限额或许可裁决。

公共适配直接调用成员`FundsStore::daily_usage_facts`，不重写成员业务SQL；额外SQL只枚举需保护的原单身份、完整账户范围和双向完整性。原业务表有单但缺资金单、资金单无公共正式提交、只有预留、公共已提交完整资金DTO但缺业务单均报错。草案及未提交的预留不计数。空快照可以为零，但读取失败/缺行/冲突不能退化为空。

逐笔使用`stored_row`验证公共原请求摘要、指纹、first SUBMITTED快照及稳定业务号；使用既有`verified_record`精确比较完整MockTransferRequest、原幂等键、task/draft/业务版本、付款账户、金额、最新回执及双边账务。只将实际`first_submission.recorded_at`绑定到`submitted_at`，且不得早于预留/晚于观察时刻。Runtime公共operation与业务operation继续分开；业务完成时间不充当提交时间。此DTO仅沿已有后端适配，不是新冻结的模型或HTTP资金Tool Schema。

资金范围绑定完整后才调用原成员`calculate_daily_usage`：本人全部付款账户、北京时间零点、成功归原提交日、往日未决占今日、明确失败释放、同业务号不重复、收款不抵扣。资金事务最终提交前重新核验新原单完整性；跨日的长事务拒绝提交并回滚，下一次重新统计。

## 并发约定

锁序：写侧公共用户锁 → 当前任务锁 → 所有既存成员业务operation advisory锁（owner/operation排序）→ 业务事实/公共首次提交核对；日累计批量核对不逐笔取得历史账户行锁。单笔提交结果与显式reconcile才沿成员原有账户ID顺序保护目标账户并保持至COMMIT。原号只读恢复使用REPEATABLE READ READ ONLY，不取得这些锁。新事务显式READ COMMITTED，等待后重新读取，并重新检查会话、绑定、事实与许可。

成员业务operation锁复用原`transfer-business-operation-v1`键，与成员`resolve`一致；统计后到提交前，已有未决原单的成功/失败释放不能穿插。写侧读取先等到正在更新的原单完成，再重新取成员事实；`original_transfer`保持无锁只读快照，`reconcile_transfer`按公共owner → task → operation取锁。不会在当前任务锁内循环调用另开事务的by_business_operation。

原号重放在新消费前返回原公共回执，不调用work、不再次计数/扣款/入队，统计损坏时仍保留原号只读恢复。禁止将成员裸提交API当成公共生产提交入口；任意SQL写入者或绕过公共组合的未授权新单写入不在此并发保证内。成员后续单笔resolve复用的operation锁已实际测试。未修改成员锁/业务语义。

## 显式启动

只在受信任后端进程环境配置：

```powershell
$env:SAFEFLOW_SHARED_DEMO_ENABLED='1'
$env:SAFEFLOW_SHARED_DEMO_RECIPIENT_OWNER='<明确采用的收款owner>'
$env:SAFEFLOW_SHARED_DEMO_RECIPIENT_ACCOUNT='<明确采用的收款account>'
```

无配置不启用；启用但缺ID、非法值或禁用与ID冲突拒绝启动。没有默认新增ID，不把成员fixture-li-*当共同ID。`SharedDemoPlan::prepare`仍负责已批准校验。底座`PgRepository::initialize_shared_demo`在一个连接/事务中按稳定顺序调用原provision_account/provision_payee，冲突整体回滚。启动成功后实际采用`TransferSqlBankGateway`覆盖SQL余额，目录仍用既有MockBankGateway，项目模拟身份模式不变，不新建可登录身份。

1288/0/50仅是首次初始化值；已存在账户由原接口核对owner/opening，绝不以启动UPDATE覆盖booked。重新连接初始化后余额不重置，备用user_002/account_002仍50。收款具体ID由部署明确提供，本候选未替团队选定正式新ID。

## 安全依据与剩余依赖

已读根目录《SafeFlow_自适应风险认证与安全检查规范_v0.1.md》6.2：execute_transfer日累计不超过1000元最低Yellow，超过1000元最低Red；风险分数只能提高认证强度。这个1000元是既有认证等级依据，不是新日限额，不是200元自动豁免；本轮未据此伪造实际裁决。合同17要求实际采用Profile/Policy和可信事实决定本笔门禁，条件需要时核验认证，不把所有金额统一要求强认证。

- 底座安全负责人＋逐风/政策负责人：采用的最终资金Profile/Tool Schema、中央规则版本及其可信风险来源；已交日累计口径与SQL事实不再列待交，不新增日限额。
- 底座身份/认证负责人（具体提供者待确认）：实际本笔认证核验来源及撤销/有效期/绑定；只有实际裁决要求强认证时才消费该证据。
- 结夏＋底座公共存储：真实PG卡片/锁/归属及revision维护、冻结/解锁与资金共用锁到COMMIT的协议。不能把Allowed测试参数或读两次相同版本当生产锁。

因此request_issuance、正式PRE_COMMIT/资金入口仍拒绝。隔离测试只构造原有封闭许可验证公共事务，并用成员批准的模拟行为测试；不是实际资金安全放行或成员整链验收。真实LLM、真实银行、外部审计发送、本机API进程级启动/真实跨午夜持续运行未在本轮验证。跨日用例使用实际公共时间＋明确标记的合成次日观察时钟。

## 验证与证据

交付根目录`artifacts/daily-usage-integration-2026-10-18`含前后manifest、原字节导入摘要、实际日志和DELIVERY；`scope-fix`单独保留本次返修基线、旧实现失败证据、指定回归和差异摘要。`crates/storage/examples/public_daily_usage.rs`可在本批隔离schema新进程读取公共绑定，不提供签发。两份成员JSON保留原字节；本批真实输出另存，不覆盖成员材料。只追加范围迁移0018，未重做前端、恢复机制或ZIP。
