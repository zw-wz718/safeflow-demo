import { expect, test, type Page } from '@playwright/test';
import { readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';

const secret = process.env.SAFEFLOW_AGENT_E2E_SECRET;
const evidence = process.env.SAFEFLOW_FIXED_EVIDENCE;
const restarted = process.env.SAFEFLOW_AGENT_RESTART_PHASE === '1';
test.skip(!secret || !evidence, 'Requires actual API/PG in explicit fixed mode, no model provider');
async function login(page: Page) {
  await page.goto('/runtime.html');
  await page.getByLabel('模拟登录口令').fill(secret!);
  await page.getByRole('button', { name: '登录', exact: true }).click();
  await expect(page.locator('.a-model-status')).toContainText('固定计划 / MockLlm');
  await expect(page.locator('textarea')).toHaveCount(0);
}
async function sample(page: Page, value: string) {
  await page.getByLabel('固定计划样例', { exact: true }).selectOption(value);
  await page.getByRole('button', { name: '运行样例', exact: true }).click();
}
async function selectObjects(page: Page) {
  await page.getByLabel('账户编号', { exact: true }).fill('account_001');
  await page.getByLabel('收款人', { exact: true }).fill('小李');
  await page.getByRole('button', { name: '查找收款人', exact: true }).click();
  await page.getByLabel('选择收款人').selectOption('payee_008');
}
test('fixed samples actual read and confirmed same task lifecycle', async ({ page }, info) => {
  test.skip(restarted);
  const errors: string[] = [];
  page.on('pageerror', (e) => errors.push(e.message));
  await login(page);
  await page.getByLabel('账户编号', { exact: true }).fill('account_001');
  await sample(page, 'READ_WAIT');
  await expect(page.locator('.a-read-result')).toContainText('1288.00');
  await page.getByRole('button', { name: '继续', exact: true }).click();
  await expect(page.locator('.a-read-result .r-state')).toContainText('已完成');
  const readId = await page.getByLabel('任务号', { exact: true }).inputValue();
  await page.getByRole('button', { name: '新任务', exact: true }).click();
  await selectObjects(page);
  await sample(page, 'CREATE_200');
  await expect(page.locator('.a-draft-fields')).toContainText('200.00');
  await expect(page.getByRole('button', { name: '确认保存草案', exact: true })).toBeEnabled();
  await page.screenshot({
    path: join(evidence!, `fixed-${info.project.name}-confirmation.png`),
    fullPage: true,
  });
  const taskId = await page.getByLabel('任务号', { exact: true }).inputValue();
  await page.getByRole('button', { name: '确认保存草案', exact: true }).click();
  await expect(page.locator('.a-receipt')).toContainText('草案已保存');
  await sample(page, 'MODIFY_300');
  await expect(page.locator('.a-draft-fields')).toContainText('300.00');
  await page.getByRole('button', { name: '确认修改草案', exact: true }).click();
  await expect(page.locator('.a-receipt')).toContainText('草案已修改');
  await page.reload();
  await expect(page.locator('.a-receipt')).toContainText('草案已修改');
  await sample(page, 'CANCEL_DRAFT');
  await page.getByRole('button', { name: '确认取消草案', exact: true }).click();
  await expect(page.locator('.a-receipt')).toContainText('草案已取消');
  await expect(page.locator('.a-draft-result .r-state')).toContainText('已取消');
  await expect(page.getByLabel('任务号', { exact: true })).toHaveValue(taskId);
  await expect(page.getByRole('button', { name: '运行样例', exact: true })).toBeDisabled();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  expect(errors).toEqual([]);
  await page.screenshot({
    path: join(evidence!, `fixed-${info.project.name}-cancelled.png`),
    fullPage: true,
  });
  await page.getByRole('button', { name: '新任务', exact: true }).click();
  await selectObjects(page);
  await sample(page, 'CREATE_200');
  await page.getByRole('button', { name: '确认保存草案', exact: true }).click();
  await expect(page.locator('.a-receipt')).toContainText('草案已保存');
  const draftId = await page.getByLabel('任务号', { exact: true }).inputValue();
  await writeFile(
    join(evidence!, `fixed-${info.project.name}-restart.json`),
    JSON.stringify({ readId, draftId }),
  );
});
test('actual process restart keeps public history and refuses missing current Store', async ({
  page,
}, info) => {
  test.skip(!restarted);
  const refs = JSON.parse(
    await readFile(join(evidence!, `fixed-${info.project.name}-restart.json`), 'utf8'),
  );
  await login(page);
  await page.getByLabel('任务号', { exact: true }).fill(refs.readId);
  await page.getByRole('button', { name: '查询原任务', exact: true }).click();
  await expect(page.locator('.a-read-result .r-state')).toContainText('已完成');
  await expect(page.locator('.a-read-result .a-result')).toHaveCount(2);
  await page.getByLabel('任务号', { exact: true }).fill(refs.draftId);
  await page.getByRole('button', { name: '查询原任务', exact: true }).click();
  await expect(page.locator('.a-receipt')).toContainText('草案已保存');
  await expect(page.locator('.a-thread')).toContainText('规划来源：固定计划 / MockLlm');
  await sample(page, 'MODIFY_300');
  await expect(page.getByRole('alert')).toContainText('所需能力或数据暂不可用');
  await expect(page.getByRole('button', { name: '确认修改草案', exact: true })).toHaveCount(0);
  await expect(page.getByLabel('任务号', { exact: true })).toHaveValue(refs.draftId);
  await page.screenshot({
    path: join(evidence!, `fixed-${info.project.name}-restart.png`),
    fullPage: true,
  });
});
