import { z } from 'zod';

const id = z.string().min(1);
const revision = z.number().int().min(1).max(Number.MAX_SAFE_INTEGER);
const version = z.number().int().min(1).max(4294967295);

export const taskStates = [
  'RUNNING',
  'WAITING_USER',
  'WAITING_EXTERNAL',
  'COMPLETED',
  'FAILED',
  'CANCELLED',
] as const;

const taskState = z.enum(taskStates);
const recovery = z.discriminatedUnion('kind', [
  z.object({ kind: z.literal('AWAIT_USER'), node_id: id }).strict(),
  z.object({ kind: z.literal('REVIEW_NODE'), node_id: id }).strict(),
  z.object({ kind: z.literal('RECONCILE_ORIGINAL'), operation_id: id }).strict(),
  z.object({ kind: z.literal('REVIEW_RECORDED_RESULT'), operation_id: id }).strict(),
  z.object({ kind: z.literal('FINISHED'), task_state: taskState }).strict(),
]);

const nextAction = z.discriminatedUnion('kind', [
  z.object({ kind: z.literal('CONTINUE'), node_id: id }).strict(),
  z.object({ kind: z.literal('REVIEW_ORIGINAL_TASK') }).strict(),
  z.object({ kind: z.literal('FINISHED') }).strict(),
]);

export const taskSchema = z
  .object({
    contract_version: z.literal(1),
    mode: z.literal('PROJECT_SIMULATION'),
    task_id: id,
    task_state: taskState,
    revision,
    plan_version: version,
    facts_version: version,
    operation_state: z.null(),
    security_outcome: z.null(),
    security_scope: z.literal('CURRENT_DECISION_NOT_PROVIDED'),
    operations: z.tuple([]),
    recovery,
    next_action: nextAction,
    replayed: z.boolean(),
  })
  .strict()
  .superRefine((task, context) => {
    if (
      task.next_action.kind === 'CONTINUE' &&
      (task.task_state !== 'WAITING_USER' ||
        task.recovery.kind !== 'AWAIT_USER' ||
        task.recovery.node_id !== task.next_action.node_id)
    ) {
      context.addIssue({ code: z.ZodIssueCode.custom, message: 'Invalid continuation state' });
    }
    if (task.recovery.kind === 'FINISHED' && task.recovery.task_state !== task.task_state) {
      context.addIssue({ code: z.ZodIssueCode.custom, message: 'Inconsistent finished state' });
    }
    if (
      (task.recovery.kind === 'FINISHED' || task.next_action.kind === 'FINISHED') &&
      !['COMPLETED', 'FAILED', 'CANCELLED'].includes(task.task_state)
    ) {
      context.addIssue({
        code: z.ZodIssueCode.custom,
        message: 'Task has not reached a terminal state',
      });
    }
  });

export const sessionSchema = z
  .object({ mode: z.literal('PROJECT_SIMULATION'), authenticated: z.boolean() })
  .strict();

const errorReason = z.enum([
  'VALIDATION_ERROR',
  'NOT_AUTHENTICATED',
  'FORBIDDEN',
  'NOT_FOUND',
  'STALE_PLAN',
  'IDEMPOTENCY_CONFLICT',
  'DEPENDENCY_UNAVAILABLE',
  'LLM_NOT_CONFIGURED',
  'LLM_GATEWAY_UNAVAILABLE',
  'LLM_PROPOSAL_REJECTED',
  'RECONCILE_REQUIRED',
]);

export const errorSchema = z
  .object({
    contract_version: z.literal(1),
    error: z
      .object({
        reason_code: errorReason,
        message: z.string().min(1),
        task_id: id.nullable(),
        next_action: z.enum(['NONE', 'QUERY_ORIGINAL_TASK']),
      })
      .strict(),
  })
  .strict()
  .superRefine((response, context) => {
    if (response.error.next_action === 'QUERY_ORIGINAL_TASK' && !response.error.task_id) {
      context.addIssue({ code: z.ZodIssueCode.custom, message: 'Original task id is required' });
    }
  });

export type TaskResponse = z.infer<typeof taskSchema>;
export type TaskState = z.infer<typeof taskState>;
export type ErrorReason = z.infer<typeof errorReason>;
