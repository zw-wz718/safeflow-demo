import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { preview } from 'vite';

const expected = JSON.parse(
  await readFile(new URL('../../docs/progress/project-progress.json', import.meta.url), 'utf8'),
);
const checks = [];
for (let run = 0; run < 2; run++) {
  const server = await preview({
    logLevel: 'error',
    preview: { host: '127.0.0.1', port: 0, strictPort: true },
  });
  try {
    const address = server.httpServer.address();
    assert(address && typeof address !== 'string');
    const base = `http://127.0.0.1:${address.port}`;
    const page = await fetch(`${base}/progress.html`);
    assert.equal(page.status, 200);
    assert.match(await page.text(), /assets\/progress-/);
    checks.push(`preview-${run + 1}: built progress entry`);
    const progress = await fetch(`${base}/api/progress`);
    assert.equal(progress.status, 200);
    assert.deepEqual(await progress.json(), expected);
    checks.push(`preview-${run + 1}: same persistent ledger`);
    const evidence = await fetch(`${base}/api/progress/evidence/transfer-report`);
    assert.equal(evidence.status, 200);
    assert.match((await evidence.json()).content, /7cea089/);
    checks.push(`preview-${run + 1}: registered evidence`);
    for (const path of [
      '/api/progress?path=secret',
      '/api/progress/evidence/missing',
      '/api/progress/evidence/..%2fsecret',
    ]) {
      assert((await fetch(`${base}${path}`)).status >= 400);
    }
    assert.equal((await fetch(`${base}/api/progress`, { method: 'POST', body: '{}' })).status, 405);
    checks.push(`preview-${run + 1}: path and write guards`);
  } finally {
    server.httpServer.closeAllConnections();
    await new Promise((resolve, reject) =>
      server.httpServer.close((error) => (error ? reject(error) : resolve())),
    );
  }
}
console.log(
  JSON.stringify({ passed: checks.length, failed: 0, checks, previewServersClosed: true }, null, 2),
);
