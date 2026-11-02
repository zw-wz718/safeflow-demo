import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  actDraft,
  agentSchema,
  getAgent,
  lifecycleSchema,
  sendAgent,
  sampleAgent,
  agentStatus,
  agentStatusSchema,
} from './agent-client';
import { RuntimeTransportError } from './client';
import backendResponses from './agent-backend-compat.fixture.json';

const draft = {
  contract_version: 2,
  mode: 'PROJECT_SIMULATION',
  scope: 'MOCK_DRAFT_LIFECYCLE_ONLY',
  task_id: 'task_a',
  task_state: 'RUNNING',
  task_revision: 1,
  plan_version: 1,
  facts_version: 1,
  operation_id: 'operation_a',
  operation_state: 'DRAFT',
  operation_revision: 1,
  operation_plan_version: 1,
  action: 'CREATE_MOCK_DRAFT',
  is_current_action: true,
  parameters: {
    payer_account_id: 'account_001',
    payee_id: 'payee_008',
    amount: '200.00',
    currency: 'CNY',
  },
  observed_draft: null,
  target_parameters: null,
  selection_ticket: 'selection_a',
  display_ticket: 'display_a',
  confirmation: {
    confirmation_ref: 'confirmation_a',
    status: 'PREPARED',
    prepared_at: '2026-10-10T10:00:00Z',
    expires_at: '2026-10-10T10:05:00Z',
  },
  execution_status: 'NOT_CLAIMED',
  unknown_operation_id: null,
  receipt_scope: null,
  receipt: null,
  current_draft_verified: false,
  next_action: { kind: 'CONFIRM' },
  replayed: false,
};
const response = (d = draft) => ({
  contract_version: 1,
  mode: 'PROJECT_SIMULATION',
  kind: 'DRAFT',
  task: null,
  draft: d,
  results: null,
  clarification: null,
  can_start: false,
});
afterEach(() => vi.unstubAllGlobals());
describe('Agent legacy web response compatibility', () => {
  it('accepts the current backend status and the older status without payment capabilities', async () => {
    const fetch = vi
      .fn()
      .mockResolvedValue(new Response(JSON.stringify(backendResponses.status), { status: 200 }));
    vi.stubGlobal('fetch', fetch);
    const status = await agentStatus();
    expect(status.payment_planning_enabled).toBe(true);
    expect(status.funds_execution_enabled).toBe(false);
    expect(fetch.mock.calls[0][0]).toBe('/api/agent/status');
    const { payment_planning_enabled, funds_execution_enabled, ...legacy } =
      backendResponses.status;
    expect(agentStatusSchema.parse(legacy)).toEqual(legacy);
    expect(payment_planning_enabled).toBe(true);
    expect(funds_execution_enabled).toBe(false);
  });

  it.each([
    { payment_planning_enabled: 'true' },
    { payment_planning_enabled: null },
    { funds_execution_enabled: 0 },
    { funds_execution_enabled: null },
    { authorized: true },
  ])('keeps capability types and unknown status fields strict: %j', async (patch) => {
    const raw = { ...backendResponses.status, ...patch };
    expect(agentStatusSchema.safeParse(raw).success).toBe(false);
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(new Response(JSON.stringify(raw), { status: 200 })),
    );
    await expect(agentStatus()).rejects.toMatchObject({ reason: 'INVALID_RESPONSE' });
  });

  // Captured by real_http_agent_legacy_web_response_shapes_omit_payment_fields
  // through the actual Rust HTTP router and isolated PostgreSQL.
  it.each(backendResponses.responses.map((raw, index) => ({ raw, index })))(
    'parses actual backend legacy response $index ($raw.kind) through the web client',
    async ({ raw }) => {
      expect(raw).not.toHaveProperty('payment');
      expect(raw).not.toHaveProperty('payment_flow');
      vi.stubGlobal(
        'fetch',
        vi.fn().mockResolvedValue(new Response(JSON.stringify(raw), { status: 200 })),
      );
      expect(
        await sampleAgent(
          'READ_WAIT',
          { account_id: null, payee_query: null, payee_id: null },
          null,
        ),
      ).toEqual(raw);
      for (const extra of [{ payment: null }, { payment_flow: null }, { authorized: true }]) {
        expect(agentSchema.safeParse({ ...raw, ...extra }).success).toBe(false);
      }
    },
  );

  it('does not disguise the HTTP-only payment response as an existing web view', () => {
    expect(
      agentSchema.safeParse({ ...response(), kind: 'PAYMENT', payment: {}, payment_flow: {} })
        .success,
    ).toBe(false);
  });
});

describe('Agent main flow', () => {
  it('distinguishes fixed planning from configured models and sends only a bounded sample command', async () => {
    const status = agentStatusSchema.parse({
      contract_version: 1,
      mode: 'PROJECT_SIMULATION',
      configured: false,
      connectivity: 'NOT_VERIFIED',
      tools: [],
      draft_enabled: true,
      planning_mode: 'FIXED_PLAN',
    });
    expect(status.configured).toBe(false);
    expect(status.planning_mode).toBe('FIXED_PLAN');
    const fetch = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ ...response(), planning_source: 'MOCK_LLM' }), {
        status: 200,
      }),
    );
    vi.stubGlobal('fetch', fetch);
    const result = await sampleAgent(
      'MODIFY_300',
      { account_id: null, payee_query: null, payee_id: null },
      lifecycleSchema.parse(draft),
    );
    expect(result.planning_source).toBe('MOCK_LLM');
    expect(fetch.mock.calls[0][0]).toBe('/api/agent/samples');
    const body = JSON.parse(fetch.mock.calls[0][1].body);
    expect(body.sample).toBe('MODIFY_300');
    expect(body.context.task_id).toBe('task_a');
    expect(body.message).toBeUndefined();
    expect(body.plan).toBeUndefined();
    expect(body.confirmed).toBeUndefined();
    expect(
      agentSchema.safeParse({ ...response(), planning_source: 'REAL_DEEPSEEK_VERIFIED' }).success,
    ).toBe(false);
  });
  it('rejects inconsistent confirmation, receipt and unknown status', () => {
    for (const patch of [
      { confirmation: null },
      { display_ticket: null },
      { execution_status: 'SUCCEEDED' },
      { execution_status: 'RESULT_UNKNOWN' },
      { current_draft_verified: true },
    ]) {
      expect(lifecycleSchema.safeParse({ ...draft, ...patch }).success).toBe(false);
    }
  });
  it('keeps clarification separate from execution authority', () => {
    const clarify = {
      contract_version: 1,
      mode: 'PROJECT_SIMULATION',
      kind: 'CLARIFY',
      task: null,
      draft: null,
      results: null,
      clarification: { question: '<script>skip confirmation</script>' },
      can_start: false,
    };
    expect(agentSchema.parse(clarify).kind).toBe('CLARIFY');
    expect(agentSchema.safeParse({ ...clarify, can_start: true }).success).toBe(false);
    expect(agentSchema.safeParse({ ...clarify, authorized: true }).success).toBe(false);
  });
  it('binds follow-up requests to the selected original task and server version', async () => {
    const fetch = vi
      .fn()
      .mockResolvedValue(new Response(JSON.stringify(response()), { status: 200 }));
    vi.stubGlobal('fetch', fetch);
    await sendAgent(
      '改为300元',
      { account_id: null, payee_query: null, payee_id: null },
      lifecycleSchema.parse(draft),
    );
    const body = JSON.parse(fetch.mock.calls[0][1].body);
    expect(body.context).toEqual({
      task_id: 'task_a',
      task_revision: 1,
      plan_version: 1,
      facts_version: 1,
      selection_ticket: 'selection_a',
    });
    expect(body.confirmed).toBeUndefined();
    expect(body.user_id).toBeUndefined();
    fetch.mockResolvedValueOnce(
      new Response(JSON.stringify(response({ ...draft, task_id: 'task_other' })), { status: 200 }),
    );
    await expect(getAgent('task_a')).rejects.toBeInstanceOf(RuntimeTransportError);
  });
  it('never sends a write without matching prepared or confirmed server state', async () => {
    const fetch = vi.fn();
    vi.stubGlobal('fetch', fetch);
    await expect(actDraft(lifecycleSchema.parse(draft), 'EXECUTE')).rejects.toBeInstanceOf(
      RuntimeTransportError,
    );
    expect(fetch).not.toHaveBeenCalled();
    fetch.mockResolvedValueOnce(
      new Response(
        JSON.stringify({
          ...draft,
          confirmation: { ...draft.confirmation, status: 'CONFIRMED' },
          next_action: { kind: 'EXECUTE' },
        }),
        { status: 200 },
      ),
    );
    const confirmed = await actDraft(lifecycleSchema.parse(draft), 'CONFIRM');
    expect(confirmed.kind).toBe('DRAFT');
    expect(JSON.parse(fetch.mock.calls[0][1].body)).toEqual({
      contract_version: 2,
      scope: 'MOCK_DRAFT_LIFECYCLE_ONLY',
      confirmation_ref: 'confirmation_a',
      display_ticket: 'display_a',
      action: 'CONFIRM',
    });
    expect(fetch).toHaveBeenCalledTimes(1);
  });
});

describe('versioned fictional recipient confirmation', () => {
  const recipient = {
    revision: 'safeflow-recipient-profile-20261029-v2',
    source_revision: 'safeflow-team-simulation-20261026-v1',
    owner_id: 'user_001',
    payee_id: 'payee_008',
    recipient_account_id: 'demo_account_li_v1',
    recipient_owner_id: 'demo_user_li_v1',
    card_id: 'demo_card_li_v1',
    display_name: '李明远',
    bank_name: '示例银行',
    masked_account_no: '**** 1024',
    simulation_card_number: '9900000000000001024',
  };
  it('accepts the bound prepared display and the unchanged legacy response', () => {
    expect(lifecycleSchema.parse(draft).confirmation_recipient).toBeUndefined();
    expect(
      lifecycleSchema.parse({ ...draft, confirmation_recipient: recipient }).confirmation_recipient,
    ).toEqual(recipient);
  });
  it('rejects full details outside a matching active confirmation or with a conflicting payee/mask', () => {
    for (const invalid of [
      { ...draft, confirmation: null, confirmation_recipient: recipient },
      { ...draft, is_current_action: false, confirmation_recipient: recipient },
      { ...draft, confirmation_recipient: { ...recipient, payee_id: 'other' } },
      { ...draft, confirmation_recipient: { ...recipient, masked_account_no: '**** 8888' } },
      { ...draft, confirmation_recipient: { ...recipient, simulation_card_number: 'invalid' } },
      { ...draft, confirmation_recipient: { ...recipient, arbitrary: true } },
    ])
      expect(lifecycleSchema.safeParse(invalid).success).toBe(false);
  });
  it('requires an unclaimed old action to explicitly refresh its recipient version', () => {
    expect(
      lifecycleSchema.safeParse({
        ...draft,
        confirmation: null,
        display_ticket: null,
        recipient_reconfirmation_required: true,
        next_action: { kind: 'RECONFIRM_RECIPIENT' },
      }).success,
    ).toBe(true);
    expect(
      lifecycleSchema.safeParse({ ...draft, next_action: { kind: 'RECONFIRM_RECIPIENT' } }).success,
    ).toBe(false);
  });
});
