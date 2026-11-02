import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import test from 'node:test';
import { ApiError, callApi } from '../public/api-client.js';

async function withLocalServer(handler, run) {
  const server = createServer(handler);
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  try {
    return await run(`http://127.0.0.1:${server.address().port}`);
  } finally {
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
  }
}

test('real HTTP 200 headers followed by a broken JSON body are rejected', async () => {
  await withLocalServer((request, response) => {
    response.writeHead(200, { 'content-type': 'application/json' });
    if (request.url === '/disconnect') {
      response.flushHeaders();
      response.write('{"receipt":');
      setTimeout(() => response.socket?.destroy(), 10);
      return;
    }
    response.end('{"receipt":');
  }, async (baseUrl) => {
    for (const route of ['/disconnect', '/malformed']) {
      await assert.rejects(
        callApi(`${baseUrl}${route}`),
        (error) => error instanceof ApiError && error.status === 200 && error.code === 'INVALID_JSON_RESPONSE',
        `${route}: invalid 200 response must not be accepted as success or empty receipt`,
      );
    }
  });
});
