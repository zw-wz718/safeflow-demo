import { request, RuntimeTransportError } from './client';
import { draftConfirmationSchema, type DraftConfirmationView } from './draft-schema';

function parseDraft(value: unknown, expectedTaskId?: string): DraftConfirmationView {
  const parsed = draftConfirmationSchema.safeParse(value);
  if (!parsed.success || (expectedTaskId && parsed.data.task_id !== expectedTaskId)) {
    throw new RuntimeTransportError('INVALID_RESPONSE');
  }
  return parsed.data;
}

export async function createDraftRequest(signal?: AbortSignal) {
  return parseDraft(await request('/api/sim/draft-confirmations', 'POST', {}, signal));
}

export async function getDraftRequest(taskId: string, signal?: AbortSignal) {
  return parseDraft(
    await request(
      `/api/sim/draft-confirmations/${encodeURIComponent(taskId)}`,
      'GET',
      undefined,
      signal,
    ),
    taskId,
  );
}

export async function prepareDraftRequest(taskId: string, signal?: AbortSignal) {
  return parseDraft(
    await request(
      `/api/sim/draft-confirmations/${encodeURIComponent(taskId)}/prepare`,
      'POST',
      {},
      signal,
    ),
    taskId,
  );
}

export async function recordDraftConfirmation(
  view: DraftConfirmationView,
  action: 'CONFIRM' | 'REJECT',
  signal?: AbortSignal,
) {
  view = parseDraft(view);
  if (
    view.next_action.kind !== 'CONFIRM' ||
    !view.operation_id ||
    !view.confirmation ||
    !view.display_ticket
  ) {
    throw new RuntimeTransportError('INVALID_RESPONSE');
  }
  const recorded = parseDraft(
    await request(
      `/api/operations/${encodeURIComponent(view.operation_id)}/confirm`,
      'POST',
      {
        scope: 'MOCK_DRAFT_CONFIRMATION_ONLY',
        task_id: view.task_id,
        confirmation_ref: view.confirmation.confirmation_ref,
        display_ticket: view.display_ticket,
        action,
      },
      signal,
    ),
    view.task_id,
  );
  if (recorded.operation_id !== view.operation_id)
    throw new RuntimeTransportError('INVALID_RESPONSE');
  return recorded;
}
