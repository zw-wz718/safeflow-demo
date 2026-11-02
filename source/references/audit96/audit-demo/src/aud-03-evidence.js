import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { AuditStore } from './audit-store.js';

// All rows and checkpoints below exist only in a temporary test directory.
// A checkpoint copied from this same test database is not a trusted external anchor.
const directory = mkdtempSync(join(tmpdir(), 'safeflow-aud03-evidence-'));
const store = new AuditStore(join(directory, 'chain.db'));
const tamperedStore = new AuditStore(join(directory, 'tampered.db'));

function append(target, suffix) {
  return target.recordAudit({
    event_id: `evt-aud03-${suffix}`, kind: 'BUSINESS_EVENT', task_id: 'task-aud03-evidence',
    operation_id: `op-aud03-${suffix}`, action: 'DEMO_EVENT',
    timestamp: '2026-10-19T06:00:00.000Z', result: 'RECORDED',
    details: { demo_only: true, step: suffix },
  });
}

function checkpointAt(target, sequence) {
  const row = target.db.prepare('SELECT sequence, record_hash FROM audit_records WHERE sequence = ?').get(sequence);
  const count = target.db.prepare('SELECT COUNT(*) AS count FROM audit_records WHERE sequence <= ?').get(sequence).count;
  return { sequence: row.sequence, count, record_hash: row.record_hash };
}

try {
  append(store, 'one');
  append(store, 'two');
  const savedPrefix = checkpointAt(store, 2);
  append(store, 'three');
  const savedHead = checkpointAt(store, 3);
  const intact = store.verifyChain();
  const prefixAfterAppend = store.verifyChain(savedPrefix);

  store.db.prepare('DELETE FROM audit_records WHERE sequence = ?').run(3);
  const tailDeleteLocal = store.verifyChain();
  const tailDeleteWithCheckpoint = store.verifyChain(savedHead);

  append(tamperedStore, 'tamper');
  tamperedStore.db.prepare('UPDATE audit_records SET result = ? WHERE sequence = ?').run('CHANGED', 1);
  const contentTamper = tamperedStore.verifyChain();

  process.stdout.write(`${JSON.stringify({
    evidence_scope: 'LOCAL_AUD03_TEST_ONLY',
    checkpoint_source: 'TEMPORARY_COPY_FROM_THE_SAME_TEST_DATABASE_NOT_TRUSTED',
    hash_version: intact.hash_version,
    hash_versions: intact.hash_versions,
    intact,
    prefix_after_append: prefixAfterAppend,
    tail_delete_without_checkpoint: tailDeleteLocal,
    tail_delete_against_saved_test_checkpoint: tailDeleteWithCheckpoint,
    content_tamper: contentTamper,
  }, null, 2)}\n`);
} finally {
  store.close();
  tamperedStore.close();
  rmSync(directory, { recursive: true, force: true });
}
