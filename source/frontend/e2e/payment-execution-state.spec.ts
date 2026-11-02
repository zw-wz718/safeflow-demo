import { expect, test, type Page, type Route } from '@playwright/test';
import captured from '../src/runtime/payment-backend.fixture.json' with { type: 'json' };
import { paymentBase } from '../src/runtime/payment-schema';
import { guardReplayApi, assertReplayIsolated } from './isolated-api';
test.afterEach(({ page }) => assertReplayIsolated(page));

// Foundation projection/transport regressions over a previously captured PG
// preparation. These are not member business samples or shared-source evidence.
function enabled() {
  const v = structuredClone(captured.waiting);
  const p = v.payment;
  const base = paymentBase(p);
  return {
    ...v,
    payment: {
      ...p,
      funds_execution_enabled: true,
      execution_conditions: [],
      execution: {
        enabled: true,
        reason: 'RECHECKED_AT_EXPLICIT_EXECUTION',
        execute_path: `${base}/execute`,
        result_path: `${base}/funds-result`,
        automatic_retry: false,
      },
    },
    payment_flow: {
      ...v.payment_flow,
      funds_execution_enabled: true,
      status: 'WAITING_EXPLICIT_PAYMENT',
      next_action: 'EXECUTE_PAYMENT',
      next_path: `${base}/execute`,
    },
  };
}
function result(state: string) {
  const p = enabled().payment;
  return {
    contract_version: 1,
    scope: 'PAYMENT_FUNDS_RESULT_ONLY',
    task_id: p.task_id,
    preparation_operation_id: p.operation.operation_id,
    snapshot_digest: p.snapshot_digest,
    transfer: p.snapshot.transfer,
    state,
    submitted: state !== 'UNSUBMITTED',
    automatic_retry: false,
  };
}
const reply = (r: Route, data: unknown) =>
  r.fulfill({ contentType: 'application/json', body: JSON.stringify(data) });
async function open(page: Page) {
  await guardReplayApi(page);
  const v = enabled();
  await page.clock.install({ time: new Date(v.payment.prepared_at) });
  await page.addInitScript(
    (id) => sessionStorage.setItem('safeflow.agent.last-task.v1', id),
    v.payment.task_id,
  );
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
  // Intentionally stale Agent view: it cannot unlock an attempted payment.
  await page.route('**/api/agent/tasks/*', (r) => reply(r, v));
  await page.goto('/runtime.html');
  await expect(page.getByRole('button', { name: '确认付款', exact: true })).toBeEnabled();
}
test.describe('isolated enabled payment projection and session regressions', () => {
  for (const state of ['PROCESSING', 'FAILED'])
    test(`${state}: original-only recovery survives stale Agent view and reload`, async ({
      page,
    }) => {
      let posts = 0;
      await open(page);
      await page.route('**/payment-preparations/*/*/execute', (r) => {
        posts++;
        return reply(r, result(state));
      });
      await page.route('**/payment-preparations/*/*/funds-result', (r) => reply(r, result(state)));
      await page.getByRole('button', { name: '确认付款', exact: true }).click();
      const label = state === 'PROCESSING' ? '原单处理中' : '原单失败（已核验）';
      await expect(page.locator('.a-original')).toContainText(label);
      await expect(page.getByRole('button', { name: '确认付款', exact: true })).toHaveCount(0);
      const newTask = page.getByRole('button', { name: '新任务', exact: true });
      if (state === 'PROCESSING') await expect(newTask).toBeDisabled();
      else await expect(newTask).toBeEnabled();
      await page.reload();
      await expect(page.getByRole('button', { name: '查询付款原单', exact: true })).toBeEnabled();
      await expect(page.getByRole('button', { name: '确认付款', exact: true })).toHaveCount(0);
      await page.getByRole('button', { name: '查询付款原单', exact: true }).click();
      await expect(page.locator('.a-original')).toContainText(label);
      expect(posts).toBe(1);
    });
  test('lost POST response followed by an unsubmitted GET never unlocks retry or a new task', async ({
    page,
  }) => {
    await open(page);
    let posts = 0;
    await page.route('**/payment-preparations/*/*/execute', (r) => {
      posts++;
      return r.abort('failed');
    });
    await page.route('**/payment-preparations/*/*/funds-result', (r) =>
      reply(r, result('UNSUBMITTED')),
    );
    await page.getByRole('button', { name: '确认付款', exact: true }).click();
    await expect(page.getByRole('button', { name: '查询付款原单', exact: true })).toBeEnabled();
    await page.getByRole('button', { name: '查询付款原单', exact: true }).click();
    await expect(page.locator('.a-original')).toContainText('当前快照未发现提交记录');
    await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeDisabled();
    await page.reload();
    await expect(page.getByRole('button', { name: '查询付款原单', exact: true })).toBeEnabled();
    await expect(page.getByRole('button', { name: '确认付款', exact: true })).toHaveCount(0);
    expect(posts).toBe(1);
  });
  test('explicit payment expires locally without a POST', async ({ page }) => {
    await open(page);
    const posts: string[] = [];
    page.on('request', (r) => {
      if (r.method() === 'POST') posts.push(r.url());
    });
    await page.clock.fastForward(301000);
    await expect(page.getByRole('button', { name: '确认付款', exact: true })).toBeDisabled();
    await expect(page.locator('.a-payment-result')).toContainText('确认已到期');
    expect(posts).toEqual([]);
  });
  test('an in-flight old execution response cannot restore a revoked session or show success', async ({
    page,
  }) => {
    await open(page);
    let releaseSession!: () => void,
      releasePayment!: () => void,
      seenSession!: () => void,
      seenPayment!: () => void;
    const sessionGate = new Promise<void>((r) => {
        releaseSession = r;
      }),
      paymentGate = new Promise<void>((r) => {
        releasePayment = r;
      });
    const sessionStarted = new Promise<void>((r) => {
        seenSession = r;
      }),
      paymentStarted = new Promise<void>((r) => {
        seenPayment = r;
      });
    await page.route('**/api/sim/session', async (r) => {
      seenSession();
      await sessionGate;
      await reply(r, { mode: 'PROJECT_SIMULATION', authenticated: false });
    });
    await page.route('**/payment-preparations/*/*/execute', async (r) => {
      seenPayment();
      await paymentGate;
      await reply(r, result('SUCCEEDED')).catch(() => {});
    });
    await page.getByRole('button', { name: '刷新连接', exact: true }).click();
    await sessionStarted;
    await page.getByRole('button', { name: '确认付款', exact: true }).click();
    await paymentStarted;
    releaseSession();
    await expect(page.getByLabel('模拟登录口令')).toBeVisible();
    releasePayment();
    await expect(page.locator('.a-original')).toHaveCount(0);
    await expect(page.getByRole('button', { name: '确认付款', exact: true })).toHaveCount(0);
    expect(
      await page.evaluate(() => !!sessionStorage.getItem('safeflow.agent.payment-attempt.v1')),
    ).toBe(true);
  });
});
