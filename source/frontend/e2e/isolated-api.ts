import { expect, type Page } from '@playwright/test';

const unexpectedByPage = new WeakMap<Page, string[]>();
export async function guardReplayApi(page: Page) {
  const unexpected: string[] = [];
  unexpectedByPage.set(page, unexpected);
  // Install before the registered routes. Playwright tries newer handlers first;
  // every unregistered API request fails locally instead of reaching an API.
  await page.route('**/api/**', (route) => {
    unexpected.push(`${route.request().method()} ${new URL(route.request().url()).pathname}`);
    return route.fulfill({ status: 503, contentType: 'application/json', body: '{}' });
  });
}
export function assertReplayIsolated(page: Page) {
  expect(unexpectedByPage.get(page) ?? [], 'Unregistered replay API requests').toEqual([]);
}
