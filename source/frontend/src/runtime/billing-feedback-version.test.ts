import { afterEach, describe, expect, it, vi } from 'vitest';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { agentSchema, getAgent } from './agent-client';
import { billingDataSchema } from './billing-schema';
import { ResultRows } from './AgentResults';
// Captured from this delivery's isolated REAL HTTP report -> feedback flow.
// Original member JSON is unchanged; this is the public HTTP response body.
import current from './billing-feedback-http-8ff.fixture.json';
import previous from './billing-duplicate-candidate.fixture.json';
import v1 from './billing-mixed.fixture.json';

afterEach(() => vi.unstubAllGlobals());
describe('adopted 8ff8c665 HTTP report compatibility', () => {
  it.each([current, previous, v1])(
    'reads current and original V1/V2 history with GET only',
    async (raw) => {
      const view = agentSchema.parse(raw);
      if (view.kind !== 'READ' || !view.results?.[0]) throw Error('Expected stored report');
      const fetch = vi.fn().mockResolvedValue(new Response(JSON.stringify(raw), { status: 200 }));
      vi.stubGlobal('fetch', fetch);
      expect(await getAgent(view.task.task_id)).toEqual(view);
      expect(fetch).toHaveBeenCalledExactlyOnceWith(
        `/api/agent/tasks/${view.task.task_id}`,
        expect.objectContaining({ method: 'GET' }),
      );
      const html = renderToStaticMarkup(createElement(ResultRows, { result: view.results[0] }));
      expect(html).toContain('消费');
      expect(html).not.toContain('<button');
    },
  );
  it('preserves the captured original report, and rejects every unadopted member revision', () => {
    const original = current.results[0].data;
    const parsed = billingDataSchema.parse(original);
    expect(parsed).toEqual(original);
    if (parsed.adapter_contract !== 'LOCAL_BILLING_REPORT_ADAPTER_V2') throw Error();
    expect(parsed.member_revision).toBe('8ff8c6657d724d4f69e7a3d69266b12ec148e0ea');
    for (const revision of ['', 'unknown', '8ff8c665', `${parsed.member_revision}x`]) {
      expect(billingDataSchema.safeParse({ ...original, member_revision: revision }).success).toBe(
        false,
      );
    }
    const swapped = structuredClone(original);
    if ('Ok' in swapped.duplicate_candidates)
      swapped.duplicate_candidates.Ok.report_id = 'other-report';
    expect(billingDataSchema.safeParse(swapped).success).toBe(false);
  });
});
