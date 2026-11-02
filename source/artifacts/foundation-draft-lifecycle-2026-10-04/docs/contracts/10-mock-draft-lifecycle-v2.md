# Mock 草案修改、取消授权合同 v2

日期：2026-10-04。当前只增加单进程内存草案的修改、取消。创建继续使用 [Mock 授权 v1](./07-mock-draft-write-v1.md)，原 `request()` 与已有创建参数不变。这里的显式确认仅确认草案操作，不是资金转账确认。

## 动作与合同

| 动作 | 工具名 | action | 授权合同版本 | Tool Schema | Policy 版本 |
|---|---|---|---|---|---|
| 创建 | `create_mock_transfer_draft` | `CREATE_MOCK_DRAFT` | 1 | 1 | `mock-draft-policy-v1` |
| 修改 | `modify_mock_transfer_draft` | `MODIFY_MOCK_DRAFT` | 2 | 1 | `mock-draft-lifecycle-policy-v2` |
| 取消 | `cancel_mock_transfer_draft` | `CANCEL_MOCK_DRAFT` | 2 | 1 | `mock-draft-lifecycle-policy-v2` |

`MockDraftAction` 是闭合白名单。Runtime 校验工具名、action、授权合同版本、Tool 合同和 Schema，三项存储动作之外全部拒绝。普通 `Runtime::execute()` 仍拒绝全部 `has_side_effect=true` 工具，Mock 入口也拒绝伪装为只读的工具。创建许可不能用于修改、取消或资金工具。`ToolExecutionAuthorization::MockDraftWriteV1` 的 Rust 调用分支名保持兼容；实际授权版本由请求及动作白名单校验，它不是资金 `Write` 权限。

三个 Tool 均声明写副作用，分别注册同名 DomainPolicy 和安全 Profile，并经过 `RuntimeBootstrap::build_mock_draft()` 的启动校验。仍仅接受 Green、`READY_TO_CONFIRM`、唯一 `ExplicitConfirmation` 门禁及无额外非信息性发现的决定。没有绕过确认、扩大资金权限或执行 PRE_COMMIT。

## 修改输入

```json
{
  "draft_id": "draft-进程号-编号",
  "expected_version": 1,
  "changes": {"amount": "300.00", "remark": ""}
}
```

| 字段 | 行为 |
|---|---|
| `draft_id` | 必填，非空，遵守现有 BankGateway ResourceId 校验：最多 128 个字符，无空白及控制字符；不另造 ID 字符集，必须属于会话用户与当前任务 |
| `expected_version` | 必填，正整数 `u32`，必须等于当前业务草案 `plan_version`；字符串、浮点、null、0、超范围拒绝 |
| `changes` | 必填非空对象；仅允许下述五项，缺省或 null、空对象均拒绝 |
| `payer_account_id` | 缺省保留，提供则必须是本人账户 ID；不能 null、空字符串或无效 ID |
| `payee_id` | 缺省保留，提供则须来自该用户/任务的可信查询候选，最终再次查询核对；同名保留多个候选，不接受姓名或 `payee_query` 代选 |
| `amount` | 缺省保留，提供则为正数 CNY 十进制元字符串，严格两位小数，不使用浮点、不四舍五入；因原核心用 `u64` 分，最大 `184467440737095516.15`，溢出拒绝 |
| `currency` | 缺省保留，仅字符串 `CNY` 合法 |
| `remark` | 缺省保留，`""` 明确清空；最多 140 个 Unicode 字符，不允许控制字符；null 拒绝 |

顶层及 changes 的未知字段全部拒绝；用户 ID、许可、已确认标记、到期时间和自报版本不能混入工具参数。每个变更字段显式 null 都拒绝，包括与有效字段同时提供时。提供与原值相同的合法字段也是一次显式改版，业务版本只递增一次；`changes:{}` 不产生无意义改版。

只有完整、未到期、未取消的 `READY_TO_CONFIRM` 草案可以通过这个入口修改。输入不全时应先等待用户补齐，不能授权一个缺参修改来覆盖旧完整草案。原独立业务接口仍保留它的 `NeedsInformation` 实验语义，不能当作本 Runtime 入口已经保存成功。

## 取消输入

```json
{"draft_id": "draft-进程号-编号", "expected_version": 2}
```

两项必填，ID/版本约束同上；未知字段包括 `changes` 均拒绝。取消不调用 BankGateway、不扣款、不撤银行订单。取消保留原业务版本、草案 ID、业务操作 ID、详情和有效期，只改变状态为 `CANCELLED`。原业务核心允许同版本重复取消返回原状态；新请求仍需新的独立确认和更高 Runtime 版本。过期草案的取消拒绝，旧业务版本的取消拒绝，取消终态不能再修改或复活。

`cancel_mock_transfer_draft` 取消的是未提交草案。`revoke()`、`revoke_task()` 撤销的是 Mock 执行资格，不会自动改变业务草案状态；资金提交后的查单和银行订单撤销均未实现。

## 确认、会话和版本

可信服务端使用以下真实入口，模型与 Policy 不能调用发证路径：

```rust
MockDraftAuthority::request_for_action(
    action: MockDraftAction,
    session_ref: SessionRef,
    task_id: TaskId,
    node_id: NodeId,
    plan_version: u32,
    arguments: serde_json::Value,
    ttl_seconds: u64,
) -> Result<MockDraftWriteRequest, RuntimeError>
```

用户明确确认展示的动作和参数后，可信装配调用 `record_explicit_confirmation(&request)` 与 `issue(&confirmation)`，随后 `Runtime::execute_mock_draft(request, &permit)`。不能让 LLM 自发确认或提交虚构确认引用。示例模拟可信服务端记录用户动作，未实现 HTTP/前端确认采集。

私有授权记录绑定会话用户、原会话引用和绑定版本、task/node、工具及其唯一 action、完整 JSON 参数（包含草案 ID 与期望业务版本）、Runtime 计划版本、请求 ID、合同版本和许可到期。复制对象身份之外的 JSON/字符串或 `unregistered()` 不构成权限；同一许可仅在 Runtime 持有执行预约时可被 Tool 使用一次。

v2 必须使用 `resolve_binding()`：服务端返回 `SessionBinding { user_id, binding_version }`，版本必须大于 0，在失效、续期或重新绑定时递增，即使仍是同一用户。默认未实现的 resolver 拒绝新增动作。签发、执行、保存前及结果注册/核验都重新读取绑定；失效后重绑不能恢复旧许可。静态 resolver 的共享映射不可变，因此固定版本 1。v1 创建保留原 `resolve()` 兼容语义，不能据此宣称它能识别同用户失效重绑；可变会话接入创建时仍须由服务端撤销旧任务授权。会话 resolver 应保证两种解析返回一致身份。

Runtime 的 `plan_version` 是整个 task 的计划版本，正数 `u32`。确认排他范围仍是 `(user, task)`，跨动作、跨节点共用一个当前确认版本；新请求必须更高，拒绝倒退和同版本不同请求/节点/参数。同一个未失效请求的合法重复登记/签发返回原对象，不重置消费状态。新版本确认原子撤销旧资格，已写业务不会回滚。

业务 `StoredDraft.plan_version` 是确认单版本：创建为 1，修改 +1，取消不增。示例旧业务版本拒绝消耗 Runtime 版本 3，随后取消用 Runtime 版本 4、业务版本 2。这两个版本不能相互替代。

## 保存顺序与幂等

修改复用 `TransferDrafts::modify_guarded()` 和 `features/transfer` 的原修改服务；最后一次异步查询得到合格账户/收款人后，取得 `lock_claim_for_save()`，在同一个授权锁内重新核验并完成同步保存。银行错误或最终选中收款人消失均在保存前拒绝，原字段和业务版本保留。修改不重算或延长业务 TTL。原核心还会在写前检查业务版本和到期。

取消复用 `TransferDrafts::cancel_guarded()` 和原取消服务，同样持授权锁保存，但不等银行查询。共享 Store 的草案互斥锁串行保护业务服务；若旧修改正在持草案锁等待银行，新取消确认会先撤销旧许可，取消执行本身等待草案锁。银行返回后旧修改在保存前失败，然后取消保存原版本。没有抢占式银行取消或查询超时机制。

撤销/新确认先取得授权锁，旧保存拒绝；保存先取得锁并完成，则已完成的业务变更不会被撤销回滚。随后持旧业务版本的取消失败，需用户看到最新草案再确认。会话快照核验不是数据库会话与业务事务；可变 resolver 的外部失效应配合服务端授权撤销，不能把此单进程方案当成生产原子认证。

同一请求的并发/重试只预约一次实际 Tool 执行；重复调用加入或重放相同结果。成功、银行失败、业务失败、回执失败和审计失败均缓存为终态。不因失败自动重试修改/取消；中断为 `EXECUTION_INTERRUPTED_RESULT_UNKNOWN`。新请求不能复用已消费许可。许可过期、撤销、会话绑定失效、计划被替代时，缓存结果访问也拒绝。

## 回执与失败

Tool 先读回实际业务保存值，再登记可信回执；Runtime 核验完整 ToolResult（名字、操作、状态、引用、正文），POST_EXECUTE Policy 按动作回执再读当前草案。修改应为 `READY_TO_CONFIRM` 且 `expected_version+1`，取消应为 `CANCELLED` 且版本不变。回执同时包括 `action`、`expected_version` 和 `runtime_plan_version`；其余字段沿用创建回执：草案/任务/业务操作 ID、业务 `plan_version`、RFC3339 `expires_at` 和五项业务字段。ToolResult 的 `operation_id` 是 Runtime 的 `operation_{task}_{node}`；正文的 `operation_id` 是原草案核心生成并保留的业务操作 ID。

历史成功正文只是当时快照，不表示当前仍可执行。取消后重新核验旧修改回执会拒绝；跨动作拿回执也拒绝。实际已保存但正文被篡改、POST_EXECUTE 拒绝或审计失败时，返回失败、保留已保存业务状态，同请求不再次变更。用可信服务端 `MockDraftStore::read()` 或 `read_created_for_task()` 核对，不凭失败输出猜测草案没变。

| 失败入口 | 可见结果 |
|---|---|
| 未登记、伪造、错参/错动作、撤销、过期、绑定变化 | `RuntimeError::MockDraftAuthorization`；入口记录 `WriteRejected` |
| 严格输入、业务版本、归属、终态、银行被 Policy 拒绝 | Runtime 门禁 `Blocked`；Policy finding 保留实际原因，如 `INVALID_DRAFT_INPUT`、`DRAFT_MODIFY_REJECTED`、`DRAFT_VERSION_CONFLICT` |
| 最后一次银行读取或保存钩子失败 | Tool 错误，如 `DRAFT_MODIFY_FAILED`；原版本不改写 |
| 回执/POST_EXECUTE/审计失败 | Runtime 返回失败；可能已经保存，必须读回核对，不可自动再写 |

审计继续覆盖 `ToolStarted`、安全评估、`ToolCompleted`、`TaskCompleted` 及 `WriteRejected`、`WriteFailed`、`WriteReplayed`，工具名区分动作，不记录会话/完整业务参数。审计不可用返回失败；若失败审计也不可用，不承诺持久日志已留存。

## 边界与入口

只支持单节点计划与当前进程的 Mock 内存草案，不支持真实资金、资金确认/认证、生产 Permit、HTTP、真实模型、草案数据库化、多实例、重启后许可/幂等恢复。300 秒业务 TTL 和 60 秒许可 TTL 是演示值。第四步 A/B 的持久恢复点不能重建这些许可或自动续跑新增写工具。

永久测试：`crates/transfer/tests/runtime_draft_lifecycle.rs`；完整注册/运行示例：`crates/transfer/examples/support/mock_draft_write.rs`、`crates/transfer/examples/runtime_draft_lifecycle.rs`。接入文件和实测结果见 [修改取消交接](../handoff/SafeFlow_草案修改取消Runtime安全入口_v0.1.md)。
