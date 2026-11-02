# 2026-10-17 集中接入

生成自测交付，独立验收另记。唯一产品树保持不变；成员下载原件只读。

## 来源与边界

- 逐风：1f0e46db7d08051140d42ac195af5f207f2d7062。保留已验收FundsStore/BusinessStore、mock_execution、0015/0016、原7组SQL JSON；新增公共结果适配，不改业务源码。
- 结夏：7ec2216343b4c33fbf89908800e7165c03e834c0。features/card/src、runtime-adapter/src及测试原样导入，仅适配Cargo工作区。底座会话/注册封装在crates/api/src/card.rs。
- 风星龙：f22842ad9c39669105bc2bb8a02918703094c1ec。实读SQLite接收/去重、HTTP ACK和v0.3，不将PG提案或未确认映射并入公共协议。
- 依据原安全底座/转账PRD及合同00/01/02/03/04/05/06/12/17/18/28。D02关联账户全部出账限制、D21有效本人主动锁规则已确认。SQLx实际为0.7.4，成员资金方法借PgConnection；旧CARD-I03B的0.8.6/Transaction不再适用于当前版本。

## 实际启用的公共Outbox

追加迁移0017，1..16不变。audit_events的AFTER INSERT触发器在原事务保存public_audit_outbox，实际Agent、lifecycle、许可、原号登记和结果入口自动使用。入队失败导致原事务失败，包含延迟COMMIT故障。没有独立提交的演示队列。

保留原event_id、sequence、可信tasks.owner_id和完整原始公共事件。event_id来自既有数据库持久UUID；enqueued_at是入队时间，public_event.occurred_at是原公共存储事件时间，均不冒充银行业务发生时间。升级回填历史事件并标historical=true，不声称过去已送达。envelope、关联和入队时间不可改写/删除，ACK只能从空写入一次。

PublicAuditOutbox::for_trusted_backend(repository).pending(limit)按原序列读取待发事件；失败/重启重读保持原ID和内容。它是有权限的后端工作者接口，不是跨用户公开查询。并发工作者可能读取相同事件；接收器须按原ID去重。

acknowledge(event_id, expected_digest, receiver, receipt_ref)只保存后端已核验的接收证据；相同ACK幂等，内容摘要/接收回执冲突拒绝。不解释外部HTTP ACK，也没有自动发送器。未来接收适配依据采用合同核验accepted、event/task/operation/audit_id后再调用。本轮实际事件保持待发送；本地ACK隔离测试不代表风星龙已接收。

ApiState::public_audit_outbox_for_backend()提供同库工作者访问。迁移成功后持久入口自动入队，审计故障不重做草案或资金。

## 资金结果与事务接缝

PermitTransaction::record_transfer_result()只能在当前包装consume及record_submission成功后调用：锁定实际业务原记录，通过FundsStore::query读取；核验原键、业务号、task、草案号、独立业务版本、完整成员请求；核对成功时恰有匹配的双边流水，非成功时无完成流水；按合同01迁移公共Operation至PROCESSING/SUCCEEDED/FAILED，并同事务写关键公共audit/outbox。最终提交守卫再次核验，任意错误会毒化并回滚事务，吞错也不能提交。

完整参数比较使用收到的MockTransferRequest原类型及精确JSON，不猜部分字段映射。这只是受信任后端内部适配表示：未冻结资金Tool Schema，模型/HTTP未采用此表示，其他参数形态拒绝。现有草案三动作不冒充资金提交。

首次公共提交仍冻结为SUBMITTED；成员首次和当前回执分别保留。ExecutionRequestStore::original_transfer(session, original_key)按原号读取二者；reconcile_transfer只根据同一实际原记录推进公共状态，不提交、不创建新号、不选择结果、不调用FundsStore::resolve。缺原单保持ReconcileRequired，读库失败保持StorageError，不把未知改判失败。会话等待前后复核，重启只需当前本人读权限，不重签/消费许可。

结果更新Operation和task revision，保留WAITING_EXTERNAL和ReconcileOperation检查点，供Runtime另行审查/继续；银行成功不自动完成整段Agent计划。

```text
PermitStore::submission_transaction(session, request, callback)
  已有首次记录 -> 返回原公共首次回执，不进入callback
  callback:
    consume(已签发许可, 当前绑定)
    FundsStore::submit(unit.connection(), 可信owner, 原key, 当前业务请求,
                       可信模拟行为, 覆盖COMMIT的实际出账裁决)
    record_submission(原公共请求)
    record_transfer_result()  // 原单/流水核验 + 公共结果 + audit/outbox
  包装层最终复核，独占COMMIT/回滚
```

这是事务接缝，不能复制测试里的MockOutgoingState::Allowed作为正式权限。request_issuance和Runtime资金PRE_COMMIT仍关闭，VerifiedPermitSnapshot仍封闭，没有HTTP扣款入口。测试中的封闭许可仅证明公共机制，不代表最终安全放行。

## 账户与卡锁装配

TransferSqlBankGateway::for_trusted_backend(repository, trusted_catalog)实现现有BankGateway：catalog核验并提供脱敏显示信息，实际FundsStore在同一只读快照读取本人SQL可用余额替换catalog余额。缺账户/读库失败不回退演示余额，不从掩码推导路由或重命名成员样例owner。find_payees沿原catalog查询，不证明这些payee已有SQL路由。

受信任启动代码可将它传入ApiState::for_project_simulation_with_bank。默认可执行文件保持已声明本地模拟来源，不自动初始化资金账户/路由。可信来源明确后调用已有FundsStore初始化，无需重写账户业务。

ApiState::card_facts_for_backend(actual_source).query(session, account)使用实际API会话，复用成员完整分页/revision、归属、有效锁和末次版本检查；读取期间会话改变拒绝。它只是快照，不是覆盖资金COMMIT的锁。

card_facts_registration_for_backend(source, approved_profile)组合收到的Tool/Policy及固定源码版本，经现有公共只读注册和启动校验。没有默认Profile或来源，中央须明确批准拟议名称/Schema/等级后才能装配。默认可执行文件未注册。隔离装配测试不冻结公共协议。

## 待对接及未验证

| 负责人 | 具体缺失材料 | 影响与恢复条件 |
| --- | --- | --- |
| 底座安全负责人＋逐风/政策负责人 | 采用的资金Profile/Schema、日累计/跨账户统计、D-08日界线/处理中跨日口径、规则版本和可信事实、本笔认证来源 | 阻塞最终READY_TO_EXECUTE及生产许可工厂；明确规则/来源后接封闭签发和本轮事务，不以200元豁免。不重索已交业务SQL |
| 账户来源负责人待确认；逐风＋底座装配 | 稳定账户owner、opening余额、payee到recipient_account_id的真实初始化/更新来源 | 阻塞默认SQL账户/路由启用；材料可核对后调用已有初始化并启用SQL Gateway，不造账户业务 |
| 结夏＋底座公共存储 | 卡/锁/归属实际PG仓储、全量revision维护、冻结/线下解锁写入与资金共同锁定顺序/协议 | 阻塞最终出账放行和正式冻结写工具；业务方给实际来源/表提案，底座统一迁移并验证锁到COMMIT。D02/D21无需重确认，两次版本相同不解决竞态 |
| 底座C-AUD/T7＋风星龙 | 原公共事件到接收器kind/action/result、主体/动作/业务号、可信业务发生时间的采用映射与接收确认合同 | 仅阻塞外部DTO/发送/ACK解释；公共Outbox已持久。确定映射后补原事件不重做业务；对方当前SQLite接收器也无正式鉴权 |
| 底座接入，逐风已提供 e72084a | 原Q3恢复API已接入，见 [08-draft-restart-adapter.md](08-draft-restart-adapter.md)；旧记录可能没有持久搜索词 | 完整ReadyToConfirm草案可重启编辑；无搜索词的旧记录需要现有入口中新的明确输入，否则拒绝恢复 |

未验证：真实银行/LLM、正式许可及完整200元资金HTTP链、真实PG卡锁并发、外部审计接收、成员机器复现/发布。各项负责人、影响和恢复条件按上表。默认候选可运行固定Plan、只读、模拟草案三动作、完整SQL草案/公共回执及自动公共事件入队。
