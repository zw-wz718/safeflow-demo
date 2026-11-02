import { expect, test, type Page, type Route } from '@playwright/test';
import { join } from 'node:path';
import { writeFileSync } from 'node:fs';
import payment from '../src/runtime/payment-backend.fixture.json' with { type: 'json' };
import mixed from '../src/runtime/billing-mixed.fixture.json' with { type: 'json' };
import partial from '../src/runtime/billing-page-failed.fixture.json' with { type: 'json' };
import refund from '../src/runtime/billing-unknown-refund.fixture.json' with { type: 'json' };
import processing from '../src/runtime/payment-processing.fixture.json' with { type: 'json' };
import { fundsResultSchema, paymentBase } from '../src/runtime/payment-schema';
import { guardReplayApi, assertReplayIsolated } from './isolated-api';

test.afterEach(({ page }) => assertReplayIsolated(page));

const evidence = process.env.SAFEFLOW_WEB_EVIDENCE;
const secret = process.env.SAFEFLOW_AGENT_E2E_SECRET;
const dataReply = (route: Route, value: unknown) =>
  route.fulfill({ contentType: 'application/json', body: JSON.stringify(value) });
async function screenshot(page: Page, name: string) {
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  if (evidence) await page.screenshot({ path: join(evidence, name), fullPage: true });
}
async function login(page: Page) {
  await page.goto('/runtime.html');
  await page.getByLabel('模拟登录口令').fill(secret!);
  await page.getByRole('button', { name: '登录', exact: true }).click();
  await expect(page.locator('.a-model-status')).toContainText('固定计划 / MockLlm');
}
async function sample(page: Page, sample: string) {
  await page.getByLabel('固定计划样例', { exact: true }).selectOption(sample);
  await page.getByRole('button', { name: '运行样例', exact: true }).click();
}
async function draft(page: Page) {
  await page.getByLabel('账户编号', { exact: true }).fill('account_001');
  await page.getByLabel('收款人', { exact: true }).fill('小李');
  await page.getByRole('button', { name: '查找收款人', exact: true }).click();
  await page.getByLabel('选择收款人').selectOption('payee_008');
  await sample(page, 'CREATE_200');
  await page.getByRole('button', { name: '确认保存草案', exact: true }).click();
  await expect(page.locator('.a-receipt')).toContainText('草案已保存');
}
test.describe('actual payment HTTP / PostgreSQL, no model and no funds execution', () => {
  test.skip(!secret, 'Requires the existing fixed-plan API and explicit shared demo source');
  test('independent confirmation, duplicate click, evaluate, original query, reload, modify and reject', async ({
    page,
  }, info) => {
    const errors: string[] = [];
    page.on('pageerror', (e) => errors.push(e.message));
    const writes: string[] = [];
    page.on('request', (r) => {
      if (r.method() === 'POST' && r.url().includes('/payment-preparations/')) writes.push(r.url());
    });
    await login(page);
    await expect(page.locator('.a-model-status')).toContainText('账单报告来源未接入');
    await draft(page);
    await sample(page, 'EXECUTE_TRANSFER');
    const panel = page.getByRole('region', { name: '独立付款准备' });
    await expect(panel).toContainText('等待独立付款确认');
    await expect(panel).toContainText('200.00 CNY');
    expect(writes).toHaveLength(0);
    await screenshot(page, `payment-${info.project.name}-prepared.png`);
    // Two synchronous click events test the command ref, before React rerenders disabled.
    await page
      .getByRole('button', { name: '确认付款信息', exact: true })
      .evaluate((button: HTMLButtonElement) => {
        button.click();
        button.click();
      });
    await expect(panel).toContainText('付款意愿已确认 · 等待执行条件');
    expect(writes.filter((p) => p.endsWith('/confirm'))).toHaveLength(1);
    await expect(panel).toContainText('YELLOW');
    await expect(panel).toContainText('OUTGOING_STATE_UNKNOWN');
    await page.getByRole('button', { name: '重新评估付款条件' }).click();
    await expect(page.getByRole('button', { name: '重新评估付款条件' })).toBeEnabled();
    await page.getByRole('button', { name: '查询付款原单', exact: true }).click();
    await expect(panel).toContainText('当前快照未发现提交记录');
    await page.reload();
    await expect(panel).toContainText('付款意愿已确认 · 等待执行条件');
    expect(writes.filter((p) => p.endsWith('/confirm'))).toHaveLength(1);
    await screenshot(page, `payment-${info.project.name}-waiting.png`);
    await sample(page, 'MODIFY_300');
    await page.getByRole('button', { name: '确认修改草案' }).click();
    await expect(page.locator('.a-receipt')).toContainText('草案已修改');
    await sample(page, 'EXECUTE_TRANSFER');
    await expect(panel).toContainText('300.00 CNY');
    await page.getByRole('button', { name: '拒绝付款', exact: true }).click();
    await expect(panel).toContainText('REJECTED');
    await expect(page.getByRole('button', { name: '确认付款信息' })).toHaveCount(0);
    await sample(page, 'CANCEL_DRAFT');
    await page.getByRole('button', { name: '确认取消草案' }).click();
    await expect(page.locator('.a-receipt')).toContainText('草案已取消');
    expect(errors).toEqual([]);
    expect(writes.every((p) => p.endsWith('/confirm') || p.endsWith('/evaluate'))).toBe(true);
  });
  test('committed confirmation with lost response recovers by GET, never reposts', async ({
    page,
  }, info) => {
    await login(page);
    await draft(page);
    await sample(page, 'EXECUTE_TRANSFER');
    let confirms = 0;
    await page.route('**/api/sim/payment-preparations/*/*/confirm', async (route) => {
      confirms++;
      const response = await route.fetch();
      expect(response.ok()).toBe(true);
      await route.abort('connectionfailed');
    });
    await page.getByRole('button', { name: '确认付款信息' }).click();
    await expect(page.locator('.r-uncertain')).toContainText('提交结果未明');
    await expect(page.getByRole('button', { name: '确认付款信息' })).toBeDisabled();
    await page.getByRole('button', { name: '刷新付款状态' }).click();
    await expect(page.locator('.a-payment-result')).toContainText('付款意愿已确认 · 等待执行条件');
    await page.reload();
    await expect(page.locator('.a-payment-result')).toContainText('付款意愿已确认');
    expect(confirms).toBe(1);
    await screenshot(page, `payment-${info.project.name}-recovered.png`);
  });
});

// Captured HTTP/PG responses; transition tests change only foundation projection
// state to model stale screens, preserving the original member request/receipts.
async function isolated(page: Page, initial: unknown) {
  await guardReplayApi(page);
  await page.clock.install({ time: new Date(payment.prepared.payment.prepared_at) });
  await page.addInitScript(
    (id) => sessionStorage.setItem('safeflow.agent.last-task.v1', id),
    (initial as typeof payment.prepared).draft?.task_id ?? (initial as typeof mixed).task.task_id,
  );
  await page.route('**/api/health', (r) =>
    dataReply(r, { status: 'READY', mode: 'PROJECT_SIMULATION' }),
  );
  await page.route('**/api/sim/session', (r) =>
    dataReply(r, { mode: 'PROJECT_SIMULATION', authenticated: true }),
  );
  await page.route('**/api/agent/status', (r) =>
    dataReply(r, {
      contract_version: 1,
      mode: 'PROJECT_SIMULATION',
      configured: false,
      connectivity: 'NOT_VERIFIED',
      tools: ['analyze_local_basic_spending', 'execute_transfer'],
      draft_enabled: true,
      planning_mode: 'FIXED_PLAN',
      payment_planning_enabled: true,
      funds_execution_enabled: false,
    }),
  );
  await page.route('**/api/agent/tasks/*', (r) => dataReply(r, initial));
  await page.goto('/runtime.html');
}
test.describe('isolated captured response browser regressions', () => {
  for (const confirmed of [false, true]) {
    test(`original discovery locks stale ${confirmed ? 'evaluation' : 'confirmation'} through pending, failed and stale refresh`, async ({
      page,
    }, info) => {
      const stale = structuredClone(processing);
      Object.assign(stale.payment, {
        business_recovery_required: false,
        invalid_reason: null,
        confirmation_status: confirmed ? 'CONFIRMED' : 'PREPARED',
        confirmed_at: confirmed ? stale.payment.prepared_at : null,
        state: confirmed
          ? 'CONFIRMED_AWAITING_EXECUTION_CONDITIONS'
          : 'AWAITING_PAYMENT_CONFIRMATION',
        decision_freshness: 'NOT_EVALUATED',
      });
      Object.assign(stale.payment_flow, {
        original: null,
        status: confirmed ? 'WAITING_EXECUTION_CONDITIONS' : 'WAITING_PAYMENT_CONFIRMATION',
        next_action: confirmed ? 'QUERY_OR_EVALUATE' : 'CONFIRM_OR_REJECT',
        next_path: stale.payment.original_result_path.replace(
          '/original-result',
          confirmed ? '/evaluate' : '/confirm',
        ),
      });
      const writes: string[] = [];
      page.on('request', (r) => {
        if (r.method() === 'POST') writes.push(r.url());
      });
      await isolated(page, stale);
      const oldAction = page.getByRole('button', {
        name: confirmed ? '重新评估付款条件' : '确认付款信息',
      });
      await expect(oldAction).toBeEnabled();
      await expect(page.getByRole('button', { name: '运行样例' })).toBeEnabled();
      let release!: () => void;
      const gate = new Promise<void>((resolve) => {
        release = resolve;
      });
      let started!: () => void;
      const seen = new Promise<void>((resolve) => {
        started = resolve;
      });
      let gets = 0;
      await page.route('**/api/agent/tasks/*', async (r) => {
        gets++;
        if (gets === 1) {
          started();
          await gate;
          await r.fulfill({ status: 503, contentType: 'application/json', body: '{}' });
        } else await dataReply(r, gets === 2 ? stale : processing);
      });
      await page.route('**/api/sim/payment-preparations/*/*/original-result', (r) =>
        dataReply(r, processing.payment_flow.original),
      );
      await page.getByRole('button', { name: '查询付款原单', exact: true }).click();
      await seen;
      const panel = page.locator('.a-payment-result');
      await expect(panel).toContainText('已有原单 · 仅查询恢复');
      await expect(panel).toContainText('PROCESSING');
      await expect(oldAction).toHaveCount(0);
      await expect(page.getByRole('button', { name: '运行样例' })).toBeDisabled();
      await expect(page.getByLabel('固定计划样例', { exact: true })).toBeDisabled();
      await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeDisabled();
      await screenshot(page, `original-${info.project.name}-${confirmed}-pending.png`);
      release();
      await expect(page.getByRole('button', { name: '刷新付款状态' })).toBeEnabled();
      await expect(page.getByRole('alert')).toBeVisible();
      await expect(oldAction).toHaveCount(0);
      await expect(page.getByRole('button', { name: '运行样例' })).toBeDisabled();
      await screenshot(page, `original-${info.project.name}-${confirmed}-failed.png`);
      // Even a successful but stale GET must not erase the discovered order.
      await page.getByRole('button', { name: '刷新付款状态' }).click();
      await expect(page.getByRole('button', { name: '查询付款原单', exact: true })).toBeEnabled();
      await expect(oldAction).toHaveCount(0);
      await expect(panel).toContainText('已有原单 · 仅查询恢复');
      await expect(page.getByRole('button', { name: '运行样例' })).toBeDisabled();
      // Exercise the submit handler independently of disabled buttons.
      await page
        .locator('.a-composer')
        .evaluate((form) =>
          form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true })),
        );
      await page.getByRole('button', { name: '刷新付款状态' }).click();
      await expect(panel).toContainText('请保留原号查询付款结果');
      await expect(panel).toContainText('PROCESSING');
      await expect(panel).not.toContainText('尚未扣款');
      await expect(oldAction).toHaveCount(0);
      await expect(page.getByRole('button', { name: '运行样例' })).toBeDisabled();
      expect(gets).toBe(3);
      expect(writes).toEqual([]);
      await screenshot(page, `original-${info.project.name}-${confirmed}-recovered.png`);
    });
  }
  test('internal-only registration lookup retains confirmation and draft modification/cancellation', async ({
    page,
  }) => {
    await isolated(page, payment.prepared);
    await expect(page.getByRole('button', { name: '确认付款信息' })).toBeEnabled();
    const registered = { ...payment.original, state: 'REGISTERED_UNSUBMITTED' };
    await page.route('**/api/sim/payment-preparations/*/*/original-result', (r) =>
      dataReply(r, registered),
    );
    await page.getByRole('button', { name: '查询付款原单', exact: true }).click();
    await expect(page.locator('.a-original')).toContainText('内部已登记 · 未提交付款');
    await expect(page.getByRole('button', { name: '确认付款信息' })).toBeEnabled();
    await expect(page.getByRole('button', { name: '运行样例' })).toBeEnabled();
    await expect(
      page.getByLabel('固定计划样例', { exact: true }).locator('option[value="MODIFY_300"]'),
    ).toHaveCount(1);
    await expect(
      page.getByLabel('固定计划样例', { exact: true }).locator('option[value="CANCEL_DRAFT"]'),
    ).toHaveCount(1);
  });
  test('payment expires locally, requery invalidates, same draft can still be modified/cancelled', async ({
    page,
  }, info) => {
    await isolated(page, payment.prepared);
    await expect(page.getByRole('button', { name: '确认付款信息' })).toBeEnabled();
    await page.clock.fastForward(301000);
    await expect(page.getByRole('button', { name: '确认付款信息' })).toBeDisabled();
    await expect(page.locator('.a-payment-result')).toContainText('确认已到期');
    const expired = structuredClone(payment.prepared);
    Object.assign(expired.payment, { invalid_reason: 'EXPIRED', state: 'INVALIDATED' });
    Object.assign(expired.payment_flow, {
      status: 'ENDED_INVALIDATED',
      next_action: 'REVIEW_CURRENT_DRAFT',
      next_path: `/api/agent/tasks/${expired.payment.task_id}`,
    });
    await page.route('**/api/agent/tasks/*', (r) => dataReply(r, expired));
    await page.getByRole('button', { name: '刷新付款状态' }).click();
    await expect(page.locator('.a-payment-result')).toContainText('付款准备已结束');
    await expect(page.getByRole('button', { name: '运行样例' })).toBeEnabled();
    await expect(
      page.getByLabel('固定计划样例', { exact: true }).locator('option[value="MODIFY_300"]'),
    ).toHaveCount(1);
    await expect(
      page.getByLabel('固定计划样例', { exact: true }).locator('option[value="CANCEL_DRAFT"]'),
    ).toHaveCount(1);
    await screenshot(page, `payment-${info.project.name}-expired.png`);
  });
  test('old payment query response cannot restore a lost session', async ({ page }) => {
    const paymentWrites: string[] = [];
    page.on('request', (r) => {
      if (r.method() === 'POST' && r.url().includes('/payment-preparations/'))
        paymentWrites.push(r.url());
    });
    await isolated(page, payment.prepared);
    await expect(page.getByRole('button', { name: '刷新付款状态' })).toBeEnabled();
    let release!: () => void;
    const gate = new Promise<void>((r) => {
      release = r;
    });
    let started!: () => void;
    const seen = new Promise<void>((r) => {
      started = r;
    });
    await page.route('**/api/agent/tasks/*', async (route) => {
      started();
      await gate;
      await dataReply(route, payment.waiting).catch(() => {});
    });
    await page.getByRole('button', { name: '刷新付款状态' }).click();
    await seen;
    await page.route('**/api/sim/session', (r) =>
      dataReply(r, { mode: 'PROJECT_SIMULATION', authenticated: false }),
    );
    // A read query does not block session polling/checks; generation invalidation
    // must discard even a successful old response after the request was aborted.
    await page.getByRole('button', { name: '刷新连接', exact: true }).click();
    await expect(page.getByLabel('模拟登录口令')).toBeVisible();
    release();
    await expect(page.locator('.a-payment-result')).toHaveCount(0);
    const changed = structuredClone(payment.prepared);
    Object.assign(changed.payment, { invalid_reason: 'SESSION_CHANGED', state: 'INVALIDATED' });
    Object.assign(changed.payment_flow, {
      status: 'ENDED_INVALIDATED',
      next_action: 'REVIEW_CURRENT_DRAFT',
      next_path: `/api/agent/tasks/${changed.payment.task_id}`,
    });
    await page.route('**/api/agent/tasks/*', (r) => dataReply(r, changed));
    // A session-interrupted discovery now replays its actual original GET even
    // on this same component. Use the existing captured original unchanged.
    const originalReads: string[] = [];
    const originalPath = payment.prepared.payment.original_result_path;
    await page.route(`**${originalPath}`, (r) => {
      originalReads.push(`${r.request().method()} ${new URL(r.request().url()).pathname}`);
      return dataReply(r, payment.original);
    });
    await page.route('**/api/sim/session', (r) =>
      dataReply(r, { mode: 'PROJECT_SIMULATION', authenticated: true }),
    );
    await page.getByLabel('模拟登录口令').fill('isolated-session-test');
    const recovered = page.waitForResponse((r) => new URL(r.url()).pathname === originalPath);
    await page.getByRole('button', { name: '登录', exact: true }).click();
    const originalResponse = await recovered;
    await originalResponse.finished();
    expect(originalResponse.status()).toBe(200);
    expect(await originalResponse.json()).toEqual(payment.original);
    await expect(page.getByRole('button', { name: '查询原任务', exact: true })).toBeEnabled();
    await page.getByRole('button', { name: '查询原任务', exact: true }).click();
    await expect(page.locator('.a-payment-result')).toContainText('SESSION_CHANGED');
    await expect(page.getByRole('button', { name: '确认付款信息' })).toHaveCount(0);
    expect(originalReads).toEqual([`GET ${originalPath}`]);
    expect(paymentWrites).toEqual([]);
  });
  test('captured processing original only queries the stable order; no new intent or retry', async ({
    page,
  }, info) => {
    const writes: string[] = [];
    page.on('request', (r) => {
      if (r.method() === 'POST') writes.push(r.url());
    });
    await isolated(page, processing);
    const original = processing.payment_flow.original;
    const expectedPath = `${paymentBase(processing.payment)}/funds-result`;
    const read = fundsResultSchema.parse({
      contract_version: 1,
      scope: 'PAYMENT_FUNDS_RESULT_ONLY',
      task_id: original.task_id,
      preparation_operation_id: original.binding.preparation_operation_id,
      snapshot_digest: original.binding.preparation_arguments_hash,
      transfer: original.binding.normalized_arguments,
      state: original.state,
      submitted: !!(original.first_public_submission || original.verified_business_result),
      automatic_retry: false,
    });
    let release!: () => void;
    const gate = new Promise<void>((r) => {
      release = r;
    });
    const requests: string[] = [];
    await page.route(`**${expectedPath}`, async (r) => {
      requests.push(`${r.request().method()} ${new URL(r.request().url()).pathname}`);
      await gate;
      await dataReply(r, read);
    });
    const panel = page.locator('.a-payment-result');
    await expect(panel).toContainText('已有原单 · 仅查询恢复');
    await expect(panel).toContainText('原单结果待核对');
    await expect(panel).toContainText('PROCESSING');
    await expect(page.getByRole('button', { name: '运行样例' })).toBeDisabled();
    await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeDisabled();
    const responseReceived = page.waitForResponse(
      (r) => new URL(r.url()).pathname === expectedPath,
    );
    await page.getByRole('button', { name: '查询付款原单', exact: true }).click();
    await expect.poll(() => requests.length).toBe(1);
    await expect(page.getByRole('button', { name: '查询付款原单', exact: true })).toBeDisabled();
    release();
    const response = await responseReceived;
    expect(response.status()).toBe(200);
    await response.finished();
    expect(await response.json()).toEqual(read);
    await expect(page.getByRole('button', { name: '查询付款原单', exact: true })).toBeEnabled();
    await expect(panel).toContainText(
      processing.payment_flow.original.binding.business_operation_id,
    );
    await expect(page.getByRole('button', { name: '确认付款信息' })).toHaveCount(0);
    await expect(page.getByRole('button', { name: '退出会话', exact: true })).toBeEnabled();
    await expect(page.getByLabel('模拟登录口令')).toHaveCount(0);
    expect(requests).toEqual([`GET ${expectedPath}`]);
    expect(writes).toEqual([]);
    await info.attach('completed-original-read', {
      body: JSON.stringify({
        requests,
        state: read.state,
        original: read.transfer.operation_id,
        sessionRetained: true,
        writes,
      }),
      contentType: 'application/json',
    });
    if (evidence)
      writeFileSync(
        join(evidence, `legacy-read-${info.project.name}-${info.repeatEachIndex}.json`),
        JSON.stringify(
          {
            requests,
            responseStatus: response.status(),
            responseBody: await response.json(),
            sessionRetained: true,
            writes,
          },
          null,
          2,
        ) + '\n',
      );
    await screenshot(page, `payment-${info.project.name}-processing.png`);
  });
  for (const [name, raw] of [
    ['mixed', mixed],
    ['partial', partial],
    ['refund', refund],
  ] as const) {
    test(`captured billing ${name}: saved history and completeness`, async ({ page }, info) => {
      const posts: string[] = [];
      page.on('request', (r) => {
        if (r.method() === 'POST') posts.push(r.url());
      });
      await isolated(page, raw);
      const report = page.getByRole('region', { name: '本地账单报告' });
      await expect(report).toBeVisible();
      if (name === 'partial') {
        await expect(report).toContainText('暂不可用');
        await expect(report).toContainText('READ_BLOCKED');
        await expect(report).not.toContainText('已取全');
      } else if (name === 'mixed') {
        await expect(report).toContainText('60.00');
        await expect(report).toContainText('5.00');
      } else await expect(report).toContainText('未提供 · 待核对');
      await page.reload();
      await expect(report).toBeVisible();
      expect(posts).toEqual([]);
      await screenshot(page, `billing-${info.project.name}-${name}.png`);
    });
  }
});
