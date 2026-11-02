import { z } from 'zod';

const id = z.string().min(1);
const revision = z.number().int().min(1).max(Number.MAX_SAFE_INTEGER);
const version = z.number().int().min(1).max(4294967295);

export const draftExecutionReferenceSchema = z.object({ task_id: id, operation_id: id }).strict();

export const draftExecutionRequestSchema = z
  .object({
    scope: z.literal('MOCK_DRAFT_EXECUTION_ONLY'),
    operation_id: id,
    confirmation_ref: id,
    task_revision: revision,
    operation_revision: revision,
    plan_version: version,
    facts_version: version,
  })
  .strict();

export const draftExecutionSchema = z
  .object({
    contract_version: z.literal(1),
    mode: z.literal('PROJECT_SIMULATION'),
    scope: z.literal('MOCK_DRAFT_EXECUTION_ONLY'),
    task_id: id,
    task_state: z.enum(['RUNNING', 'COMPLETED']),
    operation_id: id,
    operation_state: z.enum(['DRAFT', 'SUCCEEDED']),
    task_revision: revision,
    operation_revision: revision,
    plan_version: version,
    facts_version: version,
    execution_status: z.enum(['COMPLETED', 'REQUIRES_ORIGINAL_OPERATION_REVIEW']),
    receipt_scope: z.literal('HISTORICAL_EXECUTION_RECEIPT'),
    current_draft_verified: z.literal(false),
    receipt: z
      .object({
        draft_id: id,
        business_operation_id: id,
        draft_version: version,
        tool_result_ref: id,
        executed_at: z
          .string()
          .datetime({ offset: true })
          .refine((value) => /(?:Z|\+00:00)$/.test(value)),
      })
      .strict()
      .nullable(),
    replayed: z.boolean(),
    next_action: z.discriminatedUnion('kind', [
      z.object({ kind: z.literal('NONE') }).strict(),
      z.object({ kind: z.literal('QUERY_ORIGINAL_OPERATION') }).strict(),
    ]),
  })
  .strict()
  .superRefine((view, context) => {
    const completed = view.execution_status === 'COMPLETED';
    if (
      completed
        ? view.task_state !== 'COMPLETED' ||
          view.operation_state !== 'SUCCEEDED' ||
          !view.receipt ||
          view.next_action.kind !== 'NONE'
        : view.task_state !== 'RUNNING' ||
          view.operation_state !== 'DRAFT' ||
          view.receipt !== null ||
          view.next_action.kind !== 'QUERY_ORIGINAL_OPERATION'
    ) {
      context.addIssue({
        code: z.ZodIssueCode.custom,
        message: 'Execution status and historical receipt disagree',
      });
    }
  });

export type DraftExecutionReference = z.infer<typeof draftExecutionReferenceSchema>;
export type DraftExecutionView = z.infer<typeof draftExecutionSchema>;
