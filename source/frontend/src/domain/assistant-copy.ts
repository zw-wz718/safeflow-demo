import type { LocalRecord, Message } from './model';

export const assistantCopy = {
  balance: '工资卡（尾号 6628）的模拟可用余额为 ¥1,288.00 CNY。这是演示数据，未查询真实银行。',
  bills: (expense: string, count: number) =>
    `本月演示消费 ¥${expense}，共 ${count} 笔消费。转账不计入消费。仅展示演示流水，未查询真实银行。`,
  transfer: (payee: string, amount: string, sameNameCount: number) =>
    `可以为${payee}填写 ¥${amount} 的转账草案。${sameNameCount > 1 ? `有 ${sameNameCount} 位同名收款人，请选择银行与尾号。` : '请核对付款账户与收款尾号。'}请核对后自行保存草案；保存草案不会转账。`,
  incompleteTransfer:
    '还不清楚收款人和金额。请在转账汇款中选择已有收款人并填写金额；助手不会替你确认或执行转账。',
  invalidTransfer:
    '金额格式不明确。请在转账汇款中输入两位小数金额，例如 200.00；当前只支持保存草案，不会执行转账。',
  unsupported:
    '这个问题暂不支持。目前仅支持查询演示账户余额、本月账单，以及转账草案引导。可以尝试“账户余额”“本月账单”或“给小李转 200 元”；助手不会执行转账。',
};

// Exact v1 output templates from LocalDemoAdapter.sendMessage before this copy revision.
// No substring replacement: unknown replies and customer-authored text remain unchanged.
function legacyReplyText(text: string, action: Message['action'] | null) {
  if (
    (action === 'overview' || action === null) &&
    text ===
      '工资卡（尾号 6628）的模拟可用余额为 ¥1,288.00 CNY。数据来自底座固定样例，未查询真实银行。'
  ) {
    return assistantCopy.balance;
  }
  if (action === 'bills' || action === null) {
    const bill = /^本月前端样例消费 ¥([\d,]+\.\d{2})，共 (\d+) 笔消费。转账不计入消费。$/.exec(
      text,
    );
    if (bill) return assistantCopy.bills(bill[1], Number(bill[2]));
  }
  if (action === 'transfers' || action === null) {
    const transfer =
      /^拟给(小李|陈晨)生成 ¥(\d+\.\d{2}) 的本地草案。(?:有 (\d+) 位同名收款人，请选择银行与尾号。|请核对付款账户与收款尾号。)保存后不会转账。$/.exec(
        text,
      );
    if (transfer) return assistantCopy.transfer(transfer[1], transfer[2], Number(transfer[3] ?? 1));
    if (
      text ===
      '还不能确定当前收款人和金额。请到草案页选择已有收款人并填写金额；确认、扣款与改口不会由助手自动执行。'
    ) {
      return assistantCopy.incompleteTransfer;
    }
    if (text === '金额格式不明确。请到草案页输入合法两位小数金额，例如 200.00。') {
      return assistantCopy.invalidTransfer;
    }
  }
  if (
    (action === undefined || action === null) &&
    text ===
      '这条指令未匹配本地演示意图。可以查询“账户余额”“本月账单”，或填写“给小李转 200 元”；涉及资金执行的请求暂不可用。'
  ) {
    return assistantCopy.unsupported;
  }
  return text;
}

export function assistantMessageText(message: Pick<Message, 'role' | 'text' | 'action'>) {
  return message.role === 'assistant'
    ? legacyReplyText(message.text, message.action)
    : message.text;
}

export function assistantRecordText(record: Pick<LocalRecord, 'category' | 'title' | 'detail'>) {
  // This title/category pair is emitted by the old local assistant, not feedback/query free text.
  return record.category === 'query' && record.title === '助手本地意图处理'
    ? legacyReplyText(record.detail, null)
    : record.detail;
}
