# 公共原请求存储与提交事务组合 v1

## 2026-10-17 实现增补

下文保留10月12日交付边界。当前已采用逐风1f0e46d的真实业务SQL/草案来源；新增 `PermitTransaction::record_transfer_result`，依据绑定完整成员请求、实际原单及双边流水，同事务保存PROCESSING/SUCCEEDED/FAILED与公共审计/Outbox；原首次公共SUBMITTED快照不改。`ExecutionRequestStore::original_transfer/reconcile_transfer`按原键读取/恢复当前结果，不再提交。未使用该受检适配时，旧包装仍只允许原READY/SUBMITTED形态，任意callback直接写终态仍拒绝。

迁移0017为实际持久入口自动保存可靠公共事件待发记录；不将原始公共事件等同正式C-AUD业务DTO。外部映射/发送/ACK解释仍待确认。许可签发和资金Runtime入口未开放，内部适配不冻结新的公共Tool名称/Schema。当前来源、同连接顺序、测试接缝和精确待对接项见[集中接入说明](../implementation/07-integration-batch.md)。以下历史“无业务SQL/无Outbox/仅SUBMITTED”描述不作为当前缺源结论。

日期：2026-10-12。`PUBLIC_EXECUTION_STORAGE_VERSION = 1`。实现仅在 `crates/storage`，复用合同01/02/03/05/08/12；不修改冻结的 contracts/runtime 类型、状态机或许可签发条件。

业务需求来源：`zw-wz718/-SafeFlow` 的 `codex/transfer` 固定提交 `98edd3c4baaa72808f01f7594937b47fafe5e6b8`，`docs/handoff/转账接入核对表_2026-10-11.md` 第1.2、2.2、2.3、7节。采用其本人原键唯一、稳定业务号唯一、不可变原请求、首回执与当前结果分开及原号只读要求。该远端提交不含本次底座代码。

## 范围与真实来源

这是服务端公共元数据接口，不是资金 Tool、HTTP 合同或业务回执协议。`ExecutionRequest` 没有 Deserialize；身份解析使用可信 `SessionResolver::resolve_binding`，调用方不能用绑定里的 user_id 自报归属。各服务须装配同一仓库、同一会话真源。数据库及受审查后端 SQL 属信任边界，不防恶意 DBA/适配器。

| 输入 | 类型／来源 | 校验 |
|---|---|---|
| 原键 | `IdempotencyKey` | 永久按 owner + key 唯一 |
| 原业务号 | `BusinessOperationRef` | 可信业务 Store 的稳定号，按 owner + B 唯一；不由公共 O 拼接 |
| 业务版本 | `business_version: u64` | 正数据库版本；转账适配映射可信 draft_version，独立于 plan_version |
| 公共绑定 | `PermitBinding` | 含完整 ConfirmationBinding、原 O、节点／工具、task/operation revision、plan/facts版本、参数摘要和完整指纹 |
| 原参数快照 | 冻结的 `FingerprintPayloadV1` | user/O/tool/plan 与绑定一致；resource_id 是可信业务引用；schema_version 非零；完整参数摘要／指纹重算；浮点拒绝 |

首次登记仅接受本人非生命周期模式的 RUNNING 任务、READY 操作和当前完整公共绑定。原 O 必须实际属于该任务；登记只保存元数据，不签发许可、不消费许可、不推进 READY。payload 序列化上限32768字节，JSONB存储上限65536字节；ID沿用公共存储200字节上限。不是金额或备注限额。业务字段规范化与资源归属由可信业务适配负责，备注仍按已采纳的 REMARK-140-v1 原样参与参数绑定，本层不截断或静默改写。

## 已实现接口

源文件：`crates/storage/src/execution_request.rs`，从 storage 根导出。

```rust
ExecutionRequestStore::for_trusted_backend(repository, sessions)
store.reserve(session, &request) -> Result<ExecutionRequestView, StorageError>
store.by_key(session, &key) -> Result<ExecutionRequestView, StorageError>
store.by_business_operation(session, &original_b) -> Result<ExecutionRequestView, StorageError>
store.replay(session, &original_request) -> Result<Option<PublicSubmissionReceipt>, StorageError>
```

`ExecutionRequestView` 含不可变 request、数据库 reserved_at、可选 first_submission 和现有 operations 表的 current_operation。没有资金执行回调。查询从当前有效会话解析本人，先定位本人的不可变映射，再按 task-first 锁读取一致快照；数据库等待及提交返回前后复核原会话绑定。同用户的新有效会话可以查历史，不要求原执行会话或有效新许可。别人和不存在均 NotFound。不会查银行、补写日志、续期、改业务状态或重新提交。

`replay` 比较完整原请求。`Some` 返回原首次公共提交记录；`None` 明确表示已登记但尚无公共提交记录，不是成功、失败或未扣款证明。找不到是 NotFound，不是 None。历史查询和相同登记重放可读取旧版本；首次提交仍须核验当前版本和原确认／许可。

`PublicSubmissionReceipt` 只包含首次 `SUBMITTED` 的完整 OperationRecord 和数据库 recorded_at。它不是 `MockTransferReceipt`，不定义业务 `PROCESSING/SUCCEEDED/FAILED`、reason_code、completed_at 或账务来源。公共 current_operation 即使为 SUCCEEDED，也必须由已核验业务适配器解释；本接口不因枚举存在证明银行成功。业务首回执和最新回执需在业务持久结果适配中分别保存／读回，不能把这份公共提交快照冒充完整银行回执。

## 同一事务组合

新增方法位于 `crates/storage/src/permit.rs`，不替换原 `transaction`：

```rust
PermitStore::submission_transaction(session, &original_request, callback)
    -> Result<PublicSubmissionReceipt, StorageError>
PermitTransaction::record_submission(&original_request)
    -> Result<PublicSubmissionReceipt, StorageError>
```

顺序：

1. 公共包装层 BEGIN、固定 schema/事务ID、按当前本人锁 task；核验原 O 属于本人任务。
2. 在锁内比较原键／原业务号／原 O。已有首次提交则读取原 first_submission、复核当前读取权限、COMMIT 返回；**不调用 callback、不读取或消费新许可**。同键异参或同号换键在 callback 前拒绝。
3. 无首次提交时，在同一事务登记公共元数据并写 `EXECUTION_REQUEST_RESERVED` 原始公共审计；复查当前 RUNNING/READY 和完整绑定。这一元数据登记可先独立 reserve，也可只在本事务产生，不是业务受理或资金授权。
4. callback 必须先 `consume(permit_id, expected_binding)`；成功后才可借 `connection()`。业务适配在这条连接锁资源、复查业务草案／账户／限额并组合业务持久写入。不得调用独立内存 submit 再补库，不得使用独立池、提交事务或在此进行网络调用。
5. callback 调用 `record_submission(&request)`：再次核验实际许可绑定及完整 payload、当前公共绑定／原确认；条件更新 READY -> SUBMITTED，operation_revision恰好+1；RUNNING -> WAITING_EXTERNAL，task_revision恰好+1，恢复点为原 O 的 ReconcileOperation；一次保存不可改写的首条公共提交快照，写既有 OPERATION_TRANSITIONED 原始审计。
6. 业务适配必须在**同一连接、同一事务**完成合同05要求的关键领域Audit及可靠Outbox。包装层不自动生成正式C-AUD或资金Outbox，也不把原始公共事件当它们的替代。
7. callback 返回后必须有与原请求完全相同的提交记录；包装层重新读取原请求、首记录及公共 Operation，继续原许可／确认／时间／完整计划／事实／已完成节点／schema／事务ID最终复核。包装层独占COMMIT，并在返回前再检查当前会话。

任何 helper、消费、callback、审计或最终复核错误／取消均回滚未提交变化；record_submission失败会毒化事务，吞掉其错误也无法提交。提交响应丢失或提交后会话失效的错误不能解释为“肯定没提交”，应按原键／业务号读取持久事实。本包装层的重放不触碰业务结果，查询故障保持 StorageError，不将银行改判FAILED。

已有 final_snapshot 两个形态保留：未推进的READY（旧公共隔离验证），或严格的SUBMITTED形态。新增方法只组合后者，并最终比较首条记录与当前 Operation；**不允许 callback 写任意PROCESSING/SUCCEEDED/FAILED终态**。同库模拟资金需要在一次事务保存终态的适配仍未启用；需要已验证持久业务结果与领域Audit/Outbox绑定后由底座接入，不能简单放宽守卫。公共最新状态读回可复用已有可信后端 operation_event 观察，但它不是正式资金结果入口。

## 存储与错误

仅追加迁移 `0014_public_execution_requests.sql`。已有1..13校验和不变；仓库 migrator 显式追加14。新表 public_execution_requests 的三组唯一约束分别是 owner+key、owner+B、公共 O。保存类型化公共绑定、完整 FingerprintPayloadV1、域分隔 request_digest、业务版本、DB受理时间和一次 first_operation/submitted_at。触发器禁止改原绑定、删除原映射、覆盖首记录；当前状态始终复用 operations，不造第二套状态机或结果表。

原始公共审计新增 `ExecutionRequestReserved`；新二进制能读旧记录，旧二进制无法解码该新枚举会拒绝读取。启用登记前须升级所有相关公共存储读者，不能在旧读者上默默忽略事件；不改变正式C-AUD版本或假造领域事件。

| 错误 | 实际含义 |
|---|---|
| `ExecutionKeyConflict` | 同本人原键的任一原绑定／参数／业务号或版本不同 |
| `ExecutionOperationConflict` | 同本人业务号或原公共O试图换键／映射 |
| `NotFound` | 本人不可访问／原键、业务号、任务或公共O不存在 |
| `Session` | 当前会话失效、换用户或请求等待期间重绑 |
| `PermitBindingMismatch`等原许可错误 | 新登记／提交版本或完整绑定失效；不会新发许可 |
| `CorruptRecord` | 持久摘要、首记录、当前原O关联不一致 |
| `Database`／`Migration` | 数据库等待、写入、提交或迁移错误；不能推断银行结果 |
| `TransactionAborted` | 未消费却借用提交方法、helper失败、缺少本请求提交记录 |
| `Invalid` | 非法ID、零／溢出数据库版本或过大payload；拒绝登记，不裁剪 |

## 未启用与恢复条件

正式 `request_issuance` 仍返回 PermitIssuanceUnavailable，VerifiedPermitSnapshot 仍没有生产构造器。无新增资金 Tool/HTTP 注册、账户／余额／账务表、业务草案 Store、正式审计接收或资金Outbox。生产目前只能使用公共元数据登记／读取；提交组合的运行验证只使用原许可合同的 crate 内封闭 fixture，不代表转账适配完成。

待对接：逐风提供已交业务要求对应的可信持久草案／结果及同连接业务适配；底座接入其生产投影／幂等事实、已批准最终规则、正式本笔认证与许可签发；风星龙与底座确认关键领域事件和Outbox写入／恢复合同。本轮不再索要已交第2.3节，也不要求提供真实供应商Key。未接妥前资金路径不可启用；收款方读权限、未知结果受控更新及审计补录入口仍待正式公共合同。

## 可运行验证

```powershell
$env:CARGO_HOME = 'E:/DevTools/Rust/.cargo'
$env:RUSTUP_HOME = 'E:/DevTools/Rust/.rustup'
$env:CARGO_TARGET_DIR = 'E:/银行agent/artifacts/persistence-session-review-target'
$env:RUSTFLAGS = '-Dwarnings'
. 'E:/DevTools/PostgreSQL-16.15/safeflow-review.ps1' -Action Env
Set-Location 'E:/银行agent/artifacts/foundation-draft-lifecycle-2026-10-04'
& 'E:/DevTools/Rust/.cargo/bin/cargo.exe' +1.98.1 test -p safeflow-storage --lib permit::tests::public_execution:: --locked --offline -- --ignored --test-threads=1
```

实际调用例子见 `crates/storage/src/public_execution_tests.rs` 的 reserve_round_trip、overlapping_original_submissions 和 first_public_submission 用例。它们使用真实随机隔离PG schema、现有公共合同fixture；没有资金Tool／假业务插件。PG不可达或配置缺失直接失败。重建仓库／服务的验证是连接与服务重建，不宣称实际重启PG服务或整进程崩溃测试。完整生成自测、修改前副本和校验和在根 `artifacts/public-execution-storage-2026-10-12/`，独立验收另行进行。
