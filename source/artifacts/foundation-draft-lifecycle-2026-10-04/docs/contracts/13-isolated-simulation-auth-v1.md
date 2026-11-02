# 底座隔离模拟验证与认证凭证 v1

本内部合同只冻结底座自有机制，`SIMULATION_AUTH_CONTRACT_VERSION=1`，模式固定 `FOUNDATION_ISOLATED_V1`。依据本队运行流程 v0.2 Auth Service、第 5 步和 SEC-06/07/08/17、底座 PRD BASE-R03/R04，以及合同 11/12；不是跨团队正式转账认证合同。

本队待决 D-01（验证方式/期限）和 D-18 的业务语义未被本合同决定。`确认验证字段-2026-09-24.md` 首行标为外组参考，不能把其中短信、人脸、重发、期限等细节当本队授权。

## 模拟隔离与可信来源

`SimulationAuthService::for_isolated_backend[_with_clock]` 仅供可信后端装配，不注册为 LLM/客户端 Tool。必须选择 `SimulationAuthMode::FoundationIsolatedV1`，数据库 namespace 必须是 `sf_auth_sim_` 加 32 位小写十六进制；此命名约束用于隔离测试，不能代替生产权限配置。服务端显式提供 `SimulationAuthConfig::new(challenge_ttl, grant_ttl, max_failures, secret_bytes)`，没有默认值。期限为正整数毫秒，失败上限技术范围 1..100，随机材料长度技术范围 16..64 字节；这些是资源/随机秘密安全边界，不是业务验证码规则、产品 TTL 或失败策略。

身份与原会话版本来自 `SessionResolver::resolve_binding`，旧的无版本 resolver 拒绝。客户端/LLM不能传用户或可信状态/时间。数据库真源使用现有 `ConfirmationBinding` 的用户、task、operation、node、tool、task revision、plan_version、operation revision、facts_version 和 arguments_hash，并绑定原会话摘要/版本及具体已明确确认的 `ConfirmationRef`。期望绑定只用于比较，不能发证。arguments_hash 只是底座参数摘要，不是完整业务 OperationFingerprint。

## 公开入口与输出

实现：`crates/storage/src/simulation_auth.rs`；迁移：`0005_simulation_auth.sql`。

| 入口 | 输入 | 行为 |
|---|---|---|
| `create_challenge` | 原 SessionRef、TaskId、OperationId、已存 ConfirmationRef | 锁任务，重新读快照/确认，首次创建挑战和一次性后端秘密 |
| `read_challenge` | 原会话、task、SimulationChallengeId | 读取当前仍绑定/未到期的挑战元数据；FAILED/REVOKED可观察，不证明已核验 |
| `submit` | 原会话、task、challenge、SimulationSubmissionId、SimulationProof | 实际比较提交材料，失败持久计数；正确且仍有效才产生一个独立模拟凭证 |
| `read_grant` / `verify_grant` | 原会话、task、SimulationGrantId；verify另带期望绑定 | 重新核验快照/确认/会话/时间，只有当前有效的模拟凭证才返回；verify的绑定不是权威来源 |
| `revoke_challenge` / `revoke_grant` | 原会话、task、对应ID | 级联撤销挑战及凭证，重复不改时间、不重复审计；允许原会话清理过时/过期记录 |
| `request_formal_auth` | 原会话、task | 一律 `FormalAuthUnavailable`；不接受生产方式、passed布尔值或任意认证引用 |

`SimulationChallengeId`、`SimulationSubmissionId`、`SimulationGrantId` 为独立 typed ID，`new_server_id()` 使用 OS 随机 UUID v4；parse 只承认规定前缀的规范 UUID 字符串，不能用 ID 存在当授权证据。

`SimulationChallengeDelivery::take_secret()` 首次创建成功后仅取出一次 `SimulationSecret`，创建重放没有秘密。此类型及 `SimulationProof` 无 Serialize/Debug，使用 zeroize 擦除内存；`expose_for_isolated_backend()` 是明确受控的模拟后端材料出口，不能经过 UI、普通响应、日志、审计或 LLM。操作系统 getrandom 生成秘密，domain-separated SHA-256 保存/比较摘要，subtle 恒定时间摘要比较，实际材料长度必须匹配。普通元数据 DTO 不含原始会话、秘密或其摘要；数据库只存摘要。模拟演示只是服务端随机秘密回送核验，不证明真实人的身份，也不冒充短信/人脸。

`SimulationAuthGrantRecord` 是可序列化的模拟结果/引用，不能转换为正式 AuthGrantRef、ExecutionPermit 或 VerifiedPermitSnapshot。没有公开随意构造已核验生产快照的入口，生产 Permit 签发继续不可用。没有消费模拟凭证来执行资金或业务写的接口。

## 生命周期与幂等

挑战状态 `PENDING -> SUCCEEDED / FAILED / REVOKED`，SUCCEEDED/FAILED可撤销，REVOKED不复活。失败达必填上限即 FAILED；迟到正确材料不能发证。凭证只有 ACTIVE/REVOKED，期限独立核验。

每个 operation 终身最多一个挑战（数据库 UNIQUE），每个挑战最多一个凭证（数据库 UNIQUE）。同操作、同会话/版本、同确认、同绑定且同配置的创建重放返回原挑战，不重发秘密、不重置失败计数、不续期。不同确认/配置/快照创建返回冲突；新会话不能沿用旧挑战。参数改变后必须先取得新快照/确认，旧挑战及凭证失效，但本版不会替换同 operation 的旧挑战。业务重新认证/重发如何关联 operation 待对接，不为了测试绕过失败预算造业务新单。

同 submission ID 同 challenge 同材料重传返回原提交结果，不重复计失败或签发。历史失败结果返回原失败计数，不能当当前计数；同 ID 不同材料/挑战拒绝。并发同提交序列化为同一结果；并发不同成功提交仅首个可成功，其余新提交拒绝。成功重放必须原凭证仍有效，没有新期限。已撤销/过期时连旧成功也拒绝，原失败在 FAILED且未到期状态可重放原结果，不复活挑战。

正常确认/核验不变更 task/operation 状态、revision或facts_version，不设置READY。只允许当前RUNNING/WAITING_USER且相关操作DRAFT/READY；提交/处理中、终态、版本/参数/事实变化、确认未明确确认/撤销/到期、错task/operation/node/tool、跨用户/原会话/换版/撤销均拒绝。撤销是清理，不需要过时的确认仍有效，也不会授权重新创建。

## 时间与事务

挑战期限 `min(created + configured challenge TTL, original confirmation expiry)`；凭证期限 `min(issued + configured grant TTL, challenge expiry, original confirmation expiry)`。整数毫秒，到期等号即拒绝，检查加法溢出和创建/完成/签发/撤销下界。

`simulation_auth_time_fence` 是隔离 schema 级的持久时间高水位。每次请求及事务内关键观察使用独立连接池的短事务，锁唯一时间行之后才采样可信时钟；高水位只前进。已读取时间、失败提交、到期拒绝和授权失败后的可信时间观察在重建服务/连接/进程后仍存在，挑战与 grant 都不能借回退复活。不同任务共享围栏是本模拟版的保守选择：一个可信时钟未来读数会导致整个隔离 schema 拒绝较早时间，不能自动清零解封。

时间围栏不持有任务/操作/挑战锁，也不访问授权表；独立池避免授权事务占满连接后互相等待。所有认证服务端时钟观察先持久化，复用旧确认核验器时传入已观察的冻结时间，避免旧帮助函数产生未持久化的独立观察；核验等待后再次观察和检查期限。围栏提交失败立即拒绝；只提交高水位，不把挑战、attempt、grant、审计的半成品提交。它不是业务审计事件，也不是生产分布式时钟或跨系统会话事务方案。

授权使用现有task-first锁序，自有挑战/提交/凭证及审计在同一个SQLx事务。等待锁、SQL/审计后和提交前重新读当前task/operation/确认、挑战、grant、submission，比对实际快照；最终异步时间观察结束后再检查原会话，且用同一最终时间复查所有相关期限。材料不匹配是正常 NOT_VERIFIED结果，有SIM_AUTH_FAILED审计；非法绑定等拒绝回滚，不另持久化通用拒绝审计。不能把本步审计说成完整SEC-17或链上审计完成。

立即写/审计失败、延迟约束在COMMIT失败、COMMIT前取消，授权状态和自身审计一起回滚。网络中断或COMMIT已发送后的进程取消不承诺确定的提交结果；恢复后必须按原ID查记录，不能用新提交/新挑战号静默重做。最终核验到实际DB提交之间的跨系统会话变化、DB管理员篡改和生产分布式时钟仍需要正式集成设计，不声称本隔离机制已解决。

## 待对接与恢复条件

| 负责人 | 缺少材料 | 影响 | 恢复条件 |
|---|---|---|---|
| 逐风/对应业务负责人 | 本笔操作的版本化完整业务投影、参数变化/facts规则、草案与Operation映射 | 正式业务指纹、转账认证绑定 | 收到可核对接口/代码与合同后接入；不代写 |
| 认证策略负责人待确认，逐风协调D-01 | 验证方式/证据真源、期限、失败/重发策略、身份与会话服务 | 正式认证服务/凭证 | 本队确认合同和可信生产证据来源后启用；当前明确不可用 |
| 底座公共安全与业务负责人共同确认，D-18待确认 | 对各业务所需认证/确认的裁决来源、完整指纹及版本失效合同 | 封闭核验快照、正式Permit签发 | 对接上述真源后独立设计与验收；不开放构造后门 |
| 逐风及审计负责人 | 业务事务、幂等、Outbox/原单查单、正式审计接口 | 资金执行及完整审计闭环 | 收到已确认业务实现并单独授权本轮接入；当前未启用 |

未接入能力返回不可用，不以模拟成功代替以上依赖。隔离测试只证明底座公共机制，不代表业务或第5步已经完成。
