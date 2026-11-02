import { chromium } from '@playwright/test';
import { mkdir } from 'node:fs/promises';

const base = process.env.SAFEFLOW_URL ?? 'http://127.0.0.1:5185';
const output = 'E:/银行agent/artifacts/bank-demo-redesign-2026-10-19';
await mkdir(output, { recursive: true });
const browser = await chromium.launch({ headless: true, channel: 'chrome' });
const desktop = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
const mobile = await browser.newPage({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });
const checks = [];

async function visit(page, hash) {
  await page.goto(`${base}/#${hash}`, { waitUntil: 'networkidle' });
  await page.locator('#main-content').waitFor();
  await page.waitForTimeout(250);
  checks.push({ viewport: page === desktop ? 'desktop' : 'mobile', hash, url: page.url(), overflow: await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth) });
}

await visit(desktop, 'overview');
await desktop.screenshot({ path: `${output}/desktop-home-after.png`, fullPage: true });
await visit(desktop, 'transfers');
await desktop.screenshot({ path: `${output}/desktop-transfers-after.png`, fullPage: true });
await visit(desktop, 'bills');
await desktop.screenshot({ path: `${output}/desktop-bills-after.png`, fullPage: true });

await visit(mobile, 'overview');
await mobile.screenshot({ path: `${output}/mobile-home-after.png`, fullPage: true });
await mobile.getByRole('button', { name: '打开导航' }).click();
await mobile.screenshot({ path: `${output}/mobile-navigation-after.png`, fullPage: true });
const customerText = await mobile.locator('body').innerText();
checks.push({ mobileNavigationOpen: true, hasProgressEntry: /项目进度|进度台账/.test(customerText), hasRuntimeEntry: /底座任务|Runtime/.test(customerText) });
await mobile.getByRole('button', { name: '转账汇款' }).click();
await mobile.screenshot({ path: `${output}/mobile-transfers-after.png`, fullPage: true });
await mobile.getByRole('button', { name: '打开导航' }).click();
await mobile.getByRole('button', { name: '我的账单' }).click();
await mobile.screenshot({ path: `${output}/mobile-bills-after.png`, fullPage: true });
checks.push({ mobileTransferOverflow: await mobile.evaluate(() => document.documentElement.scrollWidth <= innerWidth) });

console.log(JSON.stringify({ checks }, null, 2));
await browser.close();
