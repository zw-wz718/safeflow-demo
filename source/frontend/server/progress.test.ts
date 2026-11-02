import { afterEach, describe, expect, it } from 'vitest';
import { createServer, type Server } from 'node:http';
import { randomInt } from 'node:crypto';
import { mkdtemp, mkdir, readFile, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { officialTaskIds, ownerIds, progressSchema, type Progress } from '../src/progress/schema';
import {
  createProgressMiddleware,
  evidenceRegistry,
  progressPlugin,
  readProjectText,
} from './progress';

const roots: string[] = [];
const servers: Server[] = [];
async function closeServer(server: Server) {
  server.closeAllConnections();
  if (!server.listening) return;
  await new Promise<void>((resolve, reject) =>
    server.close((error) => (error ? reject(error) : resolve())),
  );
}
async function listenForFetch(server: Server) {
  // The dynamic/private range lies above all Fetch bad ports (the highest is 10080).
  for (let attempt = 0; attempt < 32; attempt++) {
    const port = randomInt(49152, 65536);
    try {
      await new Promise<void>((resolve, reject) => {
        const cleanup = () => {
          server.off('error', onError);
          server.off('listening', onListening);
        };
        const onError = (error: Error) => {
          cleanup();
          reject(error);
        };
        const onListening = () => {
          cleanup();
          resolve();
        };
        server.once('error', onError);
        server.once('listening', onListening);
        try {
          server.listen(port, '127.0.0.1');
        } catch (error) {
          cleanup();
          reject(error);
        }
      });
      return port;
    } catch (error) {
      await closeServer(server);
      if (!['EADDRINUSE', 'EACCES'].includes((error as NodeJS.ErrnoException).code ?? ''))
        throw error;
    }
  }
  throw new Error('No Fetch-safe loopback port available after 32 binding attempts');
}
// Evidence registration scenarios use an isolated ledger, independent of live registrations.
function evidenceFixture(sources: Progress['sources'] = []): Progress {
  const date = '2026-10-22';
  return progressSchema.parse({
    schemaVersion: 1,
    updatedAt: `${date}T00:00:00Z`,
    current: {
      headline: '隔离证据读取测试',
      reviewedOn: date,
      sourceId: 'fixture-review',
      limits: ['仅验证进度接口，不代表业务验收'],
      tests: { ordinary: 0, postgres: 0, doc: 0, independent: 0 },
    },
    members: ownerIds.map((id) => ({
      id,
      name: id,
      role: '隔离测试',
      stage: '未登记',
      next: '核对',
    })),
    sources: [
      {
        id: 'fixture-plan',
        title: '测试计划',
        kind: 'plan',
        path: 'fixture/plan.md',
        date,
        version: null,
        scope: '隔离测试',
      },
      {
        id: 'fixture-review',
        title: '测试来源',
        kind: 'independent',
        path: 'fixture/review.md',
        date,
        version: null,
        scope: '隔离测试',
      },
      ...sources,
    ],
    tasks: officialTaskIds.map((id) => ({
      id,
      ownerId: 'foundation',
      title: id,
      priority: 'P0',
      requirements: ['隔离测试'],
      planSourceId: 'fixture-plan',
      status: 'unknown',
      implemented: false,
      accepted: false,
      scope: '隔离测试',
      completed: [],
      gaps: [],
      next: '核对',
      version: null,
      sourceIds: [],
      subtasks: [],
    })),
    deliveries: [],
    dependencies: [],
  });
}
async function setup(fixture?: Progress) {
  const root = await mkdtemp(join(tmpdir(), 'safeflow-progress-'));
  roots.push(root);
  await mkdir(join(root, 'docs/progress/sources'), { recursive: true });
  const raw =
    fixture ??
    JSON.parse(
      await readFile(new URL('../../docs/progress/project-progress.json', import.meta.url), 'utf8'),
    );
  const ledger = join(root, 'docs/progress/project-progress.json');
  await writeFile(ledger, JSON.stringify(raw));
  const handler = createProgressMiddleware(root);
  const server = createServer((req, res) => {
    void handler(req, res, () => {
      res.statusCode = 404;
      res.end();
    });
  });
  const port = await listenForFetch(server);
  servers.push(server);
  return { root, raw, ledger, url: `http://127.0.0.1:${port}` };
}
afterEach(async () => {
  for (const server of servers.splice(0)) {
    await closeServer(server);
  }
  for (const root of roots.splice(0)) await rm(root, { recursive: true, force: true });
});

function deliveryHistory(count: number): Progress {
  const data = evidenceFixture();
  data.deliveries = Array.from({ length: count }, (_, index) => ({
    id: `http-capacity-${index}`,
    ownerId: 'foundation',
    receivedAt: null,
    reviewedAt: null,
    recordedAt: '2026-10-24',
    version: 'isolated-fixture',
    location: 'fixture-only',
    state: 'received',
    title: '隔离容量记录',
    scope: 'No project registration',
    declared: 'Fixture only',
    result: 'Fixture only',
    taskIds: ['BASE-01'],
    sourceIds: ['fixture-plan'],
  }));
  return data;
}

describe('read-only HTTP delivery history capacity', () => {
  it.each([301, 500, 501])('handles %i deliveries without changing stored bytes', async (count) => {
    const fixture = deliveryHistory(count);
    const { ledger, url } = await setup(fixture);
    const before = await readFile(ledger);
    expect(before.length).toBeLessThan(1048576);
    const response = await fetch(`${url}/api/progress`);
    expect(response.status).toBe(count <= 500 ? 200 : 503);
    if (count <= 500) expect(await response.json()).toEqual(fixture);
    else
      expect(await response.json()).toEqual({
        error: 'PROGRESS_UNAVAILABLE',
        message: '台账暂时无法读取或未通过校验',
      });
    expect(await readFile(ledger)).toEqual(before);
  });

  it.each(['duplicate', 'accepted', 'changes-requested'] as const)(
    'rejects %s semantics at delivery 500',
    async (invalid) => {
      const fixture = deliveryHistory(500);
      const delivery = fixture.deliveries[499];
      if (invalid === 'duplicate') delivery.id = fixture.deliveries[0].id;
      else {
        delivery.state = invalid;
        delivery.reviewedAt = '2026-10-24';
        // A plan source is neither an independent pass nor a failed review.
      }
      const { ledger, url } = await setup(fixture);
      const before = await readFile(ledger);
      expect((await fetch(`${url}/api/progress`)).status).toBe(503);
      expect(await readFile(ledger)).toEqual(before);
    },
  );

  it('requires registration and an existing exact file for the new review mapping', async () => {
    const id = 'startup-cargo-resolution-review-20261024';
    const path = 'artifacts/startup-cargo-resolution-2026-10-24/REVIEW.md';
    expect(evidenceRegistry[id]).toBe(path);
    const fixture = evidenceFixture();
    const { root, ledger, url } = await setup(fixture);
    expect((await fetch(`${url}/api/progress/evidence/${id}`)).status).toBe(404);
    const source: Progress['sources'][number] = {
      id,
      path,
      title: 'Isolated review fixture',
      kind: 'review',
      date: '2026-10-24',
      version: null,
      scope: 'Fixture only; not a real review',
    };
    fixture.sources.push(source);
    await writeFile(ledger, JSON.stringify(fixture));
    expect((await fetch(`${url}/api/progress/evidence/${id}`)).status).toBe(503);
    await mkdir(join(root, 'artifacts/startup-cargo-resolution-2026-10-24'), {
      recursive: true,
    });
    await writeFile(join(root, path), 'Isolated fixture, not a review conclusion');
    const response = await fetch(`${url}/api/progress/evidence/${id}`);
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({
      sourceId: id,
      content: 'Isolated fixture, not a review conclusion',
    });
    source.path = 'artifacts/startup-cargo-resolution-2026-10-24/DELIVERY.md';
    await writeFile(ledger, JSON.stringify(fixture));
    expect((await fetch(`${url}/api/progress/evidence/${id}`)).status).toBe(404);
  });
});
describe('read-only ledger and allowlisted evidence', () => {
  for (const [id, filename, content] of [
    ['subscription-c163-public-r2-review-20261022', 'REVIEW-R2.md', '# r2 复审不通过'],
    ['subscription-c163-public-candidate-review-20261022', 'REVIEW-CANDIDATE.md', '# 首审不通过'],
  ]) {
    const path = `artifacts/subscription-c163-public-adoption-2026-10-22/${filename}`;
    const source: Progress['sources'][number] = {
      id,
      path,
      title: content,
      kind: 'review',
      date: '2026-10-22',
      version: null,
      scope: '隔离证据读取测试',
    };
    it.each([false, true])(
      `${filename}: exact content and path guard (already registered: %s)`,
      async (registered) => {
        const fixture = evidenceFixture(registered ? [{ ...source }] : []);
        const { root, ledger, url } = await setup(fixture);
        expect(evidenceRegistry[id]).toBe(path);
        expect(fixture.sources.filter((s) => s.id === id)).toHaveLength(registered ? 1 : 0);
        await mkdir(join(root, 'artifacts/subscription-c163-public-adoption-2026-10-22'), {
          recursive: true,
        });
        await writeFile(join(root, path), content);
        const initial = await fetch(`${url}/api/progress/evidence/${id}`);
        expect(initial.status).toBe(registered ? 200 : 404);
        if (registered) expect(await initial.json()).toEqual({ sourceId: id, content });
        const existing = fixture.sources.find((s) => s.id === id);
        if (!existing) fixture.sources.push({ ...source });
        expect(fixture.sources.filter((s) => s.id === id)).toHaveLength(1);
        await writeFile(ledger, JSON.stringify(progressSchema.parse(fixture)));
        const response = await fetch(`${url}/api/progress/evidence/${id}`);
        expect(response.status).toBe(200);
        expect(await response.json()).toEqual({ sourceId: id, content });
        fixture.sources.find((s) => s.id === id)!.path = path.replace(filename, 'private.md');
        await writeFile(ledger, JSON.stringify(progressSchema.parse(fixture)));
        expect((await fetch(`${url}/api/progress/evidence/${id}`)).status).toBe(404);
      },
    );
    it(`${filename}: rejects duplicate source IDs even when their paths match`, async () => {
      const fixture = evidenceFixture([{ ...source }]);
      const { root, ledger, url } = await setup(fixture);
      await mkdir(join(root, 'artifacts/subscription-c163-public-adoption-2026-10-22'), {
        recursive: true,
      });
      await writeFile(join(root, path), content);
      expect((await fetch(`${url}/api/progress/evidence/${id}`)).status).toBe(200);
      fixture.sources.push({ ...source });
      expect(() => progressSchema.parse(fixture)).toThrow('来源ID重复');
      await writeFile(ledger, JSON.stringify(fixture));
      for (const endpoint of ['/api/progress', `/api/progress/evidence/${id}`]) {
        expect((await fetch(`${url}${endpoint}`)).status).toBe(503);
      }
    });
  }
  it('reads real disk changes and rejects invalid JSON/schema instead of returning stale server data', async () => {
    const { raw, ledger, url } = await setup();
    const first = await fetch(`${url}/api/progress`);
    expect(first.status).toBe(200);
    expect(first.headers.get('cache-control')).toBe('no-store');
    raw.current.headline = '新收到的限定范围进度';
    await writeFile(ledger, JSON.stringify(raw));
    expect((await (await fetch(`${url}/api/progress`)).json()).current.headline).toBe(
      raw.current.headline,
    );
    await writeFile(ledger, '{');
    expect((await fetch(`${url}/api/progress`)).status).toBe(503);
    raw.schemaVersion = 2;
    await writeFile(ledger, JSON.stringify(raw));
    expect((await fetch(`${url}/api/progress`)).status).toBe(503);
  });
  it('only permits GET and registered source IDs with exactly matching paths', async () => {
    const { root, raw, ledger, url } = await setup();
    await writeFile(
      join(root, evidenceRegistry['transfer-report']),
      '# 原始报告\n<script>不是 HTML</script>',
    );
    const response = await fetch(`${url}/api/progress/evidence/transfer-report`);
    expect(response.status).toBe(200);
    expect((await response.json()).content).toContain('<script>');
    for (const path of [
      '/api/progress?path=C:/private',
      '/api/progress/evidence/transfer-report?path=secret',
      '/api/progress/evidence/..%2fsecret',
      '/api/progress/evidence/E%3A%5Csecret',
      '/api/progress/evidence/missing',
    ]) {
      expect((await fetch(`${url}${path}`)).status).toBeGreaterThanOrEqual(400);
    }
    expect((await fetch(`${url}/api/progress`, { method: 'POST', body: '{}' })).status).toBe(405);
    raw.sources.find((s: { id: string }) => s.id === 'transfer-report').path =
      'docs/progress/sources/another.md';
    await writeFile(ledger, JSON.stringify(raw));
    expect((await fetch(`${url}/api/progress/evidence/transfer-report`)).status).toBe(404);
  });
  it('rejects absolute paths, traversal, forbidden extensions, missing or oversized evidence', async () => {
    const { root, url } = await setup();
    for (const path of [
      '../private.md',
      'C:/private.md',
      'docs/../private.md',
      'docs\\private.md',
      'docs/key.pem',
    ])
      await expect(readProjectText(root, path)).rejects.toThrow();
    const missing = await fetch(`${url}/api/progress/evidence/transfer-report`);
    expect(missing.status).toBe(503);
    expect(await missing.text()).not.toContain(root);
    await writeFile(join(root, evidenceRegistry['transfer-report']), 'x'.repeat(262145));
    expect((await fetch(`${url}/api/progress/evidence/transfer-report`)).status).toBe(503);
  });
  it('rejects an allowlisted text file whose directory junction escapes the project root', async () => {
    const { root } = await setup();
    const outside = await mkdtemp(join(tmpdir(), 'safeflow-private-'));
    roots.push(outside);
    await writeFile(join(outside, 'private.md'), 'PRIVATE TEST MATERIAL');
    await symlink(outside, join(root, 'escape'), process.platform === 'win32' ? 'junction' : 'dir');
    await expect(readProjectText(root, 'escape/private.md')).rejects.toThrow('PATH_REJECTED');
  });
  it('mounts the same guarded interface for development and preview', () => {
    const plugin = progressPlugin('unused-trusted-root');
    expect(typeof plugin.configureServer).toBe('function');
    expect(typeof plugin.configurePreviewServer).toBe('function');
  });
});

// Exact byte boundaries use valid schema data plus JSON whitespace. The
// multi-byte fixture text ensures character counts cannot stand in for bytes.
function ledgerAtBytes(fixture: Progress, bytes: number) {
  const json = JSON.stringify(progressSchema.parse(fixture));
  const padding = bytes - Buffer.byteLength(json, 'utf8');
  expect(padding).toBeGreaterThanOrEqual(0);
  const content = json + ' '.repeat(padding);
  expect(Buffer.byteLength(content, 'utf8')).toBe(bytes);
  expect(content.length).toBeLessThan(bytes);
  expect(progressSchema.parse(JSON.parse(content))).toEqual(fixture);
  return content;
}

describe('bounded 1 MiB ledger capacity with unchanged evidence protections', () => {
  it.each([524289, 1048575, 1048576])(
    'reads an otherwise valid ledger of exactly %i UTF-8 bytes',
    async (bytes) => {
      const fixture = evidenceFixture();
      const { root, ledger, url } = await setup(fixture);
      const content = ledgerAtBytes(fixture, bytes);
      await writeFile(ledger, content, 'utf8');
      expect((await readFile(ledger)).length).toBe(bytes);
      await expect(
        readProjectText(root, 'docs/progress/project-progress.json', 524288),
      ).rejects.toThrow('FILE_REJECTED');
      expect(await readProjectText(root, 'docs/progress/project-progress.json', 1048576)).toBe(
        content,
      );
      const response = await fetch(`${url}/api/progress`);
      expect(response.status).toBe(200);
      expect(response.headers.get('cache-control')).toBe('no-store');
      expect(response.headers.get('x-content-type-options')).toBe('nosniff');
      expect(await response.json()).toEqual(fixture);
      expect(await readFile(ledger, 'utf8')).toBe(content);
    },
  );

  it.each([1048577, 1048580])(
    'rejects a valid ledger above the limit at %i bytes',
    async (bytes) => {
      const fixture = evidenceFixture();
      const { root, ledger, url } = await setup(fixture);
      const content = ledgerAtBytes(fixture, bytes);
      await writeFile(ledger, content, 'utf8');
      expect((await readFile(ledger)).length).toBe(bytes);
      await expect(
        readProjectText(root, 'docs/progress/project-progress.json', 1048576),
      ).rejects.toThrow('FILE_REJECTED');
      const response = await fetch(`${url}/api/progress`);
      expect(response.status).toBe(503);
      expect(await response.json()).toEqual({
        error: 'PROGRESS_UNAVAILABLE',
        message: '台账暂时无法读取或未通过校验',
      });
      expect(await readFile(ledger, 'utf8')).toBe(content);
    },
  );

  it.each([262144, 262145])(
    'keeps the separate evidence limit at %i UTF-8 bytes',
    async (bytes) => {
      const id = 'billing-clippy-completion-review-20261024';
      const path = 'artifacts/billing-clippy-completion-2026-10-24/REVIEW.md';
      const source: Progress['sources'][number] = {
        id,
        path,
        title: 'Isolated evidence byte-boundary test',
        kind: 'review',
        date: '2026-10-24',
        version: null,
        scope: 'Fixture only; no real review is fabricated',
      };
      const fixture = evidenceFixture([source]);
      const { root, ledger, url } = await setup(fixture);
      // Evidence reads must also accept the newly allowed larger valid ledger.
      await writeFile(ledger, ledgerAtBytes(fixture, 600000));
      await mkdir(join(root, 'artifacts/billing-clippy-completion-2026-10-24'), {
        recursive: true,
      });
      const content = '界'.repeat(Math.floor(bytes / 3)) + 'x'.repeat(bytes % 3);
      expect(content.length).toBeLessThan(262144);
      await writeFile(join(root, path), content, 'utf8');
      expect((await readFile(join(root, path))).length).toBe(bytes);
      const response = await fetch(`${url}/api/progress/evidence/${id}`);
      expect(response.status).toBe(bytes === 262144 ? 200 : 503);
      if (bytes === 262144) {
        expect(await readProjectText(root, path)).toBe(content);
        expect(await response.json()).toEqual({ sourceId: id, content });
      } else {
        await expect(readProjectText(root, path)).rejects.toThrow('FILE_REJECTED');
        expect(await response.json()).toEqual({
          error: 'EVIDENCE_UNAVAILABLE',
          message: '证据暂时无法读取',
        });
      }
      expect(await readFile(join(root, path), 'utf8')).toBe(content);
    },
  );

  it('reserves only the exact review mapping and requires matching ledger registration', async () => {
    const id = 'billing-clippy-completion-review-20261024';
    const path = 'artifacts/billing-clippy-completion-2026-10-24/REVIEW.md';
    expect(evidenceRegistry[id]).toBe(path);
    const fixture = evidenceFixture();
    const { root, ledger, url } = await setup(fixture);
    expect((await fetch(`${url}/api/progress/evidence/${id}`)).status).toBe(404);
    const source: Progress['sources'][number] = {
      id,
      path,
      title: 'Isolated mapping test',
      kind: 'review',
      date: '2026-10-24',
      version: null,
      scope: 'Fixture only',
    };
    fixture.sources.push(source);
    await writeFile(ledger, ledgerAtBytes(fixture, 600000));
    const missing = await fetch(`${url}/api/progress/evidence/${id}`);
    expect(missing.status).toBe(503);
    expect(await missing.json()).toEqual({
      error: 'EVIDENCE_UNAVAILABLE',
      message: '证据暂时无法读取',
    });
    await mkdir(join(root, 'artifacts/billing-clippy-completion-2026-10-24'), {
      recursive: true,
    });
    const content = '# Isolated fixture, not the task review';
    await writeFile(join(root, path), content);
    const present = await fetch(`${url}/api/progress/evidence/${id}`);
    expect(present.status).toBe(200);
    expect(await present.json()).toEqual({ sourceId: id, content });
    source.path = path.replace('REVIEW.md', 'private.md');
    await writeFile(ledger, ledgerAtBytes(fixture, 600000));
    expect((await fetch(`${url}/api/progress/evidence/${id}`)).status).toBe(404);
  });

  it('retains GET-only and path/query guards with a larger valid ledger', async () => {
    const fixture = evidenceFixture();
    const { ledger, url } = await setup(fixture);
    const content = ledgerAtBytes(fixture, 600000);
    await writeFile(ledger, content);
    for (const path of [
      '/api/progress',
      '/api/progress/evidence/billing-clippy-completion-review-20261024',
    ]) {
      expect((await fetch(`${url}${path}`, { method: 'POST', body: '{}' })).status).toBe(405);
    }
    expect((await fetch(`${url}/api/progress?path=C:/private`)).status).toBe(400);
    expect((await fetch(`${url}/api/progress/evidence/..%2fprivate`)).status).toBe(404);
    expect((await fetch(`${url}/api/progress/evidence/unknown-source`)).status).toBe(404);
    expect(await readFile(ledger, 'utf8')).toBe(content);
  });

  it('still validates the schema for ledgers larger than the old byte limit', async () => {
    const fixture = evidenceFixture();
    const { root, ledger, url } = await setup(fixture);
    const invalid = { ...fixture, schemaVersion: 2 };
    const json = JSON.stringify(invalid);
    const content = json + ' '.repeat(600000 - Buffer.byteLength(json));
    await writeFile(ledger, content);
    expect(Buffer.byteLength(content)).toBe(600000);
    expect(await readProjectText(root, 'docs/progress/project-progress.json', 1048576)).toBe(
      content,
    );
    expect(progressSchema.safeParse(JSON.parse(content)).success).toBe(false);
    const response = await fetch(`${url}/api/progress`);
    expect(response.status).toBe(503);
    expect(await response.json()).toEqual({
      error: 'PROGRESS_UNAVAILABLE',
      message: '台账暂时无法读取或未通过校验',
    });
  });
});
