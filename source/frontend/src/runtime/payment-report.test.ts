import { afterEach, describe, expect, it, vi } from 'vitest';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import {
  actPayment,
  agentSchema,
  getAgent,
  queryPaymentOriginal,
  sampleAgent,
} from './agent-client';
import { ResultRows, PaymentPanel } from './AgentResults';
import { billingDataSchema } from './billing-schema';
import { originalRequiresRecovery, type PaymentOriginal } from './payment-schema';
import payment from './payment-backend.fixture.json';
import processing from './payment-processing.fixture.json';
import mixed from './billing-mixed.fixture.json';
import empty from './billing-empty.fixture.json';
import partial from './billing-page-failed.fixture.json';
import refund from './billing-unknown-refund.fixture.json';

const selection = { account_id: null, payee_query: null, payee_id: null };
function prepared() {
  const v = agentSchema.parse(payment.prepared);
  if (v.kind !== 'PAYMENT') throw new Error('Wrong captured fixture');
  return v;
}
function reply(value: unknown) {
  return new Response(JSON.stringify(value), { status: 200 });
}
afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe('actual backend payment contract', () => {
  it('accepts saved prepared/waiting DTOs and binds fixed payment to an existing draft only', async () => {
    const p = prepared();
    expect(agentSchema.parse(payment.waiting).kind).toBe('PAYMENT');
    const fetch = vi.fn().mockResolvedValue(reply(payment.prepared));
    vi.stubGlobal('fetch', fetch);
    await sampleAgent('EXECUTE_TRANSFER', selection, p.draft);
    const body = JSON.parse(fetch.mock.calls[0][1].body);
    expect(body).toEqual({
      sample: 'EXECUTE_TRANSFER',
      selection,
      context: {
        task_id: p.draft.task_id,
        task_revision: p.draft.task_revision,
        plan_version: p.draft.plan_version,
        facts_version: p.draft.facts_version,
        selection_ticket: p.draft.selection_ticket,
      },
    });
    await expect(sampleAgent('EXECUTE_TRANSFER', selection, null)).rejects.toMatchObject({
      reason: 'INVALID_RESPONSE',
    });
    expect(fetch).toHaveBeenCalledTimes(1);
  });
  it.each([
    'https://other.invalid/confirm',
    '//other.invalid/confirm',
    '/api/sim/payment-preparations/other/other/confirm',
    `${payment.prepared.payment_flow.next_path}?redirect=1`,
  ])('rejects mismatched or credential leaking flow paths: %s', (path) => {
    const raw = structuredClone(payment.prepared);
    raw.payment_flow.next_path = path;
    expect(agentSchema.safeParse(raw).success).toBe(false);
  });
  it('rejects swapped IDs, snapshots, flows, capabilities and unknown payment fields', () => {
    const mutations = [
      (r: typeof payment.prepared) => {
        r.payment.task_id = 'other';
      },
      (r: typeof payment.prepared) => {
        r.payment.snapshot.display_details.amount = '300.00';
      },
      (r: typeof payment.prepared) => {
        r.payment.agent_binding.confirmation_ref = 'other';
      },
      (r: typeof payment.prepared) => {
        r.payment.agent_binding.locator.draft_version = 99;
      },
      (r: typeof payment.prepared) => {
        r.payment_flow.status = 'WAITING_EXECUTION_CONDITIONS';
      },
      (r: typeof payment.prepared) => {
        r.payment_flow.funds_execution_enabled = true;
      },
      (r: typeof payment.prepared) => {
        r.payment_flow.automatic_retry = true;
      },
      (r: typeof payment.prepared) => {
        Object.assign(r.payment, { authorized: true });
      },
    ];
    for (const mutate of mutations) {
      const raw = structuredClone(payment.prepared);
      mutate(raw);
      expect(agentSchema.safeParse(raw).success).toBe(false);
    }
  });
  it('makes a separate explicit confirmation, then GETs the saved flow without executing funds', async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date(payment.prepared.payment.prepared_at));
    const p = prepared();
    const fetch = vi
      .fn()
      .mockResolvedValueOnce(reply(payment.waiting.payment))
      .mockResolvedValueOnce(reply(payment.waiting));
    vi.stubGlobal('fetch', fetch);
    const result = await actPayment(p, 'CONFIRM');
    expect(result.kind).toBe('PAYMENT');
    expect(fetch.mock.calls[0][0]).toBe(`${p.payment_flow.next_path}`);
    expect(JSON.parse(fetch.mock.calls[0][1].body)).toEqual({
      contract_version: 1,
      scope: 'PAYMENT_INTENT_ONLY',
      confirmation_ref: p.payment.confirmation_ref,
      display_ticket: p.payment.display_ticket,
      action: 'CONFIRM',
    });
    expect(fetch.mock.calls[1][0]).toBe(`/api/agent/tasks/${p.payment.task_id}`);
    expect(fetch.mock.calls[1][1].method).toBe('GET');
    expect(fetch).toHaveBeenCalledTimes(2);
  });
  it('requires confirmed current payment for evaluation and rejects expired/rejected/recovery writes', async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date(payment.prepared.payment.prepared_at));
    const p = prepared();
    const fetch = vi.fn();
    vi.stubGlobal('fetch', fetch);
    await expect(actPayment(p, 'EVALUATE')).rejects.toMatchObject({ reason: 'INVALID_RESPONSE' });
    vi.setSystemTime(new Date(p.payment.expires_at));
    await expect(actPayment(p, 'CONFIRM')).rejects.toMatchObject({ reason: 'INVALID_RESPONSE' });
    vi.setSystemTime(new Date(p.payment.prepared_at));
    const rejected = structuredClone(payment.prepared);
    Object.assign(rejected.payment, {
      invalid_reason: 'REJECTED',
      state: 'REJECTED',
      confirmation_status: 'REVOKED',
    });
    Object.assign(rejected.payment_flow, {
      status: 'ENDED_INVALIDATED',
      next_action: 'REVIEW_CURRENT_DRAFT',
      next_path: `/api/agent/tasks/${p.payment.task_id}`,
    });
    const r = agentSchema.parse(rejected);
    if (r.kind !== 'PAYMENT') throw new Error();
    await expect(actPayment(r, 'CONFIRM')).rejects.toMatchObject({ reason: 'INVALID_RESPONSE' });
    expect(fetch).not.toHaveBeenCalled();
  });
  it('does not retry a lost confirmation response; recovery and original query are GET only', async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date(payment.prepared.payment.prepared_at));
    const p = prepared();
    const fetch = vi
      .fn()
      .mockRejectedValueOnce(new Error('lost'))
      .mockResolvedValueOnce(reply(payment.waiting))
      .mockResolvedValueOnce(reply(payment.original));
    vi.stubGlobal('fetch', fetch);
    await expect(actPayment(p, 'CONFIRM')).rejects.toMatchObject({ reason: 'UNAVAILABLE' });
    expect(fetch).toHaveBeenCalledTimes(1);
    await getAgent(p.payment.task_id);
    expect((await queryPaymentOriginal(p)).state).toBe('UNSUBMITTED');
    expect(fetch.mock.calls.map((c) => c[1].method)).toEqual(['POST', 'GET', 'GET']);
    const wrong = structuredClone(payment.original);
    wrong.binding.business_operation_id = 'other';
    fetch.mockResolvedValueOnce(reply(wrong));
    await expect(queryPaymentOriginal(p)).rejects.toMatchObject({ reason: 'INVALID_RESPONSE' });
  });
  it('renders separate payment intention and never offers debit or payment success', () => {
    const html = renderToStaticMarkup(
      createElement(PaymentPanel, {
        view: prepared(),
        original: null,
        enabled: true,
        fresh: true,
        expired: false,
        onAction() {},
        onOriginal() {},
        onRefresh() {},
      }),
    );
    expect(html).toContain('确认付款信息');
    expect(html).toContain('付款暂不可用');
    expect(html).not.toContain('尚未扣款');
    expect(html).toContain('工资卡');
    expect(html).toContain('**** 1024');
    expect(html).not.toContain('付款成功');
  });
  it('preserves a real processing business result as reconcile-required without execution authority', async () => {
    const p = agentSchema.parse(processing);
    if (p.kind !== 'PAYMENT') throw new Error();
    expect(p.payment_flow.status).toBe('QUERY_ORIGINAL');
    const fetch = vi.fn();
    vi.stubGlobal('fetch', fetch);
    await expect(actPayment(p, 'CONFIRM')).rejects.toMatchObject({ reason: 'INVALID_RESPONSE' });
    await expect(actPayment(p, 'EVALUATE')).rejects.toMatchObject({ reason: 'INVALID_RESPONSE' });
    expect(fetch).not.toHaveBeenCalled();
  });
  it.each([
    ['PROCESSING', '原单处理中'],
    ['SUCCEEDED', '原单成功（已核验）'],
    ['FAILED', '原单失败（已核验）'],
    ['RECONCILE_REQUIRED', '原单结果待核对'],
    ['SOURCE_CONFLICT', '原单来源冲突'],
  ] as const)('displays %s without old confirmation or contrary funds wording', (state, label) => {
    // Component state variants only; this does not assert a real settlement occurred.
    const original = { ...processing.payment_flow.original, state } as PaymentOriginal;
    for (const raw of [payment.prepared, payment.waiting]) {
      const view = agentSchema.parse(raw);
      if (view.kind !== 'PAYMENT') throw new Error();
      const html = renderToStaticMarkup(
        createElement(PaymentPanel, {
          view,
          original,
          enabled: true,
          fresh: true,
          expired: false,
          onAction() {},
          onOriginal() {},
          onRefresh() {},
        }),
      );
      expect(html).toContain('已有原单 · 仅查询恢复');
      expect(html).toContain(label);
      expect(html).toContain('请保留原号查询付款结果');
      for (const stale of ['尚未扣款', '确认付款信息', '拒绝付款', '重新评估付款条件'])
        expect(html).not.toContain(stale);
    }
  });
  it('keeps unsubmitted/internal registration editable but guards observed business evidence', () => {
    const original = payment.original as PaymentOriginal;
    expect(originalRequiresRecovery(original)).toBe(false);
    expect(originalRequiresRecovery({ ...original, state: 'REGISTERED_UNSUBMITTED' })).toBe(false);
    expect(
      originalRequiresRecovery({
        ...original,
        verified_business_result: processing.payment_flow.original
          .verified_business_result as PaymentOriginal['verified_business_result'],
      }),
    ).toBe(true);
    expect(originalRequiresRecovery(null)).toBe(false);
  });
});
describe('member reports captured through actual HTTP and PG', () => {
  it.each([mixed, empty, partial, refund])(
    'parses the full strict saved report, preserving history without execution',
    async (raw) => {
      const v = agentSchema.parse(raw);
      if (v.kind !== 'READ') throw new Error();
      const fetch = vi.fn().mockResolvedValue(reply(raw));
      vi.stubGlobal('fetch', fetch);
      expect(await getAgent(v.task.task_id)).toEqual(v);
      expect(fetch.mock.calls[0][1].method).toBe('GET');
      expect(v.can_start).toBe(false);
    },
  );
  it('does not convert partial/null to zero or complete; full empty scope may display zero', () => {
    const render = (raw: unknown) => {
      const v = agentSchema.parse(raw);
      if (v.kind !== 'READ' || !v.results?.[0]) throw new Error();
      return renderToStaticMarkup(createElement(ResultRows, { result: v.results[0] }));
    };
    expect(render(partial)).toContain('暂不可用');
    expect(render(partial)).toContain('READ_BLOCKED');
    expect(render(partial)).not.toContain('已取全');
    expect(render(partial)).not.toContain('0.00');
    expect(render(empty)).toContain('0.00');
    expect(render(mixed)).toContain('60.00');
    expect(render(mixed)).toContain('5.00');
    expect(render(mixed)).toContain('退款 · 单独列示');
    expect(render(refund)).toContain('未提供 · 待核对');
  });
  it('rejects made-up report fields and false completeness', () => {
    const raw = structuredClone(mixed.results[0].data);
    Object.assign(raw.report, { net_amount: '55.00' });
    expect(billingDataSchema.safeParse(raw).success).toBe(false);
    const wrong = structuredClone(partial.results[0].data);
    wrong.report.calculation_status = 'READY';
    expect(billingDataSchema.safeParse(wrong).success).toBe(false);
  });
});
