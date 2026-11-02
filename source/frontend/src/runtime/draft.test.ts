import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  createDraftRequest,
  getDraftRequest,
  prepareDraftRequest,
  recordDraftConfirmation,
} from './draft-client';
import { draftConfirmationSchema, type DraftConfirmationView } from './draft-schema';

const prepared = draftConfirmationSchema.parse({
  contract_version: 1,
  mode: 'PROJECT_SIMULATION',
  scope: 'MOCK_DRAFT_CONFIRMATION_ONLY',
  task_id: 'draft_request_123',
  task_state: 'RUNNING',
  operation_id: 'operation_123',
  operation_state: 'DRAFT',
  task_revision: 2,
  operation_revision: 1,
  plan_version: 1,
  facts_version: 7,
  parameters: {
    payer_account_id: 'account_001',
    payee_id: 'payee_008',
    amount: '200.00',
    currency: 'CNY',
    remark: '',
    replaces: null,
  },
  confirmation: {
    confirmation_ref: 'confirmation_123',
    status: 'PREPARED',
    prepared_at: '2026-10-08T10:00:00Z',
    expires_at: '2026-10-08T10:05:00Z',
    confirmed_at: null,
    revoked_at: null,
  },
  preflight: {
    evidence_kind: 'PREFLIGHT_ONLY',
    pre_tool_outcome: 'READY_TO_CONFIRM',
    pre_auth_outcome: 'READY_TO_CONFIRM',
    required_gates: ['EXPLICIT_CONFIRMATION'],
  },
  display_ticket: 'display_123',
  next_action: { kind: 'CONFIRM' },
});
const confirmed = {
  ...prepared,
  confirmation: {
    ...prepared.confirmation!,
    status: 'CONFIRMED',
    confirmed_at: '2026-10-08T10:01:00Z',
  },
  display_ticket: null,
  next_action: { kind: 'NONE' },
};
const response = (value: unknown, status = 200) =>
  new Response(JSON.stringify(value), { status, headers: { 'Content-Type': 'application/json' } });

afterEach(() => vi.unstubAllGlobals());

describe('draft confirmation view contract isolation', () => {
  it('keeps confirmation separate from business completion and operation readiness', () => {
    const view = draftConfirmationSchema.parse(confirmed);
    expect(view.confirmation?.status).toBe('CONFIRMED');
    expect(view.task_state).toBe('RUNNING');
    expect(view.operation_state).toBe('DRAFT');
    expect(view.next_action.kind).toBe('NONE');
  });

  it('accepts the existing original id before an operation or confirmation is prepared', () => {
    const view = draftConfirmationSchema.parse({
      ...prepared,
      operation_id: null,
      operation_state: null,
      operation_revision: null,
      confirmation: null,
      preflight: null,
      display_ticket: null,
      next_action: { kind: 'PREPARE' },
    });
    expect(view.next_action.kind).toBe('PREPARE');
  });

  it.each([
    { task_state: 'COMPLETED' },
    { operation_state: 'READY' },
    { task_revision: Number.MAX_SAFE_INTEGER + 1 },
    { facts_version: 4294967296 },
    { operation_id: null },
    { confirmation: null },
    { preflight: null },
    { display_ticket: null },
    { scope: 'FUNDS_CONFIRMATION' },
    { owner: 'user_001' },
    { next_action: { kind: 'EXECUTE' } },
    { parameters: { ...prepared.parameters, amount: '1000.00' } },
    { parameters: { ...prepared.parameters, user_id: 'user_001' } },
  ])('rejects unsupported state or parameters: %j', (override) => {
    expect(draftConfirmationSchema.safeParse({ ...prepared, ...override }).success).toBe(false);
  });

  it('requires recorded confirmation time and removes executable display tickets', () => {
    expect(
      draftConfirmationSchema.safeParse({
        ...confirmed,
        confirmation: { ...confirmed.confirmation, confirmed_at: null },
      }).success,
    ).toBe(false);
    expect(
      draftConfirmationSchema.safeParse({ ...confirmed, display_ticket: 'old_display_ticket' })
        .success,
    ).toBe(false);
  });

  it('does not allow a GET recovery hint to retain the previous ticket', () => {
    expect(
      draftConfirmationSchema.safeParse({ ...prepared, next_action: { kind: 'PREPARE' } }).success,
    ).toBe(false);
  });

  it('requires revoked_at for a rejected request without marking the business task cancelled', () => {
    const revoked = draftConfirmationSchema.parse({
      ...prepared,
      confirmation: {
        ...prepared.confirmation!,
        status: 'REVOKED',
        revoked_at: '2026-10-08T10:01:00Z',
      },
      display_ticket: null,
      next_action: { kind: 'NONE' },
    });
    expect(revoked.task_state).toBe('RUNNING');
    expect(revoked.operation_state).toBe('DRAFT');
  });
});

describe('draft confirmation HTTP client boundaries', () => {
  it('creates only the server-defined request with an empty JSON body', async () => {
    const fetch = vi.fn().mockResolvedValue(response(prepared));
    vi.stubGlobal('fetch', fetch);
    await createDraftRequest();
    expect(fetch.mock.calls[0][0]).toBe('/api/sim/draft-confirmations');
    expect(fetch.mock.calls[0][1]).toMatchObject({
      method: 'POST',
      credentials: 'include',
      headers: { 'Content-Type': 'application/json', 'X-SafeFlow-Request': '1' },
      body: '{}',
    });
  });

  it('queries the original request without preparing or confirming anything', async () => {
    const fetch = vi.fn().mockResolvedValue(response(prepared));
    vi.stubGlobal('fetch', fetch);
    await getDraftRequest(prepared.task_id);
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(fetch.mock.calls[0][1]).toMatchObject({
      method: 'GET',
      credentials: 'include',
      cache: 'no-store',
    });
    expect(fetch.mock.calls[0][1].body).toBeUndefined();
  });

  it('prepares only through an explicit POST to the selected original id', async () => {
    const fetch = vi.fn().mockResolvedValue(response(prepared));
    vi.stubGlobal('fetch', fetch);
    await prepareDraftRequest(prepared.task_id);
    expect(fetch.mock.calls[0][0]).toBe('/api/sim/draft-confirmations/draft_request_123/prepare');
    expect(fetch.mock.calls[0][1]).toMatchObject({ method: 'POST', body: '{}' });
  });

  it('sends the current confirmation reference and display ticket with the isolated scope', async () => {
    const fetch = vi.fn().mockResolvedValue(response(confirmed));
    vi.stubGlobal('fetch', fetch);
    const result = await recordDraftConfirmation(prepared, 'CONFIRM');
    expect(result.confirmation?.status).toBe('CONFIRMED');
    expect(JSON.parse(fetch.mock.calls[0][1].body)).toEqual({
      scope: 'MOCK_DRAFT_CONFIRMATION_ONLY',
      task_id: prepared.task_id,
      confirmation_ref: prepared.confirmation?.confirmation_ref,
      display_ticket: prepared.display_ticket,
      action: 'CONFIRM',
    });
  });

  it('does not retry a confirmation when its response is lost', async () => {
    const fetch = vi.fn().mockRejectedValue(new TypeError('Network unavailable'));
    vi.stubGlobal('fetch', fetch);
    await expect(recordDraftConfirmation(prepared, 'CONFIRM')).rejects.toMatchObject({
      reason: 'UNAVAILABLE',
    });
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it('cannot confirm after a new session requires explicit preparation', async () => {
    const fetch = vi.fn();
    vi.stubGlobal('fetch', fetch);
    const recovered = draftConfirmationSchema.parse({
      ...prepared,
      confirmation: null,
      preflight: null,
      display_ticket: null,
      next_action: { kind: 'PREPARE' },
    });
    await expect(recordDraftConfirmation(recovered, 'CONFIRM')).rejects.toMatchObject({
      reason: 'INVALID_RESPONSE',
    });
    expect(fetch).not.toHaveBeenCalled();
  });

  it('refuses malformed confirmation data before sending a request', async () => {
    const fetch = vi.fn();
    vi.stubGlobal('fetch', fetch);
    await expect(
      recordDraftConfirmation(
        { ...prepared, display_ticket: null } as DraftConfirmationView,
        'CONFIRM',
      ),
    ).rejects.toMatchObject({ reason: 'INVALID_RESPONSE' });
    expect(fetch).not.toHaveBeenCalled();
  });

  it('rejects task or operation mismatches in a recorded response', async () => {
    const fetch = vi
      .fn()
      .mockResolvedValue(response({ ...confirmed, operation_id: 'other_operation' }));
    vi.stubGlobal('fetch', fetch);
    await expect(recordDraftConfirmation(prepared, 'CONFIRM')).rejects.toMatchObject({
      reason: 'INVALID_RESPONSE',
    });
  });
});
