# 原任务只读显式续跑合同 v1

本合同落实原 PRD BASE-R04、BASE-05 的用户回复后续跑公共子项，沿用 `TaskEvent::UserResponded`。版本为 `READ_ONLY_CONTINUATION_CONTRACT_VERSION = 1`，仅追加迁移 `0010_read_only_runtime_continuations.sql`。合同 19 的首次执行及原有合同 09 等待恢复入口保持原语义。

## 入口与语义

可信后端装配 `RuntimeTaskContinuationService`，调用 `continue_task(session, task_id, waiting_node, RuntimeTaskVersion)`。节点和 revision/plan_version/facts_version 都是精确比较值；身份、原计划、参数、当前状态、已完成节点和结果引用只从数据库证据取得。服务不接受回答正文、调用方完成集、目标状态、结果正文或 Permit。

现有 `PlanNode::UserDecision` 只有节点号和依赖，没有问题、选项、回答 Schema 或业务投影。因此“继续”只明确接受当前等待节点并执行原计划剩余只读节点；不解析自然语言答案、不修改参数或计划版本，不等同于金融操作确认或授权。后续需要回答改变业务参数时，由对应负责人提供可核对合同再接入。

全计划重新核验现有直接输入 Schema、Tool/Policy/Profile、当前会话与 facts；已完成和后续节点中的任何写工具都整体拒绝。当前 Policy 复查可以查询 BankGateway；Tool0 不等于 Policy0。执行器仍逐新 Tool 经过既有 PRE_TOOL/PRE_AUTH、身份和真实结果核验、POST_EXECUTE 及 Runtime 审计。

## 永久分段认领

任务锁先于认领锁。认领唯一键为 `(task_id, source_revision)`，同一等待版本只能领取一次。记录完整 source_task/claimed_task、被接受节点、前一收据摘要、本次会话摘要/绑定版本和注册摘要，同时将 WAITING_USER 经 UserResponded 变为 RUNNING、revision+1；checkpoint 和原 completed_nodes 暂时保留。任务更新、认领与审计一起提交。普通 TaskQuery 因而返回 RUNNING 的节点复核提示，不继续提示用户回复。

每次认领在事务内审计后和成功 COMMIT 后都复读整个来源链与当前任务；提交响应未知、失败或复读不一致不进入 Tool。无租约、超时重试或原号重新执行开关。公开服务不会接受调用方进度或封闭执行结果来落库。

Runtime 的公开 `ReadOnlyContinuationSource` 与 `SessionResolver` 一样属于可信后端适配接口，Rust 调用者可以实现；它不是封闭授权凭据，也不阻止恶意后端伪造进度。Storage 私有 Claim 只有在已提交证据复核后才交给该入口，用户入口必须经过 Storage 服务。Runtime 另核对计划/身份/facts、确定性调度的精确已完成前缀和下一个等待节点；跳过更早就绪节点、重复节点或错节点拒绝。

## 结算与收据链

种子集合为已核验的完成节点加本次接受的 UserDecision。执行器跳过这些节点，封闭 `VerifiedReadOnlyExecution` 包含累计完成集和仅本段产生的真实 ToolResult。Storage 保留此前引用，追加本段 `tool_name/operation_id/result_ref`，保存本段结果摘要，不重放或保存结果正文。

结算将 RUNNING 经 AwaitUser 或 Complete 变为下一个 WAITING_USER 或 COMPLETED，revision 再+1，任务、分段收据、认领状态和审计同事务保存，并在提交前后复读。收据保存完整最终 TaskRecord、前收据摘要、注册摘要、累计引用与本段结果摘要。每段只能新增本次明确接受的一个决策节点；仍可经历多次等待，原 task_id/plan_version/参数保持。

链校验从合同 19 的原始认领及收据开始，核对每段 source_task 等于上段最终任务、claimed_task 精确等于一次 UserResponded 更新、prior_receipt_hash、最终任务和同版本审计。读取最多 257 行，并拒绝超过原计划 UserDecision 数量。完成集必须符合执行器确定性前缀；每个已完成 Tool 对应原节点工具和既有确定 operation_id。`result_ref` 只遵守既有类型合同，不额外规定不同节点引用唯一或派生引用长度不超过 200。

明确 Runtime 错误只在原任务、会话和注册仍有效时结算 FAILED。此前引用保留，完成集只记录原完成节点和本次接受的决策，不把失去可靠证据的新结果补成成功。漂移、DB/审计故障或进程中断保留永久 pending claim，要求按原 task_id 核对。提交期间失败可能已落库，不能据此宣称回滚或重新执行。

## 恢复、重放与兼容

`recover(session, task_id, expected)` 从当前本人合法会话开始核验，允许同一 owner 重新登录；所有 Runtime/数据库等待前后复核本次绑定。恢复 Tool0，不调用已完成节点，不重建许可。pending 返回 `RequiresOriginalTaskReview`，不能区分仍执行与已中断；新进程只有对已结算等待点发出新的显式继续命令才可执行。

完全相同的已结算历史请求可以返回原分段收据，Tool0；必须保持本人、精确原节点/版本及完整当前证据链和注册合法。返回 `replayed=true`，`task` 是该历史分段的任务，`current_task_revision` 标明当前头；不能把历史等待状态当作当前状态。pending 重复请求拒绝，不执行。

已有合法合同 19 WAITING_USER 可直接作为第一段来源，无需回填或改写原表。合同 09 旧等待点缺少合同 19 认领/结果引用证据，返回 `RuntimeIntakeRequired`；不猜历史进度或构造来源。旧 schedule 仍只接受初次空进度任务，唯一原始认领不被可信适配 trait 绕过。续跑后的任务使用本合同 recover；合同 19 recover 不升级为续跑链读取器。

本视图含内部 TaskRecord，只供可信后端，不能直接作为 HTTP/前端 DTO。公开展示继续使用合同 15 的受限进度视图。后续[合同 21](./21-local-read-only-http-v1.md)提供本机项目模拟 HTTP/TaskResponse 和服务端会话，历史重放转换为当前公开视图，不改变本合同内部历史收据语义；真实 LLM、业务回答 Schema、资金/锁卡/账单/订阅和正式审计仍待对应负责人。会话撤销与 DB 没有跨系统原子性，最后同步检查到提交之间仍有既有窗口；提交后复读可拒绝过时响应，不能撤销已提交事实。

永久用例：`crates/storage/tests/runtime_continuation_cases/mod.rs`。可运行新进程示例：`crates/storage/examples/planning_task_continuation.rs`，复用已有模拟银行余额 Tool/Policy；底座隔离测试不代表成员业务或整个 BASE-05 验收完成。
