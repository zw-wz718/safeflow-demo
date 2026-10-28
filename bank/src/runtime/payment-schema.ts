import { z } from 'zod';

const id = z.string().regex(/^[A-Za-z0-9_.-]{1,200}$/);
const hash = z.string().regex(/^[a-f0-9]{64}$/);
const time = z.string().datetime({ offset: true });
const uint = z.number().int().nonnegative();
const cents = uint.max(Number.MAX_SAFE_INTEGER);
const rev = uint.min(1).max(Number.MAX_SAFE_INTEGER);
const version = rev.max(4294967295);
const money = z.string().regex(/^(0|[1-9][0-9]*)\.[0-9]{2}$/);
const status = z.enum(['SUCCEEDED', 'FAILED', 'PROCESSING']);
const locator = z.object({ draft_id: id, draft_version: version }).strict();
const transfer = z
  .object({
    task_id: id,
    operation_id: id,
    ...locator.shape,
    payer_account_id: id,
    payee_id: id,
    amount: money,
    currency: z.literal('CNY'),
    remark: z.string(),
  })
  .strict();
const details = z
  .object({
    account: z
      .object({
        account_id: id,
        display_name: z.string().min(1),
        masked_account_no: z.string().min(1),
        currency: z.literal('CNY'),
      })
      .strict(),
    payee: z
      .object({
        payee_id: id,
        display_name: z.string().min(1),
        bank_name: z.string().min(1),
        masked_account_no: z.string().min(1),
      })
      .strict(),
    amount: money,
    currency: z.literal('CNY'),
    remark: z.string(),
    available_balance: money,
    balance_as_of: time,
  })
  .strict();
const operation = z
  .object({
    operation_id: id,
    task_id: id,
    node_id: id,
    state: z.enum([
      'DRAFT',
      'READY',
      'SUBMITTED',
      'PROCESSING',
      'SUCCEEDED',
      'FAILED',
      'CANCELLED',
    ]),
    revision: rev,
    plan_version: version,
    facts_version: rev,
    arguments_hash: hash,
    business_operation_ref: id.nullable(),
  })
  .strict();
const binding = z
  .object({
    user_id: id,
    task_id: id,
    operation_id: id,
    node_id: id,
    tool_name: id,
    task_revision: rev,
    plan_version: version,
    operation_revision: rev,
    facts_version: rev,
    arguments_hash: hash,
    lifecycle_anchor: hash.optional(),
  })
  .strict();
const registration = z
  .object({
    owner_id: id,
    task_id: id,
    registration_operation_id: id,
    node_id: id,
    preparation_operation_id: id,
    task_revision: rev,
    plan_version: version,
    facts_version: rev,
    transfer_arguments_hash: hash,
    preparation_binding_digest: hash,
  })
  .strict();
const link = z
  .object({
    scope: z.literal('PAYMENT_REGISTRATION_ONLY'),
    preparation_operation_id: id,
    confirmation_ref: id,
    preparation_binding: binding,
    registration_binding: registration,
    normalized_arguments: transfer,
    registration_key: id,
    business_operation_id: id,
    registration_operation: operation,
    registered_at: time,
    state: z.literal('REGISTERED_UNSUBMITTED'),
    funds_execution_enabled: z.literal(false),
    confirmation_migration_authorized: z.literal(false),
  })
  .strict();
const receipt = z
  .object({
    proposal_version: z.literal(1),
    task_id: id,
    operation_id: id,
    ...locator.shape,
    payer_account_id: id,
    payee_id: id,
    amount: money,
    currency: z.literal('CNY'),
    status,
    reason_code: id.nullable(),
    completed_at: time.nullable(),
  })
  .strict();
export const originalResultSchema = z
  .object({
    contract_version: z.literal(1),
    scope: z.literal('PAYMENT_ORIGINAL_RESULT_ONLY'),
    task_id: id,
    binding: z
      .object({
        preparation_operation_id: id,
        preparation_plan_version: version,
        preparation_arguments_hash: hash,
        business_operation_id: id,
        business_draft_version: version,
        normalized_arguments: transfer,
        transfer_arguments_hash: hash,
        execution_operation_id: id.nullable(),
        execution_plan_version: version.nullable(),
        execution_tool_name: id.nullable(),
        idempotency_key: id.nullable(),
        operation_fingerprint: hash.nullable(),
      })
      .strict(),
    state: z.enum([
      'UNSUBMITTED',
      'REGISTERED_UNSUBMITTED',
      'PROCESSING',
      'SUCCEEDED',
      'FAILED',
      'RECONCILE_REQUIRED',
      'SOURCE_CONFLICT',
    ]),
    reason: id,
    first_public_submission: operation.nullable(),
    public_submitted_at: time.nullable(),
    verified_business_result: z
      .object({ request: transfer, first_receipt: receipt, latest_receipt: receipt })
      .strict()
      .nullable(),
    funds_execution_enabled: z.literal(false),
    public_sync_enabled: z.literal(false),
    execution_link: link.nullable(),
  })
  .strict()
  .superRefine((o, ctx) => {
    const b = o.binding,
      t = b.normalized_arguments,
      r = o.verified_business_result;
    if (
      o.task_id !== t.task_id ||
      b.business_operation_id !== t.operation_id ||
      b.business_draft_version !== t.draft_version ||
      (r &&
        (Object.entries(t).some(([k, v]) => r.request[k as keyof typeof t] !== v) ||
          [r.first_receipt, r.latest_receipt].some((receipt) =>
            Object.entries(receipt).some(([k, v]) => k in t && t[k as keyof typeof t] !== v),
          ))) ||
      (['SUCCEEDED', 'FAILED', 'PROCESSING'].includes(o.state) &&
        (!o.first_public_submission || !r || r.latest_receipt.status !== o.state))
    )
      ctx.addIssue({ code: 'custom', message: 'Inconsistent original business result' });
  });
const finding = z
  .object({
    check_id: id,
    policy_version: id,
    source: id,
    kind: z.enum(['HARD_DENY', 'NEEDS_INPUT', 'REQUIRE_GATE', 'RISK_SIGNAL', 'INFORMATIONAL']),
    reason_code: id,
    evidence_refs: z.array(id),
    observed_value: z.null(),
    event_ids: z.array(id),
    dedup_key: z.string().nullable(),
    window_start_ms: uint.nullable(),
    window_end_ms: uint.nullable(),
  })
  .strict();
const minimumFacts = z
  .object({
    usage_rule_version: id,
    statistical_day: z.string(),
    used_cents: cents,
    current_cents: cents,
    combined_cents: cents,
    usage_digest: hash,
    payment_digest: hash,
    business_operation_id: id,
    tool_parameters: locator,
  })
  .strict();
const storedDecision = z
  .object({
    contract_version: z.literal(1),
    stored_decision_id: id,
    request_id: id,
    evidence_kind: z.literal('PREFLIGHT_ONLY'),
    source_mode: z.literal('TRUSTED_PAYMENT_MINIMUM_V1'),
    source_instance_id: id,
    binding,
    phase: z.literal('PRE_AUTH'),
    snapshot_digest: hash,
    assembly_digest: hash,
    assembly_metadata: z
      .object({
        confirmation_binding_digest: hash,
        confirmation_ref: id,
        facts: minimumFacts,
        kernel: z
          .object({
            domain_policy_evaluated: z.literal(false),
            funds_execution_enabled: z.literal(false),
            kernel_source_digest: hash,
            minimum_source_digest: hash,
            risk_mode: z.literal('NOT_EVALUATED'),
            scope: z.literal('PUBLIC_PAYMENT_MINIMUM_ONLY'),
            threshold_cents: z.literal(100000),
            version: id,
          })
          .strict(),
        settings: z.object({ deployment_revision_digest: hash, ttl_ms: uint }).strict(),
        storage_source_digest: hash,
      })
      .strict(),
    decision: z
      .object({
        contract_version: z.literal(1),
        decision_id: id,
        operation_id: id,
        facts_version: version,
        policy_version: id,
        outcome: z.enum(['READY_TO_CONFIRM', 'WAITING_AUTH']),
        minimum_level: z.enum(['YELLOW', 'RED']),
        risk_score: uint.max(100),
        risk_tier: z.enum(['LOW', 'MEDIUM', 'HIGH', 'CRITICAL']),
        required_gates: z.array(z.enum(['EXPLICIT_CONFIRMATION', 'STRONG_AUTHENTICATION'])),
        findings: z.array(
          z.object({ finding, applied_weight: uint.nullable(), score_delta: uint }).strict(),
        ),
        expires_at_ms: uint,
      })
      .strict(),
    decision_digest: hash,
    plan_review_digest: hash,
    evaluated_at_ms: uint,
    expires_at_ms: uint,
    status: z.enum(['ACTIVE', 'REVOKED', 'INVALIDATED']),
    ended_at_ms: uint.nullable(),
    invalidation_reason: z.enum(['SNAPSHOT_CHANGED', 'ASSEMBLY_CHANGED', 'EXPIRED']).nullable(),
  })
  .strict();
const daily = z.discriminatedUnion('complete', [
  z.object({ complete: z.literal(false), missing_reason: id }).strict(),
  z
    .object({
      complete: z.literal(true),
      scope: z.literal('OWNER_ALL_ACCOUNTS_PUBLIC_FIRST_SUBMISSION'),
      owner_id: id,
      payer_accounts: z.array(id),
      submitted: z.array(
        z
          .object({
            owner_id: id,
            operation_id: id,
            payer_account_id: id,
            amount: money,
            submitted_at: time.nullable(),
            status,
          })
          .strict(),
      ),
      usage: z
        .object({
          rule_version: id,
          statistical_day: z.string(),
          successful_cents: uint,
          pending_today_cents: uint,
          carried_pending_cents: uint,
          used_cents: uint,
        })
        .strict(),
    })
    .strict(),
]);
const observation = z
  .object({
    observation_id: id,
    ordinal: rev,
    scope: z.literal('PAYMENT_BUSINESS_OBSERVATION_ONLY'),
    preparation_binding: binding,
    snapshot_digest: hash,
    session_digest: hash,
    session_version: rev,
    context: z
      .object({
        contract_version: z.literal(1),
        task_id: id,
        node_id: id,
        operation_id: id,
        facts_version: version,
        phase: z.literal('PRE_AUTH'),
        tool_name: z.literal('foundation_payment_business_preflight'),
        plan_version: version,
        authenticated_user_id: id,
        normalized_arguments: transfer,
        arguments_hash: hash,
        idempotency_key: id,
        confirmation_ref: id,
        auth_grant_ref: z.null(),
      })
      .strict(),
    facts: z
      .object({
        member_source: id,
        policy_source: id,
        isolation: z.literal('REPEATABLE_READ'),
        owner_id: id,
        request: transfer,
        comparison_key: id,
        key_scope: z.enum(['INTERNAL_COMPARISON_ONLY', 'OBSERVED_ORIGINAL_KEY_QUERY_ONLY']),
        formal_execution_binding_adopted: z.literal(false),
        draft: z
          .object({
            draft_id: id,
            operation_id: id,
            task_id: id,
            owner_id: id,
            version,
            status: z.enum(['ReadyToConfirm', 'Cancelled', 'Expired', 'NeedsInformation']),
            expires_at: uint,
            expired: z.boolean(),
            details: details.nullable(),
          })
          .strict()
          .nullable(),
        submitted: z.boolean(),
        payer_owner_id: id.nullable(),
        funds: z.object({ available: money, booked: money, reserved: money }).strict().nullable(),
        recipient_account_id: id.nullable(),
        original_request: transfer.nullable(),
        original_key: id.nullable(),
        key_operation_id: id.nullable(),
        daily,
        outgoing: z
          .object({
            state: z.literal('UNKNOWN'),
            missing_reason: z.literal('CARD_TRANSACTION_REPOSITORY_NOT_CONNECTED'),
          })
          .strict(),
      })
      .strict(),
    source_digest: hash,
    assembly_digest: hash,
    findings: z.array(finding),
    observed_at: time,
    expires_at: time,
    funds_execution_enabled: z.literal(false),
  })
  .strict();
const agentBinding = z
  .object({
    scope: z.literal('PAYMENT_AGENT_INTENT_ONLY'),
    projection_revision: z.literal('payment-draft-locator-v1'),
    reviewed_plan: z
      .object({
        contract_version: z.literal(1),
        task_id: id,
        plan_version: version,
        nodes: z.tuple([
          z
            .object({
              type: z.literal('TOOL'),
              id,
              tool: z.literal('execute_transfer'),
              arguments: locator,
              depends_on: z.tuple([]),
            })
            .strict(),
        ]),
      })
      .strict(),
    reviewed_plan_digest: hash,
    registration_digest: hash,
    planning_source: z.enum(['MOCK_LLM', 'OPENAI_COMPATIBLE', 'UNSPECIFIED']),
    locator,
    preparation_operation_id: id,
    confirmation_ref: id,
    snapshot_digest: hash,
    lifecycle_plan_digest: hash,
  })
  .strict();
export const fundsResultSchema = z
  .object({
    contract_version: z.literal(1),
    scope: z.literal('PAYMENT_FUNDS_RESULT_ONLY'),
    task_id: id,
    preparation_operation_id: id,
    execution_operation_id: id.nullable().optional(),
    snapshot_digest: hash,
    transfer,
    state: z.enum([
      'UNSUBMITTED',
      'REGISTERED_UNSUBMITTED',
      'PROCESSING',
      'SUCCEEDED',
      'FAILED',
      'RECONCILE_REQUIRED',
      'SOURCE_CONFLICT',
    ]),
    submitted: z.boolean(),
    automatic_retry: z.literal(false),
  })
  .strict()
  .superRefine((r, ctx) => {
    if (
      r.task_id !== r.transfer.task_id ||
      (['PROCESSING', 'SUCCEEDED', 'FAILED'].includes(r.state) && !r.submitted) ||
      (['UNSUBMITTED', 'REGISTERED_UNSUBMITTED'].includes(r.state) && r.submitted)
    )
      ctx.addIssue({ code: 'custom', message: 'Inconsistent funds result' });
  });
export type PaymentFunds = z.infer<typeof fundsResultSchema>;
export const fundsReferenceSchema = z
  .object({
    task_id: id,
    operation: z.object({ operation_id: id }).strict(),
    snapshot_digest: hash,
    snapshot: z.object({ transfer }).strict(),
  })
  .strict();
export type FundsReference = z.infer<typeof fundsReferenceSchema>;
export function fundsReference(p: FundsReference): FundsReference {
  return {
    task_id: p.task_id,
    operation: { operation_id: p.operation.operation_id },
    snapshot_digest: p.snapshot_digest,
    snapshot: { transfer: p.snapshot.transfer },
  };
}
export const paymentSchema = z
  .object({
    contract_version: z.literal(1),
    scope: z.literal('PAYMENT_INTENT_ONLY'),
    agent_binding: agentBinding.nullable(),
    task_id: id,
    operation,
    snapshot: z.object({ transfer, display_details: details, draft_expires_at: uint }).strict(),
    snapshot_digest: hash,
    display_ticket: id,
    confirmation_ref: id,
    confirmation_status: z.enum(['PREPARED', 'CONFIRMED', 'REVOKED']),
    prepared_at: time,
    expires_at: time,
    confirmed_at: time.nullable(),
    state: z.enum([
      'AWAITING_PAYMENT_CONFIRMATION',
      'CONFIRMED_AWAITING_EXECUTION_CONDITIONS',
      'REJECTED',
      'INVALIDATED',
    ]),
    invalid_reason: id.nullable(),
    funds_execution_enabled: z.boolean().default(false),
    execution: z
      .object({
        enabled: z.boolean(),
        reason: z.enum(['RECHECKED_AT_EXPLICIT_EXECUTION', 'PAYMENT_DEPENDENCY_NOT_ADOPTED']),
        execute_path: z.string(),
        result_path: z.string(),
        automatic_retry: z.literal(false),
      })
      .strict()
      .optional(),
    execution_conditions: z
      .array(
        z.enum([
          'FINAL_FUNDS_AUTHORITY_NOT_ASSEMBLED',
          'FINAL_RULES_AND_AUTH_NOT_ASSEMBLED',
          'CARD_COMMIT_PROTOCOL_NOT_CONNECTED',
        ]),
      )
      .max(3),
    replayed: z.boolean(),
    security_decisions: z.array(storedDecision),
    decision_freshness: z.enum([
      'NOT_EVALUATED',
      'CURRENT_MINIMUM_ONLY',
      'HISTORICAL_REEVALUATION_REQUIRED',
      'FACTS_UNAVAILABLE',
      'QUERY_ORIGINAL_BUSINESS_OPERATION',
    ]),
    business_preflight: z
      .object({
        scope: z.literal('PAYMENT_BUSINESS_OBSERVATION_ONLY'),
        freshness: z.enum([
          'NOT_EVALUATED',
          'CURRENT_OBSERVATION_ONLY',
          'HISTORICAL_REEVALUATION_REQUIRED',
          'PREPARATION_INVALIDATED',
        ]),
        observations: z.array(observation),
        formal_execution_binding_adopted: z.literal(false),
        missing_sources: z.tuple([z.literal('CARD_TRANSACTION_REPOSITORY_NOT_CONNECTED')]),
        not_evaluated_checks: z.tuple([
          z.literal('BALANCE_SUFFICIENCY'),
          z.literal('DAILY_USAGE_BUSINESS_POLICY'),
        ]),
      })
      .strict(),
    business_recovery_required: z.boolean(),
    original_result_path: z.string(),
    execution_link: link.nullable(),
    execution_link_path: z.string(),
  })
  .strict()
  .superRefine((p, ctx) => {
    const t = p.snapshot.transfer,
      d = p.snapshot.display_details,
      b = p.agent_binding;
    const expectedState =
      p.invalid_reason !== null
        ? p.invalid_reason === 'REJECTED'
          ? 'REJECTED'
          : 'INVALIDATED'
        : p.confirmation_status === 'CONFIRMED'
          ? 'CONFIRMED_AWAITING_EXECUTION_CONDITIONS'
          : 'AWAITING_PAYMENT_CONFIRMATION';
    if (
      p.task_id !== p.operation.task_id ||
      p.task_id !== t.task_id ||
      p.state !== expectedState ||
      (!p.invalid_reason && p.confirmation_status === 'REVOKED') ||
      p.operation.arguments_hash !== p.snapshot_digest ||
      t.payer_account_id !== d.account.account_id ||
      t.payee_id !== d.payee.payee_id ||
      t.amount !== d.amount ||
      t.currency !== d.currency ||
      t.remark !== d.remark ||
      p.original_result_path !== `${paymentBase(p)}/original-result` ||
      p.execution_link_path !== `${paymentBase(p)}/execution-link` ||
      (p.funds_execution_enabled &&
        (!p.execution?.enabled || p.execution_conditions.length !== 0)) ||
      (p.execution &&
        (p.execution.enabled !== p.funds_execution_enabled ||
          p.execution.execute_path !== `${paymentBase(p)}/execute` ||
          p.execution.result_path !== `${paymentBase(p)}/funds-result` ||
          p.execution.reason !==
            (p.execution.enabled
              ? 'RECHECKED_AT_EXPLICIT_EXECUTION'
              : 'PAYMENT_DEPENDENCY_NOT_ADOPTED'))) ||
      (b &&
        (b.preparation_operation_id !== p.operation.operation_id ||
          b.confirmation_ref !== p.confirmation_ref ||
          b.snapshot_digest !== p.snapshot_digest ||
          b.locator.draft_id !== t.draft_id ||
          b.locator.draft_version !== t.draft_version ||
          b.reviewed_plan.task_id !== p.task_id ||
          b.reviewed_plan.nodes[0].arguments.draft_id !== t.draft_id ||
          b.reviewed_plan.nodes[0].arguments.draft_version !== t.draft_version))
    )
      ctx.addIssue({ code: 'custom', message: 'Inconsistent payment binding' });
  });
export const paymentFlowSchema = z
  .object({
    status: z.enum([
      'WAITING_PAYMENT_CONFIRMATION',
      'WAITING_EXECUTION_CONDITIONS',
      'WAITING_EXPLICIT_PAYMENT',
      'ENDED_INVALIDATED',
      'QUERY_ORIGINAL',
    ]),
    planning_enabled: z.literal(true),
    funds_execution_enabled: z.boolean().default(false),
    automatic_retry: z.literal(false),
    next_action: z.enum([
      'CONFIRM_OR_REJECT',
      'QUERY_OR_EVALUATE',
      'REVIEW_CURRENT_DRAFT',
      'QUERY_ORIGINAL',
      'EXECUTE_PAYMENT',
    ]),
    next_path: z.string(),
    original: originalResultSchema.nullable(),
    funds_result: fundsResultSchema.optional(),
    waiting_node_id: id,
  })
  .strict();
export type Payment = z.infer<typeof paymentSchema>;
export type PaymentOriginal = z.infer<typeof originalResultSchema>;
export function fundsMatches(p: FundsReference, r: PaymentFunds) {
  return (
    r.task_id === p.task_id &&
    r.preparation_operation_id === p.operation.operation_id &&
    r.snapshot_digest === p.snapshot_digest &&
    Object.entries(p.snapshot.transfer).every(
      ([k, v]) => r.transfer[k as keyof typeof r.transfer] === v,
    )
  );
}
export function fundsRequireRecovery(result: PaymentFunds) {
  return result.submitted || !['UNSUBMITTED', 'REGISTERED_UNSUBMITTED'].includes(result.state);
}
export function sameFundsReference(a: FundsReference, b: FundsReference) {
  return (
    a.task_id === b.task_id &&
    a.operation.operation_id === b.operation.operation_id &&
    a.snapshot_digest === b.snapshot_digest &&
    Object.entries(a.snapshot.transfer).every(
      ([key, value]) => b.snapshot.transfer[key as keyof typeof b.snapshot.transfer] === value,
    )
  );
}
export function originalRequiresRecovery(o: PaymentOriginal | null) {
  return (
    !!o &&
    ((o.state !== 'UNSUBMITTED' && o.state !== 'REGISTERED_UNSUBMITTED') ||
      o.first_public_submission !== null ||
      o.verified_business_result !== null)
  );
}
export function paymentBase(p: { task_id: string; operation: { operation_id: string } }) {
  return `/api/sim/payment-preparations/${encodeURIComponent(p.task_id)}/${encodeURIComponent(p.operation.operation_id)}`;
}
export function originalMatches(p: Payment, o: PaymentOriginal) {
  return (
    o.task_id === p.task_id &&
    o.binding.preparation_operation_id === p.operation.operation_id &&
    o.binding.preparation_arguments_hash === p.snapshot_digest &&
    o.binding.business_operation_id === p.snapshot.transfer.operation_id &&
    o.binding.business_draft_version === p.snapshot.transfer.draft_version &&
    Object.entries(p.snapshot.transfer).every(
      ([k, v]) =>
        o.binding.normalized_arguments[k as keyof typeof o.binding.normalized_arguments] === v,
    )
  );
}
