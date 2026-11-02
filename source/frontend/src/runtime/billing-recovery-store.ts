import { z } from 'zod';
import { FeedbackRecoveryStore } from './billing-feedback-store';
const uuid = z
    .string()
    .regex(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/),
  id = z.string().min(1).max(200),
  positive = z.number().int().positive().safe();
const schema = z
  .object({
    version: z.literal(2),
    local_id: id,
    protocol: z.enum(['V2', 'LEGACY']),
    recovery_realm: uuid.nullable(),
    nonce: uuid.nullable(),
    task_id: id.nullable(),
    operation_id: id.nullable(),
    phase: z.enum(['RESERVED', 'BOUND', 'EXECUTION_SENT', 'CLOSE_SENT', 'RESOLVED']),
    row_revision: positive,
    terminal_fence_revision: positive.nullable(),
  })
  .strict()
  .refine(
    (v) =>
      (v.protocol === 'V2'
        ? v.nonce !== null &&
          v.recovery_realm !== null &&
          (v.task_id === null) === (v.operation_id === null)
        : v.nonce === null && v.recovery_realm === null) &&
      (v.operation_id !== null || v.task_id === null) &&
      (v.terminal_fence_revision === null || v.phase === 'RESOLVED'),
  );
export type RecoveryRow = z.infer<typeof schema>;
type Valid = () => boolean;
export const recoveryDatabase = 'safeflow.billing-feedback.recovery.v2';
const failure = () => new Error('恢复记录无法可靠保存或读取，已阻止写操作。请保留本机数据。');
const same = (a: unknown, b: unknown) => JSON.stringify(a) === JSON.stringify(b);
export class BillingRecoveryStore {
  constructor(
    private readonly legacy = new FeedbackRecoveryStore(),
    private readonly factory: () => IDBFactory = () => indexedDB,
  ) {}
  private async open() {
    return await new Promise<IDBDatabase>((resolve, reject) => {
      let r: IDBOpenDBRequest;
      try {
        r = this.factory().open(recoveryDatabase, 1);
      } catch {
        reject(failure());
        return;
      }
      r.onupgradeneeded = () => {
        r.result
          .createObjectStore('rows', { keyPath: 'local_id' })
          .createIndex('nonce', ['recovery_realm', 'nonce'], { unique: true });
        r.result.createObjectStore('meta');
      };
      let blocked = false;
      r.onerror = () => reject(failure());
      r.onblocked = () => {
        blocked = true;
        reject(failure());
      };
      r.onsuccess = () => {
        if (blocked) {
          r.result.close();
          return;
        }
        r.result.onversionchange = () => r.result.close();
        resolve(r.result);
      };
    });
  }
  private async transaction<T>(
    write: boolean,
    valid: Valid,
    run: (
      store: IDBObjectStore,
      done: (v: T) => void,
      abort: () => void,
      meta: IDBObjectStore,
    ) => void,
  ): Promise<T> {
    if (!valid()) throw failure();
    const db = await this.open();
    try {
      return await new Promise<T>((resolve, reject) => {
        let tx: IDBTransaction;
        try {
          tx = db.transaction(
            ['rows', 'meta'],
            write ? 'readwrite' : 'readonly',
            write ? { durability: 'strict' } : undefined,
          );
          if (write && tx.durability !== 'strict') {
            tx.abort();
            reject(failure());
            return;
          }
        } catch {
          reject(failure());
          return;
        }
        let value: T,
          ready = false;
        const abort = () => {
          try {
            tx.abort();
          } catch {
            /* already finished */
          }
          reject(failure());
        };
        tx.onabort = () => reject(failure());
        tx.onerror = () => reject(failure());
        tx.oncomplete = () => (ready && valid() ? resolve(value) : reject(failure()));
        try {
          run(
            tx.objectStore('rows'),
            (v) => {
              value = v;
              ready = true;
            },
            abort,
            tx.objectStore('meta'),
          );
        } catch {
          abort();
        }
      });
    } finally {
      db.close();
    }
  }
  private async rows(valid: Valid) {
    return this.transaction<RecoveryRow[]>(false, valid, (s, done, abort) => {
      const r = s.getAll();
      r.onsuccess = () => {
        try {
          done((r.result as unknown[]).map((v) => schema.parse(v)));
        } catch {
          abort();
        }
      };
    });
  }
  private async verify(row: RecoveryRow, valid: Valid) {
    const rows = await this.rows(valid);
    if (
      !same(
        rows.find((r) => r.local_id === row.local_id),
        row,
      )
    )
      throw failure();
    return row;
  }
  async read(valid: Valid = () => true) {
    const marker = await this.transaction<{ version: 1; legacy_ids: string[] } | undefined>(
      false,
      valid,
      (_s, done, abort, meta) => {
        const r = meta.get('legacy-migration');
        r.onsuccess = () => {
          try {
            done(
              r.result === undefined
                ? undefined
                : z
                    .object({ version: z.literal(1), legacy_ids: z.array(id) })
                    .strict()
                    .parse(r.result),
            );
          } catch {
            abort();
          }
        };
      },
    );
    const originals = this.legacy.read();
    if (originals.length) {
      await this.transaction<void>(true, valid, (s, done, abort, meta) => {
        const all = s.getAll();
        all.onsuccess = () => {
          try {
            const rows = (all.result as unknown[]).map((v) => schema.parse(v));
            if (!valid()) return abort();
            for (const old of originals) {
              const prior = rows.find((r) => r.local_id === old.attempt_id);
              if (prior) {
                if (
                  prior.protocol !== 'LEGACY' ||
                  prior.task_id !== old.task_id ||
                  prior.operation_id !== old.operation_id
                )
                  return abort();
              } else
                s.add(
                  schema.parse({
                    version: 2,
                    local_id: old.attempt_id,
                    protocol: 'LEGACY',
                    recovery_realm: null,
                    nonce: null,
                    task_id: old.task_id,
                    operation_id: old.operation_id,
                    phase: old.operation_id ? 'BOUND' : 'RESERVED',
                    row_revision: 1,
                    terminal_fence_revision: null,
                  }),
                );
            }
            meta.put(
              { version: 1, legacy_ids: originals.map((r) => r.attempt_id) },
              'legacy-migration',
            );
            done();
          } catch {
            abort();
          }
        };
      });
      // Original legacy records are retained, including on interrupted migration.
      if (!same(originals, this.legacy.read())) throw failure();
    }
    const rows = await this.rows(valid);
    if (
      marker?.legacy_ids.some(
        (id) => !rows.some((row) => row.protocol === 'LEGACY' && row.local_id === id),
      )
    )
      throw failure();
    for (const old of originals)
      if (
        !rows.some(
          (r) =>
            r.protocol === 'LEGACY' &&
            r.local_id === old.attempt_id &&
            r.task_id === old.task_id &&
            r.operation_id === old.operation_id,
        )
      )
        throw failure();
    return rows.filter((r) => r.phase !== 'RESOLVED');
  }
  async reserve(realm: string, nonce: string, valid: Valid) {
    await this.read(valid);
    const row = schema.parse({
      version: 2,
      local_id: crypto.randomUUID(),
      protocol: 'V2',
      recovery_realm: realm,
      nonce,
      task_id: null,
      operation_id: null,
      phase: 'RESERVED',
      row_revision: 1,
      terminal_fence_revision: null,
    });
    await this.transaction<void>(true, valid, (s, done, abort) => {
      const r = s.getAll();
      r.onsuccess = () => {
        try {
          const rows = (r.result as unknown[]).map((v) => schema.parse(v));
          const late = this.legacy.read();
          if (
            !valid() ||
            rows.some((v) => v.phase !== 'RESOLVED') ||
            late.some(
              (v) =>
                !rows.some(
                  (r) =>
                    r.protocol === 'LEGACY' &&
                    r.local_id === v.attempt_id &&
                    r.phase === 'RESOLVED' &&
                    r.task_id === v.task_id &&
                    r.operation_id === v.operation_id,
                ),
            )
          )
            return abort();
          s.add(row);
          done();
        } catch {
          abort();
        }
      };
    });
    return this.verify(row, valid);
  }
  async change(
    row: RecoveryRow,
    patch: Partial<
      Pick<RecoveryRow, 'task_id' | 'operation_id' | 'phase' | 'terminal_fence_revision'>
    >,
    valid: Valid,
  ) {
    const next = schema.parse({ ...row, ...patch, row_revision: row.row_revision + 1 });
    await this.transaction<void>(true, valid, (s, done, abort) => {
      const r = s.get(row.local_id);
      r.onsuccess = () => {
        try {
          const current = schema.parse(r.result);
          if (!valid() || current.phase === 'RESOLVED' || !same(current, row)) return abort();
          if (
            row.task_id !== null &&
            (next.task_id !== row.task_id || next.operation_id !== row.operation_id)
          )
            return abort();
          s.put(next);
          done();
        } catch {
          abort();
        }
      };
    });
    return this.verify(next, valid);
  }
  async verifyOnly(row: RecoveryRow, valid: Valid) {
    const rows = await this.read(valid);
    if (rows.length !== 1 || !same(rows[0], row)) throw failure();
    return this.verify(row, valid);
  }
}
