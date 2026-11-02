import { expect, test, type Page } from '@playwright/test';
import { join } from 'node:path';
const secret = process.env.SAFEFLOW_AGENT_E2E_SECRET;
const evidence = process.env.SAFEFLOW_WEB_EVIDENCE;
async function shot(page: Page, name: string) {
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await page.evaluate(() => scrollTo(0, 0));
  if (evidence) await page.screenshot({ path: join(evidence, name), fullPage: true });
}
async function sample(page: Page, value: string) {
  await page.getByLabel('选择办理事项', { exact: true }).selectOption(value);
  await page.getByRole('button', { name: '办理所选事项', exact: true }).click();
}
async function ready(page: Page) {
  await page.goto('/#transfers');
  await page.getByRole('button', { name: '在线模拟付款', exact: true }).click();
  await page.getByLabel('模拟登录口令').fill(secret!);
  await page.getByRole('button', { name: '登录', exact: true }).click();
  await expect(page.locator('.a-model-status')).toContainText('固定办理选项');
  await page.getByLabel('付款账户编号', { exact: true }).fill('account_001');
  await page.getByLabel('收款人', { exact: true }).fill('小李');
  await page.getByRole('button', { name: '查找收款人', exact: true }).click();
  await page.getByLabel('选择收款人').selectOption('payee_008');
  await sample(page, 'CREATE_200');
  await page.getByRole('button', { name: '确认保存草案', exact: true }).click();
  await expect(page.locator('.a-receipt')).toContainText('草案已保存');
  await sample(page, 'EXECUTE_TRANSFER');
  await expect(page.getByRole('button', { name: '确认付款信息', exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: '确认付款', exact: true })).toHaveCount(0);
  await page.getByRole('button', { name: '确认付款信息', exact: true }).click();
  await expect(page.getByRole('button', { name: '确认付款', exact: true })).toBeEnabled();
}
test.describe('isolated real HTTP/PG customer payment; never shared catalog adoption', () => {
  test.skip(!secret, 'Requires explicitly started isolated payment browser harness');
  test('separate explicit payment, synchronous double click, actual original and reload; local data unchanged', async ({
    page,
  }, info) => {
    const errors: string[] = [];
    page.on('pageerror', (e) => errors.push(e.message));
    let posts = 0;
    page.on('request', (r) => {
      if (r.method() === 'POST' && /payment-preparations\/.*\/execute$/.test(r.url())) posts++;
    });
    await ready(page);
    expect(posts).toBe(0);
    const local = await page.evaluate(() => JSON.stringify({ ...localStorage }));
    await shot(page, `real-${info.project.name}-confirmed.png`);
    await page.getByRole('button', { name: '确认付款', exact: true }).evaluate((b) => {
      (b as HTMLButtonElement).click();
      (b as HTMLButtonElement).click();
    });
    await expect(page.locator('.a-original')).toContainText('原单成功（已核验）');
    await expect(page.locator('.a-original')).toContainText('200.00 CNY');
    expect(posts).toBe(1);
    await expect(page.getByRole('button', { name: '确认付款', exact: true })).toHaveCount(0);
    await expect(page.getByRole('button', { name: '确认付款信息', exact: true })).toHaveCount(0);
    await expect(page.locator('.r-uncertain')).toHaveCount(0);
    expect(await page.evaluate(() => JSON.stringify({ ...localStorage }))).toBe(local);
    await shot(page, `real-${info.project.name}-succeeded.png`);
    await page.reload();
    await expect(page.locator('.a-original')).toContainText('原单成功（已核验）');
    await page.getByRole('button', { name: '查询付款原单', exact: true }).click();
    await expect(page.locator('.a-original')).toContainText('200.00 CNY');
    expect(posts).toBe(1);
    expect(await page.locator('.bank-payment').innerText()).not.toMatch(
      /Runtime|MockLlm|Tool|Policy|Gateway|计划版本|底座任务/,
    );
    await shot(page, `real-${info.project.name}-reloaded.png`);
    expect(errors).toEqual([]);
    if (info.project.name === 'mobile') {
      await page.setViewportSize({ width: 360, height: 800 });
      await shot(page, 'real-mobile-360.png');
    }
  });
  test('actual commit with discarded HTTP response becomes original-only recovery, including reload', async ({
    page,
  }, info) => {
    await ready(page);
    let posts = 0;
    await page.route('**/api/sim/payment-preparations/*/*/execute', async (route) => {
      posts++;
      const actual = await route.fetch();
      expect(actual.status()).toBe(200);
      expect((await actual.json()).state).toBe('SUCCEEDED');
      await route.abort('failed');
    });
    await page.getByRole('button', { name: '确认付款', exact: true }).click();
    await expect(page.getByRole('button', { name: '查询付款原单', exact: true })).toBeEnabled();
    await expect(page.getByRole('button', { name: '确认付款', exact: true })).toHaveCount(0);
    await expect(page.getByRole('button', { name: '确认付款信息', exact: true })).toHaveCount(0);
    await expect(page.getByRole('button', { name: '办理所选事项', exact: true })).toBeDisabled();
    await shot(page, `lost-response-${info.project.name}-unknown.png`);
    await page.getByRole('button', { name: '查询付款原单', exact: true }).click();
    await expect(page.locator('.a-original')).toContainText('原单成功（已核验）');
    await page.reload();
    await expect(page.locator('.a-original')).toContainText('原单成功（已核验）');
    expect(posts).toBe(1);
    await expect(page.getByRole('button', { name: '确认付款', exact: true })).toHaveCount(0);
    await shot(page, `lost-response-${info.project.name}-recovered.png`);
  });
});
