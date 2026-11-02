import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { executeConfirmedDraft, getDraftExecution } from './draft-execution-client';
import { DraftExecutionResult } from './DraftExecutionPanel';
import { draftExecutionSchema } from './draft-execution-schema';
import { draftConfirmationSchema, type DraftConfirmationView } from './draft-schema';

const reference = { task_id: 'draft_request_123', operation_id: 'public_operation_123' };
const confirmed = draftConfirmationSchema.parse({
  contract_version: 1,
  mode: 'PROJECT_SIMULATION',
  scope: 'MOCK_DRAFT_CONFIRMATION_ONLY',
  ...reference,
  task_state: 'RUNNING',
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
    status: 'CONFIRMED',
    prepared_at: '2026-10-08T10:00:00Z',
    expires_at: '2026-10-08T10:05:00Z',
    confirmed_at: '2026-10-08T10:01:00Z',
    revoked_at: null,
  },
  preflight: {
    evidence_kind: 'PREFLIGHT_ONLY',
    pre_tool_outcome: 'READY_TO_CONFIRM',
    pre_auth_outcome: 'READY_TO_CONFIRM',
    required_gates: ['EXPLICIT_CONFIRMATION'],
  },
  display_ticket: null,
  next_action: { kind: 'NONE' },
});
const completed = {
  contract_version: 1,
  mode: 'PROJECT_SIMULATION',
  scope: 'MOCK_DRAFT_EXECUTION_ONLY',
  ...reference,
  task_state: 'COMPLETED',
  operation_state: 'SUCCEEDED',
  task_revision: 3,
  operation_revision: 2,
  plan_version: 1,
  facts_version: 7,
  execution_status: 'COMPLETED',
  receipt_scope: 'HISTORICAL_EXECUTION_RECEIPT',
  current_draft_verified: false,
  receipt: {
    draft_id: 'actual_draft_1',
    business_operation_id: 'business_operation_1',
    draft_version: 1,
    tool_result_ref: 'actual_result_1',
    executed_at: '2026-10-08T10:02:00Z',
  },
  replayed: false,
  next_action: { kind: 'NONE' },
};
const unsettled = {
  ...completed,
  task_state: 'RUNNING',
  operation_state: 'DRAFT',
  receipt: null,
  execution_status: 'REQUIRES_ORIGINAL_OPERATION_REVIEW',
  replayed: true,
  next_action: { kind: 'QUERY_ORIGINAL_OPERATION' },
};
const response = (value: unknown, status = 200) =>
  new Response(JSON.stringify(value), { status, headers: { 'Content-Type': 'application/json' } });
const errorResponse = (reason: string, taskId: string, status: number) =>
  response(
    {
      contract_version: 1,
      error: {
        reason_code: reason,
        message: 'Test contract error.',
        task_id: taskId,
        next_action: 'QUERY_ORIGINAL_TASK',
      },
    },
    status,
  );

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe('persistent mock draft execution contract', () => {
  it('keeps the real business operation separate from the original public operation', () => {
    const view = draftExecutionSchema.parse(completed);
    expect(view.operation_id).toBe(reference.operation_id);
    expect(view.receipt?.business_operation_id).toBe('business_operation_1');
    expect(view.current_draft_verified).toBe(false);
  });
  it('accepts an unresolved claim without inventing a completed receipt', () => {
    expect(draftExecutionSchema.parse(unsettled).receipt).toBeNull();
  });
  it.each([
    { task_state: 'RUNNING' },
    { operation_state: 'DRAFT' },
    { receipt: null },
    { next_action: { kind: 'QUERY_ORIGINAL_OPERATION' } },
    { current_draft_verified: true },
    { receipt_scope: 'CURRENT_DRAFT' },
    { scope: 'FUNDS_EXECUTION' },
    { task_revision: Number.MAX_SAFE_INTEGER + 1 },
    { facts_version: 4294967296 },
    { operation_revision: 0 },
    { plan_version: 0 },
    { operation_id: '' },
    { receipt: { ...completed.receipt, draft_version: 4294967296 } },
    { receipt: { ...completed.receipt, executed_at: '2026-10-08T18:02:00+08:00' } },
    { permit: 'unapproved' },
  ])('rejects unsafe or inconsistent completed responses: %j', (override) => {
    expect(draftExecutionSchema.safeParse({ ...completed, ...override }).success).toBe(false);
  });
  it('rejects a receipt or NONE action on an unresolved claim', () => {
    expect(
      draftExecutionSchema.safeParse({ ...unsettled, receipt: completed.receipt }).success,
    ).toBe(false);
    expect(
      draftExecutionSchema.safeParse({ ...unsettled, next_action: { kind: 'NONE' } }).success,
    ).toBe(false);
  });
  it('accepts UTC RFC3339 timestamps including zero offset', () => {
    expect(
      draftExecutionSchema.safeParse({
        ...completed,
        receipt: { ...completed.receipt, executed_at: '2026-10-08T10:02:00+00:00' },
      }).success,
    ).toBe(true);
  });
});

describe('persistent mock draft execution HTTP client', () => {
  it('submits only the frozen comparison fields after explicit confirmation', async () => {
    const fetch = vi.fn().mockResolvedValue(response(completed));
    vi.stubGlobal('fetch', fetch);
    await executeConfirmedDraft(confirmed);
    expect(fetch.mock.calls[0][0]).toBe('/api/sim/draft-confirmations/draft_request_123/execute');
    expect(fetch.mock.calls[0][1]).toMatchObject({
      method: 'POST',
      credentials: 'include',
      cache: 'no-store',
      headers: { 'Content-Type': 'application/json', 'X-SafeFlow-Request': '1' },
    });
    expect(JSON.parse(fetch.mock.calls[0][1].body)).toEqual({
      scope: 'MOCK_DRAFT_EXECUTION_ONLY',
      operation_id: reference.operation_id,
      confirmation_ref: confirmed.confirmation?.confirmation_ref,
      task_revision: 2,
      operation_revision: 1,
      plan_version: 1,
      facts_version: 7,
    });
  });
  it('queries the original pair with only one GET and no write body', async () => {
    const fetch = vi.fn().mockResolvedValue(response({ ...completed, replayed: true }));
    vi.stubGlobal('fetch', fetch);
    await getDraftExecution(reference);
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(fetch.mock.calls[0][0]).toBe(
      '/api/sim/draft-executions/draft_request_123/operations/public_operation_123',
    );
    expect(fetch.mock.calls[0][1]).toMatchObject({
      method: 'GET',
      credentials: 'include',
      cache: 'no-store',
    });
    expect(fetch.mock.calls[0][1].body).toBeUndefined();
  });
  it('does not turn PREPARED or REVOKED confirmation into an execution request', async () => {
    const fetch = vi.fn();
    vi.stubGlobal('fetch', fetch);
    for (const status of ['PREPARED', 'REVOKED']) {
      await expect(
        executeConfirmedDraft({
          ...confirmed,
          confirmation: { ...confirmed.confirmation!, status },
        } as DraftConfirmationView),
      ).rejects.toMatchObject({ reason: 'INVALID_RESPONSE' });
    }
    expect(fetch).not.toHaveBeenCalled();
  });
  it.each([{ task_id: 'other_task' }, { operation_id: 'other_operation' }])(
    'binds execution and history results to the original pair: %j',
    async (override) => {
      vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response({ ...completed, ...override })));
      await expect(executeConfirmedDraft(confirmed)).rejects.toMatchObject({
        reason: 'INVALID_RESPONSE',
      });
      await expect(getDraftExecution(reference)).rejects.toMatchObject({
        reason: 'INVALID_RESPONSE',
      });
    },
  );
  it('rejects a NOT_FOUND body bound to another task', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(errorResponse('NOT_FOUND', 'other_task', 404)),
    );
    await expect(getDraftExecution(reference)).rejects.toMatchObject({
      reason: 'INVALID_RESPONSE',
    });
  });
  it('preserves 401 session loss even when the error task is mismatched', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(errorResponse('NOT_AUTHENTICATED', 'other_task', 401)),
    );
    await expect(getDraftExecution(reference)).rejects.toMatchObject({
      reason: 'NOT_AUTHENTICATED',
      status: 401,
    });
  });
  it('times out at 8 seconds without repeating the POST', async () => {
    vi.useFakeTimers();
    const fetch = vi.fn(
      (_path, options: RequestInit) =>
        new Promise((_resolve, reject) => {
          options.signal?.addEventListener('abort', () =>
            reject(new DOMException('Aborted', 'AbortError')),
          );
        }),
    );
    vi.stubGlobal('fetch', fetch);
    const result = expect(executeConfirmedDraft(confirmed)).rejects.toMatchObject({
      reason: 'TIMEOUT',
    });
    await vi.advanceTimersByTimeAsync(7999);
    expect(fetch).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1);
    await result;
    expect(fetch).toHaveBeenCalledTimes(1);
  });
  it('does not repeat a lost POST or silently fabricate a receipt', async () => {
    const fetch = vi.fn().mockRejectedValue(new TypeError('Lost response'));
    vi.stubGlobal('fetch', fetch);
    await expect(executeConfirmedDraft(confirmed)).rejects.toMatchObject({ reason: 'UNAVAILABLE' });
    expect(fetch).toHaveBeenCalledTimes(1);
  });
});

describe('historical receipt presentation', () => {
  it('shows completed history while keeping current draft availability unverified', () => {
    const markup = renderToStaticMarkup(
      createElement(DraftExecutionResult, { view: draftExecutionSchema.parse(completed) }),
    );
    expect(markup).toContain('actual_draft_1');
    expect(markup).toContain('business_operation_1');
    expect(markup).toContain('当前草案未核验');
    expect(markup).toContain('历史保存记录不代表重启后草案仍存在');
    expect(markup).toContain('资金执行未启用');
    expect(markup).not.toContain('请求保存模拟草案');
  });
  it('shows unresolved claims as query-only without a draft or retry control', () => {
    const markup = renderToStaticMarkup(
      createElement(DraftExecutionResult, { view: draftExecutionSchema.parse(unsettled) }),
    );
    expect(markup).toContain('原操作已领取，结果仍待核实');
    expect(markup).toContain('仅可继续查询原任务与操作');
    expect(markup).not.toContain('actual_draft_1');
    expect(markup).not.toContain('请求保存模拟草案');
  });
});
