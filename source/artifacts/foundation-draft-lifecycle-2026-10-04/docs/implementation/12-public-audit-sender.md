# 受信后台公共审计 sender（本地未提交候选）

实现位置：`crates/storage/src/audit_sender.rs`。现有 `audit_outbox.rs` 仍唯一负责读取和确认原项；未改其方法、SQL、迁移或公共事件。此适配依据AUD-03 v0.4候选HTTP合同，不冻结C-AUD/T7、D-12或D-23。

调用位置为受信后台代码持有的 `PublicAuditSender::for_trusted_backend(outbox, config)` → `send_pending(limit).await`。单次显式批处理，没有常驻循环或自动启动，没有HTTP/LLM入口。正常API main/启动脚本未装配本候选，所以当前不投递任何共享记录。待接收端两项P2返修并独立复审、receiver采用及部署审批具备后，再接既有后台生命周期；不把库实现称为已部署worker。

配置必须显式提供endpoint、Token、receiver与 `PublicAuditRetryPolicy::new(timeout, attempts, backoff)`，或在受信后台调用 `PublicAuditSenderConfig::from_env(policy)`，读取 `SAFEFLOW_PUBLIC_AUDIT_ENDPOINT`、`SAFEFLOW_PUBLIC_AUDIT_TOKEN`、`SAFEFLOW_PUBLIC_AUDIT_RECEIVER`。任一缺失/非法拒绝构造，不提供猜测默认值。读取配置和构造client都不发请求。Token用Zeroizing持有，Authorization标记sensitive；配置/客户端无Debug或Serialize；失败报告仅固定类别、状态码和原事件号，不保存响应正文、URL、Token或数据库错误详情。部署时通过已有本机私密环境注入，不写聊天、源码、网页或运行日志。

仅允许HTTPS或数值loopback HTTP，路径必须为 `/api/audit/outbox/v1`，禁止userinfo/query/fragment；不使用环境代理、不跟随重定向，禁用reqwest自身重试。每请求超时显式100ms..30s，每项尝试1..5次，指数退避初值100ms..30s并上限30s。仅传输错误/超时/正文中断及502/503/504在本批内有限重试；400/401/409/其他状态及不合格ACK不盲重试，下次显式批处理仍可读取未确认原项。每次重投复用同一已序列化请求体；不重新读取业务或生成新号。请求上限1MiB、ACK上限16KiB为本地传输资源上限，超限不确认，不代表业务大小合同已冻结。

请求只保留pending原五字段：event_id、envelope、digest、historical、enqueued_at，不转换owner或业务时间。200/201才解析ACK；必须为完整JSON且字段严格，核对ack_version、accepted=true、event_id、task_id、operation_id（显式null或原字符串）、digest_echo以及非空/满足原确认方法约束的audit_id，duplicate必须boolean，recorded_at必须有效时间。未知/重复/缺失字段、非法JSON、非JSON类型、过大正文均拒绝。duplicate/recorded_at不是授权依据，digest_echo仅回显，不宣称接收端独立复算。通过后仅调用原 `acknowledge(&event_id, &digest, &configured_receiver, &audit_id)`；该调用成功才报告完成。audit_id→receipt_ref是候选合同指定映射，真实部署receiver标识及采用范围尚待双方确认。

所有网络和ACK失败不触碰Outbox。确认事务拒绝时不自行补写、改原项或重做业务；下次按原号重投，由原确认方法处理幂等/冲突。若数据库COMMIT后连接丢失导致结果未知，以重新读取pending/原确认证据恢复，不能保证通信错误意味着数据库必然未提交。并发sender可能读同一项，沿用接收端event_id去重及原本地确认幂等机制，不新增队列/全局锁。

2026-10-21首次独立审查发现Serde结构体也接受位置数组，首版候选因此不能通过。返修在解析前要求首个JSON token为对象起始符，再直接反序列化原字节；数组、标量和null均为AckProtocol，不经会覆盖重复键的中间Value转换。原重复/未知/缺失字段、类型和值检查保持。真实HTTP/PG新增回归验证数组等价内容仍只请求一次、原pending五字段及receiver/receipt_ref/acknowledged_at不变；之后同原项合法对象及null/string操作号可正常确认。首版失败报告/候选不覆盖，返修另交repair1候选并待复审。

自测在 `audit_sender_tests.rs` 和 `audit_sender_pg_tests.rs`，后者通过现有Db/PersistenceService与真实PG迁移触发器生成待发记录，再调用真实pending与acknowledge；HTTP对端仅本地协议fixture，不修改或运行成员接收端。跨进程用新test进程重读同schema并完成同号确认。证据见工作区 `artifacts/audit-v04-foundation-sender-2026-10-21`，该记录来自本轮隔离生成而非共享待发数据；身份均为既有测试身份，未二次改写原字段或摘要，也不冒充共享pending。

真实启用尚缺：风星龙两项P2新固定提交与独立复审；双方采用receiver身份、目标服务/Token及私有部署范围；实际共享版本与可投递pending的采用/脱敏安排；底座→成员持久接收→ACK→原项确认的双端首投、丢ACK、冲突、不可用和跨进程恢复证据。PG审计仓储、正式ACL、可信库外链头及其他原合同事项保持待对接。本次没有真实模型/银行、共享审计投递、提交、推送或发布。
