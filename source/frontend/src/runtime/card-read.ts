import { z } from 'zod';
import { request, RuntimeTransportError } from './client';

const text = z.string().max(2000);
const position = z.number().int().min(1).max(8192);
const tail = z.string().regex(/^\d{4}$/);
export const cardReadSchema = z
  .object({
    contract_version: z.literal(1),
    mode: z.literal('PROJECT_SIMULATION'),
    scope: z.literal('CARD_READ_ONLY'),
    step_ref: z.string().uuid(),
    funds_authorized: z.literal(false),
    feedback: z
      .object({
        state: z.enum(['STOP_MINE', 'STOP_UNSURE', 'STOP_NO_CARD', 'CARD_ASSOCIATION_VERIFIED']),
        message: text,
        scope_explanation: text,
      })
      .strict()
      .optional(),
    cards: z
      .array(
        z
          .object({
            position,
            last_four: tail,
            ambiguous_tail: z.boolean(),
            card_locked: z.boolean(),
            public_reasons: z.array(text).max(8192),
          })
          .strict(),
      )
      .max(4096),
    account_outgoing_restricted: z.boolean(),
    account_public_reasons: z.array(text).max(8192),
    history: z
      .object({
        last_four: tail,
        card_locked: z.boolean(),
        account_outgoing_restricted: z.boolean(),
        message: text,
        ordering_explanation: text,
        locks: z
          .array(
            z
              .object({
                position,
                active: z.boolean(),
                public_reason: text,
                records: z
                  .array(
                    z.object({ position, original_action: text, public_reason: text }).strict(),
                  )
                  .max(8192),
              })
              .strict(),
          )
          .max(8192),
      })
      .strict()
      .nullable(),
    original: z
      .object({
        state: z.enum(['RECORDED_ACTIVE', 'RECORDED_INACTIVE', 'NEEDS_REVIEW', 'UNAVAILABLE']),
        last_four: tail.nullable(),
        account_outgoing_restricted: z.boolean().nullable(),
        public_reasons: z.array(text).max(8192),
        message: text,
        original_action: text.nullable().optional(),
      })
      .strict()
      .nullable(),
    unlock_instruction: text,
  })
  .strict()
  .superRefine((v, ctx) => {
    const unique = (a: number[]) => new Set(a).size === a.length;
    if (
      !unique(v.cards.map((c) => c.position)) ||
      (v.history && !unique(v.history.locks.flatMap((l) => l.records.map((r) => r.position))))
    )
      ctx.addIssue({ code: 'custom', message: 'Ambiguous positions' });
    if (v.original && !v.history)
      ctx.addIssue({ code: 'custom', message: 'Missing selected history' });
    if (v.feedback) {
      if (v.cards.length !== 0)
        ctx.addIssue({ code: 'custom', message: 'Feedback cannot select another card' });
      if (
        v.feedback.state === 'CARD_ASSOCIATION_VERIFIED' ? !v.history : !!(v.history || v.original)
      )
        ctx.addIssue({ code: 'custom', message: 'Invalid feedback association' });
      if (
        v.original &&
        (v.original.original_action === undefined ||
          (v.original.state === 'NEEDS_REVIEW' && v.original.original_action !== null) ||
          (['RECORDED_ACTIVE', 'RECORDED_INACTIVE'].includes(v.original.state) &&
            (v.original.original_action === null || v.original.original_action === '')))
      )
        ctx.addIssue({ code: 'custom', message: 'Invalid current original action' });
    } else if (v.original?.original_action !== undefined) {
      ctx.addIssue({ code: 'custom', message: 'Unexpected feedback original action' });
    }
  });
export type CardReadView = z.infer<typeof cardReadSchema>;
export const contextSchema = z
  .object({
    authenticated: z.boolean(),
    context_ref: z
      .string()
      .regex(/^[a-f0-9]{64}$/)
      .nullable(),
  })
  .strict()
  .refine((v) => v.authenticated === (v.context_ref !== null));
export async function cardContext(signal?: AbortSignal) {
  const v = contextSchema.safeParse(
    await request('/api/card-read/session', 'GET', undefined, signal),
  );
  if (!v.success) throw new RuntimeTransportError('INVALID_RESPONSE');
  return v.data.context_ref;
}
export async function cardRead(input: unknown, signal?: AbortSignal) {
  const v = cardReadSchema.safeParse(await request('/api/card-read', 'POST', input, signal));
  if (!v.success) throw new RuntimeTransportError('INVALID_RESPONSE');
  return v.data;
}
// One epoch covers every async success/failure including context rechecks.
// No persistent storage and no restoration of selection after mount/restart.
export class CardReadEpoch {
  private version = 0;
  private controller?: AbortController;
  next() {
    this.controller?.abort();
    const controller = new AbortController();
    this.controller = controller;
    const version = ++this.version;
    return {
      signal: controller.signal,
      current: () => this.version === version && !controller.signal.aborted,
    };
  }
  invalidate() {
    this.version++;
    this.controller?.abort();
  }
}
