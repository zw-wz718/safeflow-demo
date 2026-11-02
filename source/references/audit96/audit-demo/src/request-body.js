import { TextDecoder } from 'node:util';
import { AuditInputError } from './audit-recorder.js';

export class RequestTooLargeError extends Error {
  constructor(message = 'request body too large') {
    super(message);
    this.name = 'RequestTooLargeError';
  }
}

// Decode only after all raw bytes have been joined. Decoding each HTTP chunk
// separately can replace a UTF-8 character split across chunks with U+FFFD.
export async function readJsonRequestBody(request, maxBytes) {
  const chunks = [];
  let byteLength = 0;

  for await (const chunk of request) {
    const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    byteLength += bytes.byteLength;
    if (byteLength > maxBytes) throw new RequestTooLargeError();
    chunks.push(bytes);
  }

  if (byteLength === 0) return {};

  let body;
  try {
    body = new TextDecoder('utf-8', { fatal: true }).decode(Buffer.concat(chunks, byteLength));
  } catch {
    throw new AuditInputError('request body must be valid UTF-8');
  }

  try { return JSON.parse(body); }
  catch (error) {
    if (error instanceof SyntaxError) throw new AuditInputError('request body must be valid JSON');
    throw error;
  }
}
