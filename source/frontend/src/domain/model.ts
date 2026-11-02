import { z } from 'zod';

export const pages = [
  'overview',
  'agent',
  'transfers',
  'bills',
  'cards',
  'subscriptions',
  'activity',
] as const;
export type Page = (typeof pages)[number];
export type TransactionKind = 'EXPENSE' | 'INCOME' | 'TRANSFER';
export type TaskState =
  'RUNNING' | 'WAITING_USER' | 'WAITING_EXTERNAL' | 'COMPLETED' | 'FAILED' | 'CANCELLED';
export type OperationState =
  'DRAFT' | 'READY' | 'SUBMITTED' | 'PROCESSING' | 'SUCCEEDED' | 'FAILED' | 'CANCELLED';
export type SecurityOutcome =
  'ALLOW' | 'NEEDS_INPUT' | 'READY_TO_CONFIRM' | 'WAITING_AUTH' | 'BLOCKED' | 'READY_TO_EXECUTE';

export interface Account {
  id: string;
  userId: string;
  name: string;
  tail: string;
  balance: string;
  currency: 'CNY';
  source: string;
}
export interface Payee {
  id: string;
  name: string;
  bank: string;
  tail: string;
  initial: string;
  color: string;
}
export interface Transaction {
  id: string;
  title: string;
  category: string;
  kind: TransactionKind;
  amount: string;
  date: string;
  direction: 'in' | 'out';
  mark: string;
  color: string;
  accountId: string;
}
export interface DemoCard {
  id: string;
  accountId: string;
  name: string;
  tail: string;
  network: string;
  type: string;
  status: '正常';
  color: string;
}
export interface Subscription {
  id: string;
  name: string;
  amount: string;
  cycle: string;
  nextDate: string;
  category: string;
  mark: string;
  color: string;
}

const amount = z.string().regex(/^(0|[1-9]\d{0,11})\.\d{2}$/);
const timestamp = z.string().datetime({ offset: true });
const localId = z.string().regex(/^local_[a-z]+_[a-zA-Z0-9-]+$/);
export const draftSchema = z
  .object({
    id: localId,
    userId: z.literal('user_001'),
    accountId: z.string(),
    payeeId: z.string(),
    amount,
    currency: z.literal('CNY'),
    note: z.string().max(100),
    version: z.number().int().positive(),
    createdAt: timestamp,
    updatedAt: timestamp,
    expiresAt: timestamp,
    state: z.enum(['DRAFT', 'CANCELLED']),
  })
  .strict();
export type Draft = z.infer<typeof draftSchema>;
export const recordSchema = z
  .object({
    id: localId,
    at: timestamp,
    title: z.string().max(200),
    detail: z.string().max(1000),
    category: z.enum(['query', 'draft', 'feedback', 'subscription']),
    task: z.enum([
      'RUNNING',
      'WAITING_USER',
      'WAITING_EXTERNAL',
      'COMPLETED',
      'FAILED',
      'CANCELLED',
    ]),
    operation: z
      .enum(['DRAFT', 'READY', 'SUBMITTED', 'PROCESSING', 'SUCCEEDED', 'FAILED', 'CANCELLED'])
      .nullable(),
    security: z.enum([
      'ALLOW',
      'NEEDS_INPUT',
      'READY_TO_CONFIRM',
      'WAITING_AUTH',
      'BLOCKED',
      'READY_TO_EXECUTE',
    ]),
    reference: z.string().max(100),
  })
  .strict();
export type LocalRecord = z.infer<typeof recordSchema>;
export const messageSchema = z
  .object({
    id: localId,
    role: z.enum(['user', 'assistant']),
    text: z.string().max(2000),
    at: timestamp,
    action: z.enum(['overview', 'bills', 'transfers']).optional(),
    amount: amount.optional(),
    payeeQuery: z.string().max(40).optional(),
  })
  .strict();
export type Message = z.infer<typeof messageSchema>;
export const stateSchema = z
  .object({
    schemaVersion: z.literal(1),
    userId: z.literal('user_001'),
    drafts: z.array(draftSchema).max(100),
    feedback: z
      .array(
        z
          .object({ transactionId: z.string(), reason: z.string().min(1).max(300), at: timestamp })
          .strict(),
      )
      .max(100),
    subscriptionPrefs: z.record(
      z.object({ reminder: z.boolean(), cancelRequested: z.boolean() }).strict(),
    ),
    records: z.array(recordSchema).max(200),
    messages: z.array(messageSchema).max(100),
  })
  .strict();
export type DemoState = z.infer<typeof stateSchema>;

export const taskLabels: Record<TaskState, string> = {
  RUNNING: '进行中',
  WAITING_USER: '等待用户',
  WAITING_EXTERNAL: '等待外部',
  COMPLETED: '本地已完成',
  FAILED: '失败',
  CANCELLED: '已取消',
};
export const operationLabels: Record<OperationState, string> = {
  DRAFT: '草案',
  READY: '准备提交',
  SUBMITTED: '已提交',
  PROCESSING: '处理中',
  SUCCEEDED: '执行成功',
  FAILED: '执行失败',
  CANCELLED: '已取消',
};
export const securityLabels: Record<SecurityOutcome, string> = {
  ALLOW: '只读展示',
  NEEDS_INPUT: '待补充信息',
  READY_TO_CONFIRM: '待核对草案',
  WAITING_AUTH: '等待认证',
  BLOCKED: '执行阻止',
  READY_TO_EXECUTE: '可执行',
};

export function parseMoney(value: string): bigint {
  if (!/^(0|[1-9]\d{0,11})\.\d{2}$/.test(value)) throw new Error('请输入两位小数金额，例如 200.00');
  const [whole, fraction] = value.split('.');
  return BigInt(whole) * 100n + BigInt(fraction);
}
export function moneyFromCents(value: bigint): string {
  const positive = value < 0n ? -value : value;
  return `${value < 0n ? '-' : ''}${positive / 100n}.${String(positive % 100n).padStart(2, '0')}`;
}
export function formatMoney(value: string): string {
  const [whole, fraction] = value.split('.');
  return `${whole.replace(/\B(?=(\d{3})+(?!\d))/g, ',')}.${fraction}`;
}
export function normalizeIntentAmount(value: string): string {
  if (!/^(0|[1-9]\d{0,11})(\.\d{1,2})?$/.test(value))
    throw new Error('金额不明确，请在草案页填写两位小数金额。');
  const [whole, fraction = ''] = value.split('.');
  const result = `${whole}.${fraction.padEnd(2, '0')}`;
  parseMoney(result);
  return result;
}
export function monthKey(date: Date): string {
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, '0')}`;
}
export function draftStatus(draft: Draft, now = Date.now()): 'DRAFT' | 'CANCELLED' | 'EXPIRED' {
  return draft.state === 'CANCELLED'
    ? 'CANCELLED'
    : now >= Date.parse(draft.expiresAt)
      ? 'EXPIRED'
      : 'DRAFT';
}
export function billSummary(transactions: Transaction[]) {
  const total = (kind: TransactionKind) =>
    moneyFromCents(
      transactions.filter((t) => t.kind === kind).reduce((n, t) => n + parseMoney(t.amount), 0n),
    );
  return {
    expense: total('EXPENSE'),
    income: total('INCOME'),
    transfer: total('TRANSFER'),
    count: transactions.length,
  };
}
export function categoryTotals(transactions: Transaction[]) {
  const amounts = new Map<string, bigint>();
  transactions
    .filter((t) => t.kind === 'EXPENSE')
    .forEach((t) =>
      amounts.set(t.category, (amounts.get(t.category) ?? 0n) + parseMoney(t.amount)),
    );
  const total = [...amounts.values()].reduce((a, b) => a + b, 0n);
  return [...amounts]
    .sort((a, b) => (a[1] > b[1] ? -1 : a[1] < b[1] ? 1 : 0))
    .map(([category, cents]) => ({
      category,
      amount: moneyFromCents(cents),
      percent: total === 0n ? 0 : Number((cents * 100n) / total),
    }));
}
