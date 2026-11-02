import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createServer as createNetServer } from 'node:net';
import { DatabaseSync } from 'node:sqlite';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const allocatedPorts = new Set();
const fingerprintFields = ['event_id', 'kind', 'task_id', 'node_id', 'operation_id', 'user_id', 'action', 'timestamp', 'security_result', 'confirmation', 'auth_result', 'result', 'tool_name', 'details'];

function stableJson(value) {
  if (Array.isArray(value)) return `[${value.map(stableJson).join(',')}]`;
  if (value && typeof value === 'object') return `{${Object.keys(value).sort().map((key) => `${JSON.stringify(key)}:${stableJson(value[key])}`).join(',')}}`;
  return JSON.stringify(value);
}

function eventFingerprint(event) {
  return createHash('sha256').update(stableJson(Object.fromEntries(
    fingerprintFields.map((field) => [field, field === 'details' ? event[field] ?? {} : event[field] ?? null]),
  )), 'utf8').digest('hex');
}

async function unusedLoopbackPort() {
  for (let attempt = 0; attempt < 20; attempt += 1) {
    const probe = createNetServer();
    await new Promise((resolve, reject) => {
      probe.once('error', reject);
      probe.listen(0, '127.0.0.1', resolve);
    });
    const { port } = probe.address();
    await new Promise((resolve) => probe.close(resolve));
    if (!allocatedPorts.has(port)) {
      allocatedPorts.add(port);
      return port;
    }
  }
  throw new Error('could not allocate a distinct loopback port for audit server tests');
}

async function waitForHealth(baseUrl, child) {
  for (let attempt = 0; attempt < 80; attempt += 1) {
    if (child.exitCode !== null) throw new Error(`audit server exited early with code ${child.exitCode}`);
    try {
      const response = await fetch(`${baseUrl}/api/health`);
      if (response.ok) return;
    } catch {}
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error('audit server did not become healthy within 8 seconds');
}

test('HTTP receiver recovers by event_id after response loss and process restart', async (t) => {
  const directory = mkdtempSync(join(tmpdir(), 'safeflow-audit-http-'));
  const dbPath = join(directory, 'audit.db');
  const serverPath = new URL('../src/server.js', import.meta.url);
  const children = [];
  const launchServer = async (port) => {
    const child = spawn(process.execPath, [fileURLToPath(serverPath)], {
      cwd: fileURLToPath(new URL('..', import.meta.url)),
      env: { ...process.env, PORT: String(port), AUDIT_DB_PATH: dbPath },
      windowsHide: true,
      stdio: ['ignore', 'ignore', 'pipe'],
    });
    children.push(child);
    let stderr = '';
    child.stderr.setEncoding('utf8');
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    const baseUrl = `http://127.0.0.1:${port}`;
    try { await waitForHealth(baseUrl, child); }
    catch (error) { throw new Error(`${error.message}${stderr ? `\n${stderr}` : ''}`); }
    return { child, baseUrl };
  };
  t.after(async () => {
    for (const child of children) {
      if (child.exitCode === null && child.signalCode === null) {
        const exited = once(child, 'exit');
        child.kill('SIGTERM');
        await Promise.race([exited, new Promise((resolve) => setTimeout(resolve, 5000))]);
      }
    }
    rmSync(directory, { recursive: true, force: true });
  });

  let { child, baseUrl } = await launchServer(await unusedLoopbackPort());

  const event = {
    event_id: 'evt-http-ack-recovery', kind: 'TOOL_COMPLETED', task_id: 'task-http-ack',
    operation_id: 'op-http-ack', user_id: 'demo-user', action: 'TRANSFER_SUCCEEDED',
    occurred_at: '2026-10-17T00:00:00Z', result: 'SUCCEEDED', details: { source: 'http-test' },
  };
  const postEvent = () => fetch(`${baseUrl}/api/audit/events`, {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(event),
  });

  const firstResponse = await postEvent();
  assert.equal(firstResponse.status, 201);
  await firstResponse.body.cancel();

  const recoveredResponse = await fetch(`${baseUrl}/api/audit?event_id=${encodeURIComponent(event.event_id)}`);
  assert.equal(recoveredResponse.status, 200);
  const recovered = await recoveredResponse.json();
  assert.equal(recovered.records.length, 1);
  const originalAuditId = recovered.records[0].audit_id;
  assert.ok(Number.isFinite(Date.parse(recovered.records[0].recorded_at)));

  const retryResponse = await postEvent();
  assert.equal(retryResponse.status, 200);
  const retry = await retryResponse.json();
  assert.equal(retry.duplicate, true);
  assert.equal(retry.audit_id, originalAuditId);

  const conflictResponse = await fetch(`${baseUrl}/api/audit/events`, {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ ...event, result: 'FAILED' }),
  });
  assert.equal(conflictResponse.status, 409);

  const taskResponse = await fetch(`${baseUrl}/api/audit?task_id=${encodeURIComponent(event.task_id)}`);
  const task = await taskResponse.json();
  assert.equal(task.records.length, 1);
  const chainResponse = await fetch(`${baseUrl}/api/verify-chain`);
  assert.equal((await chainResponse.json()).valid, true);

  const stopped = once(child, 'exit');
  child.kill('SIGTERM');
  await stopped;
  ({ child, baseUrl } = await launchServer(await unusedLoopbackPort()));
  const afterRestartResponse = await fetch(`${baseUrl}/api/audit?event_id=${encodeURIComponent(event.event_id)}`);
  const afterRestart = await afterRestartResponse.json();
  assert.equal(afterRestart.records.length, 1);
  assert.equal(afterRestart.records[0].audit_id, originalAuditId);
  const afterRestartChain = await fetch(`${baseUrl}/api/verify-chain`);
  const intactChain = await afterRestartChain.json();
  assert.equal(intactChain.valid, true);
  assert.equal(intactChain.external_anchor, 'not_checked');
  assert.equal(Object.hasOwn(intactChain, 'broken_audit_id'), false);
  assert.equal(Object.hasOwn(intactChain, 'head_hash'), false);

  const tamper = new DatabaseSync(dbPath);
  tamper.prepare('UPDATE audit_records SET result = ? WHERE event_id = ?').run('TAMPERED', event.event_id);
  tamper.close();
  const failedChain = await (await fetch(`${baseUrl}/api/verify-chain`)).json();
  assert.equal(failedChain.valid, false);
  assert.equal(failedChain.first_break_sequence, 1);
  assert.equal(failedChain.reason, 'RECORD_HASH_MISMATCH');
  assert.equal(Object.hasOwn(failedChain, 'broken_audit_id'), false);
  assert.equal(Object.hasOwn(failedChain, 'record_hash'), false);
  assert.equal(JSON.stringify(failedChain).includes(originalAuditId), false);

  const damageDetails = new DatabaseSync(dbPath);
  damageDetails.prepare('UPDATE audit_records SET details_json = ? WHERE event_id = ?').run('{bad JSON', event.event_id);
  damageDetails.close();
  const malformedResponse = await fetch(`${baseUrl}/api/verify-chain`);
  assert.equal(malformedResponse.status, 200);
  const malformedChain = await malformedResponse.json();
  assert.equal(malformedChain.valid, false);
  assert.equal(malformedChain.reason, 'INVALID_DETAILS_JSON');
  assert.equal(malformedChain.first_break_sequence, 1);

  const corruptedRetry = await postEvent();
  assert.equal(corruptedRetry.status, 503);
  assert.equal((await corruptedRetry.json()).error, 'AUDIT_STORAGE_UNAVAILABLE');
  const invalidRequest = await fetch(`${baseUrl}/api/audit/events`, {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: '{bad JSON',
  });
  assert.equal(invalidRequest.status, 400);
  assert.equal((await invalidRequest.json()).error, 'INVALID_AUDIT_EVENT');
});

test('foundation Postgres outbox envelope is durably accepted and safely redeliverable', async (t) => {
  const directory = mkdtempSync(join(tmpdir(), 'safeflow-audit-outbox-'));
  const dbPath = join(directory, 'audit.db');
  const serverPath = new URL('../src/server.js', import.meta.url);
  const children = [];
  const launchServer = async () => {
    const port = await unusedLoopbackPort();
    const child = spawn(process.execPath, [fileURLToPath(serverPath)], {
      cwd: fileURLToPath(new URL('..', import.meta.url)),
      env: { ...process.env, PORT: String(port), AUDIT_DB_PATH: dbPath },
      windowsHide: true,
      stdio: ['ignore', 'ignore', 'pipe'],
    });
    children.push(child);
    let stderr = '';
    child.stderr.setEncoding('utf8');
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    const baseUrl = `http://127.0.0.1:${port}`;
    try { await waitForHealth(baseUrl, child); }
    catch (error) { throw new Error(`${error.message}${stderr ? `\n${stderr}` : ''}`); }
    return { child, baseUrl };
  };
  const stopServer = async (child) => {
    if (child.exitCode !== null || child.signalCode !== null) return;
    const exited = once(child, 'exit');
    child.kill('SIGTERM');
    await Promise.race([exited, new Promise((resolve) => setTimeout(resolve, 5000))]);
  };
  t.after(async () => {
    for (const child of children) await stopServer(child);
    rmSync(directory, { recursive: true, force: true });
  });

  const input = {
    event_id: 'evt-foundation-outbox-http',
    envelope: {
      storage_version: 1,
      owner_id: 'foundation-owner-not-authenticated',
      public_event: {
        sequence: 4,
        event_id: 'evt-foundation-outbox-http',
        task_id: 'task-foundation-outbox-http',
        operation_id: 'op-foundation-outbox-http',
        kind: 'OPERATION_TRANSITIONED',
        task_revision: 3,
        operation_revision: 2,
        details: { state: 'PROCESSING', sample: true },
        occurred_at: '2026-10-19T09:30:00.000Z',
      },
    },
    digest: 'b'.repeat(64),
    historical: false,
    enqueued_at: '2026-10-19T09:30:01.000Z',
  };
  const send = (baseUrl, payload = input) => fetch(`${baseUrl}/api/audit/outbox/v1`, {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(payload),
  });

  let { child, baseUrl } = await launchServer();
  const earlierNonDemoTransfer = await fetch(`${baseUrl}/api/audit/events`, {
    method: 'POST', headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      event_id: 'evt-earlier-non-demo-transfer', kind: 'TOOL_COMPLETED', task_id: input.envelope.public_event.task_id,
      operation_id: 'op-earlier-non-demo-transfer', action: 'TRANSFER_SUCCEEDED', result: 'SUCCEEDED',
      details: { source: 'acceptance-test-only', simulated: false },
    }),
  });
  assert.equal(earlierNonDemoTransfer.status, 201);
  const acceptedResponse = await send(baseUrl);
  assert.equal(acceptedResponse.status, 201);
  const accepted = await acceptedResponse.json();
  assert.equal(accepted.accepted, true);
  assert.equal(accepted.duplicate, false);
  assert.equal(accepted.event_id, input.event_id);
  assert.equal(accepted.task_id, input.envelope.public_event.task_id);
  assert.equal(accepted.operation_id, input.envelope.public_event.operation_id);
  assert.equal(accepted.digest_echo, input.digest);
  const firstAuditId = accepted.audit_id;

  const concurrentRedeliveries = await Promise.all(Array.from({ length: 8 }, () => send(baseUrl)));
  const concurrentResults = await Promise.all(concurrentRedeliveries.map(async (response) => ({
    status: response.status,
    body: await response.json(),
  })));
  assert.equal(concurrentResults.filter((result) => result.status === 201).length, 0);
  assert.equal(concurrentResults.every((result) => result.status === 200 && result.body.duplicate === true && result.body.audit_id === firstAuditId), true);

  const query = await (await fetch(`${baseUrl}/api/audit?event_id=${encodeURIComponent(input.event_id)}`)).json();
  assert.equal(query.records.length, 1);
  assert.equal(query.records[0].audit_id, firstAuditId);
  assert.equal(query.records[0].timestamp, null);
  assert.equal(query.records[0].user_id, null);
  assert.deepEqual(query.records[0].details.foundation.public_event, input.envelope.public_event);
  const latest = await (await fetch(`${baseUrl}/api/audit/latest`)).json();
  assert.equal(latest.records.length, 2);
  assert.equal(latest.records.at(-1).operation_id, input.envelope.public_event.operation_id);
  const absentReceipt = await fetch(`${baseUrl}/api/receipt/${encodeURIComponent(input.envelope.public_event.operation_id)}?view=payer`);
  assert.equal(absentReceipt.status, 404);
  assert.equal((await absentReceipt.json()).error, 'AUDIT_NOT_FOUND');
  assert.equal((await (await fetch(`${baseUrl}/api/verify-chain`)).json()).valid, true);

  const simulatedTransferResponse = await fetch(`${baseUrl}/api/demo-transfer`, {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ outcome: 'success' }),
  });
  assert.equal(simulatedTransferResponse.status, 201);
  const simulatedTransfer = await simulatedTransferResponse.json();
  const availableReceipt = await fetch(`${baseUrl}/api/receipt/${encodeURIComponent(simulatedTransfer.operation_id)}?view=payer`);
  assert.equal(availableReceipt.status, 200);
  const receiptBody = await availableReceipt.json();
  assert.equal(receiptBody.receipt.simulated, true);
  assert.equal(receiptBody.receipt.receipt_ref, simulatedTransfer.operation_id.slice(-8).toUpperCase());

  await stopServer(child);
  ({ child, baseUrl } = await launchServer());
  const retryResponse = await send(baseUrl);
  assert.equal(retryResponse.status, 200);
  const retried = await retryResponse.json();
  assert.equal(retried.duplicate, true);
  assert.equal(retried.audit_id, firstAuditId);

  const changed = structuredClone(input);
  changed.envelope.public_event.details.state = 'SUCCEEDED';
  const conflict = await send(baseUrl, changed);
  assert.equal(conflict.status, 409);
  assert.equal((await conflict.json()).error, 'EVENT_ID_CONFLICT');
  const mismatch = await send(baseUrl, { ...input, event_id: 'different-top-level-id' });
  assert.equal(mismatch.status, 400);
});

test('HTTP receiver refuses a downgraded legacy fingerprint without overwriting the original or acknowledging', async (t) => {
  const directory = mkdtempSync(join(tmpdir(), 'safeflow-audit-fingerprint-http-'));
  const dbPath = join(directory, 'audit.db');
  const port = await unusedLoopbackPort();
  const child = spawn(process.execPath, [fileURLToPath(new URL('../src/server.js', import.meta.url))], {
    cwd: fileURLToPath(new URL('..', import.meta.url)),
    env: { ...process.env, PORT: String(port), AUDIT_DB_PATH: dbPath },
    windowsHide: true,
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  let stderr = '';
  child.stderr.setEncoding('utf8');
  child.stderr.on('data', (chunk) => { stderr += chunk; });
  const baseUrl = `http://127.0.0.1:${port}`;
  t.after(async () => {
    if (child.exitCode === null && child.signalCode === null) {
      const exited = once(child, 'exit');
      child.kill('SIGTERM');
      await Promise.race([exited, new Promise((resolve) => setTimeout(resolve, 5000))]);
    }
    rmSync(directory, { recursive: true, force: true });
  });
  try { await waitForHealth(baseUrl, child); }
  catch (error) { throw new Error(`${error.message}${stderr ? `\n${stderr}` : ''}`); }

  const original = {
    event_id: 'evt-legacy-fingerprint-http', kind: 'TOOL_COMPLETED', task_id: 'task-legacy-fingerprint-http',
    operation_id: 'op-legacy-fingerprint-http', user_id: 'test-user', action: 'TRANSFER_SUCCEEDED',
    timestamp: '2026-10-19T10:00:00.000Z', security_result: 'ALLOW', confirmation: 'CONFIRMED',
    auth_result: 'AUTHENTICATED', result: 'SUCCEEDED', tool_name: 'simulated-transfer', details: { sample: true },
  };
  const send = (event) => fetch(`${baseUrl}/api/audit/events`, {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(event),
  });
  const accepted = await send(original);
  assert.equal(accepted.status, 201);
  const originalAck = await accepted.json();
  const alternate = { ...original, result: 'FAILED' };

  const tamper = new DatabaseSync(dbPath);
  tamper.prepare('UPDATE audit_records SET fingerprint_version = NULL, event_fingerprint = ? WHERE event_id = ?')
    .run(eventFingerprint(alternate), original.event_id);
  tamper.close();

  const rejected = await send(alternate);
  assert.equal(rejected.status, 503);
  assert.equal((await rejected.json()).error, 'AUDIT_STORAGE_UNAVAILABLE');
  const saved = await (await fetch(`${baseUrl}/api/audit?event_id=${encodeURIComponent(original.event_id)}`)).json();
  assert.equal(saved.records.length, 1);
  assert.equal(saved.records[0].audit_id, originalAck.audit_id);
  assert.equal(saved.records[0].result, 'SUCCEEDED');
  const chain = await (await fetch(`${baseUrl}/api/verify-chain`)).json();
  assert.equal(chain.valid, false);
  assert.equal(chain.reason, 'UNVERIFIABLE_LEGACY_FINGERPRINT');
});

test('service token protects every audit API when configured for base-network access', async (t) => {
  const directory = mkdtempSync(join(tmpdir(), 'safeflow-audit-token-'));
  const dbPath = join(directory, 'audit.db');
  const port = await unusedLoopbackPort();
  const child = spawn(process.execPath, [fileURLToPath(new URL('../src/server.js', import.meta.url))], {
    cwd: fileURLToPath(new URL('..', import.meta.url)),
    env: { ...process.env, PORT: String(port), AUDIT_DB_PATH: dbPath, AUDIT_SERVICE_TOKEN: 'test-only-token' },
    windowsHide: true,
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  let stderr = '';
  child.stderr.setEncoding('utf8');
  child.stderr.on('data', (chunk) => { stderr += chunk; });
  const baseUrl = `http://127.0.0.1:${port}`;
  t.after(async () => {
    if (child.exitCode === null && child.signalCode === null) {
      const exited = once(child, 'exit');
      child.kill('SIGTERM');
      await Promise.race([exited, new Promise((resolve) => setTimeout(resolve, 5000))]);
    }
    rmSync(directory, { recursive: true, force: true });
  });
  try { await waitForHealth(baseUrl, child); }
  catch (error) { throw new Error(`${error.message}${stderr ? `\n${stderr}` : ''}`); }

  assert.equal((await fetch(`${baseUrl}/api/health`)).status, 200);
  assert.equal((await fetch(`${baseUrl}/api/verify-chain`)).status, 401);
  assert.equal((await fetch(`${baseUrl}/api/verify-chain`, { headers: { authorization: 'Bearer wrong' } })).status, 401);
  assert.equal((await fetch(`${baseUrl}/api/verify-chain`, { headers: { authorization: 'Bearer test-only-token' } })).status, 200);
});

test('Jonathan billing feedback candidate survives lost ACK without a second audit record', async (t) => {
  const directory = mkdtempSync(join(tmpdir(), 'safeflow-audit-billing-http-'));
  const dbPath = join(directory, 'audit.db');
  const children = [];
  const launchServer = async () => {
    const port = await unusedLoopbackPort();
    const child = spawn(process.execPath, [fileURLToPath(new URL('../src/server.js', import.meta.url))], {
      cwd: fileURLToPath(new URL('..', import.meta.url)),
      env: { ...process.env, PORT: String(port), AUDIT_DB_PATH: dbPath },
      windowsHide: true,
      stdio: ['ignore', 'ignore', 'pipe'],
    });
    children.push(child);
    let stderr = '';
    child.stderr.setEncoding('utf8');
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    const baseUrl = `http://127.0.0.1:${port}`;
    try { await waitForHealth(baseUrl, child); }
    catch (error) { throw new Error(`${error.message}${stderr ? `\n${stderr}` : ''}`); }
    return { child, baseUrl };
  };
  const stopServer = async (child) => {
    if (child.exitCode !== null || child.signalCode !== null) return;
    const exited = once(child, 'exit');
    child.kill('SIGTERM');
    await Promise.race([exited, new Promise((resolve) => setTimeout(resolve, 5000))]);
  };
  t.after(async () => {
    for (const child of children) await stopServer(child);
    rmSync(directory, { recursive: true, force: true });
  });

  // Derived from Jonathan's fictional PG read-back case not_mine_with_card.
  // Echo has not supplied a real billing PendingPublicAudit. Public IDs, event
  // time and digest below are unmistakably test-only, not an approved mapping.
  const pending = {
    event_id: 'test-only-public-billing-event-001',
    envelope: {
      storage_version: 1,
      owner_id: 'test-only-owner-scope-not-authenticated-user',
      public_event: {
        sequence: 1,
        event_id: 'test-only-public-billing-event-001',
        task_id: 'test-only-public-billing-task-001',
        operation_id: 'card_handoff_not_mine_with_card_v1',
        kind: 'BILLING_FEEDBACK_TEST_ONLY',
        task_revision: 1,
        operation_revision: 1,
        details: {
          billing_feedback_candidate_v1: {
            original_operation_id: 'card_handoff_not_mine_with_card_v1',
            local_event_ref: { namespace: 'BILLING_FEEDBACK_RECORDED_LOCAL_V1', id: 1 },
            local_recorded_at: '2026-10-21T04:18:18.400821Z',
            choice: 'NOT_MINE',
            state: 'SAVED',
            persisted: true,
            lock_authorized: false,
            card_handoff: 'BLOCKED_CARD_ASSOCIATION_UNVERIFIED',
          },
        },
        occurred_at: '2026-10-22T00:00:00Z',
      },
    },
    digest: '0'.repeat(64),
    historical: false,
    enqueued_at: '2026-10-22T00:00:01Z',
  };
  const send = (baseUrl, payload = pending) => fetch(`${baseUrl}/api/audit/outbox/v1`, {
    method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(payload),
  });

  let { child, baseUrl } = await launchServer();
  const first = await send(baseUrl);
  assert.equal(first.status, 201);
  await first.body.cancel(); // Sender lost the response after audit commit.

  const storedBeforeRestart = await (await fetch(`${baseUrl}/api/audit?event_id=${pending.event_id}`)).json();
  assert.equal(storedBeforeRestart.records.length, 1);
  const auditId = storedBeforeRestart.records[0].audit_id;
  await stopServer(child);

  ({ child, baseUrl } = await launchServer());
  const retry = await send(baseUrl);
  assert.equal(retry.status, 200);
  const ack = await retry.json();
  assert.equal(ack.ack_version, 'audit-receiver-candidate-v1');
  assert.equal(ack.accepted, true);
  assert.equal(ack.duplicate, true);
  assert.equal(ack.audit_id, auditId);
  assert.equal(ack.event_id, pending.event_id);
  assert.equal(ack.task_id, pending.envelope.public_event.task_id);
  assert.equal(ack.operation_id, pending.envelope.public_event.operation_id);
  assert.equal(ack.digest_echo, pending.digest);

  const byOperation = await (await fetch(`${baseUrl}/api/audit?operation_id=${pending.envelope.public_event.operation_id}`)).json();
  const byTask = await (await fetch(`${baseUrl}/api/audit?task_id=${pending.envelope.public_event.task_id}`)).json();
  assert.equal(byOperation.records.length, 1);
  assert.equal(byTask.records.length, 1);
  assert.equal(byOperation.records[0].audit_id, auditId);
  assert.equal(byOperation.records[0].user_id, null, 'owner scope is not a verified user identity');
  assert.equal(byOperation.records[0].timestamp, null, 'local DB timestamp is not a confirmed business occurrence');
  assert.equal(byOperation.records[0].result, null, 'feedback choice is not a bank or card outcome');
  assert.deepEqual(byOperation.records[0].details.foundation.public_event.details, pending.envelope.public_event.details);
  assert.equal((await (await fetch(`${baseUrl}/api/verify-chain`)).json()).valid, true);

  const changed = structuredClone(pending);
  changed.envelope.public_event.details.billing_feedback_candidate_v1.choice = 'MINE';
  changed.digest = '1'.repeat(64);
  const conflict = await send(baseUrl, changed);
  assert.equal(conflict.status, 409);
  assert.equal((await conflict.json()).error, 'EVENT_ID_CONFLICT');
  const count = new DatabaseSync(dbPath);
  assert.equal(count.prepare('SELECT COUNT(*) AS n FROM audit_records WHERE event_id = ?').get(pending.event_id).n, 1);
  count.close();

  const invalid = await send(baseUrl, { event: { event_id: 1, event_kind: 'BILLING_FEEDBACK_RECORDED_LOCAL_V1' } });
  assert.equal(invalid.status, 400, 'the raw billing SavedFeedback is not a public Outbox delivery');
  const corrupted = new DatabaseSync(dbPath);
  corrupted.prepare('UPDATE audit_records SET details_json = ? WHERE event_id = ?').run('{invalid', pending.event_id);
  corrupted.close();
  const unavailable = await send(baseUrl);
  assert.equal(unavailable.status, 503);
  assert.equal((await unavailable.json()).error, 'AUDIT_STORAGE_UNAVAILABLE');
});
