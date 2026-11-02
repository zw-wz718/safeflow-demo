import { z } from 'zod';
import {
  fundsMatches,
  fundsReference,
  fundsReferenceSchema,
  fundsRequireRecovery,
  sameFundsReference,
  type FundsReference,
  type PaymentFunds,
} from './payment-schema';

export const legacyRecoveryKey = 'safeflow.agent.payment-attempt.v1';
export const recoveryKey = 'safeflow.agent.payment-attempts.v2';
export type RecoveryEntry = { reference: FundsReference; outcome: PaymentFunds | null };
const recoveryReferenceSchema = fundsReferenceSchema.refine(
  (r) => r.task_id === r.snapshot.transfer.task_id,
);
const storedSchema = z
  .object({
    contract_version: z.literal(2),
    scope: z.literal('PAYMENT_RECOVERY_REFERENCES_ONLY'),
    references: z.array(recoveryReferenceSchema),
  })
  .strict();
export const isResolved = (entry: RecoveryEntry) =>
  !!entry.outcome &&
  fundsMatches(entry.reference, entry.outcome) &&
  entry.outcome.submitted &&
  ['SUCCEEDED', 'FAILED'].includes(entry.outcome.state);
export function retain(entries: RecoveryEntry[], reference: FundsReference): RecoveryEntry[] {
  return entries.some((e) => sameFundsReference(e.reference, reference))
    ? entries
    : [...entries, { reference: fundsReference(reference), outcome: null }];
}
export function observe(entries: RecoveryEntry[], result: PaymentFunds): RecoveryEntry[] {
  const reference = {
    task_id: result.task_id,
    operation: { operation_id: result.preparation_operation_id },
    snapshot_digest: result.snapshot_digest,
    snapshot: { transfer: result.transfer },
  };
  const retained = fundsRequireRecovery(result) ? retain(entries, reference) : entries;
  return retained.map((entry) => {
    if (!fundsMatches(entry.reference, result)) return entry;
    // An older nonterminal projection must not erase this session's verified
    // terminal. Session changes explicitly discard every in-memory outcome.
    if (isResolved(entry) && !['SUCCEEDED', 'FAILED'].includes(result.state)) return entry;
    return { ...entry, outcome: result };
  });
}
export function restoreRecovery(current: string | null, legacy: string | null) {
  let entries: RecoveryEntry[] = [],
    damaged = false;
  if (current !== null) {
    try {
      const parsed = storedSchema.parse(JSON.parse(current));
      for (const reference of parsed.references) entries = retain(entries, reference);
    } catch {
      damaged = true;
    }
  }
  if (legacy !== null) {
    try {
      entries = retain(entries, recoveryReferenceSchema.parse(JSON.parse(legacy)));
    } catch {
      damaged = true;
    }
  }
  return { entries, damaged };
}
export function serializeRecovery(entries: RecoveryEntry[]) {
  // Persist selectors only. A cached terminal is never proof in a new session.
  return JSON.stringify({
    contract_version: 2,
    scope: 'PAYMENT_RECOVERY_REFERENCES_ONLY',
    references: entries.map((e) => fundsReference(e.reference)),
  });
}
export function primaryRecovery(entries: RecoveryEntry[]) {
  return (entries.find((e) => !isResolved(e)) ?? entries[0])?.reference ?? null;
}
