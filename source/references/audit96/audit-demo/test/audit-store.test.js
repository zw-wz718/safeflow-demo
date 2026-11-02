import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { existsSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Worker } from 'node:worker_threads';
import test from 'node:test';
import { DatabaseSync } from 'node:sqlite';
import { AuditConflictError, AuditIntegrityError, AuditStore } from '../src/audit-store.js';
import { AuditInputError, AuditRecorder } from '../src/audit-recorder.js';
import { recordSimulatedDraft } from '../src/demo-draft.js';
import { recordSimulatedTransfer } from '../src/demo-transfer.js';
import { createParticipantReceipt } from '../src/receipt-view.js';

function withStore(run) {
  const dir = mkdtempSync(join(tmpdir(), 'safeflow-audit-'));
  const store = new AuditStore(join(dir, 'audit.db'));
  try { return run(store); } finally { store.close(); rmSync(dir, { recursive: true, force: true }); }
}

function stableJson(value) {
  if (Array.isArray(value)) return `[${value.map(stableJson).join(',')}]`;
  if (value && typeof value === 'object') return `{${Object.keys(value).sort().map((key) => `${JSON.stringify(key)}:${stableJson(value[key])}`).join(',')}}`;
  return JSON.stringify(value);
}

const fingerprintFields = ['event_id', 'kind', 'task_id', 'node_id', 'operation_id', 'user_id', 'action', 'timestamp', 'security_result', 'confirmation', 'auth_result', 'result', 'tool_name', 'details'];
function eventFingerprint(event) {
  return createHash('sha256').update(stableJson(Object.fromEntries(
    fingerprintFields.map((field) => [field, field === 'details' ? event[field] ?? {} : event[field] ?? null]),
  )), 'utf8').digest('hex');
}

function waitForWorkerState(worker, expectedState) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`worker did not report ${expectedState}`)), 10000);
    const onMessage = (message) => {
      if (message.state === 'error') {
        clearTimeout(timer);
        worker.off('message', onMessage);
        reject(new Error(message.error));
      } else if (message.state === expectedState) {
        clearTimeout(timer);
        worker.off('message', onMessage);
        resolve(message);
      }
    };
    worker.on('message', onMessage);
    worker.once('error', (error) => {
      clearTimeout(timer);
      reject(error);
    });
  });
}

function createAuditWorker(dbPath, event) {
  const source = `
    const { parentPort, workerData } = require('node:worker_threads');
    (async () => {
      let store;
      try {
        const { AuditStore } = await import(workerData.moduleUrl);
        store = new AuditStore(workerData.dbPath);
        parentPort.postMessage({ state: 'ready' });
        await new Promise((resolve) => parentPort.once('message', resolve));
        const record = store.recordAudit(workerData.event);
        store.close();
        parentPort.postMessage({ state: 'done', audit_id: record.audit_id });
      } catch (error) {
        try { store?.close(); } catch {}
        parentPort.postMessage({ state: 'error', error: error.message });
      }
    })();
  `;
  return new Worker(source, {
    eval: true,
    workerData: { dbPath, event, moduleUrl: new URL('../src/audit-store.js', import.meta.url).href },
  });
}

test('simulated 200 CNY transfer yields queryable four-event chain', () => withStore((store) => {
  const demo = recordSimulatedTransfer(store);
  assert.equal(demo.records.length, 4);
  assert.equal(store.getAuditsByTask(demo.task_id).length, 4);
  assert.equal(store.getAuditByOperation(demo.operation_id).length, 4);
  assert.equal(store.getAudit(demo.records[0].audit_id).event_id, demo.records[0].event_id);
  assert.equal(demo.records[2].details.amount, 'CNY 200.00');
  assert.equal(new Set(demo.records.map((record) => record.operation_id)).size, 1);
  assert.equal(new Set(demo.records.map((record) => record.event_id)).size, 4);
  assert.equal(store.verifyChain().valid, true);
}));

test('simulated failure remains a business failure on the same operation', () => withStore((store) => {
  const demo = recordSimulatedTransfer(store, 'failure');
  assert.equal(demo.outcome, 'FAILED');
  assert.equal(demo.records[2].action, 'TRANSFER_FAILED');
  assert.equal(demo.records[2].result, 'FAILED');
  assert.equal(demo.records[2].details.failure_reason, '模拟失败，未实际提交银行');
  assert.equal(store.getAuditByOperation(demo.operation_id).length, 4);
  assert.equal(store.verifyChain().valid, true);
}));

test('payer and recipient receipts expose the same redacted fields only', () => withStore((store) => {
  const demo = recordSimulatedTransfer(store, 'failure');
  const payer = createParticipantReceipt(store.getAuditByOperation(demo.operation_id), 'payer');
  const recipient = createParticipantReceipt(store.getAuditByOperation(demo.operation_id), 'recipient');
  assert.equal(payer.status, 'FAILED');
  assert.equal(recipient.status, 'FAILED');
  assert.equal(payer.failure_reason, '模拟失败，未实际提交银行');
  assert.equal(payer.amount, recipient.amount);
  assert.equal(payer.payer_label, recipient.payer_label);
  assert.equal(payer.payee_label, recipient.payee_label);
  assert.deepEqual(Object.keys(payer).sort(), ['amount', 'failure_reason', 'occurred_at', 'payee_label', 'payer_label', 'perspective', 'receipt_ref', 'simulated', 'status'].sort());
  assert.equal(JSON.stringify(payer).includes('security_result'), false);
  assert.equal(JSON.stringify(payer).includes('auth_result'), false);
  assert.notEqual(payer.perspective, recipient.perspective);
}));

test('lost audit acknowledgement can be backfilled with same event_id without a duplicate', () => withStore((store) => {
  const event = { event_id: 'evt-1', kind: 'TOOL_STARTED', task_id: 'task-1', operation_id: 'op-1', user_id: 'demo', action: 'SUBMIT', timestamp: '2026-10-06T00:00:00Z', result: 'PROCESSING', details: {} };
  assert.equal(store.recordAudit(event).audit_id, store.recordAudit(event).audit_id);
  assert.equal(store.getAuditsByTask('task-1').length, 1);
}));

test('same event_id with changed content is rejected', () => withStore((store) => {
  const event = { event_id: 'evt-1', kind: 'TOOL_STARTED', task_id: 'task-1', operation_id: 'op-1', user_id: 'demo', action: 'SUBMIT', timestamp: '2026-10-06T00:00:00Z', result: 'PROCESSING', details: {} };
  store.recordAudit(event);
  assert.throws(() => store.recordAudit({ ...event, result: 'SUCCEEDED' }), AuditConflictError);
}));

test('duplicate event_id is not acknowledged from a damaged stored row', () => {
  const event = {
    event_id: 'evt-duplicate-integrity', kind: 'TOOL_COMPLETED', task_id: 'task-duplicate-integrity',
    operation_id: 'op-duplicate-integrity', result: 'SUCCEEDED', details: { amount: 'CNY 200.00' },
  };
  for (const damage of [
    (store, auditId) => store.db.prepare('UPDATE audit_records SET result = ? WHERE audit_id = ?').run('TAMPERED', auditId),
    (store, auditId) => store.db.prepare('UPDATE audit_records SET details_json = ? WHERE audit_id = ?').run('{bad JSON', auditId),
    (store, auditId) => store.db.prepare('UPDATE audit_records SET event_fingerprint = ? WHERE audit_id = ?').run('tampered', auditId),
  ]) {
    withStore((store) => {
      const saved = store.recordAudit(event);
      damage(store, saved.audit_id);
      assert.throws(() => store.recordAudit(event), AuditIntegrityError);
      assert.equal(store.db.prepare('SELECT COUNT(*) AS count FROM audit_records WHERE task_id = ?').get(event.task_id).count, 1);
    });
  }
});

test('failed SQLite append leaves no partial record and the same event can be retried', () => withStore((store) => {
  const event = {
    event_id: 'evt-store-failure', kind: 'TOOL_COMPLETED', task_id: 'task-store-failure',
    operation_id: 'op-store-failure', user_id: 'demo-user', action: 'TRANSFER_SUCCEEDED',
    timestamp: '2026-10-17T00:00:00Z', result: 'SUCCEEDED', details: {},
  };
  store.db.exec(`CREATE TRIGGER reject_audit_insert BEFORE INSERT ON audit_records
    WHEN NEW.event_id = 'evt-store-failure' BEGIN SELECT RAISE(ABORT, 'forced storage failure'); END`);
  assert.throws(() => store.recordAudit(event), /forced storage failure/);
  assert.equal(store.getAuditByEventId(event.event_id), null);
  store.db.exec('DROP TRIGGER reject_audit_insert');
  const saved = store.recordAudit(event);
  assert.equal(store.getAuditByEventId(event.event_id).audit_id, saved.audit_id);
  assert.equal(store.getAuditsByTask(event.task_id).length, 1);
  assert.equal(store.verifyChain().valid, true);
}));

test('concurrent duplicate deliveries from separate SQLite connections return one audit_id', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'safeflow-audit-concurrent-'));
  const dbPath = join(dir, 'audit.db');
  const initialStore = new AuditStore(dbPath);
  initialStore.close();
  const event = {
    event_id: 'evt-concurrent-same', kind: 'TOOL_COMPLETED', task_id: 'task-concurrent',
    operation_id: 'op-concurrent', user_id: 'demo-user', action: 'TRANSFER_SUCCEEDED',
    timestamp: '2026-10-17T00:00:00Z', result: 'SUCCEEDED', details: { source: 'concurrency-test' },
  };
  const workers = [createAuditWorker(dbPath, event)];
  try {
    await waitForWorkerState(workers[0], 'ready');
    workers.push(createAuditWorker(dbPath, event));
    await waitForWorkerState(workers[1], 'ready');
    const completions = workers.map((worker) => waitForWorkerState(worker, 'done'));
    for (const worker of workers) worker.postMessage({ action: 'record' });
    const results = await Promise.all(completions);
    assert.equal(results[0].audit_id, results[1].audit_id);
    const check = new AuditStore(dbPath);
    try {
      assert.equal(check.getAuditsByTask(event.task_id).length, 1);
      assert.equal(check.verifyChain().valid, true);
    } finally { check.close(); }
  } finally {
    await Promise.all(workers.map((worker) => worker.terminate()));
    rmSync(dir, { recursive: true, force: true });
  }
});

test('concurrent distinct events serialize into one valid hash chain', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'safeflow-audit-chain-concurrent-'));
  const dbPath = join(dir, 'audit.db');
  const initialStore = new AuditStore(dbPath);
  initialStore.close();
  const events = ['a', 'b'].map((suffix) => ({
    event_id: `evt-concurrent-${suffix}`, kind: 'TOOL_COMPLETED', task_id: 'task-concurrent-chain',
    operation_id: `op-concurrent-${suffix}`, user_id: 'demo-user', action: 'TRANSFER_SUCCEEDED',
    timestamp: '2026-10-17T00:00:00Z', result: 'SUCCEEDED', details: { source: 'concurrency-test' },
  }));
  const workers = [createAuditWorker(dbPath, events[0])];
  try {
    await waitForWorkerState(workers[0], 'ready');
    workers.push(createAuditWorker(dbPath, events[1]));
    await waitForWorkerState(workers[1], 'ready');
    const completions = workers.map((worker) => waitForWorkerState(worker, 'done'));
    for (const worker of workers) worker.postMessage({ action: 'record' });
    await Promise.all(completions);
    const check = new AuditStore(dbPath);
    try {
      const timeline = check.getAuditsByTask('task-concurrent-chain');
      assert.equal(timeline.length, 2);
      assert.equal(timeline[0].prev_hash, null);
      assert.equal(timeline[1].prev_hash, timeline[0].record_hash);
      assert.equal(check.verifyChain().valid, true);
    } finally { check.close(); }
  } finally {
    await Promise.all(workers.map((worker) => worker.terminate()));
    rmSync(dir, { recursive: true, force: true });
  }
});

test('draft create, update, and cancel stay as draft events under one task', () => withStore((store) => {
  const recorder = new AuditRecorder(store);
  const taskId = 'task-draft-demo';
  const draftRef = 'draft-demo-001';
  const businessOperationId = 'business-op-draft-001';
  const created = recordSimulatedDraft(recorder, {
    transition: 'create', event_id: 'evt-draft-create', task_id: taskId,
    operation_id: 'op-draft-create', draft_ref: draftRef, business_operation_id: businessOperationId,
    draft_version: 1, draft_amount: 'CNY 200.00',
  });
  const updated = recordSimulatedDraft(recorder, {
    transition: 'update', event_id: 'evt-draft-update', task_id: taskId,
    operation_id: 'op-draft-update', draft_ref: draftRef, business_operation_id: businessOperationId,
    draft_version: 2, draft_amount: 'CNY 300.00',
  });
  const cancelled = recordSimulatedDraft(recorder, {
    transition: 'cancel', event_id: 'evt-draft-cancel', task_id: taskId,
    operation_id: 'op-draft-cancel', draft_ref: draftRef, business_operation_id: businessOperationId,
    draft_version: 2, draft_amount: 'CNY 300.00',
  });

  const timeline = recorder.getAuditsByTask(taskId);
  assert.deepEqual(timeline.map((record) => record.action), [
    'DRAFT_CREATED', 'DRAFT_UPDATED', 'DRAFT_CANCELLED',
  ]);
  assert.deepEqual(timeline.map((record) => record.details.draft_version), [1, 2, 2]);
  assert.deepEqual(timeline.map((record) => record.details.draft_amount), ['CNY 200.00', 'CNY 300.00', 'CNY 300.00']);
  assert.deepEqual(timeline.map((record) => record.details.draft_status), ['READY_TO_CONFIRM', 'READY_TO_CONFIRM', 'CANCELLED']);
  assert.deepEqual(timeline.map((record) => record.details.business_operation_id), [businessOperationId, businessOperationId, businessOperationId]);
  assert.deepEqual(timeline.map((record) => record.timestamp), [null, null, null]);
  assert.deepEqual(timeline.map((record) => record.user_id), [null, null, null]);
  assert.ok(timeline.every((record) => Number.isFinite(Date.parse(record.recorded_at))));
  assert.ok(timeline.every((record) => record.details.source_business_occurred_at === null));
  assert.ok(timeline.every((record) => record.details.trusted_user_id === null));
  assert.equal(new Set(timeline.map((record) => record.operation_id)).size, 3);
  assert.equal(new Set(timeline.map((record) => record.details.draft_ref)).size, 1);
  assert.equal(created.operation_id, 'op-draft-create');
  assert.equal(updated.operation_id, 'op-draft-update');
  assert.equal(cancelled.record.result, 'DRAFT_CANCELLED');
  assert.equal(recorder.getAuditByOperation('op-draft-update').length, 1);
  assert.equal(recorder.getAuditByEventId('evt-draft-create').audit_id, created.record.audit_id);
  assert.equal(createParticipantReceipt(recorder.getAuditByOperation('op-draft-update'), 'payer'), null);
  assert.equal(recorder.verifyChain().valid, true);
}));

test('draft source projection rejects a locally invented business occurrence time', () => withStore((store) => {
  const recorder = new AuditRecorder(store);
  assert.throws(() => recordSimulatedDraft(recorder, {
    transition: 'create', event_id: 'evt-fake-time', task_id: 'task-fake-time',
    operation_id: 'op-fake-time', draft_ref: 'draft-fake-time', draft_version: 1,
    occurred_at: '2026-10-17T01:00:00.000Z',
  }), /source draft sample has no business occurrence time/);
  assert.equal(recorder.getAuditsByTask('task-fake-time').length, 0);
}));

test('generic AuditRecorder accepts occurred_at and makes exact retries idempotent', () => withStore((store) => {
  const recorder = new AuditRecorder(store);
  const event = {
    event_id: 'evt-generic-1', kind: 'BUSINESS_EVENT', task_id: 'task-generic-1',
    operation_id: 'op-generic-1', user_id: 'demo-user', action: 'SAMPLE_ACTION',
    occurred_at: '2026-10-14T02:00:00.000Z', result: 'RECORDED', details: { source: 'test' },
  };
  const first = recorder.recordAudit(event);
  const replay = recorder.recordAudit(event);
  assert.equal(first.audit_id, replay.audit_id);
  assert.equal(first.timestamp, event.occurred_at);
  assert.equal(recorder.getAuditsByTask(event.task_id).length, 1);
}));

test('raw Runtime-shaped events preserve missing identity, occurrence time, and result fields', () => withStore((store) => {
  const recorder = new AuditRecorder(store);
  const event = {
    event_id: 'evt-runtime-minimal', kind: 'TOOL_COMPLETED', task_id: 'task-runtime-minimal',
    node_id: 'node-transfer', tool_name: 'create_transfer_draft', details: { runtime_source: true },
  };
  const record = recorder.recordAudit(event);
  assert.equal(record.operation_id, null);
  assert.equal(record.user_id, null);
  assert.equal(record.timestamp, null);
  assert.equal(record.action, null);
  assert.equal(record.result, null);
  assert.equal(record.security_result, null);
  assert.ok(Number.isFinite(Date.parse(record.recorded_at)));
  assert.equal(recorder.verifyChain().valid, true);
}));

test('generic AuditRecorder requires explicit IDs and does not silently drop unknown contract fields', () => withStore((store) => {
  const recorder = new AuditRecorder(store);
  const event = {
    kind: 'BUSINESS_EVENT', task_id: 'task-generic-2', operation_id: 'op-generic-2',
    user_id: 'demo-user', action: 'SAMPLE_ACTION', occurred_at: '2026-10-14T02:00:00Z', result: 'RECORDED',
  };
  assert.throws(() => recorder.recordAudit(event), AuditInputError);
  assert.throws(() => recorder.recordAudit({ ...event, event_id: 'evt-2', decision_ref: 'not-mapped' }), AuditInputError);
  assert.throws(() => recorder.recordAudit({ ...event, event_id: 'evt-3', occurred_at: '2026-10-14T02:00:00' }), AuditInputError);
  assert.equal(recorder.getAuditsByTask('task-generic-2').length, 0);
}));

test('verifyChain detects changed stored content', () => withStore((store) => {
  const demo = recordSimulatedTransfer(store);
  store.db.prepare("UPDATE audit_records SET result='TAMPERED' WHERE audit_id=?").run(demo.records[1].audit_id);
  const check = store.verifyChain();
  assert.equal(check.valid, false);
  assert.equal(check.local_valid, false);
  assert.equal(check.broken_audit_id, demo.records[1].audit_id);
  assert.equal(check.first_break_sequence, 2);
  assert.equal(check.hash_version, 'local-js-v2');
}));

test('local-js-v1 hash vector fixes sorted JSON, nulls, and exact time strings', () => withStore((store) => {
  const expectedHash = 'a27825cf5e2368bde748cc544939aaf5174b80c0e1eed81bbfdcc928198448b8';
  const event = {
    event_id: 'evt-vector-1', kind: 'TOOL_COMPLETED', task_id: 'task-vector-1', node_id: null,
    operation_id: 'op-vector-1', user_id: null, action: 'TRANSFER_FAILED', timestamp: '2026-10-17T14:00:00+08:00',
    security_result: null, confirmation: null, auth_result: null, result: 'FAILED', tool_name: 'demo_transfer',
    details: { nested: { z: 1, a: null }, amount: 'CNY 200.00' },
  };
  store.db.prepare(`INSERT INTO audit_records (
    sequence,audit_id,event_id,event_fingerprint,kind,task_id,node_id,operation_id,user_id,action,timestamp,
    recorded_at,security_result,confirmation,auth_result,result,tool_name,details_json,prev_hash,record_hash,hash_version
  ) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`).run(
    1,
    'audit-vector-1', 'evt-vector-1', eventFingerprint(event), 'TOOL_COMPLETED',
    'task-vector-1', null, 'op-vector-1', null, 'TRANSFER_FAILED', '2026-10-17T14:00:00+08:00',
    '2026-10-17T06:00:01.000Z', null, null, null, 'FAILED', 'demo_transfer',
    '{"nested":{"z":1,"a":null},"amount":"CNY 200.00"}', null, expectedHash, 'local-js-v1',
  );
  const saved = store.getAudit('audit-vector-1');
  assert.equal(saved.timestamp, '2026-10-17T14:00:00+08:00');
  assert.equal(saved.recorded_at, '2026-10-17T06:00:01.000Z');
  assert.equal(saved.user_id, null);
  const check = store.verifyChain();
  assert.equal(check.valid, true);
  assert.equal(check.head_hash, expectedHash);
  assert.equal(check.hash_version, 'local-js-v1');
  assert.equal(check.legacy_fingerprint_count, 1);
  assert.deepEqual(check.checkpoint, { status: 'NOT_CHECKED' });
}));

test('local-js-v2 vector binds hash version and sequence while keeping JSON ordering stable', () => withStore((store) => {
  const expectedHash = 'e1d3fb58187055ca2b6fbb4fb18415a555033d69112c48304b84b66b30be411a';
  const event = {
    event_id: 'evt-v2-vector-1', kind: 'OPERATION_CREATED', task_id: 'task-v2-vector-1', node_id: null,
    operation_id: 'op-v2-vector-1', user_id: null, action: null, timestamp: null,
    security_result: null, confirmation: null, auth_result: null, result: null, tool_name: null,
    details: { nested: { z: 1, a: null }, amount: 'CNY 200.00' },
  };
  store.db.prepare(`INSERT INTO audit_records (
    sequence,audit_id,event_id,event_fingerprint,kind,task_id,node_id,operation_id,user_id,action,timestamp,
    recorded_at,security_result,confirmation,auth_result,result,tool_name,details_json,prev_hash,record_hash,hash_version
  ) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`).run(
    1, 'audit-v2-vector-1', 'evt-v2-vector-1', eventFingerprint(event), 'OPERATION_CREATED',
    'task-v2-vector-1', null, 'op-v2-vector-1', null, null, null,
    '2026-10-19T09:00:01.000Z', null, null, null, null, null,
    '{"nested":{"z":1,"a":null},"amount":"CNY 200.00"}', null, expectedHash, 'local-js-v2',
  );
  const record = store.getAudit('audit-v2-vector-1');
  assert.equal(record.sequence, 1);
  assert.equal(record.hash_version, 'local-js-v2');
  assert.equal(record.record_hash, expectedHash);
  const check = store.verifyChain();
  assert.equal(check.valid, true);
  assert.deepEqual(check.hash_versions, ['local-js-v2']);
}));

test('recordAudit rejects non-JSON details before writing a record', () => withStore((store) => {
  const event = { event_id: 'evt-bad-json', kind: 'BUSINESS_EVENT', task_id: 'task-bad-json' };
  const cycle = {};
  cycle.self = cycle;
  for (const details of [
    { nested: { missing: undefined } }, { number: Number.NaN }, { number: Infinity },
    { date: new Date('2026-10-17T00:00:00Z') }, { list: [, 1] }, cycle,
  ]) {
    assert.throws(() => store.recordAudit({ ...event, details }), TypeError);
  }
  assert.equal(store.getAuditsByTask(event.task_id).length, 0);
  assert.equal(store.verifyChain().valid, true);
}));

test('verifyChain reports malformed details JSON as a first break rather than throwing', () => withStore((store) => {
  const demo = recordSimulatedTransfer(store);
  store.db.prepare('UPDATE audit_records SET details_json = ? WHERE audit_id = ?').run('{bad JSON', demo.records[1].audit_id);
  const check = store.verifyChain();
  assert.equal(check.valid, false);
  assert.equal(check.checked, 1);
  assert.equal(check.broken_audit_id, demo.records[1].audit_id);
  assert.equal(check.first_break_sequence, 2);
  assert.equal(check.reason, 'INVALID_DETAILS_JSON');
}));

test('verifyChain detects sequence reordering without changing existing row hashes', () => withStore((store) => {
  const demo = recordSimulatedTransfer(store);
  store.db.exec('BEGIN IMMEDIATE');
  try {
    store.db.prepare('UPDATE audit_records SET sequence = -1 WHERE audit_id = ?').run(demo.records[1].audit_id);
    store.db.prepare('UPDATE audit_records SET sequence = 2 WHERE audit_id = ?').run(demo.records[2].audit_id);
    store.db.prepare('UPDATE audit_records SET sequence = 3 WHERE audit_id = ?').run(demo.records[1].audit_id);
    store.db.exec('COMMIT');
  } catch (error) {
    store.db.exec('ROLLBACK');
    throw error;
  }
  const check = store.verifyChain();
  assert.equal(check.valid, false);
  assert.equal(check.reason, 'PREVIOUS_HASH_MISMATCH');
  assert.equal(check.first_break_sequence, 2);
  assert.equal(check.broken_audit_id, demo.records[2].audit_id);
}));

test('verifyChain checks marked fingerprints while preserving unmarked legacy rows', () => withStore((store) => {
  const demo = recordSimulatedTransfer(store);
  assert.equal(store.verifyChain().legacy_fingerprint_count, 0);
  store.db.prepare('UPDATE audit_records SET event_fingerprint = ? WHERE audit_id = ?').run('changed', demo.records[1].audit_id);
  const check = store.verifyChain();
  assert.equal(check.valid, false);
  assert.equal(check.reason, 'EVENT_FINGERPRINT_MISMATCH');
  assert.equal(check.broken_audit_id, demo.records[1].audit_id);
}));

test('fingerprint marker downgrade cannot make a changed historical payload verifiable', () => withStore((store) => {
  const original = {
    event_id: 'evt-marker-downgrade', kind: 'TOOL_COMPLETED', task_id: 'task-marker-downgrade',
    operation_id: 'op-marker-downgrade', result: 'SUCCEEDED', details: { outcome: 'original' },
  };
  const saved = store.recordAudit(original);
  const changed = { ...original, result: 'FAILED', details: { outcome: 'changed' } };
  store.db.prepare('UPDATE audit_records SET fingerprint_version = NULL, event_fingerprint = ? WHERE audit_id = ?')
    .run(eventFingerprint(changed), saved.audit_id);

  const local = store.verifyChain();
  assert.equal(local.valid, false);
  assert.equal(local.reason, 'UNVERIFIABLE_LEGACY_FINGERPRINT');
  assert.equal(local.first_break_sequence, 1);
  const checkpoint = store.verifyChain({ sequence: 1, count: 1, record_hash: saved.record_hash });
  assert.equal(checkpoint.valid, false);
  assert.equal(checkpoint.reason, 'UNVERIFIABLE_LEGACY_FINGERPRINT');
  assert.throws(() => store.recordAudit(changed), AuditIntegrityError);

  const stillStored = store.getAudit(saved.audit_id);
  assert.equal(stillStored.result, 'SUCCEEDED');
  assert.equal(stillStored.record_hash, saved.record_hash);
  assert.equal(stillStored.timestamp, saved.timestamp);
  assert.equal(stillStored.recorded_at, saved.recorded_at);
  assert.equal(store.getAuditsByTask(original.task_id).length, 1);
}));

test('optional external checkpoint authenticates a prefix and labels later records unanchored', () => withStore((store) => {
  const first = store.recordAudit({ event_id: 'evt-anchor-1', kind: 'BUSINESS_EVENT', task_id: 'task-anchor' });
  const initial = store.verifyChain();
  const checkpoint = { sequence: initial.head_sequence, count: initial.checked, record_hash: initial.head_hash };
  const second = store.recordAudit({ event_id: 'evt-anchor-2', kind: 'BUSINESS_EVENT', task_id: 'task-anchor' });
  const afterAppend = store.verifyChain(checkpoint);
  assert.equal(afterAppend.valid, true);
  assert.equal(afterAppend.local_valid, true);
  assert.equal(afterAppend.checked, 2);
  assert.equal(afterAppend.checkpoint.status, 'MATCHED');
  assert.equal(afterAppend.checkpoint.anchored_count, 1);
  assert.equal(afterAppend.checkpoint.unanchored_count, 1);
  assert.equal(new AuditRecorder(store).verifyChain(checkpoint).checkpoint.status, 'MATCHED');
  assert.equal(store.verifyChain().checkpoint.status, 'NOT_CHECKED');
  assert.equal(store.getAudit(first.audit_id).record_hash, checkpoint.record_hash);

  const wrongHash = store.verifyChain({ ...checkpoint, record_hash: '0'.repeat(64) });
  assert.equal(wrongHash.valid, false);
  assert.equal(wrongHash.local_valid, true);
  assert.equal(wrongHash.checked, 2);
  assert.equal(wrongHash.reason, 'CHECKPOINT_HASH_MISMATCH');
  assert.equal(wrongHash.first_break_sequence, checkpoint.sequence);
  const wrongCount = store.verifyChain({ ...checkpoint, count: 2 });
  assert.equal(wrongCount.reason, 'CHECKPOINT_COUNT_MISMATCH');
  assert.throws(() => store.verifyChain({ sequence: 1, count: 1, record_hash: 'bad' }), TypeError);

  store.db.prepare("UPDATE audit_records SET result = 'TAMPERED' WHERE audit_id = ?").run(second.audit_id);
  const tailBreak = store.verifyChain(checkpoint);
  assert.equal(tailBreak.valid, false);
  assert.equal(tailBreak.local_valid, false);
  assert.equal(tailBreak.reason, 'RECORD_HASH_MISMATCH');
  assert.equal(tailBreak.checkpoint.status, 'MATCHED');
  assert.equal(tailBreak.checkpoint.anchored_count, 1);

  const badCheckpointAndBadTail = store.verifyChain({ ...checkpoint, record_hash: '0'.repeat(64) });
  assert.equal(badCheckpointAndBadTail.valid, false);
  assert.equal(badCheckpointAndBadTail.local_valid, false);
  assert.equal(badCheckpointAndBadTail.reason, 'RECORD_HASH_MISMATCH');
  assert.equal(badCheckpointAndBadTail.first_break_sequence, 2);
  assert.equal(badCheckpointAndBadTail.checkpoint.status, 'MISMATCH');
  assert.equal(badCheckpointAndBadTail.checkpoint.reason, 'CHECKPOINT_HASH_MISMATCH');
}));

test('external head checkpoint detects otherwise invisible tail deletion', () => withStore((store) => {
  const demo = recordSimulatedTransfer(store);
  const before = store.verifyChain();
  const checkpoint = { sequence: before.head_sequence, count: before.checked, record_hash: before.head_hash };
  store.db.prepare('DELETE FROM audit_records WHERE audit_id = ?').run(demo.records.at(-1).audit_id);
  assert.equal(store.verifyChain().valid, true);
  const anchored = store.verifyChain(checkpoint);
  assert.equal(anchored.valid, false);
  assert.equal(anchored.local_valid, true);
  assert.equal(anchored.reason, 'CHECKPOINT_RECORD_MISSING');
  assert.equal(anchored.first_break_sequence, checkpoint.sequence);
  assert.equal(anchored.checkpoint.status, 'MISSING');
}));

test('a complete local chain rewrite passes local verification but fails an independently retained checkpoint', () => withStore((store) => {
  const demo = recordSimulatedTransfer(store);
  const before = store.verifyChain();
  const checkpoint = { sequence: before.head_sequence, count: before.checked, record_hash: before.head_hash };
  const inputFields = [
    'event_id', 'kind', 'task_id', 'node_id', 'operation_id', 'user_id', 'action',
    'timestamp', 'security_result', 'confirmation', 'auth_result', 'result', 'tool_name', 'details',
  ];
  const hashFields = [
    'audit_id', 'event_id', 'kind', 'task_id', 'node_id', 'operation_id', 'user_id',
    'action', 'timestamp', 'recorded_at', 'security_result', 'confirmation',
    'auth_result', 'result', 'tool_name', 'details', 'prev_hash',
  ];
  const hash = (value) => createHash('sha256').update(stableJson(value), 'utf8').digest('hex');
  store.db.exec('BEGIN IMMEDIATE');
  try {
    store.db.prepare("UPDATE audit_records SET result = 'TAMPERED' WHERE audit_id = ?").run(demo.records[0].audit_id);
    let previousHash = null;
    for (const row of store.db.prepare('SELECT * FROM audit_records ORDER BY sequence').all()) {
      const material = { ...row, details: JSON.parse(row.details_json), prev_hash: previousHash };
      const fingerprint = hash(Object.fromEntries(inputFields.map((field) => [field, material[field] ?? null])));
      const recordHash = hash({
        hash_version: 'local-js-v2', sequence: row.sequence,
        ...Object.fromEntries(hashFields.map((field) => [field, material[field]])),
      });
      store.db.prepare('UPDATE audit_records SET event_fingerprint = ?, prev_hash = ?, record_hash = ?, hash_version = ? WHERE sequence = ?')
        .run(fingerprint, previousHash, recordHash, 'local-js-v2', row.sequence);
      previousHash = recordHash;
    }
    store.db.exec('COMMIT');
  } catch (error) {
    store.db.exec('ROLLBACK');
    throw error;
  }
  const localOnly = store.verifyChain();
  assert.equal(localOnly.valid, true);
  assert.equal(localOnly.checkpoint.status, 'NOT_CHECKED');
  assert.notEqual(localOnly.head_hash, before.head_hash);
  const compared = store.verifyChain(checkpoint);
  assert.equal(compared.valid, false);
  assert.equal(compared.local_valid, true);
  assert.equal(compared.reason, 'CHECKPOINT_HASH_MISMATCH');
  assert.equal(compared.first_break_sequence, checkpoint.sequence);
}));

test('verifyChain detects interior deletion but cannot detect tail deletion without an external anchor', () => {
  withStore((store) => {
    const demo = recordSimulatedTransfer(store);
    store.db.prepare('DELETE FROM audit_records WHERE audit_id = ?').run(demo.records[1].audit_id);
    const check = store.verifyChain();
    assert.equal(check.valid, false);
    assert.equal(check.reason, 'SEQUENCE_GAP');
    assert.equal(check.broken_audit_id, demo.records[2].audit_id);
  });
  withStore((store) => {
    const demo = recordSimulatedTransfer(store);
    store.db.prepare('DELETE FROM audit_records WHERE audit_id = ?').run(demo.records[3].audit_id);
    const check = store.verifyChain();
    assert.equal(check.valid, true);
    assert.equal(check.checked, 3);
  });
});

test('saved timeline is still available after reopening the database', () => {
  const dir = mkdtempSync(join(tmpdir(), 'safeflow-audit-persist-'));
  const dbPath = join(dir, 'audit.db');
  const first = new AuditStore(dbPath);
  const demo = recordSimulatedTransfer(first);
  first.close();
  const reopened = new AuditStore(dbPath);
  try {
    assert.equal(reopened.getLatestTask()[0].task_id, demo.task_id);
    assert.equal(reopened.getLatestTask().length, 4);
    const original = demo.records[0];
    const replay = reopened.recordAudit({
      event_id: original.event_id, kind: original.kind, task_id: original.task_id,
      node_id: original.node_id, operation_id: original.operation_id, user_id: original.user_id,
      action: original.action, timestamp: original.timestamp, security_result: original.security_result,
      confirmation: original.confirmation, auth_result: original.auth_result,
      result: original.result, tool_name: original.tool_name, details: original.details,
    });
    assert.equal(replay.audit_id, original.audit_id);
    assert.equal(replay.record_hash, original.record_hash);
    assert.equal(replay.timestamp, original.timestamp);
    assert.equal(replay.recorded_at, original.recorded_at);
    assert.equal(reopened.getAuditsByTask(demo.task_id).length, 4);
    assert.equal(reopened.verifyChain().valid, true);
  } finally {
    reopened.close();
    rmSync(dir, { recursive: true, force: true });
  }
});

test('adding fingerprint version to an existing database does not rewrite IDs, times, or hashes', () => {
  const dir = mkdtempSync(join(tmpdir(), 'safeflow-audit-fingerprint-migration-'));
  const dbPath = join(dir, 'audit.db');
  const original = new AuditStore(dbPath);
  const saved = original.recordAudit({
    event_id: 'evt-before-fingerprint-version', kind: 'TOOL_COMPLETED', task_id: 'task-fingerprint-version',
    timestamp: '2026-10-17T14:00:00+08:00', details: { amount: 'CNY 200.00' },
  });
  original.close();
  const oldSchema = new DatabaseSync(dbPath);
  oldSchema.exec('ALTER TABLE audit_records DROP COLUMN fingerprint_version');
  oldSchema.close();
  const reopened = new AuditStore(dbPath);
  try {
    const row = reopened.db.prepare('SELECT * FROM audit_records WHERE audit_id = ?').get(saved.audit_id);
    assert.equal(row.fingerprint_version, null);
    assert.equal(row.record_hash, saved.record_hash);
    assert.equal(row.timestamp, saved.timestamp);
    assert.equal(row.recorded_at, saved.recorded_at);
    assert.equal(row.audit_id, saved.audit_id);
    assert.equal(reopened.verifyChain().valid, true);
    assert.equal(reopened.verifyChain().legacy_fingerprint_count, 1);
    assert.equal(reopened.migrationBackupPath, undefined);
    const replay = reopened.recordAudit({
      event_id: saved.event_id, kind: saved.kind, task_id: saved.task_id, node_id: saved.node_id,
      operation_id: saved.operation_id, user_id: saved.user_id, action: saved.action,
      timestamp: saved.timestamp, security_result: saved.security_result, confirmation: saved.confirmation,
      auth_result: saved.auth_result, result: saved.result, tool_name: saved.tool_name, details: saved.details,
    });
    assert.equal(replay.audit_id, saved.audit_id);
    assert.equal(replay.record_hash, saved.record_hash);
    assert.throws(() => reopened.recordAudit({
      event_id: saved.event_id, kind: saved.kind, task_id: saved.task_id, node_id: saved.node_id,
      operation_id: saved.operation_id, user_id: saved.user_id, action: saved.action,
      timestamp: saved.timestamp, security_result: saved.security_result, confirmation: saved.confirmation,
      auth_result: saved.auth_result, result: 'FAILED', tool_name: saved.tool_name, details: saved.details,
    }), AuditConflictError);
    const appended = reopened.recordAudit({
      event_id: 'evt-after-fingerprint-version', kind: 'TOOL_COMPLETED', task_id: 'task-fingerprint-version',
      details: { amount: 'CNY 200.00' },
    });
    const appendedRow = reopened.db.prepare('SELECT sequence,fingerprint_version FROM audit_records WHERE audit_id = ?').get(appended.audit_id);
    assert.equal(appendedRow.sequence, 2);
    assert.equal(appendedRow.fingerprint_version, 'sha256-v1');
    assert.equal(appended.prev_hash, saved.record_hash);
    assert.equal(reopened.verifyChain().valid, true);
  } finally {
    reopened.close();
    rmSync(dir, { recursive: true, force: true });
  }
});

test('nullable-field migration retains an existing fingerprint version marker', () => {
  const dir = mkdtempSync(join(tmpdir(), 'safeflow-audit-nullable-migration-'));
  const dbPath = join(dir, 'audit.db');
  const original = new AuditStore(dbPath);
  const saved = original.recordAudit({
    event_id: 'evt-strict-old-schema', kind: 'TOOL_COMPLETED', task_id: 'task-strict-old-schema',
    operation_id: 'op-strict-old-schema', timestamp: '2026-10-17T14:00:00+08:00',
    details: { amount: 'CNY 200.00' },
  });
  original.close();
  const oldSchema = new DatabaseSync(dbPath);
  oldSchema.exec(`BEGIN IMMEDIATE;
    CREATE TABLE audit_records_strict (
      sequence INTEGER PRIMARY KEY AUTOINCREMENT, audit_id TEXT NOT NULL UNIQUE,
      event_id TEXT NOT NULL UNIQUE, event_fingerprint TEXT NOT NULL, fingerprint_version TEXT,
      kind TEXT NOT NULL, task_id TEXT NOT NULL, node_id TEXT, operation_id TEXT NOT NULL,
      user_id TEXT, action TEXT, timestamp TEXT, recorded_at TEXT NOT NULL,
      security_result TEXT, confirmation TEXT, auth_result TEXT, result TEXT, tool_name TEXT,
      details_json TEXT NOT NULL, prev_hash TEXT, record_hash TEXT NOT NULL, hash_version TEXT
    );
    INSERT INTO audit_records_strict SELECT * FROM audit_records;
    DROP TABLE audit_records;
    ALTER TABLE audit_records_strict RENAME TO audit_records;
    COMMIT;`);
  oldSchema.close();
  const migrated = new AuditStore(dbPath);
  try {
    const row = migrated.db.prepare('SELECT * FROM audit_records WHERE audit_id = ?').get(saved.audit_id);
    assert.equal(row.fingerprint_version, 'sha256-v1');
    assert.equal(row.record_hash, saved.record_hash);
    assert.equal(row.timestamp, saved.timestamp);
    assert.equal(row.recorded_at, saved.recorded_at);
    assert.equal(migrated.verifyChain().valid, true);
    assert.equal(migrated.verifyChain().legacy_fingerprint_count, 0);
    assert.ok(migrated.migrationBackupPath);
  } finally {
    const backup = migrated.migrationBackupPath;
    migrated.close();
    rmSync(dir, { recursive: true, force: true });
    if (backup && existsSync(backup)) rmSync(backup, { force: true });
  }
});

test('legacy database migration preserves IDs and times and re-links hashes', () => {
  const dir = mkdtempSync(join(tmpdir(), 'safeflow-audit-legacy-'));
  const dbPath = join(dir, 'legacy.db');
  const timestamp = '2026-10-06T06:00:00.000Z';
  const event = {
    audit_id: 'audit-legacy',
    event_id: 'evt-legacy', kind: 'TOOL_COMPLETED', task_id: 'task-legacy', node_id: 'node-demo',
    operation_id: 'op-legacy', user_id: 'demo-user', action: 'TRANSFER_SUCCEEDED', timestamp,
    security_result: 'DEMO_ONLY', confirmation: 'DEMO_ONLY', auth_result: 'DEMO_ONLY',
    result: 'SUCCEEDED', tool_name: 'execute_transfer', details: {}, prev_hash: null,
  };
  const hash = createHash('sha256').update(stableJson(event), 'utf8').digest('hex');
  const seed = new DatabaseSync(dbPath);
  seed.exec(`CREATE TABLE audit_records (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT, audit_id TEXT NOT NULL UNIQUE, event_id TEXT NOT NULL UNIQUE,
    event_fingerprint TEXT NOT NULL, kind TEXT NOT NULL, task_id TEXT NOT NULL, node_id TEXT,
    operation_id TEXT NOT NULL, user_id TEXT NOT NULL, action TEXT NOT NULL, timestamp TEXT NOT NULL,
    security_result TEXT NOT NULL, confirmation TEXT NOT NULL, auth_result TEXT NOT NULL, result TEXT NOT NULL,
      tool_name TEXT, details_json TEXT NOT NULL, prev_hash TEXT, record_hash TEXT NOT NULL, hash_version TEXT
  )`);
  seed.prepare(`INSERT INTO audit_records (
    audit_id,event_id,event_fingerprint,kind,task_id,node_id,operation_id,user_id,action,timestamp,
    security_result,confirmation,auth_result,result,tool_name,details_json,prev_hash,record_hash
  ) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`).run(
    'audit-legacy', event.event_id, eventFingerprint(event), event.kind, event.task_id, event.node_id,
    event.operation_id, event.user_id, event.action, event.timestamp, event.security_result,
    event.confirmation, event.auth_result, event.result, event.tool_name, '{}', null, hash,
  );
  seed.close();

  const migrated = new AuditStore(dbPath);
  try {
    const record = migrated.getAudit('audit-legacy');
    assert.equal(record.audit_id, 'audit-legacy');
    assert.equal(record.event_id, 'evt-legacy');
    assert.equal(record.hash_version, 'local-js-v1');
    assert.equal(record.timestamp, timestamp);
    assert.equal(record.recorded_at, timestamp);
    const migratedColumns = migrated.db.prepare('PRAGMA table_info(audit_records)').all();
    for (const columnName of ['operation_id', 'user_id', 'action', 'timestamp', 'security_result', 'confirmation', 'auth_result', 'result']) {
      assert.equal(migratedColumns.find((column) => column.name === columnName).notnull, 0);
    }
    assert.equal(migrated.verifyChain().valid, true);
    const appended = migrated.recordAudit({
      event_id: 'evt-after-legacy', kind: 'TOOL_COMPLETED', task_id: 'task-legacy',
      details: { source: 'after-migration' },
    });
    const appendedRow = migrated.db.prepare('SELECT sequence,fingerprint_version FROM audit_records WHERE audit_id = ?').get(appended.audit_id);
    assert.equal(appendedRow.sequence, 2);
    assert.equal(appended.hash_version, 'local-js-v2');
    assert.equal(appendedRow.fingerprint_version, 'sha256-v1');
    assert.equal(appended.prev_hash, record.record_hash);
    assert.equal(migrated.verifyChain().valid, true);
    assert.ok(migrated.migrationBackupPath);
    assert.equal(existsSync(migrated.migrationBackupPath), true);
  } finally {
    const backup = migrated.migrationBackupPath;
    migrated.close();
    rmSync(dir, { recursive: true, force: true });
    if (backup && existsSync(backup)) rmSync(backup, { force: true });
  }
});
