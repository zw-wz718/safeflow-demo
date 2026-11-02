// Real external Chromium + real origin IndexedDB. No IAB operations.
import { chromium } from '@playwright/test';
import { readFile, writeFile } from 'node:fs/promises';
import assert from 'node:assert/strict';
const root = process.env.SAFEFLOW_RECOVERY_BROWSER_ROOT;
assert(root?.startsWith('C:/SafeFlowDevRuns/billing-prepare-recovery-candidate-20261024/'));
const state = JSON.parse(await readFile(`${root}/browser-state.json`, 'utf8'));
const origin = 'http://127.0.0.1:59438';
const browser = await chromium.launch({ channel: 'chrome', headless: true });
const checks = [];
async function test(name, fn) {
  await fn();
  checks.push({ name, passed: true });
  await writeFile(`${root}/browser-results.json`, JSON.stringify({ checks }, null, 2));
}
// These pages belong to an external browser launched by this script, never IAB.
try {
  await test('T20/T21 real strict IDB two-page reserve, refresh, CAS and monotone resolved', async () => {
    const context = await browser.newContext();
    const a = await context.newPage(),
      b = await context.newPage();
    await a.goto(`${origin}/runtime.html`);
    await b.goto(`${origin}/runtime.html`);
    const reserve = (page) =>
      page.evaluate(async (realm) => {
        const { BillingRecoveryStore } = await import('/src/runtime/billing-recovery-store.ts');
        const s = new BillingRecoveryStore();
        try {
          return { row: await s.reserve(realm, crypto.randomUUID(), () => true) };
        } catch (e) {
          return { error: String(e) };
        }
      }, state.realm);
    const both = await Promise.all([reserve(a), reserve(b)]);
    assert.equal(both.filter((v) => v.row).length, 1);
    await a.reload();
    const row = await a.evaluate(async () => {
      const { BillingRecoveryStore } = await import('/src/runtime/billing-recovery-store.ts');
      const s = new BillingRecoveryStore();
      return (await s.read())[0];
    });
    assert.equal(row.nonce, both.find((v) => v.row).row.nonce);
    const cas = await a.evaluate(async (row) => {
      const { BillingRecoveryStore } = await import('/src/runtime/billing-recovery-store.ts');
      const s = new BillingRecoveryStore();
      const terminal = await s.change(
        row,
        { phase: 'RESOLVED', terminal_fence_revision: 1 },
        () => true,
      );
      let refused = false;
      try {
        await s.change(row, { phase: 'BOUND' }, () => true);
      } catch {
        refused = true;
      }
      return { terminal, refused, rows: await s.read() };
    }, row);
    assert(cas.refused);
    assert.equal(cas.rows.length, 0);
    assert.equal((await reserve(b)).row.phase, 'RESERVED');
    await context.close();
  });
  await test('T20 strict durability unsupported, corrupt record, invalidated epoch preserve protection', async () => {
    for (const failure of ['strict', 'corrupt', 'epoch']) {
      const context = await browser.newContext();
      const p = await context.newPage();
      await p.goto(`${origin}/runtime.html`);
      const result = await p.evaluate(
        async ({ realm, failure }) => {
          const { BillingRecoveryStore, recoveryDatabase } =
            await import('/src/runtime/billing-recovery-store.ts');
          const s = new BillingRecoveryStore();
          let permitted = true;
          if (failure === 'strict') {
            const original = IDBDatabase.prototype.transaction;
            IDBDatabase.prototype.transaction = function (names, mode, options) {
              return original.call(
                this,
                names,
                mode,
                mode === 'readwrite' ? { durability: 'relaxed' } : options,
              );
            };
          }
          if (failure === 'corrupt') {
            await s.read();
            await new Promise((resolve, reject) => {
              const o = indexedDB.open(recoveryDatabase);
              o.onsuccess = () => {
                const db = o.result,
                  t = db.transaction('rows', 'readwrite', { durability: 'strict' });
                t.objectStore('rows').put({ local_id: 'corrupt', version: 99 });
                t.oncomplete = () => {
                  db.close();
                  resolve();
                };
                t.onabort = reject;
              };
            });
          }
          if (failure === 'epoch') permitted = false;
          try {
            await s.reserve(realm, crypto.randomUUID(), () => permitted);
            return false;
          } catch {
            return true;
          }
        },
        { realm: state.realm, failure },
      );
      assert(result, failure);
      await context.close();
    }
  });
  await test('T26 legacy migration retains original, late v1 writer jointly blocks new reserve', async () => {
    const context = await browser.newContext();
    const p = await context.newPage();
    await p.goto(`${origin}/runtime.html`);
    const result = await p.evaluate(async (realm) => {
      const { BillingRecoveryStore } = await import('/src/runtime/billing-recovery-store.ts');
      const { feedbackRecoveryPrefix } = await import('/src/runtime/billing-feedback-store.ts');
      const s = new BillingRecoveryStore();
      const id = crypto.randomUUID(),
        raw = JSON.stringify({ version: 1, attempt_id: id, task_id: null, operation_id: null });
      localStorage.setItem(feedbackRecoveryPrefix + id, raw);
      const rows = await s.read();
      let blocked = false;
      try {
        await s.reserve(realm, crypto.randomUUID(), () => true);
      } catch {
        blocked = true;
      }
      const retained = localStorage.getItem(feedbackRecoveryPrefix + id) === raw;
      await s.change(rows[0], { phase: 'RESOLVED' }, () => true);
      const lateId = crypto.randomUUID();
      localStorage.setItem(
        feedbackRecoveryPrefix + lateId,
        JSON.stringify({ version: 1, attempt_id: lateId, task_id: null, operation_id: null }),
      );
      let lateBlocked = false;
      try {
        await s.reserve(realm, crypto.randomUUID(), () => true);
      } catch {
        lateBlocked = true;
      }
      const lateRows = await s.read();
      return { rows, blocked, retained, lateBlocked, lateRows, lateId };
    }, state.realm);
    assert.equal(result.rows[0].protocol, 'LEGACY');
    assert(result.blocked && result.retained);
    assert(result.lateBlocked);
    assert.equal(result.lateRows.length, 1);
    assert.equal(result.lateRows[0].local_id, result.lateId);
    await context.close();
  });
  await test('T20 injected disabled/quota/readback/migration faults on real IDB reject reserve and preserve originals', async () => {
    for (const failure of ['disabled', 'quota', 'readback', 'migration']) {
      const context = await browser.newContext();
      const p = await context.newPage();
      await p.goto(`${origin}/runtime.html`);
      let prepares = 0;
      p.on('request', (r) => {
        if (r.method() === 'POST' && r.url().endsWith('/v2/preparations')) prepares++;
      });
      const result = await p.evaluate(
        async ({ realm, failure }) => {
          const { BillingRecoveryStore, recoveryDatabase } =
            await import('/src/runtime/billing-recovery-store.ts');
          const { feedbackRecoveryPrefix } = await import('/src/runtime/billing-feedback-store.ts');
          let readback = false;
          const add = IDBObjectStore.prototype.add,
            all = IDBObjectStore.prototype.getAll,
            put = IDBObjectStore.prototype.put;
          const s = new BillingRecoveryStore(
            undefined,
            failure === 'disabled'
              ? () => {
                  throw new DOMException('Disabled', 'SecurityError');
                }
              : () => indexedDB,
          );
          const id = crypto.randomUUID();
          const raw = JSON.stringify({
            version: 1,
            attempt_id: id,
            task_id: null,
            operation_id: null,
          });
          if (failure === 'migration') localStorage.setItem(feedbackRecoveryPrefix + id, raw);
          IDBObjectStore.prototype.add = function (value, ...args) {
            if (failure === 'quota')
              throw new DOMException('Injected quota exhaustion', 'QuotaExceededError');
            const request = add.call(this, value, ...args);
            if (failure === 'readback') readback = true;
            return request;
          };
          IDBObjectStore.prototype.put = function (value, ...args) {
            if (failure === 'migration' && this.name === 'meta')
              throw new DOMException('Interrupted migration', 'AbortError');
            return put.call(this, value, ...args);
          };
          IDBObjectStore.prototype.getAll = function (...args) {
            const request = all.apply(this, args);
            if (failure === 'readback' && readback)
              request.addEventListener('success', () =>
                Object.defineProperty(request, 'result', { value: [] }),
              );
            return request;
          };
          let blocked = false;
          try {
            await s.reserve(realm, crypto.randomUUID(), () => true);
          } catch {
            blocked = true;
          }
          IDBObjectStore.prototype.add = add;
          IDBObjectStore.prototype.getAll = all;
          IDBObjectStore.prototype.put = put;
          const retained =
            failure !== 'migration' || localStorage.getItem(feedbackRecoveryPrefix + id) === raw;
          const rows = failure === 'disabled' ? [] : await new BillingRecoveryStore().read();
          return { blocked, retained, rows, recoveryDatabase };
        },
        { realm: state.realm, failure },
      );
      assert(result.blocked && result.retained, failure);
      assert.equal(prepares, 0);
      if (failure === 'readback')
        assert.equal(result.rows.length, 1, 'unverified committed reserve remains protected');
      if (failure === 'migration')
        assert.equal(result.rows[0].protocol, 'LEGACY', 'retry copies retained original');
      await context.close();
    }
  });
  await test('T20 actual controller sends no prepare HTTP after IDB quota failure', async () => {
    const context = await browser.newContext(),
      p = await context.newPage();
    await p.goto(`${origin}/runtime.html`);
    assert.equal(
      (
        await context.request.post(`${origin}/api/sim/session`, {
          headers: { 'X-SafeFlow-Request': '1', Origin: origin },
          data: { login_secret: 'isolated-http-test-secret-123456' },
        })
      ).status(),
      200,
    );
    let writes = 0;
    p.on('request', (r) => {
      if (r.method() === 'POST' && r.url().endsWith('/v2/preparations')) writes++;
    });
    const result = await p.evaluate(async (task) => {
      const { BillingRecoveryController } =
        await import('/src/runtime/billing-recovery-controller.ts');
      const { BillingRecoveryStore } = await import('/src/runtime/billing-recovery-store.ts');
      const c = new BillingRecoveryController(
        () => 1,
        () => {},
        () => {},
      );
      c.activate();
      await c.load(task);
      c.select(0, 0, 'NOT_MINE');
      const add = IDBObjectStore.prototype.add;
      IDBObjectStore.prototype.add = function () {
        throw new DOMException('Injected quota', 'QuotaExceededError');
      };
      try {
        await c.prepare();
      } finally {
        IDBObjectStore.prototype.add = add;
      }
      const result = {
        prepared: c.state.prepared,
        rows: await new BillingRecoveryStore().read(),
        notice: c.state.notice,
      };
      c.deactivate();
      return result;
    }, state.report_task_id);
    assert.equal(writes, 0);
    assert.equal(result.prepared, null);
    assert.equal(result.rows.length, 0);
    assert(result.notice);
    await context.close();
  });
  await test('T21/T23 multiple preexisting unknowns: pure404 and GET B never clear A; each fence resolves one', async () => {
    const context = await browser.newContext();
    const p = await context.newPage();
    await p.goto(`${origin}/runtime.html`);
    assert.equal(
      (
        await context.request.post(`${origin}/api/sim/session`, {
          headers: { 'X-SafeFlow-Request': '1', Origin: origin },
          data: { login_secret: 'isolated-http-test-secret-123456' },
        })
      ).status(),
      200,
    );
    const result = await p.evaluate(async (realm) => {
      const { BillingRecoveryStore, recoveryDatabase } =
        await import('/src/runtime/billing-recovery-store.ts');
      const { BillingRecoveryController } =
        await import('/src/runtime/billing-recovery-controller.ts');
      const s = new BillingRecoveryStore(),
        c = new BillingRecoveryController(
          () => 1,
          () => {},
          () => {},
        );
      c.activate();
      await c.query('missing-read-only');
      if ((await s.read()).length !== 0) throw new Error('pure404 created write protection');
      const a = await s.reserve(realm, crypto.randomUUID(), () => true);
      // Reproduce already-existing records from separate past sessions, without business fixtures.
      const b = { ...a, local_id: crypto.randomUUID(), nonce: crypto.randomUUID() };
      await new Promise((resolve, reject) => {
        const o = indexedDB.open(recoveryDatabase);
        o.onsuccess = () => {
          const db = o.result,
            t = db.transaction('rows', 'readwrite', { durability: 'strict' });
          t.objectStore('rows').add(b);
          t.oncomplete = () => {
            db.close();
            resolve();
          };
          t.onabort = reject;
        };
      });
      await c.recover(a);
      const after404 = await s.read();
      await c.query('another-missing-original');
      const afterOther = await s.read();
      await c.recover(a, true);
      const afterA = await s.read();
      let staleRefused = false;
      try {
        await s.change(a, { phase: 'BOUND' }, () => true);
      } catch {
        staleRefused = true;
      }
      await c.recover(b, true);
      const final = await s.read();
      c.deactivate();
      return { a, b, after404, afterOther, afterA, staleRefused, final };
    }, state.realm);
    assert.equal(result.after404.length, 2);
    assert.equal(result.afterOther.length, 2);
    assert.equal(result.afterA.length, 1);
    assert.equal(result.afterA[0].nonce, result.b.nonce);
    assert(result.staleRefused);
    assert.equal(result.final.length, 0);
    await context.close();
  });
  await test('T22 external same-owner Cookie change with unchanged generation rejects late bind', async () => {
    const context = await browser.newContext();
    const p = await context.newPage();
    await p.goto(`${origin}/runtime.html`);
    const login = () =>
      context.request.post(`${origin}/api/sim/session`, {
        headers: { 'X-SafeFlow-Request': '1', Origin: origin },
        data: { login_secret: 'isolated-http-test-secret-123456' },
      });
    assert.equal((await login()).status(), 200);
    await p.evaluate(async (task) => {
      const { BillingRecoveryController } =
        await import('/src/runtime/billing-recovery-controller.ts');
      window.recoveryController = new BillingRecoveryController(
        () => 1,
        () => {},
        () => {},
      );
      const c = window.recoveryController;
      c.activate();
      await c.load(task);
      c.select(0, 0, 'NOT_MINE');
    }, state.report_task_id);
    let release;
    const gate = new Promise((r) => (release = r));
    let intercepted;
    const started = new Promise((r) => (intercepted = r));
    await p.route('**/api/billing-feedback/v2/preparations', async (route) => {
      const response = await route.fetch();
      intercepted();
      await gate;
      await route.fulfill({ response });
    });
    const pending = p.evaluate(() => window.recoveryController.prepare());
    await started;
    assert.equal((await login()).status(), 200);
    release();
    await pending;
    const stale = await p.evaluate(async () => {
      const { BillingRecoveryStore } = await import('/src/runtime/billing-recovery-store.ts');
      return {
        state: window.recoveryController.state,
        rows: await new BillingRecoveryStore().read(),
      };
    });
    assert.equal(stale.state.prepared, null);
    assert.equal(stale.state.reports.length, 0);
    assert.equal(stale.rows.length, 1);
    assert.equal(stale.rows[0].operation_id, null);
    await p.unroute('**/api/billing-feedback/v2/preparations');
    await p.evaluate(async (row) => {
      await window.recoveryController.recover(row, true);
      window.recoveryController.deactivate();
    }, stale.rows[0]);
    assert.equal(
      await p.evaluate(async () => {
        const { BillingRecoveryStore } = await import('/src/runtime/billing-recovery-store.ts');
        return (await new BillingRecoveryStore().read()).length;
      }),
      0,
    );
    await context.close();
  });
  await test('T22 selection/unload and late transport error cannot bind or revive a prepared response', async () => {
    for (const invalidate of ['select', 'deactivate', 'late-error']) {
      const context = await browser.newContext();
      const p = await context.newPage();
      await p.goto(`${origin}/runtime.html`);
      assert.equal(
        (
          await context.request.post(`${origin}/api/sim/session`, {
            headers: { 'X-SafeFlow-Request': '1', Origin: origin },
            data: { login_secret: 'isolated-http-test-secret-123456' },
          })
        ).status(),
        200,
      );
      await p.evaluate(async (task) => {
        const { BillingRecoveryController } =
          await import('/src/runtime/billing-recovery-controller.ts');
        const c = (window.recoveryController = new BillingRecoveryController(
          () => 1,
          () => {},
          () => {},
        ));
        c.activate();
        await c.load(task);
        c.select(0, 0, 'NOT_MINE');
      }, state.report_task_id);
      let release, arrived;
      const gate = new Promise((r) => (release = r)),
        started = new Promise((r) => (arrived = r));
      await p.route('**/api/billing-feedback/v2/preparations', async (route) => {
        const response = await route.fetch();
        assert.equal(response.status(), 201);
        arrived();
        await gate;
        try {
          if (invalidate === 'late-error')
            await route.fulfill({
              status: 503,
              contentType: 'application/json',
              body: JSON.stringify({ contract_version: 2, code: 'UNAVAILABLE_OR_INTEGRITY' }),
            });
          else await route.fulfill({ response });
        } catch {
          /* the real fetch was explicitly cancelled on epoch invalidation */
        }
      });
      const pending = p.evaluate(() => window.recoveryController.prepare());
      await started;
      await p.evaluate((kind) => {
        if (kind === 'deactivate') window.recoveryController.deactivate();
        else window.recoveryController.select(null);
      }, invalidate);
      release();
      await pending;
      const rows = await p.evaluate(async () => {
        const { BillingRecoveryStore } = await import('/src/runtime/billing-recovery-store.ts');
        if (window.recoveryController.state.prepared !== null)
          throw new Error('stale confirmation revived');
        return await new BillingRecoveryStore().read();
      });
      assert.equal(rows.length, 1);
      assert.equal(rows[0].operation_id, null);
      await p.unroute('**/api/billing-feedback/v2/preparations');
      await p.evaluate(async (row) => {
        const c = window.recoveryController;
        c.activate();
        await c.recover(row, true);
        c.deactivate();
      }, rows[0]);
      assert.equal(
        await p.evaluate(async () => {
          const { BillingRecoveryStore } = await import('/src/runtime/billing-recovery-store.ts');
          return (await new BillingRecoveryStore().read()).length;
        }),
        0,
      );
      await context.close();
    }
  });
  await test('T08/T17/T30 actual workspace prepare refresh close then explicit three actions + saved GET', async () => {
    const context = await browser.newContext();
    const p = await context.newPage();
    await p.goto(`${origin}/#bills`);
    // Real session endpoint, fixture login secret already used by the project tests.
    const login = await p.evaluate(async () => {
      const r = await fetch('/api/sim/session', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', 'X-SafeFlow-Request': '1' },
        body: JSON.stringify({ login_secret: 'isolated-http-test-secret-123456' }),
      });
      return r.status;
    });
    assert.equal(login, 200);
    await p.reload();
    await p.getByRole('button', { name: '后台报告与反馈', exact: true }).click();
    const box = p.getByLabel('后台账单报告与反馈');
    await box.getByLabel('原报告任务号', { exact: true }).fill(state.report_task_id);
    await box.getByRole('button', { name: '读取原报告', exact: true }).click();
    await box.locator('input[name="billing-report"]').first().check();
    await box.getByLabel('原报告交易引用', { exact: true }).selectOption('0');
    await box.getByLabel('本人判断', { exact: true }).selectOption('NOT_MINE');
    await box.getByRole('button', { name: '1. 准备反馈', exact: true }).click();
    await box.getByLabel('核对原反馈参数', { exact: true }).waitFor();
    await box.getByRole('button', { name: '2. 本人确认原参数', exact: true }).click();
    await box
      .getByRole('button', { name: '3. 明确保存反馈', exact: true })
      .waitFor({ state: 'visible' });
    await p.reload();
    await p.getByRole('button', { name: '后台报告与反馈', exact: true }).click();
    await box.getByRole('button', { name: '只读核对此条', exact: true }).click();
    await p.waitForTimeout(400);
    assert.equal(
      await box.getByLabel('核对原反馈参数', { exact: true }).count(),
      0,
      'refresh cannot revive confirmation',
    );
    await box.getByRole('button', { name: '明确放弃此准备', exact: true }).click();
    await box.getByLabel('未核清反馈', { exact: true }).waitFor({ state: 'hidden' });
    await box.getByLabel('原报告任务号', { exact: true }).fill(state.report_task_id);
    await box.getByRole('button', { name: '读取原报告', exact: true }).click();
    await box.locator('input[name="billing-report"]').first().check();
    await box.getByLabel('原报告交易引用', { exact: true }).selectOption('0');
    await box.getByLabel('本人判断', { exact: true }).selectOption('NOT_MINE');
    await box.getByRole('button', { name: '1. 准备反馈', exact: true }).click();
    await box.getByLabel('核对原反馈参数', { exact: true }).waitFor();
    await box.getByRole('button', { name: '2. 本人确认原参数', exact: true }).click();
    await box.getByRole('button', { name: '3. 明确保存反馈', exact: true }).click();
    await box.getByLabel('原反馈保存结果', { exact: true }).waitFor();
    assert.equal(await box.getByLabel('未核清反馈', { exact: true }).count(), 0);
    await p.screenshot({ path: `${root}/actual-workspace-saved.png`, fullPage: true });
    const persisted = await p.evaluate(async () => {
      const { recoveryDatabase } = await import('/src/runtime/billing-recovery-store.ts');
      return await new Promise((resolve) => {
        const o = indexedDB.open(recoveryDatabase);
        o.onsuccess = () => {
          const db = o.result,
            t = db.transaction('rows', 'readonly'),
            r = t.objectStore('rows').getAll();
          r.onsuccess = () => resolve(r.result);
          t.oncomplete = () => db.close();
        };
      });
    });
    assert.equal(persisted.length, 2);
    assert(persisted.every((r) => r.phase === 'RESOLVED'));
    assert(
      !/confirmation_ref|original_digest|session|permit|NOT_MINE|report_id/.test(
        JSON.stringify(persisted),
      ),
    );
    const unrelated = await p.evaluate(
      async ({ realm, saved }) => {
        const { BillingRecoveryStore } = await import('/src/runtime/billing-recovery-store.ts');
        const { BillingRecoveryController } =
          await import('/src/runtime/billing-recovery-controller.ts');
        const s = new BillingRecoveryStore(),
          a = await s.reserve(realm, crypto.randomUUID(), () => true);
        const c = new BillingRecoveryController(
          () => 1,
          () => {},
          () => {},
        );
        c.activate();
        await c.query(saved.operation_id, saved.task_id);
        const remaining = await s.read(),
          shown = c.state.result?.operation_id;
        await c.recover(a, true);
        c.deactivate();
        return { a, remaining, shown, final: await s.read() };
      },
      { realm: state.realm, saved: persisted.find((r) => r.terminal_fence_revision === null) },
    );
    assert.equal(unrelated.remaining.length, 1);
    assert.equal(
      unrelated.remaining[0].nonce,
      unrelated.a.nonce,
      'GET saved B must not resolve unknown A',
    );
    assert(unrelated.shown);
    assert.equal(unrelated.final.length, 0);
    await context.close();
  });
} finally {
  await writeFile(`${root}/browser-results.json`, JSON.stringify({ checks }, null, 2));
  await browser.close();
}
