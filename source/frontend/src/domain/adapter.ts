import { accounts, payees, makeFixtures } from './fixtures';
import {
  draftStatus,
  normalizeIntentAmount,
  parseMoney,
  stateSchema,
  monthKey,
  billSummary,
  formatMoney,
} from './model';
import type { DemoState, Draft, LocalRecord, Message } from './model';
import { assistantCopy } from './assistant-copy';

export const STORAGE_KEY = 'safeflow.demo.v1';
export const DRAFT_TTL_MS = 30 * 60 * 1000;
export interface StoragePort {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}
export interface DraftInput {
  accountId: string;
  payeeId: string;
  amount: string;
  note: string;
}
const emptyState = (): DemoState => ({
  schemaVersion: 1,
  userId: 'user_001',
  drafts: [],
  feedback: [],
  subscriptionPrefs: {},
  records: [],
  messages: [],
});
const id = (kind: string) => `local_${kind}_${crypto.randomUUID()}`;

export function restoreState(
  raw: string | null,
  now = new Date(),
): { state: DemoState; recovered: boolean } {
  if (!raw) return { state: emptyState(), recovered: false };
  try {
    const state = stateSchema.parse(JSON.parse(raw));
    const fixtures = makeFixtures(now);
    const unique = (values: string[]) => new Set(values).size === values.length;
    if (
      !unique(state.drafts.map((d) => d.id)) ||
      !unique(state.records.map((r) => r.id)) ||
      !unique(state.messages.map((m) => m.id)) ||
      !unique(state.feedback.map((f) => f.transactionId))
    )
      throw new Error('重复记录');
    for (const draft of state.drafts) {
      if (
        !accounts.some((a) => a.id === draft.accountId) ||
        !payees.some((p) => p.id === draft.payeeId) ||
        parseMoney(draft.amount) <= 0n ||
        parseMoney(draft.amount) >
          parseMoney(accounts.find((a) => a.id === draft.accountId)!.balance)
      )
        throw new Error('非法草案');
      if (
        Date.parse(draft.updatedAt) < Date.parse(draft.createdAt) ||
        Date.parse(draft.expiresAt) <= Date.parse(draft.updatedAt)
      )
        throw new Error('非法时间');
    }
    if (
      state.feedback.some((f) => !fixtures.transactions.some((t) => t.id === f.transactionId)) ||
      Object.keys(state.subscriptionPrefs).some(
        (key) => !fixtures.subscriptions.some((s) => s.id === key),
      )
    )
      throw new Error('未知样例引用');
    return { state, recovered: false };
  } catch {
    return { state: emptyState(), recovered: true };
  }
}

// This port represents a local UI demo. It issues no permissions and calls no bank/Runtime API.
export interface WorkbenchAdapter {
  getSnapshot(): DemoState;
  subscribe(listener: () => void): () => void;
  createDraft(input: DraftInput): Draft;
  updateDraft(draftId: string, version: number, input: DraftInput): Draft;
  cancelDraft(draftId: string, version: number): void;
  registerFeedback(transactionId: string, reason: string): void;
  setReminder(subscriptionId: string, enabled: boolean): void;
  requestSubscriptionCancel(subscriptionId: string): void;
  sendMessage(text: string): Message;
  clearMessages(): void;
  recordQuery(title: string, detail: string): void;
}

export class LocalDemoAdapter implements WorkbenchAdapter {
  private state: DemoState;
  private listeners = new Set<() => void>();
  readonly recovered: boolean;
  readonly storageUnavailable: boolean;
  constructor(
    private storage: StoragePort,
    private clock: () => Date = () => new Date(),
  ) {
    let raw: string | null = null;
    let unavailable = false;
    try {
      raw = storage.getItem(STORAGE_KEY);
    } catch {
      unavailable = true;
    }
    const restored = restoreState(raw, clock());
    this.state = restored.state;
    this.recovered = restored.recovered;
    this.storageUnavailable = unavailable;
  }
  getSnapshot = () => this.state;
  subscribe = (listener: () => void) => {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  };
  private save(next: DemoState) {
    try {
      this.storage.setItem(STORAGE_KEY, JSON.stringify(next));
    } catch {
      throw new Error('浏览器无法保存数据，本次操作未保存。请检查本地存储权限或可用空间。');
    }
    this.state = next;
    this.listeners.forEach((listener) => listener());
  }
  private record(
    title: string,
    detail: string,
    category: LocalRecord['category'],
    reference = '',
    task: LocalRecord['task'] = 'COMPLETED',
    operation: LocalRecord['operation'] = null,
    security: LocalRecord['security'] = 'ALLOW',
  ): LocalRecord {
    return {
      id: id('record'),
      at: this.clock().toISOString(),
      title,
      detail,
      category,
      reference,
      task,
      operation,
      security,
    };
  }
  private withRecord(next: DemoState, record: LocalRecord) {
    return { ...next, records: [record, ...next.records].slice(0, 200) };
  }
  private validateInput(input: DraftInput) {
    const account = accounts.find((a) => a.id === input.accountId && a.userId === 'user_001');
    if (!account) throw new Error('请选择本演示用户的付款账户');
    if (!payees.some((p) => p.id === input.payeeId)) throw new Error('请选择明确的已有收款人');
    const amount = parseMoney(input.amount);
    if (amount <= 0n) throw new Error('金额必须大于 0.00');
    if (amount > parseMoney(account.balance)) throw new Error('草案金额超过所选账户的模拟可用余额');
    if (input.note.length > 100) throw new Error('备注最多 100 个字符');
  }
  private editable(draftId: string, version: number) {
    const draft = this.state.drafts.find((d) => d.id === draftId);
    if (!draft) throw new Error('草案不存在');
    if (draft.version !== version) throw new Error('草案内容已更新，请重新打开最新草案');
    if (draftStatus(draft, this.clock().getTime()) !== 'DRAFT')
      throw new Error('已取消或已到期的草案不可继续操作');
    return draft;
  }
  createDraft(input: DraftInput) {
    this.validateInput(input);
    if (this.state.drafts.length >= 100) throw new Error('本地草案已达到 100 条容量上限');
    const now = this.clock();
    const draft: Draft = {
      ...input,
      id: id('draft'),
      userId: 'user_001',
      currency: 'CNY',
      version: 1,
      state: 'DRAFT',
      createdAt: now.toISOString(),
      updatedAt: now.toISOString(),
      expiresAt: new Date(now.getTime() + DRAFT_TTL_MS).toISOString(),
    };
    this.save(
      this.withRecord(
        { ...this.state, drafts: [draft, ...this.state.drafts] },
        this.record(
          '保存转账草案',
          `${payees.find((p) => p.id === input.payeeId)!.name} · ¥${input.amount} · 首次保存，仅保存在本机，未执行转账。`,
          'draft',
          draft.id,
          'WAITING_USER',
          'DRAFT',
          'READY_TO_CONFIRM',
        ),
      ),
    );
    return draft;
  }
  updateDraft(draftId: string, version: number, input: DraftInput) {
    const original = this.editable(draftId, version);
    this.validateInput(input);
    const now = this.clock();
    const draft: Draft = {
      ...original,
      ...input,
      version: original.version + 1,
      updatedAt: now.toISOString(),
      expiresAt: new Date(now.getTime() + DRAFT_TTL_MS).toISOString(),
    };
    this.save(
      this.withRecord(
        { ...this.state, drafts: this.state.drafts.map((d) => (d.id === draftId ? draft : d)) },
        this.record(
          '修改转账草案',
          `第${draft.version - 1}次修改 · ¥${draft.amount}，仍未执行转账。`,
          'draft',
          draft.id,
          'WAITING_USER',
          'DRAFT',
          'READY_TO_CONFIRM',
        ),
      ),
    );
    return draft;
  }
  cancelDraft(draftId: string, version: number) {
    const draft = this.editable(draftId, version);
    this.save(
      this.withRecord(
        {
          ...this.state,
          drafts: this.state.drafts.map((d) =>
            d.id === draftId
              ? { ...d, state: 'CANCELLED', updatedAt: this.clock().toISOString() }
              : d,
          ),
        },
        this.record(
          '取消本地草案',
          `¥${draft.amount} 的本地草案已取消，没有资金操作。`,
          'draft',
          draftId,
          'CANCELLED',
          'CANCELLED',
          'BLOCKED',
        ),
      ),
    );
  }
  registerFeedback(transactionId: string, reason: string) {
    const transaction = makeFixtures(this.clock()).transactions.find((t) => t.id === transactionId);
    if (!transaction) throw new Error('未找到该样例流水');
    if (!reason.trim() || reason.trim().length > 300)
      throw new Error('请填写 1 至 300 字的反馈原因');
    if (this.state.feedback.some((f) => f.transactionId === transactionId))
      throw new Error('该流水已登记本地反馈');
    if (this.state.feedback.length >= 100) throw new Error('本地反馈容量已满');
    const feedback = { transactionId, reason: reason.trim(), at: this.clock().toISOString() };
    this.save(
      this.withRecord(
        { ...this.state, feedback: [feedback, ...this.state.feedback] },
        this.record(
          '登记账单反馈',
          `${transaction.title}：${feedback.reason}。仅本地登记，未提交银行，也未锁卡。`,
          'feedback',
          transactionId,
        ),
      ),
    );
  }
  setReminder(subscriptionId: string, enabled: boolean) {
    const subscription = makeFixtures(this.clock()).subscriptions.find(
      (s) => s.id === subscriptionId,
    );
    if (!subscription) throw new Error('订阅不存在');
    const prefs = this.state.subscriptionPrefs[subscriptionId] ?? {
      reminder: true,
      cancelRequested: false,
    };
    if (prefs.reminder === enabled) return;
    this.save(
      this.withRecord(
        {
          ...this.state,
          subscriptionPrefs: {
            ...this.state.subscriptionPrefs,
            [subscriptionId]: { ...prefs, reminder: enabled },
          },
        },
        this.record(
          `${enabled ? '开启' : '关闭'}订阅提醒`,
          `${subscription.name}的订阅提醒已${enabled ? '开启' : '关闭'}，仅在本机展示，不发送外部通知。`,
          'subscription',
          subscriptionId,
        ),
      ),
    );
  }
  requestSubscriptionCancel(subscriptionId: string) {
    const subscription = makeFixtures(this.clock()).subscriptions.find(
      (s) => s.id === subscriptionId,
    );
    if (!subscription) throw new Error('订阅不存在');
    const prefs = this.state.subscriptionPrefs[subscriptionId] ?? {
      reminder: true,
      cancelRequested: false,
    };
    if (prefs.cancelRequested) throw new Error('该订阅的取消意图已登记');
    this.save(
      this.withRecord(
        {
          ...this.state,
          subscriptionPrefs: {
            ...this.state.subscriptionPrefs,
            [subscriptionId]: { ...prefs, cancelRequested: true },
          },
        },
        this.record(
          '登记订阅取消意图',
          `${subscription.name}：已本地登记，商户订阅仍有效，请自行前往商户处理。`,
          'subscription',
          subscriptionId,
          'WAITING_USER',
          null,
          'NEEDS_INPUT',
        ),
      ),
    );
  }
  recordQuery(title: string, detail: string) {
    this.save(this.withRecord(this.state, this.record(title, detail, 'query')));
  }
  sendMessage(text: string) {
    const input = text.trim();
    if (!input || input.length > 1000) throw new Error('请输入 1 至 1000 字的内容');
    const now = this.clock().toISOString();
    const reply: Message = { id: id('message'), role: 'assistant', at: now, text: '' };
    // Deliberately finite intents; unmatched or ambiguous input requires user selection.
    if (/^(?:请|帮我)?(?:查(?:询)?|查看|看看)?(?:我的)?(?:账户|工资卡)?余额[。？?]?$/.test(input)) {
      reply.text = assistantCopy.balance;
      reply.action = 'overview';
    } else if (
      /^(?:请|帮我)?(?:查(?:询)?|查看|看看)?(?:我的)?(?:本月|这个月)?(?:账单|消费|支出|花了多少)[。？?]?$/.test(
        input,
      )
    ) {
      const rows = makeFixtures(this.clock()).transactions.filter(
        (t) => monthKey(new Date(t.date)) === monthKey(this.clock()),
      );
      reply.text = assistantCopy.bills(
        formatMoney(billSummary(rows).expense),
        rows.filter((t) => t.kind === 'EXPENSE').length,
      );
      reply.action = 'bills';
    } else if (/转|汇|草案/.test(input)) {
      const match = input.match(
        /^(?:请|帮我)?\s*给(小李|陈晨)\s*(?:转(?:账)?|生成|准备)\s*(\d+(?:\.\d{1,2})?)\s*元(?:的?(?:本地|模拟|转账)?草案)?[。！]?$/,
      );
      reply.action = 'transfers';
      if (!match) {
        reply.text = assistantCopy.incompleteTransfer;
      } else {
        try {
          const amount = normalizeIntentAmount(match[2]);
          if (parseMoney(amount) <= 0n) throw new Error('金额须大于零');
          reply.amount = amount;
          reply.payeeQuery = match[1];
          const matches = payees.filter((p) => p.name === match[1]);
          reply.text = assistantCopy.transfer(match[1], amount, matches.length);
        } catch {
          reply.text = assistantCopy.invalidTransfer;
        }
      }
    } else {
      reply.text = assistantCopy.unsupported;
    }
    const user: Message = { id: id('message'), role: 'user', text: input, at: now };
    this.save(
      this.withRecord(
        { ...this.state, messages: [...this.state.messages, user, reply].slice(-100) },
        this.record(
          '智能助手咨询',
          reply.text,
          'query',
          '',
          reply.action === 'transfers' ? 'WAITING_USER' : 'COMPLETED',
          null,
          reply.action === 'transfers' ? 'NEEDS_INPUT' : 'ALLOW',
        ),
      ),
    );
    return reply;
  }
  clearMessages() {
    this.save({ ...this.state, messages: [] });
  }
}
