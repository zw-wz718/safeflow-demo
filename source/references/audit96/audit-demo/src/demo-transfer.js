import { randomUUID } from 'node:crypto';

const USER_ID = 'demo-user-001';
const PAYER_REF = 'demo-payer-001';
const PAYEE_REF = 'demo-payee-001';

export function recordSimulatedTransfer(store, outcome = 'success') {
  if (!['success', 'failure'].includes(outcome)) throw new TypeError('outcome must be success or failure');
  const taskId = `TASK-${randomUUID()}`;
  const operationId = `OP-${randomUUID()}`;
  const finalResult = outcome === 'success' ? 'SUCCEEDED' : 'FAILED';
  const common = {
    task_id: taskId,
    node_id: 'node_transfer_demo',
    operation_id: operationId,
    user_id: USER_ID,
    tool_name: 'execute_transfer',
    confirmation: 'CONFIRMED_DEMO_ONLY',
    auth_result: 'DEMO_NOT_AUTHENTICATED',
    security_result: 'DEMO_PLACEHOLDER_NOT_A_SECURITY_DECISION',
  };
  const events = [
    {
      kind: 'SECURITY_EVALUATED', action: 'SECURITY_RESULT_OBSERVED', result: 'NOT_SUBMITTED',
      details: { demo_only: true, decision_id: null, note: '占位，不执行 SecurityKernel 或授权判断' },
    },
    {
      kind: 'TOOL_STARTED', action: 'TRANSFER_SUBMITTED', result: 'PROCESSING',
      details: { confirmation: 'CONFIRMED_DEMO_ONLY', amount: 'CNY 200.00', payer_ref: PAYER_REF, payee_ref: PAYEE_REF, simulated: true },
    },
    {
      kind: 'TOOL_COMPLETED', action: outcome === 'success' ? 'TRANSFER_SUCCEEDED' : 'TRANSFER_FAILED', result: finalResult,
      details: {
        amount: 'CNY 200.00', payer_ref: PAYER_REF, payee_ref: PAYEE_REF, simulated: true,
        note: outcome === 'success' ? '虚构的模拟成功结果' : '模拟失败，未实际提交银行',
        ...(outcome === 'failure' ? { failure_reason: '模拟失败，未实际提交银行' } : {}),
      },
    },
    {
      kind: 'TASK_COMPLETED', action: 'TASK_COMPLETED', result: finalResult,
      details: { demo_only: true, note: '任务终态来自本地演示脚本，不是银行或业务系统回执' },
    },
  ];
  const records = events.map((event) => store.recordAudit({
    ...common,
    ...event,
    event_id: randomUUID(),
    timestamp: new Date().toISOString(),
  }));
  return { task_id: taskId, operation_id: operationId, user_id: USER_ID, outcome: finalResult, records };
}
