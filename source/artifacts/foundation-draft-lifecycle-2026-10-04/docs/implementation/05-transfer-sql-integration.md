# 转账业务 SQL 接入（2026-10-17）

采用逐风 `codex/transfer@1f0e46db7d08051140d42ac195af5f207f2d7062`，业务提交为 `e4b7ecdd6414c61b70f8739ee0f2afe161407e25`。`business_store.rs`、`funds_store.rs`、`mock_execution.rs`、两份业务 SQL 和 `fixtures/transfer/postgres_business_actual_v1.json` 保持收到时的字节内容；来源与 SHA256 见本轮证据目录的 `imports.json`。

## 公共迁移

业务 SQL 已作为公共迁移 `0015_transfer_business_orders_v1.sql`、`0016_transfer_funds_v1.sql` 加入 `PgRepository::migrate()` 原有迁移链。旧 0001..0014 不变。迁移副本保留成员提案原文及历史注释，其安装状态以公共 migrator 为准；`crates/transfer/sql/*.proposed.sql` 仅为来源，不另行执行。

新增六张模拟银行业务表：业务原单及首次/最新回执、持久草案、模拟资金账户、收款路由、资金单/占用、双边不可改写流水。它们不是正式用户身份或第二个正式账户主库。SQLx 仍为 0.7.4。

## 受检读取入口

`ApiState::transfer_sql_reader_for_backend()` 复用 API 当前的仓库与会话解析器，返回 `TransferSqlReader`。纯后端装配也可使用 `TransferSqlReader::for_trusted_backend(repository, sessions)`，其中 `sessions` 必须是调用系统正在使用的可信版本化会话源。

```rust
let reader = api_state.transfer_sql_reader_for_backend();
let draft = reader.draft(&session, &draft_id).await?;
let funds = reader.funds(&session, &payer_account_id).await?;
let original = reader.business(&session, &business_operation_id).await?;
let rows = reader.transactions(&session).await?;
let snapshot = reader
    .project_draft(&session, &runtime_operation_id, &draft_id, expected_business_version)
    .await?;
```

读取在只读、可重复读事务中进行，获取连接、设置事务、调用各业务读取及提交的前后均复核身份/会话版本。按当前本人范围查询；样例 `sql-owner-*` 不映射成 API `user_001`。数据库错误返回 `StorageError::Database`；原单不存在返回 `None`，不代表没有扣款，更不代表银行明确失败。

`project_draft` 从实际公共 Operation/Task 与持久草案读取参数，验证本人、任务、业务版本、状态和数据库当前时间下的期限。业务草案版本和 Runtime 计划版本分别保留；业务号不覆盖 Runtime 动作号。返回值只是快照，不是许可、未提交证明或保留到扣款后的锁。正式提交必须在受控事务中重新锁定并复核当前草案、来源、规则和授权。

## 执行边界

存储库已启用成员 `postgres-store`，成员方法可借用消费许可之后的 `PermitTransaction::connection()`。新增隔离测试通过真实业务 SQL 验证公共包装的整体回滚，未改变 `final_guard`。合同 28 的公共记录仍只允许 `SUBMITTED`，业务 `SUCCEEDED` 不直接变成公共资金终态。

没有注册资金 Tool/HTTP 入口，`request_issuance` 仍返回 `PermitIssuanceUnavailable`。正式账户/收款路由/当前草案初始化、资金指纹及真实许可来源、最终卡锁并发保护、已批准规则来源和领域审计/Outbox 尚未装配；原内存草案 Runtime 未被悄悄切换。测试中的可信初始化、静态身份、模拟出账状态和公共存储 fixture 都不能作为这些正式来源的替代。

## 验证材料

本轮证据位置：`E:/银行agent/artifacts/transfer-sql-integration-2026-10-17/DELIVERY.md`。`verify.ps1` 显式执行需 PostgreSQL 的测试并核对实际数量，不把 ignored 算通过。七组原始 JSON 由实际 SQL 重跑，回执/资金/流水按原始业务语义比较，仅处理动态完成时间、数据库流水号及重跑所需的草案有效期；原 JSON 不修改。新进程只读探针不初始化、不迁移、不写入，也不是正式登录入口。
