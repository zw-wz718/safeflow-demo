# 事务边界合同 v1

## 本地模拟转账

第一版本地模拟转账必须在同一PostgreSQL事务中完成：

```text
BEGIN
锁定Operation与相关账户
断言OperationState = READY
验证并条件消费ExecutionPermit
插入或读取Idempotency记录
复查余额、限额、账户和草案版本
写不可变借方/贷方账务记录
更新Operation状态和业务结果引用
写关键AuditEvent
写OutboxEvent
COMMIT
```

任意步骤失败必须全部回滚。余额不得只依赖事务外的SecurityKernel快照。

## 外部银行调用

外部网络调用不得放在数据库长事务中：

```text
事务A：消费Permit + Operation转SUBMITTED + 写幂等与Outbox + 关键审计
COMMIT
Outbox Worker使用相同operation_id和幂等键提交
取得明确结果或OutcomeUnknown
事务B：核验结果 + 更新Operation + 写审计
```

结果未知时只允许查单或接收回调，禁止创建新operation重新提交。

## 必须同事务的数据

| 数据 | 本地模拟业务事务 | 外部提交事务A | 结果事务B |
|---|---:|---:|---:|
| Permit消费 | 是 | 是 | 否 |
| 幂等记录 | 是 | 是 | 读取 |
| Operation提交状态 | 是 | 是 | 更新结果 |
| 本地账务变化 | 是 | 不适用 | 按适配器模型处理 |
| 关键AuditEvent | 是 | 是 | 是 |
| OutboxEvent | 是 | 是 | 可选 |
| 哈希链/WORM/外部存证 | 否，异步 | 否，异步 | 否，异步 |

## 并发约束

- 相同幂等键和相同指纹返回同一Operation。
- 相同幂等键但不同指纹必须拒绝。
- Permit条件消费受影响行数不为1时必须回滚。
- 并发余额更新使用行锁、序列化策略或等价的乐观并发控制。
- 审计哈希链可以异步追加，但原始关键AuditEvent必须与业务状态同步持久化。

## 故障注入验收

测试必须在Permit消费、幂等写入、余额复查、账务写入、Operation更新、Audit写入和提交响应丢失处分别注入故障，并断言不存在部分账务变化或第二次提交。

