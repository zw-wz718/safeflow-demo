import { z } from 'zod';
import { taskSchema } from './schema';

const shared = {
  contract_version: z.literal(1),
  mode: z.literal('PROJECT_SIMULATION'),
  planner_source: z.literal('OPENAI_COMPATIBLE'),
};

export const plannerStatusSchema = z
  .object({
    ...shared,
    configured: z.boolean(),
    connectivity: z.literal('NOT_VERIFIED'),
    reason_code: z.literal('LLM_NOT_CONFIGURED').nullable(),
    tool_scope: z.literal('READ_ONLY_BALANCE'),
  })
  .strict()
  .superRefine((status, context) => {
    if (
      (status.configured && status.reason_code !== null) ||
      (!status.configured && status.reason_code !== 'LLM_NOT_CONFIGURED')
    ) {
      context.addIssue({
        code: z.ZodIssueCode.custom,
        message: 'Inconsistent planner configuration status',
      });
    }
  });

export const planningSchema = z
  .discriminatedUnion('kind', [
    z
      .object({
        ...shared,
        kind: z.literal('TASK'),
        task: taskSchema,
        clarification: z.null(),
        can_start: z.boolean(),
      })
      .strict(),
    z
      .object({
        ...shared,
        kind: z.literal('CLARIFY'),
        task: z.null(),
        clarification: z.object({ question: z.string().min(1) }).strict(),
        can_start: z.literal(false),
      })
      .strict(),
  ])
  .superRefine((view, context) => {
    if (
      view.kind === 'TASK' &&
      view.can_start &&
      (view.task.task_state !== 'RUNNING' ||
        view.task.next_action.kind !== 'REVIEW_ORIGINAL_TASK' ||
        view.task.recovery.kind !== 'REVIEW_NODE')
    ) {
      context.addIssue({
        code: z.ZodIssueCode.custom,
        message: 'Inconsistent initial read-only plan',
      });
    }
  });

export type PlannerStatus = z.infer<typeof plannerStatusSchema>;
export type PlanningView = z.infer<typeof planningSchema>;
