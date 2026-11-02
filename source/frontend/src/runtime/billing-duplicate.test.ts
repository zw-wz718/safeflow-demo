import { afterEach, describe, expect, it, vi } from 'vitest';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { agentSchema, getAgent } from './agent-client';
import { billingDataSchema } from './billing-schema';
import { ResultRows } from './AgentResults';
import candidate from './billing-duplicate-candidate.fixture.json';
import missing from './billing-duplicate-missing-merchant.fixture.json';
import unavailable from './billing-duplicate-source-unavailable.fixture.json';
import limit from './billing-duplicate-pair-limit.fixture.json';
import member from './billing-report-actions.member.fixture.json';
import legacy from './billing-mixed.fixture.json';

afterEach(() => vi.unstubAllGlobals());
const render = (raw: unknown) => {
  const view = agentSchema.parse(raw);
  if (view.kind !== 'READ' || !view.results?.[0]) throw Error('Expected saved read result');
  return renderToStaticMarkup(createElement(ResultRows, { result: view.results[0] }));
};
describe('d822709 duplicate candidates captured through actual HTTP/PG', () => {
  it.each([candidate, missing, unavailable, limit, legacy])(
    'strictly reads saved history with GET only',
    async (raw) => {
      const view = agentSchema.parse(raw);
      if (view.kind !== 'READ') throw Error();
      const fetch = vi.fn().mockResolvedValue(new Response(JSON.stringify(raw), { status: 200 }));
      vi.stubGlobal('fetch', fetch);
      expect(await getAgent(view.task.task_id)).toEqual(view);
      expect(fetch).toHaveBeenCalledExactlyOnceWith(
        `/api/agent/tasks/${view.task.task_id}`,
        expect.objectContaining({ method: 'GET' }),
      );
      expect(view.can_start).toBe(false);
    },
  );
  it('preserves all original member candidate fields except the public report identifier', () => {
    for (const [index, raw] of [candidate, missing, unavailable].entries()) {
      const data = billingDataSchema.parse(raw.results[0].data);
      if (data.adapter_contract !== 'LOCAL_BILLING_REPORT_ADAPTER_V2') throw Error();
      const actual = data.duplicate_candidates;
      const expected = member.cases[index].duplicates;
      if ('Ok' in actual && expected.Ok) {
        expect(actual.Ok).toEqual({ ...expected.Ok, report_id: data.report.metadata.report_id });
        expect(actual.Ok.snapshot_id).toBe(data.report.read.snapshot_id);
        expect(actual.Ok.source_id).toBe(data.report.read.source_id);
      } else expect(actual).toEqual(expected);
      expect(data.report.capabilities.anomalies.executed).toBe(false);
    }
  });
  it('renders review-only references, and leaves legacy V1 display unchanged', () => {
    const html = render(candidate);
    for (const text of [
      '1 组候选',
      '仅供核对',
      'food-1',
      'food-2',
      'fictional-report-actions',
      'LOCAL_SAME_MERCHANT_CNY_AMOUNT_0_600S_V1',
    ])
      expect(html).toContain(text);
    expect(html).not.toContain('<button');
    expect(html).not.toContain('确认重复扣款');
    expect(render(legacy)).not.toContain('重复消费候选');
  });
  it('does not mistake missing evidence, Err or null pair overflow for no duplicates', () => {
    expect(render(missing)).toContain('商户证据不足');
    expect(render(missing)).toContain('food-1');
    expect(render(unavailable)).toContain('REPORT_NOT_COMPLETE');
    expect(render(unavailable)).toContain('候选判断不可用');
    expect(render(limit)).toContain('超过1000组上限');
    for (const raw of [missing, unavailable, limit])
      expect(render(raw)).not.toContain('本规则未发现');
  });
  it('rejects wrong versions, swapped report/source/snapshot and unbound transaction references', () => {
    const data = candidate.results[0].data;
    const mutations: Array<(r: typeof data) => void> = [
      (r) => {
        r.adapter_contract = 'LOCAL_BILLING_REPORT_ADAPTER_V1';
      },
      (r) => {
        r.member_revision = 'other';
      },
      (r) => {
        r.duplicate_candidates.Ok.report_id = 'other';
      },
      (r) => {
        r.duplicate_candidates.Ok.source_id = 'other';
      },
      (r) => {
        r.duplicate_candidates.Ok.snapshot_id = 'other';
      },
      (r) => {
        r.duplicate_candidates.Ok.pairs[0].second.transaction_id = 'other';
      },
      (r) => {
        r.duplicate_candidates.Ok.pairs[0].second.account_id = 'other';
      },
      (r) => {
        r.duplicate_candidates.Ok.risk_action_authorized = true;
      },
      (r) => {
        Object.assign(r.duplicate_candidates, { Err: 'REPORT_NOT_COMPLETE' });
      },
      (r) => {
        Object.assign(r.duplicate_candidates.Ok, { persisted: true });
      },
      (r) => {
        r.duplicate_candidates.Ok.pairs.push(
          ...Array(1000).fill(r.duplicate_candidates.Ok.pairs[0]),
        );
      },
      (r) => {
        r.duplicate_candidates.Ok.pairs.push(r.duplicate_candidates.Ok.pairs[0]);
      },
    ];
    for (const mutate of mutations) {
      const raw = structuredClone(data);
      mutate(raw);
      expect(billingDataSchema.safeParse(raw).success).toBe(false);
    }
  });
  it('rejects contradictory completeness, fake empty lists, missing member result and incomplete references', () => {
    const absent = structuredClone(candidate.results[0].data);
    delete (absent as Partial<typeof absent>).duplicate_candidates;
    expect(billingDataSchema.safeParse(absent).success).toBe(false);
    const over = structuredClone(limit.results[0].data);
    Object.assign(over.duplicate_candidates.Ok, { pairs: [] });
    expect(billingDataSchema.safeParse(over).success).toBe(false);
    const incomplete = structuredClone(missing.results[0].data);
    incomplete.duplicate_candidates.Ok.state = 'COMPLETE';
    expect(billingDataSchema.safeParse(incomplete).success).toBe(false);
    incomplete.duplicate_candidates.Ok.state = 'MERCHANT_EVIDENCE_INCOMPLETE';
    incomplete.duplicate_candidates.Ok.missing_merchant = [];
    expect(billingDataSchema.safeParse(incomplete).success).toBe(false);
    const failed = structuredClone(unavailable.results[0].data);
    failed.duplicate_candidates.Err = 'NO_DUPLICATES';
    expect(billingDataSchema.safeParse(failed).success).toBe(false);
    const swapped = structuredClone(candidate);
    swapped.results[0].operation_id = 'other';
    expect(agentSchema.safeParse(swapped).success).toBe(false);
  });
});
