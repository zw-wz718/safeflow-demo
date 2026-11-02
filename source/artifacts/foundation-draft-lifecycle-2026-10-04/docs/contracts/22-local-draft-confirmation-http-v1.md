# 本机模拟草案公共确认 HTTP 合同 v1

2026-10-08：BASE-03/BASE-05 公共确认准备子项，复用合同 07 的既有创建参数与 Tool/Policy、合同 11/14/18 的真实持久来源。公开 `contract_version:1`、`mode:PROJECT_SIMULATION`、`scope:MOCK_DRAFT_CONFIRMATION_ONLY`。这是公共操作草案与明确确认采集；不保存业务草案、不执行 Tool、不启用资金、正式验证、Permit 或 PRE_COMMIT。

## 固定来源与兼容

创建实际调用已有 MockLlm，固定提案为单个 `create_mock_transfer_draft` Tool 节点；经 PlanningIntakeService 的结构、注册、Schema 和版本化会话检查后保存。金额 `200.00 CNY` 是明确模拟输入，不是风险阈值。候选来自既有 MockDraftStore 对共享 MockBankGateway 的真实 `小李` 查询；服务端核对明确选择的 `payee_008` 存在，不选第一个候选，不接受客户端候选或任意参数。

原只读 Runtime 注册集合、输入合同和完整来源哈希保持兼容。草案准备使用独立 RuntimeAssembly，复用相同服务端会话、仓储和 MockBank；注册已有 CreateMockDraftTool/Policy，采用已确认 Green profile。普通 RuntimeBootstrap 没有安装 MockDraftAuthority 执行能力；构造 Tool 所需 authority 不调用确认、签发或写入方法。

## 路由与严格输入

沿合同 21 的本机 Host、Origin、Cookie、正文上限、命令头、禁止缓存和会话复核规则。

| 路由 | 输入 | 行为 |
|---|---|---|
| POST `/api/sim/draft-confirmations` | `{}` | 创建新的固定计划并准备公共确认 |
| GET `/api/sim/draft-confirmations/:task_id` | 无 | 只读原任务与当前会话的既有展示记录 |
| POST `/api/sim/draft-confirmations/:task_id/prepare` | `{}` | 用户明确准备原任务；可恢复尚未完成的准备阶段 |
| POST `/api/operations/:operation_id/confirm` | 下列 DTO | 只处理已展示票据的明确决定 |

```json
{"scope":"MOCK_DRAFT_CONFIRMATION_ONLY","task_id":"task_...","confirmation_ref":"confirmation_...","display_ticket":"display_0123456789abcdef0123456789abcdef","action":"CONFIRM"}
```

action 只接受 `CONFIRM` / `REJECT`。请求拒绝额外或重复字段。未知或缺失 scope 返回 501；已知 scope 缺字段返回 400。确认入口不创建任务、不准备确认、不刷新票据或 TTL，不接收 owner、参数、业务结果或授权对象。

## 公开 DTO

严格投影 16 个字段，不直接序列化内部任务、安全裁决或绑定：

```json
{
  "contract_version":1,
  "mode":"PROJECT_SIMULATION",
  "scope":"MOCK_DRAFT_CONFIRMATION_ONLY",
  "task_id":"task_...",
  "task_state":"RUNNING",
  "operation_id":"operation_...",
  "operation_state":"DRAFT",
  "task_revision":2,
  "operation_revision":1,
  "plan_version":1,
  "facts_version":7,
  "parameters":{"payer_account_id":"account_001","payee_id":"payee_008","amount":"200.00","currency":"CNY","remark":"","replaces":null},
  "confirmation":{"confirmation_ref":"confirmation_...","status":"PREPARED","prepared_at":"2026-10-08T00:00:00Z","expires_at":"2026-10-08T00:05:00Z","confirmed_at":null,"revoked_at":null},
  "preflight":{"evidence_kind":"PREFLIGHT_ONLY","pre_tool_outcome":"READY_TO_CONFIRM","pre_auth_outcome":"READY_TO_CONFIRM","required_gates":["EXPLICIT_CONFIRMATION"]},
  "display_ticket":"display_0123456789abcdef0123456789abcdef",
  "next_action":{"kind":"CONFIRM"}
}
```

时间示例不是默认时长；实际值来自已保存确认记录，使用 RFC3339。所有版本遵守合同 21 的 JavaScript 安全数字边界。该单 Tool 计划没有 UserDecision 检查点，因此 TaskState 始终 RUNNING，OperationState 始终 DRAFT；独立 next_action 表示公共确认等待，不伪装 WAITING_USER 或 READY。

| next_action | 展示字段 |
|---|---|
| PREPARE | confirmation / preflight / display_ticket 全 null；创建操作前中断时 operation_id / operation_state / operation_revision 一起 null |
| CONFIRM | confirmation 为 PREPARED；两阶段 preflight 与 display_ticket 完整 |
| NONE | confirmation 为 CONFIRMED 或 REVOKED；真实时间与完整 preflight 保留；display_ticket 为 null |

参数来自服务器保存的原计划。确认后返回同一冻结参数，不展示最近修改值冒充已确认参数。未知版本、字段组合或查询错误时客户端清除本地确认动作。

## 持久来源与事务

迁移 0011 新增 planning_confirmation_requests / planning_confirmation_displays，不修改迁移 1..10。首次 checked review、TaskCreated 审计、原任务快照摘要、完整注册摘要和保留的服务端 operation_id 在同一创建事务保存。普通 PersistenceService 手写的等价 TaskRecord 没有此锚点，不能接入确认准备。

准备只创建保留号对应的公共 DRAFT 操作。每会话及正绑定版本有独立展示 journal，服务器随机票据和两个阶段 request_id 持久保存；使用已有 DecisionService 实际运行 PRE_TOOL 与 PRE_AUTH，只有真实 READY_TO_CONFIRM、Green、唯一 EXPLICIT_CONFIRMATION gate 才能继续；不合成或降级裁决。随后使用既有 ConfirmationService.prepare，最终保存两份裁决引用、确认引用与展示摘要。

各阶段沿用既有独立事务，不声称整个流程原子。中途失败保留原 task_id 和已创建 operation_id。后续 GET 不执行准备；明确 POST prepare 使用原 journal 的 request_id 恢复，已有裁决/确认复用原时间与有效期。相同会话过期或撤销后不续期、不补新确认，返回 409。

GET 只读来源，不查候选、不运行 Kernel/Policy/Tool、不新增确认/票据、不刷新 TTL；时钟围栏只读取。最终 task-first 事务锁住任务、两份裁决和确认，复核绑定、当前快照、状态、完整装配、展示摘要与有效期，提交前复读实际来源并共同复核时间。锁等待中发生过期或撤销不能返回确认动作，也不能输出过期来源对应的确认成功。

CONFIRM 绑定当前会话、原任务、operation_id、confirmation_ref、随机 display_ticket 及冻结参数。先核验两份预检，记录明确确认，再分别调用合同 18 的实际裁决/确认组合核验。仍无执行权。REJECT 只将尚未确认记录撤销，保持 DRAFT；确认后 REJECT 返回冲突，拒绝后 CONFIRM 返回冲突。相同决定可重放原记录，无额外审计或续期；并发相反决定由确认记录事务锁保证只有一种成立。

重新登录或服务重启后，同 owner 可读原计划与原操作，但无当前会话展示来源时只返回 PREPARE。只有明确 POST prepare 才可产生新会话的预检、确认和票据。旧会话票据不能串用。版本、参数、事实、注册或部署来源改变时拒绝原展示，不自动准备新版本；本轮没有重规划入口。

## 错误与边界

复用合同 21 错误 envelope 和 reason_code，无新增原因枚举。过期、撤销、裁决失效或来源/绑定变化为 409 STALE_PLAN；互斥决定为 409 IDEMPOTENCY_CONFLICT；未检查规划来源为 403 FORBIDDEN；未知或其他 owner 为 404；已知原号错误保留 task_id 和 QUERY_ORIGINAL_TASK。断网或首响应丢失且未获原号时停止新增并人工核对，不能自动创建第二任务。

正式验证与审计路由继续 501。本轮没有接入合同 13：其隔离 namespace 与当前 HTTP schema 不同，不能通过放宽限制或泄露秘密接通。完整认证/Runtime 写、业务投影、业务幂等/回执、正式审计/Outbox 及真实模型仍分别待公共机制继续实现或对应负责人交付；本合同不代表 BASE-03/05 父项及业务完成。验证与启动见[交接](../handoff/SafeFlow_本机模拟草案公共确认_v0.1.md)。
