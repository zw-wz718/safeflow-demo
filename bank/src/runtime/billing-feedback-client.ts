import { z } from 'zod';
import { request, RuntimeApiError, RuntimeTransportError } from './client';
import type { AgentView } from './agent-client';

const id = z.string().min(1).max(200);
export const sourceKeySchema = z
  .object({ source_id: id, account_id: id, transaction_id: id })
  .strict();
export const choiceSchema = z.enum(['MINE', 'NOT_MINE', 'UNSURE']);
export const feedbackIntentSchema = z
  .object({
    input_version: z.literal(1),
    report_id: id,
    transaction: sourceKeySchema,
    choice: choiceSchema,
  })
  .strict();
export type FeedbackIntent = z.infer<typeof feedbackIntentSchema>;
const common = {
  contract_version: z.literal(1),
  scope: z.literal('BILLING_FEEDBACK_LOCAL_ONLY'),
  task_id: id,
  operation_id: id,
  lock_authorized: z.literal(false),
  automatic_retry: z.literal(false),
};
export const feedbackPreparedSchema = z
  .object({
    ...common,
    parameters: feedbackIntentSchema
      .extend({ original_digest: z.string().regex(/^[a-f0-9]{64}$/) })
      .strict(),
    confirmation: z
      .object({
        confirmation_ref: id,
        status: z.literal('PREPARED'),
        expires_at: z.string().datetime({ offset: true }),
      })
      .strict(),
  })
  .strict();
export type FeedbackPrepared = z.infer<typeof feedbackPreparedSchema>;
export const feedbackConfirmedSchema = z
  .object({
    ...common,
    confirmation_ref: id,
    status: z.literal('CONFIRMED'),
  })
  .strict();
export const feedbackResultSchema = z
  .object({
    ...common,
    task_state: z.literal('COMPLETED'),
    operation_state: z.literal('SUCCEEDED'),
    saved: z.record(z.unknown()),
    replayed: z.boolean(),
    // Old candidates omit this; absence never asserts domain audit adoption.
    domain_audit: z.enum(['NOT_ACTIVATED', 'LEGACY_WITHOUT_DOMAIN_EVENT', 'VERIFIED_V1']).optional(),
  })
  .strict();
export type FeedbackResult = z.infer<typeof feedbackResultSchema>;
export type FeedbackSelector = { task_id: string; operation_id: string };
export type BillingResult = Extract<
  NonNullable<Extract<AgentView, { kind: 'READ' }>['results']>[number],
  { tool_name: 'analyze_local_basic_spending' }
>;

function invalid(): never {
  throw new RuntimeTransportError('INVALID_RESPONSE');
}
function parse<T>(schema: z.ZodType<T>, value: unknown): T {
  const parsed = schema.safeParse(value);
  return parsed.success ? parsed.data : invalid();
}
export function feedbackEligible(result: BillingResult) {
  const r = result.data.report;
  return (
    result.data.adapter_contract === 'LOCAL_BILLING_REPORT_ADAPTER_V2' &&
    r.metadata.report_id === `billing_report_${result.operation_id}` &&
    result.result_ref === `billing_result_${result.operation_id}` &&
    r.read.is_complete &&
    r.read.state === 'COMPLETE' &&
    r.calculation_status === 'READY' &&
    r.evidence !== null &&
    r.evidence.length > 0
  );
}
export function sameSource(a: FeedbackIntent['transaction'], b: FeedbackIntent['transaction']) {
  return (
    a.source_id === b.source_id &&
    a.account_id === b.account_id &&
    a.transaction_id === b.transaction_id
  );
}
function bound(view: FeedbackSelector, original: FeedbackSelector) {
  if (view.operation_id !== original.operation_id || view.task_id !== original.task_id) invalid();
}
export async function prepareBillingFeedback(intent: FeedbackIntent, signal?: AbortSignal) {
  const input = parse(feedbackIntentSchema, intent);
  const value = parse(
    feedbackPreparedSchema,
    await request('/api/billing-feedback/prepare', 'POST', input, signal),
  );
  if (
    value.parameters.report_id !== input.report_id ||
    value.parameters.choice !== input.choice ||
    !sameSource(value.parameters.transaction, input.transaction) ||
    Date.parse(value.confirmation.expires_at) <= Date.now()
  )
    invalid();
  return value;
}
const actionPath = (p: FeedbackSelector, action: string) =>
  `/api/billing-feedback/tasks/${encodeURIComponent(p.task_id)}/operations/${encodeURIComponent(p.operation_id)}/${action}`;
export async function confirmBillingFeedback(p: FeedbackPrepared, signal?: AbortSignal) {
  const value = parse(
    feedbackConfirmedSchema,
    await request(
      actionPath(p, 'confirm'),
      'POST',
      { confirmation_ref: p.confirmation.confirmation_ref },
      signal,
    ),
  );
  bound(value, p);
  if (value.confirmation_ref !== p.confirmation.confirmation_ref) invalid();
  return value;
}
export async function executeBillingFeedback(p: FeedbackPrepared, signal?: AbortSignal) {
  const value = parse(
    feedbackResultSchema,
    await request(
      actionPath(p, 'execute'),
      'POST',
      { confirmation_ref: p.confirmation.confirmation_ref },
      signal,
    ),
  );
  bound(value, p);
  return value;
}
export async function getBillingFeedback(operation: string, task?: string, signal?: AbortSignal) {
  const value = parse(
    feedbackResultSchema,
    await request(
      `/api/billing-feedback/operations/${encodeURIComponent(operation)}`,
      'GET',
      undefined,
      signal,
    ),
  );
  if (
    value.operation_id !== operation ||
    (task !== undefined && value.task_id !== task) ||
    !value.replayed
  )
    invalid();
  return value;
}
export function feedbackError(error: unknown) {
  if (error instanceof RuntimeApiError) {
    const prefix = `HTTP ${error.status} · ${error.reason}：`;
    if (error.status === 501) return prefix + '反馈能力尚未启用，查询也不可用。';
    if (error.status === 404) return prefix + '原操作不存在或当前会话不可见；不能据此认定未提交。';
    if (error.status === 401 || error.status === 403)
      return prefix + '会话或授权不可用，已清除当前报告和确认。';
    if (error.status === 409) return prefix + '原操作需要核对，仅可查询原号。';
    if (error.status === 503) return prefix + '服务暂不可用，结果尚未核实。';
    return prefix + '请求未完成。';
  }
  if (error instanceof RuntimeTransportError)
    return `${error.reason}：连接中断、超时或响应未通过校验，结果尚未核实。`;
  return error instanceof Error ? error.message : '状态未核实。';
}
