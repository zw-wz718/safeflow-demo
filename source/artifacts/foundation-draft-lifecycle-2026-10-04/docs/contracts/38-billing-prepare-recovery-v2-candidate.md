# 账单准备恢复 v2：隔离实现候选

2026-10-24。只描述本任务C盘候选；共享部署、成员兼容、来源、期限和配额尚未采用。合同37的原输入、复合交易、三choice、D20、成员保存及严格完成不变。活动main仍关闭本能力。

## 受信安装

`BillingRecoveryConfig::new(realm, namespace, source_id, source_revision, continuity, open_limit, retained_limit, write_owner, write_namespace, read_owner, read_namespace, allow_owner_cleanup)` 必须显式提供全部值。realm严格canonical小写UUIDv4；数量正、retained>=open、cleanup=true，无默认政策。另须显式ConfirmationConfig/PermitConfig。

`PgRepository::install_billing_recovery_for_reviewed_backend` 登记不可变realm/namespace/source/revision/continuity/policy摘要。已存在不同登记拒绝；`set_billing_recovery_enabled_for_reviewed_backend` 只暂停/恢复相同登记，不换realm或删除历史。调用者须从外部核实writer排空与备份连续性，方法本身不证明这些事实。HTTP无安装/配置接口。只有 `BillingFeedbackConfig::for_reviewed_backend(...).with_recovery_v2(config)` 装配v2；默认None。

原唯一迁移链追加0027，旧SQL不改。闭合writer锁序：安装共享→owner→nonce串行保护→稳定task→root/attempt→confirmation→permit/成员。普通owner/task入口也先核安装，安装变更取排它锁。登记后旧账单generic writer拒绝，原已保存GET保留。事务内私有标记不是客户字段或执行许可。

## HTTP与响应

全部原perimeter/Cookie/CSRF/no-store/8KiB/未知及重复字段拒绝继续适用。所有写须contract_version=2及预期recovery_realm；读query只允许原样单个recovery_realm。nonce/realm均严格UUIDv4。不自动重试、不降级v1。

| 方法/路径 | exact输入及结果 |
| --- | --- |
| GET `/api/billing-feedback/v2/context` | 无query/body；contract_version/scope/recovery_realm/available。禁用realm=null；此安装context不能代替实时session context |
| POST `/api/billing-feedback/v2/preparations` | `{contract_version:2,recovery_realm,nonce,intent:{input_version:1,report_id,transaction:{source_id,account_id,transaction_id},choice}}`；首次201 CREATED，原输入重放200 RECOVERY_ONLY |
| GET `/api/billing-feedback/v2/preparations/:nonce?recovery_realm=...` | 原根只读投影，无ref/session/binding/permit |
| GET `/api/billing-feedback/v2/operations/:O/preparation?recovery_realm=...` | 同一快照核原O/根/realm/完整关联；legacy拒绝LEGACY_UNFENCED |
| POST `/api/billing-feedback/v2/preparations/:nonce/confirm` | `{contract_version:2,recovery_realm,task_id,operation_id,confirmation_ref}`；本人明确动作，原session/期限 |
| POST `/api/billing-feedback/v2/preparations/:nonce/execute` | 同confirm输入；一次入场，成功SAVED；已保存不重执，应原GET |
| POST `/api/billing-feedback/v2/preparations/:nonce/abandon` | `{contract_version:2,recovery_realm}`；明确关闭，ABANDONED永久fence，不是撤回反馈 |

完整exact/null及关联规则见前端billing-recovery-client.ts。CREATED独有原confirmation引用，仅当前连续页面内存持有；读/重放只给历史确认状态及原期限，不补ref/续期。跨owner404；任一原账户失权时nonce读只给最小owner元数据、原号/参数/确认观测null，O读404，本人仍可专用减权关闭。

错误：400 INVALID_REQUEST；401 SESSION_UNAVAILABLE；404 NOT_FOUND_OR_NOT_OWNED；409 RECOVERY_REALM_MISMATCH/PREPARATION_KEY_CONFLICT/PREPARATION_ALREADY_OPEN/RECONCILE_REQUIRED/ALREADY_SAVED/LEGACY_UNFENCED；410 PREPARATION_CLOSED；429 PREPARATION_CAPACITY；501 DISABLED；503 UNAVAILABLE_OR_INTEGRITY。原perimeter错误保留原形状。写超时409未知，读超时503。

## 事务、状态及速率

prepare唯一RC事务恢复真实原归档、核完整账户与typed身份，创建原task/O/checkpoint/READY/确认/原audit-Outbox及根，最终task rev4/op rev2；不调用成员stage。整笔失败回滚，COMMIT后独立读回才CREATED。session不进内容幂等键；重放不新建确认。active claim拒绝换nonce重办同交易，保存后原D20仍停止。

五态PREPARED_OPEN/EXECUTION_ADMITTED/SAVED/CLOSED_ROLLBACK/ABANDONED。ADMITTED先独立COMMIT，确认应答后才产不可Clone/序列化私有token；丢应答不产token。成员保存/事件、公共结果/严格完成/audit-Outbox、SAVED与BUSINESS_COMMITTED同唯一业务COMMIT。提交不明只查nonce及原O，不重试/接管/取消。

仅原执行者未发送COMMIT且显式整笔ROLLBACK成功可产消耗型证明，封口CLOSED_ROLLBACK，再需明确abandon。Drop/超时/404/READY/无结果均非证明。abandon同事务撤原确认及ACTIVE许可、取消原task/O并留永久屏障。空nonce关闭形成全局墓碑吸收迟到prepare。已有根关闭不耗新容量，未知nonce满额无fence。根/尝试/墓碑不能GC或重置。

nonce/O业务快照使用RR READ ONLY，不补确认/业务结果。**速率另用独立操作性事务持久计数，所以GET不是零数据库写入。** owner/namespace的读/写各有rolling60限制，持久high_water拒时钟倒退；重启不清有效计数，旧速率行可过期，根历史不能删除。policy不可原地修改，扩容须未来受审迁移，不能换realm腾空间。

## 前端、清理及边界

实际BillsFeedbackWorkspace接v2；nonce在HTTP前用IDB strict事务完成并独立读回，逐行CAS及永久RESOLVED。EXECUTION_SENT/CLOSE_SENT先存后发。多个未知逐笔处理；刷新不恢复执行权，明确放弃后需新准备和再确认。异步动作核页epoch/generation及原cardContext实际session/binding_version，另核安装context。

v1 localStorage原件保留，复制LEGACY并联合保护晚到旧writer；无nonce不伪造、无O不猜。legacy已保存原GET可核清；未保存legacy专用关闭需另案真实切换/唯一关联/连续性证据，本候选不伪造或实现该业务决议。此owner清理是账单闭合服务特例，不扩大合同11通用revoke权限。

测试入口：API feedback_recovery_tests.rs、传输代理feedback_recovery_pg_fault.rs、前端scripts/recovery-browser.mjs。命令、最终结果和逐项T01—T30在新证据根 `E:/银行agent/artifacts/billing-prepare-recovery-candidate-2026-10-24`。真实部署来源/权限/TTL/配额采用、writer排空/备份连续性及Jonathan兼容仍未验。末次外部授权检查至物理COMMIT仍有窗口；IDB不能防清数据/驱逐；原始审计出口治理不由白名单DTO替代。未启卡/资金/worker/真实LLM。
