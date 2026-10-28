import { z } from 'zod';
import { RuntimeApiError, RuntimeTransportError } from './client';
import { errorSchema } from './schema';
import {
  feedbackIntentSchema,
  feedbackPreparedSchema,
  feedbackResultSchema,
  sameSource,
  type FeedbackIntent,
} from './billing-feedback-client';
const uuid = z
  .string()
  .regex(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
const id = z.string().min(1).max(200),
  revision = z.number().int().positive().safe();
const envelope = {
  contract_version: z.literal(2),
  scope: z.literal('BILLING_FEEDBACK_LOCAL_ONLY'),
  recovery_realm: uuid,
  nonce: uuid,
  lock_authorized: z.literal(false),
  automatic_retry: z.literal(false),
};
const parameters = feedbackPreparedSchema.shape.parameters;
const prepared = z
  .object({
    ...envelope,
    kind: z.literal('CREATED'),
    state: z.literal('PREPARED_OPEN'),
    revision,
    task_id: id,
    operation_id: id,
    parameters,
    confirmation: feedbackPreparedSchema.shape.confirmation,
  })
  .strict();
const read = z
  .object({
    ...envelope,
    kind: z.enum(['READ', 'RECOVERY_ONLY']),
    state: z.enum(['PREPARED_OPEN', 'EXECUTION_ADMITTED', 'SAVED', 'CLOSED_ROLLBACK', 'ABANDONED']),
    revision,
    task_id: id.nullable(),
    operation_id: id.nullable(),
    can_continue: z.literal(false),
    parameters: parameters.nullable(),
    confirmation_observation: z
      .object({
        status: z.enum(['PREPARED', 'CONFIRMED', 'REVOKED', 'EXPIRED']),
        original_expires_at: z.string().datetime({ offset: true }),
      })
      .strict()
      .nullable(),
    result_available: z.boolean(),
    fence_revision: revision.nullable(),
  })
  .strict()
  .superRefine((v, c) => {
    const visible = v.task_id !== null;
    if (
      visible !== (v.operation_id !== null) ||
      visible !== (v.parameters !== null) ||
      visible !== (v.confirmation_observation !== null) ||
      (v.state === 'ABANDONED') !== (v.fence_revision !== null) ||
      (v.fence_revision !== null && v.fence_revision !== v.revision) ||
      v.result_available !== (visible && v.state === 'SAVED')
    )
      c.addIssue({ code: 'custom', message: 'inconsistent recovery' });
  });
const abandoned = z
  .object({
    ...envelope,
    kind: z.literal('ABANDONED'),
    state: z.literal('ABANDONED'),
    revision,
    fence_revision: revision,
    task_id: id.nullable(),
    operation_id: id.nullable(),
  })
  .strict()
  .refine(
    (v) => v.revision === v.fence_revision && (v.task_id === null) === (v.operation_id === null),
  );
const confirmed = z
  .object({
    ...envelope,
    kind: z.literal('CONFIRMED'),
    state: z.literal('PREPARED_OPEN'),
    revision,
    task_id: id,
    operation_id: id,
    confirmation: prepared.shape.confirmation.extend({ status: z.literal('CONFIRMED') }).strict(),
  })
  .strict();
const executed = z
  .object({
    ...envelope,
    kind: z.literal('SAVED'),
    state: z.literal('SAVED'),
    revision,
    task_id: id,
    operation_id: id,
    result: feedbackResultSchema,
  })
  .strict()
  .refine((v) => v.result.task_id === v.task_id && v.result.operation_id === v.operation_id);
export const recoveryContextSchema = z
  .object({
    contract_version: z.literal(2),
    scope: z.literal('BILLING_FEEDBACK_LOCAL_ONLY'),
    recovery_realm: uuid.nullable(),
    available: z.boolean(),
  })
  .strict()
  .refine((v) => !v.available || v.recovery_realm !== null);
export type RecoveryCreated = z.infer<typeof prepared>;
export type RecoveryRead = z.infer<typeof read>;
export type RecoveryClosed = z.infer<typeof abandoned>;
export class RecoveryError extends Error {
  constructor(
    public readonly status: number,
    public readonly code: string,
  ) {
    super(`HTTP ${status} · ${code}：状态未核清，保留原记录。`);
  }
}
function invalid(): never {
  throw new RuntimeTransportError('INVALID_RESPONSE');
}
function parse<T>(schema: z.ZodType<T>, value: unknown): T {
  const p = schema.safeParse(value);
  return p.success ? p.data : invalid();
}
const codes = [
  'INVALID_REQUEST',
  'SESSION_UNAVAILABLE',
  'NOT_FOUND_OR_NOT_OWNED',
  'PREPARATION_KEY_CONFLICT',
  'PREPARATION_ALREADY_OPEN',
  'RECONCILE_REQUIRED',
  'ALREADY_SAVED',
  'LEGACY_UNFENCED',
  'RECOVERY_REALM_MISMATCH',
  'PREPARATION_CLOSED',
  'PREPARATION_CAPACITY',
  'DISABLED',
  'UNAVAILABLE_OR_INTEGRITY',
] as const;
async function wire(path: string, body: unknown, signal?: AbortSignal) {
  const controller = new AbortController();
  const abort = () => controller.abort();
  signal?.addEventListener('abort', abort, { once: true });
  if (signal?.aborted) controller.abort();
  let timed = false;
  const timer = setTimeout(() => {
    timed = true;
    controller.abort();
  }, 8000);
  try {
    const r = await fetch(path, {
      method: body === undefined ? 'GET' : 'POST',
      credentials: 'include',
      cache: 'no-store',
      signal: controller.signal,
      ...(body === undefined
        ? {}
        : {
            headers: { 'Content-Type': 'application/json', 'X-SafeFlow-Request': '1' },
            body: JSON.stringify(body),
          }),
    });
    const value: unknown = await r.json();
    if (!r.ok) {
      const domain = z
        .object({ contract_version: z.literal(2), code: z.enum(codes) })
        .strict()
        .safeParse(value);
      if (domain.success) throw new RecoveryError(r.status, domain.data.code);
      const early = errorSchema.safeParse(value);
      if (early.success)
        throw new RuntimeApiError(
          early.data.error.reason_code,
          early.data.error.task_id,
          early.data.error.next_action,
          r.status,
        );
      invalid();
    }
    return value;
  } catch (e) {
    if (timed) throw new RuntimeTransportError('TIMEOUT');
    if (signal?.aborted) throw new RuntimeTransportError('ABORTED');
    if (
      e instanceof RecoveryError ||
      e instanceof RuntimeApiError ||
      e instanceof RuntimeTransportError
    )
      throw e;
    throw new RuntimeTransportError('UNAVAILABLE');
  } finally {
    clearTimeout(timer);
    signal?.removeEventListener('abort', abort);
  }
}
const base = '/api/billing-feedback/v2';
const path = (nonce: string) => `${base}/preparations/${encodeURIComponent(parse(uuid, nonce))}`;
function bound(v: { recovery_realm: string; nonce: string }, realm: string, nonce: string) {
  if (v.recovery_realm !== realm || v.nonce !== nonce) invalid();
}
export async function recoveryContext(signal?: AbortSignal) {
  return parse(recoveryContextSchema, await wire(`${base}/context`, undefined, signal));
}
export async function recoveryPrepare(
  realm: string,
  nonce: string,
  intent: FeedbackIntent,
  signal?: AbortSignal,
) {
  const input = parse(feedbackIntentSchema, intent);
  const v = parse(
    z.union([prepared, read]),
    await wire(
      `${base}/preparations`,
      {
        contract_version: 2,
        recovery_realm: parse(uuid, realm),
        nonce: parse(uuid, nonce),
        intent: input,
      },
      signal,
    ),
  );
  bound(v, realm, nonce);
  if (
    v.kind === 'CREATED' &&
    (v.parameters.report_id !== input.report_id ||
      v.parameters.choice !== input.choice ||
      !sameSource(v.parameters.transaction, input.transaction) ||
      Date.parse(v.confirmation.expires_at) <= Date.now())
  )
    invalid();
  return v;
}
export async function recoveryRead(realm: string, nonce: string, signal?: AbortSignal) {
  const v = parse(
    read,
    await wire(
      `${path(nonce)}?recovery_realm=${encodeURIComponent(parse(uuid, realm))}`,
      undefined,
      signal,
    ),
  );
  bound(v, realm, nonce);
  return v;
}
export async function recoveryReadOperation(
  realm: string,
  operation: string,
  signal?: AbortSignal,
) {
  const v = parse(
    read,
    await wire(
      `${base}/operations/${encodeURIComponent(operation)}/preparation?recovery_realm=${encodeURIComponent(parse(uuid, realm))}`,
      undefined,
      signal,
    ),
  );
  if (v.recovery_realm !== realm || v.operation_id !== operation) invalid();
  return v;
}
function action(p: RecoveryCreated) {
  return {
    contract_version: 2,
    recovery_realm: p.recovery_realm,
    task_id: p.task_id,
    operation_id: p.operation_id,
    confirmation_ref: p.confirmation.confirmation_ref,
  };
}
export async function recoveryConfirm(p: RecoveryCreated, signal?: AbortSignal) {
  const v = parse(confirmed, await wire(`${path(p.nonce)}/confirm`, action(p), signal));
  bound(v, p.recovery_realm, p.nonce);
  if (
    v.task_id !== p.task_id ||
    v.operation_id !== p.operation_id ||
    v.confirmation.confirmation_ref !== p.confirmation.confirmation_ref ||
    v.confirmation.expires_at !== p.confirmation.expires_at
  )
    invalid();
  return v;
}
export async function recoveryExecute(p: RecoveryCreated, signal?: AbortSignal) {
  const v = parse(executed, await wire(`${path(p.nonce)}/execute`, action(p), signal));
  bound(v, p.recovery_realm, p.nonce);
  if (v.task_id !== p.task_id || v.operation_id !== p.operation_id) invalid();
  return v;
}
export async function recoveryAbandon(realm: string, nonce: string, signal?: AbortSignal) {
  const v = parse(
    abandoned,
    await wire(
      `${path(nonce)}/abandon`,
      { contract_version: 2, recovery_realm: parse(uuid, realm) },
      signal,
    ),
  );
  bound(v, realm, nonce);
  return v;
}
