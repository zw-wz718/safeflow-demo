import { expect, test, type Page } from '@playwright/test';
import { join } from 'node:path';
import candidate from '../src/runtime/billing-duplicate-candidate.fixture.json' with { type: 'json' };
import missing from '../src/runtime/billing-duplicate-missing-merchant.fixture.json' with { type: 'json' };
import unavailable from '../src/runtime/billing-duplicate-source-unavailable.fixture.json' with { type: 'json' };
import limit from '../src/runtime/billing-duplicate-pair-limit.fixture.json' with { type: 'json' };
import legacy from '../src/runtime/billing-mixed.fixture.json' with { type: 'json' };

// Isolated replay of actual Rust HTTP/PG captures. No new source is registered
// in the default server; no member evidence is constructed by this browser.
async function history(page: Page, raw: { task: { task_id: string } }) {
  await page.addInitScript(
    (id) => sessionStorage.setItem('safeflow.agent.last-task.v1', id),
    raw.task.task_id,
  );
  await page.route('**/api/health', (r) =>
    r.fulfill({ json: { status: 'READY', mode: 'PROJECT_SIMULATION' } }),
  );
  await page.route('**/api/sim/session', (r) =>
    r.fulfill({ json: { mode: 'PROJECT_SIMULATION', authenticated: true } }),
  );
  await page.route('**/api/agent/status', (r) =>
    r.fulfill({
      json: {
        contract_version: 1,
        mode: 'PROJECT_SIMULATION',
        configured: false,
        connectivity: 'NOT_VERIFIED',
        tools: [],
        draft_enabled: true,
        planning_mode: 'FIXED_PLAN',
        payment_planning_enabled: true,
        funds_execution_enabled: false,
      },
    }),
  );
  await page.route('**/api/agent/tasks/*', (r) => r.fulfill({ json: raw }));
  await page.goto('/runtime.html');
}
for (const [name, raw] of [
  ['candidate', candidate],
  ['missing-merchant', missing],
  ['source-unavailable', unavailable],
  ['pair-limit', limit],
  ['legacy-v1', legacy],
] as const) {
  test(`duplicate report history: ${name}`, async ({ page }, info) => {
    const writes: string[] = [];
    const errors: string[] = [];
    page.on('request', (r) => {
      if (r.method() === 'POST') writes.push(r.url());
    });
    page.on('pageerror', (e) => errors.push(e.message));
    await history(page, raw);
    const report = page.getByRole('region', { name: '本地账单报告' });
    const duplicates = page.getByRole('region', { name: '重复消费候选', exact: true });
    await expect(report).toBeVisible();
    await expect(page.locator('.a-model-status')).toContainText('账单报告来源未接入');
    if (name === 'legacy-v1') {
      await expect(duplicates).toHaveCount(0);
      await expect(report).toContainText('60.00');
      await expect(report).toContainText('退款 · 单独列示');
    } else {
      await expect(duplicates).toContainText('仅供核对');
      if (name === 'candidate') {
        await expect(duplicates).toContainText('1 组候选');
        await expect(duplicates).toContainText('food-1');
        await expect(duplicates).toContainText('food-2');
        await expect(duplicates).toContainText('fictional-report-actions');
        await expect(duplicates).toContainText(raw.results[0].data.report.metadata.report_id);
      } else {
        await expect(duplicates).not.toContainText('本规则未发现');
        await expect(duplicates).toContainText(
          name === 'missing-merchant'
            ? '商户证据不足'
            : name === 'pair-limit'
              ? '超过1000组上限'
              : 'REPORT_NOT_COMPLETE',
        );
        if (name === 'missing-merchant') await expect(duplicates).toContainText('food-1');
        if (name === 'source-unavailable') await expect(report).toContainText('暂不可用');
      }
      await expect(duplicates.getByRole('button')).toHaveCount(0);
    }
    await expect(report.getByRole('button')).toHaveCount(0);
    await page.getByRole('button', { name: '查询原任务', exact: true }).click();
    await expect(report).toBeVisible();
    await page.reload();
    await expect(report).toBeVisible();
    if (name !== 'legacy-v1') await expect(duplicates).toBeVisible();
    expect(writes).toEqual([]);
    expect(errors).toEqual([]);
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(
      true,
    );
    const evidence = process.env.SAFEFLOW_WEB_EVIDENCE;
    if (evidence)
      await page.screenshot({
        path: join(evidence, `billing-duplicate-${info.project.name}-${name}.png`),
        fullPage: true,
      });
  });
}
