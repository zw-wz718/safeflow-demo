import { describe, it, expect, vi, afterEach } from 'vitest';
import {
  subscriptionReadSchema,
  subscriptionRead,
  SubscriptionReadEpoch,
  subscriptionOriginal,
  subscriptionOriginalSchema,
  subscriptionDetails,
} from './subscription-read';
const view = () => ({
  contract_version: 1,
  mode: 'PROJECT_SIMULATION',
  scope: 'SUBSCRIPTION_READ_ONLY',
  status: 'AVAILABLE',
  items: [],
  originals: [],
  pending: [],
  writes_enabled: false,
  notification_sent: false,
  merchant_action_executed: false,
  message: '本次完整只读',
});
afterEach(() => vi.unstubAllGlobals());
describe('subscription projection and lifecycle', () => {
  it('only forwards the opaque choice or original number and rejects private original results', async () => {
    const id = crypto.randomUUID();
    const fetch = vi.fn(async (_input: unknown, _init?: RequestInit) => ({
      ok: true,
      json: async () => ({ fictional: true, status: 'NEEDS_REVIEW', message: '原结果待核对' }),
    }));
    vi.stubGlobal('fetch', fetch);
    expect((await subscriptionOriginal(id)).status).toBe('NEEDS_REVIEW');
    expect(JSON.parse(String(fetch.mock.calls[0][1]?.body))).toEqual({
      kind: 'ORIGINAL',
      original_ref: id,
    });
    expect(
      subscriptionOriginalSchema.safeParse({
        fictional: true,
        status: 'OK',
        message: 'x',
        original_binding: {},
      }).success,
    ).toBe(false);
    await expect(subscriptionDetails(id)).rejects.toThrow();
    expect(JSON.parse(String(fetch.mock.calls[1][1]?.body))).toEqual({
      kind: 'DETAILS',
      selection_ref: id,
    });
  });
  it('distinguishes missing owner ledger from a known empty saved ledger', () => {
    expect(subscriptionReadSchema.parse(view()).status).toBe('AVAILABLE');
    expect(subscriptionReadSchema.parse({ ...view(), status: 'NO_LEDGER' }).status).toBe(
      'NO_LEDGER',
    );
    expect(
      subscriptionReadSchema.safeParse({
        ...view(),
        status: 'NO_LEDGER',
        originals: [{ original_ref: crypto.randomUUID(), status: 'NEEDS_REVIEW', message: '核对' }],
      }).success,
    ).toBe(false);
  });
  it.each(['owner', 'context', 'journal', 'original_request', 'receipt', 'permit'])(
    'rejects internal field %s',
    (field) => {
      expect(subscriptionReadSchema.safeParse({ ...view(), [field]: {} }).success).toBe(false);
    },
  );
  it('cannot report writes, notification or merchant actions enabled', () => {
    for (const field of ['writes_enabled', 'notification_sent', 'merchant_action_executed'])
      expect(subscriptionReadSchema.safeParse({ ...view(), [field]: true }).success).toBe(false);
  });
  it('rejects a malformed real HTTP payload instead of showing an empty list', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue({ ok: true, json: async () => ({ ...view(), journal: {} }) }),
    );
    await expect(subscriptionRead()).rejects.toThrow();
  });
  it('invalidates every old success or failure after cancel, replacement or unmount', () => {
    const epoch = new SubscriptionReadEpoch(),
      first = epoch.next(),
      next = epoch.next();
    expect(first.current()).toBe(false);
    expect(first.signal.aborted).toBe(true);
    epoch.invalidate();
    expect(next.current()).toBe(false);
    expect(next.signal.aborted).toBe(true);
  });
});
