# 本机项目模拟只读 HTTP 合同 v1

后续受限扩展：[合同22](./22-local-draft-confirmation-http-v1.md)显式开启 `MOCK_DRAFT_CONFIRMATION_ONLY` 公共确认准备 scope；本合同的只读 Runtime 注册与 TaskResponse 保持不变。本文确认501描述适用于未启用新scope的服务和未知scope，正式验证/审计仍501。

2026-10-08：本合同将已确认的合同 15、16、19、20 接到 `crates/api` 的本机 HTTP 入口，落实 BASE-05/BASE-06 的公共只读流程子项。公开响应 `contract_version = 1`，所有可用入口的 `mode = PROJECT_SIMULATION`。本合同是当前项目模拟前后端采用的受限协议，不冻结全队业务 C-HTTP，不替其他负责人决定回答 Schema、业务字段或正式身份合同。

## 装配与身份

服务仅监听 `127.0.0.1` 的非零端口，默认 `127.0.0.1:8787`。装配复用已有 `MockLlm`、模拟银行余额 Tool/Policy/MockBank，提案固定为三次余额只读查询和两个 `UserDecision`。请求不能选择 Tool、账户参数、owner、profile、安全等级、Permit 或进度；不存在自由文本到余额查询的隐式转换。

`SAFEFLOW_SIM_LOGIN_SECRET` 由可信启动环境提供，长度 16..256 字节，没有内置生产密码；比较秘密摘要时使用常量时间比较。会话随机 token 为 256 bit，Cookie 名为 `safeflow_sim_session`，带 `HttpOnly; SameSite=Strict; Path=/; Max-Age`。服务只保存 token 摘要派生的内部 SessionRef，使用单调递增的正绑定版本；模拟 owner 固定为服务端 `user_001`，不由请求或模型决定。会话只存当前进程，默认 TTL 1800 秒，最多 2048 个；登录携带原 cookie 时撤销旧绑定，注销和过期使旧绑定失效。

Runtime 和持久化服务继续使用版本化绑定 guard；HTTP 在等待前后和外层超时结束后复核本次绑定。失效会话不返回可执行提示。重启丢失模拟会话，不丢失 PostgreSQL 中的任务；同一模拟 owner 重新登录后可以查询并明确续接已核验的已结算等待点。最后会话检查到数据库提交之间仍有既有跨系统窗口，失败响应或超时不能证明提交回滚。

## 请求外围

Host 必须唯一且精确等于绑定地址。Origin 可缺省供本机 CLI 使用；存在时须唯一并精确匹配 API origin 或 `SAFEFLOW_FRONTEND_ORIGIN`，默认后者为 `http://127.0.0.1:5174`。Origin 配置只接受带显式端口、无路径/查询的规范 `http://127.0.0.1:PORT`。服务不授予 CORS；前端以同源 `/api` 代理访问。

POST 和 DELETE 必须有唯一的 `X-SafeFlow-Request: 1` 及唯一的 `Content-Type: application/json`，可带合法 MIME 参数。正文最多 8192 字节，读取期限 5 秒。请求 DTO 严格拒绝未知字段、重复字段、未知 kind、浮点版本及越界版本。已路由的业务错误使用固定 JSON，不回显请求、SQL、连接串或内部错误；非法 UTF-8 task 路径也转换为 JSON 400。响应禁止缓存，正常处理器响应另带 `X-Content-Type-Options: nosniff`。

## 可用入口

| 方法与路径 | 输入 | 成功返回 |
|---|---|---|
| GET `/api/health` | 无 | 200 健康视图；数据库不可用为 503、`ready:false` |
| GET `/api/sim/session` | Cookie 可选 | 200 模拟会话状态；缺失、非法或过期为 `authenticated:false` |
| POST `/api/sim/session` | `{"login_secret":"可信启动秘密"}` | 200 模拟会话状态及随机 Cookie |
| DELETE `/api/sim/session` | 当前 Cookie 和 `{}` | 204 并删除 Cookie |
| POST `/api/chat` | 下列明确命令 | 200 TaskResponse |
| GET `/api/tasks/:task_id` | 当前 Cookie | 200 本人任务受限视图；其他 owner 和未知任务均 404 |

会话状态只有 `mode` 和 `authenticated`，不是正式身份认证结果。健康视图如下；`ready` 只表示当前已装配的本机模拟服务及数据库可达，不表示真实模型、资金、正式认证或审计就绪：

```json
{"contract_version":1,"mode":"PROJECT_SIMULATION","ready":true,"llm_mode":"MOCK_LLM","funds_execution_enabled":false,"formal_auth_enabled":false,"formal_audit_enabled":false}
```

创建命令只允许：

```json
{"kind":"NEW_READ_ONLY_DEMO"}
```

服务端生成新 task_id，沿合同 16 接收固定 MockLlm 提案、保存任务，再沿合同 19 执行到第一个等待点。首个等待的 revision 为 2、plan_version 为 2、facts_version 为 7。创建没有客户端幂等键；重复 NEW 会创建另一条只读任务。首响应丢失且没有 task_id 时必须停止新增并人工核对，不自动发送第二次 NEW，不把创建标为幂等。

续接命令示例：

```json
{"kind":"CONTINUE","task_id":"task_...","waiting_node_id":"choose","task_revision":2,"plan_version":2,"facts_version":7}
```

节点和版本仅作精确比较，不是授权。只能提交最近查询的 `next_action.kind=CONTINUE` 对应节点及版本；计划、参数、事实、完整来源、会话和注册信息仍由服务端重核验。`task_revision` 为 1..9007199254740991，`plan_version` 和 `facts_version` 为 1..4294967295；公开 task 和 operation 版本也遵守这组上限，异常存储版本直接拒绝响应，不能由 JavaScript 四舍五入。

现有 UserDecision 没有业务问题、选项或回答投影，因此 CONTINUE 只接受原等待节点并运行剩余只读节点，不解析答案、不修改计划/参数，不是金融确认。两次明确续接后任务分别到 revision 4 的第二个等待点和 revision 6 的完成态。查询、恢复和已结算请求重放为 Tool0；来源复查可以调用已有余额 Policy/BankGateway，不能宣称 BankGateway0。

## TaskResponse

公开 DTO 只序列化下列 14 个字段，不直接序列化内部 TaskRecord：

```json
{
  "contract_version":1,
  "mode":"PROJECT_SIMULATION",
  "task_id":"task_...",
  "task_state":"WAITING_USER",
  "revision":2,
  "plan_version":2,
  "facts_version":7,
  "operation_state":null,
  "security_outcome":null,
  "security_scope":"CURRENT_DECISION_NOT_PROVIDED",
  "operations":[],
  "recovery":{"kind":"AWAIT_USER","node_id":"choose"},
  "next_action":{"kind":"CONTINUE","node_id":"choose"},
  "replayed":false
}
```

`task_state` 沿合同 01。`operation_state` 仅在 operations 恰有一项时展示该已记录状态，否则为 null；不会推导 READY 或资金成功。`security_outcome` 始终为 null，`security_scope` 始终为 `CURRENT_DECISION_NOT_PROVIDED`，不输出当前授权裁决。每项 operations 仅有 `operation_id/node_id/operation_state/revision/plan_version/facts_version`，不包含业务原单号。

`recovery` 沿合同 15：`AWAIT_USER`、`REVIEW_NODE` 带 node_id，`RECONCILE_ORIGINAL`、`REVIEW_RECORDED_RESULT` 带 operation_id，`FINISHED` 带 task_state。这些是记录位置提示，不是重提指令。`next_action` 只有以下三种：

| kind | 条件与含义 |
|---|---|
| CONTINUE + node_id | 当前已结算等待点，且合同 19/20 的来源链和当前版本均核验成功；可发送一次明确 CONTINUE |
| REVIEW_ORIGINAL_TASK | 原号复核；pending、证据不足或旧合同 09 等待点不开放继续 |
| FINISHED | 当前记录已终结，不开放继续 |

合同 09 旧等待点缺少首次执行来源时，HTTP 保留合法受限查询，但只返回 REVIEW_ORIGINAL_TASK；不补造来源。坏记录、篡改证据或注册漂移返回错误。完全相同的已结算历史 CONTINUE 可 Tool0 重放，`replayed:true`，但 HTTP 返回当前公开视图和当前 revision，不能把合同 20 的历史收据状态作为当前等待点。

禁止输出 owner、user_id、原计划、原参数、完成节点集、会话、登录秘密、Permit、审计内容、内部结果引用或 ToolResult 正文。

## 错误与未启用入口

```json
{"contract_version":1,"error":{"reason_code":"DEPENDENCY_UNAVAILABLE","message":"固定安全提示","task_id":"task_...","next_action":"QUERY_ORIGINAL_TASK"}}
```

可供核对的原 task_id 已确定时返回该值及 `QUERY_ORIGINAL_TASK`；否则 task_id 为 null、next_action 为 `NONE`。固定 reason_code 为 `VALIDATION_ERROR`、`NOT_AUTHENTICATED`、`FORBIDDEN`、`NOT_FOUND`、`STALE_PLAN`、`IDEMPOTENCY_CONFLICT`、`DEPENDENCY_UNAVAILABLE`。格式/版本 400、会话 401、来源/安全拒绝 403、本人范围内未知任务 404、方法 405、正文超时 408、冲突 409、超大正文 413、媒体类型 415、未启用 501、依赖或结果未能确认 503。

确认 POST `/api/operations/:operation_id/confirm`、正式验证 POST `/api/auth/challenges/:challenge_id/verify`、审计 GET `/api/audit/operations/:operation_id` 和 GET `/api/audit/:id` 在当前会话核验后统一返回 501 `DEPENDENCY_UNAVAILABLE`；没有默认成功、空审计结果或资金写入。没有接入既有隔离模拟强验证以冒充正式验证。

超时、断网、503、401、畸形或未知响应都不得自动重提 NEW/CONTINUE。已知 task_id 时先查询原号；查询失败应清除可继续状态。pending 无自动恢复执行或人工重试开关。前端不缓存执行权限，不接受未知版本/枚举或不安全数字，不从文案、task_state、recovery 或历史响应推导 CONTINUE。

## 依赖与交付范围

API 的 Runtime 审计记录器为进程内 `InMemoryAuditRecorder`，重启后丢失，不提供正式审计查询；合同 19/20 的公共持久执行证据仍保存在 PostgreSQL。数据库连接及秘密只从私有启动环境取得。新增 HTTP 依赖使用官方 Cargo 包和锁文件，不修改成员业务或已有迁移。

公共 HTTP、服务端模拟会话、公共确认/认证机制、Gateway 与页面装配由不高兴/底座负责。本轮已实现只读 HTTP 与模拟会话；公共确认/验证的完整 HTTP/Runtime 装配、真实模型 Gateway 及尚未完成的公共安全写机制仍由底座继续实现，不将底座未做机制全部归为等待别人材料。正式身份/认证的可信来源和 D-01 / D-18 具体方式、等级、期限及撤销协调约定待确认；真实 LLM 的 Base URL、模型名、协议及本地凭据由用户提供，Gateway 由底座实现。

逐风负责转账业务实现、幂等/回执和原号查单合同；风星龙负责正式审计事件、Outbox/补写及查询合同；Jonathan 负责账单，结夏/Hannah 负责卡与订阅的业务语义。底座负责职责内的公共核验、迁移和接入。全队业务 C-HTTP/TaskResponse 由底座统一公共协议、对应业务负责人确认业务字段；未确认的业务回答、正式来源或规则只暂停依赖它们的接入部分，不阻塞已有依据的公共机制。本合同不表示整个 BASE-05/06 或任何成员业务验收完成。生成侧和独立验收结果分开记录，启动与复现见[本轮交接](../handoff/SafeFlow_本机只读HTTP与模拟会话_v0.1.md)。
