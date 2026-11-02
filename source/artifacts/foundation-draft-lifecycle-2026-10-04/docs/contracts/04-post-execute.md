# POST_EXECUTE 合同 v1

合同版本：`EXECUTION_OBSERVATION_CONTRACT_VERSION = 1`

执行适配器只能返回它能够证明的事实：

```rust
enum ExecutionObservation {
    NotSubmitted { attempt_id, reason_code },
    ResultReceived { attempt_id, business_operation_ref, tool_result_ref },
    OutcomeUnknown { attempt_id, business_operation_ref, reason_code },
}
```

## 处理规则

| Observation | Runtime动作 | 是否允许再次提交 |
|---|---|---:|
| `NOT_SUBMITTED` | 保持或重新建立`READY`所需门禁；必须复用原operation和幂等键 | 仅在确证未提交后允许 |
| `RESULT_RECEIVED` | 核验Tool结果与业务数据库，转为成功或失败 | 否 |
| `OUTCOME_UNKNOWN` | Operation转为`PROCESSING`，Task转为`WAITING_EXTERNAL`，启动查单 | 否 |

`OUTCOME_UNKNOWN`不是失败。网络超时、连接中断或响应丢失只要无法证明请求未发送，就必须使用该类型。

## 引用要求

- 每次尝试都必须有稳定`attempt_id`。
- `RESULT_RECEIVED`和`OUTCOME_UNKNOWN`必须有`business_operation_ref`。
- 只有`RESULT_RECEIVED`必须有`tool_result_ref`。
- `business_operation_ref`必须在对外提交前生成并持久化。

## 结果核验

SecurityKernel或结果核验组件读取可信业务状态后返回独立的`PostExecuteAssessment`：

- `VERIFIED_SUCCEEDED`
- `VERIFIED_FAILED`
- `PROCESSING`

这些是执行结果核验，不得放入`SecurityOutcome`。

