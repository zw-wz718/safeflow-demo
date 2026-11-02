# 前置裁决与明确确认的公共来源组合核验 v1

日期：2026-10-07。`PREFLIGHT_CONFIRMATION_CHECK_VERSION = 1`。本次用户授权的底座来源组合核验，复用[合同11](./11-server-confirmation-storage-v1.md)、[合同14](./14-trusted-preflight-decision-storage-v1.md)与[合同17](./17-conditional-pre-commit-v2.md)；冻结旧合同、既有数据库列及正式许可封闭性不变。微秒回退修订仅追加迁移8，旧迁移1..7保持不变。

## API与信任来源

```rust
DecisionService::verify_with_confirmation(
    session, task, stored_decision_id, confirmation_ref, expected_binding
) -> Result<VerifiedPreflightConfirmation, StorageError>
```

通过既有 `DecisionService::for_trusted_backend` 装配，同一PgRepository与Runtime提供数据库、版本化会话、注册信息和部署/输入配置。时间来自该服务可信时钟，不由请求输入。没有第二个确认服务注入或不同会话真源；确认行从同一数据库读取。方法不接受外部SecurityDecision、ConfirmationRecord、confirmed/verified布尔值或成功提供器。

expected_binding只是比较数据，引用只定位数据；不能证明已核验。核验实际读取security_decisions与server_confirmations，前者必须是当前有效ACTIVE/TRUSTED_RUNTIME_V1/PREFLIGHT_ONLY/PRE_TOOL或PRE_AUTH，后者必须是当前有效CONFIRMED。PREPARED、REVOKED、过期、错误绑定或损坏记录拒绝。裁决里的BLOCKED仍为BLOCKED，不因确认而获批准；组合方法检查来源一致性，不重新裁决或证明业务允许。

## 同一事务与最终核对

1. 解析版本化会话；复用持久时间围栏采样。开启既有数据库事务并设定仓库schema，先按用户锁定task，等待后重核验会话。
2. 同事务锁定裁决行，复用原完整解码/摘要/当前状态、时间、装配、输入合同和当前任务/全部操作快照核验。
3. 同事务锁定确认行。分别使用原有域分隔会话摘要，检查两行均属同一用户、原会话与binding_version；不会错误比较两种不同摘要算法的字符串。
4. 两行绑定与锁内当前绑定及expected逐字段相等：用户、任务、操作、节点、工具、task_revision、plan_version、operation_revision、facts_version、arguments_hash。保留DRAFT/READY及RUNNING/WAITING_USER约束，处理中、终态、业务提交引用和不一致恢复点拒绝。原参数重新计算摘要，浮点拒绝，不猜业务投影。
5. 提交前重读task，锁定全部operation，再重读原裁决和确认并逐字段比较。再次核验完整快照、期限和当前装配，最后一次异步时间观察后重核验会话与确认期限，再提交只读核验事务；仅提交成功才返回结果。

迁移8在原 `security_decision_time_fence` 追加非空、非负的 `last_observed_us`，保留 `last_observed_ms`。每次时间观察在同一持锁的独立事务比较微秒围栏并原子更新两列，包括随后返回过期、会话失败或其他拒绝的观察；时间围栏只记录时间，不记录确认通过或授权成功。确认期限按原合同微秒精度比较，等号拒绝；同事务、跨请求或重建服务后的同毫秒微秒回退都拒绝。组合有效截止为裁决与确认期限较早者，不续期、不修改来源状态，不新增成功审计或许可。

旧毫秒记录无法还原当时的微秒，迁移保守回填到该毫秒末尾（`ms*1000+999`，超BIGINT范围饱和到最大值并拒绝后续不一致时钟）。因此升级后仍落在旧毫秒较早位置的时间会被拒绝，需到下一毫秒再核验，不能把旧观察解释为该毫秒的开头。如果旧写入只推进毫秒列，读取也保守拒绝该未知微秒区间。原毫秒字段保持并随新代码更新，用于既有查询兼容；不保证仍运行旧二进制代码的服务具备新防护，部署必须升级全部可信核验实例。该字段不是生产集群时钟真源，不支持时钟回退后自动清空围栏。

事务失败、读失败、提交失败或取消均无有效回执。该事务没有业务写入、许可消费或业务回调，不对外借出SQL连接。已核验行及操作行在事务期间持锁；遵守task-first协议的修改串行处理。时间围栏只保留时间观察，拒绝不能产生授权成功事件。

## 封闭回执与未启用能力

`VerifiedPreflightConfirmation`字段私有，无公开构造器、Serialize/Deserialize或Debug；访问器提供原preflight_decision、confirmation、checked_at、expires_at及独立版本。它是本次查询核对结果，不是凭证，提交后不再持锁；下次使用必须重新读数据库，不能缓存后签发或执行业务。不能作为VerifiedPermitSnapshot传给PermitStore。

来源真伪仍依赖可信数据库/Runtime/明确确认采集处理器，不能防恶意DBA或可信代码直接伪造SQL。跨系统会话撤销未与PostgreSQL共事务，最后检查与COMMIT之间的竞争边界保留，不能声称跨系统绝对原子撤销；未来实际执行必须在其授权/业务事务重核验。

本方法不重新执行Policy、BankGateway或Tool，不生成确认、不改变任务/操作状态，不产生READY或Permit。来源通过不代表PRE_COMMIT通过，未知业务投影、正式认证真源及批准规则仍未补齐；`request_precommit_evidence`和正式签发入口保持拒绝，Runtime资金拦截不变。强验证是否必要仍由已确认Profile/Policy与可信事实决定，不能将本回执解释为NOT_REQUIRED，更不能使用合同13模拟凭证代替正式来源。

生产确认UI/HTTP、实际业务投影/完整指纹、正式认证与批准来源及其组合签发装配仍待实际对接；不替业务负责人定义字段、账户或账务。本次核验也不是同一事务的资金批准，不提供假资金运行例子。

## 测试入口

永久PG用例位于 `crates/storage/tests/decision_cases/source_combination.rs`，由decisions集成测试注册，默认ignored。生成方只编译；由检查方在真实PG运行。原回归不删。两个compile_fail doc证明回执不能由JSON伪造或作为Permit快照使用。范围、命令及本轮实测见[短交接](../handoff/SafeFlow_裁决与确认公共来源组合核验_v0.1.md)。
