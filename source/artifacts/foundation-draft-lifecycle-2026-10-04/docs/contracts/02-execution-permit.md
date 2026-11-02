# ExecutionPermit 合同 v1

合同版本：`EXECUTION_PERMIT_CONTRACT_VERSION = 1`

## 定义

ExecutionPermit是有副作用Tool的一次性服务端许可。它不发送给LLM或前端，也不以客户端提交的内容作为授权证据。

Permit至少绑定：

```text
permit_id
operation_id
decision_id
user_id
tool_name
operation_fingerprint
facts_version
issued_at
expires_at
status
```

## 签发规则

- 只有`SecurityOutcome::READY_TO_EXECUTE`允许签发。
- `facts_version`必须大于0。
- 过期时间必须晚于签发时间。
- `permit_id`必须由服务端密码学安全随机源生成。
- 数据库必须限制同一`operation_id + facts_version`最多一个有效Permit。
- 新事实版本、会话撤销或关键参数变化必须撤销旧Permit。

## 使用规则

Tool提交前必须验证：

1. Permit在数据库中存在且状态为`ACTIVE`。
2. 当前时间早于`expires_at`。
3. 用户、Tool、Operation、指纹和`facts_version`全部一致。
4. Operation当前状态为`READY`。
5. Permit未被消费、撤销或替换。

Permit消费必须使用条件更新，例如：

```sql
UPDATE execution_permits
SET status = 'CONSUMED', consumed_at = now()
WHERE permit_id = $1 AND status = 'ACTIVE' AND expires_at > now();
```

受影响行数必须恰好为1，并与业务提交处于同一事务边界。内存对象的`consume()`只表达合同语义，不能替代数据库条件更新。

## 明确禁止

- Tool、LLM、客户端或普通功能代码自行签发Permit。
- 仅凭反序列化出的Permit结构执行写操作。
- Permit消费失败后继续业务写入。
- 为同一未知结果操作重新签发Permit并重新提交。

