import { expect, test, type Page, type Route } from '@playwright/test';

const journalKey = 'safeflow.runtime.draft.execution-attempts.v1';
const oldPair = { task_id: 'old_task', operation_id: 'old_operation' };

function prepared(taskId: string, operationId: string) {
  const now = Date.now();
  return {
    contract_version: 1,
    mode: 'PROJECT_SIMULATION',
    scope: 'MOCK_DRAFT_CONFIRMATION_ONLY',
    task_id: taskId,
    operation_id: operationId,
    task_state: 'RUNNING',
    operation_state: 'DRAFT',
    task_revision: 2,
    operation_revision: 1,
    plan_version: 1,
    facts_version: 7,
    parameters: {
      payer_account_id: 'account_001',
      payee_id: 'payee_008',
      amount: '200.00',
      currency: 'CNY',
      remark: '',
      replaces: null,
    },
    confirmation: {
      confirmation_ref: `confirmation_${taskId}`,
      status: 'PREPARED',
      prepared_at: new Date(now).toISOString(),
      expires_at: new Date(now + 300000).toISOString(),
      confirmed_at: null as string | null,
      revoked_at: null,
    },
    preflight: {
      evidence_kind: 'PREFLIGHT_ONLY',
      pre_tool_outcome: 'READY_TO_CONFIRM',
      pre_auth_outcome: 'READY_TO_CONFIRM',
      required_gates: ['EXPLICIT_CONFIRMATION'],
    },
    display_ticket: `display_${taskId}` as string | null,
    next_action: { kind: 'CONFIRM' },
  };
}
function completed(pair = oldPair) {
  return {
    contract_version: 1,
    mode: 'PROJECT_SIMULATION',
    scope: 'MOCK_DRAFT_EXECUTION_ONLY',
    ...pair,
    task_state: 'COMPLETED',
    operation_state: 'SUCCEEDED',
    task_revision: 3,
    operation_revision: 2,
    plan_version: 1,
    facts_version: 7,
    execution_status: 'COMPLETED',
    receipt_scope: 'HISTORICAL_EXECUTION_RECEIPT',
    current_draft_verified: false,
    receipt: {
      draft_id: `draft_${pair.task_id}`,
      business_operation_id: `business_${pair.task_id}`,
      draft_version: 1,
      tool_result_ref: `result_${pair.task_id}`,
      executed_at: new Date().toISOString(),
    },
    replayed: true,
    next_action: { kind: 'NONE' },
  };
}
async function reply(route: Route, value: unknown, status = 200) {
  await route.fulfill({ status, contentType: 'application/json', body: JSON.stringify(value) });
}
async function notFound(route: Route, taskId: string) {
  await reply(
    route,
    {
      contract_version: 1,
      error: {
        reason_code: 'NOT_FOUND',
        message: 'No original claim in this fixture.',
        task_id: taskId,
        next_action: 'QUERY_ORIGINAL_TASK',
      },
    },
    404,
  );
}

// Frozen public wire fixtures exercise only frontend behavior, not backend execution.
async function runtimeFixture(
  page: Page,
  options: {
    saved?: (typeof oldPair)[];
    executionQuery?: (route: Route, pair: typeof oldPair) => Promise<void>;
    executionPost?: (route: Route, pair: typeof oldPair) => Promise<void>;
  } = {},
) {
  await page.addInitScript(
    ({ key, saved }) => {
      sessionStorage.setItem('safeflow.runtime.task-mode.v1', 'draft');
      if (saved && !sessionStorage.getItem(key))
        sessionStorage.setItem(key, JSON.stringify({ version: 1, references: saved }));
    },
    { key: journalKey, saved: options.saved },
  );
  const state = {
    posts: [] as { taskId: string; body: Record<string, unknown> }[],
    confirmationGets: [] as string[],
    queryPairs: [] as (typeof oldPair)[],
    receipts: new Map<string, ReturnType<typeof completed>>(),
    ownerVisible: true,
  };
  const views = new Map<string, ReturnType<typeof prepared>>();
  let taskCounter = 0;
  await page.route('**/api/**', async (route) => {
    const path = new URL(route.request().url()).pathname;
    const method = route.request().method();
    if (path === '/api/sim/session') {
      if (method === 'DELETE') return route.fulfill({ status: 204 });
      return reply(route, { mode: 'PROJECT_SIMULATION', authenticated: true });
    }
    if (path === '/api/sim/draft-confirmations' && method === 'POST') {
      taskCounter++;
      const view = prepared(`new_task_${taskCounter}`, `new_operation_${taskCounter}`);
      views.set(view.task_id, view);
      return reply(route, view);
    }
    const confirm = /^\/api\/operations\/([^/]+)\/confirm$/.exec(path);
    if (confirm) {
      const body = route.request().postDataJSON();
      const view = views.get(body.task_id)!;
      view.confirmation.status = 'CONFIRMED';
      view.confirmation.confirmed_at = new Date().toISOString();
      view.display_ticket = null;
      view.next_action = { kind: 'NONE' };
      return reply(route, view);
    }
    const execution = /^\/api\/sim\/draft-confirmations\/([^/]+)\/execute$/.exec(path);
    if (execution && method === 'POST') {
      const body = route.request().postDataJSON();
      const pair = { task_id: execution[1], operation_id: body.operation_id };
      state.posts.push({ taskId: pair.task_id, body });
      if (options.executionPost) return options.executionPost(route, pair);
      const receipt = completed(pair);
      state.receipts.set(pair.task_id, receipt);
      return reply(route, { ...receipt, replayed: false });
    }
    const query = /^\/api\/sim\/draft-executions\/([^/]+)\/operations\/([^/]+)$/.exec(path);
    if (query && method === 'GET') {
      const pair = { task_id: query[1], operation_id: query[2] };
      state.queryPairs.push(pair);
      if (!state.ownerVisible) return notFound(route, pair.task_id);
      if (options.executionQuery) return options.executionQuery(route, pair);
      const receipt = state.receipts.get(pair.task_id);
      return receipt ? reply(route, receipt) : notFound(route, pair.task_id);
    }
    const original = /^\/api\/sim\/draft-confirmations\/([^/]+)$/.exec(path);
    if (original && method === 'GET') {
      state.confirmationGets.push(original[1]);
      if (state.receipts.has(original[1]))
        return reply(
          route,
          {
            contract_version: 1,
            error: {
              reason_code: 'STALE_PLAN',
              message: 'Old confirmation unavailable.',
              task_id: original[1],
              next_action: 'QUERY_ORIGINAL_TASK',
            },
          },
          409,
        );
      const view = views.get(original[1]);
      return view ? reply(route, view) : notFound(route, original[1]);
    }
    return notFound(route, 'unknown_fixture_task');
  });
  return state;
}
async function createAndConfirm(page: Page) {
  await page.getByRole('button', { name: '新建草案请求', exact: true }).click();
  await page.getByRole('button', { name: '确认草案请求', exact: true }).click();
  await expect(page.getByRole('button', { name: '请求保存模拟草案', exact: true })).toBeEnabled();
}

test('confirmation does not save; one explicit POST produces independent history after reload', async ({
  page,
}, info) => {
  const state = await runtimeFixture(page);
  await page.goto('/runtime.html');
  await createAndConfirm(page);
  expect(state.posts).toHaveLength(0);
  await page.getByRole('button', { name: '请求保存模拟草案', exact: true }).click();
  await expect(page.getByText('模拟草案保存已记录', { exact: true })).toBeVisible();
  expect(state.posts).toEqual([
    {
      taskId: 'new_task_1',
      body: {
        scope: 'MOCK_DRAFT_EXECUTION_ONLY',
        operation_id: 'new_operation_1',
        confirmation_ref: 'confirmation_new_task_1',
        task_revision: 2,
        operation_revision: 1,
        plan_version: 1,
        facts_version: 7,
      },
    },
  ]);
  await expect(page.getByRole('button', { name: '请求保存模拟草案', exact: true })).toHaveCount(0);
  await page.reload();
  await expect(page.getByText('模拟草案保存已记录', { exact: true })).toBeVisible();
  await expect(page.getByText('当前草案未核验', { exact: true })).toBeVisible();
  expect(state.posts).toHaveLength(1);
  expect(state.confirmationGets).not.toContain('new_task_1');
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.screenshot({ path: info.outputPath('historical-draft-receipt.png'), fullPage: true });
});

test('lost POST and later NOT_FOUND remain query-only across refresh and task switches', async ({
  page,
}) => {
  const state = await runtimeFixture(page, {
    executionPost: async (route) => route.abort('failed'),
  });
  await page.goto('/runtime.html');
  await createAndConfirm(page);
  await page.getByRole('button', { name: '请求保存模拟草案', exact: true }).click();
  await expect(
    page.getByText('原操作历史回执尚未取得，仅可继续查询原号。', { exact: true }),
  ).toBeVisible();
  await expect(page.getByRole('button', { name: '请求保存模拟草案', exact: true })).toHaveCount(0);
  await page.reload();
  await expect(
    page.getByText('原操作历史回执尚未取得，仅可继续查询原号。', { exact: true }),
  ).toBeVisible();
  expect(state.posts).toHaveLength(1);
  await createAndConfirm(page);
  await expect(page.getByRole('button', { name: '请求保存模拟草案', exact: true })).toBeVisible();
  await page
    .getByLabel('已记录原操作', { exact: true })
    .selectOption(JSON.stringify(['new_task_1', 'new_operation_1']));
  await expect(
    page.getByText('原操作历史回执尚未取得，仅可继续查询原号。', { exact: true }),
  ).toBeVisible();
  await expect(page.getByRole('button', { name: '请求保存模拟草案', exact: true })).toHaveCount(0);
  expect(state.posts).toHaveLength(1);
});

test('late old history cannot clear a different newly prepared or confirmed task', async ({
  page,
}) => {
  let oldRoute: Route | undefined;
  let release: (() => void) | undefined;
  const barrier = new Promise<void>((resolve) => {
    release = resolve;
  });
  await runtimeFixture(page, {
    saved: [oldPair],
    executionQuery: async (route, pair) => {
      if (pair.task_id === oldPair.task_id) {
        oldRoute = route;
        await barrier;
        return reply(route, completed(oldPair));
      }
      return notFound(route, pair.task_id);
    },
  });
  await page.goto('/runtime.html');
  await expect.poll(() => !!oldRoute).toBe(true);
  await page.getByRole('button', { name: '新建草案请求', exact: true }).click();
  await expect(page.getByRole('button', { name: '确认草案请求', exact: true })).toBeEnabled();
  release!();
  await expect(page.getByText('模拟草案保存已记录', { exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: '确认草案请求', exact: true })).toBeEnabled();
  await page.getByRole('button', { name: '确认草案请求', exact: true }).click();
  await expect(page.getByRole('button', { name: '请求保存模拟草案', exact: true })).toBeEnabled();
  await expect(page.locator('#draft-execution-task-id')).toHaveValue('new_task_1');
});

test('logout clears receipt authority and a new owner cannot see old history', async ({ page }) => {
  const state = await runtimeFixture(page);
  await page.goto('/runtime.html');
  await createAndConfirm(page);
  await page.getByRole('button', { name: '请求保存模拟草案', exact: true }).click();
  await expect(page.getByText('模拟草案保存已记录', { exact: true })).toBeVisible();
  await page.getByRole('button', { name: '退出会话', exact: true }).click();
  await expect(page.getByText('模拟草案保存已记录', { exact: true })).toHaveCount(0);
  state.ownerVisible = false;
  await page.getByLabel('模拟登录口令', { exact: true }).fill('frontend-fixture-secret');
  await page.getByRole('button', { name: '登录', exact: true }).click();
  await expect(
    page.getByText('原操作历史回执尚未取得，仅可继续查询原号。', { exact: true }),
  ).toBeVisible();
  await expect(page.getByText('模拟草案保存已记录', { exact: true })).toHaveCount(0);
  await expect(page.getByRole('button', { name: '请求保存模拟草案', exact: true })).toHaveCount(0);
  const saved = await page.evaluate((key) => JSON.parse(sessionStorage.getItem(key)!), journalKey);
  expect(saved.references).toEqual([{ task_id: 'new_task_1', operation_id: 'new_operation_1' }]);
});

test('unretained attempt journal prevents the execution POST', async ({ page }) => {
  const state = await runtimeFixture(page);
  await page.addInitScript((key) => {
    const original = Storage.prototype.setItem;
    Storage.prototype.setItem = function (name, value) {
      if (name === key) return;
      original.call(this, name, value);
    };
  }, journalKey);
  await page.goto('/runtime.html');
  await createAndConfirm(page);
  await page.getByRole('button', { name: '请求保存模拟草案', exact: true }).click();
  await expect(
    page.getByText('本页无法保存尝试记录，保存请求未发送。', { exact: true }),
  ).toBeVisible();
  expect(state.posts).toHaveLength(0);
  await expect(page.getByRole('button', { name: '请求保存模拟草案', exact: true })).toHaveCount(0);
});
