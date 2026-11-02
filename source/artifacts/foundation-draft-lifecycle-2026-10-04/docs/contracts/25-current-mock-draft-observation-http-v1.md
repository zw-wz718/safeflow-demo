# Current Mock Draft Observation HTTP v1 (Contract 25)

Date: 2026-10-09. Foundation-only extension under the generation handoff.
Scope: `MOCK_DRAFT_CURRENT_STATE_ONLY`; mode: `PROJECT_SIMULATION`; `contract_version=1`.
Contracts 10/11/14/18/22/24 and the existing Create routes are unchanged.
This observation is not a confirmation, a permission, fresh bank facts or a funds result.

## Read

`GET /api/sim/draft-executions/:task_id/operations/:operation_id/current-draft`

Uses the existing cookie, Host/Origin checks and `no-store` response policy.
Only GET is supported. Query parameters are rejected; no client draft ID, business
version, parameters, user, action, confirmation or verified flag is accepted.

The service first checks current session ownership and the actual original durable
claim using contract 24's complete historical validation. It then constructs the
existing business context from that checked task and operation and calls
`MockDraftStore::read_created_for_task`. The actual Store checks the same user and
task. Session binding is rechecked after each asynchronous read and before return.

The response has exactly these nine fields:

```json
{
  "contract_version": 1,
  "mode": "PROJECT_SIMULATION",
  "scope": "MOCK_DRAFT_CURRENT_STATE_ONLY",
  "task_id": "<original task>",
  "operation_id": "<original public operation>",
  "current_draft_verified": true,
  "draft": {
    "draft_id": "<actual stored draft>",
    "business_operation_id": "<actual separate business operation>",
    "draft_version": 1,
    "status": "READY_TO_CONFIRM",
    "expires_at": "<actual UTC RFC3339 expiry>"
  },
  "lifecycle_execution_enabled": false,
  "lifecycle_unavailable_reason": "DEPENDENCY_UNAVAILABLE"
}
```

`current_draft_verified=true` means that this response observed the actual
in-process Store, including its current lifecycle/version. It does not mean the
draft is ready or authorized: status is the existing `READY_TO_CONFIRM`,
`NEEDS_INFORMATION`, `EXPIRED` or `CANCELLED`. The existing business read lazily
marks expired drafts; no create/modify/cancel Tool, Policy or bank query is run.
The operation ID, business version and expiry are actual Store values, never
reconstructed from the historical receipt. A completed claim's draft and business
IDs must still match; a later business version may be higher than the historical
version. Runtime plan version is not a business version.

An unresolved original claim can also be observed if the real Store contains the
draft. That fact does not settle PostgreSQL execution or permit another call.
This read neither alters the database nor renews sources, allocates a claim,
changes the task, or retries a write. The snapshot can change after return and
cannot be cached as execution evidence.

## Failure And Compatibility

- No original claim or wrong owner/task/operation: existing `404 NOT_FOUND`.
- Current Store absent (including restart), unavailable or inconsistent: existing
  `503 DEPENDENCY_UNAVAILABLE`, no successful draft response or history fallback.
- Invalid session: `401 NOT_AUTHENTICATED`; malformed input: `400 VALIDATION_ERROR`.
- Corrupt persistent history: existing error from contract 24; it is not ignored.
- No draft assembly configured: existing `501 DEPENDENCY_UNAVAILABLE`.

After a missing Store response, query only the original historical operation for
review. Missing memory does not prove that an earlier write did not execute.
The contract-24 history route remains `current_draft_verified=false`, even when
this separate route observes a draft. No existing response fields are changed.

## Pending Lifecycle Dependency

The original Create task is completed and its one-per-task claim is permanent.
Contract 10 requires modify/cancel to remain in that same task with a higher
Runtime version and a new explicit confirmation. Contract 08 permits plan
replacement only before operation creation and forbids terminal changes. Existing
code also rejects plan/operation changes after a mock execution claim. There is
currently no confirmed transition/plan mechanism that reconciles these constraints.

Persistent lifecycle writes remain disabled. Foundation owner: 不高兴; business
semantics confirmation: 逐风; exact contract approver: 待确认. Required material:
an agreed task/plan/operation lifecycle retaining the original ownership, old
claims, original history and unknown-result handling, plus confirmation/source
invalidation and new-claim uniqueness rules. Resume only after a verifiable
confirmed contract/source is received or the user explicitly adjusts scope.
No cross-task ownership, completed-task reopening, claim deletion or guessed
business interface is implemented. This partial generation awaits independent review.
