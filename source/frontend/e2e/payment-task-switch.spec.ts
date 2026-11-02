import { expect, test, type Page, type Route } from '@playwright/test';
import captured from '../src/runtime/payment-backend.fixture.json' with { type: 'json' };
import processing from '../src/runtime/payment-processing.fixture.json' with { type: 'json' };
import { fundsResultSchema, paymentBase, type PaymentFunds } from '../src/runtime/payment-schema';
import { guardReplayApi, assertReplayIsolated } from './isolated-api';
test.afterEach(({ page }) => assertReplayIsolated(page));

// Replay only previously captured foundation protocols. These cases do not
// represent member business samples, actual settlement or shared-source adoption.
const markerKey = 'safeflow.agent.payment-attempt.v1';
const reply = (route: Route, data: unknown) =>
  route.fulfill({ contentType: 'application/json', body: JSON.stringify(data) });
function enabled() {
  const v = structuredClone(captured.waiting);
  const p = v.payment;
  p.expires_at = new Date(Date.now() + 300000).toISOString();
  return {
    ...v,
    payment: {
      ...p,
      funds_execution_enabled: true,
      execution_conditions: [],
      execution: {
        enabled: true,
        reason: 'RECHECKED_AT_EXPLICIT_EXECUTION',
        execute_path: `${paymentBase(p)}/execute`,
        result_path: `${paymentBase(p)}/funds-result`,
        automatic_retry: false,
      },
    },
    payment_flow: {
      ...v.payment_flow,
      funds_execution_enabled: true,
      status: 'WAITING_EXPLICIT_PAYMENT',
      next_action: 'EXECUTE_PAYMENT',
      next_path: `${paymentBase(p)}/execute`,
    },
  };
}
type Projection = ReturnType<typeof enabled>;
function result(
  v: { payment: Projection['payment'] | typeof processing.payment },
  state: PaymentFunds['state'],
): PaymentFunds {
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
async function open(page: Page, mode: 'PROCESSING' | 'LOST_RESPONSE' | 'LEGACY') {
  await guardReplayApi(page);
  const a = mode === 'LEGACY' ? structuredClone(processing) : enabled();
  const taskA = a.payment.task_id;
  const b: Projection = JSON.parse(
    JSON.stringify(enabled()).replaceAll(captured.waiting.payment.task_id, 'repair_other_task'),
  );
  let aResult = result(a, 'PROCESSING'),
    bResult = result(b, 'UNSUBMITTED');
  let invalidA: unknown = null,
    aStatus = 200;
  const posts: string[] = [];
  await page.clock.install({ time: new Date(a.payment.prepared_at) });
  await page.addInitScript((id) => {
    if (!sessionStorage.getItem('safeflow.agent.last-task.v1'))
      sessionStorage.setItem('safeflow.agent.last-task.v1', id);
  }, taskA);
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
  await page.route('**/api/agent/tasks/*', (r) =>
    reply(r, r.request().url().endsWith('/repair_other_task') ? b : a),
  );
  await page.route('**/payment-preparations/*/*/execute', (r) => {
    posts.push(r.request().url().includes('/repair_other_task/') ? 'B' : 'A');
    return mode === 'LOST_RESPONSE' ? r.abort('failed') : reply(r, aResult);
  });
  await page.route('**/payment-preparations/*/*/funds-result', (r) => {
    if (r.request().url().includes('/repair_other_task/')) return reply(r, bResult);
    return aStatus === 200
      ? reply(r, invalidA ?? aResult)
      : r.fulfill({ status: aStatus, contentType: 'application/json', body: '{}' });
  });
  await page.goto('/#transfers');
  await page.getByRole('button', { name: '在线模拟付款', exact: true }).click();
  if (mode !== 'LEGACY') {
    await page.getByRole('button', { name: '确认付款', exact: true }).click();
  }
  await expect(page.getByRole('button', { name: '查询付款原单', exact: true })).toBeEnabled();
  await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeDisabled();
  const marker = await page.evaluate((key) => sessionStorage.getItem(key), markerKey);
  expect(JSON.parse(marker!).task_id).toBe(taskA);
  return {
    a,
    b,
    posts,
    marker,
    setA(state: PaymentFunds['state']) {
      aResult = result(a, state);
      invalidA = null;
      aStatus = 200;
    },
    setB(state: PaymentFunds['state']) {
      bResult = result(b, state);
    },
    invalidateA(data: unknown, status = 200) {
      invalidA = data;
      aStatus = status;
    },
  };
}
async function switchToB(page: Page) {
  await page.getByLabel('办理编号', { exact: true }).fill('repair_other_task');
  await page.getByRole('button', { name: '查询原任务', exact: true }).click();
  await expect(page.getByRole('region', { name: '付款恢复' })).toBeVisible();
  await expect(page.getByRole('button', { name: '确认付款', exact: true })).toBeDisabled();
}
async function stillProtected(page: Page, marker: string | null) {
  await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeDisabled();
  await expect(page.getByRole('button', { name: '查询保留的付款原单', exact: true })).toBeEnabled();
  expect(await page.evaluate((key) => sessionStorage.getItem(key), markerKey)).toBe(marker);
}
test.describe('isolated unresolved payment across task selection', () => {
  for (const mode of ['PROCESSING', 'LOST_RESPONSE'] as const) {
    test(`${mode}: other task states, reads and reload cannot replace A; only matching terminal releases`, async ({
      page,
    }, info) => {
      const h = await open(page, mode);
      await switchToB(page);
      // Force DOM clicks and submit while the unresolved reference is retained.
      await page.getByRole('button', { name: '确认付款', exact: true }).evaluate((b) => {
        b.removeAttribute('disabled');
        b.dispatchEvent(new MouseEvent('click', { bubbles: true }));
        b.dispatchEvent(new MouseEvent('click', { bubbles: true }));
      });
      await page
        .locator('.a-composer')
        .evaluate((form) =>
          form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })),
        );
      await page.getByRole('button', { name: '刷新付款状态', exact: true }).click();
      for (const state of ['UNSUBMITTED', 'SUCCEEDED', 'FAILED'] as const) {
        h.setB(state);
        await page.getByRole('button', { name: '查询付款原单', exact: true }).click();
        await stillProtected(page, h.marker);
        await page.getByRole('button', { name: '刷新付款状态', exact: true }).click();
        await stillProtected(page, h.marker);
      }
      await page.reload();
      await page.getByRole('button', { name: '在线模拟付款', exact: true }).click();
      await stillProtected(page, h.marker);
      expect(await page.getByLabel('办理编号', { exact: true }).inputValue()).toBe(
        'repair_other_task',
      );
      h.setB('SUCCEEDED');
      await page.getByRole('button', { name: '查询付款原单', exact: true }).click();
      const recovery = page.getByRole('region', { name: '付款恢复' });
      for (const status of [404, 503]) {
        h.invalidateA({}, status);
        await recovery.getByRole('button').click();
        await stillProtected(page, h.marker);
      }
      for (const field of [
        'task_id',
        'preparation_operation_id',
        'snapshot_digest',
        'payee_id',
      ] as const) {
        const bad = result(h.a, 'SUCCEEDED');
        if (field === 'payee_id') bad.transfer = { ...bad.transfer, payee_id: 'other_payee' };
        else bad[field] = field === 'snapshot_digest' ? 'f'.repeat(64) : 'other_original';
        h.invalidateA(bad);
        await recovery.getByRole('button').click();
        await stillProtected(page, h.marker);
      }
      h.setA('UNSUBMITTED');
      await recovery.getByRole('button').click();
      await expect(recovery).toContainText('当前快照未发现提交记录');
      await stillProtected(page, h.marker);
      const terminal = mode === 'PROCESSING' ? 'SUCCEEDED' : 'FAILED';
      h.setA(terminal);
      await recovery.getByRole('button').click();
      await expect(recovery).toContainText(
        terminal === 'SUCCEEDED' ? '原单成功（已核验）' : '原单失败（已核验）',
      );
      await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeEnabled();
      // A's query must leave the displayed B terminal intact and non-actionable.
      await expect(page.locator('.a-payment-result .a-original')).toContainText(
        '原单成功（已核验）',
      );
      await expect(page.getByRole('button', { name: '确认付款', exact: true })).toHaveCount(0);
      expect(h.posts).toEqual(['A']);
      expect(await page.evaluate((key) => sessionStorage.getItem(key), markerKey)).toBe(h.marker);
      if (process.env.SAFEFLOW_WEB_EVIDENCE)
        await page.screenshot({
          path: `${process.env.SAFEFLOW_WEB_EVIDENCE}/switch-${info.project.name}-${mode}.png`,
          fullPage: true,
        });
      await page.getByRole('button', { name: '新任务', exact: true }).click();
      await expect(page.getByRole('button', { name: '办理所选事项', exact: true })).toBeEnabled();
      expect(await page.evaluate((key) => sessionStorage.getItem(key), markerKey)).toBeNull();
    });
  }
  test('legacy PROCESSING discovery without a local POST also retains A while viewing B', async ({
    page,
  }) => {
    const h = await open(page, 'LEGACY');
    await switchToB(page);
    await page.getByRole('button', { name: '查询付款原单', exact: true }).click();
    await page.getByRole('button', { name: '刷新付款状态', exact: true }).click();
    await stillProtected(page, h.marker);
    h.setA('FAILED');
    await page.getByRole('button', { name: '查询保留的付款原单', exact: true }).click();
    await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeEnabled();
    expect(h.posts).toEqual([]);
  });
  test('session revocation and an old terminal response cannot release A after login', async ({
    page,
  }) => {
    const h = await open(page, 'LOST_RESPONSE');
    await switchToB(page);
    let releaseSession!: () => void,
      seenSession!: () => void,
      releaseFunds!: () => void,
      seenFunds!: () => void;
    const sessionGate = new Promise<void>((r) => {
      releaseSession = r;
    });
    const sessionSeen = new Promise<void>((r) => {
      seenSession = r;
    });
    const fundsGate = new Promise<void>((r) => {
      releaseFunds = r;
    });
    const fundsSeen = new Promise<void>((r) => {
      seenFunds = r;
    });
    await page.route('**/api/sim/session', async (r) => {
      if (r.request().method() === 'POST')
        return reply(r, { mode: 'PROJECT_SIMULATION', authenticated: true });
      seenSession();
      await sessionGate;
      return reply(r, { mode: 'PROJECT_SIMULATION', authenticated: false });
    });
    await page.route(`**${paymentBase(h.a.payment)}/funds-result`, async (r) => {
      seenFunds();
      await fundsGate;
      await reply(r, result(h.a, 'SUCCEEDED')).catch(() => {});
    });
    // The customer page checks the session on its existing 15-second timer.
    await page.clock.fastForward(15000);
    await sessionSeen;
    await page.getByRole('button', { name: '查询保留的付款原单', exact: true }).click();
    await fundsSeen;
    releaseSession();
    await expect(page.getByLabel('模拟登录口令')).toBeVisible();
    releaseFunds();
    await page.getByLabel('模拟登录口令').fill('protocol-replay-only');
    await page.getByRole('button', { name: '登录', exact: true }).click();
    await stillProtected(page, h.marker);
    await expect(page.getByRole('region', { name: '付款恢复' })).not.toContainText(
      '原单成功（已核验）',
    );
    expect(h.posts).toEqual(['A']);
  });
});
