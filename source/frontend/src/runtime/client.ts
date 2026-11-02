import {
  errorSchema,
  sessionSchema,
  taskSchema,
  type ErrorReason,
  type TaskResponse,
} from './schema';

const requestTimeoutMs = 8000;
type RequestStage = 'directory' | 'original' | 'model' | 'query' | 'session' | 'operation';
export type RequestTiming = {
  stage: RequestStage;
  phase: 'start' | 'end';
  durationMs?: number;
  responseMs?: number;
  bodyMs?: number;
  status?: number;
  serverTiming?: string;
};
const observers = new Set<(event: RequestTiming) => void>();
const sessionObservers = new Set<() => void>();
export function observeRequests(observer: (event: RequestTiming) => void) {
  observers.add(observer);
  return () => {
    observers.delete(observer);
  };
}
export function observeSessionExpired(observer: () => void) {
  sessionObservers.add(observer);
  return () => {
    sessionObservers.delete(observer);
  };
}
export function notifySessionExpired() {
  for (const observer of sessionObservers) {
    try {
      observer();
    } catch {
      /* A view observer cannot change HTTP outcomes. */
    }
  }
}
function stageFor(path: string, method: string): RequestStage {
  if (path === '/api/bank/recovery') return 'directory';
  if (path === '/api/bank/conversation' && method === 'POST') return 'model';
  if (path.includes('/session')) return 'session';
  if (method === 'GET' && /draft-lifecycles|payment-preparations|agent\/tasks/.test(path))
    return 'original';
  return method === 'GET' ? 'query' : 'operation';
}
function report(event: RequestTiming) {
  for (const observer of observers) {
    try {
      observer(event);
    } catch {
      /* Diagnostics cannot change HTTP outcomes. */
    }
  }
}

export class RuntimeApiError extends Error {
  constructor(
    public readonly reason: ErrorReason,
    public readonly taskId: string | null,
    public readonly nextAction: 'NONE' | 'QUERY_ORIGINAL_TASK',
    public readonly status: number,
    public readonly validated = true,
  ) {
    super(reason);
  }
}

export class RuntimeTransportError extends Error {
  constructor(public readonly reason: 'TIMEOUT' | 'UNAVAILABLE' | 'INVALID_RESPONSE' | 'ABORTED') {
    super(reason);
  }
}

export async function request(
  path: string,
  method: 'GET' | 'POST' | 'DELETE',
  body?: unknown,
  signal?: AbortSignal,
  timeoutMs = requestTimeoutMs,
) {
  const began = performance.now(),
    stage = stageFor(path, method);
  let responseMs: number | undefined,
    bodyMs: number | undefined,
    status: number | undefined,
    serverTiming: string | undefined;
  report({ stage, phase: 'start' });
  const controller = new AbortController();
  let timedOut = false;
  const abort = () => controller.abort();
  signal?.addEventListener('abort', abort, { once: true });
  if (signal?.aborted) controller.abort();
  const timeout = setTimeout(() => {
    timedOut = true;
    controller.abort();
  }, timeoutMs);
  try {
    const response = await fetch(path, {
      method,
      credentials: 'include',
      cache: 'no-store',
      signal: controller.signal,
      ...(method !== 'GET'
        ? {
            headers: { 'Content-Type': 'application/json', 'X-SafeFlow-Request': '1' },
            body: JSON.stringify(body),
          }
        : {}),
    });
    responseMs = performance.now() - began;
    status = response.status;
    // Only allow fixed metric names and numeric durations; never relay arbitrary
    // upstream header strings, URLs, identifiers or provider diagnostics.
    serverTiming =
      response.headers
        ?.get?.('Server-Timing')
        ?.split(',')
        .map((v) => v.trim())
        .filter((v) => /^(model|plan|tools|total);dur=\d+(?:\.\d+)?$/.test(v))
        .join(', ') || undefined;
    if (response.status === 401) notifySessionExpired();
    if (method === 'DELETE' && response.status === 204) return null;
    let value: unknown;
    try {
      value = await response.json();
      bodyMs = performance.now() - began - responseMs;
    } catch {
      if (response.status === 401) {
        throw new RuntimeApiError('NOT_AUTHENTICATED', null, 'NONE', 401, false);
      }
      throw new RuntimeTransportError('INVALID_RESPONSE');
    }
    if (!response.ok) {
      const parsed = errorSchema.safeParse(value);
      if (response.status === 401) {
        throw new RuntimeApiError(
          'NOT_AUTHENTICATED',
          parsed.success ? parsed.data.error.task_id : null,
          parsed.success ? parsed.data.error.next_action : 'NONE',
          401,
          parsed.success,
        );
      }
      if (!parsed.success) throw new RuntimeTransportError('INVALID_RESPONSE');
      throw new RuntimeApiError(
        parsed.data.error.reason_code,
        parsed.data.error.task_id,
        parsed.data.error.next_action,
        response.status,
      );
    }
    return value;
  } catch (error) {
    if (timedOut) throw new RuntimeTransportError('TIMEOUT');
    if (signal?.aborted) throw new RuntimeTransportError('ABORTED');
    if (error instanceof RuntimeApiError || error instanceof RuntimeTransportError) throw error;
    throw new RuntimeTransportError('UNAVAILABLE');
  } finally {
    report({
      stage,
      phase: 'end',
      durationMs: performance.now() - began,
      responseMs,
      bodyMs,
      status,
      serverTiming,
    });
    clearTimeout(timeout);
    signal?.removeEventListener('abort', abort);
  }
}

function parseTask(value: unknown): TaskResponse {
  const parsed = taskSchema.safeParse(value);
  if (!parsed.success) throw new RuntimeTransportError('INVALID_RESPONSE');
  return parsed.data;
}

function parseSession(value: unknown) {
  const parsed = sessionSchema.safeParse(value);
  if (!parsed.success) throw new RuntimeTransportError('INVALID_RESPONSE');
  return parsed.data;
}

export async function getSession(signal?: AbortSignal) {
  return parseSession(await request('/api/sim/session', 'GET', undefined, signal));
}

export async function login(loginSecret: string, signal?: AbortSignal) {
  return parseSession(
    await request('/api/sim/session', 'POST', { login_secret: loginSecret }, signal),
  );
}

export async function logout(signal?: AbortSignal) {
  const value = await request('/api/sim/session', 'DELETE', {}, signal);
  if (value !== null) throw new RuntimeTransportError('INVALID_RESPONSE');
}

export async function getTask(taskId: string, signal?: AbortSignal) {
  const task = parseTask(
    await request(`/api/tasks/${encodeURIComponent(taskId)}`, 'GET', undefined, signal),
  );
  if (task.task_id !== taskId) throw new RuntimeTransportError('INVALID_RESPONSE');
  return task;
}

export async function createTask(signal?: AbortSignal) {
  return parseTask(await request('/api/chat', 'POST', { kind: 'NEW_READ_ONLY_DEMO' }, signal));
}

export async function continueTask(task: TaskResponse, signal?: AbortSignal) {
  if (task.task_state !== 'WAITING_USER' || task.next_action.kind !== 'CONTINUE') {
    throw new RuntimeTransportError('INVALID_RESPONSE');
  }
  const latest = parseTask(
    await request(
      '/api/chat',
      'POST',
      {
        kind: 'CONTINUE',
        task_id: task.task_id,
        waiting_node_id: task.next_action.node_id,
        task_revision: task.revision,
        plan_version: task.plan_version,
        facts_version: task.facts_version,
      },
      signal,
    ),
  );
  if (latest.task_id !== task.task_id) throw new RuntimeTransportError('INVALID_RESPONSE');
  return latest;
}
