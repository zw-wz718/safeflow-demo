import { createServer } from 'node:http';
import { timingSafeEqual } from 'node:crypto';
import { mkdirSync, readFileSync } from 'node:fs';
import { dirname, extname, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { AuditConflictError, AuditStore } from './audit-store.js';
import { AuditInputError, AuditRecorder } from './audit-recorder.js';
import { foundationOutboxToAuditEvent } from './foundation-outbox.js';
import { RequestTooLargeError, readJsonRequestBody } from './request-body.js';
import { recordSimulatedTransfer } from './demo-transfer.js';
import { recordSimulatedDraft } from './demo-draft.js';
import { createParticipantReceipt } from './receipt-view.js';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const publicRoot = resolve(root, 'public');
const dbPath = resolve(process.env.AUDIT_DB_PATH ?? resolve(root, 'data/audit-demo.db'));
const host = process.env.AUDIT_HOST ?? '127.0.0.1';
const serviceToken = process.env.AUDIT_SERVICE_TOKEN ?? '';
const maxRequestBytes = Number(process.env.AUDIT_MAX_REQUEST_BYTES ?? 1024 * 1024);
if (!Number.isSafeInteger(maxRequestBytes) || maxRequestBytes < 1024 || maxRequestBytes > 10 * 1024 * 1024) {
  throw new Error('AUDIT_MAX_REQUEST_BYTES must be between 1024 and 10485760');
}
if (host !== '127.0.0.1' && !serviceToken) {
  throw new Error('AUDIT_SERVICE_TOKEN is required when AUDIT_HOST is not 127.0.0.1');
}
mkdirSync(dirname(dbPath), { recursive: true });
const store = new AuditStore(dbPath);
const recorder = new AuditRecorder(store);
const mime = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript; charset=utf-8', '.css': 'text/css; charset=utf-8' };
const json = (res, status, value) => { res.writeHead(status, { 'content-type': 'application/json; charset=utf-8', 'cache-control': 'no-store' }); res.end(JSON.stringify(value)); };

// The browser is a local demo, not an internal audit diagnostic client.
// Keep record IDs, hashes and event contents out of its chain-status response.
function publicChainStatus(result) {
  return {
    valid: result.valid,
    checked: result.checked,
    first_break_sequence: result.first_break_sequence ?? null,
    reason: result.reason,
    hash_version: result.hash_version ?? 'local-js-v2',
    hash_versions: result.hash_versions ?? [result.hash_version ?? 'local-js-v2'],
    external_anchor: 'not_checked',
  };
}

const server = createServer(async (req, res) => {
  try {
    const url = new URL(req.url, 'http://127.0.0.1');
    if (url.pathname.startsWith('/api/') && url.pathname !== '/api/health' && serviceToken) {
      const match = /^Bearer\s+(.+)$/i.exec(req.headers.authorization ?? '');
      const supplied = Buffer.from(match?.[1] ?? '', 'utf8');
      const expected = Buffer.from(serviceToken, 'utf8');
      if (supplied.length !== expected.length || !timingSafeEqual(supplied, expected)) {
        return json(res, 401, { error: 'SERVICE_AUTH_REQUIRED' });
      }
    }
    if (req.method === 'GET' && url.pathname === '/api/health') return json(res, 200, { ok: true, mode: 'local-demo-only' });
    if (req.method === 'GET' && url.pathname === '/api/verify-chain') return json(res, 200, publicChainStatus(recorder.verifyChain()));
    if (req.method === 'GET' && url.pathname === '/api/audit/latest') return json(res, 200, { records: recorder.getLatestTask() });
    if (req.method === 'GET' && url.pathname.startsWith('/api/receipt/')) {
      const operationId = decodeURIComponent(url.pathname.slice('/api/receipt/'.length));
      const receipt = createParticipantReceipt(recorder.getAuditByOperation(operationId), url.searchParams.get('view') ?? 'payer');
      return json(res, receipt ? 200 : 404, { receipt, error: receipt ? null : 'AUDIT_NOT_FOUND' });
    }
    if (req.method === 'GET' && url.pathname === '/api/audit') {
      const records = url.searchParams.has('task_id') ? recorder.getAuditsByTask(url.searchParams.get('task_id'))
        : url.searchParams.has('operation_id') ? recorder.getAuditByOperation(url.searchParams.get('operation_id'))
          : url.searchParams.has('event_id') ? [recorder.getAuditByEventId(url.searchParams.get('event_id'))].filter(Boolean) : [];
      return json(res, 200, { records });
    }
    if (req.method === 'GET' && url.pathname.startsWith('/api/audit/')) {
      const record = recorder.getAudit(decodeURIComponent(url.pathname.slice('/api/audit/'.length)));
      return json(res, record ? 200 : 404, { record, error: record ? null : 'AUDIT_NOT_FOUND' });
    }
    if (req.method === 'POST' && url.pathname === '/api/demo-transfer') {
      const { outcome = 'success' } = await readJsonRequestBody(req, maxRequestBytes);
      const demo = recordSimulatedTransfer(recorder, outcome);
      return json(res, 201, { ...demo, chain: publicChainStatus(recorder.verifyChain()) });
    }
    if (req.method === 'POST' && url.pathname === '/api/demo-draft') {
      const input = await readJsonRequestBody(req, maxRequestBytes);
      const prior = recorder.getAuditByEventId(input.event_id);
      const demo = recordSimulatedDraft(recorder, input);
      return json(res, prior ? 200 : 201, {
        accepted: true,
        duplicate: Boolean(prior),
        ...demo,
        records: recorder.getAuditsByTask(demo.task_id),
        chain: publicChainStatus(recorder.verifyChain()),
      });
    }
    if (req.method === 'POST' && url.pathname === '/api/audit/events') {
      const input = await readJsonRequestBody(req, maxRequestBytes);
      const { record, duplicate } = recorder.recordAuditWithStatus(input);
      return json(res, duplicate ? 200 : 201, {
        accepted: true,
        duplicate,
        audit_id: record.audit_id,
        event_id: record.event_id,
        task_id: record.task_id,
        operation_id: record.operation_id,
      });
    }
    if (req.method === 'POST' && url.pathname === '/api/audit/outbox/v1') {
      const input = await readJsonRequestBody(req, maxRequestBytes);
      const event = foundationOutboxToAuditEvent(input);
      const { record, duplicate } = recorder.recordAuditWithStatus(event);
      return json(res, duplicate ? 200 : 201, {
        ack_version: 'audit-receiver-candidate-v1',
        accepted: true,
        duplicate,
        event_id: record.event_id,
        audit_id: record.audit_id,
        task_id: record.task_id,
        operation_id: record.operation_id,
        digest_echo: input.digest,
        recorded_at: record.recorded_at,
      });
    }
    if (req.method === 'GET' && !url.pathname.startsWith('/api/')) {
      const relative = url.pathname === '/' ? 'index.html' : decodeURIComponent(url.pathname.slice(1));
      const path = resolve(publicRoot, relative);
      if (path !== publicRoot && !path.startsWith(`${publicRoot}${sep}`)) return res.writeHead(403).end('Forbidden');
      try { const content = readFileSync(path); res.writeHead(200, { 'content-type': mime[extname(path)] ?? 'application/octet-stream' }); return res.end(content); }
      catch { res.writeHead(404); return res.end('Not found'); }
    }
    return json(res, 404, { error: 'NOT_FOUND' });
  } catch (error) {
    if (error instanceof AuditConflictError) return json(res, 409, { error: 'EVENT_ID_CONFLICT' });
    if (error instanceof AuditInputError || error instanceof TypeError) {
      return json(res, 400, { error: 'INVALID_AUDIT_EVENT', message: error.message });
    }
    if (error instanceof RequestTooLargeError) return json(res, 413, { error: 'REQUEST_TOO_LARGE' });
    console.error('Audit request failed:', error);
    return json(res, 503, { error: 'AUDIT_STORAGE_UNAVAILABLE' });
  }
});

const port = Number(process.env.PORT ?? 4173);
server.listen(port, host, () => console.log(`SafeFlow 审计演示：http://${host}:${port}（演示模式；业务事件不执行转账）`));
function close() { server.close(() => { recorder.close(); process.exit(0); }); }
process.on('SIGINT', close);
process.on('SIGTERM', close);
