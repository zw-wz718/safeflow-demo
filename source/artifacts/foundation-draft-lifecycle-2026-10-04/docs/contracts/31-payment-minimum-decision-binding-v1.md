# 付款最低安全等级判断与持久绑定 v1

2026-10-18恢复入口增量：`business_recovery_required` 不再仅提示查询。实际付款响应的 `original_result_path` 可直接读取原单，路由及完整公共绑定核验见[合同32](./32-payment-execution-recovery-v1.md)。该GET只读历史，不重新评估最低等级或授予付款权限。

本合同是底座内部实现约定。依据2026-10-18用户确认的“已有累计加尚未提交本笔”、安全规范6.2及逐风固定 `f0997ec1e0ebf1bfa88f7a44ffff25cac3cd0cc6` 的日累计合同；不代表成员已经共同确认资金 Tool/Profile。

## 实际调用

现有 `POST /api/sim/payment-preparations/:task_id/:operation_id/confirm` 首次明确 CONFIRM 后，在同一事务内读取当前完整付款投影和 owner 全账户日累计，调用实际 `DeterministicSecurityKernel::evaluate_payment_minimum`，由现有 `DecisionService` 保存至 `security_decisions`，并写原有决策审计/Outbox。确认、判断、审计任一失败全部回滚。

公共规则版本 `payment-used-plus-current-v1`：整数分 `used_cents + current_cents <= 100000` 为 Yellow，超过为 Red；溢出、缺事实和非当前本人账户拒绝评估。沿用成员已确认的成功/处理中/跨日处理中算法，既有业务原单和不可改首次公共提交时间同连接绑定。没有修改成员算法或JSON。

请求仍只有原确认号、显示票据、scope与明确动作；不能上传累计、金额、权重、风险结论或认证结果。内部资金参数投影为 `draft_id/draft_version`，后者是业务草案版本；公共计划版本另记。完整收款人、账户、金额、币种、备注、草案期限和稳定业务原号仍绑定在原付款快照，未将内部 `foundation_payment_intent` 注册或改名为扣款 Tool。

## 结果范围

响应增加 `security_decisions` 和 `decision_freshness`，记录包括实际 Kernel 输出、所需门禁、公共确认绑定、完整付款摘要、日累计/本笔/合计、统计日、成员规则版本、本人账户/全量原单事实摘要及实现摘要。已有决策表迁移20增加独立来源 `TRUSTED_PAYMENT_MINIMUM_V1`，数据库限制该来源为内部付款意愿、PRE_AUTH及 READY_TO_CONFIRM/WAITING_AUTH，每笔最多一个 ACTIVE。

这是**公共最低等级判断**，不是完整业务 Policy 评估。Yellow 的 READY_TO_CONFIRM、Red 的 WAITING_AUTH只表示此最低等级规则的门禁；已有付款明确确认也不能把它转为 READY_TO_EXECUTE。`domain_policy_evaluated=false`、`risk_mode=NOT_EVALUATED`、`funds_execution_enabled=false`及固定信息 Finding一起标明覆盖范围。冻结 SecurityDecision 的数字0/Low不能解释为付款风险已被核验；没有启用或伪造任何风险来源。原有软评分继续计算/审计模式，本入口未取得可绑定的付款软风险事实。

复用 Kernel 的等级门禁/结果规则，没有注册永远允许的转账业务 Policy，也没有添加第二套认证/卡锁缺源守卫。资金 DomainPolicy/Profile、必要交易认证来源、卡锁到COMMIT协议仍未接通；现有 PRE_COMMIT、正式签发和普通 Runtime 写入口保持关闭。预评估不占用余额、不生成资金单，不授予实际付款权限。最终执行仍须在原 owner/原号/账户保护与公共事务内重新核验当前全部来源。

## 查询、重放与新评估

- 既有 GET 单笔或任务数组返回原判断；仅核验新鲜性，不静默重新计算或重定价。新鲜性为 `CURRENT_MINIMUM_ONLY`、`NOT_EVALUATED`、`HISTORICAL_REEVALUATION_REQUIRED` 或 `FACTS_UNAVAILABLE`。GET使用一致性只读事务，响应中的INVALIDATED及失效观察时间是当前投影，不是已经保存的撤销事件；显式命令保存实际失效时间和审计，因此两次观察的ended_at_ms可以不同。
- 相同明确确认重放保留原操作、确认时间和判断ID；不会重复累计本笔。旧判断过时后，重复确认也只返回原历史。
- 新增 `POST /api/sim/payment-preparations/:task_id/:operation_id/evaluate`，请求体与 CONFIRM 相同，但要求已经独立确认。它显式重读事实；相同来源复用当前判断，变化后保留旧记录并另存新判断。未确认调用返回409，不能用评估代替确认。
- 比较依据含统计日及全量原单/账户范围事实摘要，排除瞬时观察时钟。同日无变化查询不制造新版本，跨日即使合计相同也失效。评估写入后重读来源和期限，提交前继续校验会话与当前实际草案。
- 判断有效期取现有决策TTL与原确认到期时间的较早者；不新增产品默认TTL。TTL和部署版本摘要也参与装配比对。判断过期不撤销仍有效的独立确认，但查询/重复确认仅返回过期历史，必须显式重评；当前来源不可核验时原号重放返回 `FACTS_UNAVAILABLE`，不能刷新成可用判断。
- 草案修改/取消/过期、公共参数/事实变化、会话撤销或重新登录均不能复活旧确认。历史记录仍可由当前同owner会话查询，原判断标记 INVALIDATED；新会话不能借查询取得执行权。
- `business_recovery_required=true` 与 `decision_freshness=QUERY_ORIGINAL_BUSINESS_OPERATION` 表示已有提交标记/成员原单/公共原号记录，禁止新评估或新付款；沿既有 `TransferSqlReader::business` / `ExecutionRequestStore::original_transfer` 查询响应快照中的稳定业务号。没有原单不能推断未扣款，也不能换号重付。

`snapshot_digest`包括确认绑定、付款/日累计事实及确认号；`assembly_digest`覆盖 Kernel/存储代码和规则元数据，`decision_digest`覆盖实际结果。`plan_review_digest`在此来源只散列带 PAYMENT_MINIMUM_ONLY 标签的准备绑定，不声称资金执行计划已经通过 DomainPolicy。通用决策/确认验证器不能将本来源提升为普通 Runtime 或 PRE_COMMIT 证据。原 SECURITY_DECISION_RECORDED 持久审计 JSON结构保持一致。

## 验证边界

新增 HTTP 用真实生命周期草案验证边界、重放、账户归属、查询/显式重评和事务故障；原付款回归继续验证实际修改/取消、过期、会话撤销、锁等待及跨进程恢复。日累计隔离测试复用既有 sealed Permit 测试夹具，验证真实SQL900元加另一账户未提交200元及原请求重放，不表示正式资金许可、卡锁或业务 Policy 已接通。跨日测试使用成员计算器的固定 UTC+8 日界线，验证合计不变时事实身份仍变化。

本地候选和本轮实际测试日志见工作区 `artifacts/payment-decision-binding-2026-10-18/DELIVERY.md`；独立验收另行进行。
