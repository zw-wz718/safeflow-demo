import { expect, test } from '@playwright/test';
import type { Page } from '@playwright/test';
import { readFileSync } from 'node:fs';

const ledger = () =>
  JSON.parse(
    readFileSync(new URL('../../docs/progress/project-progress.json', import.meta.url), 'utf8'),
  );
async function synced(page: Page) {
  await page.goto('/progress.html');
  await expect(page.getByText('台账已同步', { exact: true })).toBeVisible();
}
async function noOverflow(page: Page) {
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(
    true,
  );
}

test('progress four views and long paths stay readable at each viewport', async ({
  page,
}, info) => {
  const errors: string[] = [];
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('response', (response) => {
    if (response.status() >= 400) errors.push(`${response.status()} ${response.url()}`);
  });
  await synced(page);
  await expect(page.locator('.p-member')).toHaveCount(5);
  await expect(page.locator('.p-stages')).toContainText('LLM 与页面');
  await expect(page.locator('.p-stages')).toContainText('计划与安全');
  await expect(page.locator('.p-member').filter({ hasText: '逐风' })).toContainText(
    '实现 3 · 验收 0',
  );
  await expect(page.locator('.p-update')).toBeVisible();
  for (const [label, name] of [
    ['总览', 'overview'],
    ['成员与阶段', 'members'],
    ['交付台账', 'deliveries'],
    ['待对接', 'dependencies'],
  ]) {
    await page
      .getByRole('navigation')
      .getByRole('button', { name: label, exact: label !== '待对接' })
      .click();
    await noOverflow(page);
    await page.screenshot({ path: info.outputPath(`${name}.png`), fullPage: true });
    if (name === 'overview')
      await page.screenshot({ path: info.outputPath('overview-viewport.png') });
  }
  await page.getByRole('navigation').getByRole('button', { name: '交付台账', exact: true }).click();
  await page.getByRole('button', { name: '查看 CARD-01 只读查询与模拟样例 交付' }).click();
  await expect(page.getByRole('dialog')).toContainText('a2a33b93911d0dc86fa87bc8a86055a2ee2dbda5');
  await expect(page.getByRole('dialog')).toContainText('P1 订阅未收到进度');
  expect(
    await page
      .getByRole('dialog')
      .evaluate((element) => element.scrollWidth <= element.clientWidth),
  ).toBe(true);
  await page.screenshot({ path: info.outputPath('card-detail.png') });
  await page.keyboard.press('Escape');
  await expect(page.getByRole('dialog')).not.toBeVisible();
  await page.reload();
  await expect(page.locator('h1')).toHaveText('交付台账');
  expect(errors).toEqual([]);
});

test('current review opens the matching delivery and its exact persistence-session evidence', async ({
  page,
  isMobile,
}, info) => {
  await synced(page);
  await noOverflow(page);
  await page.screenshot({ path: info.outputPath('current-overview.png'), fullPage: true });
  const data = ledger();
  const current = data.deliveries.find(
    (delivery: { state: string; reviewedAt: string; sourceIds: string[] }) =>
      delivery.state === 'accepted' &&
      delivery.reviewedAt === data.current.reviewedOn &&
      delivery.sourceIds.includes(data.current.sourceId),
  );
  expect(current.id).toBe('foundation-persistence-session-v1-review');
  const pending = data.deliveries.find(
    (delivery: { id: string }) => delivery.id === 'foundation-directory-ownership-v1',
  );
  expect(pending.state).toBe('pending-review');
  const accepted = data.deliveries.find(
    (delivery: { id: string }) => delivery.id === 'foundation-directory-ownership-v1-review',
  );
  expect(accepted.state).toBe('accepted');
  const latest = page.locator('.p-recent').first();
  await expect(latest.locator('.p-recent-title strong')).toHaveText(accepted.title);
  await expect(latest.locator('.p-badge')).toHaveText('限定范围验收通过');
  await expect(page.locator('.p-recent').nth(1).locator('.p-recent-title strong')).toHaveText(
    pending.title,
  );
  await expect(page.locator('.p-recent').nth(1).locator('.p-badge')).toHaveText('待验收');
  await expect(page.locator('.p-recent').nth(2).locator('.p-recent-title strong')).toHaveText(
    current.title,
  );
  await latest.locator('.p-recent-title').click();
  await expect(page.getByRole('dialog').getByRole('heading', { level: 2 })).toHaveText(
    accepted.title,
  );
  const toolReview = data.sources.find(
    (source: { id: string }) => source.id === 'directory-ownership-review',
  );
  await page.getByRole('button', { name: `读取${toolReview.title}`, exact: true }).click();
  await expect(page.locator('.p-evidence-content')).toContainText('BASE-01');
  await expect(page.locator('.p-evidence-content')).toContainText('13组独立检查');
  await page.screenshot({
    path: info.outputPath('accepted-directory-evidence.png'),
    fullPage: true,
  });
  await page.getByRole('button', { name: '关闭证据内容', exact: true }).click();
  await page.keyboard.press('Escape');
  if (isMobile) {
    await page
      .getByRole('navigation')
      .getByRole('button', { name: '交付台账', exact: true })
      .click();
    await page.getByRole('button', { name: `查看 ${current.title} 交付`, exact: true }).click();
  } else {
    await page.getByRole('button', { name: '查看验收范围', exact: true }).click();
  }
  await expect(page.getByRole('dialog').getByRole('heading', { level: 2 })).toHaveText(
    current.title,
  );
  await expect(page.getByRole('dialog')).toContainText(current.scope);
  for (const sourceId of ['persistence-session-review', 'persistence-session-handoff']) {
    const source = data.sources.find((s: { id: string }) => s.id === sourceId);
    await page.getByRole('button', { name: `读取${source.title}`, exact: true }).click();
    await expect(page.locator('.p-evidence-content')).toContainText('BASE-05');
    await page.getByRole('button', { name: '关闭证据内容', exact: true }).click();
  }
  await page.keyboard.press('Escape');
  await page
    .getByRole('navigation')
    .getByRole('button', { name: '成员与阶段', exact: true })
    .click();
  await page.getByLabel('筛选负责人').selectOption('foundation');
  for (const label of ['实现', '验收']) {
    await expect(
      page.getByRole('checkbox', { name: `BASE-01 队员独立复现说明 ${label}`, exact: true }),
    ).not.toBeChecked();
  }
  await page.getByLabel('筛选状态').selectOption('accepted');
  for (const label of ['实现', '验收']) {
    await expect(
      page.getByRole('checkbox', { name: `BASE-01 目录职责本地检查 ${label}`, exact: true }),
    ).toBeChecked();
    await expect(
      page.getByRole('checkbox', { name: `BASE-01 ${label}`, exact: true }),
    ).not.toBeChecked();
    await expect(
      page.getByRole('checkbox', {
        name: `BASE-06 模型计划安全接收 ${label}`,
        exact: true,
      }),
    ).toBeChecked();
    await expect(
      page.getByRole('checkbox', { name: `BASE-06 ${label}`, exact: true }),
    ).not.toBeChecked();
    await expect(
      page.getByRole('checkbox', {
        name: `BASE-05 本人任务进度与安全范围公共查询 ${label}`,
        exact: true,
      }),
    ).toBeChecked();
    await expect(
      page.getByRole('checkbox', {
        name: `BASE-05 持久化入口会话复核 ${label}`,
        exact: true,
      }),
    ).toBeChecked();
    await expect(
      page.getByRole('checkbox', { name: `BASE-05 ${label}`, exact: true }),
    ).not.toBeChecked();
  }
  await noOverflow(page);
  await page.screenshot({
    path: info.outputPath('accepted-persistence-session.png'),
    fullPage: true,
  });
});

test('implemented and independently accepted children are visible and filterable without checking parents', async ({
  page,
}) => {
  await synced(page);
  await page
    .getByRole('navigation')
    .getByRole('button', { name: '成员与阶段', exact: true })
    .click();
  const child = page.getByRole('checkbox', {
    name: 'BASE-03 可信前置裁决存储与核验 验收',
    exact: true,
  });
  await expect(child).toBeChecked();
  await expect(child).toBeDisabled();
  await expect(page.getByRole('checkbox', { name: 'BASE-03 验收', exact: true })).not.toBeChecked();
  await page.getByLabel('筛选状态').selectOption('accepted');
  await expect(child).toBeVisible();
  await expect(page.locator('.p-task-row').filter({ hasText: 'BASE-03' })).toContainText(
    '部分完成',
  );
  await page.getByLabel('筛选负责人').selectOption('cards');
  await expect(
    page.getByRole('checkbox', { name: 'CARD-01 只读查询与归属 / 多锁样例 验收', exact: true }),
  ).toBeChecked();
  await expect(page.getByRole('checkbox', { name: 'CARD-01 验收', exact: true })).not.toBeChecked();
  await page.getByLabel('搜索进度').fill('无此条目xyz');
  await expect(page.getByText('没有匹配的结果', { exact: true })).toBeVisible();
  await page.getByRole('button', { name: '清除筛选', exact: true }).click();
  await page.getByLabel('筛选负责人').selectOption('transfer');
  await expect(
    page.getByRole('checkbox', { name: 'TRF-02 修改、取消与过期 实现', exact: true }),
  ).toBeChecked();
  await expect(
    page.getByRole('checkbox', { name: 'TRF-02 修改、取消与过期 验收', exact: true }),
  ).not.toBeChecked();
  await page.getByLabel('筛选负责人').selectOption('bills');
  await expect(page.locator('.p-task-row')).toHaveCount(4);
  await expect(page.locator('input[type=checkbox]:checked')).toHaveCount(0);
  await noOverflow(page);
});

test('dependency-only words match material and restore conditions independently of task search', async ({
  page,
}) => {
  await synced(page);
  await page.getByRole('navigation').getByRole('button', { name: '待对接' }).click();
  await page.getByLabel('搜索进度').fill('吊销');
  await expect(page.locator('.p-dependency')).toHaveCount(1);
  await expect(page.locator('.p-dependency')).toContainText('正式认证政策与可信真源');
  await page.getByRole('button', { name: '恢复条件与证据' }).click();
  await expect(page.getByRole('dialog')).toContainText('恢复条件');
  await page.keyboard.press('Escape');
  await page.getByLabel('搜索进度').fill('无此条目xyz');
  await expect(page.getByText('没有匹配的结果', { exact: true })).toBeVisible();
  await page.getByRole('button', { name: '清除筛选', exact: true }).click();
  await page.getByLabel('筛选负责人').selectOption('audit');
  await expect(page.locator('.p-dependency')).toHaveCount(2);
});

test('future delivery states use accurate labels in list, overview and detail', async ({
  page,
}) => {
  const data = ledger();
  data.sources.push({
    ...data.sources[0],
    id: 'test-review',
    kind: 'review',
    date: '2026-10-06',
    scope: '独立检查未通过',
  });
  for (const [state, title] of [
    ['received', '只收到源码'],
    ['pending-review', '进入验收队列'],
    ['changes-requested', '修复提交前问题'],
  ]) {
    data.deliveries.push({
      ...data.deliveries[0],
      id: `test-${state}`,
      title,
      state,
      reviewedAt: state === 'changes-requested' ? '2026-10-06' : null,
      receivedAt: '2026-10-06',
      sourceIds: [state === 'changes-requested' ? 'test-review' : 'transfer-report'],
    });
  }
  await page.route('**/api/progress', (route) => route.fulfill({ json: data }));
  await synced(page);
  await expect(page.locator('.p-recent').filter({ hasText: '只收到源码' })).toContainText(
    '已收到材料',
  );
  await page.getByRole('navigation').getByRole('button', { name: '交付台账', exact: true }).click();
  for (const [title, label, dateLabel] of [
    ['只收到源码', '已收到材料', '收到日期'],
    ['进入验收队列', '待验收', '收到日期'],
    ['修复提交前问题', '待修复', '复核日期'],
  ]) {
    const row = page.locator('.p-delivery').filter({ hasText: title });
    await expect(row.locator('.p-badge')).toHaveText(label);
    await expect(row.locator('.p-delivery-date')).toContainText(dateLabel);
    await row.getByRole('button').click();
    await expect(page.getByRole('dialog').locator('.p-badge').first()).toHaveText(label);
    await page.keyboard.press('Escape');
  }
  await page.getByLabel('筛选状态').selectOption('changes-requested');
  await expect(page.locator('.p-delivery')).toHaveCount(1);
  await noOverflow(page);
});

test('evidence stays plain text, failure is explicit, export keeps source date', async ({
  page,
}) => {
  await synced(page);
  const download = page.waitForEvent('download');
  await page.getByRole('button', { name: '导出 JSON', exact: true }).click();
  const file = await download;
  expect(file.suggestedFilename()).toContain('SafeFlow_项目进度_2026-10-06');
  const path = await file.path();
  const exported = JSON.parse(readFileSync(path!, 'utf8'));
  expect(exported.updatedAt).toBe(ledger().updatedAt);
  await page
    .getByRole('navigation')
    .getByRole('button', { name: '成员与阶段', exact: true })
    .click();
  const button = page.getByRole('button', { name: '查看 CARD-01 详情', exact: true });
  await button.click();
  await page.getByRole('button', { name: '读取CARD-01 独立结论摘录' }).click();
  await expect(page.getByRole('dialog').locator('pre')).toContainText(
    '01a109fe-6234-7cd3-b754-71b231f41225',
  );
  await page.getByRole('button', { name: '关闭证据内容', exact: true }).click();
  await page.route('**/api/progress/evidence/card-review-excerpt', (route) =>
    route.fulfill({
      json: {
        sourceId: 'card-review-excerpt',
        content: '<img src=x onerror="window.unsafe=true">纯文本',
      },
    }),
  );
  await page.getByRole('button', { name: '读取CARD-01 独立结论摘录' }).click();
  await expect(page.getByRole('dialog').locator('pre')).toContainText('<img');
  expect(await page.evaluate(() => 'unsafe' in window)).toBe(false);
  await page.getByRole('button', { name: '关闭证据内容', exact: true }).click();
  await page.unroute('**/api/progress/evidence/card-review-excerpt');
  await page.route('**/api/progress/evidence/card-review-excerpt', (route) =>
    route.fulfill({ status: 503, json: { error: 'EVIDENCE_UNAVAILABLE' } }),
  );
  await page.getByRole('button', { name: '读取CARD-01 独立结论摘录' }).click();
  await expect(page.getByRole('dialog').getByRole('alert')).toContainText('证据暂时无法读取');
  await page.keyboard.press('Escape');
  await expect(button).toBeFocused();
});

test('automatic refresh reads newer snapshots, failure keeps old content and reconnect clears stale state', async ({
  page,
}) => {
  let data = ledger();
  let failure = false;
  let count = 0;
  await page.clock.install();
  await page.route('**/api/progress', (route) => {
    count++;
    return failure ? route.fulfill({ status: 503 }) : route.fulfill({ json: data });
  });
  await synced(page);
  data = {
    ...data,
    updatedAt: '2026-10-06T12:00:00+08:00',
    current: { ...data.current, headline: '新台账：保留限定范围与未闭环边界' },
  };
  await page.clock.runFor(15001);
  await expect(page.locator('h1')).toHaveText(data.current.headline);
  await expect(page.locator('.p-update')).toContainText('12:00:00');
  failure = true;
  await page.clock.runFor(15001);
  await expect(page.getByRole('alert')).toContainText('显示上次成功读取的台账');
  await expect(page.locator('h1')).toHaveText(data.current.headline);
  await expect(page.locator('.p-update')).toContainText('12:00:00');
  failure = false;
  await page.getByRole('button', { name: '立即刷新', exact: true }).click();
  await expect(page.getByText('台账已同步', { exact: true })).toBeVisible();
  await expect(page.getByRole('alert')).not.toBeVisible();
  expect(count).toBeGreaterThanOrEqual(4);
});

test('first read failure never fabricates progress and new tabs reread persistent ledger', async ({
  page,
  context,
}) => {
  await page.route('**/api/progress', (route) => route.fulfill({ status: 503 }));
  await page.goto('/progress.html');
  await expect(page.locator('h1')).toHaveText('无法读取项目台账');
  await expect(page.locator('.p-member')).toHaveCount(0);
  await page.unroute('**/api/progress');
  await page.getByRole('button', { name: '重新读取', exact: true }).click();
  await expect(page.locator('.p-member')).toHaveCount(5);
  await page.evaluate(() => localStorage.clear());
  const second = await context.newPage();
  await synced(second);
  await expect(second.locator('.p-member')).toHaveCount(5);
  await expect(second.locator('h1')).toHaveText(ledger().current.headline);
  await second.close();
});
