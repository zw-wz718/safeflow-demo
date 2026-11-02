import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { executePlanning, getPlannerStatus, getPlanning, submitPlanning } from './planning-client';
import { plannerStatusSchema, planningSchema } from './planning-schema';
import { PlanningSummary } from './PlanningWorkspace';

const accepted = planningSchema.parse({
  contract_version: 1,
  mode: 'PROJECT_SIMULATION',
  planner_source: 'OPENAI_COMPATIBLE',
  kind: 'TASK',
  task: {
    contract_version: 1,
    mode: 'PROJECT_SIMULATION',
    task_id: 'model_task_123',
    task_state: 'RUNNING',
    revision: 1,
    plan_version: 1,
    facts_version: 1,
    operation_state: null,
    security_outcome: null,
    security_scope: 'CURRENT_DECISION_NOT_PROVIDED',
    operations: [],
    recovery: { kind: 'REVIEW_NODE', node_id: 'dynamic_model_entry' },
    next_action: { kind: 'REVIEW_ORIGINAL_TASK' },
    replayed: false,
  },
  clarification: null,
  can_start: true,
});
if (accepted.kind !== 'TASK') throw new Error('Test contract must contain a task');
const waiting = {
  ...accepted,
  task: {
    ...accepted.task,
    task_state: 'WAITING_USER',
    revision: 2,
    recovery: { kind: 'AWAIT_USER', node_id: 'dynamic_model_wait' },
    next_action: { kind: 'CONTINUE', node_id: 'dynamic_model_wait' },
  },
  can_start: false,
};
const configured = {
  contract_version: 1,
  mode: 'PROJECT_SIMULATION',
  planner_source: 'OPENAI_COMPATIBLE',
  configured: true,
  connectivity: 'NOT_VERIFIED',
  reason_code: null,
  tool_scope: 'READ_ONLY_BALANCE',
};
const clarify = {
  contract_version: 1,
  mode: 'PROJECT_SIMULATION',
  planner_source: 'OPENAI_COMPATIBLE',
  kind: 'CLARIFY',
  task: null,
  clarification: { question: '<img src=x onerror="alert(1)">请说明请求。' },
  can_start: false,
};
const response = (value: unknown, status = 200) =>
  new Response(JSON.stringify(value), { status, headers: { 'Content-Type': 'application/json' } });

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe('model planning HTTP contract isolation', () => {
  it('does not convert a configured service into verified connectivity', () => {
    expect(plannerStatusSchema.parse(configured).connectivity).toBe('NOT_VERIFIED');
    expect(
      plannerStatusSchema.safeParse({ ...configured, connectivity: 'CONNECTED' }).success,
    ).toBe(false);
    expect(
      plannerStatusSchema.safeParse({ ...configured, api_key: 'not-a-real-key' }).success,
    ).toBe(false);
  });

  it('requires the explicit unconfigured reason and read-only tool scope', () => {
    expect(
      plannerStatusSchema.safeParse({
        ...configured,
        configured: false,
        reason_code: 'LLM_NOT_CONFIGURED',
      }).success,
    ).toBe(true);
    expect(plannerStatusSchema.safeParse({ ...configured, configured: false }).success).toBe(false);
    expect(
      plannerStatusSchema.safeParse({ ...configured, tool_scope: 'FUNDS_EXECUTION' }).success,
    ).toBe(false);
  });

  it.each([
    { planner_source: 'MOCK_LLM' },
    { user_id: 'model_owner' },
    { kind: 'BUSINESS_SUCCEEDED' },
    { task: null },
    { clarification: { question: 'Also clarify' } },
    { can_start: 'true' },
    { plan: { nodes: [] } },
    { task: { ...accepted.task, revision: Number.MAX_SAFE_INTEGER + 1 } },
    { task: { ...accepted.task, plan_version: 4294967296 } },
    {
      task: {
        ...accepted.task,
        task_state: 'COMPLETED',
        recovery: { kind: 'FINISHED', task_state: 'COMPLETED' },
        next_action: { kind: 'FINISHED' },
      },
    },
    { task: { ...accepted.task, next_action: { kind: 'EXECUTE_FUNDS' } } },
    { task: { ...accepted.task, operation_state: 'SUCCEEDED' } },
    { task: { ...accepted.task, balance: '10000.00' } },
  ])('rejects unsupported proposal content: %j', (override) => {
    expect(planningSchema.safeParse({ ...accepted, ...override }).success).toBe(false);
  });

  it('requires an independent no-start signal for a clarification', () => {
    expect(planningSchema.safeParse(clarify).success).toBe(true);
    expect(planningSchema.safeParse({ ...clarify, can_start: true }).success).toBe(false);
    expect(planningSchema.safeParse({ ...clarify, task: accepted.task }).success).toBe(false);
  });

  it('displays accepted planning as unexecuted and renders model text as plain text', () => {
    const taskMarkup = renderToStaticMarkup(createElement(PlanningSummary, { view: accepted }));
    expect(taskMarkup).toContain('计划已接收，尚未执行');
    expect(taskMarkup).not.toContain('已完成');
    const clarificationMarkup = renderToStaticMarkup(
      createElement(PlanningSummary, { view: planningSchema.parse(clarify) }),
    );
    expect(clarificationMarkup).toContain('&lt;img');
    expect(clarificationMarkup).not.toContain('<img');
    expect(clarificationMarkup).not.toContain('<button');
  });
});

describe('model planning HTTP client boundaries', () => {
  it('queries configuration without probing or submitting a model request', async () => {
    const fetch = vi.fn().mockResolvedValue(response(configured));
    vi.stubGlobal('fetch', fetch);
    await getPlannerStatus();
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(fetch.mock.calls[0][0]).toBe('/api/llm/status');
    expect(fetch.mock.calls[0][1]).toMatchObject({
      method: 'GET',
      credentials: 'include',
      cache: 'no-store',
    });
  });

  it('submits only user text once and does not execute the accepted task', async () => {
    const fetch = vi.fn().mockResolvedValue(response(accepted));
    vi.stubGlobal('fetch', fetch);
    await submitPlanning('查询余额');
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(fetch.mock.calls[0][0]).toBe('/api/llm/plans');
    expect(fetch.mock.calls[0][1]).toMatchObject({
      method: 'POST',
      credentials: 'include',
      headers: { 'Content-Type': 'application/json', 'X-SafeFlow-Request': '1' },
      body: JSON.stringify({ message: '查询余额' }),
    });
  });

  it('uses UTF-8 bytes rather than character count for the input limit', async () => {
    const fetch = vi.fn();
    vi.stubGlobal('fetch', fetch);
    await expect(submitPlanning('查'.repeat(1366))).rejects.toMatchObject({
      reason: 'INVALID_RESPONSE',
    });
    await expect(submitPlanning('  ')).rejects.toMatchObject({ reason: 'INVALID_RESPONSE' });
    expect(fetch).not.toHaveBeenCalled();
  });

  it('uses the precise current versions only for an explicit execution', async () => {
    const fetch = vi.fn().mockResolvedValue(response(waiting));
    vi.stubGlobal('fetch', fetch);
    const result = await executePlanning(accepted);
    expect(result.can_start).toBe(false);
    expect(fetch.mock.calls[0][0]).toBe('/api/llm/plans/model_task_123/execute');
    expect(JSON.parse(fetch.mock.calls[0][1].body)).toEqual({
      task_revision: 1,
      plan_version: 1,
      facts_version: 1,
    });
  });

  it('does not infer start permission from RUNNING and an empty operation list', async () => {
    const fetch = vi.fn();
    vi.stubGlobal('fetch', fetch);
    await expect(
      executePlanning(planningSchema.parse({ ...accepted, can_start: false })),
    ).rejects.toMatchObject({ reason: 'INVALID_RESPONSE' });
    expect(fetch).not.toHaveBeenCalled();
  });

  it('requires execution success to clear the start permission', async () => {
    const fetch = vi.fn().mockResolvedValue(response(accepted));
    vi.stubGlobal('fetch', fetch);
    await expect(executePlanning(accepted)).rejects.toMatchObject({ reason: 'INVALID_RESPONSE' });
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it('does not accept an automatically executed result from the planning submit route', async () => {
    const fetch = vi.fn().mockResolvedValue(response(waiting));
    vi.stubGlobal('fetch', fetch);
    await expect(submitPlanning('查询余额')).rejects.toMatchObject({ reason: 'INVALID_RESPONSE' });
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it('requires the original task id and a verified planning source on GET', async () => {
    const fetch = vi
      .fn()
      .mockResolvedValue(
        response({ ...accepted, task: { ...accepted.task, task_id: 'other_task' } }),
      );
    vi.stubGlobal('fetch', fetch);
    await expect(getPlanning('model_task_123')).rejects.toMatchObject({
      reason: 'INVALID_RESPONSE',
    });
    expect(fetch.mock.calls[0][1].method).toBe('GET');
  });

  it.each(['LLM_NOT_CONFIGURED', 'LLM_GATEWAY_UNAVAILABLE', 'LLM_PROPOSAL_REJECTED'])(
    'preserves %s as an unavailable or rejected proposal, not a success',
    async (reason) => {
      const fetch = vi.fn().mockResolvedValue(
        response(
          {
            contract_version: 1,
            error: {
              reason_code: reason,
              message: 'Fixed safe explanation.',
              task_id: null,
              next_action: 'NONE',
            },
          },
          reason === 'LLM_PROPOSAL_REJECTED' ? 422 : 503,
        ),
      );
      vi.stubGlobal('fetch', fetch);
      await expect(submitPlanning('查询余额')).rejects.toMatchObject({
        reason,
        message: reason,
        taskId: null,
      });
      expect(fetch).toHaveBeenCalledTimes(1);
    },
  );

  it('does not fall back or repeat a model request when the response is lost', async () => {
    const fetch = vi.fn().mockRejectedValue(new TypeError('Network unavailable'));
    vi.stubGlobal('fetch', fetch);
    await expect(submitPlanning('查询余额')).rejects.toMatchObject({ reason: 'UNAVAILABLE' });
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it('allows the approved 135-second planning deadline without applying the old 8-second timeout', async () => {
    vi.useFakeTimers();
    let requestSignal: AbortSignal | null | undefined;
    const fetch = vi.fn(
      (_path, options: RequestInit) =>
        new Promise((_resolve, reject) => {
          requestSignal = options.signal;
          requestSignal?.addEventListener('abort', () =>
            reject(new DOMException('Aborted', 'AbortError')),
          );
        }),
    );
    vi.stubGlobal('fetch', fetch);
    const result = expect(submitPlanning('查询余额')).rejects.toMatchObject({ reason: 'TIMEOUT' });
    await vi.advanceTimersByTimeAsync(8000);
    expect(requestSignal?.aborted).toBe(false);
    await vi.advanceTimersByTimeAsync(127000);
    await result;
    expect(fetch).toHaveBeenCalledTimes(1);
  });
});
