# 成员数据库接入说明与运行例（2026-10-18）

本轮是底座交接及本地候选，不是正式卡片/订阅写接入验收。公共机制无须重写；采用与等级、来源、业务DDL有关的建议仍须负责人确认。代码尚未发布，不能用旧共同提交 f747f05c5cb51b02505b0bcf4da64b473a4bb46a 代表本轮。

## 1. 实际核对来源

| 材料 | 固定版本/位置 | 当前结论 |
| --- | --- | --- |
| 卡片只读注册 | 67773884a0456f7faec0314af671131d26479c0a，features/card/api-adapter/src/lib.rs | build_card_read_only_registration 接受后端提供的 Profile 和实施版本，不自行批准安全等级 |
| 原操作异步读取/重放 | 345c70aa75e0d79fc43dbfc499a98655194be294，features/card/src/operation_records_async.rs、operation_replay_async.rs、INTEGRATION_QUESTIONS.md | 已有异步历史读取；未知重试键不新增；不是SQL仓储或写许可 |
| 卡片事务核对 | 同提交，docs/collaboration/deliverables/04_结夏/SafeFlow_CARD-02_锁卡持久化模型与事务核对_v0.1.md | D02/D21、锁/操作/原因和整体回滚要求沿用；不重选业务规则 |
| 订阅 | 70dc9eaaf4331a05eac16c90ccd26948689dbee7，backend/src/features/subscription | 已有独立虚构PostgreSQL保存、统一运行与恢复，不再描述为“没有数据库保存” |
| 公共底座 | crates/storage/src/permit.rs、daily_usage.rs、shared_demo.rs、execution_request.rs、transfer_result.rs；合同28；迁移0016 | SQLx 0.7.4；既有受控连接和同用户事务锁可复用；当前迁移为1..21 |

成员源码通过产品工作树的Git对象读取；订阅路径先由 git ls-tree 定位。没有改成员源码或JSON，也没有导入订阅隔离schema。

## 2. 连接、账户和迁移

底座 crates/storage/Cargo.toml 固定 sqlx = "=0.7.4"，PostgreSQL驱动。业务仓储接收同版本 `&mut sqlx::PgConnection`，不得自行建立PgPool、BEGIN/COMMIT/ROLLBACK、切换schema或进行网络调用。包装层成功消费许可后，`PermitTransaction::connection()` 才能借出连接；消费失败回滚，未消费时借连接返回 TransactionAborted。

`PermitStore::transaction` 和 `submission_transaction` 拥有事务。后者还处理本人原键/业务号冲突、首次公共提交和原号重放；callback必须消费许可并调用 `record_submission`，成功后由包装最终复核及COMMIT。回调返回错误、消费/最终复核失败或取消会回滚未提交内容。业务错误应传播；不能吞掉错误后返回Ok，也不能先改独立内存再声称数据库回滚了整个业务。提交响应丢失应查询原号，不能据错误推断“肯定未提交”。

现有账户来源是迁移0016的 `transfer_accounts_v1`：`account_id TEXT PRIMARY KEY`、`owner_id TEXT NOT NULL`、非负分值余额。`transfer_payee_routes_v1` 仅为owner/payee到收款account映射。`initialize_shared_demo` 在显式采用配置后调用既有provision方法，不重置已有余额。1288/0/50是已交项目模拟方案，不是正式账户来源。

卡片 `Account.user_id` 对应此模拟范围的 `owner_id` 需结夏明确采用；不存在已批准的公共卡ID映射。当前迁移没有卡片表、卡到账户的外键或锁revision真源。尾号不能当卡ID，不能从展示掩码推账户。结夏应提出业务DDL/索引/版本维护方案，底座审查后追加唯一公共迁移；不另造账户表、不改既有1..21校验和。

## 3. 锁序：现状与待采用协议

现有锁函数 `daily_usage::protect_owner` 执行：

```sql
SELECT pg_advisory_xact_lock(hashtextextended(current_schema() || $1,0));
-- $1为serde_json::json!(["public-owner-execution-v1", owner]).to_string()
```

键包含当前schema和原始可信owner；必须使用相同schema、JSON编码、哈希seed。事务级锁一直保持到COMMIT/ROLLBACK。不能把session ID、卡尾号或账户ID替代owner。此函数目前crate私有；业务在既有PermitTransaction内运行时已经持锁，无需暴露第二个获取入口。

| 拟复用入口 | 代码实际顺序及范围 |
| --- | --- |
| PermitStore::transaction / submission_transaction | open: BEGIN → READ COMMITTED → 固定search_path/txid → owner锁 → 会话重查 → task行锁；随后公共请求/许可校验、consume、借连接；资金范围consume再锁既有业务operation并读取累计事实；callback资源锁始终在这些步骤之后 |
| ExecutionRequestStore::reserve | BEGIN → owner锁 → task锁 → 公共原请求登记；仅元数据，不是业务提交或授权 |
| ExecutionRequestStore::daily_usage | BEGIN → READ COMMITTED → owner锁 → 稳定排序业务operation锁 → 无账户行锁的业务事实/公共首次提交核对 → COMMIT；不是持锁返回给外部写者 |
| original_transfer / reconcile_transfer | `original_transfer`: BEGIN → REPEATABLE READ READ ONLY → 查本人原键 → task快照读 → 无锁核验；`reconcile_transfer`: BEGIN → owner锁 → 查本人原键 → task锁 → 原业务operation锁 → 单笔业务/账户保护核验；reconcile可更新公共观察结果，不重新付款 |
| initialize_shared_demo | BEGIN → 将计划中owner去重排序后依次加owner锁 → 按account_id排序provision账户 → provision路由 → COMMIT；不是运行时归属变更接口 |
| by_key / by_business_operation / replay历史查询 | 查本人映射 → task-first一致读取；没有owner写保护，不能拿查询结果作为之后写操作的锁事实 |

卡片接入建议（待结夏采用，尚非团队批准）：锁卡写、资金提交边界的当前卡事实读取，都在同一受控事务内，先用相同owner锁，再按公共task/既有业务锁约定，最后以稳定顺序锁账户、卡/锁、revision事实。读取和修改同一事实必须遵守同一约定；任一相关变化包括改后改回都更新稳定版本。早先独立只读查询的“无锁”结果不能直接用于付款COMMIT。

事务锁仅保护遵守协议的参与者。独立PgPool写入、裸SQL修改、外部银行动作不自动受保护。现有转账resolve另用原业务operation锁，不能据此声称外部卡写也被覆盖。跨用户转账涉及收款账户的行锁不等于所有受影响owner均被锁；归属迁移、跨用户锁动作和多owner动态操作尚未设计/验证。初始化中多owner排序不是这些操作已经获准的证明。这些场景先拒绝启用，待双方提交具体锁集合/顺序和并发样例后核对。

日累计批量扫描不会逐笔取得历史付款人/收款账户行锁；否则后续单笔提交的账户升级锁可能按不同历史顺序形成死锁。单笔提交结果与显式 reconcile 才在原业务/账户路径中按账号 ID 稳定顺序取得保护，并保持至事务结束。对向多账户并发用例继续复用成员 `opposite_transfers_lock_accounts_in_the_same_order_and_conserve_funds`，公共侧的多任务累计回归仍要求在隔离 PostgreSQL 上显式运行。

## 4. 公共原键与卡片别名

合同28/迁移0014按owner+原键、owner+原业务号及原公共O唯一；原参数/版本/绑定不可变。相同原请求读取原首次提交，callback不再次执行、不消费新许可。同键异参返回 ExecutionKeyConflict；同业务号或O试图换键返回 ExecutionOperationConflict。business_version与plan_version不能混用。

卡片模拟允许给同操作增加重试键；345c70a异步历史查询只接受已保存的键，并不写入新别名。建议公共接入始终保留首次原键，未知别名拒绝，不把用户提交的新键偷偷转换成旧键，也不为“兼容”扩展公共唯一规则。业务已经保存的历史别名只用于受授权历史查询，不能据此新增公共执行映射。请结夏确认采用此适配；若确需新别名，先共同评审写入权限、冲突/不可变约束及回滚测试，再改合同，当前不实现。

## 5. 可运行受控事务例

本候选保留已验收的真实可运行例，而非新增空仓储：`crates/storage/src/integration_batch_tests.rs` 中 `setup`、`submit` 和 `batch_first_current_receipts_atomic_results_and_original_replay`。核心调用就是：

```rust
// setup仅在crate内部封闭测试中建立公共许可fixture。
// db/p/r/b均来自同文件既有setup，不是HTTP输入或生产许可。
let id = p.permit_id().clone();
let binding = p.binding().clone();
let r2 = r.clone();
let b = b.clone();
db.store.submission_transaction(&db.session, r, move |unit| {
    Box::pin(async move {
        unit.consume(&id, &binding).await?;
        FundsStore::submit(
            unit.connection()?, &binding.snapshot.user_id,
            &r2.idempotency_key, &b, behavior, MockOutgoingState::Allowed,
        ).await.map_err(crate::transfer_sql::business_error)?;
        unit.record_submission(&r2).await?;
        unit.record_transfer_result().await?;
        Ok(())
    })
}).await?;
```

这里实际复用逐风FundsStore，`MockOutgoingState::Allowed`仅来自原封闭隔离测试，不是卡锁来源。`record_transfer_result`只核验已接转账完整结果，不是供卡/订阅复用的通用终态方法。未来卡仓储应在借出的连接上完成实际锁/原因/回执/version写入；其公共结果及领域审计映射须另接，不能照搬转账终态方法。许可生产构造器仍封闭，`request_issuance`仍拒绝，不能为跑例子开放发行器。

在新候选根目录，先私下设置 `SAFEFLOW_TEST_DATABASE_URL` 为专用PostgreSQL16测试库，配置Rust工具链及缓存，然后运行（脚本不打印凭据，不自动建真实业务数据）：

```powershell
& ./scripts/verify-member-storage.ps1 -CargoProgram cargo -Toolchain 1.98.1
```

脚本精确运行7项既有真实PG用例：借连接/首次提交及不重复回调；callback失败的SQL/许可/公共记录整体回滚；资金/公共审计/Outbox/COMMIT故障整体回滚；相同业务号换键拒绝；同用户两任务两账户并发的提交/回滚可见性；显式共享账户初始化幂等；当前付款登记的原版本恢复。用例在随机隔离schema运行并清理，只证明公共接法和已有已接转账，不证明卡仓储/订阅已验收。数据库缺失或未实际运行一项即失败。

## 6. 订阅已有持久化和需要的接法

70dc9e 的 `postgres_ledger_mock.rs` 已有 `PostgresLedgerMock`：持有PgPool，initialize_local_schema创建隔离schema，save_fixture_mock自行begin/commit，read_owned_fixture_mock按owner/snapshot读取。`confirmation_mock/postgres_intents.rs`另保存意图；`confirmation_mock/runtime_recovery.rs` 的 `SubscriptionRuntimeMock` 统一保存journal/version，start_mock恢复、使未完成确认失效并保留未知提醒。其dedicated校验要求数据库名为 `safeflow_subscription_mock_test`，append通过自有pool做version条件UPDATE。这是实际本地恢复实现，不是公共事务保存。

成员manifest的可选 `postgres-mock-store` 固定SQLx0.8.6、Tokio1.53.1；底座SQLx0.7.4。两个版本的PgConnection/Transaction是不同Rust类型，不能强转，也不能另开pool后声称同事务。当前不导入这套独立mock SQL，不把journal重放的HistoricalGate当新授权或外部发送事实，也不擅自升级公共驱动。

请结夏沿已交业务逻辑提供适配模块：采用底座SQLx0.7.4借连接仓储，不拥有事务；可信本人/公共原O、原键/完整参数版本从公共绑定取得；原回执、实际台账/意图/提醒状态、版本变化和关键事件待发一起保存/回滚。必要业务DDL由结夏提出后底座纳入公共迁移。普通订阅许可不应依赖转账累计；现有COMMON/TRANSFER隔离已实现，但订阅正式Profile/公共结果映射仍需确认。真实通知提供者、原attempt_id查询和未知结果恢复单独待接，不能重发来补审计。

## 7. 给结夏的本轮集中回复（供转发，未发送）

已核对6777388/345c70a及订阅70dc9e；承认你已交本地保存和统一恢复，不要求重做旧模拟。新的本地候选包含当前付款准备/登记、累计、账单及公共迁移1..21；使用本包README和scripts启动/关闭，候选不是新共同Git提交。

请下一步集中交：卡片采用 `transfer_accounts_v1.account_id/owner_id` 的明确确认、卡ID映射与锁/操作/原因/revision业务DDL提案；确认原键保持/未知别名拒绝及上面的同owner事务锁协议；给SQLx0.7.4借 `&mut PgConnection` 的真实仓储固定提交/路径、运行方法，以及你自己模块原JSON（引用已有固定文件即可，不必重复发送已有样例）。订阅交已实现业务到借连接仓储的实际适配、DDL及原JSON，不再新增独立mock。跨用户或归属变更若需支持，单列影响owner集合及锁序，不默认承诺。

验收需要：同键/同号重放与冲突、同事件复用与多原因、并发唯一有效锁、完整版本变化、每个写点失败整体回滚、响应丢失按原号查询、重建服务读回、越权拒绝；订阅另核对journal/台账/意图/提醒的同事务事实及未知结果。收到后底座审查DDL并纳入唯一迁移，装配可信连接及公共结果/审计映射，再做对应公共集成验收。卡片安全写、最终付款许可、正式订阅写和审计外部接收仍未启用；验证码细则不在本轮实现。
