import { test, expect } from '@playwright/test';
import { mkdir, access } from 'node:fs/promises';
import { resolve } from 'node:path';
const secret = 'isolated-http-test-secret-123456';
const panel = (page: import('@playwright/test').Page) =>
  page.getByRole('region', { name: '本人订阅只读查询' });
test('editing an original number invalidates its delayed result', async ({ page }) => {
  await page.addInitScript(() => {
    const original = window.fetch.bind(window);
    window.fetch = (input, init) =>
      original(
        input,
        init?.method === 'POST' && String(init.body).includes('"kind":"ORIGINAL"')
          ? { ...init, signal: undefined }
          : init,
      );
  });
  await signIn(page);
  const p = panel(page);
  await p.getByRole('button', { name: '查询 / 刷新订阅' }).click();
  await expect(p.getByRole('button', { name: '核对这笔原结果' })).toBeVisible();
  let release: () => void = () => {},
    arrived: () => void = () => {};
  const started = new Promise<void>((r) => (arrived = r)),
    delay = new Promise<void>((r) => (release = r));
  let first = true;
  await page.route('**/api/subscriptions', async (route) => {
    if (!first || route.request().postDataJSON()?.kind !== 'ORIGINAL') return route.continue();
    first = false;
    arrived();
    await delay;
    await route
      .fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          fictional: true,
          status: 'NEEDS_REVIEW',
          message: '迟到旧原结果不得展示',
        }),
      })
      .catch(() => {});
  });
  await p.getByRole('button', { name: '核对这笔原结果' }).click();
  await started;
  await p.getByLabel('原操作编号', { exact: true }).fill('f297fd19-3949-416e-a3de-888b180560cf');
  await p.getByRole('button', { name: '查询原操作结果' }).click();
  await expect(p).toContainText('暂时无法核实订阅资料或原结果');
  release();
  await expect(p.getByTestId('subscription-original-result')).toHaveCount(0);
  await expect(p).not.toContainText('迟到旧原结果不得展示');
});
async function signIn(page: import('@playwright/test').Page) {
  await page.goto('/#subscriptions');
  const p = panel(page);
  await p.getByLabel('模拟登录口令').fill(secret);
  await p.getByRole('button', { name: '登录', exact: true }).click();
  await expect(p).toContainText('模拟会话已登录');
}
async function navigate(page: import('@playwright/test').Page, name: string) {
  const target = page.getByRole('button', { name, exact: true }).filter({ visible: true });
  if (!(await target.isVisible())) await page.getByRole('button', { name: '打开导航' }).click();
  await target.click();
}
test('real HTTP PG owner read stays separate from samples and refuses writes', async ({
  page,
}, info) => {
  const errors: string[] = [];
  page.on('pageerror', (e) => errors.push(e.message));
  await signIn(page);
  const p = panel(page);
  await p.getByRole('button', { name: '查询 / 刷新订阅' }).click();
  await expect(p.getByTestId('subscription-read-result')).toContainText('订阅记录 1');
  await expect(p).not.toContainText(
    /user_demo|fictional-subscription|session_ref|Journal|original_binding|original_request|permit/i,
  );
  await expect(page.locator('#main-content')).toContainText('以下为本机样例');
  await p.getByRole('button', { name: '查看订阅记录 1 的详情' }).click();
  const detail = p.getByTestId('subscription-detail-result');
  await expect(detail).toContainText('本地取消状态与历史');
  await expect(detail).toContainText('提醒计划与本地记录');
  await expect(detail).toContainText('未记录本地取消或撤回历史');
  await expect(detail).not.toContainText(/subscription_id|user_demo|anchor_date|cycle_ref/);
  await p.getByRole('button', { name: '核对这笔原结果' }).click();
  await expect(p.getByTestId('subscription-original-result')).toContainText('原请求的结果仍待核对');
  await expect(p.getByTestId('subscription-original-result')).toContainText('不重新执行操作');
  // A fresh list invalidates the old choice, without doing any business write.
  expect(
    await page.evaluate(
      async () =>
        (
          await fetch('/api/subscriptions', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json', 'X-SafeFlow-Request': '1' },
            body: JSON.stringify({ kind: 'LIST' }),
          })
        ).status,
    ),
  ).toBe(200);
  await p.getByRole('button', { name: '查看订阅记录 1 的详情' }).click();
  await expect(p).toContainText('请重新查询列表后选择');
  await expect(p.getByTestId('subscription-detail-result')).toHaveCount(0);
  await p.getByRole('button', { name: '查询 / 刷新订阅' }).click();
  await p.getByRole('button', { name: '查看订阅记录 1 的详情' }).click();
  await expect(p.getByTestId('subscription-detail-result')).toContainText('本地取消状态与历史');
  const statuses = await page.evaluate(async () => {
    const statuses = [];
    for (const kind of [
      'RECORD_CANCELLATION',
      'REVOKE_CANCELLATION',
      'REMINDER_SETTINGS',
      'REGISTRATION',
    ]) {
      const response = await fetch('/api/subscriptions/change', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', 'X-SafeFlow-Request': '1' },
        body: JSON.stringify({ kind }),
      });
      statuses.push(response.status);
    }
    return statuses;
  });
  expect(statuses).toEqual([503, 503, 503, 503]);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  const root = process.env.SAFEFLOW_SUBSCRIPTION_EVIDENCE;
  if (!root) throw Error('New task evidence directory required');
  await mkdir(root, { recursive: true });
  const file = resolve(root, `${info.project.name}-owner-read.png`);
  await expect(access(file)).rejects.toThrow();
  await page.screenshot({ path: file, fullPage: true });
  await p.getByRole('button', { name: '退出会话' }).click();
  await expect(p.getByTestId('subscription-read-result')).toHaveCount(0);
  await expect(p.getByRole('button', { name: '查询 / 刷新订阅' })).toBeDisabled();
  expect(errors).toEqual([]);
});
test('late failed reads cannot erase a newer result after clear or StrictMode remount', async ({
  page,
}) => {
  await page.addInitScript(() => {
    const original = window.fetch.bind(window);
    let first = true;
    window.fetch = (input, init) => {
      if (first && String(input).endsWith('/api/subscriptions') && init?.method === 'POST') {
        first = false;
        return original(input, { ...init, signal: undefined });
      }
      return original(input, init);
    };
  });
  await signIn(page);
  let release: () => void = () => {};
  let arrived: () => void = () => {};
  const started = new Promise<void>((r) => (arrived = r));
  const delay = new Promise<void>((r) => (release = r));
  let first = true;
  await page.route('**/api/subscriptions', async (route) => {
    if (!first) return route.continue();
    first = false;
    arrived();
    await delay;
    await route
      .fulfill({
        status: 503,
        contentType: 'application/json',
        body: JSON.stringify({
          error: {
            contract_version: 1,
            reason_code: 'UNAVAILABLE',
            task_id: null,
            next_action: 'NONE',
            funds_authorized: false,
          },
        }),
      })
      .catch(() => {});
  });
  const p = panel(page);
  await p.getByRole('button', { name: '查询 / 刷新订阅' }).click();
  await started;
  await p.getByRole('button', { name: '清除结果' }).click();
  await p.getByRole('button', { name: '查询 / 刷新订阅' }).click();
  await expect(p.getByTestId('subscription-read-result')).toContainText('订阅记录 1');
  release();
  await expect(p.getByTestId('subscription-read-result')).toContainText('订阅记录 1');
  await expect(p).not.toContainText('暂时无法核实');
  await navigate(page, '银行卡');
  await navigate(page, '订阅管理');
  await expect(panel(page).getByTestId('subscription-read-result')).toHaveCount(0);
  await expect(panel(page)).toContainText('模拟会话已登录');
  await panel(page).getByRole('button', { name: '查询 / 刷新订阅' }).click();
  await expect(panel(page).getByTestId('subscription-read-result')).toContainText('订阅记录 1');
});
