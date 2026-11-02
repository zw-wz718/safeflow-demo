import { afterEach, describe, expect, it, vi } from 'vitest';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import fixture from './payment-backend.fixture.json';
import { agentSchema, executePayment, queryPaymentFunds } from './agent-client';
import {
  fundsReference,
  fundsResultSchema,
  paymentBase,
  type PaymentFunds,
} from './payment-schema';
import { PaymentPanel } from './AgentResults';

function enabled(confirmed = true) {
  const raw = JSON.parse(JSON.stringify(confirmed ? fixture.waiting : fixture.prepared));
  const p = raw.payment;
  p.expires_at = new Date(Date.now() + 300_000).toISOString();
  p.funds_execution_enabled = raw.payment_flow.funds_execution_enabled = true;
  p.execution_conditions = [];
  p.execution = {
    enabled: true,
    reason: 'RECHECKED_AT_EXPLICIT_EXECUTION',
    execute_path: `${paymentBase(p)}/execute`,
    result_path: `${paymentBase(p)}/funds-result`,
    automatic_retry: false,
  };
  if (confirmed)
    Object.assign(raw.payment_flow, {
      status: 'WAITING_EXPLICIT_PAYMENT',
      next_action: 'EXECUTE_PAYMENT',
      next_path: p.execution.execute_path,
    });
  const v = agentSchema.parse(raw);
  if (v.kind !== 'PAYMENT') throw new Error();
  return v;
}
function result(state: PaymentFunds['state'] = 'SUCCEEDED'): PaymentFunds {
  const p = enabled().payment;
  return {
    contract_version: 1,
    scope: 'PAYMENT_FUNDS_RESULT_ONLY',
    task_id: p.task_id,
    preparation_operation_id: p.operation.operation_id,
    snapshot_digest: p.snapshot_digest,
    transfer: p.snapshot.transfer,
    state,
    submitted: !['UNSUBMITTED', 'REGISTERED_UNSUBMITTED'].includes(state),
    automatic_retry: false,
  };
}
const reply = (r: unknown) => new Response(JSON.stringify(r), { status: 200 });
afterEach(() => vi.unstubAllGlobals());

describe('explicit payment command and minimal original protocol', () => {
  it('defaults missing old capability to disabled and rejects execution before networking', async () => {
    const old = JSON.parse(JSON.stringify(fixture.waiting));
    delete old.payment.funds_execution_enabled;
    delete old.payment_flow.funds_execution_enabled;
    const v = agentSchema.parse(old);
    if (v.kind !== 'PAYMENT') throw new Error();
    expect(v.payment.funds_execution_enabled).toBe(false);
    const fetch = vi.fn();
    vi.stubGlobal('fetch', fetch);
    await expect(executePayment(v)).rejects.toMatchObject({ reason: 'INVALID_RESPONSE' });
    expect(fetch).not.toHaveBeenCalled();
  });
  it('posts once with the original confirmation, and GET independently recovers the same amount/payee', async () => {
    const v = enabled(),
      r = result();
    const fetch = vi.fn().mockImplementation(() => Promise.resolve(reply(r)));
    vi.stubGlobal('fetch', fetch);
    expect(await executePayment(v)).toEqual(r);
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(JSON.parse(fetch.mock.calls[0][1].body)).toEqual({
      contract_version: 1,
      scope: 'PAYMENT_EXECUTION_ONLY',
      confirmation_ref: v.payment.confirmation_ref,
      display_ticket: v.payment.display_ticket,
    });
    expect(await queryPaymentFunds(v.payment)).toEqual(r);
    expect(fetch.mock.calls.map((c) => c[1].method)).toEqual(['POST', 'GET']);
  });
  it('transport loss never retries POST or infers a terminal state', async () => {
    const fetch = vi.fn().mockRejectedValue(new TypeError('response lost'));
    vi.stubGlobal('fetch', fetch);
    await expect(executePayment(enabled())).rejects.toMatchObject({ reason: 'UNAVAILABLE' });
    expect(fetch).toHaveBeenCalledTimes(1);
  });
  it.each(['amount', 'payee_id', 'remark', 'draft_version', 'operation_id'] as const)(
    'rejects swapped original %s',
    async (key) => {
      const r = structuredClone(result());
      Object.assign(r.transfer, {
        [key]: key === 'draft_version' ? 99 : key === 'amount' ? '999.00' : 'other',
      });
      vi.stubGlobal('fetch', vi.fn().mockResolvedValue(reply(r)));
      await expect(executePayment(enabled())).rejects.toMatchObject({ reason: 'INVALID_RESPONSE' });
    },
  );
  it('rejects foreign paths, invented authority, and contradictory settlement', () => {
    const v = enabled();
    expect(
      agentSchema.safeParse({
        ...v,
        payment: {
          ...v.payment,
          execution: { ...v.payment.execution, execute_path: 'https://other.invalid/pay' },
        },
      }).success,
    ).toBe(false);
    expect(fundsResultSchema.safeParse({ ...result(), permit: 'forged' }).success).toBe(false);
    expect(fundsResultSchema.safeParse({ ...result(), submitted: false }).success).toBe(false);
    expect(fundsResultSchema.safeParse({ ...result('UNSUBMITTED'), submitted: true }).success).toBe(
      false,
    );
  });
  it.each([
    'task_id',
    'preparation_operation_id',
    'snapshot_digest',
    'transfer.task_id',
    'transfer.operation_id',
    'transfer.draft_id',
    'transfer.draft_version',
    'transfer.payer_account_id',
    'transfer.payee_id',
    'transfer.amount',
    'transfer.currency',
    'transfer.remark',
  ])('original GET rejects a terminal with a different %s', async (field) => {
    const r = structuredClone(result());
    const [parent, child] = field.split('.');
    const target = (child ? r.transfer : r) as unknown as Record<string, unknown>;
    const key = child ?? parent;
    target[key] =
      key === 'draft_version'
        ? 999
        : key === 'amount'
          ? '999.00'
          : key === 'snapshot_digest'
            ? 'f'.repeat(64)
            : 'different';
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(reply(r)));
    await expect(queryPaymentFunds(enabled().payment)).rejects.toMatchObject({
      reason: 'INVALID_RESPONSE',
    });
  });
  it('blocks another payment while leaving read controls available; a discovered terminal remains original-only', () => {
    const props = {
      view: enabled(),
      original: null,
      enabled: true,
      fresh: true,
      expired: false,
      onAction() {},
      onOriginal() {},
      onRefresh() {},
      onExecute() {},
    };
    const blocked = renderToStaticMarkup(
      createElement(PaymentPanel, { ...props, writeBlocked: true }),
    );
    expect(blocked).toMatch(/<button[^>]*disabled=""[^>]*>[\s\S]*?确认付款<\/button>/);
    expect(blocked).toMatch(/<button class="r-command"><svg[\s\S]*?查询付款原单<\/button>/);
    for (const state of ['SUCCEEDED', 'FAILED'] as const) {
      const terminal = renderToStaticMarkup(
        createElement(PaymentPanel, { ...props, funds: result(state) }),
      );
      expect(terminal).not.toContain('确认付款</button>');
      expect(terminal).toContain('查询付款原单');
    }
  });
  it('rejects expired and prepared commands before networking', async () => {
    const fetch = vi.fn();
    vi.stubGlobal('fetch', fetch);
    const v = enabled();
    v.payment.expires_at = new Date(0).toISOString();
    await expect(executePayment(v)).rejects.toMatchObject({ reason: 'INVALID_RESPONSE' });
    await expect(executePayment(enabled(false))).rejects.toMatchObject({
      reason: 'INVALID_RESPONSE',
    });
    expect(fetch).not.toHaveBeenCalled();
  });
  it('separates information confirmation from payment, then closes stale actions for pending/unknown originals', () => {
    const props = {
      original: null,
      enabled: true,
      fresh: true,
      expired: false,
      onAction() {},
      onOriginal() {},
      onRefresh() {},
      onExecute() {},
    };
    const prepared = renderToStaticMarkup(
      createElement(PaymentPanel, { ...props, view: enabled(false) }),
    );
    expect(prepared).toContain('确认付款信息');
    expect(prepared).not.toContain('>确认付款</button>');
    const waiting = renderToStaticMarkup(
      createElement(PaymentPanel, { ...props, view: enabled() }),
    );
    expect(waiting).toContain('确认付款</button>');
    for (const state of ['PROCESSING', 'SUCCEEDED', 'FAILED', 'RECONCILE_REQUIRED'] as const) {
      const html = renderToStaticMarkup(
        createElement(PaymentPanel, {
          ...props,
          view: enabled(),
          attempted: true,
          funds: result(state),
        }),
      );
      expect(html).not.toContain('确认付款</button>');
      expect(html).not.toContain('确认付款信息');
      expect(html).toContain('查询付款原单');
    }
    const reference = fundsReference(enabled().payment);
    expect(Object.keys(reference).sort()).toEqual([
      'operation',
      'snapshot',
      'snapshot_digest',
      'task_id',
    ]);
    expect(JSON.stringify(reference)).not.toMatch(
      /display_ticket|confirmation_ref|idempotency_key|source|security_decisions/,
    );
  });
});
