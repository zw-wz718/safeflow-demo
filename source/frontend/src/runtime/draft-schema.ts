import { z } from 'zod';

const id = z.string().min(1);
const revision = z.number().int().min(1).max(Number.MAX_SAFE_INTEGER);
const version = z.number().int().min(1).max(4294967295);
const timestamp = z.string().datetime({ offset: true });

const confirmationSchema = z
  .object({
    confirmation_ref: id,
    status: z.enum(['PREPARED', 'CONFIRMED', 'REVOKED']),
    prepared_at: timestamp,
    expires_at: timestamp,
    confirmed_at: timestamp.nullable(),
    revoked_at: timestamp.nullable(),
  })
  .strict();

const preflightSchema = z
  .object({
    evidence_kind: z.literal('PREFLIGHT_ONLY'),
    pre_tool_outcome: z.literal('READY_TO_CONFIRM'),
    pre_auth_outcome: z.literal('READY_TO_CONFIRM'),
    required_gates: z.tuple([z.literal('EXPLICIT_CONFIRMATION')]),
  })
  .strict();

export const draftConfirmationSchema = z
  .object({
    contract_version: z.literal(1),
    mode: z.literal('PROJECT_SIMULATION'),
    scope: z.literal('MOCK_DRAFT_CONFIRMATION_ONLY'),
    task_id: id,
    task_state: z.literal('RUNNING'),
    operation_id: id.nullable(),
    operation_state: z.literal('DRAFT').nullable(),
    task_revision: revision,
    operation_revision: revision.nullable(),
    plan_version: version,
    facts_version: version,
    parameters: z
      .object({
        payer_account_id: id,
        payee_id: id,
        amount: z.literal('200.00'),
        currency: z.literal('CNY'),
        remark: z.string(),
        replaces: z.null(),
      })
      .strict(),
    confirmation: confirmationSchema.nullable(),
    preflight: preflightSchema.nullable(),
    display_ticket: id.nullable(),
    next_action: z.discriminatedUnion('kind', [
      z.object({ kind: z.literal('PREPARE') }).strict(),
      z.object({ kind: z.literal('CONFIRM') }).strict(),
      z.object({ kind: z.literal('NONE') }).strict(),
    ]),
  })
  .strict()
  .superRefine((view, context) => {
    const invalid = (message: string) => context.addIssue({ code: z.ZodIssueCode.custom, message });
    const hasOperation = view.operation_id !== null;
    if (
      hasOperation !== (view.operation_state !== null) ||
      hasOperation !== (view.operation_revision !== null)
    ) {
      invalid('Incomplete operation identity');
    }
    if (view.next_action.kind === 'PREPARE') {
      if (view.confirmation || view.preflight || view.display_ticket)
        invalid('Preparation requires a fresh explicit request');
      return;
    }
    if (!hasOperation || !view.confirmation || !view.preflight) {
      invalid('Confirmation evidence is missing');
      return;
    }
    if (view.next_action.kind === 'CONFIRM') {
      if (
        view.confirmation.status !== 'PREPARED' ||
        !view.display_ticket ||
        view.confirmation.confirmed_at ||
        view.confirmation.revoked_at
      ) {
        invalid('No current confirmation ticket');
      }
    } else if (view.display_ticket || view.confirmation.status === 'PREPARED') {
      invalid('Inactive confirmation cannot retain a ticket');
    }
    if (
      view.confirmation.status === 'CONFIRMED' &&
      (!view.confirmation.confirmed_at || view.confirmation.revoked_at)
    ) {
      invalid('Confirmed request requires a recorded time');
    }
    if (view.confirmation.status === 'REVOKED' && !view.confirmation.revoked_at) {
      invalid('Revoked request requires a recorded time');
    }
  });

export type DraftConfirmationView = z.infer<typeof draftConfirmationSchema>;
