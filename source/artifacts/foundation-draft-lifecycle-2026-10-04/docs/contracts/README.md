# SafeFlow 公共合同索引 v0.2

本目录冻结 Runtime、SecurityKernel、DomainPolicy 和 Tool 之间的第一版安全合同。这里的合同优先于早期流程文档中的示意性类型和状态名称；发生变更时必须提升相应 `contract_version`，并更新合同测试。

2026-10-06 安全补齐：[合同08](./08-recovery-storage-v1.md)与[合同09](./09-runtime-wait-recovery-v1.md)补充实际持久化事务的会话复核及兼容性要求。公开参数 / 存储版本 / 迁移保持，旧 resolve-only 会话源必须适配版本化快照；创建 / 更新 / 读取 / 幂等 / 拒绝审计均受原绑定 guard 守护。提交窗口不冒充跨系统原子撤销，不提供正式身份或业务执行；本子项已[独立验收通过](../../../persistence-session-review-2026-10-06/REVIEW.md)，BASE-05 父项仍部分完成，详情见[本轮交接](../handoff/SafeFlow_持久化入口会话复核_v0.1.md)。

## 第零步合同

| 合同 | 文档 | Rust实现 | 核心验收 |
|---|---|---|---|
| v0.1范围与不变量 | [00-v0.1-scope.md](./00-v0.1-scope.md) | 不适用 | 第一版边界清楚 |
| 三维状态 | [01-state-machines.md](./01-state-machines.md) | `states.rs` | 非法迁移Fail Closed |
| 执行许可 | [02-execution-permit.md](./02-execution-permit.md) | `execution_permit.rs` | 未授权、过期、重放、错参均拒绝 |
| 操作指纹 | [03-operation-fingerprint.md](./03-operation-fingerprint.md) | `fingerprint.rs` | 同义输入稳定，关键参数变化必变 |
| 执行后观察 | [04-post-execute.md](./04-post-execute.md) | `execution_observation.rs` | 未知结果只查单、不重提 |
| 事务边界 | [05-transaction-boundaries.md](./05-transaction-boundaries.md) | [许可存储事务原语](./12-execution-permit-storage-v1.md)，完整业务事务待对接 | 不产生部分账务变化 |

## 第一步合同

| 合同 | 文档 | Rust实现 | 核心验收 |
|---|---|---|---|
| Runtime与安全公共合同 | [06-runtime-security-public-contracts.md](./06-runtime-security-public-contracts.md) | `planning.rs`、`tool.rs`、`security.rs` | 身份不由模型指定，Tool与Policy边界独立，PRE_COMMIT缺证据即拒绝 |

## 权威边界

2026-10-12 增加[公共原请求存储与提交事务组合 v1](./28-public-execution-storage-v1.md)：本人原键／稳定业务号／公共O不可变映射、独立业务版本、冻结首条公共SUBMITTED记录与当前Operation读取、持锁重放跳过回调及新许可消费。复用既有许可事务与最终守卫，新增迁移14；不提供银行业务回执、资金终态适配、正式C-AUD或Outbox，正式签发及资金路径仍未启用。

2026-10-08 增加[OpenAI兼容模型规划 HTTP v1](./23-openai-compatible-planning-http-v1.md)：服务端明确配置的真实 Gateway 单次传输、完整 PlanningResult Schema 与既有严格原始 JSON intake；PASS 仅创建初始受检任务，CLARIFY 不创建任务，零自动 Tool 执行。独立配置状态只表示 configured，不冒充供应方 live 连通；本人纯 GET 核对持久来源，新会话旧未执行计划不提供启动提示，用户另行明确只读 execute 复用原号永久认领。旧 Mock 和草案确认 scope 保持，正式业务写继续待对接，见[交接](../handoff/SafeFlow_OpenAI兼容模型规划_v0.1.md)。

2026-10-08 增加[本机模拟草案公共确认 HTTP v1](./22-local-draft-confirmation-http-v1.md)：独立草案准备 Runtime 复用既有 MockLlm / 合同07 Tool与Policy，保存 checked 来源锚点、公共 DRAFT、真实 PRE_TOOL/PRE_AUTH 和冻结展示票据。明确确认/拒绝核验实际持久来源，保持 RUNNING/DRAFT，不启用业务保存、Permit、正式验证或资金。原只读 assembly 保持兼容；GET 只读，过期/撤销不续期，中断明确按原号准备。生成侧与独立验收分别记录，见[交接](../handoff/SafeFlow_本机模拟草案公共确认_v0.1.md)。下方合同21的确认501描述保留为未开启新scope时的原行为。

2026-10-08 增加[本机项目模拟只读 HTTP v1](./21-local-read-only-http-v1.md)：通过 Axum 接入合同 15/16/19/20，使用可信启动秘密与随机进程会话，公开受限 TaskResponse，支持固定 MockLlm 只读新建、本人查询及明确续接。旧等待点只复核原号，历史重放返回当前公开视图，异常数字/响应不提供继续提示；确认/正式验证/审计入口明确 501。该受限协议不冻结全队业务 C-HTTP，不启用真实身份、资金或正式审计。生成侧自测和独立验收分开记录，见[本轮交接](../handoff/SafeFlow_本机只读HTTP与模拟会话_v0.1.md)。

2026-10-08 增加[原任务只读显式续跑 v1](./20-persisted-read-only-runtime-continuation-v1.md)：合同 19 等待点经明确继续命令按原计划运行剩余只读节点，可多次等待，承接结果引用；永久分段认领和审计/收据链核验防重复执行。中断仅原号核对，不自动重试；不解析业务答案、不启用 HTTP 或写权限。

2026-10-08 增加[已存任务只读 Runtime 执行 v1](./19-persisted-read-only-runtime-execution-v1.md)：模型初始任务按原号领取，重校验数据库计划与 Schema，实际执行既有只读 Tool，完成/等待及审计原子保存；持久领取阻止重复调用，进程中断仅原号核对，不自动重试。恢复允许当前本人重新登录，不新增业务写或 HTTP 合同。旧等待入口保持原语义，历史缺创建来源的任务拒绝新调度。

2026-10-06 增加[模型计划安全接收 v1](./16-model-planning-intake-v1.md)：可信 Runtime 装配、服务端预期任务 / 版本、严格有界 Serde JSON / 同一强类型校验、已批准直接参数与工具绑定、Kernel 结构门禁、等待后会话复核。仅接收模型提案，不创建数据库任务、执行业务或提供授权；MockLlm 可运行，真实模型 / HTTP / 正式身份未接通。BASE-06 仅本子项已[独立验收通过](../../../planning-intake-review-2026-10-06/REVIEW.md)，父项仍部分完成。

2026-10-06 增加[本人任务进度查询与安全范围视图 v1](./15-task-progress-query-v1.md)：复用可信 Runtime 版本化会话，在 PostgreSQL 只读一致快照中查询本人任务 / 操作版本及受限恢复提示；不输出计划、参数、凭证或业务原单号。明确不提供当前安全裁决、不查银行、不自动续跑；只是 BASE-05 公共查询子项，不冻结业务 HTTP / TaskResponse。

2026-10-05 增加[可信前置裁决持久化与核验 v1](./14-trusted-preflight-decision-storage-v1.md)：公共存储复用可信 Runtime 的实际 Kernel/Policy 评估，绑定数据库完整快照、原会话和注册/输入/部署版本，支持持久幂等、撤销/失效及时间围栏。只保存 PREFLIGHT_ONLY 的 PRE_TOOL/PRE_AUTH，不执行 Tool、不推进 READY；写工具前置判断不等于写权限。未知投影拒绝，PRE_COMMIT/正式 Permit/资金仍未启用，不以已有演示查询替代通用机制。

2026-10-05 增加[隔离模拟验证与认证凭证 v1](./13-isolated-simulation-auth-v1.md)：底座自有服务端随机秘密核验、挑战/尝试/独立模拟凭证持久化、原会话/确认/当前快照绑定、幂等、终态、撤销及持久时间高水位。正式认证入口未启用，模拟类型不接 AuthGrantRef/ExecutionPermit，不替代 D-01/D-18 或其他负责人的业务验证合同，不表示第 5 步完成。

2026-10-05 增加[ExecutionPermit 存储与事务内条件消费 v1](./12-execution-permit-storage-v1.md)：复用冻结 Permit/完整指纹及状态，绑定原会话和已确认底座快照，提供 PostgreSQL 条件消费、task-first 锁和提交前最新快照复查。正式签发仍未启用，封闭已核验快照没有生产构造入口；不会凭 READY 枚举或引用存在放行。业务投影、认证/裁决真源、资金事务、幂等/Outbox/原单查单均待对接，不表示合同 02/05 的完整资金流程已实现。

2026-10-05 新增[服务端确认记录持久化与核验 v1](./11-server-confirmation-storage-v1.md)：PostgreSQL 保存对底座操作快照的明确确认，绑定原会话、任务/操作和当前版本/摘要，提供幂等、核验及撤销。仅供可信后端装配，未接通 Runtime 资金路径；不替代业务投影、AuthGrant、Permit 或资金事务。完整业务指纹与确认采集等依赖仍待对接。

2026-10-04 新增 [Mock 草案生命周期授权 v2](./10-mock-draft-lifecycle-v2.md)：受限修改、取消分别确认，绑定业务期望版本及版本化会话；创建 v1 保持兼容。只操作单进程内存草案，不开放通用写或资金执行。

2026-10-04 底座第四步 B 增加可选的 [Runtime 只读等待点恢复 v1](./09-runtime-wait-recovery-v1.md)：真正执行只读查询并验证结果后保存等待点，新进程重新核验身份/版本/注册与 Policy 后仍等待用户。该入口不自动续跑、不重建许可，不扩展资金权限。

2026-10-04 底座第四步 A 新增 [PostgreSQL 恢复点持久化 v1](./08-recovery-storage-v1.md)，实现任务/操作/原始审计的保存与读取。持久化状态不是确认、认证或执行许可；这不是下述资金事务的实现，Runtime 尚不自动续跑。

2026-10-04 新增独立可选的 [Mock 内存草案授权 v1](./07-mock-draft-write-v1.md)，仅供单进程模拟草案创建；不替代下列生产许可与事务合同，不授权资金执行。原普通 Runtime 继续拒绝写工具。

- `SecurityOutcome`只描述执行前门禁，不表示业务执行结果。
- `OperationState`只描述单个业务副作用的持久化生命周期。
- `TaskState`只描述整个任务是否运行、等待或结束。
- `ExecutionPermit`只是服务端持久化许可的类型化引用；数据库中的有效记录才是授权真源。
- `ExecutionObservation::OutcomeUnknown`不是失败，必须进入对账流程。
- 写业务、消费Permit、更新Operation、关键审计和写Outbox必须遵守事务合同。

2026-10-21新增[账单反馈公共组合v1](./37-billing-feedback-public-composition-v1.md)：原完整来源归档与只读结算同事务、API受信工厂及默认关闭的显式HTTP准备/确认/执行/原号GET、同Kernel非资金确认/许可/成员反馈/公共结果/原始审计Outbox及原号恢复。本地候选待独立审查；正常共享站点未装配，锁卡、SQLx安全处置与正式业务采用仍单列。

## 完成定义

2026-10-10新增[Agent主流程HTTP v1](./27-agent-main-flow-http-v1.md)：真实Gateway受检计划、
实际只读历史结果、原计划继续、服务端草案确认与同任务自然语言修改/取消。
复用现有Runtime和已交付Tool/Policy/Store；生成交付待独立验收，真实供应商连接待配置验证。

2026-10-09新增[同任务持久模拟草案生命周期HTTP v2](./26-persisted-mock-draft-lifecycle-http-v2.md)：
依据限定技术评审实现新的显式模式，Create/Modify后持久等待、Cancel真实成功后终结，
逐动作永久领取、未知屏障与多版本历史。默认关闭，生成交付待独立验收；合同10成员业务不变，
旧22/24/25保持隔离。内存Store不跨进程恢复，未接普通客户修改/取消页面或自然语言改口。

第零步只有在以下条件全部满足后才能进入Runtime实现：

1. `safeflow-contracts`能够编译且全部测试通过。
2. 所有合同类型带版本或由所属合同版本覆盖。
3. 状态迁移、Permit绑定、指纹和超时恢复均有正反测试。
4. 安全、Runtime和转账负责人确认本目录合同。
5. 后续代码不能重新定义同名状态或使用单个通用`status`替代三维状态。

第一步的独立完成定义见[Runtime与安全公共合同](./06-runtime-security-public-contracts.md#第一步完成定义)。
