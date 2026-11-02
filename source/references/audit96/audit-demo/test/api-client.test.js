import assert from 'node:assert/strict';
import test from 'node:test';
import { ApiError, callApi, loadOptionalReceipt } from '../public/api-client.js';

const response = (status, body) => ({
  ok: status >= 200 && status < 300,
  status,
  async text() { return typeof body === 'string' ? body : JSON.stringify(body); },
});

test('missing demo receipt renders an honest empty state without interrupting the page flow', async () => {
  let rendered = 'not-called';
  const result = await loadOptionalReceipt(
    async () => callApi('/api/receipt/op-base?view=payer', undefined, async () => response(404, { error: 'AUDIT_NOT_FOUND', receipt: null })),
    (receipt) => { rendered = receipt; },
  );
  assert.deepEqual(result, { available: false });
  assert.equal(rendered, null);
});

test('an available demo receipt is rendered unchanged', async () => {
  const expected = { receipt_ref: 'DEMO-1234', status: 'SUCCEEDED' };
  let rendered = null;
  const result = await loadOptionalReceipt(async () => expected, (receipt) => { rendered = receipt; });
  assert.deepEqual(result, { available: true });
  assert.equal(rendered, expected);
});

test('unexpected HTTP and network errors still propagate for visible feedback', async () => {
  await assert.rejects(
    loadOptionalReceipt(
      async () => callApi('/api/receipt/op?view=payer', undefined, async () => response(404, { error: 'NOT_FOUND' })),
      () => assert.fail('unexpected 404 must not be rendered as an empty receipt'),
    ),
    (error) => error instanceof ApiError && error.status === 404 && error.code === 'NOT_FOUND',
  );
  await assert.rejects(
    loadOptionalReceipt(async () => { throw new TypeError('Failed to fetch'); }, () => assert.fail('network error must remain visible')),
    /Failed to fetch/,
  );
  await assert.rejects(
    loadOptionalReceipt(
      async () => callApi('/api/receipt/op?view=payer', undefined, async () => response(503, { error: 'AUDIT_STORAGE_UNAVAILABLE' })),
      () => assert.fail('service errors must remain visible'),
    ),
    (error) => error instanceof ApiError && error.status === 503 && error.code === 'AUDIT_STORAGE_UNAVAILABLE',
  );
});

test('HTTP 200 with invalid or incomplete JSON is surfaced rather than treated as an empty success', async () => {
  await assert.rejects(
    callApi('/api/receipt/op?view=payer', undefined, async () => response(200, '{"receipt":')),
    (error) => error instanceof ApiError && error.status === 200 && error.code === 'INVALID_JSON_RESPONSE',
  );
  await assert.rejects(
    callApi('/api/receipt/op?view=payer', undefined, async () => response(200, '[]')),
    (error) => error instanceof ApiError && error.status === 200 && error.code === 'INVALID_JSON_RESPONSE',
  );
});

test('HTTP response stream truncation remains a visible API error', async () => {
  await assert.rejects(
    callApi('/api/receipt/op?view=payer', undefined, async () => ({
      ok: true,
      status: 200,
      async text() { throw new TypeError('terminated response body'); },
    })),
    (error) => error instanceof ApiError && error.status === 200 && error.code === 'INVALID_JSON_RESPONSE',
  );
});
