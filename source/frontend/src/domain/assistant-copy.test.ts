import { describe, expect, it } from 'vitest';
import { assistantMessageText, assistantRecordText } from './assistant-copy';
import legacy from './assistant-legacy.fixture.json';
import { messageSchema } from './model';

describe('known local assistant history presentation', () => {
  it.each(legacy.replies)(
    'translates the complete old $input reply without mutating its data',
    ({ message }) => {
      const stored = messageSchema.parse({
        ...message,
        id: 'local_message_old',
        at: '2026-10-19T00:00:00Z',
      });
      const original = structuredClone(stored);
      const shown = assistantMessageText(stored);
      expect(shown).not.toBe(stored.text);
      expect(shown).not.toMatch(/底座|前端|意图/);
      expect(stored).toEqual(original);
      expect(
        assistantRecordText({ category: 'query', title: '助手本地意图处理', detail: stored.text }),
      ).toBe(shown);
    },
  );

  it('preserves the historic amount and count rather than querying or recalculating the current month', () => {
    const shown = assistantMessageText({
      role: 'assistant',
      action: 'bills',
      text: '本月前端样例消费 ¥6,234.56，共 12 笔消费。转账不计入消费。',
    });
    expect(shown).toContain('¥6,234.56');
    expect(shown).toContain('12 笔');
    expect(shown).toContain('转账不计入消费');
  });

  it.each(legacy.replies)(
    'preserves user text even when it exactly equals the old $input reply',
    ({ message }) => {
      expect(assistantMessageText({ ...message, role: 'user', action: undefined })).toBe(
        message.text,
      );
    },
  );

  it('leaves arbitrary replies, quotations, mismatched actions and free-text records untouched', () => {
    const text = `请保留我的原文：“${legacy.replies[0].message.text}” <img src=x>`;
    expect(assistantMessageText({ role: 'assistant', text })).toBe(text);
    expect(
      assistantMessageText({
        role: 'assistant',
        action: 'bills',
        text: legacy.replies[0].message.text,
      }),
    ).toBe(legacy.replies[0].message.text);
    for (const source of [
      { category: 'feedback' as const, title: '登记账单反馈' },
      { category: 'query' as const, title: '自定义查询备注' },
    ]) {
      expect(assistantRecordText({ ...source, detail: legacy.replies[0].message.text })).toBe(
        legacy.replies[0].message.text,
      );
    }
  });

  it.each([
    '还不能确定当前收款人和金额。请到草案页选择已有收款人并填写金额；确认、扣款与改口不会由助手自动执行。',
    '金额格式不明确。请到草案页输入合法两位小数金额，例如 200.00。',
  ])('retains the no-transfer boundary for old incomplete transfer replies', (text) => {
    const shown = assistantMessageText({ role: 'assistant', action: 'transfers', text });
    expect(shown).toContain('转账汇款');
    expect(shown).toMatch(/不会.*转账/);
  });
});
