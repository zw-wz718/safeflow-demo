import fs from 'node:fs';
import { expect, test, type Page, type Route } from '@playwright/test';
import captured from '../src/runtime/payment-backend.fixture.json' with { type: 'json' };
import { agentSchema, type AgentView } from '../src/runtime/agent-client';
import { paymentBase, fundsReference, type PaymentFunds } from '../src/runtime/payment-schema';
import { legacyRecoveryKey, recoveryKey } from '../src/runtime/payment-recovery';
import { recoveryJournalKey } from '../src/runtime/payment-recovery-store';
import { guardReplayApi, assertReplayIsolated } from './isolated-api';

test.afterEach(({ page }) => assertReplayIsolated(page));
type PaymentView = Extract<AgentView, { kind: 'PAYMENT' }>;
const reply = (r: Route, data: unknown) =>
  r.fulfill({ contentType: 'application/json', body: JSON.stringify(data) });
function projection(tag: 'a' | 'b' | 'c'): PaymentView {
  const seed = captured.waiting;
  let text = JSON.stringify(seed);
  for (const [from, to] of [
    [seed.payment.task_id, `lifetime_task_${tag}`],
    [seed.payment.operation.operation_id, `lifetime_preparation_${tag}`],
    [seed.payment.operation.node_id, `lifetime_node_${tag}`],
    [seed.payment.snapshot.transfer.operation_id, `lifetime_operation_${tag}`],
    [seed.payment.snapshot.transfer.draft_id, `lifetime_draft_${tag}`],
    [seed.payment.snapshot_digest, tag.repeat(64)],
  ])
    text = text.replaceAll(from, to);
  const raw = JSON.parse(text),
    p = raw.payment;
  p.expires_at = new Date(Date.now() + 300000).toISOString();
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
  const parsed = agentSchema.parse(raw);
  if (parsed.kind !== 'PAYMENT') throw Error('Invalid foundation fixture');
  return parsed;
}
const funds = (v: PaymentView, state: PaymentFunds['state']): PaymentFunds => ({
  contract_version: 1,
  scope: 'PAYMENT_FUNDS_RESULT_ONLY',
  task_id: v.payment.task_id,
  preparation_operation_id: v.payment.operation.operation_id,
  snapshot_digest: v.payment.snapshot_digest,
  transfer: v.payment.snapshot.transfer,
  state,
  submitted: state !== 'UNSUBMITTED',
  automatic_retry: false,
});
function retained(page: Page, v: PaymentView) {
  return page.getByRole('article', {
    name: `保留付款 ${v.payment.snapshot.transfer.operation_id}`,
    exact: true,
  });
}
const ready = (page: Page) =>
  expect(page.getByRole('button', { name: '查询原任务', exact: true })).toBeEnabled();
async function select(page: Page, v: PaymentView, customer = false) {
  await page.getByLabel(customer ? '办理编号' : '任务号', { exact: true }).fill(v.payment.task_id);
  await page.getByRole('button', { name: '查询原任务', exact: true }).click();
  await expect(page.locator('.a-payment-result')).toContainText(
    v.payment.snapshot.transfer.operation_id,
  );
  await ready(page);
}
async function snapshot(page: Page) {
  return page.evaluate(
    (keys) => Object.fromEntries(keys.map((k) => [k, sessionStorage.getItem(k)])),
    [recoveryKey, legacyRecoveryKey, recoveryJournalKey],
  );
}
// Wait for the response's promise handlers and React commits, not just HTTP headers.
const settled = (page: Page) =>
  page.evaluate(
    () =>
      new Promise<void>((resolve) =>
        requestAnimationFrame(() => requestAnimationFrame(() => resolve())),
      ),
  );
async function setup(
  page: Page,
  customer = false,
  bState: PaymentFunds['state'] = 'PROCESSING',
  exact = false,
) {
  await guardReplayApi(page);
  await page.clock.install();
  const a = projection('a'),
    b = projection('b'),
    c = projection('c'),
    views = [a, b, c];
  const states = { a: 'PROCESSING' as PaymentFunds['state'], b: bState };
  const posts: string[] = [];
  const failedRequests: { path: string; error: string | null }[] = [];
  page.on('requestfailed', (request) =>
    failedRequests.push({
      path: new URL(request.url()).pathname,
      error: request.failure()?.errorText ?? null,
    }),
  );
  let old: Route | undefined,
    released = false,
    authenticated = true;
  let seen!: () => void;
  const pending = new Promise<void>((r) => {
    seen = r;
  });
  let finish!: () => void;
  const gate = new Promise<void>((r) => {
    finish = r;
  });
  let outcome = 'failure';
  await page.addInitScript((id) => {
    if (!sessionStorage.getItem('safeflow.agent.last-task.v1'))
      sessionStorage.setItem('safeflow.agent.last-task.v1', id);
    sessionStorage.setItem('safeflow.runtime.task-mode.v1', 'agent');
  }, a.payment.task_id);
  await page.route('**/api/health', (r) =>
    reply(r, { contract_version: 1, mode: 'PROJECT_SIMULATION', ready: true }),
  );
  await page.route('**/api/sim/session', (r) => {
    if (r.request().method() === 'POST') authenticated = true;
    return reply(r, { mode: 'PROJECT_SIMULATION', authenticated });
  });
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
  const delay = async (r: Route) => {
    old = r;
    seen();
    await gate;
    try {
      if (outcome === 'abort') await r.abort('aborted');
      else if (outcome === 'success')
        await reply(
          r,
          exact
            ? funds(b, 'SUCCEEDED')
            : {
                ...b,
                payment_flow: { ...b.payment_flow, funds_result: funds(b, 'SUCCEEDED') },
              },
        );
      else await r.fulfill({ status: 503, contentType: 'application/json', body: '{}' });
    } finally {
      released = true;
    }
  };
  await page.route('**/api/agent/tasks/*', async (r) => {
    const v = views.find((v) =>
      r
        .request()
        .url()
        .endsWith('/' + v.payment.task_id),
    );
    if (!v) throw Error('Unknown task');
    if (v === b && !old && !exact) return delay(r);
    return reply(r, v);
  });
  await page.route('**/payment-preparations/*/*/execute', (r) => {
    const v = views.find((v) =>
      r
        .request()
        .url()
        .includes('/' + v.payment.task_id + '/'),
    )!;
    posts.push(v.payment.task_id);
    return reply(r, funds(v, 'PROCESSING'));
  });
  await page.route('**/payment-preparations/*/*/funds-result', async (r) => {
    const v = views.find((v) =>
      r
        .request()
        .url()
        .includes('/' + v.payment.task_id + '/'),
    )!;
    if (v === b && !old && exact) return delay(r);
    return reply(r, funds(v, v === a ? states.a : v === b ? states.b : 'UNSUBMITTED'));
  });
  if (customer) {
    await page.goto('/#transfers');
    await page.getByRole('button', { name: '在线模拟付款', exact: true }).click();
  } else await page.goto('/runtime.html');
  await page.getByRole('button', { name: '确认付款', exact: true }).click();
  await expect(page.getByRole('button', { name: '查询付款原单', exact: true })).toBeEnabled();
  if (exact) {
    await select(page, b, customer);
    await page.getByRole('button', { name: '查询付款原单', exact: true }).click();
  } else {
    await page
      .getByLabel(customer ? '办理编号' : '任务号', { exact: true })
      .fill(b.payment.task_id);
    await page.getByRole('button', { name: '查询原任务', exact: true }).click();
  }
  await pending;
  return {
    a,
    b,
    c,
    states,
    posts,
    failedRequests,
    revoke: () => {
      authenticated = false;
    },
    release: async (kind: string, cancelled = false) => {
      outcome = kind;
      const done =
        cancelled || kind === 'abort' || kind === 'timeout'
          ? Promise.resolve()
          : page.waitForResponse((r) => r.url() === old!.request().url());
      finish();
      const response = await done;
      if (response) await response.finished();
      await expect.poll(() => released).toBe(true);
      await settled(page);
    },
  };
}

test.describe('isolated recovery callbacks respect component and session lifetime', () => {
  for (const kind of [
    'failure',
    'abort',
    'success',
    'timeout',
    'funds-success',
    'session-abort',
    'strict-processing',
    'strict-unsubmitted',
  ]) {
    test(`${kind}: obsolete callbacks preserve the new ledger and zero premature C POST`, async ({
      page,
    }, info) => {
      const customer = kind.startsWith('strict-') || kind === 'funds-success';
      const exact = kind === 'funds-success';
      const h = await setup(
        page,
        customer,
        kind === 'strict-unsubmitted' ? 'UNSUBMITTED' : 'PROCESSING',
        exact,
      );
      const interrupted = await snapshot(page);
      expect(
        JSON.parse(interrupted[recoveryJournalKey]!).reads.some(
          (r: { task_id: string }) => r.task_id === h.b.payment.task_id,
        ),
      ).toBe(true);
      if (kind === 'session-abort') {
        h.revoke();
        await page.getByRole('button', { name: '刷新连接', exact: true }).click();
        await expect(page.getByLabel('模拟登录口令')).toBeVisible();
        // Parent abort really ran; the pending lookup must survive it.
        expect(JSON.parse((await snapshot(page))[recoveryJournalKey]!).reads).toHaveLength(1);
        await page.getByLabel('模拟登录口令').fill('isolated-lifetime-protocol-only');
        await page.getByRole('button', { name: '登录', exact: true }).click();
      } else if (customer) {
        // The real bank entry mounts RuntimeApp under main.tsx StrictMode.
        await page.getByRole('button', { name: '本机草案', exact: true }).click();
        await page.getByRole('button', { name: '在线模拟付款', exact: true }).click();
      } else {
        await page.getByRole('button', { name: '只读任务', exact: true }).click();
        await page.getByRole('button', { name: 'Agent 助手', exact: true }).click();
      }
      await ready(page);
      await select(page, h.b, customer);
      if (exact) {
        await page.getByRole('button', { name: '查询付款原单', exact: true }).click();
        await expect(page.locator('.a-payment-result')).toContainText('原单处理中');
      }
      const bSubmitted = kind !== 'strict-unsubmitted';
      if (bSubmitted) await expect(page.locator('.a-payment-result')).toContainText('原单处理中');
      else await expect(retained(page, h.b)).toHaveCount(0);
      await expect(page.getByRole('alert')).toHaveCount(0);
      await select(page, h.c, customer);
      const before = await snapshot(page);
      expect(JSON.parse(before[recoveryKey]!).references).toEqual([
        fundsReference(h.a.payment),
        ...(bSubmitted ? [fundsReference(h.b.payment)] : []),
      ]);
      if (kind === 'timeout') await page.clock.fastForward(36000);
      await h.release(exact ? 'success' : kind, customer || kind === 'session-abort');
      if (customer || ['session-abort', 'abort', 'timeout'].includes(kind))
        await expect
          .poll(() => h.failedRequests.some((r) => r.path.includes(h.b.payment.task_id)))
          .toBe(true);
      const after = await snapshot(page);
      expect(after).toEqual(before); // All three complete durable values, including journal reads/phase.
      if (bSubmitted) await expect(retained(page, h.b)).toContainText('原单处理中');
      await page.screenshot({
        path: `${process.env.SAFEFLOW_WEB_EVIDENCE}/lifetime-${info.project.name}-${kind}-before-reload.png`,
        fullPage: true,
      });
      await page.reload();
      if (customer) await page.getByRole('button', { name: '在线模拟付款', exact: true }).click();
      await ready(page);
      if (bSubmitted) await expect(retained(page, h.b)).toBeVisible();
      h.states.a = 'SUCCEEDED';
      await retained(page, h.a)
        .getByRole('button', { name: '查询保留的付款原单', exact: true })
        .click();
      const newTask = page.getByRole('button', { name: '新任务', exact: true });
      if (bSubmitted) {
        await expect(newTask).toBeDisabled();
        await expect(page.getByRole('button', { name: '确认付款', exact: true })).toBeDisabled();
        for (const button of [newTask, page.getByRole('button', { name: '确认付款', exact: true })])
          await button.evaluate((b) => {
            b.removeAttribute('disabled');
            b.dispatchEvent(new MouseEvent('click', { bubbles: true }));
          });
        await select(page, h.c, customer);
        expect(h.posts).toEqual([h.a.payment.task_id]);
        h.states.b = 'FAILED';
        await retained(page, h.b)
          .getByRole('button', { name: '查询保留的付款原单', exact: true })
          .click();
      }
      await expect(newTask).toBeEnabled();
      const protectedSnapshot = await snapshot(page);
      await page.screenshot({
        path: `${process.env.SAFEFLOW_WEB_EVIDENCE}/lifetime-${info.project.name}-${kind}-resolved.png`,
        fullPage: true,
      });
      await newTask.click();
      await select(page, h.c, customer);
      await page.getByRole('button', { name: '确认付款', exact: true }).click();
      await expect.poll(() => h.posts).toEqual([h.a.payment.task_id, h.c.payment.task_id]);
      fs.writeFileSync(
        `${process.env.SAFEFLOW_WEB_EVIDENCE}/lifetime-${info.project.name}-${kind}.json`,
        JSON.stringify(
          {
            protocolReplayOnly: true,
            actualUnmount: kind !== 'session-abort',
            customerStrictMode: customer,
            interrupted,
            beforeOldCallback: before,
            afterOldCallback: after,
            allReferencesFullyRetained: true,
            protectedUntilEachTerminal: bSubmitted,
            afterEachTerminal: protectedSnapshot,
            postsBeforeAllResolved: [h.a.payment.task_id],
            postsAfterAllResolved: h.posts,
            failedRequests: h.failedRequests,
          },
          null,
          2,
        ),
      );
    });
  }
});
