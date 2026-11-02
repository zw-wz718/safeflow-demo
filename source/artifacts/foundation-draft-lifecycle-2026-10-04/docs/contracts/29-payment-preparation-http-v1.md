# 本地付款准备与独立确认 v1

2026-10-18登记增量：响应增加 `execution_link_path` 与可为空的 `execution_link`。确认后可显式POST该路径保存内部关联，按准备版本恢复；登记不占用正式公共执行原号位置，见[合同32登记补充](./32-payment-execution-recovery-v1.md)。所有付款GET均使用只读事务，失效状态只是当前读取投影，GET不持久撤销或写决策/审计。

2026-10-18原单恢复增量：准备/查询响应提供 `original_result_path`，实际 `GET .../:operation_id/original-result` 按原任务及付款准备操作定位本人稳定业务原号，核验真实SQL原请求/回执/双边流水；新会话和过期确认可读历史。查询无资金/公共状态写入，单侧来源明确待核对，详见[合同32](./32-payment-execution-recovery-v1.md)。

2026-10-18增量：首次付款明确确认已接公共最低等级评估、现有决策存储及历史查询，新增显式 `/evaluate`。规则为“已有累计加尚未提交本笔”，详见[合同31](./31-payment-minimum-decision-binding-v1.md)。下文“付款意愿”及资金执行边界仍适用；旧版本仅保存意愿的描述不代表本次评估仍未接入。

本轮底座内部合同，scope=`PAYMENT_INTENT_ONLY`。它组织用户意愿，不是已冻结的跨团队资金 Tool/Profile，不注册 `execute_transfer`，也不签发许可。已有模拟会话、Host/Origin/CSRF/请求体限制继续适用。启用条件是现有草案生命周期 v2 装配，TTL 复用启动配置，不另设产品默认期限。

## 来源与保存

服务端按会话取得 owner，锁 owner → task → 原业务号 → SQL 草案。核验公共历史及真实成功草案回执，从成员 FundsStore 读当前持久草案，再调用已交 `MockTransferRequest::from_trusted_draft`。请求只比较草案号、业务版本、公共版本和选择票据，不能传金额、owner、收款路由或确认状态。

迁移19的 `payment_preparations` 保存完整成员投影、原样显示详情/备注、期限、快照摘要、显示票据和内部准备计划。使用已有 `operations`、`server_confirmations`、`audit_events`/Outbox；四者同事务提交，失败全部回滚。快照不可修改或删除，失效原因只能设置一次。

付款操作属于同一 task，处于公共 `DRAFT`，与草案动作操作号、ConfirmationRef 分开。准备计划使用公共 `AgentPlan` / `PlanNode` 结构，版本钉在当时草案执行计划版本，另存基础计划摘要和任务修订号。这是付款意愿分支快照，不替换 `tasks.plan`、不增加草案计划版本、不写草案执行历史、不参与 Runtime 节点调度。现有 Runtime/任务进度投影只取执行操作；付款操作经专用查询入口返回。旧生命周期模式与最终许可入口的隔离不变。

`snapshot_digest` 和公共 `arguments_hash` 都散列完整 `PaymentSnapshot`；确认 binding 另绑定 owner、task/operation/node、基础任务修订/计划/事实版本及准备计划摘要。此摘要不是跨团队 `OperationFingerprint v1`，不能用作资金许可。

## HTTP

- `POST /api/sim/payment-preparations/:task_id`：准备；输入如下。selection_ticket 等来自当前草案生命周期查询；draft_version 是真实业务版本，不能拿公共计划版本替代。
- `GET /api/sim/payment-preparations/:task_id`：只凭原 task_id 恢复该任务的付款意愿数组。
- `GET /api/sim/payment-preparations/:task_id/:operation_id`：查询原付款意愿。
- `POST /api/sim/payment-preparations/:task_id/:operation_id/confirm`：明确 `CONFIRM` 或 `REJECT`，必须回传这份付款显示的票据及确认号。

```json
{
  "contract_version": 1,
  "scope": "PAYMENT_INTENT_ONLY",
  "source": {
    "selection_ticket": "从当前草案响应取得",
    "task_revision": 2,
    "plan_version": 1,
    "facts_version": 7,
    "draft_id": "从当前草案成功回执取得",
    "draft_version": 1
  }
}
```

```json
{
  "contract_version": 1,
  "scope": "PAYMENT_INTENT_ONLY",
  "confirmation_ref": "从付款准备响应取得",
  "display_ticket": "从同一付款准备响应取得",
  "action": "CONFIRM"
}
```

成功响应含完整 `snapshot.transfer`（账户、payee、金额、币种、原样备注、草案及版本、稳定原业务号）、`snapshot.display_details`、公共操作、摘要、显示票据、确认号及真实服务端时间。不会返回会话摘要、AuthGrant 或 Permit。

状态：`AWAITING_PAYMENT_CONFIRMATION` → `CONFIRMED_AWAITING_EXECUTION_CONDITIONS` 或 `REJECTED`；过期/来源变化/会话不匹配为 `INVALIDATED`。确认成功的公共操作仍为 `DRAFT`，`funds_execution_enabled=false`。

## 重放、失效、恢复

同 task/选择票据重复准备严格比较原请求并返回原操作/确认；同号变参409。数据库唯一约束加 task 锁阻止第二个并发意图。重复确认时间不变、不追加第二个确认事件。确认后再拒绝返回冲突；拒绝、失效记录永不复活。

确认命令在锁内重读实际草案和业务原号，比较完整快照及公共版本，并在写入后、提交前复核会话与有效期。查询在一致性只读事务中进行相同比较，不取得执行权。真实修改/取消、过期、会话撤销或新绑定都不能继续使用旧确认。GET只投影失效状态；准备重放、确认等显式命令才会持久保存失效及公共撤销事件。存储里的历史 `confirmed_at` 不表示当前仍可执行。

重启不恢复进程内模拟会话。重新登录同一 owner 可按原 task_id 读回原快照、操作和历史确认时间；原会话确认变为 `SESSION_CHANGED`，不能沿用。相同请求仍返回原失效记录；需要新意图时必须有新的实际来源版本，不能通过换会话、换请求号复活旧内容。

已提交标记、已存在成员原业务记录或正式公共执行原号记录都阻止新准备。内部登记存于独立表，不属于这些资金来源；登记后仍可沿现有流程修改/取消，修改后的新版需要重新准备、确认，再保存自己的登记历史。数据库错误不视为“无原单”。原号查询沿已有 `TransferSqlReader::business` 和原任务查询，不能换号重付。

## 下一段装配

本轮可复用的是持久付款内容与独立用户意愿。进入最终资金许可仍需已确认资金 Tool/Profile、最终规则及必要认证装配、卡锁在提交事务内的核验协议。不得把本快照/确认当作这些证据，也不填入未经确认的本笔累计、软评分或卡锁结论。

实际调用脚本：`crates/api/examples/payment-preparation-http.ps1`。它只准备/确认用户已有的本人 SQL 草案，不创建付款资金结果。
