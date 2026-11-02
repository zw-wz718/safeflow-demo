import { expect, test, type Page, type TestInfo } from '@playwright/test';
import { mkdir, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import legacy from '../src/domain/assistant-legacy.fixture.json' with { type: 'json' };
import { stateSchema } from '../src/domain/model';

const storageKey = 'safeflow.demo.v1';
const evidence = resolve('../artifacts/bank-demo-redesign-2026-10-19/assistant-revision');
// Expected customer-facing results, asserted against real sends and frozen v1 history.
const expectedReplies = [
  '工资卡（尾号 6628）的模拟可用余额为 ¥1,288.00 CNY。这是演示数据，未查询真实银行。',
  '本月演示消费 ¥756.50，共 7 笔消费。转账不计入消费。仅展示演示流水，未查询真实银行。',
  '可以为小李填写 ¥200.00 的转账草案。有 2 位同名收款人，请选择银行与尾号。请核对后自行保存草案；保存草案不会转账。',
  '这个问题暂不支持。目前仅支持查询演示账户余额、本月账单，以及转账草案引导。可以尝试“账户余额”“本月账单”或“给小李转 200 元”；助手不会执行转账。',
];

async function storedState(page: Page) {
  return stateSchema.parse(
    await page.evaluate((key) => JSON.parse(localStorage.getItem(key)!), storageKey),
  );
}

async function sendAndWait(page: Page, input: string, replyCount: number) {
  await page.getByRole('textbox', { name: '对话输入' }).fill(input);
  await page.getByRole('button', { name: '发送消息' }).click();
  await expect(page.getByText('正在处理你的问题…', { exact: true })).toBeVisible();
  // The busy indicator is also .message.assistant: only .message-body proves completion.
  await expect(page.locator('.message.assistant .message-body')).toHaveCount(replyCount);
  await expect(page.locator('.processing')).toHaveCount(0);
  await expect(page.getByRole('textbox', { name: '对话输入' })).toBeEnabled();
}

function watchEffects(page: Page) {
  const errors: string[] = [];
  const writes: string[] = [];
  page.on('pageerror', (error) => errors.push(error.message));
  page.on('request', (request) => {
    if (request.method() !== 'GET') writes.push(`${request.method()} ${request.url()}`);
  });
  return { errors, writes };
}

async function capture(page: Page, info: TestInfo, name: string) {
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await mkdir(evidence, { recursive: true });
  await page.screenshot({
    path: resolve(evidence, `${info.project.name}-${name}.png`),
    fullPage: true,
  });
}

async function saveEvidence(info: TestInfo, name: string, value: unknown) {
  await mkdir(evidence, { recursive: true });
  await writeFile(
    resolve(evidence, `${info.project.name}-${name}.json`),
    JSON.stringify(value, null, 2),
  );
}

test('four completed replies survive refresh and transfer guidance requires explicit payee selection', async ({
  page,
}, info) => {
  const effects = watchEffects(page);
  await page.goto('/#agent');
  await expect(page.locator('h1')).toHaveText('智能助手');
  await expect(page.locator('.chat-topline')).toContainText('未接入真实模型');
  for (const [index, sample] of legacy.replies.entries()) {
    await sendAndWait(page, sample.input, index + 1);
    await expect(page.locator('.message.assistant .message-body').nth(index)).toHaveText(
      expectedReplies[index],
    );
  }
  expect(
    (await page.locator('.message.assistant .message-body').allTextContents()).join('\n'),
  ).not.toMatch(/底座|前端|意图|本地草案/);
  const completed = await storedState(page);
  expect(completed.messages.filter((m) => m.role === 'user').map((m) => m.text)).toEqual(
    legacy.replies.map((sample) => sample.input),
  );
  expect(completed.records.map((r) => r.detail)).toEqual([...expectedReplies].reverse());
  expect(completed.records.every((r) => r.title === '智能助手咨询')).toBe(true);
  expect(completed.drafts).toEqual([]);
  await capture(page, info, 'fresh-replies');

  // Refresh only after all four real bodies are present and the input is enabled.
  await page.reload();
  await expect(page.locator('.message.assistant .message-body')).toHaveText(expectedReplies);
  expect(await storedState(page)).toEqual(completed);
  await capture(page, info, 'fresh-restored');
  if (info.project.name === 'mobile') {
    await page.setViewportSize({ width: 360, height: 800 });
    await capture(page, info, 'fresh-restored-360');
    await page.setViewportSize({ width: 390, height: 844 });
  }

  await page.goto('/#activity');
  await expect(page.locator('.activity-main p')).toHaveText([...expectedReplies].reverse());
  await capture(page, info, 'fresh-records');
  await page.goto('/#agent');
  await page.getByRole('button', { name: '选择收款人并核对草案' }).click();
  await expect(page.getByRole('textbox', { name: '转账金额', exact: true })).toHaveValue('200.00');
  const choices = page.getByRole('radio');
  await expect(choices).toHaveCount(2);
  await expect(choices.nth(0)).not.toBeChecked();
  await expect(choices.nth(1)).not.toBeChecked();
  expect((await storedState(page)).drafts).toEqual([]);
  await capture(page, info, 'payee-unresolved');
  await page.getByRole('radio', { name: '小李 第二示例银行 · 7788' }).check();
  await page.getByRole('button', { name: '保存并核对草案' }).click();
  await expect(page.getByRole('dialog')).toContainText('第二示例银行');
  await expect(page.getByRole('dialog')).toContainText('7788');
  await expect(page.getByRole('button', { name: '确认并转账 · 尚未接入' })).toBeDisabled();
  const selected = await storedState(page);
  expect(selected.drafts).toHaveLength(1);
  expect(selected.drafts[0]).toMatchObject({
    payeeId: 'payee_009',
    amount: '200.00',
    state: 'DRAFT',
    version: 1,
  });
  await capture(page, info, 'selected-draft');
  expect(effects).toEqual({ errors: [], writes: [] });
  await saveEvidence(info, 'fresh-history', {
    inputs: legacy.replies.map((sample) => sample.input),
    completed,
    restoredUnchanged: true,
    selected,
    effects,
  });
});

test('known v1 assistant history is translated only for display while user text and stored resources remain intact', async ({
  page,
}, info) => {
  const effects = watchEffects(page);
  const at = '2026-10-19T01:00:00Z';
  const userText = [legacy.replies[0].message.text, legacy.replies[3].message.text];
  const quoted = `请保留原文：“${legacy.replies[1].message.text}” <img src=x>`;
  const feedback = '请保留：前端样例、底座固定样例、版本 v1 → v2、工作台提醒、助手本地意图处理。';
  const seeded = stateSchema.parse({
    schemaVersion: 1,
    userId: 'user_001',
    drafts: [
      {
        id: 'local_draft_legacy',
        userId: 'user_001',
        accountId: 'account_001',
        payeeId: 'payee_008',
        amount: '120.00',
        currency: 'CNY',
        note: feedback,
        version: 2,
        state: 'DRAFT',
        createdAt: at,
        updatedAt: at,
        expiresAt: '2026-10-19T01:30:00Z',
      },
    ],
    feedback: [{ transactionId: 'demo_tx_001', reason: feedback, at }],
    subscriptionPrefs: { demo_sub_music: { reminder: false, cancelRequested: true } },
    messages: [
      ...legacy.replies.flatMap((sample, index) => [
        { id: `local_message_user${index}`, role: 'user', text: sample.input, at },
        { ...sample.message, id: `local_message_reply${index}`, at },
      ]),
      ...userText.map((text, index) => ({
        id: `local_message_literal${index}`,
        role: 'user',
        text,
        at,
      })),
      { id: 'local_message_quoted', role: 'assistant', text: quoted, at },
    ],
    records: [
      ...legacy.replies.map((sample, index) => ({
        id: `local_record_reply${index}`,
        at,
        title: '助手本地意图处理',
        detail: sample.message.text,
        category: 'query',
        reference: '',
        task: sample.message.action === 'transfers' ? 'WAITING_USER' : 'COMPLETED',
        operation: null,
        security: sample.message.action === 'transfers' ? 'NEEDS_INPUT' : 'ALLOW',
      })),
      {
        id: 'local_record_feedback',
        at,
        title: '登记账单反馈',
        detail: feedback,
        category: 'feedback',
        reference: 'demo_tx_001',
        task: 'COMPLETED',
        operation: null,
        security: 'ALLOW',
      },
      {
        id: 'local_record_custom',
        at,
        title: '自定义查询备注',
        detail: legacy.replies[3].message.text,
        category: 'query',
        reference: '',
        task: 'COMPLETED',
        operation: null,
        security: 'ALLOW',
      },
    ],
  });
  const raw = JSON.stringify(seeded);
  await page.goto('/#agent');
  await expect(page.locator('h1')).toHaveText('智能助手');
  await page.evaluate(({ key, value }) => localStorage.setItem(key, value), {
    key: storageKey,
    value: raw,
  });
  await page.reload();
  const expectedHistory = [...expectedReplies, quoted];
  await expect(page.locator('.message.assistant .message-body')).toHaveText(expectedHistory);
  const expectedUserText = [...legacy.replies.map((sample) => sample.input), ...userText];
  await expect(page.locator('.message.user .message-body')).toHaveText(expectedUserText);
  await expect(page.locator('.chat-history img')).toHaveCount(0);
  expect(await storedState(page)).toEqual(seeded);
  await capture(page, info, 'legacy-history');
  await page.reload();
  await expect(page.locator('.message.assistant .message-body')).toHaveText(expectedHistory);
  await expect(page.locator('.message.user .message-body')).toHaveText(expectedUserText);
  expect(await page.evaluate((key) => localStorage.getItem(key), storageKey)).toBe(raw);

  await page.goto('/#activity');
  await expect(page.getByRole('button', { name: '查看智能助手咨询记录' })).toHaveCount(4);
  await expect(page.locator('.activity-main p')).toHaveText([
    ...expectedReplies,
    feedback,
    legacy.replies[3].message.text,
  ]);
  await page.getByRole('button', { name: '查看智能助手咨询记录' }).nth(1).click();
  await expect(page.locator('.record-detail-text')).toHaveText(expectedReplies[1]);
  await page.keyboard.press('Escape');
  await page.getByRole('button', { name: '查看登记账单反馈记录' }).click();
  await expect(page.locator('.record-detail-text')).toHaveText(feedback);
  await capture(page, info, 'legacy-feedback-unchanged');
  await page.keyboard.press('Escape');
  expect(await page.evaluate((key) => localStorage.getItem(key), storageKey)).toBe(raw);
  await page.goto('/#agent');
  // Sending the literal old balance reply must also preserve it as a new user message.
  await sendAndWait(page, userText[0], expectedHistory.length + 1);
  await expect(page.locator('.message.user .message-body').last()).toHaveText(userText[0]);
  const afterSend = await storedState(page);
  expect(afterSend.messages.slice(0, seeded.messages.length)).toEqual(seeded.messages);
  expect(afterSend.messages).toHaveLength(seeded.messages.length + 2);
  expect(afterSend.records.slice(1)).toEqual(seeded.records);
  expect(afterSend.drafts).toEqual(seeded.drafts);
  expect(afterSend.feedback).toEqual(seeded.feedback);
  expect(afterSend.subscriptionPrefs).toEqual(seeded.subscriptionPrefs);
  await page.reload();
  await expect(page.locator('.message.user .message-body').last()).toHaveText(userText[0]);
  expect(await storedState(page)).toEqual(afterSend);
  expect(effects).toEqual({ errors: [], writes: [] });
  await saveEvidence(info, 'legacy-history', {
    fixtureSource: legacy.source,
    seeded,
    displayedReplies: expectedHistory,
    rawUnchangedOnViewAndReload: true,
    afterSend,
    effects,
  });
});
