import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { AuditRecorder } from './audit-recorder.js';
import { AuditStore } from './audit-store.js';
import { recordSimulatedDraft } from './demo-draft.js';

const directory = mkdtempSync(join(tmpdir(), 'safeflow-aud01-evidence-'));
const store = new AuditStore(join(directory, 'evidence.db'));
const recorder = new AuditRecorder(store);

try {
  const events = [
    { transition: 'create', event_id: 'evt-aud01-create', operation_id: 'op-aud01-create', draft_version: 1, draft_amount: 'CNY 200.00' },
    { transition: 'update', event_id: 'evt-aud01-update', operation_id: 'op-aud01-update', draft_version: 2, draft_amount: 'CNY 300.00' },
    { transition: 'cancel', event_id: 'evt-aud01-cancel', operation_id: 'op-aud01-cancel', draft_version: 2, draft_amount: 'CNY 300.00' },
  ];
  for (const event of events) {
    recordSimulatedDraft(recorder, {
      ...event,
      task_id: 'task-aud01-evidence',
      draft_ref: 'draft-aud01-evidence',
      business_operation_id: 'business-op-aud01-evidence',
    });
  }
  process.stdout.write(`${JSON.stringify({
    evidence_scope: 'LOCAL_AUD01_DEMO_ONLY',
    source_transfer_sample: '557a6ff428771a1bb16018492415e58d2c8522ed',
    source_runtime_event_id_imported: false,
    source_runtime_event_id_status: 'RAW_RUNTIME_ID_IS_PROCESS_LOCAL; STABLE_PUBLIC_ID_PENDING',
    source_business_occurred_at: null,
    source_trusted_user_id: null,
    records: recorder.getAuditsByTask('task-aud01-evidence'),
    chain: recorder.verifyChain(),
  }, null, 2)}\n`);
} finally {
  recorder.close();
  rmSync(directory, { recursive: true, force: true });
}
