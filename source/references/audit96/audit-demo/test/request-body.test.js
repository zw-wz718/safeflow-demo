import assert from 'node:assert/strict';
import { Readable } from 'node:stream';
import test from 'node:test';
import { AuditInputError } from '../src/audit-recorder.js';
import { readJsonRequestBody, RequestTooLargeError } from '../src/request-body.js';

test('valid JSON keeps multibyte text when a UTF-8 character crosses chunk boundaries', async () => {
  const expected = { details: { public_reason: '卡片丢失／安全顾虑' } };
  const encoded = Buffer.from(JSON.stringify(expected), 'utf8');
  const character = Buffer.from('卡', 'utf8');
  const start = encoded.indexOf(character);
  assert.notEqual(start, -1);

  const chunks = [encoded.subarray(0, start + 1), encoded.subarray(start + 1)];
  assert.deepEqual(await readJsonRequestBody(Readable.from(chunks), encoded.byteLength), expected);
});

test('request size limit is counted from original bytes', async () => {
  const encoded = Buffer.from(JSON.stringify({ reason: '冻结' }), 'utf8');
  await assert.rejects(
    readJsonRequestBody(Readable.from([encoded]), encoded.byteLength - 1),
    RequestTooLargeError,
  );
});

test('malformed UTF-8 is rejected instead of being silently replaced', async () => {
  const invalidUtf8 = Buffer.from([0x7b, 0x22, 0x78, 0x22, 0x3a, 0x22, 0xc3, 0x28, 0x22, 0x7d]);
  await assert.rejects(
    readJsonRequestBody(Readable.from([invalidUtf8]), 1024),
    (error) => error instanceof AuditInputError && /valid UTF-8/.test(error.message),
  );
});
