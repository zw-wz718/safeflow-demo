import { z } from 'zod';
import type { FeedbackSelector } from './billing-feedback-client';

export const feedbackRecoveryPrefix = 'safeflow.billing-feedback.recovery.v1.';
const id = z.string().min(1).max(200);
const rowSchema = z
  .object({
    version: z.literal(1),
    attempt_id: id,
    task_id: id.nullable(),
    operation_id: id.nullable(),
  })
  .strict()
  .refine((r) => r.operation_id !== null || r.task_id === null);
export type FeedbackRecovery = z.infer<typeof rowSchema>;
export type RecoveryStorage = Pick<
  Storage,
  'length' | 'key' | 'getItem' | 'setItem' | 'removeItem'
>;
export class FeedbackRecoveryStore {
  constructor(private readonly storage: () => RecoveryStorage = () => localStorage) {}
  private failure(): never {
    throw new Error('恢复记录无法可靠保存或读取，已阻止所有写操作。请保留本机数据并核对原号。');
  }
  read(): FeedbackRecovery[] {
    try {
      const store = this.storage();
      const rows: FeedbackRecovery[] = [];
      for (let i = 0; i < store.length; i++) {
        const key = store.key(i);
        if (!key?.startsWith(feedbackRecoveryPrefix)) continue;
        const row = rowSchema.parse(JSON.parse(store.getItem(key)!));
        if (key !== feedbackRecoveryPrefix + row.attempt_id) return this.failure();
        rows.push(row);
      }
      return rows;
    } catch {
      return this.failure();
    }
  }
  private put(row: FeedbackRecovery) {
    try {
      const store = this.storage(),
        key = feedbackRecoveryPrefix + row.attempt_id;
      const value = JSON.stringify(rowSchema.parse(row));
      store.setItem(key, value);
      if (store.getItem(key) !== value) return this.failure();
    } catch {
      return this.failure();
    }
  }
  reserve(attempt: string) {
    if (this.read().length) throw new Error('仍有未核清反馈，不能新建。');
    this.put({ version: 1, attempt_id: attempt, task_id: null, operation_id: null });
  }
  bind(attempt: string, selector: FeedbackSelector) {
    if (!this.read().some((r) => r.attempt_id === attempt && r.operation_id === null))
      return this.failure();
    this.put({
      version: 1,
      attempt_id: attempt,
      task_id: selector.task_id,
      operation_id: selector.operation_id,
    });
  }
  ensureOriginal(selector: FeedbackSelector) {
    if (
      this.read().some(
        (r) => r.operation_id === selector.operation_id && r.task_id === selector.task_id,
      )
    )
      return;
    this.put({
      version: 1,
      attempt_id: crypto.randomUUID(),
      task_id: selector.task_id,
      operation_id: selector.operation_id,
    });
  }
  verifyOnly(attempt: string, selector: FeedbackSelector) {
    const rows = this.read();
    if (
      rows.length !== 1 ||
      rows[0].attempt_id !== attempt ||
      rows[0].task_id !== selector.task_id ||
      rows[0].operation_id !== selector.operation_id
    )
      return this.failure();
    // Check writable storage again immediately before a subsequent write request.
    this.put(rows[0]);
  }
  resolveOriginal(selector: FeedbackSelector) {
    // Re-read; never replace the collection using an earlier component's snapshot.
    for (const row of this.read()) {
      if (
        row.operation_id === selector.operation_id &&
        (row.task_id === null || row.task_id === selector.task_id)
      )
        this.remove(row.attempt_id);
    }
  }
  remove(attempt: string) {
    try {
      const store = this.storage(),
        key = feedbackRecoveryPrefix + attempt;
      store.removeItem(key);
      if (store.getItem(key) !== null) return this.failure();
    } catch {
      return this.failure();
    }
  }
}
