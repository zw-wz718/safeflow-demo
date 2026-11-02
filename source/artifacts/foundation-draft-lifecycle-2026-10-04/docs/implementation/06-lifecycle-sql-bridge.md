# 生命周期草案结果与公共结算同事务保存

日期：2026-10-17。沿用合同 05/17/18/26/27/28 及逐风 `1f0e46db7d08051140d42ac195af5f207f2d7062` 的 `FundsStore::save_draft`；本轮没有修改成员实现、样例或表结构。

## 已启用的实际入口

可信启动配置调用 `DraftConfirmationConfig::with_lifecycle_v2()` 后，`http::draft_lifecycle::service()` 自动启用 SQL 保存桥接，不需要第二个开关，不会静默降级到只存内存。

- `/api/sim/draft-lifecycles/:task_id/operations/:operation_id/execute` 的创建、修改、取消，在 Runtime 执行并验回执后保存完整业务草案。
- Agent 的 `/api/agent/samples` 固定计划、受检规划及 `/api/agent/tasks/:task_id/prepare` 复用同一生命周期服务，确认和执行继续走原有的生命周期接口。固定计划模式无需 Key，本轮实际验证三步动作；未向真实 LLM 发请求。
- `ApiState::transfer_sql_reader_for_backend()` 可以读到这些实际动作保存的草案，不再只限于手工预置数据。

已有模拟 HTTP 演示仍可使用：先按项目现有启动入口启用生命周期配置、运行公共迁移并设置私有登录环境，再执行：

```powershell
& 'E:/银行agent/artifacts/foundation-draft-lifecycle-2026-10-04/crates/api/examples/lifecycle-http.ps1' -Url 'http://127.0.0.1:8787' -ConfirmDemonstrationActions
```

该例子实际创建、修改、取消草案。它不执行资金，不自动补身份、账户或收款路由。SQL 读取仅有现有后端库入口，本轮没有新增 SQL 查询 HTTP 路由。

## 保存顺序与失败含义

1. 仍先持久提交原有 claim。只有明确提交成功的原调用可以进入业务 Tool。
2. 修改/取消在确认和执行前比较完整内存草案与 SQL 当前记录，沿用成员 advisory lock 的键及锁顺序，再锁业务草案行。缺 SQL、已提交、版本或完整内容不符均拒绝；不会从历史内存状态补库。
3. 仍由实际 Runtime 调用已有内存 Tool/Policy 并验回执。随后通过 `LifecycleDraftObserver::stored(context, draft_id)` 读取业务服务当前完整 `StoredDraft`。API 实现直接调用已有 `MockDraftStore::read`；不从客户端 JSON 或显示用 ToolResult 重建草案。
4. 检查可信 owner/task、草案号、业务操作号/版本、公共 Runtime 动作号/计划版本、状态、参数和期限。业务版本与 Runtime 计划版本不互相替代；期限同时核对数据库当前时间。
5. `FundsStore::save_draft` 借用公共 settle 的同一连接保存完整草案，并读回核对。公共成功回执、Task/Operation 状态和已有关键审计证据随后一起提交；任何错误统一回滚这一笔数据库事务。提交前仍复核会话及公共来源。
6. 原号重放只读取已有 claim/历史回执，既不重调 Tool，也不重存 SQL 草案。保存失败、提交失败或进程中断后，已提交 claim 保持可识别；未确定结算时返回错误或 `RESULT_UNKNOWN`，阻止后续动作和自动重执。

**内存业务动作不在 PostgreSQL 事务内。** SQL/公共保存失败时，内存中的创建、修改或取消可能已经发生；它不会被数据库回滚。公开成功记录与 SQL 均不半提交，既有 claim 屏障会阻止再次执行。原有 `current-draft` 仍读取实际内存，用于明确范围内核对，不是 SQL 已持久成功的证明；是否完成公共结算须看原回执和 SQL。

## 后端装配和兼容范围

```rust
let lifecycle = LifecycleService::for_trusted_backend(
    repository, runtime, sessions, actual_store_observer,
    contracts, decision_config, confirmation_config,
)?
.with_transfer_sql_bridge_for_backend();
```

桥接要求 observer 提供完整真实业务读取。旧 observer 仅实现 `observe` 时，`stored` 默认明确返回 `DependencyUnavailable`，开启桥接后不回退。旧库调用方只调用原构造器则保留原先的“公共结算持久、业务仅内存”范围，不能宣称 SQL 草案已保存。

本轮没有接入旧 `/api/sim/draft-confirmations/:task_id/execute` 的独立执行实现，也没有将直接 `Runtime`/`MockDraftStore` 调用自动纳入 SQL；这些仍是原有内存业务范围。历史成功回执重放不补库，已有内存草案没有 SQL 记录时不允许通过本桥接继续修改/取消。

## 重启边界与逐风对接项

2026-10-17 后续更新：逐风 e72084a 已提供恢复 API，底座已装配完整、未提交、未过期的 ReadyToConfirm 草案恢复。下面的缺口描述保留为本批桥接的历史范围；当前入口、搜索词来源及限制以 [08-draft-restart-adapter.md](08-draft-restart-adapter.md) 为准。

重建 ApiState 或新进程后，SQL 完整草案和公共原回执仍可查询；重放不会重调工具。但现有 `TransferDrafts`/`MockDraftStore` 没有恢复导入 API，内部任务选择、候选查询及草案引用不会因读 SQL 自动恢复。重启后继续修改/取消明确不可用，不能默认新建或用空内存覆盖 SQL。

精确归入原 `crates/transfer/INTEGRATION_QUESTIONS.md`：

- **Q3，底座已做：**实际生命周期执行结果到 `save_draft` 和公共结算的原子保存、API/Agent 装配、同号防重、受检读取与测试。本轮不再列“缺业务草案 SQL”。
- **Q3，逐风待提供、底座待装配：**可调用的已有草案恢复或继续编辑接口，说明可信 owner/task 与完整 StoredDraft、稳定业务号/版本/期限，以及任务选择、草案引用和候选查询上下文如何受检恢复。当前 `prepare_modify` 依赖内存 `selections[task_id].query`，单把 StoredDraft 读回不足以继续编辑。无需重发已有 SQL 或七组 JSON，也不由底座代写成员内部恢复逻辑。接口及上下文依据可核对后再接重启编辑。
- **Q2/Q5/Q6：**这条草案保存桥接不授予资金许可，不调用 submit/resolve，不造账户、收款路由或卡锁事实，不实现正式领域审计/Outbox。最终资金入口仍受原缺口约束；公共生命周期关键记录不替代成员领域审计。

本轮自测与完整证据位于 `E:/银行agent/artifacts/lifecycle-sql-bridge-2026-10-17/DELIVERY.md`，最终验收由检查侧独立进行。
