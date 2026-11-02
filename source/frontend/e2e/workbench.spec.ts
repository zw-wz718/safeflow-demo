import { expect, test } from '@playwright/test';
import type { Page } from '@playwright/test';

async function noOverflow(page: Page) {
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(
    true,
  );
}
test('all seven views load and are framed at the viewport', async ({ page }, info) => {
  const errors: string[] = [];
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('response', (response) => {
    if (response.status() >= 400) errors.push(`${response.status()} ${response.url()}`);
  });
  for (const [route, heading] of [
    ['overview', 'Echo'],
    ['agent', '智能助手'],
    ['transfers', '转账汇款'],
    ['bills', '我的账单'],
    ['cards', '银行卡'],
    ['subscriptions', '订阅管理'],
    ['activity', '办理记录'],
  ]) {
    await page.goto(`/#${route}`);
    await expect(page.locator('h1')).toContainText(heading);
    await noOverflow(page);
    await page.screenshot({ path: info.outputPath(`${route}.png`), fullPage: true });
  }
  expect(errors).toEqual([]);
  if (info.project.name === 'mobile') {
    await page.getByRole('button', { name: '打开导航' }).click();
    await page.getByRole('dialog').getByRole('button', { name: '我的账单' }).click();
  } else {
    await page.getByRole('navigation').getByRole('button', { name: '我的账单' }).click();
  }
  await expect(page.locator('h1')).toHaveText('我的账单');
  await page.reload();
  await expect(page.locator('h1')).toHaveText('我的账单');
});

test('draft validation, disambiguation, edit, reload, cancel and explicit execution boundary', async ({
  page,
}) => {
  await page.goto('/#transfers');
  await page.getByRole('button', { name: '保存并核对草案' }).click();
  await expect(page.getByRole('alert')).toContainText('收款人');
  await page.getByRole('radio').nth(0).check();
  await page.getByRole('textbox', { name: '转账金额', exact: true }).fill('2.999');
  await page.getByRole('button', { name: '保存并核对草案' }).click();
  await expect(page.getByRole('alert')).toContainText('两位小数');
  await page.getByRole('textbox', { name: '转账金额', exact: true }).fill('200.00');
  await page.getByRole('textbox', { name: '转账备注' }).fill('聚餐');
  await page.getByRole('button', { name: '保存并核对草案' }).click();
  let dialog = page.getByRole('dialog');
  await expect(dialog).toContainText('示例银行');
  await expect(dialog).toContainText('1024');
  await expect(dialog.getByRole('button', { name: '确认并转账 · 尚未接入' })).toBeDisabled();
  await dialog.getByRole('button', { name: '修改草案', exact: true }).click();
  await page.getByRole('textbox', { name: '转账金额', exact: true }).fill('300.00');
  await page.getByRole('button', { name: '保存修改' }).click();
  await expect(page.getByRole('dialog')).toContainText('第1次修改');
  await page.keyboard.press('Escape');
  await expect(page.getByRole('dialog')).not.toBeVisible();
  await page.reload();
  await page.getByRole('button', { name: '查看小李 300.00元草案 · 第1次修改' }).click();
  dialog = page.getByRole('dialog');
  await dialog.getByRole('button', { name: '取消草案', exact: true }).click();
  await dialog.getByRole('button', { name: '取消这份草案' }).click();
  await page.getByRole('button', { name: '查看小李 300.00元草案 · 第1次修改' }).click();
  await expect(
    page.getByRole('dialog').getByRole('button', { name: '修改草案', exact: true }),
  ).toBeDisabled();
  await page.keyboard.press('Escape');
  await page.goto('/#activity');
  await expect(page.getByRole('button', { name: '查看取消本地草案记录' })).toBeVisible();
  await noOverflow(page);
});

test('ledger filters, CSV, feedback, subscription preferences and lock boundary', async ({
  page,
}) => {
  await page.goto('/#bills');
  await page.getByRole('button', { name: '转账', exact: true }).click();
  await expect(page.locator('.metric-value').nth(0)).toHaveText('¥0.00');
  await expect(page.locator('.transaction-row')).toHaveCount(1);
  const downloadPromise = page.waitForEvent('download');
  await page.getByRole('button', { name: '导出 CSV' }).click();
  const download = await downloadPromise;
  expect(download.suggestedFilename()).toContain('SafeFlow_样例账单');
  await page.getByRole('button', { name: '全部', exact: true }).click();
  await page.getByRole('textbox', { name: '搜索流水' }).fill('不存在');
  await expect(page.getByText('没有匹配的结果')).toBeVisible();
  await page.getByRole('textbox', { name: '搜索流水' }).fill('盒马');
  await page.getByRole('button', { name: '查看盒马鲜生流水详情' }).click();
  await page.getByRole('dialog').getByRole('button', { name: '反馈异常' }).click();
  await page.getByPlaceholder('请描述这笔样例流水的问题').fill('非本人消费');
  await page.getByRole('dialog').getByRole('button', { name: '登记本地反馈' }).click();
  await expect(page.getByRole('dialog')).toContainText('未触发锁卡');
  await page.keyboard.press('Escape');
  await page.goto('/#subscriptions');
  await page.getByRole('switch', { name: '网易云音乐订阅提醒' }).uncheck();
  await page.getByRole('button', { name: '查看网易云音乐订阅详情' }).click();
  await page.getByRole('dialog').getByRole('button', { name: '登记取消意图' }).click();
  await page.getByRole('dialog').getByRole('button', { name: '登记取消意图' }).click();
  await expect(page.getByRole('dialog')).toContainText('商户仍有效');
  await page.keyboard.press('Escape');
  await page.reload();
  await expect(page.getByRole('switch', { name: '网易云音乐订阅提醒' })).not.toBeChecked();
  await page.goto('/#cards');
  await page.getByRole('button', { name: '锁卡核对' }).first().click();
  await expect(
    page.getByRole('dialog').getByRole('button', { name: '确认锁卡 · 暂未开通' }),
  ).toBeDisabled();
  await page.keyboard.press('Escape');
  await expect(page.getByRole('button', { name: '锁卡核对' }).first()).toBeFocused();
});

test('finite agent intent and local storage recovery', async ({ page }) => {
  await page.goto('/#agent');
  await page.getByRole('textbox', { name: '对话输入' }).fill('给小李转 200 元');
  await page.getByRole('button', { name: '发送消息', exact: true }).click();
  await expect(page.getByText('有 2 位同名收款人', { exact: false })).toBeVisible();
  await page.getByRole('button', { name: '选择收款人并核对草案' }).click();
  await expect(page.getByRole('textbox', { name: '转账金额', exact: true })).toHaveValue('200.00');
  await expect(page.getByRole('radio')).toHaveCount(2);
  await expect(page.getByRole('radio').first()).not.toBeChecked();
  await page.goto('/#agent');
  await page.getByRole('textbox', { name: '对话输入' }).fill('<img src=x onerror=alert(1)>');
  await page.getByRole('button', { name: '发送消息', exact: true }).click();
  await expect(page.locator('.message.user').last()).toContainText('<img src=x onerror=alert(1)>');
  expect(await page.locator('.message img').count()).toBe(0);
  await page.evaluate(() => localStorage.setItem('safeflow.demo.v1', '{bad'));
  await page.reload();
  await expect(page.getByRole('alert')).toContainText('已恢复为空白演示状态');
  await expect(page.locator('.message')).toHaveCount(0);
});

test('alternative same-name selection and expired draft remain safe after refresh', async ({
  page,
}) => {
  await page.goto('/#transfers');
  await page.getByRole('radio').nth(1).check();
  await page.getByRole('textbox', { name: '转账金额', exact: true }).fill('200.00');
  await page.getByRole('button', { name: '保存并核对草案' }).click();
  const dialog = page.getByRole('dialog');
  await expect(dialog).toContainText('第二示例银行');
  await expect(dialog).toContainText('7788');
  for (let i = 0; i < 8; i++) {
    await page.keyboard.press('Tab');
    expect(await dialog.evaluate((el) => el.contains(document.activeElement))).toBe(true);
  }
  await page.keyboard.press('Escape');
  await page.evaluate(() => {
    const state = JSON.parse(localStorage.getItem('safeflow.demo.v1')!);
    state.drafts[0].createdAt = new Date(Date.now() - 3600000).toISOString();
    state.drafts[0].updatedAt = state.drafts[0].createdAt;
    state.drafts[0].expiresAt = new Date(Date.now() - 1800000).toISOString();
    localStorage.setItem('safeflow.demo.v1', JSON.stringify(state));
  });
  await page.reload();
  await page.getByRole('button', { name: '查看小李 200.00元草案 · 首次保存' }).click();
  await expect(page.getByRole('dialog')).toContainText('已到期');
  await expect(
    page.getByRole('dialog').getByRole('button', { name: '修改草案', exact: true }),
  ).toBeDisabled();
  await noOverflow(page);
});
