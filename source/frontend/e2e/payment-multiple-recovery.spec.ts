import { expect, test, type Page, type Route } from '@playwright/test';
import captured from '../src/runtime/payment-backend.fixture.json' with { type: 'json' };
import legacy from '../src/runtime/payment-processing.fixture.json' with { type: 'json' };
import { agentSchema, type AgentView } from '../src/runtime/agent-client';
import {
  fundsReference,
  fundsResultSchema,
  paymentBase,
  type PaymentFunds,
} from '../src/runtime/payment-schema';
import { legacyRecoveryKey, recoveryKey } from '../src/runtime/payment-recovery';
import { recoveryJournalKey } from '../src/runtime/payment-recovery-store';
import { guardReplayApi, assertReplayIsolated } from './isolated-api';
test.afterEach(({ page }) => assertReplayIsolated(page));

type PaymentView = Extract<AgentView, { kind: 'PAYMENT' }>;
const reply = (r: Route, data: unknown) =>
  r.fulfill({ contentType: 'application/json', body: JSON.stringify(data) });
// Mutate foundation selectors consistently, including nested paths and bindings.
// The captured member fixture files themselves remain untouched.
function projection(tag: 'a' | 'b' | 'c', old = false): PaymentView {
  const seed = old ? legacy : captured.waiting;
  let text = JSON.stringify(seed);
  for (const [from, to] of [
    [seed.payment.task_id, `task_${tag}`],
    [seed.payment.operation.operation_id, `preparation_${tag}`],
    [seed.payment.operation.node_id, `node_${tag}`],
    [seed.payment.snapshot.transfer.operation_id, `operation_${tag}`],
    [seed.payment.snapshot.transfer.draft_id, `draft_${tag}`],
    [seed.payment.snapshot_digest, tag.repeat(64)],
  ])
    text = text.replaceAll(from, to);
  const raw = JSON.parse(text);
  if (!old) {
    const p = raw.payment;
    p.funds_execution_enabled = true;
    p.execution_conditions = [];
    p.execution = {
      enabled: true,
      reason: 'RECHECKED_AT_EXPLICIT_EXECUTION',
      execute_path: `${paymentBase(p)}/execute`,
      result_path: `${paymentBase(p)}/funds-result`,
      automatic_retry: false,
    };
    Object.assign(raw.payment_flow, {
      funds_execution_enabled: true,
      status: 'WAITING_EXPLICIT_PAYMENT',
      next_action: 'EXECUTE_PAYMENT',
      next_path: p.execution.execute_path,
    });
  }
  const parsed = agentSchema.parse(raw);
  if (parsed.kind !== 'PAYMENT') throw Error('Invalid captured payment');
  return parsed;
}
function funds(v: PaymentView, state: PaymentFunds['state']) {
  const p = v.payment;
  return fundsResultSchema.parse({
    contract_version: 1,
    scope: 'PAYMENT_FUNDS_RESULT_ONLY',
    task_id: p.task_id,
    preparation_operation_id: p.operation.operation_id,
    snapshot_digest: p.snapshot_digest,
    transfer: p.snapshot.transfer,
    state,
    submitted: !['UNSUBMITTED', 'REGISTERED_UNSUBMITTED'].includes(state),
    automatic_retry: false,
  });
}
async function select(page: Page, v: PaymentView) {
  await page.getByLabel('办理编号', { exact: true }).fill(v.payment.task_id);
  await page.getByRole('button', { name: '查询原任务', exact: true }).click();
  await expect(page.locator('.a-payment-result')).toContainText(
    v.payment.snapshot.transfer.operation_id,
  );
  await expect(page.getByRole('button', { name: '查询原任务', exact: true })).toBeEnabled();
}
function retained(page: Page, v: PaymentView) {
  return page.getByRole('article', {
    name: `保留付款 ${v.payment.snapshot.transfer.operation_id}`,
    exact: true,
  });
}
async function read(page: Page, v: PaymentView) {
  const selected = await page.getByLabel('办理编号', { exact: true }).inputValue();
  const button =
    selected === v.payment.task_id
      ? page.getByRole('button', { name: '查询付款原单', exact: true })
      : retained(page, v).getByRole('button');
  const response = page.waitForResponse(
    (r) => new URL(r.url()).pathname === `${paymentBase(v.payment)}/funds-result`,
  );
  await button.click();
  const received = await response;
  await received.finished();
  await expect(button).toBeEnabled();
  return received;
}
async function references(page: Page) {
  return page.evaluate((k) => JSON.parse(sessionStorage.getItem(k)!).references, recoveryKey);
}
async function setup(
  page: Page,
  mode: 'PROCESSING' | 'LOST_RESPONSE',
  stateB: PaymentFunds['state'],
  discovery: 'GET' | 'AGENT' | 'LEGACY' = 'GET',
  post = true,
) {
  await guardReplayApi(page);
  const a = projection('a'),
    b = projection('b', discovery === 'LEGACY'),
    c = projection('c');
  const states = { a: 'PROCESSING' as PaymentFunds['state'], b: stateB };
  const failures = { status: 200, mismatch: false };
  const posts: string[] = [];
  await page.clock.install({ time: new Date(a.payment.prepared_at) });
  await page.addInitScript((id) => {
    if (!sessionStorage.getItem('safeflow.agent.last-task.v1'))
      sessionStorage.setItem('safeflow.agent.last-task.v1', id);
  }, a.payment.task_id);
  await page.route('**/api/health', (r) =>
    reply(r, { contract_version: 1, mode: 'PROJECT_SIMULATION', ready: true }),
  );
  await page.route('**/api/sim/session', (r) =>
    reply(r, { mode: 'PROJECT_SIMULATION', authenticated: true }),
  );
  await page.route('**/api/agent/status', (r) =>
    reply(r, {
      contract_version: 1,
      mode: 'PROJECT_SIMULATION',
      configured: false,
      connectivity: 'NOT_VERIFIED',
      tools: ['execute_transfer'],
      draft_enabled: true,
      planning_mode: 'FIXED_PLAN',
      payment_planning_enabled: true,
      funds_execution_enabled: true,
    }),
  );
  await page.route('**/api/agent/tasks/*', (r) => {
    const id = new URL(r.request().url()).pathname.split('/').at(-1);
    if (id === b.payment.task_id)
      return reply(
        r,
        discovery === 'AGENT'
          ? { ...b, payment_flow: { ...b.payment_flow, funds_result: funds(b, states.b) } }
          : b,
      );
    return reply(r, id === a.payment.task_id ? a : c);
  });
  await page.route('**/payment-preparations/*/*/execute', (r) => {
    posts.push(new URL(r.request().url()).pathname);
    return mode === 'LOST_RESPONSE' ? r.abort('failed') : reply(r, funds(a, states.a));
  });
  await page.route('**/payment-preparations/*/*/funds-result', (r) => {
    const id = new URL(r.request().url()).pathname.split('/')[4];
    if (id === b.payment.task_id) {
      if (failures.status !== 200)
        return r.fulfill({ status: failures.status, contentType: 'application/json', body: '{}' });
      const result = funds(b, states.b);
      if (failures.mismatch) result.transfer = { ...result.transfer, payee_id: 'wrong_payee' };
      return reply(r, result);
    }
    return reply(
      r,
      funds(id === a.payment.task_id ? a : c, id === a.payment.task_id ? states.a : 'UNSUBMITTED'),
    );
  });
  await page.goto('/#transfers');
  await page.getByRole('button', { name: '在线模拟付款', exact: true }).click();
  if (post) {
    await page.getByRole('button', { name: '确认付款', exact: true }).click();
    await expect(page.getByRole('button', { name: '查询付款原单', exact: true })).toBeEnabled();
  }
  return { a, b, c, states, failures, posts };
}
async function blocked(page: Page) {
  await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeDisabled();
  await expect(page.getByRole('button', { name: '办理所选事项', exact: true })).toBeDisabled();
}
async function discoverB(page: Page, h: Awaited<ReturnType<typeof setup>>, get = true) {
  await select(page, h.b);
  if (get) await read(page, h.b);
  expect((await references(page)).map((r: { task_id: string }) => r.task_id)).toEqual([
    'task_a',
    'task_b',
  ]);
  await blocked(page);
}
test.describe('isolated all known originals retain independent protection', () => {
  for (const discovery of ['GET', 'AGENT'] as const)
    test(`real Chrome quota: ${discovery} B survives C/reload/A terminal/new-task and later storage recovery`, async ({
      page,
    }, info) => {
      const h = await setup(page, 'PROCESSING', 'PROCESSING', discovery);
      const quota = await fillSessionQuota(page);
      expect(quota.lastError).toBe('QuotaExceededError');
      await select(page, h.b);
      if (discovery === 'GET') await read(page, h.b);
      await expect(page.getByRole('alert')).toContainText('付款恢复记录无法完整读取或保存');
      const old = await references(page);
      expect(old.map((r: { task_id: string }) => r.task_id)).toEqual(['task_a']);
      await select(page, h.c);
      await page.reload();
      await expect(retained(page, h.b).getByRole('button')).toBeEnabled();
      await expect(page.getByRole('alert')).toContainText('付款恢复记录无法完整读取或保存');
      h.states.a = 'SUCCEEDED';
      await read(page, h.a);
      await blocked(page);
      // Include the exact ordinary new-task bypass, plus force the DOM event to
      // verify its handler cannot clear the old on-disk A set while B is pending.
      await page
        .getByRole('button', { name: '新任务', exact: true })
        .evaluate((b) => b.dispatchEvent(new MouseEvent('click', { bubbles: true })));
      await select(page, h.c);
      await page
        .getByRole('button', { name: '确认付款', exact: true })
        .evaluate((b) => b.dispatchEvent(new MouseEvent('click', { bubbles: true })));
      await blocked(page);
      expect(h.posts).toHaveLength(1);
      let authenticated = false;
      await page.route('**/api/sim/session', (r) => {
        if (r.request().method() === 'POST') authenticated = true;
        return reply(r, { mode: 'PROJECT_SIMULATION', authenticated });
      });
      await page.clock.fastForward(15000);
      await expect(page.getByLabel('模拟登录口令')).toBeVisible();
      await page.getByLabel('模拟登录口令').fill('isolated-protocol-session');
      await page.getByRole('button', { name: '登录', exact: true }).click();
      await expect(page.getByRole('button', { name: '查询原任务', exact: true })).toBeEnabled();
      await blocked(page);
      await page.reload();
      await expect(retained(page, h.b).getByRole('button')).toBeEnabled();
      await read(page, h.a);
      await read(page, h.b);
      await blocked(page);
      // Storage can recover without deleting a single recovery key or faking
      // a terminal. Only the independent test's filler is removed.
      await page.evaluate(() => sessionStorage.removeItem('repair3.quota.fill'));
      await page.getByRole('button', { name: '重试保存付款恢复记录', exact: true }).click();
      await expect(page.getByRole('alert')).toHaveCount(0);
      await blocked(page);
      h.states.b = 'FAILED';
      await read(page, h.b);
      await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeEnabled();
      await page.getByRole('button', { name: '新任务', exact: true }).click();
      expect(await page.evaluate((k) => sessionStorage.getItem(k), recoveryKey)).toBeNull();
      expect(h.posts).toHaveLength(1);
      await test.info().attach('quota-observation', {
        body: JSON.stringify({
          quota,
          old,
          posts: h.posts,
          journal: await page.evaluate(
            (k) => JSON.parse(sessionStorage.getItem(k)!),
            recoveryJournalKey,
          ),
        }),
        contentType: 'application/json',
      });
      await page.screenshot({
        path: `${process.env.SAFEFLOW_WEB_EVIDENCE}/quota-${discovery}-${info.project.name}-recovered.png`,
        fullPage: true,
      });
    });
  test('an interrupted original query retains its exact pending selector through reload and rehydrates B', async ({
    page,
  }) => {
    const h = await setup(page, 'PROCESSING', 'PROCESSING');
    await select(page, h.b);
    let release!: () => void, seen!: () => void;
    const gate = new Promise<void>((r) => {
        release = r;
      }),
      started = new Promise<void>((r) => {
        seen = r;
      });
    await page.route(`**${paymentBase(h.b.payment)}/funds-result`, async (r) => {
      seen();
      await gate;
      await reply(r, funds(h.b, 'PROCESSING')).catch(() => {});
    });
    await page.getByRole('button', { name: '查询付款原单', exact: true }).click();
    await started;
    const journal = await page.evaluate(
      (k) => JSON.parse(sessionStorage.getItem(k)!),
      recoveryJournalKey,
    );
    expect(journal.phase).toBe('PENDING');
    expect(journal.reads[0].reference).toEqual(fundsReference(h.b.payment));
    await page.reload();
    release();
    await page.unroute(`**${paymentBase(h.b.payment)}/funds-result`);
    await expect(page.getByRole('button', { name: '查询原任务', exact: true })).toBeEnabled();
    // Refreshing the task alone does not establish the pending funds read.
    await expect(page.getByRole('button', { name: '恢复原单查询', exact: true })).toBeEnabled();
    await page.getByRole('button', { name: '恢复原单查询', exact: true }).click();
    await expect(page.getByRole('button', { name: '恢复原单查询', exact: true })).toHaveCount(0);
    await select(page, h.c);
    h.states.a = 'SUCCEEDED';
    await read(page, h.a);
    await blocked(page);
    h.states.b = 'SUCCEEDED';
    await read(page, h.b);
    await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeEnabled();
    expect(h.posts).toHaveLength(1);
  });
  test('an interrupted task projection replays the original read instead of trusting a stale payment view or locking forever', async ({
    page,
  }) => {
    const h = await setup(page, 'PROCESSING', 'PROCESSING');
    await select(page, h.b);
    let release!: () => void,
      seen!: () => void,
      first = true;
    const gate = new Promise<void>((r) => {
        release = r;
      }),
      started = new Promise<void>((r) => {
        seen = r;
      });
    await page.route(`**/api/agent/tasks/${h.b.payment.task_id}`, async (r) => {
      if (first) {
        first = false;
        seen();
        await gate;
      }
      await reply(r, h.b).catch(() => {});
    });
    await page.getByRole('button', { name: '刷新付款状态', exact: true }).click();
    await started;
    await page.reload();
    release();
    await expect(page.getByRole('button', { name: '查询原任务', exact: true })).toBeEnabled();
    await expect(page.locator('.a-payment-result')).toContainText('原单处理中');
    expect((await references(page)).map((r: { task_id: string }) => r.task_id)).toEqual([
      'task_a',
      'task_b',
    ]);
    await select(page, h.c);
    h.states.a = 'SUCCEEDED';
    await read(page, h.a);
    await blocked(page);
    h.states.b = 'SUCCEEDED';
    await read(page, h.b);
    await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeEnabled();
    expect(h.posts).toHaveLength(1);
  });
  test('real quota during v1 migration keeps A through reload and recovers after capacity is restored', async ({
    page,
  }) => {
    const h = await setup(page, 'PROCESSING', 'PROCESSING');
    await page.evaluate(
      ({ current, journal }) => {
        sessionStorage.removeItem(current);
        sessionStorage.removeItem(journal);
      },
      { current: recoveryKey, journal: recoveryJournalKey },
    );
    const quota = await fillSessionQuota(page);
    await page.reload();
    await expect(
      page.getByRole('alert').filter({ hasText: '付款恢复记录无法完整读取或保存' }),
    ).toBeVisible();
    const button = retained(page, h.a).getByRole('button');
    await expect(button).toBeEnabled();
    h.states.a = 'SUCCEEDED';
    await button.click();
    await expect(retained(page, h.a)).toContainText('已核清');
    await blocked(page);
    await page.reload();
    await expect(retained(page, h.a).getByRole('button')).toBeEnabled();
    expect(
      await page.evaluate((k) => JSON.parse(sessionStorage.getItem(k)!).task_id, legacyRecoveryKey),
    ).toBe(h.a.payment.task_id);
    await page.evaluate(() => sessionStorage.removeItem('repair3.quota.fill'));
    await page.getByRole('button', { name: '重试保存付款恢复记录', exact: true }).click();
    await retained(page, h.a).getByRole('button').click();
    await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeEnabled();
    expect(quota.lastError).toBe('QuotaExceededError');
    expect(h.posts).toHaveLength(1);
  });
  for (const mode of ['PROCESSING', 'LOST_RESPONSE'] as const)
    for (const state of ['PROCESSING', 'RECONCILE_REQUIRED', 'SOURCE_CONFLICT'] as const) {
      test(`${mode}: B ${state} survives resolving A, selecting C, reload and unsuccessful reads`, async ({
        page,
      }, info) => {
        const h = await setup(page, mode, state);
        await discoverB(page, h);
        h.states.a = mode === 'PROCESSING' ? 'SUCCEEDED' : 'FAILED';
        await read(page, h.a);
        await blocked(page);
        await select(page, h.c);
        await expect(page.getByRole('button', { name: '确认付款', exact: true })).toBeDisabled();
        await read(page, h.a);
        await read(page, h.a);
        await page
          .locator('.a-composer')
          .evaluate((f) =>
            f.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })),
          );
        const saved = await references(page);
        await page.reload();
        await expect(retained(page, h.b).getByRole('button')).toBeEnabled();
        expect(await references(page)).toEqual(saved);
        await blocked(page);
        await read(page, h.a);
        for (const status of [404, 503]) {
          h.failures.status = status;
          await read(page, h.b);
          await blocked(page);
        }
        h.failures.status = 200;
        h.failures.mismatch = true;
        await read(page, h.b);
        await blocked(page);
        h.failures.mismatch = false;
        h.states.b = 'UNSUBMITTED';
        await read(page, h.b);
        await blocked(page);
        expect(await references(page)).toEqual(saved);
        h.states.b = 'FAILED';
        await read(page, h.b);
        await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeEnabled();
        expect(h.posts).toEqual([`${paymentBase(h.a.payment)}/execute`]);
        if (process.env.SAFEFLOW_WEB_EVIDENCE)
          await page.screenshot({
            path: `${process.env.SAFEFLOW_WEB_EVIDENCE}/multiple-${info.project.name}-${mode}-${state}.png`,
            fullPage: true,
          });
        await page.getByRole('button', { name: '新任务', exact: true }).click();
        await expect(page.getByRole('button', { name: '办理所选事项', exact: true })).toBeEnabled();
        expect(await page.evaluate((k) => sessionStorage.getItem(k), recoveryKey)).toBeNull();
      });
    }
  test('reverse resolution: B terminal clears only B; A unsubmitted or stale reads still require its terminal', async ({
    page,
  }) => {
    const h = await setup(page, 'LOST_RESPONSE', 'PROCESSING');
    await discoverB(page, h);
    h.states.b = 'SUCCEEDED';
    await read(page, h.b);
    await read(page, h.b);
    await blocked(page);
    h.states.a = 'UNSUBMITTED';
    await read(page, h.a);
    await blocked(page);
    h.states.a = 'FAILED';
    await read(page, h.a);
    await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeEnabled();
    expect(h.posts).toHaveLength(1);
  });
  for (const discovery of ['AGENT', 'LEGACY'] as const)
    test(`${discovery} discovered B retains its own selector without a B POST`, async ({
      page,
    }) => {
      const h = await setup(page, 'PROCESSING', 'RECONCILE_REQUIRED', discovery);
      await discoverB(page, h, false);
      h.states.a = 'SUCCEEDED';
      await read(page, h.a);
      await blocked(page);
      await select(page, h.c);
      await page.reload();
      await expect(retained(page, h.b).getByRole('button')).toBeEnabled();
      await read(page, h.a);
      await blocked(page);
      h.states.b = 'SUCCEEDED';
      await read(page, h.b);
      await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeEnabled();
      expect(h.posts).toHaveLength(1);
    });
  test('session revocation discards an old B terminal and rechecks every preserved original after login', async ({
    page,
  }) => {
    const h = await setup(page, 'PROCESSING', 'PROCESSING');
    await discoverB(page, h);
    h.states.a = 'SUCCEEDED';
    await read(page, h.a);
    let releaseSession!: () => void,
      seenSession!: () => void,
      releaseB!: () => void,
      seenB!: () => void;
    const sessionGate = new Promise<void>((r) => {
        releaseSession = r;
      }),
      sessionSeen = new Promise<void>((r) => {
        seenSession = r;
      });
    const bGate = new Promise<void>((r) => {
        releaseB = r;
      }),
      bSeen = new Promise<void>((r) => {
        seenB = r;
      });
    await page.route('**/api/sim/session', async (r) => {
      if (r.request().method() === 'POST')
        return reply(r, { mode: 'PROJECT_SIMULATION', authenticated: true });
      seenSession();
      await sessionGate;
      return reply(r, { mode: 'PROJECT_SIMULATION', authenticated: false });
    });
    await page.route(`**${paymentBase(h.b.payment)}/funds-result`, async (r) => {
      seenB();
      await bGate;
      await reply(r, funds(h.b, 'SUCCEEDED')).catch(() => {});
    });
    await page.clock.fastForward(15000);
    await sessionSeen;
    await page.getByRole('button', { name: '查询付款原单', exact: true }).click();
    await bSeen;
    releaseSession();
    await expect(page.getByLabel('模拟登录口令')).toBeVisible();
    releaseB();
    await page.getByLabel('模拟登录口令').fill('isolated-protocol-session');
    await page.getByRole('button', { name: '登录', exact: true }).click();
    await expect(page.getByRole('button', { name: '查询原任务', exact: true })).toBeEnabled();
    await blocked(page);
    await page.unroute(`**${paymentBase(h.b.payment)}/funds-result`);
    await select(page, h.c);
    await read(page, h.a);
    await blocked(page);
    h.states.b = 'FAILED';
    await read(page, h.b);
    await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeEnabled();
    expect(h.posts).toHaveLength(1);
  });
  test('v1 single-selector migration retains A and newly discovered B through reload', async ({
    page,
  }) => {
    const a = projection('a');
    await page.addInitScript(
      ({ key, reference }) => {
        if (!sessionStorage.getItem(key)) sessionStorage.setItem(key, JSON.stringify(reference));
      },
      { key: legacyRecoveryKey, reference: fundsReference(a.payment) },
    );
    const h = await setup(page, 'PROCESSING', 'PROCESSING', 'GET', false);
    await expect(page.getByRole('button', { name: '确认付款', exact: true })).toHaveCount(0);
    await discoverB(page, h);
    await page.reload();
    await blocked(page);
    h.states.a = 'SUCCEEDED';
    await read(page, h.a);
    await blocked(page);
    h.states.b = 'FAILED';
    await read(page, h.b);
    await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeEnabled();
    expect(h.posts).toEqual([]);
  });
  test('corrupt recovery stays visible and fails closed across reads and reload', async ({
    page,
  }) => {
    await page.addInitScript((k) => sessionStorage.setItem(k, '{'), recoveryKey);
    const h = await setup(page, 'PROCESSING', 'PROCESSING', 'GET', false);
    await expect(page.getByRole('alert')).toContainText('付款恢复记录无法完整读取或保存');
    await blocked(page);
    h.states.a = 'SUCCEEDED';
    await read(page, h.a);
    await blocked(page);
    expect(await page.evaluate((k) => sessionStorage.getItem(k), recoveryKey)).toBe('{');
    await page.reload();
    await blocked(page);
    expect(h.posts).toEqual([]);
  });
});

async function fillSessionQuota(page: Page) {
  return page.evaluate(() => {
    let lo = 0,
      hi = 12 * 1024 * 1024,
      lastError = '';
    while (lo + 1 < hi) {
      const mid = Math.floor((lo + hi) / 2);
      try {
        sessionStorage.setItem('repair3.quota.fill', 'x'.repeat(mid));
        lo = mid;
      } catch (e) {
        lastError = (e as DOMException).name;
        hi = mid;
      }
    }
    sessionStorage.setItem('repair3.quota.fill', 'x'.repeat(Math.max(0, lo - 64)));
    return { largestAcceptedChars: lo, remainingMarginChars: 64, lastError };
  });
}
