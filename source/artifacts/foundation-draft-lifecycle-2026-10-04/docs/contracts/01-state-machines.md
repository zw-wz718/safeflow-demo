# 三维状态合同 v1

合同版本：`STATE_MACHINE_CONTRACT_VERSION = 1`

## 为什么分成三个维度

同一时刻可以同时满足：任务等待外部结果、转账结果未知、安全门禁此前只允许提交一次。三者若共用一个`status`，Runtime恢复时无法判断应查单还是重新执行。

## TaskState

| 当前状态 | 事件 | 下一状态 |
|---|---|---|
| `RUNNING` | `AWAIT_USER` | `WAITING_USER` |
| `WAITING_USER` | `USER_RESPONDED` | `RUNNING` |
| `RUNNING` | `AWAIT_EXTERNAL` | `WAITING_EXTERNAL` |
| `WAITING_EXTERNAL` | `EXTERNAL_RESOLVED` | `RUNNING` |
| `RUNNING` | `COMPLETE` | `COMPLETED` |
| 任意非终态 | `FAIL` | `FAILED` |
| 任意非终态 | `CANCEL` | `CANCELLED` |

`COMPLETED`、`FAILED`和`CANCELLED`是终态，不接受后续迁移。

## OperationState

| 当前状态 | 事件 | 下一状态 | 约束 |
|---|---|---|---|
| `DRAFT` | `GATES_SATISFIED` | `READY` | 已有有效确认、认证和Decision |
| `READY` | `SUBMISSION_RECORDED` | `SUBMITTED` | 同事务消费Permit并记录提交 |
| `SUBMITTED` | `OUTCOME_UNKNOWN` | `PROCESSING` | 禁止重新提交 |
| `SUBMITTED` | `EXECUTION_SUCCEEDED` | `SUCCEEDED` | 真实结果已核验 |
| `SUBMITTED` | `EXECUTION_FAILED` | `FAILED` | 真实失败已核验 |
| `PROCESSING` | `STATUS_CONFIRMED_SUCCEEDED` | `SUCCEEDED` | 仅由查单或回调触发 |
| `PROCESSING` | `STATUS_CONFIRMED_FAILED` | `FAILED` | 仅由查单或回调触发 |
| `DRAFT`或`READY` | `CANCEL` | `CANCELLED` | 尚未提交 |

只有`READY`允许首次提交。`SUBMITTED`、`PROCESSING`和所有终态都不允许再次提交。

## SecurityOutcome

`SecurityOutcome`是某个事实版本的门禁裁决，不是业务状态机：

- `ALLOW`：允许本次只读调用。
- `NEEDS_INPUT`：等待字段或对象选择。
- `READY_TO_CONFIRM`：等待用户确认准确操作。
- `WAITING_AUTH`：等待强验证。
- `BLOCKED`：拒绝继续。
- `READY_TO_EXECUTE`：允许签发一次写操作Permit。

`PROCESSING`、`SUCCEEDED`和`FAILED`不属于`SecurityOutcome`。

## 并发与恢复

- Task和Operation持久化时必须带数据库版本号或等价的并发控制。
- 重放同一事件不能创建第二次副作用。
- Runtime恢复时读取Task、Operation和最新Decision三个独立字段。
- 非法迁移必须返回错误并记录审计，不能猜测下一状态。

