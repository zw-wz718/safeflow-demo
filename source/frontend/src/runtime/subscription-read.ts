import { z } from 'zod';
import { request, RuntimeTransportError } from './client';
import { contextSchema, CardReadEpoch } from './card-read';
export { CardReadEpoch as SubscriptionReadEpoch };
const text = z.string().max(2000);
export const subscriptionReadSchema = z
  .object({
    contract_version: z.literal(1),
    mode: z.literal('PROJECT_SIMULATION'),
    scope: z.literal('SUBSCRIPTION_READ_ONLY'),
    status: z.enum(['AVAILABLE', 'NO_LEDGER']),
    items: z
      .array(
        z
          .object({
            position: z.number().int().positive().max(4096),
            selection_ref: z.string().uuid(),
            current_state: text,
            service_data_state: text,
            message: text,
            reminder_state: text,
            reminder: z
              .object({
                expected_date: z.string().regex(/^\d{4}-\d{2}-\d{2}$/),
                enabled: z.boolean(),
                days_before: z.number().int().min(0).max(255),
              })
              .strict()
              .nullable(),
          })
          .strict(),
      )
      .max(4096),
    originals: z
      .array(z.object({ original_ref: z.string().uuid(), status: text, message: text }).strict())
      .max(8192),
    pending: z.array(z.object({ action: text, state: text, expires_at: text }).strict()).max(8192),
    writes_enabled: z.literal(false),
    notification_sent: z.literal(false),
    merchant_action_executed: z.literal(false),
    message: text,
  })
  .strict()
  .superRefine((v, c) => {
    if (
      new Set(v.items.map((i) => i.position)).size !== v.items.length ||
      (v.status === 'NO_LEDGER' && (v.items.length || v.originals.length || v.pending.length))
    )
      c.addIssue({ code: 'custom', message: 'Inconsistent subscription observation' });
  });
export type SubscriptionReadView = z.infer<typeof subscriptionReadSchema>;
const pending = z.object({ action: text, state: text, expires_at: text }).strict();
const date = z.string().regex(/^\d{4}-\d{2}-\d{2}$/);
const count = z.number().int().nonnegative().max(4294967295);
export const subscriptionDetailSchema = z
  .object({
    fictional: z.literal(true),
    service_display_name: z.null(),
    current_state: text,
    service_data_state: text,
    message: text,
    cancellation: z
      .object({
        fictional: z.literal(true),
        current_state: text,
        history: z
          .array(
            z
              .object({
                kind: z.enum(['RECORD', 'REVOKE']),
                occurred_at: text,
                state_after_operation: text,
              })
              .strict(),
          )
          .max(8192),
        merchant_cancellation_confirmed: z.literal(false),
        message: text,
      })
      .strict(),
    reminder: z
      .object({
        fictional: z.literal(true),
        data_state: text,
        schedule: z
          .object({
            cycle: text,
            expected_date: date,
            enabled: z.boolean(),
            days_before: z.number().int().min(0).max(255),
          })
          .strict()
          .nullable(),
        active_kind: text.nullable(),
        current_attempt: text.nullable(),
        current_attempt_count: count.nullable(),
        retries_remaining: z.number().int().min(0).max(255).nullable(),
        saved_cycle_outcomes: z
          .array(
            z
              .object({
                kind: text,
                last_attempt_date: date,
                state: text,
                attempt_count: count,
                retries_remaining: z.number().int().min(0).max(255),
              })
              .strict(),
          )
          .max(8192),
        notification_sent: z.literal(false),
        message: text,
      })
      .strict(),
    pending_confirmations: z.array(pending).max(8192),
  })
  .strict();
export const subscriptionOriginalSchema = z
  .object({ fictional: z.literal(true), status: text, message: text })
  .strict();
export type SubscriptionDetailView = z.infer<typeof subscriptionDetailSchema>;
export type SubscriptionOriginalView = z.infer<typeof subscriptionOriginalSchema>;
export async function subscriptionDetails(selection_ref: string, signal?: AbortSignal) {
  const parsed = subscriptionDetailSchema.safeParse(
    await request('/api/subscriptions', 'POST', { kind: 'DETAILS', selection_ref }, signal),
  );
  if (!parsed.success) throw new RuntimeTransportError('INVALID_RESPONSE');
  return parsed.data;
}
export async function subscriptionOriginal(original_ref: string, signal?: AbortSignal) {
  const parsed = subscriptionOriginalSchema.safeParse(
    await request('/api/subscriptions', 'POST', { kind: 'ORIGINAL', original_ref }, signal),
  );
  if (!parsed.success) throw new RuntimeTransportError('INVALID_RESPONSE');
  return parsed.data;
}
export async function subscriptionContext(signal?: AbortSignal) {
  const result = contextSchema.safeParse(
    await request('/api/subscriptions/session', 'GET', undefined, signal),
  );
  if (!result.success) throw new RuntimeTransportError('INVALID_RESPONSE');
  return result.data.context_ref;
}
export async function subscriptionRead(signal?: AbortSignal) {
  const result = subscriptionReadSchema.safeParse(
    await request('/api/subscriptions', 'POST', { kind: 'LIST' }, signal),
  );
  if (!result.success) throw new RuntimeTransportError('INVALID_RESPONSE');
  return result.data;
}
