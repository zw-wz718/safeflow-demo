import { expect, test } from '@playwright/test';
import { join } from 'node:path';

const secret = process.env.SAFEFLOW_AGENT_E2E_SECRET;
test.skip(
  !secret,
  'Requires actual Rust API/PG and explicitly configured protocol fixture or provider',
);

test('Agent complete read and confirmed draft lifecycle', async ({ page }, info) => {
  const errors: string[] = [];
  page.on('pageerror', (e) => errors.push(e.message));
  await page.goto('/runtime.html');
  await page.getByRole('button', { name: 'Agent 助手', exact: true }).click();
  await page.getByLabel('模拟登录口令').fill(secret!);
  await page.getByRole('button', { name: '登录', exact: true }).click();
  await expect(page.getByText('模型已配置', { exact: true })).toBeVisible();
  await page.getByLabel('账户编号', { exact: true }).fill('account_001');
  await page.getByLabel('任务请求', { exact: true }).fill('分两次查询余额');
  await page.getByRole('button', { name: '发送请求', exact: true }).click();
  await expect(page.getByText('1288.00', { exact: false }).first()).toBeVisible();
  await page.getByRole('button', { name: '继续', exact: true }).click();
  await expect(page.locator('.a-read-result .r-state')).toContainText('已完成');
  await page.getByRole('button', { name: '新任务', exact: true }).click();
  await page.getByLabel('收款人', { exact: true }).fill('小李');
  await page.getByRole('button', { name: '查找收款人', exact: true }).click();
  await page.getByLabel('选择收款人').selectOption('payee_008');
  await page.getByLabel('任务请求', { exact: true }).fill('给选择的收款人生成200元草案');
  await page.getByRole('button', { name: '发送请求', exact: true }).click();
  await expect(page.getByRole('button', { name: '确认保存草案', exact: true })).toBeEnabled();
  await expect(page.locator('.a-draft-fields')).toContainText('200.00');
  const screenshots = process.env.SAFEFLOW_AGENT_SCREENSHOTS;
  if (screenshots)
    await page.screenshot({
      path: join(screenshots, `agent-${info.project.name}-confirmation.png`),
      fullPage: true,
    });
  const originalId = await page.getByLabel('任务号', { exact: true }).inputValue();
  await page.getByRole('button', { name: '确认保存草案', exact: true }).click();
  await expect(page.locator('.a-receipt')).toContainText('草案已保存');
  await page.getByLabel('下一步请求', { exact: true }).fill('修改草案金额，先澄清');
  await page.getByRole('button', { name: '发送请求', exact: true }).click();
  await expect(page.locator('.a-clarification')).toBeVisible();
  await expect(page.getByLabel('任务号', { exact: true })).toHaveValue(originalId);
  await page.getByLabel('任务请求', { exact: true }).fill('改为300元');
  await page.getByRole('button', { name: '发送请求', exact: true }).click();
  await expect(page.getByRole('button', { name: '确认修改草案', exact: true })).toBeEnabled();
  await expect(page.locator('.a-draft-fields')).toContainText('300.00');
  await page.getByRole('button', { name: '确认修改草案', exact: true }).click();
  await expect(page.locator('.a-receipt')).toContainText('草案已修改');
  await page.reload();
  await expect(page.locator('.a-receipt')).toContainText('草案已修改');
  await expect(page.getByLabel('任务号', { exact: true })).toHaveValue(originalId);
  await page.getByLabel('下一步请求', { exact: true }).fill('取消当前草案');
  await page.getByRole('button', { name: '发送请求', exact: true }).click();
  await page.getByRole('button', { name: '确认取消草案', exact: true }).click();
  await expect(page.locator('.a-receipt')).toContainText('草案已取消');
  await expect(page.locator('.a-draft-result .r-state')).toContainText('已取消');
  await expect(page.getByRole('button', { name: '发送请求', exact: true })).toBeDisabled();
  await expect(page.getByLabel('任务号', { exact: true })).toHaveValue(originalId);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(
    true,
  );
  expect(errors).toEqual([]);
  if (screenshots)
    await page.screenshot({
      path: join(screenshots, `agent-${info.project.name}-cancelled.png`),
      fullPage: true,
    });
});
