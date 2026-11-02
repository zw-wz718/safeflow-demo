import { z } from 'zod';
import {
  fundsReference,
  fundsReferenceSchema,
  sameFundsReference,
  type FundsReference,
} from './payment-schema';
import {
  legacyRecoveryKey,
  recoveryKey,
  primaryRecovery,
  restoreRecovery,
  retain,
  serializeRecovery,
  type RecoveryEntry,
} from './payment-recovery';

export const recoveryJournalKey = 'safeflow.agent.payment-recovery-journal.v1';
// Reserve room before discovery. Replacing this value needs no extra quota for
// ordinary selectors. Larger journals grow only if storage actually accepts them.
export const recoveryJournalReserve = 131072;
// Agent and original responses are capped at 35000 bytes by agent-client. Before
// dispatch, reserve enough to journal the entire next selector and its lookup.
const discoveryHeadroom = 70000;
const reference = fundsReferenceSchema.refine((r) => r.task_id === r.snapshot.transfer.task_id);
const readSchema = z
  .object({
    task_id: z.string().regex(/^[A-Za-z0-9_.-]{1,200}$/),
    reference: reference.nullable(),
  })
  .strict()
  .refine((r) => !r.reference || r.reference.task_id === r.task_id);
const journalSchema = z
  .object({
    contract_version: z.literal(1),
    scope: z.literal('PAYMENT_RECOVERY_JOURNAL_ONLY'),
    phase: z.enum(['PENDING', 'COMMITTED']),
    references: z.array(reference),
    reads: z.array(readSchema),
  })
  .strict();
export type RecoveryRead = z.infer<typeof readSchema>;
export type RecoveryState = {
  entries: RecoveryEntry[];
  damaged: boolean;
  reads: RecoveryRead[];
};
type StoragePort = Pick<Storage, 'getItem' | 'setItem' | 'removeItem'>;

/** A synchronous write-ahead journal for this tab, never a payment authority. */
export class PaymentRecoveryStore {
  state: RecoveryState = { entries: [], damaged: true, reads: [] };
  private unreadable = false;
  private persistentFault = false;
  private replayTasks = new Set<string>();
  private size = recoveryJournalReserve;
  constructor(private storage: () => StoragePort) {
    try {
      const s = storage();
      const loaded = restoreRecovery(s.getItem(recoveryKey), s.getItem(legacyRecoveryKey));
      this.state.entries = loaded.entries;
      this.unreadable = loaded.damaged;
      this.persistentFault = loaded.damaged;
      const raw = s.getItem(recoveryJournalKey);
      if (raw !== null) {
        this.size = Math.max(this.size, raw.length);
        try {
          const journal = journalSchema.parse(JSON.parse(raw));
          for (const ref of journal.references)
            this.state.entries = retain(this.state.entries, ref);
          this.state.reads = journal.reads;
          this.replayTasks = new Set(
            journal.reads.filter((r) => !r.reference).map((r) => r.task_id),
          );
          this.state.damaged =
            this.unreadable || journal.phase !== 'COMMITTED' || !!journal.reads.length;
        } catch {
          this.unreadable = true;
          this.persistentFault = true;
        }
      } else if (!this.unreadable) {
        // Also journals legacy migration before either old key can be changed.
        this.commit(this.state.entries);
      }
    } catch {
      this.unreadable = true;
    }
    if (this.unreadable) this.state = { ...this.state, damaged: true };
  }
  private journal(
    entries: RecoveryEntry[],
    reads: RecoveryRead[],
    phase: 'PENDING' | 'COMMITTED',
    headroom = 0,
  ) {
    const raw = JSON.stringify({
      contract_version: 1,
      scope: 'PAYMENT_RECOVERY_JOURNAL_ONLY',
      phase,
      references: entries.map((e) => e.reference),
      reads,
    });
    const size = Math.max(this.size, raw.length + headroom);
    this.storage().setItem(recoveryJournalKey, raw.padEnd(size, ' '));
    this.size = size;
  }
  /** Must succeed BEFORE a request can reveal an unretained original. */
  beginRead(task_id: string, ref: FundsReference | null = null) {
    if (ref && this.state.entries.some((e) => sameFundsReference(e.reference, ref))) return true;
    if (
      this.state.reads.some(
        (r) =>
          r.task_id === task_id && (!ref || (r.reference && sameFundsReference(r.reference, ref))),
      )
    )
      return true;
    // A malformed value is itself durable evidence of incompleteness. Preserve
    // it and allow reads; an inaccessible storage API offers no such guarantee.
    if (this.unreadable) return this.persistentFault;
    const reads = [
      ...this.state.reads,
      readSchema.parse({ task_id, reference: ref ? fundsReference(ref) : null }),
    ];
    try {
      this.journal(this.state.entries, reads, 'PENDING', discoveryHeadroom);
      this.state = { ...this.state, reads, damaged: true };
      return true;
    } catch {
      this.state = { ...this.state, damaged: true };
      return false;
    }
  }
  commit(
    entries: RecoveryEntry[],
    completedTask?: string,
    completedReference?: FundsReference,
    nonPaymentProjection = false,
  ) {
    const reads = this.state.reads.filter(
      (r) =>
        r.task_id !== completedTask ||
        (!r.reference &&
          this.replayTasks.has(r.task_id) &&
          !completedReference &&
          !nonPaymentProjection) ||
        (!!r.reference &&
          (!completedReference || !sameFundsReference(r.reference, completedReference))),
    );
    this.state = { ...this.state, entries };
    if (this.unreadable) return false;
    try {
      // The last durable journal remains PENDING if ANY following step fails.
      this.journal(entries, reads, 'PENDING');
      this.state = { ...this.state, reads };
      if (completedTask && (completedReference || nonPaymentProjection))
        this.replayTasks.delete(completedTask);
      const s = this.storage(),
        primary = primaryRecovery(entries);
      if (primary) {
        s.setItem(recoveryKey, serializeRecovery(entries));
        s.setItem(legacyRecoveryKey, JSON.stringify(primary));
      } else {
        s.removeItem(recoveryKey);
        s.removeItem(legacyRecoveryKey);
      }
      this.journal(entries, reads, reads.length ? 'PENDING' : 'COMMITTED');
      this.state = { entries, reads, damaged: !!reads.length };
      return !this.state.damaged;
    } catch {
      // Do not discard in-memory originals or erase/repair the old valid keys.
      this.state = { ...this.state, damaged: true };
      return false;
    }
  }
  resetOutcomes() {
    // Session invalidation can interrupt discovery without remounting this
    // store. Replay those reads just as after reload; a stale PAYMENT projection
    // alone cannot prove that an original was never submitted.
    for (const read of this.state.reads) if (!read.reference) this.replayTasks.add(read.task_id);
    this.state = {
      ...this.state,
      entries: this.state.entries.map((e) => ({ ...e, outcome: null })),
    };
  }
}
