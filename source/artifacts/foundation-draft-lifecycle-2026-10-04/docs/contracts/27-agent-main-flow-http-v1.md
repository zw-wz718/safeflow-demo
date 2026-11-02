# Agent 主流程 HTTP v1

2026-10-10，按 `docs/handoff/NEXT_AGENT_MAIN_FLOW_TASK.md` 实施底座公共装配。
新增合同27；复用23的真实 Gateway、16的严格 intake、19/20持久只读调度和26模拟草案生命周期。
不是成员业务合同、资金接口或独立验收结论。旧入口和固定样例保持兼容。

## 无API固定计划补充（2026-10-11）

按最新任务优先级，可信服务端 `SAFEFLOW_AGENT_FIXED_PLAN_ENABLED=1` 明确启用固定计划。
二进制默认关闭；与真实Gateway互斥，真实失败不自动回退，也不启动协议fixture。
status新增planning_mode=DISABLED/FIXED_PLAN/OPENAI_COMPATIBLE；configured仍只表示真实Gateway配置。
固定模式拒绝自由文本messages；`POST /api/agent/samples`只接受sample、可选selection/context，
sample严格为READ_WAIT、CREATE_200、MODIFY_300、CANCEL_DRAFT。客户端不能提交任意计划或确认字段。
服务端由受控对象/实际当前草案构造有限PlanningResult，再经现有MockLlm和同一严格intake。
账户/候选选择、等待继续、确认和执行、未知屏障均沿下方现有机制。缺对象澄清不创建任务。

Agent wrapper新增nullable planning_source：MOCK_LLM或OPENAI_COMPATIBLE按实际受检review记录。
只读沿原planning_gateway_source；草案来源随既有action.request、摘要和同事务审计保存，无SQL迁移，
query按所选原operation读取。旧草案未记录来源为null，不能按当前配置补成真实模型来源。
原合同26公开DTO保持不变；来源只用于说明，不替代确认或权限。切换模式/进程重启保留原来源。
前端固定模式只用有明确样例名称的选择器，不提供自由自然语言入口，不声称具备理解能力。

## 装配和能力

Gateway 配置完全沿用合同23的服务端环境变量，不提供供应商默认值，不读取其他凭据。
本轮任务指定 Command Code DeepSeek；完整地址/型号及账号权限边界见生成报告的 STARTUP.md，
它们仍为显式服务端配置，没有硬编码回退或浏览器密钥入口，真实账号尚未验证。
未配置且未启用固定模式时 status 返回 configured=false，message 返回503/LLM_NOT_CONFIGURED；不降级为Mock。
仅启用余额时模型只见 get_account_balance；显式启用合同26时，既有生命周期 Runtime
额外注册已交付的 get_account_balance/find_payees Tool/Policy，供统一主流程使用。
没有新增 Runtime 引擎、业务插件或账户/收款人数据。旧只读/草案路由继续使用原合同范围。

模型可提出现有只读DAG（含UserDecision等待），或一个创建/修改/取消草案Tool节点。
不支持同一计划混合读写、多个写动作或业务结果参数引用，拒绝此类提案而不部分执行。
模型工具名称、schema、依赖、task_id/plan_version和内层重复键仍严格校验。
单写提案通过intake后，只将已核对的动作及原参数映射到合同26的规范持久计划；
模型不决定Operation身份、确认内容、领取、状态迁移或成功结论。模型原始文本不持久化。

## 路由

全部沿既有本机Host/Origin、模拟Cookie、命令头、禁止缓存和等待后会话复核。
请求拒绝额外字段，所有参数来自类型化输入或既有可信服务。

| 路由                                    | 输入                                       | 行为                                                                                                  |
| --------------------------------------- | ------------------------------------------ | ----------------------------------------------------------------------------------------------------- |
| GET /api/agent/status                   | 无                                         | configured、NOT_VERIFIED、实际可用工具名、draft_enabled；无模型或工具调用                             |
| POST /api/agent/payees                  | query                                      | 查询本人实际已交付MockBank保存的收款人候选；不自动选择                                                |
| POST /api/agent/messages                | message、可选selection/context             | 一次Gateway调用、严格校验；澄清不创建任务，读计划保存初始任务，草案计划映射为待确认动作；零写Tool调用 |
| GET /api/agent/tasks/:task_id           | 无                                         | 本人原号状态、实际已核验的只读历史结果或合同26草案视图；不重做模型或工具                              |
| POST /api/agent/tasks/:task_id/execute  | task_revision、plan_version、facts_version | 原只读计划永久领取后实际Runtime执行                                                                   |
| POST /api/agent/tasks/:task_id/continue | 同上及waiting_node_id                      | 原计划、原等待节点、原版本继续；不重新规划                                                            |
| POST /api/agent/tasks/:task_id/prepare  | operation_id                               | 复用合同26准备；只使用已查询选定的候选，不调用旧固定样例候选填充                                      |

确认、拒绝和实际写调用直接复用合同26原operation路径及DTO，不新增授权字段。
准备确认单不会调用写Tool；浏览器明确确认后才发送确认，再按原引用/版本执行。

message是1..4096 UTF-8 bytes。selection仅含可空的account_id、payee_query、payee_id：
账户须用户明确指定并通过实际Gateway归属/返回核对；收款人必须由用户选择实际查询候选，
服务端在本任务上下文再次查既有Store的候选并核对所选ID。模型不能改变选择，缺选择时澄清。
修改现有字段时，未变的账户/收款人沿实际草案；改变的对象仍须用户明确选择并重新核验。
当前没有账户列表合同，页面提供账户编号输入，不猜默认账户。

context仅用于已保存草案的同任务修改/取消：task_id、task_revision、plan_version、
facts_version、selection_ticket。进入模型前核对当前公共版本，读取本人实际Store草案，
只向模型提供draft_id、draft版本、字段及状态。模型返回的draft_id/expected_version
必须完全相同，select事务继续复核来源和版本。Store缺失、过期、未知或任务终结拒绝。
尚未保存的初始草案不能被当成已存在的业务草案修改；用户可拒绝本次提案或明确另建任务。

澄清回复在前端仅内存保存的当前用户消息中合并，仍作为不可信user文本，受4096 bytes预算。
登录代际变化、退出、另建任务时清除上下文；不向模型发送身份、Cookie、确认、Policy或Permit。
工具输出不能作为授权指令；只有上述有界实际草案字段作为system中的controlled_task_data。
旧合同23Gateway调用没有该字段，三字段协议保持原样。

## 结果与兼容

统一wrapper严格包含contract_version=1、mode=PROJECT_SIMULATION、kind、task、draft、
results、clarification、can_start。READ使用旧TaskResponse；DRAFT使用原合同26视图；
CLARIFY仅有纯文本问题。三者的其他载荷互斥为空，客户端检查task和operation身份。
can_start是后端的当前提示，不能代替执行时的会话/注册/版本/永久领取复核。

仅新的Agent只读启动方法选择持久化已核验ToolResult正文，随原结算事务和审计原子保存。
结果必须匹配已有结果摘要、工具/操作/引用，继续段累计结果须保持原前缀，新增段匹配原摘要。
正文只是历史结果，不代表当前余额或授权；界面显示工具as_of。查原号不重新查询银行。
2026-10-19历史兼容修订：Agent GET通过query_saved_history核验原保存链、结果摘要、审计、会话和owner后投影历史，不要求当前工具实现与旧报告相同；未装配/升级后仍可读取已存正文。GET不执行Policy或Tool、不赋予继续权；execute/continue继续核验当前注册和原计划绑定。账单V1/V2差异见合同33扩展。
旧schedule、合同19/20入口仍只存引用，旧回执可缺results；缺正文返回null，不补查或伪造结果。
没有SQL迁移。可选字段缺失时序列化不增加键，旧回执摘要链保持兼容。

Create/Modify实际成功后仍WAITING_USER，页面允许自然语言修改/取消；真实Cancel后为CANCELLED，
按服务端FINISHED结束。草案保存/修改/取消不是资金交易。
超时可能保留原task_id；客户端不自动重试POST。未知只查询原任务/操作，不换号、不重领、不解封。
供应商故障、非法提案、缺Policy/工具、来源缺失沿固定错误码，敏感内容不进入错误或日志。

## 验证边界

### 2026-10-13 启动装配补充

API 的 `assembly.rs` 集中现有五个工具的具体 Tool/Policy 构造、中央最低等级、
schema/Policy 版本、批准输入合同及执行类别。三个装配范围分别保留余额旧投影、
旧创建草案投影和 Agent 生命周期 v2 投影。该配置只由服务端源码提供，不能通过请求或模型修改。

服务发布前复用 RuntimeBootstrap 核对 Tool/Policy/Profile，复用 PlanningIntakeService
核对输入合同与模型定义，并使用原参数校验器检查 schema 能否按批准投影处理。
缺项、重复、名称/版本不一致、不支持的投影、无已批准执行路径或执行类别不一致拒绝启动。
`has_side_effect` 和名字仅用于核对已批准绑定，不产生等级或执行许可；未来工具无默认等级或路径。

Agent status、模型清单及派发读取同一份 CheckedAssembly。只读使用已有持久 Runtime；
草案类别只映射已有 Create/Modify/Cancel 入口。账户显式选择/可信核验、单写节点限制、
确认、归属和原号恢复保持原合同。候选查询 HTTP 是原有选择辅助接口，不等同于模型工具注册。

Runtime 来源元数据增加后端装配元数据，包含实际输入合同、执行类别及装配/Agent 派发源码摘要；
既有工具、策略和 Profile 元数据继续参与注册摘要。该元数据仅绑定来源，不授予执行权限。
本次 Runtime/装配源码变更会使旧来源摘要不同，未执行旧计划或旧确认若不匹配当前来源，
按既有合同拒绝启动/继续或报告来源不可用，不改写旧记录、重领或绕过验证。
新增已批准工具仍需明确其合同、版本、等级、可信数据来源和既有执行路径；新业务语义及
混合读写、多写节点、动态结果引用、资金执行均不因该装配入口自动获得支持。

实际HTTP/PG和浏览器测试使用本机模型协议响应服务，业务使用现有实际Tool/Policy/Store。
它不证明真实供应商联通或模型自然语言能力。真实供应商仍待用户配置和验证。
成员资金执行、正式身份、最终Permit/PRE_COMMIT、Outbox、业务Store持久化和未知解封仍待对接。
生成侧报告：`artifacts/agent-main-flow-2026-10-10/REPORT.md`，独立检查另行验收。

### 2026-10-15 服务端只读公共接入口

公开 Rust 入口 `ApiState::for_project_simulation_with_read_only_tools` 接受原有 BankGateway
和 `Vec<ReadOnlyToolRegistration>`。这是可信启动代码中的显式中央批准，不是 HTTP DTO、
模型配置或动态加载接口。注册项必须提供实际 Tool、Policy、Policy/schema 版本、中央 Profile、
PreflightInputContract、批准 action 及已审阅实现的 implementation_revision；无缺省批准。
最低等级来自 Profile，只读路径来自本入口的明确选择，名称和 has_side_effect 仅用于核对。
实现修订由可信部署方维护，变更 Tool/Policy 实现时必须同时更新；不声称运行时自动散列 trait 实现。

本入口只接受已有 `bounded-direct-input-v2` 投影声明和既有只读 ToolResult；扩展 schema
还须通过只读恢复现用的平坦直接对象校验，不启用嵌套业务参数或结果引用。
包含扩展的余额装配将原余额输入一并纳入同一 v2 校验模式；空扩展的旧构造保留原投影和范围。
复用 RuntimeBootstrap、原 schema 校验器及 PlanningIntakeService；名称/版本/schema 不符、
缺配置、重复（包括内置工具）、写工具或不支持投影，均在 ApiState 返回前拒绝。
必需字段通过 Rust 类型要求提供，不能从请求补齐。部分注册失败不发布任何状态。

同一组批准项加入旧只读 Runtime 和启用时的 Agent 生命周期 Runtime；Agent status、
规划、执行、继续与查询仍从同一 CheckedAssembly 选择。旧封闭创建草案装配不接收扩展，
草案决策合同仍仅含原草案工具。生命周期已内置 find_payees，重复接入它会拒绝，不能覆盖；
公开例从默认仅余额配置接入原 FindPayeesTool/FindPayeesPolicy，不启用新业务。

实际批准 action、实现修订、输入合同、执行类别和既有 Tool/Policy/Profile 全部参与来源摘要。
同配置重建保持稳定；批准变更后旧任务启动/继续按原来源检查拒绝，历史记录不被改写或重新执行。
Runtime 原只读 bindings 摘要在有后端装配元数据时同时绑定完整既有注册来源元数据（含未使用工具的批准），确保恢复和所有续跑入口
核对同一批准内容；无后端元数据的 Runtime 保留原摘要格式。新会话仍按原本人恢复合同核验，
不要求持有旧登录会话。注册变化可导致旧结果来源不可用，此时不补查、不复写历史结果。
装配源码本轮变化会改变旧来源摘要，不提供绕过兼容。仍为 PROJECT_SIMULATION，正式身份、
其他成员业务和真实模型均未因此启用。示例：`crates/api/examples/readonly_plugin.rs`。
