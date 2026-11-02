# Persistent Mock Draft Execution HTTP v1 (Contract 24)

Scope: `MOCK_DRAFT_EXECUTION_ONLY`; mode: `PROJECT_SIMULATION`; response `contract_version=1`.
Only existing CreateMockDraftTool is connected. Confirmation HTTP contract 22 is unchanged and confirmation never automatically invokes a write. No funds or production ExecutionPermit.

## Commands

`POST /api/sim/draft-confirmations/:task_id/execute` uses the existing cookie, CSRF and body perimeter. Strict body, all fields mandatory:

```json
{"scope":"MOCK_DRAFT_EXECUTION_ONLY","operation_id":"<original public operation>","confirmation_ref":"<server confirmation>","task_revision":1,"operation_revision":1,"plan_version":1,"facts_version":1}
```

IDs and versions are comparisons only; the backend reads actual persisted task, operation, both decisions and confirmed action. Task/operation revisions are positive safe integers; plan/facts versions are positive u32. No user, permit, TTL, business ID, verified boolean, parameters or decision can be supplied.

`GET /api/sim/draft-executions/:task_id/operations/:operation_id` reads the same original claim. No claim is `404 NOT_FOUND`. GET never invokes Tool/Policy execution or creates/renews a permit. Repeated POST with the original comparisons reads the old claim; it cannot retry execution.

## Response

Common fields: `contract_version`, `mode`, `scope`, `task_id`, `task_state`, `operation_id`, `operation_state`, `task_revision`, `operation_revision`, `plan_version`, `facts_version`, `execution_status`, `receipt_scope`, `current_draft_verified`, `receipt`, `replayed`, `next_action`.

| Status | Task/operation | Receipt / next action |
|---|---|---|
| `COMPLETED` | `COMPLETED` / `SUCCEEDED` | actual receipt object; `{ "kind": "NONE" }` |
| `REQUIRES_ORIGINAL_OPERATION_REVIEW` | `RUNNING` / `DRAFT` | null; `{ "kind": "QUERY_ORIGINAL_OPERATION" }` |

An unresolved claim is not permission to submit a DRAFT again. It may have created an in-memory business draft before interruption; only original GET/review is allowed. No invented business number is generated to force SUBMITTED/PROCESSING.

Concurrent commands can observe the original committed claim before its result settles. They return that original operation's unresolved view or its completed receipt, never execute again. Querying the original operation after settlement returns the same historical receipt.

Receipt fields: `draft_id`, actual separate `business_operation_id`, positive u32 `draft_version`, actual `tool_result_ref`, UTC RFC3339 `executed_at`. Public operation remains the confirmation's original reserved operation. `receipt_scope=HISTORICAL_EXECUTION_RECEIPT` and `current_draft_verified=false` always: the historical execution completed then, but the business draft is memory-only and may be absent after process restart. The receipt is not a current transferable draft, funds success or C-AUD.

`replayed=false` is the first settled POST response; repeated POST and GET use true. Query requires current authorized ownership, not re-approval of an old expired confirmation. Backend execution expiry is the earliest existing confirmation/decision deadline, never a new TTL. Task-first and actual source row locks cover only the local Mock tool; the synchronous save guard also checks versioned session and deadline.

## Errors And Recovery

Existing wire reasons are preserved: `VALIDATION_ERROR`, `NOT_AUTHENTICATED`, `FORBIDDEN`, `DEPENDENCY_UNAVAILABLE`, `NOT_FOUND`, `IDEMPOTENCY_CONFLICT`, `STALE_PLAN`. The original 8-second frontend command timeout is compatible. Timeout, connection loss or uncertain persistence must be followed only by original GET, never automatic POST retry or a new task.

The claim is committed before Tool use. The in-memory draft and PostgreSQL settlement are not one atomic transaction. After claim, any uncertainty leaves the original claim; loss of the settlement COMMIT response can still mean that a historical completed receipt exists. Ordinary task query reports actual persisted states; these states alone do not confer execution capability.
