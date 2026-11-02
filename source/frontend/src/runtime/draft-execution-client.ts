import { request, RuntimeApiError, RuntimeTransportError } from './client';
import { draftConfirmationSchema, type DraftConfirmationView } from './draft-schema';
import {
  draftExecutionReferenceSchema,
  draftExecutionRequestSchema,
  draftExecutionSchema,
  type DraftExecutionReference,
  type DraftExecutionView,
} from './draft-execution-schema';

function parseExecution(value: unknown, reference: DraftExecutionReference): DraftExecutionView {
  const parsed = draftExecutionSchema.safeParse(value);
  if (
    !parsed.success ||
    parsed.data.task_id !== reference.task_id ||
    parsed.data.operation_id !== reference.operation_id
  ) {
    throw new RuntimeTransportError('INVALID_RESPONSE');
  }
  return parsed.data;
}

async function requestOriginal(
  path: string,
  method: 'GET' | 'POST',
  body: unknown,
  reference: DraftExecutionReference,
  signal?: AbortSignal,
) {
  try {
    return parseExecution(await request(path, method, body, signal), reference);
  } catch (error) {
    if (
      error instanceof RuntimeApiError &&
      error.reason !== 'NOT_AUTHENTICATED' &&
      error.taskId &&
      error.taskId !== reference.task_id
    )
      throw new RuntimeTransportError('INVALID_RESPONSE');
    throw error;
  }
}

export async function getDraftExecution(reference: DraftExecutionReference, signal?: AbortSignal) {
  const parsed = draftExecutionReferenceSchema.safeParse(reference);
  if (!parsed.success) throw new RuntimeTransportError('INVALID_RESPONSE');
  return requestOriginal(
    `/api/sim/draft-executions/${encodeURIComponent(reference.task_id)}/operations/${encodeURIComponent(reference.operation_id)}`,
    'GET',
    undefined,
    reference,
    signal,
  );
}

export async function executeConfirmedDraft(view: DraftConfirmationView, signal?: AbortSignal) {
  const parsed = draftConfirmationSchema.safeParse(view);
  if (
    !parsed.success ||
    parsed.data.confirmation?.status !== 'CONFIRMED' ||
    parsed.data.next_action.kind !== 'NONE' ||
    !parsed.data.operation_id ||
    !parsed.data.operation_revision
  ) {
    throw new RuntimeTransportError('INVALID_RESPONSE');
  }
  const original = parsed.data;
  const body = draftExecutionRequestSchema.parse({
    scope: 'MOCK_DRAFT_EXECUTION_ONLY',
    operation_id: original.operation_id,
    confirmation_ref: original.confirmation!.confirmation_ref,
    task_revision: original.task_revision,
    operation_revision: original.operation_revision,
    plan_version: original.plan_version,
    facts_version: original.facts_version,
  });
  return requestOriginal(
    `/api/sim/draft-confirmations/${encodeURIComponent(original.task_id)}/execute`,
    'POST',
    body,
    { task_id: original.task_id, operation_id: original.operation_id! },
    signal,
  );
}
