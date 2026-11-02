import { describe, expect, it } from 'vitest';
import fixture from './payment-backend.fixture.json';
import { fundsReferenceSchema, type PaymentFunds } from './payment-schema';
import {
  isResolved,
  legacyRecoveryKey,
  observe,
  recoveryKey,
  retain,
  serializeRecovery,
} from './payment-recovery';
import { PaymentRecoveryStore, recoveryJournalKey } from './payment-recovery-store';

const a = fundsReferenceSchema.parse({
  task_id: fixture.waiting.payment.task_id,
  operation: { operation_id: fixture.waiting.payment.operation.operation_id },
  snapshot_digest: fixture.waiting.payment.snapshot_digest,
  snapshot: { transfer: fixture.waiting.payment.snapshot.transfer },
});
const b = {
  ...a,
  task_id: 'task_b',
  operation: { operation_id: 'preparation_b' },
  snapshot: {
    transfer: { ...a.snapshot.transfer, task_id: 'task_b', operation_id: 'operation_b' },
  },
};
const result = (ref: typeof a, state: PaymentFunds['state']): PaymentFunds => ({
  contract_version: 1,
  scope: 'PAYMENT_FUNDS_RESULT_ONLY',
  task_id: ref.task_id,
  preparation_operation_id: ref.operation.operation_id,
  snapshot_digest: ref.snapshot_digest,
  transfer: ref.snapshot.transfer,
  state,
  submitted: true,
  automatic_retry: false,
});
class StorageFaults {
  values = new Map<string, string>();
  operations = 0;
  failAt = Infinity;
  limit = Infinity;
  getItem(k: string) {
    return this.values.get(k) ?? null;
  }
  mutate() {
    if (++this.operations === this.failAt) throw new Error('interrupted storage step');
  }
  setItem(k: string, v: string) {
    this.mutate();
    const total =
      [...this.values.values()].reduce((n, s) => n + s.length, 0) -
      (this.values.get(k)?.length ?? 0) +
      v.length;
    if (total > this.limit) throw new DOMException('quota', 'QuotaExceededError');
    this.values.set(k, v);
  }
  removeItem(k: string) {
    this.mutate();
    this.values.delete(k);
  }
}
const reopen = (s: StorageFaults) => new PaymentRecoveryStore(() => s);
describe('durable payment recovery journal', () => {
  it('retains B and an explicit incomplete checkpoint across actual-size quota failure and reload', () => {
    const s = new StorageFaults(),
      store = reopen(s);
    expect(store.commit(retain([], a))).toBe(true);
    s.limit = [...s.values.values()].reduce((n, v) => n + v.length, 0) + 64;
    expect(store.beginRead(b.task_id, b)).toBe(true);
    expect(store.commit(observe(store.state.entries, result(b, 'PROCESSING')), b.task_id, b)).toBe(
      false,
    );
    expect(JSON.parse(s.getItem(recoveryKey)!).references).toEqual([a]);
    const loaded = reopen(s);
    expect(loaded.state.damaged).toBe(true);
    expect(loaded.state.entries.map((e) => e.reference)).toEqual([a, b]);
    loaded.commit(observe(loaded.state.entries, result(a, 'SUCCEEDED')));
    expect(loaded.state.damaged).toBe(true);
    expect(loaded.state.entries.map(isResolved)).toEqual([true, false]);
    loaded.resetOutcomes();
    expect(loaded.state.damaged).toBe(true);
    expect(loaded.state.entries.every((e) => !isResolved(e))).toBe(true);
    s.limit = Infinity;
    expect(
      loaded.commit(
        observe(observe(loaded.state.entries, result(a, 'SUCCEEDED')), result(b, 'FAILED')),
        b.task_id,
      ),
    ).toBe(true);
    expect(loaded.state.entries.every(isResolved)).toBe(true);
    expect(loaded.state.damaged).toBe(false);
    expect(reopen(s).state.entries.map(isResolved)).toEqual([false, false]);
  });
  it.each([1, 2, 3, 4])(
    'interruption at commit step %i never makes a readable old A collection complete',
    (step) => {
      const s = new StorageFaults(),
        store = reopen(s);
      store.commit(retain([], a));
      store.beginRead(b.task_id, b);
      s.failAt = s.operations + step;
      expect(
        store.commit(observe(store.state.entries, result(b, 'PROCESSING')), b.task_id, b),
      ).toBe(false);
      const loaded = reopen(s);
      expect(loaded.state.damaged).toBe(true);
      expect(
        loaded.state.entries.some((e) => e.reference.task_id === b.task_id) ||
          loaded.state.reads.some((r) => r.reference?.task_id === b.task_id),
      ).toBe(true);
      // Retrying the pending exact selector can rehydrate B without guessing it.
      const recovered = observe(loaded.state.entries, result(b, 'PROCESSING'));
      expect(loaded.commit(recovered, b.task_id, b)).toBe(true);
      expect(loaded.state.entries.map(isResolved)).toEqual([false, false]);
    },
  );
  it.each([1, 2, 3, 4])(
    'legacy migration interruption %i preserves originals and resumes without trusting cached outcomes',
    (step) => {
      const s = new StorageFaults();
      s.values.set(legacyRecoveryKey, JSON.stringify(a));
      s.failAt = step;
      const store = reopen(s);
      expect(store.state.damaged).toBe(true);
      const loaded = reopen(s);
      expect(loaded.state.entries.map((e) => e.reference)).toEqual([a]);
      expect(loaded.state.entries.every((e) => !isResolved(e))).toBe(true);
      expect(loaded.commit(loaded.state.entries)).toBe(true);
    },
  );
  it('a crash after arming a new task keeps its recovery lookup even before any response was accepted', () => {
    const s = new StorageFaults(),
      store = reopen(s);
    store.commit(retain([], a));
    expect(store.beginRead(b.task_id)).toBe(true);
    const loaded = reopen(s);
    expect(loaded.state.reads).toEqual([{ task_id: b.task_id, reference: null }]);
    expect(loaded.state.damaged).toBe(true);
    expect(
      loaded.commit(observe(loaded.state.entries, result(b, 'PROCESSING')), b.task_id, b),
    ).toBe(true);
    expect(reopen(s).state.entries.map((e) => e.reference)).toEqual([a, b]);
  });
  it('a generic stale task projection cannot complete an interrupted exact original query', () => {
    const s = new StorageFaults(),
      store = reopen(s);
    store.commit(retain([], a));
    store.beginRead(b.task_id, b);
    const loaded = reopen(s);
    expect(loaded.commit(loaded.state.entries, b.task_id)).toBe(false);
    expect(loaded.state.reads[0].reference).toEqual(b);
    expect(
      loaded.commit(observe(loaded.state.entries, result(b, 'PROCESSING')), b.task_id, b),
    ).toBe(true);
    expect(loaded.state.entries.map(isResolved)).toEqual([false, false]);
  });
  it('a completed read stays completed in memory when only the following collection write fails', () => {
    const s = new StorageFaults(),
      store = reopen(s);
    store.commit(retain([], a));
    store.beginRead(b.task_id, b);
    s.failAt = s.operations + 2;
    expect(store.commit(observe(store.state.entries, result(b, 'PROCESSING')), b.task_id, b)).toBe(
      false,
    );
    expect(store.state.reads).toEqual([]);
    expect(store.commit(store.state.entries)).toBe(true);
    expect(store.state.damaged).toBe(false);
    expect(store.state.entries.map(isResolved)).toEqual([false, false]);
  });
  it('a reloaded task discovery requires its original read; a stale PAYMENT projection alone is insufficient', () => {
    const s = new StorageFaults(),
      store = reopen(s);
    store.commit(retain([], a));
    store.beginRead(b.task_id);
    const loaded = reopen(s);
    expect(loaded.commit(loaded.state.entries, b.task_id)).toBe(false);
    expect(loaded.state.reads).toEqual([{ task_id: b.task_id, reference: null }]);
    expect(
      loaded.commit(observe(loaded.state.entries, result(b, 'PROCESSING')), b.task_id, b),
    ).toBe(true);
    expect(loaded.state.entries.map(isResolved)).toEqual([false, false]);
  });
  it('a validated nonpayment task can complete its interrupted read without an invented funds query', () => {
    const s = new StorageFaults(),
      store = reopen(s);
    store.beginRead('read_task');
    const loaded = reopen(s);
    expect(loaded.commit(loaded.state.entries, 'read_task', undefined, true)).toBe(true);
    expect(loaded.state.reads).toEqual([]);
  });
  it('a session change replays interrupted discovery on the same store before accepting a PAYMENT projection', () => {
    const s = new StorageFaults(),
      store = reopen(s);
    store.commit(retain([], a));
    store.beginRead(b.task_id);
    const durable = new Map(s.values);
    store.resetOutcomes();
    expect(s.values).toEqual(durable);
    expect(store.commit(store.state.entries, b.task_id)).toBe(false);
    expect(store.state.reads).toEqual([{ task_id: b.task_id, reference: null }]);
    expect(store.commit(observe(store.state.entries, result(b, 'PROCESSING')), b.task_id, b)).toBe(
      true,
    );
    expect(reopen(s).state.entries.map((e) => e.reference)).toEqual([a, b]);
  });
  it('an interrupted unsubmitted task is released by its matching original read after a session change', () => {
    const s = new StorageFaults(),
      store = reopen(s);
    store.beginRead(b.task_id);
    store.resetOutcomes();
    expect(store.commit([], b.task_id)).toBe(false);
    expect(store.commit([], b.task_id, b)).toBe(true);
    expect(store.state.reads).toEqual([]);
    expect(reopen(s).state.damaged).toBe(false);
  });
  it('cannot start an untracked discovery when its write-ahead entry cannot be saved; known reads remain possible', () => {
    const s = new StorageFaults(),
      store = reopen(s);
    store.commit(retain([], a));
    s.failAt = s.operations + 1;
    expect(store.beginRead(b.task_id, b)).toBe(false);
    expect(store.beginRead(a.task_id, a)).toBe(true);
    expect(store.state.entries.map((e) => e.reference)).toEqual([a]);
  });
  it('a journal too large for its reservation retains the pre-request task and supports recovery after space is restored', () => {
    const s = new StorageFaults(),
      store = reopen(s);
    store.commit(retain([], a));
    store.beginRead(b.task_id);
    s.limit = [...s.values.values()].reduce((n, v) => n + v.length, 0);
    const large = {
      ...b,
      snapshot: { transfer: { ...b.snapshot.transfer, remark: 'x'.repeat(200000) } },
    };
    expect(
      store.commit(observe(store.state.entries, result(large, 'PROCESSING')), b.task_id, b),
    ).toBe(false);
    const loaded = reopen(s);
    expect(loaded.state.damaged).toBe(true);
    expect(loaded.state.reads[0].task_id).toBe(b.task_id);
    s.limit = Infinity;
    expect(
      loaded.commit(observe(loaded.state.entries, result(large, 'PROCESSING')), b.task_id, b),
    ).toBe(true);
  });
  it('corrupt journal never erases readable originals or repairs itself into an empty collection', () => {
    const s = new StorageFaults();
    s.values.set(recoveryKey, serializeRecovery(retain([], a)));
    s.values.set(recoveryJournalKey, '{');
    const store = reopen(s);
    expect(store.state.damaged).toBe(true);
    expect(store.beginRead(a.task_id, a)).toBe(true);
    expect(store.commit(observe(store.state.entries, result(a, 'SUCCEEDED')))).toBe(false);
    expect(s.getItem(recoveryJournalKey)).toBe('{');
    expect(reopen(s).state.entries.map((e) => e.reference)).toEqual([a]);
  });
  it('journal selectors contain no terminal evidence, tickets or payment permissions', () => {
    const s = new StorageFaults(),
      store = reopen(s);
    store.commit(observe([], result(a, 'SUCCEEDED')));
    expect(s.getItem(recoveryJournalKey)).not.toMatch(
      /outcome|SUCCEEDED|confirmation_ref|display_ticket|permit/,
    );
    expect(reopen(s).state.entries.map(isResolved)).toEqual([false]);
  });
});
