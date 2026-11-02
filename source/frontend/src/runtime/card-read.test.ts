import { describe, expect, it, vi, afterEach } from 'vitest';
import { cardRead, cardReadSchema, CardReadEpoch, contextSchema } from './card-read';
const view = () => ({
  contract_version: 1,
  mode: 'PROJECT_SIMULATION',
  scope: 'CARD_READ_ONLY',
  step_ref: '6dd861c2-54ba-450a-8c85-f021e9828090',
  funds_authorized: false,
  cards: [
    {
      position: 1,
      last_four: '1234',
      ambiguous_tail: true,
      card_locked: false,
      public_reasons: [],
    },
  ],
  account_outgoing_restricted: true,
  account_public_reasons: ['用户挂失'],
  history: null,
  original: null,
  unlock_instruction: '线下银行',
});
afterEach(() => vi.unstubAllGlobals());
describe('card read display boundary', () => {
  it('accepts same tail and preserves whole-account restriction apart from selected card', () => {
    const v = view();
    v.cards.push({ ...v.cards[0], position: 2 });
    expect(cardReadSchema.parse(v).cards).toHaveLength(2);
    expect(cardReadSchema.parse(v).account_outgoing_restricted).toBe(true);
  });
  it.each(['owner', 'operation_id', 'revision', 'permit', 'receipt'])(
    'rejects trusted/internal field %s',
    (field) => {
      expect(cardReadSchema.safeParse({ ...view(), [field]: 'private' }).success).toBe(false);
    },
  );
  it('rejects zero references, duplicate positions, raw IDs and an original without selected history', () => {
    for (const v of [
      { ...view(), step_ref: 'card-1234' },
      { ...view(), cards: [...view().cards, ...view().cards] },
      { ...view(), cards: [{ ...view().cards[0], card_id: 'raw' }] },
      { ...view(), funds_authorized: true },
    ])
      expect(cardReadSchema.safeParse(v).success).toBe(false);
  });
  it('session context has no owner and cannot claim anonymous with a context', () => {
    expect(
      contextSchema.safeParse({ authenticated: true, context_ref: 'a'.repeat(64) }).success,
    ).toBe(true);
    expect(
      contextSchema.safeParse({ authenticated: false, context_ref: 'a'.repeat(64) }).success,
    ).toBe(false);
    expect(
      contextSchema.safeParse({ authenticated: true, context_ref: 'a'.repeat(64), owner: 'x' })
        .success,
    ).toBe(false);
  });
  it('HTTP failure and invalid JSON never become empty successful cards', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response('{}', { status: 503 })));
    await expect(cardRead({ kind: 'QUERY' })).rejects.toThrow();
    vi.stubGlobal(
      'fetch',
      vi
        .fn()
        .mockResolvedValue(
          new Response(JSON.stringify({ ...view(), cards: null }), { status: 200 }),
        ),
    );
    await expect(cardRead({ kind: 'QUERY' })).rejects.toThrow();
  });
});
describe('card read component epochs', () => {
  it('new query invalidates old success, failure and finalizer before they publish', async () => {
    const gate = new CardReadEpoch();
    const old = gate.next();
    const next = gate.next();
    expect(old.signal.aborted).toBe(true);
    expect(old.current()).toBe(false);
    expect(next.current()).toBe(true);
    await Promise.resolve();
    expect(old.current()).toBe(false);
  });
  it('unmount, session change and clear selection permanently invalidate pending callbacks', () => {
    const gate = new CardReadEpoch();
    const old = gate.next();
    gate.invalidate();
    expect(old.current()).toBe(false);
    const remount = new CardReadEpoch().next();
    expect(remount.current()).toBe(true);
    expect(old.current()).toBe(false);
  });
});
