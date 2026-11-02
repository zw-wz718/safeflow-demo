import { request, RuntimeTransportError } from './client';
import { plannerStatusSchema, planningSchema, type PlanningView } from './planning-schema';
import type { TaskResponse } from './schema';

const planningTimeoutMs = 135000;

function parsePlanning(value: unknown, expectedTaskId?: string): PlanningView {
  const parsed = planningSchema.safeParse(value);
  if (
    !parsed.success ||
    (expectedTaskId && (parsed.data.kind !== 'TASK' || parsed.data.task.task_id !== expectedTaskId))
  ) {
    throw new RuntimeTransportError('INVALID_RESPONSE');
  }
  return parsed.data;
}

export async function getPlannerStatus(signal?: AbortSignal) {
  const parsed = plannerStatusSchema.safeParse(
    await request('/api/llm/status', 'GET', undefined, signal),
  );
  if (!parsed.success) throw new RuntimeTransportError('INVALID_RESPONSE');
  return parsed.data;
}

export async function submitPlanning(message: string, signal?: AbortSignal) {
  if (!message.trim() || new TextEncoder().encode(message).byteLength > 4096) {
    throw new RuntimeTransportError('INVALID_RESPONSE');
  }
  const accepted = parsePlanning(
    await request('/api/llm/plans', 'POST', { message }, signal, planningTimeoutMs),
  );
  if (accepted.kind === 'TASK' && !accepted.can_start)
    throw new RuntimeTransportError('INVALID_RESPONSE');
  return accepted;
}

export async function getPlanning(taskId: string, signal?: AbortSignal) {
  return parsePlanning(
    await request(`/api/llm/plans/${encodeURIComponent(taskId)}`, 'GET', undefined, signal),
    taskId,
  );
}

export async function executePlanning(view: PlanningView, signal?: AbortSignal) {
  view = parsePlanning(view);
  if (view.kind !== 'TASK' || !view.can_start) throw new RuntimeTransportError('INVALID_RESPONSE');
  const task: TaskResponse = view.task;
  const executed = parsePlanning(
    await request(
      `/api/llm/plans/${encodeURIComponent(task.task_id)}/execute`,
      'POST',
      {
        task_revision: task.revision,
        plan_version: task.plan_version,
        facts_version: task.facts_version,
      },
      signal,
    ),
    task.task_id,
  );
  if (executed.can_start) throw new RuntimeTransportError('INVALID_RESPONSE');
  return executed;
}
