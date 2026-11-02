# 付款业务观察 v1

依据：2026-10-19 付款业务预检任务；沿用合同06、17、28、29、31、32。
成员固定来源 `cacc62c09ac07555844c3422b62709a577ed5375`，Policy 来自此前 b45473f。成员实现和原 JSON 未修改。

## 产品入口与边界

现有付款准备响应增加 `business_preflight`，无新增可写端点或客户端事实输入。
现有 `/confirm` 首次明确确认保存业务观察；`/evaluate` 显式重新评估过时观察；付款单 GET 和任务付款列表 GET 返回保存的历史及当前过时投影。

`scope=PAYMENT_BUSINESS_OBSERVATION_ONLY`。观察直接调用成员 `FundsBusinessPolicy::evaluate`，不是 Kernel 最终付款裁决；合同31的 `TRUSTED_PAYMENT_MINIMUM_V1` 历史和 `domain_policy_evaluated=false` 含义不变。
原有缺付款账户等确认拒绝继续原子回滚，不以业务观察绕过原确认约束。

未设置 READY_TO_EXECUTE，未签发或消费 Permit，未调用 submit/resolve，`funds_execution_enabled=false`。
未注册资金 Tool/Profile，未占用 `public_execution_requests`；修改、取消及新版付款准备仍沿原入口执行。

## 可信来源及比较键

- 认证身份、任务和版本来自后端会话与数据库，完整付款参数来自保存的准备快照，时间来自 PostgreSQL。
- 准备、确认、评估在 REPEATABLE READ 事务内按 owner、task、原业务操作/草案顺序保护。确认和评估的全部来源复用同一个借入连接；不另开 Pool，不锁全体历史账户。
- GET 使用已有 REPEATABLE READ READ ONLY 事务，不申请 advisory/行锁，不调用 Policy 重新生成发现，不写失效历史。
- `foundation_payment_business_preflight` 是私有预检比较名，没有 Runtime Tool/Profile 注册。无原单时，`payment_preflight_<preparation_operation_id>` 仅在内存中比较，明确 `INTERNAL_COMPARISON_ONLY`，没有分配或保存正式执行键。
- 已有原单时，成员仓储实际保存的原键仅用于再次只读比较，范围为 `OBSERVED_ORIGINAL_KEY_QUERY_ONLY`；不将内部比较键误报为真实执行键冲突，不采用或恢复原执行权。实际参数不同时保留 Policy 的原单冲突发现。
- 草案、本人账户余额、路由和原单由 `FundsStore::policy_facts` 读取。日累计经过公共首次提交/完整原单/账务绑定核验，使用本人全账户范围；来源不完整时保存缺失原因，不填完整空统计。
- 卡事务仓储尚未收到，始终使用成员 `MockOutgoingState::Unknown`。保留实际 findings；Policy 在此处提前返回时，不声称余额充足或后续日累计业务检查通过。

这些保护提供一致性观察，不宣称可变余额、路由或卡锁已保护至实际扣款 COMMIT。最终执行仍须在受控资金事务内重新读取、判断并持有共同资源保护。

## 保存、重放与查询

追加迁移0022：`payment_business_observations`。既有1..21不变。
每条观察保存顺序号、准备绑定、会话绑定摘要/版本、完整预检上下文、来源事实及缺失原因、源摘要、装配摘要、Policy原 findings、数据库观察时间及有效期。
有效期复用已配置决策 TTL，且不超过付款确认期限；没有新增产品默认时长。

观察只追加，数据库拒绝 UPDATE/DELETE。来源身份摘要不含持续变化的读取时钟，但包含实际事实、统计日和草案到期状态；装配摘要覆盖底座适配、成员 Policy/仓储、公共日累计代码与 TTL。
观察和 `CHECKPOINT_SAVED.details.payment_business_preflight` 审计及现有 Outbox 同事务保存；读取核对记录摘要、准备绑定、源摘要及关联审计，缺失或不一致返回错误。

确认重放保留原观察，不根据后来事实追加或恢复判断。显式评估在当前观察不变时同样幂等，过时后才追加；GET 仅计算 freshness，不改写记录。
过期或已拒绝确认不读取新的预检来源、不新增观察或观察审计，保留原观察及确认失效处理；过期写请求按原流程保存撤销及失效原因后返回拒绝。未过期的版本冲突仍可保存明确失效的诊断观察，不显示CURRENT，也不授予执行权。
REPEATABLE READ 在锁等待后可能遭遇40001，准备及确认/评估最多尝试4次完整事务。竞争准备票据或观察顺序号的唯一冲突也只在对应写入点触发完整重试；不重复留下确认、最低等级或观察历史。

`freshness`：

| 值 | 含义 |
| --- | --- |
| NOT_EVALUATED | 尚未保存业务观察 |
| CURRENT_OBSERVATION_ONLY | 当前来源/会话/装配/期限仍匹配，仅表示观察新鲜，不表示可执行 |
| HISTORICAL_REEVALUATION_REQUIRED | 保留旧发现，来源、会话、装配或期限已不匹配，须显式评估 |
| PREPARATION_INVALIDATED | 原准备、确认或会话已失效，保留历史供查询；不得恢复授权 |

## 后续装配依赖

结夏提供同连接实际卡锁仓储、完整卡映射及业务样例；Echo按既有共同锁顺序装配，逐风核对 submit/resolve 的锁内新鲜读取接法。
Echo继续采用资金 Tool/Profile、执行指纹、唯一安全裁决和封闭许可签发/消费；只在其依赖已确认后启用。验证码细则仍待确认，真实 LLM 后置。
本观察合同不批准上述缺失业务，也不把生成侧隔离测试当成成员业务联调或独立验收。
