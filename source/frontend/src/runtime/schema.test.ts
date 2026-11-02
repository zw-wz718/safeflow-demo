import { describe, expect, it } from 'vitest';
import { errorSchema, sessionSchema, taskSchema } from './schema';

const waiting = {
  contract_version: 1,
  mode: 'PROJECT_SIMULATION',
  task_id: 'task_123',
  task_state: 'WAITING_USER',
  revision: 2,
  plan_version: 2,
  facts_version: 7,
  operation_state: null,
  security_outcome: null,
  security_scope: 'CURRENT_DECISION_NOT_PROVIDED',
  operations: [],
  recovery: { kind: 'AWAIT_USER', node_id: 'choose' },
  next_action: { kind: 'CONTINUE', node_id: 'choose' },
  replayed: false,
};

describe('runtime HTTP contract validation', () => {
  it('accepts dynamic waiting nodes without inventing a security decision or operation', () => {
    const parsed = taskSchema.parse({
      ...waiting,
      revision: 4,
      recovery: { kind: 'AWAIT_USER', node_id: 'choose_two' },
      next_action: { kind: 'CONTINUE', node_id: 'choose_two' },
    });
    expect(parsed.next_action).toEqual({ kind: 'CONTINUE', node_id: 'choose_two' });
    expect(parsed.operations).toEqual([]);
  });

  it.each([
    { revision: Number.MAX_SAFE_INTEGER + 1 },
    { plan_version: 4294967296 },
    { facts_version: 0 },
    { task_state: 'COMPLETED' },
    { recovery: { kind: 'AWAIT_USER', node_id: 'other_node' } },
    { next_action: { kind: 'CONTINUE', node_id: '' } },
    { security_outcome: 'ALLOW' },
    { operation_state: 'SUCCEEDED' },
    { operations: [{ operation_id: 'invented' }] },
    { balance: 10000 },
    { mode: 'PRODUCTION' },
    { contract_version: 2 },
  ])('rejects unsupported or inconsistent task content: %j', (override) => {
    expect(taskSchema.safeParse({ ...waiting, ...override }).success).toBe(false);
  });

  it('accepts a current completed state returned on replay', () => {
    expect(
      taskSchema.parse({
        ...waiting,
        task_state: 'COMPLETED',
        revision: 6,
        recovery: { kind: 'FINISHED', task_state: 'COMPLETED' },
        next_action: { kind: 'FINISHED' },
        replayed: true,
      }).task_state,
    ).toBe('COMPLETED');
  });

  it('rejects finished recovery attached to a running task', () => {
    expect(
      taskSchema.safeParse({
        ...waiting,
        task_state: 'RUNNING',
        recovery: { kind: 'FINISHED', task_state: 'RUNNING' },
        next_action: { kind: 'FINISHED' },
      }).success,
    ).toBe(false);
  });

  it('rejects client identity fields in session responses', () => {
    expect(
      sessionSchema.safeParse({ mode: 'PROJECT_SIMULATION', authenticated: true, user_id: 'other' })
        .success,
    ).toBe(false);
  });

  it('requires an original id for query recovery and accepts a forbidden response', () => {
    const error = {
      reason_code: 'FORBIDDEN',
      message: 'Request denied.',
      task_id: null,
      next_action: 'NONE',
    };
    expect(errorSchema.safeParse({ contract_version: 1, error }).success).toBe(true);
    expect(
      errorSchema.safeParse({
        contract_version: 1,
        error: { ...error, next_action: 'QUERY_ORIGINAL_TASK' },
      }).success,
    ).toBe(false);
  });
});
