import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  observeRequests,
  observeSessionExpired,
  request,
  RuntimeApiError,
  RuntimeTransportError,
} from './client';
import { runtimeErrorText } from './runtime-feedback';
import { RecoveryError } from './billing-recovery-client';

afterEach(() => vi.unstubAllGlobals());
describe('request diagnostics and service-scoped recovery guidance', () => {
  it('records fixed phases and numeric timings without request body, selectors or unsafe header data', async () => {
    const events: unknown[] = [],
      stop = observeRequests((event) => events.push(event));
    vi.stubGlobal(
      'fetch',
      vi
        .fn()
        .mockResolvedValue(
          new Response('{"ok":true}', {
            headers: {
              'Content-Type': 'application/json',
              'Server-Timing': 'model;dur=12.25, endpoint=DO_NOT_RELAY, plan;dur=1.2',
            },
          }),
        ),
    );
    try {
      await request('/api/bank/conversation', 'POST', { message: 'PRIVATE_TEXT', code: '123456' });
      const json = JSON.stringify(events);
      expect(json).toContain('model;dur=12.25, plan;dur=1.2');
      expect(json).not.toMatch(/PRIVATE_TEXT|123456|DO_NOT_RELAY|\/api\//);
      expect(events).toHaveLength(2);
    } finally {
      stop();
    }
  });
  it('notifies expired login before rejecting even a non-JSON 401', async () => {
    let expired = 0;
    const stop = observeSessionExpired(() => expired++);
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(new Response('upstream html', { status: 401 })),
    );
    try {
      await expect(request('/api/bank/overview', 'GET')).rejects.toMatchObject({
        status: 401,
        reason: 'NOT_AUTHENTICATED',
      });
      expect(expired).toBe(1);
    } finally {
      stop();
    }
  });
  it('never suggests payment retries in subscription and feedback errors', () => {
    const error = new RuntimeApiError('DEPENDENCY_UNAVAILABLE', null, 'NONE', 503);
    expect(runtimeErrorText(error, 'subscriptions')).toContain('订阅办理结果');
    expect(runtimeErrorText(error, 'subscriptions')).not.toContain('付款');
    expect(runtimeErrorText(error, 'bills')).toContain('原反馈');
    expect(runtimeErrorText(error, 'transfer')).toContain('不要重新付款');
  });
  it('separates read timeout, rejected model plan, domain permission and saved-feedback recovery', () => {
    expect(runtimeErrorText(new RuntimeTransportError('TIMEOUT'), 'agent')).toContain('超时');
    expect(
      runtimeErrorText(new RuntimeApiError('LLM_PROPOSAL_REJECTED', null, 'NONE', 422), 'agent'),
    ).toContain('没有据此执行业务');
    expect(
      runtimeErrorText(new RuntimeApiError('FORBIDDEN', null, 'NONE', 403), 'cards'),
    ).toContain('不等于服务故障');
    expect(runtimeErrorText(new RecoveryError(409, 'RECONCILE_REQUIRED'), 'bills')).toContain(
      '按原反馈编号查询',
    );
  });
  it('does not expose raw schema dumps as business error text', () => {
    expect(
      runtimeErrorText(new Error('[{"code":"custom","secret":"PRIVATE"}]'), 'transfer'),
    ).not.toContain('PRIVATE');
  });
});
