import { z } from 'zod';
import { request, RuntimeTransportError } from './client';
import { taskSchema } from './schema';
import { billingDataSchema } from './billing-schema';
import {
  paymentSchema,
  paymentFlowSchema,
  originalResultSchema,
  originalMatches,
  paymentBase,
  fundsResultSchema,
  fundsMatches,
  type FundsReference,
} from './payment-schema';

const id = z.string().min(1).max(200);
const revision = z.number().int().min(1).max(Number.MAX_SAFE_INTEGER);
const version = z.number().int().min(1).max(4294967295);
const parameters = z.record(z.unknown());
const timestamp = z.string().datetime({ offset: true });
const observation = z
  .object({
    draft_id: id,
    business_operation_id: id,
    draft_version: version,
    status: z.enum(['READY_TO_CONFIRM', 'CANCELLED', 'EXPIRED', 'NEEDS_INFORMATION']),
    expires_at: timestamp,
    parameters,
  })
  .strict();
export const intentTerminationSchema=z.object({
  schema:z.literal('UNEXECUTED_DRAFT_TERMINATED_V1'),task_id:id,operation_id:id,reason:z.enum(['EXPIRED','REPLACED','ABANDONED']),
  proof:z.object({scope:z.enum(['NEVER_CLAIMED','DRAFT_CREATED_NO_PAYMENT_ACCEPTANCE','EXPIRED_UNSUBMITTED_PREPARATION']),task_digest:z.string().regex(/^[a-f0-9]{64}$/),operations_digest:z.string().regex(/^[a-f0-9]{64}$/),parameters_digest:z.string().regex(/^[a-f0-9]{64}$/),receipt_digest:z.string().regex(/^[a-f0-9]{64}$/).nullable(),expires_at:timestamp.nullable(),preparation:z.object({operation_id:id,snapshot_digest:z.string().regex(/^[a-f0-9]{64}$/),expires_at:timestamp,draft_id:id,draft_version:version}).strict().optional()}).strict(),
  original_preserved:z.literal(true),payment_cancelled:z.literal(false),funding_authorized:z.literal(false),
  replacement:z.object({task_id:id,parameters,facts_version:revision}).strict().nullable(),
}).strict().superRefine((v,c)=>{
  if((v.proof.scope==='EXPIRED_UNSUBMITTED_PREPARATION')!==(v.proof.preparation!==undefined)||(v.proof.preparation&&(v.reason!=='ABANDONED'||v.proof.expires_at===null)))c.addIssue({code:'custom',message:'invalid expired preparation proof'});
  if((v.reason==='REPLACED')!==(v.replacement!==null)||v.replacement?.task_id===v.task_id||(v.reason==='EXPIRED'&&v.proof.expires_at===null)||
    (v.proof.scope==='NEVER_CLAIMED')!==(v.proof.receipt_digest===null))c.addIssue({code:'custom',message:'inconsistent draft termination proof'});
});
export const lifecycleSchema = z
  .object({
    contract_version: z.literal(2),
    mode: z.literal('PROJECT_SIMULATION'),
    scope: z.literal('MOCK_DRAFT_LIFECYCLE_ONLY'),
    task_id: id,
    task_state: z.enum([
      'RUNNING',
      'WAITING_USER',
      'WAITING_EXTERNAL',
      'COMPLETED',
      'FAILED',
      'CANCELLED',
    ]),
    task_revision: revision,
    plan_version: version,
    facts_version: version,
    operation_id: id,
    active_operation_id: id.optional(),
    operation_state: z.enum([
      'DRAFT',
      'READY',
      'SUBMITTED',
      'PROCESSING',
      'SUCCEEDED',
      'FAILED',
      'CANCELLED',
    ]),
    operation_revision: revision,
    operation_plan_version: version,
    action: z.enum(['CREATE_MOCK_DRAFT', 'MODIFY_MOCK_DRAFT', 'CANCEL_MOCK_DRAFT']),
    is_current_action: z.boolean(),
    parameters,
    observed_draft: observation.nullable(),
    target_parameters: parameters.nullable(),
    selection_ticket: id,
    recipient_reconfirmation_required: z.literal(true).optional(),
    intent_termination:intentTerminationSchema.optional(),
    intent_expires_at:timestamp.optional(),
    display_ticket: id.nullable(),
    confirmation_recipient: z
      .object({
        revision: id,
        source_revision: id,
        owner_id: id,
        payee_id: id,
        recipient_account_id: id,
        recipient_owner_id: id,
        card_id: id,
        display_name: z.string().min(1).max(100),
        bank_name: z.string().min(1).max(100),
        masked_account_no: z.string().regex(/^\*{4} [0-9]{4}$/),
        simulation_card_number: z.string().regex(/^99[0-9]{17}$/),
      })
      .strict()
      .optional(),
    confirmation: z
      .object({
        confirmation_ref: id,
        status: z.enum(['PREPARED', 'CONFIRMED', 'REVOKED']),
        prepared_at: timestamp,
        expires_at: timestamp,
      })
      .strict()
      .nullable(),
    execution_status: z.enum(['SUCCEEDED', 'RESULT_UNKNOWN', 'NOT_CLAIMED']),
    unknown_operation_id: id.nullable(),
    receipt_scope: z.literal('HISTORICAL_EXECUTION_RECEIPT').nullable(),
    receipt: z
      .object({
        action: z.string(),
        operation_id: id,
        plan_version: version,
        expected_draft_version: version.nullable(),
        draft_id: id,
        business_operation_id: id,
        draft_version: version,
        tool_result_ref: id,
        executed_at: timestamp,
      })
      .strict()
      .nullable(),
    current_draft_verified: z.literal(false),
    next_action: z.discriminatedUnion('kind', [
      z.object({ kind: z.literal('QUERY_ORIGINAL_OPERATION'), operation_id: id }).strict(),
      ...(
        [
          'FINISHED',
          'REVIEW_ORIGINAL_TASK',
          'SELECT_ACTION',
          'EXECUTE',
          'CONFIRM',
          'PREPARE_OR_REPLACE_ACTION',
          'RECONFIRM_RECIPIENT',
        ] as const
      ).map((kind) => z.object({ kind: z.literal(kind) }).strict()),
    ]),
    replayed: z.boolean(),
  })
  .strict()
  .superRefine((view, ctx) => {
    if (
      // The termination ends the TASK's proven unpaid intent. Its operation_id
      // binds the authoritative active operation, not a selected history row.
      (view.active_operation_id !== undefined &&
        (view.is_current_action !== (view.operation_id === view.active_operation_id) ||
         (view.is_current_action ? view.operation_plan_version !== view.plan_version : view.operation_plan_version >= view.plan_version))) ||
      (view.intent_termination && (view.intent_termination.task_id!==view.task_id || !view.active_operation_id || view.intent_termination.operation_id!==view.active_operation_id || view.next_action.kind!=='FINISHED'||!['CREATE_MOCK_DRAFT','MODIFY_MOCK_DRAFT'].includes(view.action)||view.execution_status==='RESULT_UNKNOWN'||view.confirmation!==null||view.display_ticket!==null)) ||
      (view.next_action.kind === 'RECONFIRM_RECIPIENT' &&
        (!view.recipient_reconfirmation_required || view.execution_status !== 'NOT_CLAIMED')) ||
      (view.confirmation_recipient !== undefined &&
        (!view.confirmation ||
          !view.display_ticket ||
          !view.is_current_action ||
          view.execution_status !== 'NOT_CLAIMED' ||
          view.task_state !== 'RUNNING' ||
          !['PREPARED', 'CONFIRMED'].includes(view.confirmation.status) ||
          view.confirmation_recipient.payee_id !==
            (view.target_parameters?.payee_id ?? view.parameters.payee_id) ||
          !view.confirmation_recipient.masked_account_no.endsWith(
            view.confirmation_recipient.simulation_card_number.slice(-4),
          ))) ||
      (view.execution_status === 'SUCCEEDED') !== (view.receipt !== null) ||
      (view.execution_status === 'RESULT_UNKNOWN') !== (view.unknown_operation_id !== null) ||
      (view.receipt &&
        (view.receipt.operation_id !== view.operation_id ||
          view.receipt.plan_version !== view.operation_plan_version)) ||
      (view.next_action.kind === 'CONFIRM' &&
        (view.confirmation?.status !== 'PREPARED' || !view.display_ticket)) ||
      (view.next_action.kind === 'EXECUTE' && view.confirmation?.status !== 'CONFIRMED')
    ) {
      ctx.addIssue({ code: z.ZodIssueCode.custom, message: 'Inconsistent lifecycle response' });
    }
  });
export const payeeSchema = z
  .object({
    payee_id: id,
    display_name: z.string().min(1),
    bank_name: z.string().min(1),
    masked_account_no: z.string().min(1),
  })
  .strict();
const decimal = z.string().regex(/^(0|[1-9][0-9]*)\.[0-9]{2}$/);
const resultFields = {
  contract_version: z.literal(1),
  operation_id: id,
  result_ref: id,
  status: z.literal('SUCCEEDED'),
};
export const resultSchema = z
  .discriminatedUnion('tool_name', [
    z
      .object({
        ...resultFields,
        tool_name: z.literal('analyze_local_basic_spending'),
        data: billingDataSchema,
      })
      .strict(),
    z
      .object({
        ...resultFields,
        tool_name: z.literal('get_account_balance'),
        data: z
          .object({
            account: z
              .object({
                account_id: id,
                display_name: z.string().min(1),
                masked_account_no: z.string().min(1),
                currency: z.literal('CNY'),
              })
              .strict(),
            available_balance: decimal,
            as_of: timestamp,
          })
          .strict(),
      })
      .strict(),
    z
      .object({
        ...resultFields,
        tool_name: z.literal('find_payees'),
        data: z.object({ candidates: z.array(payeeSchema) }).strict(),
      })
      .strict(),
  ])
  .superRefine((result, ctx) => {
    if (
      result.tool_name === 'analyze_local_basic_spending' &&
      result.data.adapter_contract === 'LOCAL_BILLING_REPORT_ADAPTER_V2' &&
      (result.data.report.metadata.report_id !== `billing_report_${result.operation_id}` ||
        result.result_ref !== `billing_result_${result.operation_id}`)
    )
      ctx.addIssue({ code: 'custom', message: 'Billing report does not match public operation' });
  });
const shared = { contract_version: z.literal(1), mode: z.literal('PROJECT_SIMULATION') };
const proposalFields = {
  ...shared,
  planning_source: z
    .enum(['MOCK_LLM', 'OPENAI_COMPATIBLE', 'UNSPECIFIED'])
    .nullable()
    .optional()
    .default(null),
};
export const agentSchema = z
  .discriminatedUnion('kind', [
    z
      .object({
        ...proposalFields,
        kind: z.literal('PAYMENT'),
        task: z.null(),
        draft: lifecycleSchema,
        results: z.null(),
        clarification: z.null(),
        can_start: z.literal(false),
        payment: paymentSchema,
        payment_flow: paymentFlowSchema,
      })
      .strict(),
    z
      .object({
        ...proposalFields,
        kind: z.literal('READ'),
        task: taskSchema,
        draft: z.null(),
        results: z.array(resultSchema).nullable(),
        clarification: z.null(),
        can_start: z.boolean(),
      })
      .strict(),
    z
      .object({
        ...proposalFields,
        kind: z.literal('DRAFT'),
        task: z.null(),
        draft: lifecycleSchema,
        results: z.null(),
        clarification: z.null(),
        can_start: z.literal(false),
      })
      .strict(),
    z
      .object({
        ...proposalFields,
        kind: z.literal('CLARIFY'),
        bank_service: z.enum(['CARDS', 'SUBSCRIPTIONS', 'BILLS']).optional(),
        task: z.null(),
        draft: z.null(),
        results: z.null(),
        clarification: z.object({ question: z.string().min(1).max(4096) }).strict(),
        can_start: z.literal(false),
      })
      .strict(),
  ])
  .superRefine((v, ctx) => {
    if (v.kind !== 'PAYMENT') return;
    const p = v.payment,
      f = v.payment_flow;
    const expected = p.business_recovery_required
      ? ['QUERY_ORIGINAL', 'QUERY_ORIGINAL', `${paymentBase(p)}/original-result`]
      : p.invalid_reason !== null
        ? [
            'ENDED_INVALIDATED',
            'REVIEW_CURRENT_DRAFT',
            `/api/agent/tasks/${encodeURIComponent(p.task_id)}`,
          ]
        : p.confirmation_status === 'PREPARED'
          ? ['WAITING_PAYMENT_CONFIRMATION', 'CONFIRM_OR_REJECT', `${paymentBase(p)}/confirm`]
          : p.execution?.enabled &&
              p.funds_execution_enabled &&
              p.confirmation_status === 'CONFIRMED'
            ? ['WAITING_EXPLICIT_PAYMENT', 'EXECUTE_PAYMENT', `${paymentBase(p)}/execute`]
            : ['WAITING_EXECUTION_CONDITIONS', 'QUERY_OR_EVALUATE', `${paymentBase(p)}/evaluate`];
    if (
      v.draft.task_id !== p.task_id ||
      !p.agent_binding ||
      p.agent_binding.planning_source !== v.planning_source ||
      p.agent_binding.reviewed_plan.nodes[0].id !== f.waiting_node_id ||
      f.status !== expected[0] ||
      f.next_action !== expected[1] ||
      f.next_path !== expected[2] ||
      p.business_recovery_required !== (f.original !== null) ||
      (f.original && !originalMatches(p, f.original)) ||
      f.funds_execution_enabled !== p.funds_execution_enabled ||
      (f.funds_result && !fundsMatches(p, f.funds_result))
    )
      ctx.addIssue({ code: 'custom', message: 'Inconsistent Agent payment flow' });
  });
export const agentStatusSchema = z
  .object({
    ...shared,
    chat_request_epoch: z.string().uuid().optional(),
    configured: z.boolean(),
    connectivity: z.literal('NOT_VERIFIED'),
    tools: z.array(id),
    draft_enabled: z.boolean(),
    planning_mode: z.enum(['DISABLED', 'FIXED_PLAN', 'OPENAI_COMPATIBLE']),
    payment_planning_enabled: z.boolean().optional(),
    funds_execution_enabled: z.boolean().optional(),
  })
  .strict();
export type AgentView = z.infer<typeof agentSchema>;
export type LifecycleView = z.infer<typeof lifecycleSchema>;
export type Payee = z.infer<typeof payeeSchema>;
export type AgentStatus = z.infer<typeof agentStatusSchema>;
export type FixedSample =
  'READ_WAIT' | 'CREATE_200' | 'MODIFY_300' | 'CANCEL_DRAFT' | 'EXECUTE_TRANSFER';
export type AgentSelection = {
  account_id: string | null;
  payee_query: string | null;
  payee_id: string | null;
};
function parse<S extends z.ZodTypeAny>(schema: S, raw: unknown): z.output<S> {
  const result = schema.safeParse(raw);
  if (!result.success) throw new RuntimeTransportError('INVALID_RESPONSE');
  return result.data;
}
export function viewId(view: AgentView) {
  return view.kind === 'READ'
    ? view.task.task_id
    : view.kind === 'DRAFT' || view.kind === 'PAYMENT'
      ? view.draft.task_id
      : null;
}
export async function agentStatus(signal?: AbortSignal) {
  return parse(agentStatusSchema, await request('/api/agent/status', 'GET', undefined, signal));
}
export async function searchPayees(query: string, signal?: AbortSignal) {
  return parse(
    z.object({ candidates: z.array(payeeSchema) }).strict(),
    await request('/api/agent/payees', 'POST', { query }, signal),
  );
}
export async function getAgent(id: string, signal?: AbortSignal) {
  const view = parse(
    agentSchema,
    await request(`/api/agent/tasks/${encodeURIComponent(id)}`, 'GET', undefined, signal, 35000),
  );
  if (viewId(view) !== id) throw new RuntimeTransportError('INVALID_RESPONSE');
  return view;
}
export async function sendAgent(
  message: string,
  selection: AgentSelection,
  context: LifecycleView | null,
  signal?: AbortSignal,
) {
  return submitProposal('/api/agent/messages', { message }, selection, context, signal);
}
export async function sampleAgent(
  sample: FixedSample,
  selection: AgentSelection,
  context: LifecycleView | null,
  signal?: AbortSignal,
) {
  if (
    sample === 'EXECUTE_TRANSFER' &&
    (!context?.receipt ||
      context.next_action.kind !== 'SELECT_ACTION' ||
      context.action === 'CANCEL_MOCK_DRAFT')
  )
    throw new RuntimeTransportError('INVALID_RESPONSE');
  return submitProposal('/api/agent/samples', { sample }, selection, context, signal);
}

export async function actPayment(
  view: Extract<AgentView, { kind: 'PAYMENT' }>,
  action: 'CONFIRM' | 'REJECT' | 'EVALUATE',
  signal?: AbortSignal,
) {
  const checked = parse(agentSchema, view);
  if (checked.kind !== 'PAYMENT') throw new RuntimeTransportError('INVALID_RESPONSE');
  const p = checked.payment;
  if (
    p.invalid_reason ||
    p.business_recovery_required ||
    Date.parse(p.expires_at) <= Date.now() ||
    (action === 'EVALUATE'
      ? p.confirmation_status !== 'CONFIRMED'
      : p.confirmation_status !== 'PREPARED')
  )
    throw new RuntimeTransportError('INVALID_RESPONSE');
  const next = parse(
    paymentSchema,
    await request(
      `${paymentBase(p)}/${action === 'EVALUATE' ? 'evaluate' : 'confirm'}`,
      'POST',
      {
        contract_version: 1,
        scope: 'PAYMENT_INTENT_ONLY',
        confirmation_ref: p.confirmation_ref,
        display_ticket: p.display_ticket,
        action: action === 'EVALUATE' ? 'CONFIRM' : action,
      },
      signal,
      35000,
    ),
  );
  if (
    next.task_id !== p.task_id ||
    next.operation.operation_id !== p.operation.operation_id ||
    next.confirmation_ref !== p.confirmation_ref ||
    next.snapshot_digest !== p.snapshot_digest
  )
    throw new RuntimeTransportError('INVALID_RESPONSE');
  // Re-read the Agent projection after a write; never manufacture flow/authority locally.
  return getAgent(p.task_id, signal);
}
export async function queryPaymentOriginal(
  view: Extract<AgentView, { kind: 'PAYMENT' }>,
  signal?: AbortSignal,
) {
  const checked = parse(agentSchema, view);
  if (checked.kind !== 'PAYMENT') throw new RuntimeTransportError('INVALID_RESPONSE');
  const original = parse(
    originalResultSchema,
    await request(
      `${paymentBase(checked.payment)}/original-result`,
      'GET',
      undefined,
      signal,
      35000,
    ),
  );
  if (!originalMatches(checked.payment, original))
    throw new RuntimeTransportError('INVALID_RESPONSE');
  return original;
}
export async function queryPaymentFunds(p: FundsReference, signal?: AbortSignal) {
  const result = parse(
    fundsResultSchema,
    await request(`${paymentBase(p)}/funds-result`, 'GET', undefined, signal, 35000),
  );
  if (!fundsMatches(p, result)) throw new RuntimeTransportError('INVALID_RESPONSE');
  return result;
}
export async function executePayment(
  view: Extract<AgentView, { kind: 'PAYMENT' }>,
  signal?: AbortSignal,
) {
  const checked = parse(agentSchema, view);
  if (checked.kind !== 'PAYMENT') throw new RuntimeTransportError('INVALID_RESPONSE');
  const p = checked.payment;
  if (
    !p.execution?.enabled ||
    !p.funds_execution_enabled ||
    p.invalid_reason ||
    p.business_recovery_required ||
    checked.payment_flow.original ||
    p.confirmation_status !== 'CONFIRMED' ||
    Date.parse(p.expires_at) <= Date.now()
  )
    throw new RuntimeTransportError('INVALID_RESPONSE');
  const result = parse(
    fundsResultSchema,
    await request(
      `${paymentBase(p)}/execute`,
      'POST',
      {
        contract_version: 1,
        scope: 'PAYMENT_EXECUTION_ONLY',
        confirmation_ref: p.confirmation_ref,
        display_ticket: p.display_ticket,
      },
      signal,
      35000,
    ),
  );
  if (!fundsMatches(p, result)) throw new RuntimeTransportError('INVALID_RESPONSE');
  return result;
}
async function submitProposal(
  path: string,
  input: { message: string } | { sample: FixedSample },
  selection: AgentSelection,
  context: LifecycleView | null,
  signal?: AbortSignal,
) {
  const view = parse(
    agentSchema,
    await request(
      path,
      'POST',
      {
        ...input,
        selection,
        context: context
          ? {
              task_id: context.task_id,
              task_revision: context.task_revision,
              plan_version: context.plan_version,
              facts_version: context.facts_version,
              selection_ticket: context.selection_ticket,
            }
          : null,
      },
      signal,
      250000,
    ),
  );
  if (context && view.kind !== 'CLARIFY' && viewId(view) !== context.task_id)
    throw new RuntimeTransportError('INVALID_RESPONSE');
  return view;
}
export async function runRead(
  view: Extract<AgentView, { kind: 'READ' }>,
  continuing: boolean,
  signal?: AbortSignal,
) {
  const t = view.task;
  if (continuing ? t.next_action.kind !== 'CONTINUE' : !view.can_start)
    throw new RuntimeTransportError('INVALID_RESPONSE');
  const result = parse(
    agentSchema,
    await request(
      `/api/agent/tasks/${encodeURIComponent(t.task_id)}/${continuing ? 'continue' : 'execute'}`,
      'POST',
      {
        task_revision: t.revision,
        plan_version: t.plan_version,
        facts_version: t.facts_version,
        ...(continuing && t.next_action.kind === 'CONTINUE'
          ? { waiting_node_id: t.next_action.node_id }
          : {}),
      },
      signal,
      35000,
    ),
  );
  if (viewId(result) !== t.task_id) throw new RuntimeTransportError('INVALID_RESPONSE');
  return result;
}
export async function prepareAgent(view: LifecycleView, signal?: AbortSignal) {
  const result = parse(
    agentSchema,
    await request(
      `/api/agent/tasks/${encodeURIComponent(view.task_id)}/prepare`,
      'POST',
      { operation_id: view.operation_id },
      signal,
      35000,
    ),
  );
  if (
    result.kind !== 'DRAFT' ||
    result.draft.task_id !== view.task_id ||
    result.draft.operation_id !== view.operation_id
  )
    throw new RuntimeTransportError('INVALID_RESPONSE');
  return result;
}
export async function actDraft(
  view: LifecycleView,
  action: 'CONFIRM' | 'REJECT' | 'EXECUTE',
  signal?: AbortSignal,
) {
  if (
    !view.confirmation ||
    (action !== 'EXECUTE' && (!view.display_ticket || view.confirmation.status !== 'PREPARED')) ||
    (action === 'EXECUTE' && view.confirmation.status !== 'CONFIRMED')
  )
    throw new RuntimeTransportError('INVALID_RESPONSE');
  const body =
    action === 'EXECUTE'
      ? {
          task_revision: view.task_revision,
          operation_revision: view.operation_revision,
          plan_version: view.plan_version,
          facts_version: view.facts_version,
        }
      : { display_ticket: view.display_ticket, action };
  const result = parse(
    lifecycleSchema,
    await request(
      `/api/sim/draft-lifecycles/${encodeURIComponent(view.task_id)}/operations/${encodeURIComponent(view.operation_id)}/${action === 'EXECUTE' ? 'execute' : 'confirm'}`,
      'POST',
      {
        contract_version: 2,
        scope: 'MOCK_DRAFT_LIFECYCLE_ONLY',
        confirmation_ref: view.confirmation.confirmation_ref,
        ...body,
      },
      signal,
      35000,
    ),
  );
  if (result.task_id !== view.task_id || result.operation_id !== view.operation_id)
    throw new RuntimeTransportError('INVALID_RESPONSE');
  return parse(agentSchema, {
    contract_version: 1,
    mode: 'PROJECT_SIMULATION',
    kind: 'DRAFT',
    task: null,
    draft: result,
    results: null,
    clarification: null,
    can_start: false,
  });
}

// Replaces only an unclaimed old action, with original parameters and original
// selection/CAS rules; a claim or submitted payment never enters this route.
export async function refreshRecipient(view: LifecycleView, signal?: AbortSignal) {
  if (view.next_action.kind !== 'RECONFIRM_RECIPIENT' || view.execution_status !== 'NOT_CLAIMED')
    throw new RuntimeTransportError('INVALID_RESPONSE');
  const next = parse(
    lifecycleSchema,
    await request(
      `/api/sim/draft-lifecycles/${encodeURIComponent(view.task_id)}/actions`,
      'POST',
      {
        contract_version: 2,
        scope: 'MOCK_DRAFT_LIFECYCLE_ONLY',
        selection_ticket: view.selection_ticket,
        task_revision: view.task_revision,
        plan_version: view.plan_version,
        facts_version: view.facts_version,
        action: view.action === 'CREATE_MOCK_DRAFT' ? 'CREATE_MOCK_DRAFT' : 'MODIFY_MOCK_DRAFT',
        parameters: view.parameters,
      },
      signal,
      35000,
    ),
  );
  if (
    next.task_id !== view.task_id ||
    next.plan_version !== view.plan_version + 1 ||
    next.recipient_reconfirmation_required ||
    next.confirmation ||
    next.receipt
  )
    throw new RuntimeTransportError('INVALID_RESPONSE');
  return next;
}

// Select only the existing cancel action for an actually observed saved draft.
// Selection is not confirmation or execution, and never reconciles an unknown claim.
export async function selectSavedDraftCancellation(view: LifecycleView, signal?: AbortSignal) {
  const latest = await getAgent(view.task_id, signal);
  if (
    latest.kind !== 'DRAFT' ||
    latest.draft.operation_id !== view.operation_id ||
    latest.draft.execution_status !== 'SUCCEEDED' ||
    !latest.draft.receipt ||
    latest.draft.action === 'CANCEL_MOCK_DRAFT' ||
    latest.draft.next_action.kind !== 'SELECT_ACTION'
  )
    throw Error('原草案尚未满足取消条件，请先查询原办理进度');
  const current = latest.draft;
  const preparations = z
    .array(paymentSchema)
    .max(256)
    .parse(
      await request(
        `/api/sim/payment-preparations/${encodeURIComponent(current.task_id)}`,
        'GET',
        undefined,
        signal,
      ),
    );
  if (preparations.length) throw Error('此草案已有付款准备，请按付款原号核对；这里不能取消付款');
  const observed = z
    .object({
      contract_version: z.literal(2),
      mode: z.literal('PROJECT_SIMULATION'),
      scope: z.literal('MOCK_DRAFT_LIFECYCLE_CURRENT_ONLY'),
      task_id: id,
      operation_id: id,
      current_draft_verified: z.literal(true),
      draft: observation,
    })
    .strict()
    .parse(
      await request(
        `/api/sim/draft-lifecycles/${encodeURIComponent(current.task_id)}/operations/${encodeURIComponent(current.operation_id)}/current-draft`,
        'GET',
        undefined,
        signal,
      ),
    );
  if (
    observed.task_id !== current.task_id ||
    observed.operation_id !== current.operation_id ||
    observed.draft.status !== 'READY_TO_CONFIRM' ||
    Date.parse(observed.draft.expires_at) <= Date.now()
  )
    throw Error('原草案当前状态未满足取消条件，未发送取消请求');
  const next = parse(
    lifecycleSchema,
    await request(
      `/api/sim/draft-lifecycles/${encodeURIComponent(current.task_id)}/actions`,
      'POST',
      {
        contract_version: 2,
        scope: 'MOCK_DRAFT_LIFECYCLE_ONLY',
        selection_ticket: current.selection_ticket,
        task_revision: current.task_revision,
        plan_version: current.plan_version,
        facts_version: current.facts_version,
        action: 'CANCEL_MOCK_DRAFT',
        parameters: {
          draft_id: observed.draft.draft_id,
          expected_version: observed.draft.draft_version,
        },
      },
      signal,
      35000,
    ),
  );
  if (
    next.task_id !== current.task_id ||
    next.action !== 'CANCEL_MOCK_DRAFT' ||
    next.plan_version !== current.plan_version + 1 ||
    next.confirmation ||
    next.receipt
  )
    throw new RuntimeTransportError('INVALID_RESPONSE');
  return next;
}
