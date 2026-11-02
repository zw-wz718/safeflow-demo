import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  continueTask,
  createTask,
  getSession,
  getTask,
  login,
  logout,
  RuntimeApiError,
} from './client';
import { taskSchema } from './schema';

const waiting = taskSchema.parse({
  contract_version: 1,
  mode: 'PROJECT_SIMULATION',
  task_id: 'task_123',
  task_state: 'WAITING_USER',
  revision: 4,
  plan_version: 2,
  facts_version: 7,
  operation_state: null,
  security_outcome: null,
  security_scope: 'CURRENT_DECISION_NOT_PROVIDED',
  operations: [],
  recovery: { kind: 'AWAIT_USER', node_id: 'choose_two' },
  next_action: { kind: 'CONTINUE', node_id: 'choose_two' },
  replayed: false,
});
const jsonResponse = (value: unknown, status = 200) =>
  new Response(JSON.stringify(value), { status, headers: { 'Content-Type': 'application/json' } });

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe('runtime HTTP client boundaries', () => {
  it('sends only the approved login field and keeps authentication in cookies', async () => {
    const fetch = vi
      .fn()
      .mockResolvedValue(jsonResponse({ mode: 'PROJECT_SIMULATION', authenticated: true }));
    vi.stubGlobal('fetch', fetch);
    await login('test-only-login-secret');
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(fetch.mock.calls[0][0]).toBe('/api/sim/session');
    expect(fetch.mock.calls[0][1]).toMatchObject({
      method: 'POST',
      credentials: 'include',
      cache: 'no-store',
      headers: { 'Content-Type': 'application/json', 'X-SafeFlow-Request': '1' },
      body: JSON.stringify({ login_secret: 'test-only-login-secret' }),
    });
  });

  it('creates the fixed read-only plan without customer parameters', async () => {
    const fetch = vi.fn().mockResolvedValue(jsonResponse(waiting));
    vi.stubGlobal('fetch', fetch);
    await createTask();
    expect(JSON.parse(fetch.mock.calls[0][1].body)).toEqual({ kind: 'NEW_READ_ONLY_DEMO' });
  });

  it('continues with the precise current node and versions, and uses latest replay state', async () => {
    const completed = {
      ...waiting,
      task_state: 'COMPLETED',
      revision: 6,
      recovery: { kind: 'FINISHED', task_state: 'COMPLETED' },
      next_action: { kind: 'FINISHED' },
      replayed: true,
    };
    const fetch = vi.fn().mockResolvedValue(jsonResponse(completed));
    vi.stubGlobal('fetch', fetch);
    expect((await continueTask(waiting)).task_state).toBe('COMPLETED');
    expect(JSON.parse(fetch.mock.calls[0][1].body)).toEqual({
      kind: 'CONTINUE',
      task_id: 'task_123',
      waiting_node_id: 'choose_two',
      task_revision: 4,
      plan_version: 2,
      facts_version: 7,
    });
  });

  it('does not retry a POST when the response is lost', async () => {
    const fetch = vi.fn().mockRejectedValue(new TypeError('Network unavailable'));
    vi.stubGlobal('fetch', fetch);
    await expect(createTask()).rejects.toMatchObject({ reason: 'UNAVAILABLE' });
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it('times out without retrying the creation request', async () => {
    vi.useFakeTimers();
    const fetch = vi.fn(
      (_path, options: RequestInit) =>
        new Promise((_resolve, reject) => {
          options.signal?.addEventListener('abort', () =>
            reject(new DOMException('Aborted', 'AbortError')),
          );
        }),
    );
    vi.stubGlobal('fetch', fetch);
    const result = expect(createTask()).rejects.toMatchObject({ reason: 'TIMEOUT' });
    await vi.advanceTimersByTimeAsync(8000);
    await result;
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it('treats every HTTP 401 as an invalid session even when the body is malformed', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response('not-json', { status: 401 })));
    await expect(getSession()).rejects.toMatchObject({
      reason: 'NOT_AUTHENTICATED',
      status: 401,
      validated: false,
    });
  });

  it('preserves a recorded original task id when creation returns a valid HTTP 401', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(
        jsonResponse(
          {
            contract_version: 1,
            error: {
              reason_code: 'NOT_AUTHENTICATED',
              message: 'Simulation session expired.',
              task_id: 'created_before_expiry',
              next_action: 'QUERY_ORIGINAL_TASK',
            },
          },
          401,
        ),
      ),
    );
    await expect(createTask()).rejects.toMatchObject({
      reason: 'NOT_AUTHENTICATED',
      taskId: 'created_before_expiry',
      nextAction: 'QUERY_ORIGINAL_TASK',
      status: 401,
    });
  });

  it('keeps a dependency error distinct from a task failure', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(
        jsonResponse(
          {
            contract_version: 1,
            error: {
              reason_code: 'DEPENDENCY_UNAVAILABLE',
              message: 'Dependency unavailable.',
              task_id: 'task_123',
              next_action: 'QUERY_ORIGINAL_TASK',
            },
          },
          503,
        ),
      ),
    );
    const error = await createTask().catch((error: unknown) => error);
    expect(error).toBeInstanceOf(RuntimeApiError);
    expect(error).toMatchObject({
      reason: 'DEPENDENCY_UNAVAILABLE',
      taskId: 'task_123',
      nextAction: 'QUERY_ORIGINAL_TASK',
    });
  });

  it('rejects a mismatched query task id', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue(jsonResponse({ ...waiting, task_id: 'other_task' })),
    );
    await expect(getTask('task_123')).rejects.toMatchObject({ reason: 'INVALID_RESPONSE' });
  });

  it('requires 204 logout with the approved request header and empty JSON object', async () => {
    const fetch = vi.fn().mockResolvedValue(new Response(null, { status: 204 }));
    vi.stubGlobal('fetch', fetch);
    await logout();
    expect(fetch.mock.calls[0][1]).toMatchObject({
      method: 'DELETE',
      credentials: 'include',
      headers: { 'Content-Type': 'application/json', 'X-SafeFlow-Request': '1' },
      body: '{}',
    });
  });
});
