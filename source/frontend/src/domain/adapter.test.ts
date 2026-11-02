import { beforeEach, describe, expect, it } from 'vitest';
import { DRAFT_TTL_MS, LocalDemoAdapter, restoreState, STORAGE_KEY } from './adapter';
import type { DraftInput, StoragePort } from './adapter';
import { billSummary, draftStatus, moneyFromCents, parseMoney } from './model';
import { makeFixtures } from './fixtures';

class MemoryStorage implements StoragePort {
  values = new Map<string, string>();
  fail = false;
  getItem(key: string) {
    return this.values.get(key) ?? null;
  }
  setItem(key: string, value: string) {
    if (this.fail) throw new Error('Quota exceeded');
    this.values.set(key, value);
  }
}
let storage: MemoryStorage;
let adapter: LocalDemoAdapter;
let now: Date;
const input: DraftInput = {
  accountId: 'account_001',
  payeeId: 'payee_008',
  amount: '200.00',
  note: '聚餐',
};
beforeEach(() => {
  storage = new MemoryStorage();
  now = new Date('2026-10-04T08:00:00Z');
  adapter = new LocalDemoAdapter(storage, () => now);
});

describe('integer money and ledger semantics', () => {
  it('adds financial amounts exactly without floating point rounding', () => {
    expect(moneyFromCents(parseMoney('0.10') + parseMoney('0.20'))).toBe('0.30');
    expect(parseMoney('999999999999.99')).toBe(99999999999999n);
  });
  it.each([
    '200',
    '2.1',
    '-1.00',
    '1e2',
    '1.001',
    'NaN',
    'Infinity',
    ' 2.00',
    '02.00',
    '1000000000000.00',
  ])('rejects non-canonical money %s', (value) => expect(() => parseMoney(value)).toThrow());
  it('separates transfers from consumption and income', () => {
    const rows = makeFixtures(now).transactions;
    const transfer = rows.find((r) => r.kind === 'TRANSFER')!;
    const expense = rows.find((r) => r.kind === 'EXPENSE')!;
    expect(billSummary([transfer, expense])).toEqual({
      expense: expense.amount,
      income: '0.00',
      transfer: '200.00',
      count: 2,
    });
  });
});
describe('local draft lifecycle', () => {
  it('saves and reloads a bound five-field versioned draft without altering bank fixtures', () => {
    const draft = adapter.createDraft(input);
    expect(new LocalDemoAdapter(storage).getSnapshot().drafts[0]).toEqual(draft);
    expect(draft).toMatchObject({ ...input, userId: 'user_001', currency: 'CNY', version: 1 });
    expect(adapter.getSnapshot().records[0]).toMatchObject({
      task: 'WAITING_USER',
      operation: 'DRAFT',
      security: 'READY_TO_CONFIRM',
    });
    expect(makeFixtures(now).transactions).toHaveLength(11);
  });
  it('increments version and rejects stale editors', () => {
    const draft = adapter.createDraft(input);
    const edited = adapter.updateDraft(draft.id, 1, { ...input, amount: '300.00' });
    expect(edited.version).toBe(2);
    expect(edited.amount).toBe('300.00');
    expect(() => adapter.updateDraft(draft.id, 1, input)).toThrow('内容已更新');
    expect(() => adapter.cancelDraft(draft.id, 1)).toThrow('内容已更新');
    expect(adapter.getSnapshot().records).toHaveLength(2);
  });
  it('cancels without allowing subsequent changes', () => {
    const draft = adapter.createDraft(input);
    adapter.cancelDraft(draft.id, 1);
    expect(draftStatus(adapter.getSnapshot().drafts[0], now.getTime())).toBe('CANCELLED');
    expect(() => adapter.updateDraft(draft.id, 1, input)).toThrow('不可');
    expect(() => adapter.cancelDraft(draft.id, 1)).toThrow('不可');
    expect(adapter.getSnapshot().records[0].operation).toBe('CANCELLED');
  });
  it('expires at the exact TTL boundary and cannot be edited or cancelled', () => {
    const draft = adapter.createDraft(input);
    now = new Date(now.getTime() + DRAFT_TTL_MS);
    expect(draftStatus(draft, now.getTime())).toBe('EXPIRED');
    expect(() => adapter.updateDraft(draft.id, 1, input)).toThrow('到期');
    expect(() => adapter.cancelDraft(draft.id, 1)).toThrow('到期');
  });
  it.each([
    { ...input, payeeId: '' },
    { ...input, payeeId: 'unknown' },
    { ...input, accountId: 'account_002' },
    { ...input, amount: '0.00' },
    { ...input, amount: '1288.01' },
    { ...input, note: 'x'.repeat(101) },
  ])('refuses invalid inputs before saving: %j', (invalid) => {
    expect(() => adapter.createDraft(invalid)).toThrow();
    expect(adapter.getSnapshot().drafts).toHaveLength(0);
    expect(storage.getItem(STORAGE_KEY)).toBeNull();
  });
  it('does not report success or mutate state when storage writes fail', () => {
    const draft = adapter.createDraft(input);
    storage.fail = true;
    expect(() => adapter.updateDraft(draft.id, 1, { ...input, amount: '300.00' })).toThrow(
      '未保存',
    );
    expect(adapter.getSnapshot().drafts[0].amount).toBe('200.00');
    expect(adapter.getSnapshot().records).toHaveLength(1);
  });
});
describe('persistence validation and local side effects', () => {
  it.each([
    ['账户余额', '模拟可用余额', 'overview'],
    ['本月账单', '转账不计入消费', 'bills'],
    ['给小李转 200 元', '2 位同名收款人', 'transfers'],
    ['你好', '这个问题暂不支持', undefined],
  ])(
    'saves the actual %s response in customer language without bank execution',
    (question, expected, action) => {
      const reply = adapter.sendMessage(question!);
      expect(reply.text).toContain(expected);
      expect(reply.text).not.toMatch(/底座|前端|意图/);
      expect(reply.action).toBe(action);
      const restored = new LocalDemoAdapter(storage).getSnapshot();
      expect(restored.messages).toHaveLength(2);
      expect(restored.messages[0].text).toBe(question);
      expect(restored.messages[1]).toEqual(reply);
      expect(restored.records[0]).toMatchObject({ title: '智能助手咨询', detail: reply.text });
      expect(restored.drafts).toHaveLength(0);
    },
  );

  it.each(['{bad', 'null', '{}', '{"schemaVersion":2}', '{"schemaVersion":1,"userId":"other"}'])(
    'recovers corrupted storage: %s',
    (raw) => expect(restoreState(raw).recovered).toBe(true),
  );
  it('rejects unknown and foreign resource references and duplicate ids on restore', () => {
    adapter.createDraft(input);
    const state = adapter.getSnapshot();
    expect(
      restoreState(
        JSON.stringify({ ...state, drafts: [{ ...state.drafts[0], accountId: 'foreign' }] }),
      ).recovered,
    ).toBe(true);
    expect(
      restoreState(JSON.stringify({ ...state, drafts: [...state.drafts, state.drafts[0]] }))
        .recovered,
    ).toBe(true);
    expect(
      restoreState(
        JSON.stringify({ ...state, drafts: [{ ...state.drafts[0], userId: 'user_002' }] }),
      ).recovered,
    ).toBe(true);
  });
  it('tolerates storage reads being unavailable', () => {
    const port: StoragePort = {
      getItem() {
        throw new Error('denied');
      },
      setItem() {
        throw new Error('denied');
      },
    };
    const local = new LocalDemoAdapter(port);
    expect(local.storageUnavailable).toBe(true);
    expect(local.getSnapshot().drafts).toHaveLength(0);
    expect(() => local.createDraft(input)).toThrow('未保存');
  });
  it('registers feedback once and never changes fixture transactions or card status', () => {
    adapter.registerFeedback('demo_tx_001', '非本人消费');
    expect(() => adapter.registerFeedback('demo_tx_001', '重复')).toThrow('已登记');
    expect(adapter.getSnapshot().feedback).toHaveLength(1);
    expect(adapter.getSnapshot().records[0].detail).toContain('未锁卡');
  });
  it('keeps reminder and cancellation intent separate from merchant cancellation', () => {
    adapter.setReminder('demo_sub_music', false);
    adapter.requestSubscriptionCancel('demo_sub_music');
    const restored = new LocalDemoAdapter(storage).getSnapshot();
    expect(restored.subscriptionPrefs.demo_sub_music).toEqual({
      reminder: false,
      cancelRequested: true,
    });
    expect(restored.records[0].detail).toContain('商户订阅仍有效');
  });
  it('keeps same-name candidates unresolved, handles ambiguous intents without a draft', () => {
    const reply = adapter.sendMessage('给小李转 200 元');
    expect(reply.text).toContain('2 位同名');
    expect(reply.amount).toBe('200.00');
    expect(adapter.getSnapshot().drafts).toHaveLength(0);
    expect(adapter.sendMessage('给小李转200元还是300元').amount).toBeUndefined();
    expect(adapter.sendMessage('给小李和陈晨转200元').payeeQuery).toBeUndefined();
  });
  it('preserves untrusted text as text and clearing conversations preserves drafts', () => {
    adapter.createDraft(input);
    adapter.sendMessage('<img src=x onerror=alert(1)>');
    expect(adapter.getSnapshot().messages[0].text).toBe('<img src=x onerror=alert(1)>');
    adapter.clearMessages();
    expect(adapter.getSnapshot().drafts).toHaveLength(1);
  });
  it.each([
    '给小李转-200元',
    '给小李转1e2元',
    '给小李转200.001元',
    '给小李转200元或300元',
    '不要给小李转200元',
    '给小李确认扣款200元',
  ])('does not extract an invented amount from %s', (text) => {
    expect(adapter.sendMessage(text).amount).toBeUndefined();
    expect(adapter.getSnapshot().drafts).toHaveLength(0);
  });
  it('refuses unsupported foreign balance instructions instead of answering with own account', () => {
    expect(adapter.sendMessage('查询user_002的余额').action).toBeUndefined();
  });
});
