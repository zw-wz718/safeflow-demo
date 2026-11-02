import { describe, expect, it } from 'vitest';
import fixture from './payment-backend.fixture.json';
import {
  fundsReference,
  fundsReferenceSchema,
  fundsResultSchema,
  type FundsReference,
  type PaymentFunds,
} from './payment-schema';
import {
  isResolved,
  observe,
  primaryRecovery,
  restoreRecovery,
  retain,
  serializeRecovery,
} from './payment-recovery';

const a = fundsReference(
  fundsReferenceSchema.parse({
    task_id: fixture.waiting.payment.task_id,
    operation: { operation_id: fixture.waiting.payment.operation.operation_id },
    snapshot_digest: fixture.waiting.payment.snapshot_digest,
    snapshot: { transfer: fixture.waiting.payment.snapshot.transfer },
  }),
);
const b = fundsReferenceSchema.parse({
  task_id: 'other_task',
  operation: { operation_id: 'other_preparation' },
  snapshot_digest: 'b'.repeat(64),
  snapshot: {
    transfer: {
      ...a.snapshot.transfer,
      task_id: 'other_task',
      operation_id: 'other_operation',
      draft_id: 'other_draft',
    },
  },
});
function result(reference: FundsReference, state: PaymentFunds['state']) {
  return fundsResultSchema.parse({
    contract_version: 1,
    scope: 'PAYMENT_FUNDS_RESULT_ONLY',
    task_id: reference.task_id,
    preparation_operation_id: reference.operation.operation_id,
    snapshot_digest: reference.snapshot_digest,
    transfer: reference.snapshot.transfer,
    state,
    submitted: !['UNSUBMITTED', 'REGISTERED_UNSUBMITTED'].includes(state),
    automatic_retry: false,
  });
}
describe('each discovered original retains independent recovery protection', () => {
  it.each(['PROCESSING', 'RECONCILE_REQUIRED', 'SOURCE_CONFLICT'] as const)(
    'resolving A retains %s B and its complete selector',
    (state) => {
      let entries = observe(retain([], a), result(a, 'PROCESSING'));
      entries = observe(entries, result(b, state));
      entries = observe(entries, result(a, 'SUCCEEDED'));
      expect(entries.map(isResolved)).toEqual([true, false]);
      expect(primaryRecovery(entries)).toEqual(b);
      const restored = restoreRecovery(serializeRecovery(entries), JSON.stringify(b));
      expect(restored.damaged).toBe(false);
      expect(restored.entries.map((e) => e.reference)).toEqual([a, b]);
      expect(restored.entries.every((e) => !isResolved(e))).toBe(true);
      entries = observe(entries, result(b, 'FAILED'));
      expect(entries.every(isResolved)).toBe(true);
    },
  );
  it('resolving B first leaves A, and repeated/stale reads do not replace or duplicate either original', () => {
    let entries = observe(observe([], result(a, 'PROCESSING')), result(b, 'PROCESSING'));
    for (let i = 0; i < 3; i++) entries = observe(entries, result(b, 'SUCCEEDED'));
    entries = observe(entries, result(b, 'UNSUBMITTED'));
    entries = observe(entries, result(a, 'REGISTERED_UNSUBMITTED'));
    expect(entries).toHaveLength(2);
    expect(entries.map(isResolved)).toEqual([false, true]);
    expect(primaryRecovery(entries)).toEqual(a);
    expect(observe(entries, result(a, 'FAILED')).every(isResolved)).toBe(true);
  });
  it('a submitted attempt stays unresolved after an unsubmitted GET, while a never submitted B adds no lock', () => {
    const entries = observe(retain([], a), result(a, 'UNSUBMITTED'));
    expect(entries.map(isResolved)).toEqual([false]);
    expect(observe(entries, result(b, 'UNSUBMITTED'))).toEqual(entries);
  });
  it.each(['SUCCEEDED', 'FAILED'] as const)(
    'a newly discovered %s B is resolved immediately, without losing pending A',
    (state) => {
      const entries = observe(retain([], a), result(b, state));
      expect(entries.map(isResolved)).toEqual([false, true]);
      expect(observe(entries, result(a, 'SUCCEEDED')).every(isResolved)).toBe(true);
    },
  );
  it('a different preparation/snapshot of the same task is a separate original', () => {
    const other = {
      ...a,
      operation: { operation_id: 'other_preparation' },
      snapshot_digest: 'c'.repeat(64),
    };
    const entries = observe(retain([], a), result(other, 'SUCCEEDED'));
    expect(entries.map(isResolved)).toEqual([false, true]);
    expect(entries[0].reference).toEqual(a);
  });
  it('legacy single-selector recovery migrates without trusting cached outcomes', () => {
    const loaded = restoreRecovery(null, JSON.stringify(a));
    expect(loaded).toEqual({ entries: [{ reference: a, outcome: null }], damaged: false });
    const saved = serializeRecovery(observe(loaded.entries, result(a, 'SUCCEEDED')));
    expect(saved).not.toMatch(/outcome|SUCCEEDED|display_ticket|confirmation_ref|permit/);
    expect(restoreRecovery(saved, JSON.stringify(a)).entries.map(isResolved)).toEqual([false]);
  });
  it.each([
    '',
    '{',
    '{"contract_version":3}',
    JSON.stringify({
      contract_version: 2,
      scope: 'PAYMENT_RECOVERY_REFERENCES_ONLY',
      references: [{ ...a, task_id: 'wrong' }],
    }),
  ])('unreadable v2 recovery is explicitly damaged and never silently resolved: %s', (raw) => {
    const loaded = restoreRecovery(raw, JSON.stringify(a));
    expect(loaded.damaged).toBe(true);
    expect(loaded.entries).toEqual([{ reference: a, outcome: null }]);
  });
  it('corrupt legacy recovery also stays explicit even if the v2 references are readable', () => {
    const loaded = restoreRecovery(serializeRecovery(retain([], b)), '{');
    expect(loaded.damaged).toBe(true);
    expect(loaded.entries[0].reference).toEqual(b);
  });
});
