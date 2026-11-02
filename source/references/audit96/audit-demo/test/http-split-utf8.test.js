import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdtempSync, rmSync } from 'node:fs';
import { request as httpRequest } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createServer as createNetServer } from 'node:net';
import { DatabaseSync } from 'node:sqlite';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

async function availablePort() {
  const probe = createNetServer();
  await new Promise((resolve, reject) => {
    probe.once('error', reject);
    probe.listen(0, '127.0.0.1', resolve);
  });
  const { port } = probe.address();
  await new Promise((resolve) => probe.close(resolve));
  return port;
}

async function waitForHealth(baseUrl, child) {
  for (let attempt = 0; attempt < 80; attempt += 1) {
    if (child.exitCode !== null) throw new Error(`audit server exited with ${child.exitCode}`);
    try {
      if ((await fetch(`${baseUrl}/api/health`)).ok) return;
    } catch {}
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error('audit server did not become healthy within 8 seconds');
}

// Each request.write() sends a separate HTTP chunk. Both boundaries below
// fall inside a UTF-8 code point, not merely between JSON characters.
async function postWithSplitUtf8(baseUrl, payload) {
  const bytes = Buffer.from(JSON.stringify(payload), 'utf8');
  const chineseStart = bytes.indexOf(Buffer.from('锁', 'utf8'));
  const emojiStart = bytes.indexOf(Buffer.from('😀', 'utf8'));
  assert.ok(chineseStart >= 0 && emojiStart > chineseStart);
  const chineseSplit = chineseStart + 1;
  const emojiSplit = emojiStart + 2;
  assert.equal(bytes[chineseSplit] & 0xc0, 0x80);
  assert.equal(bytes[emojiSplit] & 0xc0, 0x80);

  const response = await new Promise((resolve, reject) => {
    const req = httpRequest(`${baseUrl}/api/audit/outbox/v1`, {
      method: 'POST',
      headers: { 'content-type': 'application/json; charset=utf-8' },
    }, resolve);
    req.once('error', reject);
    req.write(bytes.subarray(0, chineseSplit));
    setTimeout(() => {
      req.write(bytes.subarray(chineseSplit, emojiSplit));
      setTimeout(() => req.end(bytes.subarray(emojiSplit)), 15);
    }, 15);
  });
  const chunks = [];
  for await (const chunk of response) chunks.push(chunk);
  return { status: response.statusCode, body: JSON.parse(Buffer.concat(chunks).toString('utf8')) };
}

test('real HTTP split Chinese and emoji bytes: first 201, same delivery 200, changed event 409, original persists', async (t) => {
  const directory = mkdtempSync(join(tmpdir(), 'safeflow-audit-split-utf8-'));
  const dbPath = join(directory, 'audit.db');
  const port = await availablePort();
  const child = spawn(process.execPath, [fileURLToPath(new URL('../src/server.js', import.meta.url))], {
    cwd: fileURLToPath(new URL('..', import.meta.url)),
    env: { ...process.env, PORT: String(port), AUDIT_DB_PATH: dbPath },
    windowsHide: true,
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  let stderr = '';
  child.stderr.setEncoding('utf8');
  child.stderr.on('data', (chunk) => { stderr += chunk; });
  t.after(async () => {
    if (child.exitCode === null && child.signalCode === null) {
      const exited = once(child, 'exit');
      child.kill('SIGTERM');
      await Promise.race([exited, new Promise((resolve) => setTimeout(resolve, 5000))]);
    }
    rmSync(directory, { recursive: true, force: true });
  });
  const baseUrl = `http://127.0.0.1:${port}`;
  try { await waitForHealth(baseUrl, child); }
  catch (error) { throw new Error(`${error.message}${stderr ? `\n${stderr}` : ''}`); }

  const original = {
    event_id: 'evt-split-utf8-http-test',
    envelope: {
      storage_version: 1,
      owner_id: 'test-only-owner',
      public_event: {
        sequence: 1,
        event_id: 'evt-split-utf8-http-test',
        task_id: 'task-split-utf8-http-test',
        operation_id: 'op-split-utf8-http-test',
        kind: 'TEST_ONLY_OPERATION_TRANSITIONED',
        task_revision: 1,
        operation_revision: 1,
        details: { note: '锁卡完成😀（仅供 UTF-8 验收）' },
        occurred_at: '2026-10-22T08:00:00.000Z',
      },
    },
    digest: 'a'.repeat(64),
    historical: false,
    enqueued_at: '2026-10-22T08:00:01.000Z',
  };

  const first = await postWithSplitUtf8(baseUrl, original);
  assert.equal(first.status, 201);
  assert.equal(first.body.accepted, true);
  assert.equal(first.body.duplicate, false);
  const firstAuditId = first.body.audit_id;
  assert.ok(firstAuditId);

  const duplicate = await postWithSplitUtf8(baseUrl, original);
  assert.equal(duplicate.status, 200);
  assert.equal(duplicate.body.accepted, true);
  assert.equal(duplicate.body.duplicate, true);
  assert.equal(duplicate.body.audit_id, firstAuditId);

  const changed = structuredClone(original);
  changed.envelope.public_event.details.note = '锁卡失败😀（同事件号但内容改变）';
  const conflict = await postWithSplitUtf8(baseUrl, changed);
  assert.equal(conflict.status, 409);
  assert.equal(conflict.body.error, 'EVENT_ID_CONFLICT');

  const query = await fetch(`${baseUrl}/api/audit?event_id=${encodeURIComponent(original.event_id)}`);
  assert.equal(query.status, 200);
  const saved = await query.json();
  assert.equal(saved.records.length, 1);
  assert.equal(saved.records[0].audit_id, firstAuditId);
  assert.equal(saved.records[0].details.foundation.public_event.details.note, original.envelope.public_event.details.note);
  const db = new DatabaseSync(dbPath);
  try {
    assert.equal(db.prepare('SELECT COUNT(*) AS count FROM audit_records WHERE event_id = ?').get(original.event_id).count, 1);
  } finally {
    db.close();
  }
  const chain = await (await fetch(`${baseUrl}/api/verify-chain`)).json();
  assert.equal(chain.valid, true);
});
