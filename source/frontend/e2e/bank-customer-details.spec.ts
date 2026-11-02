import { expect, test, type Page, type TestInfo } from '@playwright/test';
import { mkdir } from 'node:fs/promises';
import { resolve } from 'node:path';

const evidence = resolve('../artifacts/bank-demo-redesign-2026-10-19/revision');
const internalCopy =
  /demo_(?:card|tx|sub)_\w+|(?:demo_)?account_\d+|local_(?:draft|record)_[\w-]+|\b(?:Tool|Policy|Runtime)\b|前端|版本 v\d+|任务[：:]|操作[：:]|安全[：:]/;

async function checkView(page: Page, info: TestInfo, name: string) {
  const content = page.getByRole('dialog');
  const target = (await content.count()) ? content : page.locator('#main-content');
  await expect(target).not.toContainText(internalCopy);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  if (await content.count()) {
    expect(
      await content.evaluate((dialog) => {
        const bounds = dialog.getBoundingClientRect();
        return (
          bounds.left >= 0 && bounds.right <= innerWidth && dialog.scrollWidth <= dialog.clientWidth
        );
      }),
    ).toBe(true);
  }
  await mkdir(evidence, { recursive: true });
  await page.screenshot({
    path: resolve(evidence, `${info.project.name}-${name}.png`),
    fullPage: true,
  });
}

test('customer records and all detail dialogs hide internal fields and preserve local boundaries', async ({
  page,
}, info) => {
  const errors: string[] = [];
  const writes: string[] = [];
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('request', (request) => {
    if (request.method() !== 'GET' && new URL(request.url()).pathname.startsWith('/api/')) {
      writes.push(request.url());
    }
  });

  await page.goto('/#transfers');
  await page.getByRole('radio').first().check();
  await page.getByRole('textbox', { name: '转账金额', exact: true }).fill('200.00');
  await page.getByRole('button', { name: '保存并核对草案' }).click();
  await expect(page.getByRole('dialog')).toContainText('首次保存');
  await expect(page.getByRole('button', { name: '确认并转账 · 尚未接入' })).toBeDisabled();
  await checkView(page, info, 'draft-detail');
  await page.keyboard.press('Escape');
  const original = await page.evaluate(() => JSON.parse(localStorage.getItem('safeflow.demo.v1')!));

  await page.goto('/#activity');
  await expect(page.locator('.activity-list')).toContainText('草案已保存');
  await checkView(page, info, 'activity-list');
  await page.getByRole('button', { name: '查看保存转账草案记录' }).click();
  await expect(page.getByRole('dialog')).toContainText('仅保存本机草案，尚未转账');
  await expect(page.getByRole('dialog')).toContainText('本机演示');
  await checkView(page, info, 'activity-detail');
  await page.keyboard.press('Escape');

  await page.goto('/#cards');
  await page.getByRole('button', { name: '卡片详情', exact: true }).first().click();
  await expect(page.getByRole('dialog')).toContainText('6628');
  await checkView(page, info, 'card-detail');
  await page.getByRole('button', { name: '核对锁卡对象', exact: true }).click();
  await expect(page.getByRole('button', { name: '确认锁卡 · 暂未开通' })).toBeDisabled();
  await checkView(page, info, 'card-lock');
  await page.keyboard.press('Escape');

  await page.goto('/#bills');
  await page.getByRole('button', { name: '查看盒马鲜生流水详情' }).click();
  await expect(page.getByRole('dialog')).toContainText('演示流水');
  await checkView(page, info, 'bill-detail');
  await page.keyboard.press('Escape');
  const downloaded = page.waitForEvent('download');
  await page.getByRole('button', { name: '导出 CSV' }).click();
  const stream = await (await downloaded).createReadStream();
  let csv = '';
  for await (const chunk of stream!) csv += chunk.toString();
  expect(csv).toContain('序号');
  expect(csv).not.toContain('demo_tx_');

  await page.goto('/#subscriptions');
  await page.getByRole('button', { name: '查看网易云音乐订阅详情' }).click();
  await expect(page.getByRole('dialog')).toContainText('商户仍有效');
  await checkView(page, info, 'subscription-detail');
  await page.keyboard.press('Escape');

  // Available at all viewport widths without exposing the desktop-only sidebar button.
  await page.locator('.demo-badge').click();
  await expect(page.getByRole('dialog')).toContainText('每次保存后 30 分钟有效');
  await expect(page.getByRole('dialog')).toContainText('不会执行转账、锁卡或取消商户订阅');
  await checkView(page, info, 'about');
  await page.keyboard.press('Escape');
  const final = await page.evaluate(() => JSON.parse(localStorage.getItem('safeflow.demo.v1')!));
  // Viewing and exporting these details must not mutate or erase internal stored fields.
  expect(final.drafts).toEqual(original.drafts);
  expect(final.records).toEqual(original.records);
  expect(final.drafts[0].id).toMatch(/^local_draft_/);
  expect(final.drafts[0].version).toBe(1);
  expect(final.records[0]).toMatchObject({
    task: 'WAITING_USER',
    operation: 'DRAFT',
    security: 'READY_TO_CONFIRM',
  });
  expect(writes).toEqual([]);
  expect(errors).toEqual([]);
});

test('customer navigation hides internal pages; Runtime keeps its entry without a progress link', async ({
  page,
}, info) => {
  // A failed backend is represented as unavailable; this is not a simulated successful session.
  await page.route('**/api/**', (route) =>
    route.fulfill({ status: 503, contentType: 'application/json', body: '{}' }),
  );
  await page.goto('/#overview');
  await expect(page.locator('a[href="/runtime.html"], a[href="/progress.html"]')).toHaveCount(0);
  if (info.project.name === 'mobile') {
    await page.setViewportSize({ width: 360, height: 800 });
    expect(await page.locator('.app-main').evaluate((el) => el.getBoundingClientRect().left)).toBe(
      0,
    );
    expect(
      await page.locator('.mobile-brand').evaluate((el) => el.getBoundingClientRect().height),
    ).toBeLessThan(30);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(
      true,
    );
    await mkdir(evidence, { recursive: true });
    await page.screenshot({ path: resolve(evidence, 'mobile-home-360.png'), fullPage: true });
    await page.setViewportSize({ width: 390, height: 844 });
    await page.getByRole('button', { name: '打开导航' }).click();
    await expect(page.getByRole('dialog')).not.toContainText(/项目进度|底座任务|Runtime/);
    await page.keyboard.press('Escape');
    await expect(page.getByRole('button', { name: '打开导航' })).toBeFocused();
  }
  await page.goto('/runtime.html');
  await expect(page.locator('.r-navigation a[href="/runtime.html"]')).toBeVisible();
  await expect(page.locator('.r-navigation a[href="/progress.html"]')).toHaveCount(0);
  await expect(page.locator('.r-navigation')).not.toContainText('项目进度');
  await mkdir(evidence, { recursive: true });
  await page.screenshot({
    path: resolve(evidence, `${info.project.name}-runtime-navigation.png`),
    fullPage: true,
  });
  await page.unroute('**/api/**');
  await page.goto('/progress.html');
  await expect(page.locator('body')).toContainText('项目进度');
});
