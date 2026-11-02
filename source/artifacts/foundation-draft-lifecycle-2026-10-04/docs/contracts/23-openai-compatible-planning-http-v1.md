# OpenAI 兼容模型规划 HTTP 合同 v1

2026-10-08：依据原始 `SafeFlow_完整项目直拷版_2026-10-02/docs/collaboration/prd/02_底座PRD.md` 的 BASE-R06，以及 `docs/collaboration/tasks/02_不高兴.md` 的 BASE-06，实现底座自有 Gateway、结构化提案接收、持久初始任务与配置状态接口。完整流程 PRD 中的 OpenAI 兼容要求作为传输依据。评审建议不自动视为冻结合同。

本协议复用合同 16/19/20/21，`contract_version:1`、`mode:PROJECT_SIMULATION`。真实模型只提出既有注册白名单计划；当前 HTTP 装配仅包含已批准的 `get_account_balance` 只读 Tool/Policy。自然语言、模型输出和界面状态均不提供身份、确认、认证或执行许可。业务保存、资金、正式身份、PRE_COMMIT、Permit 和正式审计继续待对接，合同 22 的确认 scope 不转换为草案保存授权。

## 服务端配置与传输

默认没有真实模型连接。启用需要服务器进程环境显式提供全部材料，HTTP 正文不能提供或修改配置：

| 环境变量 | 规则 |
|---|---|
| `SAFEFLOW_LLM_ENABLED` | `1` 才尝试装配；未设置或 `0` 不装配 |
| `SAFEFLOW_LLM_ENDPOINT` | 完整 chat/completions URL，不自动拼接路径；HTTPS，仅字面 `127.0.0.1` / `::1` 回环地址允许 HTTP；禁止 userinfo、query、fragment，长度不超过 2048 bytes |
| `SAFEFLOW_LLM_MODEL` | 明确模型名，非空，无控制字符或首尾空白，不超过 200 bytes |
| `SAFEFLOW_LLM_API_KEY` | 明确 Bearer 凭证，仅进程环境私下注入；非空可见 ASCII，无空白，不超过 4096 bytes |
| `SAFEFLOW_LLM_TIMEOUT_SECS` | 明确整数 `1..120` 秒，覆盖连接、响应头和完整正文读取 |

启用但材料不完整时保持 Gateway 未配置，原 Mock 功能仍可用；完整但非法配置拒绝启动。没有公共默认 endpoint、model、key 或网络调用。配置状态接口不发探测请求，`configured:true` 不表示已连通。状态里的 `connectivity` 始终为 `NOT_VERIFIED`；某次成功 POST 只证明那一次真实响应被接收和校验。

采用成熟 `reqwest` / Rustls 客户端，禁止环境代理、自动重试、重定向和 HTTP/2；不使用连接池重放请求。只发送一次非流式 `POST`，JSON 包含明确 model、两条 system/user message、`response_format:{"type":"json_object"}`、`n:1`、`stream:false`、`max_tokens:8192`。服务端 system message 提供完整 PlanningResult JSON Schema、实际 ToolDefinitions 及服务端 task_id/plan_version，用户文字独立位于 user message。不会发送用户身份、会话、Policy、凭证、Permit、业务结果或历史任务正文。

供应方须支持上述 OpenAI 兼容 Chat Completions / JSON object 协议；不支持时明确失败，不自动切换其他协议、修复、重试或降级为 Mock。请求序列化上限 128 KiB，响应上限 256 KiB，实际分块读取也计数；压缩响应拒绝。整个模型调用 deadline 覆盖正文读取。HTTP 规划外围 deadline 为该配置加 10 秒，最多 130 秒，用于 intake 和数据库等待；前端该单次请求 deadline 为 135 秒。

只接受 HTTP 200、JSON MIME、唯一 choice/index=0、finish_reason=stop、assistant string content。拒绝工具调用、function_call、非空 refusal、截断、多 choice、markdown/非结构化 content；合法 `refusal:null` 可接受。外层响应使用独立传输预算，允许 usage 中合法浮点，但仍拒绝重复键。内层 content 复用合同 16 的原始重复键有界 parser：64 KiB、深度 16、最多 4096 values、256 元素容器、200 bytes key/identifier、4096 bytes text，拒绝任意嵌套重复键和浮点数字。未知字段、组合/版本、task/version、直接参数和 Kernel 结构均继续经过原校验。Gateway 不改写模型原 plan 的任务号、版本或参数。

错误仅使用固定原因，不输出供应方响应正文、密钥、endpoint、用户输入或原始模型 content；配置及 Gateway 没有 Debug/Serialize 实现。

## 路由与 DTO

沿合同 21 的本机 Host/Origin、Cookie、命令头、正文上限、禁止缓存与等待后会话复核规则。请求拒绝额外和重复字段。

| 路由 | 输入 | 行为 |
|---|---|---|
| GET `/api/llm/status` | 无 | 仅返回配置状态；不调用模型、Tool 或写数据库 |
| POST `/api/llm/plans` | `{"message":"查询 account_001 的余额"}` | 非空，最多 4096 UTF-8 bytes；至多一次真实模型调用；受检 PASS 保存初始任务，CLARIFY 不创建任务；零 Tool 执行 |
| GET `/api/llm/plans/:task_id` | 无 | 本人读取原号、持久模型来源和受限 TaskResponse；不调用模型或 Tool，不创建执行 claim |
| POST `/api/llm/plans/:task_id/execute` | `{"task_revision":1,"plan_version":1,"facts_version":1}` | 用户另行明确启动原受检只读计划；版本仅作比较，实际计划/owner/原会话来自数据库与 Runtime；复用合同 19 的永久认领和安全调度 |

配置状态严格为 7 字段：

```json
{"contract_version":1,"mode":"PROJECT_SIMULATION","planner_source":"OPENAI_COMPATIBLE","configured":false,"connectivity":"NOT_VERIFIED","reason_code":"LLM_NOT_CONFIGURED","tool_scope":"READ_ONLY_BALANCE"}
```

已配置时 `configured:true`、`reason_code:null`；不输出供应方配置。

规划与查询成功 wrapper 也严格为 7 字段：

```json
{"contract_version":1,"mode":"PROJECT_SIMULATION","planner_source":"OPENAI_COMPATIBLE","kind":"TASK","task":{"contract_version":1,"mode":"PROJECT_SIMULATION","task_id":"task_...","task_state":"RUNNING","revision":1,"plan_version":1,"facts_version":1,"operation_state":null,"security_outcome":null,"security_scope":"CURRENT_DECISION_NOT_PROVIDED","operations":[],"recovery":{"kind":"REVIEW_NODE","node_id":"read"},"next_action":{"kind":"REVIEW_ORIGINAL_TASK"},"replayed":false},"clarification":null,"can_start":true}
```

`task` 复用合同 21 的原 14 字段投影，不含 plan、参数、身份、凭证或业务结果正文。示例 entry node_id 来自实际合法计划，不能写死。plan_version=1/facts_version=1 是本轮底座初始公共规划版本，不是 MockBank 数据版本、银行事实新鲜度或成员业务版本；旧固定演示仍使用 2/7。

`can_start:true` 仅表示受检来源、原规划 session hash/binding_version、当前注册来源均匹配，且后端核对尚无 runtime read claim，任务仍初始 RUNNING/revision=1/无 operation。它是提示，schedule 再次核验全部来源、当前版本和只读门禁，不能把提示当授权。明确启动完成、失败或中断已有 claim 后均为 false；并发旧提示不越过永久认领。新登录、新进程可以查询同 owner 的原号，但未执行的旧计划不重新授权，can_start=false；不伪造确认或自动重规划。

澄清只投影模型原 question，须按不可信纯文本展示：

```json
{"contract_version":1,"mode":"PROJECT_SIMULATION","planner_source":"OPENAI_COMPATIBLE","kind":"CLARIFY","task":null,"clarification":{"question":"请指定要查询的账户"},"can_start":false}
```

澄清没有 task_id、执行提示或数据库任务。后续输入是另一项用户明确提交的新规划，不自动再次请求。明确执行到 WAITING_USER 时，继续使用合同 21 `/api/chat` 的 `CONTINUE`；继续后可查询本协议原号 wrapper 核对来源。

## 来源与失败恢复

`LlmGateway::generate(PlanningInput)` 与旧 PlanningInput 字段不变。新增默认分派的 context-aware 方法只传后端 task_id/plan_version；旧 Mock 保持可复现，真实 Gateway 拒绝无 trusted context 的旧直接调用。受检 `PlanningPersistenceReview` 捕获 Gateway 来源，只有该封闭 review 参与的创建事务才在 `TaskCreated.details.planning_gateway_source` 追加标记，同原任务和原 session/registration anchor 原子提交。该标记与 planning_origin 同级，旧严格解析及迁移不变。未经 intake 的手写任务不获得模型来源；原 Mock 或旧无标记任务在本协议 GET/execute 返回 404，旧通用任务 GET 保持兼容。

查询在本人只读一致快照中核对唯一 TaskCreated、revision=1、原创建摘要、当前 plan/facts 摘要、原 planning_origin 和记录来源。不根据 task_id、客户端字段或未核验的页面值推断模型来源。执行仍由既有 Runtime 复核原 owner、session、注册、TaskRevision/PlanVersion/FactsVersion、只读 review 和永久 claim；不改变其安全规则。

错误复用合同 21 envelope。未配置返回 503 / `LLM_NOT_CONFIGURED`；transport、HTTP、deadline、请求/响应长度等故障返回 503 / `LLM_GATEWAY_UNAVAILABLE`；结构、版本、重复键、非结构化结果、工具/参数越权和非法图返回 422 / `LLM_PROPOSAL_REJECTED`。客户端格式/额外配置字段返回 400 / `VALIDATION_ERROR`；会话、原号和版本沿原错误规则。

明确模型或 intake 失败未进入创建时不提供假 task_id。进入数据库后的故障或外围 deadline 可能存在未知提交，返回已保留原号供查询；不声称回滚、不重复模型规划或 Tool 调用。NEW 没有创建幂等键，前端不会自动重试 POST。原号 query/execute 不依赖模型配置仍在，可在供应方配置缺失或中断后核对既有任务。

## 验证与待对接

永久测试使用实际本机 HTTP 传输服务，它只提供 OpenAI wire 响应，不实现或替代业务 Tool/Policy。六组 Gateway 测试与四组新增实际 HTTP/PG 测试覆盖单次调用、格式/工具拒绝、分块预算/deadline、GET 零调用、原号单次执行、来源隔离、撤销与重启/新会话提示。生成侧自测与主检查独立验收分开记录，见[本轮交接](../handoff/SafeFlow_OpenAI兼容模型规划_v0.1.md)。

真实供应方 live 连接仍待用户提供完整 endpoint、model、明确协议兼容性及私下注入的 API key；未取得材料，不宣称供应方连通、真实账户或业务集成验收通过。BASE-06 父项仍有成员交付与 C-HTTP/C-REG 等待对接材料，不以这一步或传输 fixture 完成整个业务演示。
