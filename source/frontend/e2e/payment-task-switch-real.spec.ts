import { expect, test, type Page } from '@playwright/test';
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { recoveryKey } from '../src/runtime/payment-recovery';

const secret = process.env.SAFEFLOW_AGENT_E2E_SECRET;
const evidence = process.env.SAFEFLOW_WEB_EVIDENCE;
const markerKey = 'safeflow.agent.payment-attempt.v1';
async function sample(page: Page, value: string) {
  await page.getByLabel('选择办理事项', { exact: true }).selectOption(value);
  await page.getByRole('button', { name: '办理所选事项', exact: true }).click();
}
async function confirmed(page: Page) {
  await sample(page, 'CREATE_200');
  await page.getByRole('button', { name: '确认保存草案', exact: true }).click();
  await expect(page.locator('.a-receipt')).toContainText('草案已保存');
  await sample(page, 'EXECUTE_TRANSFER');
  await page.getByRole('button', { name: '确认付款信息', exact: true }).click();
  await expect(page.getByRole('button', { name: '确认付款', exact: true })).toBeEnabled();
  return page.getByLabel('办理编号', { exact: true }).inputValue();
}
test.describe('isolated real HTTP/PG task-switch payment recovery', () => {
  test.skip(!secret, 'Requires the explicitly started isolated payment browser harness');
  test('A response loss, B reads and reload retain A; original verification releases normal explicit B payment', async ({
    page,
  }, info) => {
    await page.goto('/#transfers');
    await page.getByRole('button', { name: '在线模拟付款', exact: true }).click();
    await page.getByLabel('模拟登录口令').fill(secret!);
    await page.getByRole('button', { name: '登录', exact: true }).click();
    await expect(page.locator('.a-model-status')).toContainText('固定办理选项');
    await page.getByLabel('付款账户编号', { exact: true }).fill('account_001');
    await page.getByLabel('收款人', { exact: true }).fill('小李');
    await page.getByRole('button', { name: '查找收款人', exact: true }).click();
    await page.getByLabel('选择收款人').selectOption('payee_008');
    const taskB = await confirmed(page);
    await page.getByRole('button', { name: '新任务', exact: true }).click();
    const taskA = await confirmed(page);
    const localBefore = await page.evaluate(() => JSON.stringify({ ...localStorage }));
    const posts: { task_id: string; state: string; amount: string; status: number }[] = [];
    // Every POST actually reaches the isolated HTTP/PG service. Only A's
    // response is discarded; no route invents a success or business result.
    await page.route('**/api/sim/payment-preparations/*/*/execute', async (route) => {
      const actual = await route.fetch();
      const body = await actual.json();
      posts.push({
        task_id: body.task_id,
        state: body.state,
        amount: body.transfer.amount,
        status: actual.status(),
      });
      if (body.task_id === taskA) await route.abort('failed');
      else await route.fulfill({ response: actual });
    });
    await page.getByRole('button', { name: '确认付款', exact: true }).click();
    await expect(page.getByRole('button', { name: '查询付款原单', exact: true })).toBeEnabled();
    await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeDisabled();
    const marker = await page.evaluate((key) => sessionStorage.getItem(key), markerKey);
    expect(JSON.parse(marker!).task_id).toBe(taskA);
    await page.getByLabel('办理编号', { exact: true }).fill(taskB);
    await page.getByRole('button', { name: '查询原任务', exact: true }).click();
    await expect(page.getByRole('button', { name: '确认付款', exact: true })).toBeDisabled();
    await page.getByRole('button', { name: '确认付款', exact: true }).evaluate((b) => {
      b.removeAttribute('disabled');
      b.dispatchEvent(new MouseEvent('click', { bubbles: true }));
      b.dispatchEvent(new MouseEvent('click', { bubbles: true }));
    });
    await page.getByRole('button', { name: '查询付款原单', exact: true }).click();
    await expect(page.locator('.a-payment-result .a-original')).toContainText(
      '当前快照未发现提交记录',
    );
    await page.getByRole('button', { name: '刷新付款状态', exact: true }).click();
    await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeDisabled();
    expect(await page.evaluate((key) => sessionStorage.getItem(key), markerKey)).toBe(marker);
    expect(posts.map((p) => p.task_id)).toEqual([taskA]);
    await page.reload();
    await expect(page.getByRole('button', { name: '确认付款', exact: true })).toBeDisabled();
    const recovery = page.getByRole('region', { name: '付款恢复' });
    await expect(recovery.getByRole('button')).toBeEnabled();
    expect(await page.getByLabel('办理编号', { exact: true }).inputValue()).toBe(taskB);
    expect(await page.evaluate((key) => sessionStorage.getItem(key), markerKey)).toBe(marker);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(
      true,
    );
    if (evidence)
      await page.screenshot({
        path: join(evidence, `real-switch-${info.project.name}-protected.png`),
        fullPage: true,
      });
    await recovery.getByRole('button').click();
    await expect(recovery).toContainText('原单成功（已核验）');
    await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeEnabled();
    expect(posts.map((p) => p.task_id)).toEqual([taskA]);
    expect(await page.evaluate((key) => sessionStorage.getItem(key), markerKey)).toBe(marker);
    // Only after A is independently verified can B be explicitly paid. Its
    // synchronous double click must still create just one additional POST.
    await expect(page.getByRole('button', { name: '确认付款', exact: true })).toBeEnabled();
    await page.getByRole('button', { name: '确认付款', exact: true }).evaluate((b) => {
      (b as HTMLButtonElement).click();
      (b as HTMLButtonElement).click();
    });
    await expect(page.locator('.a-payment-result .a-original')).toContainText('原单成功（已核验）');
    expect(posts.map((p) => p.task_id)).toEqual([taskA, taskB]);
    expect(
      posts.every((p) => p.status === 200 && p.state === 'SUCCEEDED' && p.amount === '200.00'),
    ).toBe(true);
    // The legacy key is a compatibility pointer, while v2 must retain both
    // complete selectors. A terminal B must never replace the entire collection.
    const savedReferences = await page.evaluate(
      (k) => JSON.parse(sessionStorage.getItem(k)!).references,
      recoveryKey,
    );
    expect(savedReferences.map((r: { task_id: string }) => r.task_id)).toEqual([taskA, taskB]);
    expect(savedReferences[0]).toEqual(JSON.parse(marker!));
    expect(await page.evaluate(() => JSON.stringify({ ...localStorage }))).toBe(localBefore);
    await page.reload();
    await expect(page.locator('.a-payment-result .a-original')).toContainText('原单成功（已核验）');
    // Reload restores both selectors without trusting old in-memory terminals.
    // B's own terminal must not silently clear the other preserved original A.
    const restored = await page.evaluate(
      (k) => JSON.parse(sessionStorage.getItem(k)!).references,
      recoveryKey,
    );
    expect(restored.map((r: { task_id: string }) => r.task_id)).toEqual([taskA, taskB]);
    await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeDisabled();
    await page.getByRole('button', { name: '查询付款原单', exact: true }).click();
    await expect(page.getByRole('button', { name: '查询付款原单', exact: true })).toBeEnabled();
    await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeDisabled();
    await page.getByRole('button', { name: '查询保留的付款原单', exact: true }).click();
    await expect(page.getByRole('button', { name: '新任务', exact: true })).toBeEnabled();
    expect(posts).toHaveLength(2);
    if (info.project.name === 'mobile') await page.setViewportSize({ width: 360, height: 800 });
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(
      true,
    );
    if (evidence) {
      await page.screenshot({
        path: join(evidence, `real-switch-${info.project.name}-verified.png`),
        fullPage: true,
      });
      writeFileSync(
        join(evidence, `real-switch-${info.project.name}.json`),
        JSON.stringify(
          {
            isolatedRealHttpPg: true,
            taskA,
            taskB,
            protectedMarker: JSON.parse(marker!),
            beforeAVerificationPosts: [taskA],
            afterAVerificationExplicitPosts: posts,
            localStorageUnchanged: true,
            reloadedRecoveryVerified: true,
            allReferencesRestoredIndependently: true,
          },
          null,
          2,
        ) + '\n',
      );
    }
  });
});
