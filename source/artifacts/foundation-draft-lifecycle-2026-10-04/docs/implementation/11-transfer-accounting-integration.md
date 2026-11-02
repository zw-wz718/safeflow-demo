# 逐风只读账务核对接入（2026-10-18）

本轮采用逐风固定提交 `b45473fa8ac2aa8b831cbefc4fdb3afa79c7d36f` 的原字节文件。`FundsStore::check_accounting` 保持成员定义：它只借用底座已经打开的 `&mut PgConnection`，不会创建连接、控制事务、修改业务状态或返回收款账户余额；缺少原单返回 `None`，账本/业务投影/数据库故障传播为错误。

公共接入集中在 `crates/storage/src/transfer_result.rs::verified_business_record`。原键、完整请求、业务单和双边流水核验仍先执行；日累计批量扫描只复用这条无锁事实核验，避免逐笔历史记录取得账户锁。单笔提交结果和公共 `reconcile` 才调用 `verified_record`，按账户 ID 稳定排序保护目标账户直到 COMMIT。因此 `PermitTransaction::record_transfer_result`、`ExecutionRequestStore::reconcile_transfer` 和付款原单恢复使用同一账务核对路径；`ExecutionRequestStore::original_transfer` 只使用无锁核验。原号恢复使用 `REPEATABLE READ READ ONLY`，不执行 `FOR UPDATE/FOR SHARE` 或写入。

本轮没有注册 `FundsBusinessPolicy` 为完整资金 Policy。其 `verify_execution` 明确返回 `FUNDS_POST_EXECUTION_NOT_CONNECTED`，卡锁、正式资金许可、审计 ACK 和真实付款入口仍未启用；也没有把 `None` 解释成未提交、重试许可或失败。

固定成员测试覆盖成功/处理中/失败、双方账本一分钱漂移、其他合法占用、只读 borrowed transaction、数据库读取故障、缺原单和收款人信息不泄露。公共存储测试继续覆盖原键/完整请求/双边流水、提交前失败整体回滚和原号恢复。
