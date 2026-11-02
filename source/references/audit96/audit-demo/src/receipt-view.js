const PAYER_LABEL = '演示付款人 A';
const PAYEE_LABEL = '演示收款人 B';

// This is a display projection only, not an authentication or authorization boundary.
export function createParticipantReceipt(records, view) {
  if (!['payer', 'recipient'].includes(view)) throw new Error('view must be payer or recipient');
  if (!records.length) return null;
  // Never invent participant identities for an event received from a real business source.
  // Formal receipts require a trusted identity and D-12 authorization contract.
  const demoRecords = records.filter((record) => record.details?.simulated === true);
  const submitted = demoRecords.find((record) => record.action === 'TRANSFER_SUBMITTED');
  const completed = demoRecords.find((record) => record.action === 'TRANSFER_SUCCEEDED' || record.action === 'TRANSFER_FAILED');
  if (!submitted && !completed) return null;
  const current = completed ?? submitted;
  if (!current.operation_id) return null;
  const status = completed?.result ?? submitted.result;
  return {
    receipt_ref: current.operation_id.slice(-8).toUpperCase(),
    perspective: view === 'payer' ? '付款人视角' : '收款人视角',
    payer_label: PAYER_LABEL,
    payee_label: PAYEE_LABEL,
    amount: current.details.amount ?? submitted?.details.amount ?? '未提供',
    status,
    occurred_at: current.timestamp,
    failure_reason: status === 'FAILED' ? '模拟失败，未实际提交银行' : null,
    simulated: true,
  };
}
