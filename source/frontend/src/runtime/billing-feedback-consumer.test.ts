import { afterEach, describe, expect, it, vi } from 'vitest';
import { agentSchema } from './agent-client';
import { BillingFeedbackController, feedbackApi } from './billing-feedback-controller';
import {
  FeedbackRecoveryStore,
  feedbackRecoveryPrefix,
  type RecoveryStorage,
} from './billing-feedback-store';
import {
  confirmBillingFeedback,
  executeBillingFeedback,
  feedbackEligible,
  getBillingFeedback,
  prepareBillingFeedback,
  type FeedbackIntent,
  type FeedbackPrepared,
} from './billing-feedback-client';
import { RuntimeApiError, RuntimeTransportError } from './client';
import current from './billing-feedback-http-8ff.fixture.json';
import incomplete from './billing-duplicate-source-unavailable.fixture.json';
import legacy from './billing-mixed.fixture.json';

afterEach(() => vi.unstubAllGlobals());
class Memory implements RecoveryStorage {
  rows = new Map<string, string>();
  fail = false;
  get length() {
    return this.rows.size;
  }
  key(i: number) {
    return [...this.rows.keys()][i] ?? null;
  }
  getItem(k: string) {
    return this.rows.get(k) ?? null;
  }
  setItem(k: string, v: string) {
    if (this.fail) throw Error('quota');
    this.rows.set(k, v);
  }
  removeItem(k: string) {
    if (this.fail) throw Error('unavailable');
    this.rows.delete(k);
  }
}
const view = () => agentSchema.parse(current);
function intent(): FeedbackIntent {
  const v = view();
  if (v.kind !== 'READ' || v.results?.[0].tool_name !== 'analyze_local_basic_spending')
    throw Error();
  return {
    input_version: 1,
    report_id: v.results[0].data.report.metadata.report_id,
    transaction: v.results[0].data.report.evidence![0].reference,
    choice: 'NOT_MINE',
  };
}
function prepared(input = intent()): FeedbackPrepared {
  return {
    contract_version: 1,
    scope: 'BILLING_FEEDBACK_LOCAL_ONLY',
    task_id: 'task_feedback',
    operation_id: 'operation_feedback',
    parameters: { ...input, original_digest: 'a'.repeat(64) },
    confirmation: {
      confirmation_ref: 'confirmation_secret',
      status: 'PREPARED',
      expires_at: new Date(Date.now() + 300_000).toISOString(),
    },
    lock_authorized: false,
    automatic_retry: false,
  };
}
const result = (operation = 'operation_feedback', task = 'task_feedback') => ({
  contract_version: 1,
  scope: 'BILLING_FEEDBACK_LOCAL_ONLY',
  task_id: task,
  operation_id: operation,
  task_state: 'COMPLETED',
  operation_state: 'SUCCEEDED',
  saved: { opaque_member_structure: 'original' },
  replayed: true,
  lock_authorized: false,
  automatic_retry: false,
});
const response = (v: unknown, status = 200) => new Response(JSON.stringify(v), { status });
const apiError = (status: number, reason = 'DEPENDENCY_UNAVAILABLE') =>
  response(
    {
      contract_version: 1,
      error: { reason_code: reason, message: 'rejected', task_id: null, next_action: 'NONE' },
    },
    status,
  );
function setup(api = feedbackApi) {
  const memory = new Memory(),
    store = new FeedbackRecoveryStore(() => memory);
  let generation = 0;
  const notify = vi.fn(),
    auth = vi.fn();
  const boundApi = {
    ...api,
    context:
      api.context === feedbackApi.context ? vi.fn().mockResolvedValue('a'.repeat(64)) : api.context,
  };
  const c = new BillingFeedbackController(() => generation, notify, auth, store, boundApi);
  c.activate();
  return { c, store, memory, notify, auth, changeSession: () => generation++ };
}
async function select(c: BillingFeedbackController) {
  const v = view();
  if (v.kind !== 'READ') throw Error();
  await c.load(v.task.task_id);
  c.select(0, 0, 'NOT_MINE');
}
const deferred = <T>() => {
  let resolve!: (v: T) => void, reject!: (e: unknown) => void;
  const promise = new Promise<T>((a, b) => {
    resolve = a;
    reject = b;
  });
  return { promise, resolve, reject };
};

describe('strict billing feedback HTTP consumer', () => {
  it('sends only three adopted inputs and rejects mismatched prepared parameters/flags', async () => {
    const input = intent(),
      p = prepared();
    const fetch = vi.fn().mockResolvedValue(response(p));
    vi.stubGlobal('fetch', fetch);
    await expect(prepareBillingFeedback(input)).resolves.toEqual(p);
    expect(JSON.parse(fetch.mock.calls[0][1].body)).toEqual(input);
    expect(fetch.mock.calls[0][1]).toMatchObject({
      method: 'POST',
      credentials: 'include',
      headers: { 'X-SafeFlow-Request': '1' },
    });
    for (const bad of [
      { ...p, lock_authorized: true },
      { ...p, automatic_retry: true },
      { ...p, extra: 'forged' },
      { ...p, parameters: { ...p.parameters, choice: 'MINE' } },
      {
        ...p,
        parameters: {
          ...p.parameters,
          transaction: { ...input.transaction, transaction_id: 'other' },
        },
      },
      { ...p, confirmation: { ...p.confirmation, status: 'CONFIRMED' } },
      { ...p, confirmation: { ...p.confirmation, expires_at: '2020-01-01T00:00:00Z' } },
    ]) {
      fetch.mockResolvedValueOnce(response(bad));
      await expect(prepareBillingFeedback(input)).rejects.toMatchObject({
        reason: 'INVALID_RESPONSE',
      });
    }
  });
  it('binds confirm reference and execute/GET task/O/state and sends no evidence', async () => {
    const p = prepared(),
      f = vi.fn();
    vi.stubGlobal('fetch', f);
    f.mockResolvedValueOnce(
      response({
        contract_version: 1,
        scope: p.scope,
        task_id: p.task_id,
        operation_id: p.operation_id,
        confirmation_ref: p.confirmation.confirmation_ref,
        status: 'CONFIRMED',
        lock_authorized: false,
        automatic_retry: false,
      }),
    );
    await confirmBillingFeedback(p);
    expect(JSON.parse(f.mock.calls[0][1].body)).toEqual({
      confirmation_ref: p.confirmation.confirmation_ref,
    });
    for (const bad of [
      { ...result(), operation_id: 'other' },
      { ...result(), task_id: 'other' },
      { ...result(), task_state: 'RUNNING' },
      { ...result(), operation_state: 'PROCESSING' },
      { ...result(), saved: null },
      { ...result(), scope: 'CARD' },
    ]) {
      f.mockResolvedValueOnce(response(bad));
      await expect(executeBillingFeedback(p)).rejects.toBeInstanceOf(RuntimeTransportError);
    }
    f.mockResolvedValueOnce(response({ ...result(), replayed: false }));
    await expect(getBillingFeedback(p.operation_id, p.task_id)).rejects.toBeInstanceOf(
      RuntimeTransportError,
    );
    f.mockResolvedValueOnce(
      response({
        contract_version: 1,
        scope: p.scope,
        task_id: p.task_id,
        operation_id: p.operation_id,
        confirmation_ref: 'wrong',
        status: 'CONFIRMED',
        lock_authorized: false,
        automatic_retry: false,
      }),
    );
    await expect(confirmBillingFeedback(p)).rejects.toBeInstanceOf(RuntimeTransportError);
  });
  it.each([401, 403, 404, 409, 501, 503])(
    'preserves actual HTTP %s; accepts RECONCILE_REQUIRED strictly',
    async (status) => {
      vi.stubGlobal(
        'fetch',
        vi
          .fn()
          .mockResolvedValue(
            apiError(status, status === 409 ? 'RECONCILE_REQUIRED' : 'DEPENDENCY_UNAVAILABLE'),
          ),
      );
      await expect(getBillingFeedback('operation_feedback')).rejects.toMatchObject({ status });
    },
  );
  it('failed/incomplete and V1 reports remain readable but ineligible', () => {
    for (const raw of [incomplete, legacy]) {
      const v = agentSchema.parse(raw);
      if (v.kind !== 'READ' || v.results?.[0].tool_name !== 'analyze_local_basic_spending')
        throw Error();
      expect(feedbackEligible(v.results[0])).toBe(false);
    }
  });
});
describe('page actions and durable original recovery', () => {
  it('GET-only typo creates no write protection; after reload a real report can prepare', async () => {
    const api = {
      ...feedbackApi,
      context: vi.fn().mockResolvedValue('a'.repeat(64)),
      getAgent: vi.fn().mockResolvedValue(view()),
      get: vi.fn().mockRejectedValue(new RuntimeApiError('NOT_FOUND', null, 'NONE', 404)),
      prepare: vi.fn().mockResolvedValue(prepared()),
    };
    const { c, store, memory } = setup(api);
    await select(c);
    expect(c.canPrepare()).toBe(true);
    await c.query('operation_typo_never_submitted');
    expect(store.read()).toEqual([]);
    expect(api.prepare).not.toHaveBeenCalled();
    c.deactivate();
    const reloaded = new BillingFeedbackController(
      () => 0,
      vi.fn(),
      vi.fn(),
      new FeedbackRecoveryStore(() => memory),
      api,
    );
    reloaded.activate();
    await select(reloaded);
    expect(reloaded.canPrepare()).toBe(true);
    await reloaded.prepare();
    expect(api.prepare).toHaveBeenCalledTimes(1);
    expect(store.read()[0].operation_id).toBe('operation_feedback');
  });
  it('real unknown prepare A survives unrelated GET404 B without adding B or enabling another write', async () => {
    const api = {
      ...feedbackApi,
      getAgent: vi.fn().mockResolvedValue(view()),
      prepare: vi.fn().mockRejectedValue(new RuntimeTransportError('TIMEOUT')),
      get: vi.fn().mockRejectedValue(new RuntimeApiError('NOT_FOUND', null, 'NONE', 404)),
    };
    const { c, store } = setup(api);
    await select(c);
    await c.prepare();
    const before = store.read();
    expect(before).toHaveLength(1);
    expect(before[0].operation_id).toBeNull();
    await c.query('B_never_submitted');
    expect(store.read()).toEqual(before);
    c.reset();
    await select(c);
    await c.prepare();
    expect(c.canPrepare()).toBe(false);
    expect(api.prepare).toHaveBeenCalledTimes(1);
  });
  it('GET saved B cannot clear real unresolved A or create another recovery record', async () => {
    const api = {
      ...feedbackApi,
      getAgent: vi.fn().mockResolvedValue(view()),
      get: vi.fn().mockResolvedValue(result('B', 'task_B')),
    };
    const { c, store } = setup(api);
    store.reserve('real-A');
    store.bind('real-A', { task_id: 'task_A', operation_id: 'A' });
    const before = store.read();
    c.reset();
    await c.query('B');
    expect(c.state.result?.operation_id).toBe('B');
    expect(store.read()).toEqual(before);
    await select(c);
    expect(c.canPrepare()).toBe(false);
  });
  it('successful GET resolves only the matching original write and preserves another real write', async () => {
    const api = { ...feedbackApi, get: vi.fn().mockResolvedValue(result('A', 'task_A')) };
    const { c, store } = setup(api);
    store.ensureOriginal({ task_id: 'task_A', operation_id: 'A' });
    store.ensureOriginal({ task_id: 'task_B', operation_id: 'B' });
    c.reset();
    await c.query('A', 'task_A');
    expect(store.read().map((r) => r.operation_id)).toEqual(['B']);
  });
  it('chooses the second report explicitly without submitting the first report reference', async () => {
    const raw = structuredClone(current);
    const second = structuredClone(raw.results[0]);
    second.operation_id = 'second_operation';
    second.result_ref = 'billing_result_second_operation';
    second.data.report.metadata.report_id = 'billing_report_second_operation';
    if ('Ok' in second.data.duplicate_candidates)
      second.data.duplicate_candidates.Ok.report_id = 'billing_report_second_operation';
    raw.results.push(second);
    const api = {
      ...feedbackApi,
      getAgent: vi.fn().mockResolvedValue(agentSchema.parse(raw)),
      prepare: vi.fn(async (input: FeedbackIntent) => prepared(input)),
    };
    const { c } = setup(api);
    await c.load(raw.task.task_id);
    expect(c.state.selected).toBeNull();
    c.select(1, 0, 'MINE');
    await c.prepare();
    expect(api.prepare.mock.calls[0][0].report_id).toBe('billing_report_second_operation');
    expect(c.state.prepared?.parameters.report_id).toBe('billing_report_second_operation');
  });
  it('partial storage failure after prepare preserves the unknown marker and blocks confirm', async () => {
    const api = {
      ...feedbackApi,
      getAgent: vi.fn().mockResolvedValue(view()),
      prepare: vi.fn(async () => {
        memory.fail = true;
        return prepared();
      }),
      confirm: vi.fn(),
    };
    const { c, memory, store } = setup(api);
    await select(c);
    await c.prepare();
    await c.confirm();
    expect(api.prepare).toHaveBeenCalledTimes(1);
    expect(api.confirm).not.toHaveBeenCalled();
    memory.fail = false;
    expect(store.read()).toEqual([expect.objectContaining({ operation_id: null })]);
  });
  it('expired confirmation cannot execute and only original recovery remains', async () => {
    const p = prepared(),
      api = {
        ...feedbackApi,
        getAgent: vi.fn().mockResolvedValue(view()),
        prepare: vi.fn().mockResolvedValue(p),
        confirm: vi.fn(),
        execute: vi.fn(),
      };
    const { c, store } = setup(api);
    await select(c);
    await c.prepare();
    await c.confirm();
    p.confirmation.expires_at = '2020-01-01T00:00:00Z';
    await c.execute();
    expect(api.execute).not.toHaveBeenCalled();
    expect(c.state.prepared).toBeNull();
    expect(store.read()).toHaveLength(1);
  });
  it('external context change without local generation invalidates late report and prepared data', async () => {
    let context = 'a'.repeat(64);
    const wait = deferred<FeedbackPrepared>();
    const api = {
      ...feedbackApi,
      context: vi.fn(async () => context),
      getAgent: vi.fn().mockResolvedValue(view()),
      prepare: vi.fn().mockReturnValue(wait.promise),
    };
    const { c, store } = setup(api);
    await select(c);
    const pending = c.prepare();
    await vi.waitFor(() => expect(api.prepare).toHaveBeenCalledTimes(1));
    context = 'b'.repeat(64);
    wait.resolve(prepared());
    await pending;
    expect(c.state.prepared).toBeNull();
    expect(c.state.reports).toEqual([]);
    expect(store.read()[0].operation_id).toBeNull();
  });
  it('late confirmed response after selection change cannot authorize execute', async () => {
    const wait = deferred<Awaited<ReturnType<typeof feedbackApi.confirm>>>();
    const api = {
      ...feedbackApi,
      getAgent: vi.fn().mockResolvedValue(view()),
      prepare: vi.fn().mockResolvedValue(prepared()),
      confirm: vi.fn().mockReturnValue(wait.promise),
      execute: vi.fn(),
    };
    const { c } = setup(api);
    await select(c);
    await c.prepare();
    const pending = c.confirm();
    await vi.waitFor(() => expect(api.confirm).toHaveBeenCalledTimes(1));
    c.select(0, 1, 'MINE');
    wait.resolve({
      contract_version: 1,
      scope: 'BILLING_FEEDBACK_LOCAL_ONLY',
      task_id: 'task_feedback',
      operation_id: 'operation_feedback',
      confirmation_ref: 'confirmation_secret',
      status: 'CONFIRMED',
      lock_authorized: false,
      automatic_retry: false,
    });
    await pending;
    await c.execute();
    expect(c.state.confirmed).toBe(false);
    expect(api.execute).not.toHaveBeenCalled();
  });
  it('late execute result after unmount cannot issue another request or resolve records', async () => {
    const wait = deferred<ReturnType<typeof result>>();
    const api = {
      ...feedbackApi,
      getAgent: vi.fn().mockResolvedValue(view()),
      prepare: vi.fn().mockResolvedValue(prepared()),
      confirm: vi.fn(),
      execute: vi.fn().mockReturnValue(wait.promise),
      get: vi.fn(),
    };
    const { c, memory, store } = setup(api);
    await select(c);
    await c.prepare();
    await c.confirm();
    const pending = c.execute();
    await vi.waitFor(() => expect(api.execute).toHaveBeenCalledTimes(1));
    c.deactivate();
    store.ensureOriginal({ task_id: 'task_B', operation_id: 'B' });
    const before = JSON.stringify([...memory.rows]);
    wait.resolve(result());
    await pending;
    expect(api.get).not.toHaveBeenCalled();
    expect(JSON.stringify([...memory.rows])).toBe(before);
  });
  it.each([401, 403])(
    'authorization %s clears visible report and confirmation while retaining selectors',
    async (status) => {
      const api = {
        ...feedbackApi,
        getAgent: vi.fn().mockResolvedValue(view()),
        prepare: vi.fn().mockResolvedValue(prepared()),
        confirm: vi
          .fn()
          .mockRejectedValue(
            new RuntimeApiError(
              status === 401 ? 'NOT_AUTHENTICATED' : 'FORBIDDEN',
              null,
              'NONE',
              status,
            ),
          ),
      };
      const { c, store } = setup(api);
      await select(c);
      await c.prepare();
      await c.confirm();
      expect(c.state.reports).toEqual([]);
      expect(c.state.prepared).toBeNull();
      expect(store.read()).toHaveLength(1);
    },
  );
  it('requires explicit report/evidence/choice and separate prepare/confirm/execute; GET verifies save', async () => {
    const log: string[] = [],
      p = prepared();
    const fetch = vi.fn(async (url: string, init: RequestInit) => {
      log.push(`${init.method} ${url}`);
      if (url.startsWith('/api/agent/')) return response(current);
      if (url.endsWith('/prepare')) return response(p);
      if (url.endsWith('/confirm'))
        return response({
          ...p,
          parameters: undefined,
          confirmation: undefined,
          confirmation_ref: p.confirmation.confirmation_ref,
          status: 'CONFIRMED',
        });
      return response(result());
    });
    vi.stubGlobal('fetch', fetch);
    const { c, memory, store } = setup();
    const v = view();
    if (v.kind !== 'READ') throw Error();
    await c.load(v.task.task_id);
    expect(c.canPrepare()).toBe(false);
    c.select(0);
    expect(c.canPrepare()).toBe(false);
    c.select(0, 0, 'NOT_MINE');
    await c.execute();
    expect(log).toHaveLength(1);
    await c.prepare();
    expect(log).toHaveLength(2);
    expect(c.state.confirmed).toBe(false);
    const saved = JSON.stringify([...memory.rows]);
    expect(saved).toContain(p.operation_id);
    expect(saved).not.toContain('confirmation_secret');
    expect(saved).not.toContain('NOT_MINE');
    expect(saved).not.toContain('evidence');
    await c.confirm();
    expect(log).toHaveLength(3);
    expect(c.state.result).toBeNull();
    await c.execute();
    expect(log.map((s) => s.split(' ')[0])).toEqual(['GET', 'POST', 'POST', 'POST', 'GET']);
    expect(c.state.result?.operation_id).toBe(p.operation_id);
    expect(store.read()).toEqual([]);
  });
  it.each(['TIMEOUT', 'ABORTED', 'UNAVAILABLE', 'INVALID_RESPONSE'] as const)(
    'execute %s issues only GET original; failed GET retains all selectors',
    async (reason) => {
      const api = {
        ...feedbackApi,
        getAgent: vi.fn().mockResolvedValue(view()),
        prepare: vi.fn().mockResolvedValue(prepared()),
        confirm: vi.fn(),
        execute: vi.fn().mockRejectedValue(new RuntimeTransportError(reason)),
        get: vi.fn().mockRejectedValue(new RuntimeApiError('NOT_FOUND', null, 'NONE', 404)),
      };
      const { c, store } = setup(api);
      await select(c);
      await c.prepare();
      await c.confirm();
      await c.execute();
      expect(api.prepare).toHaveBeenCalledTimes(1);
      expect(api.confirm).toHaveBeenCalledTimes(1);
      expect(api.execute).toHaveBeenCalledTimes(1);
      expect(api.get).toHaveBeenCalledExactlyOnceWith(
        'operation_feedback',
        'task_feedback',
        expect.any(AbortSignal),
      );
      expect(store.read()).toHaveLength(1);
      expect(c.state.prepared).toBeNull();
      expect(c.canPrepare()).toBe(false);
      await c.execute();
      expect(api.execute).toHaveBeenCalledTimes(1);
    },
  );
  it('prepare response loss leaves unrecoverable marker and never retries; reload does not resurrect confirmation', async () => {
    const api = {
      ...feedbackApi,
      getAgent: vi.fn().mockResolvedValue(view()),
      prepare: vi.fn().mockRejectedValue(new RuntimeTransportError('TIMEOUT')),
    };
    const { c, store, memory } = setup(api);
    await select(c);
    await c.prepare();
    await c.prepare();
    expect(api.prepare).toHaveBeenCalledTimes(1);
    expect(store.read()[0].operation_id).toBeNull();
    c.deactivate();
    const other = new BillingFeedbackController(
      () => 1,
      vi.fn(),
      vi.fn(),
      new FeedbackRecoveryStore(() => memory),
      api,
    );
    other.activate();
    expect(other.state.prepared).toBeNull();
    expect(other.state.recovery).toHaveLength(1);
    expect(other.canPrepare()).toBe(false);
  });
  it('501 is a genuine disabled path without a stuck unknown prepare', async () => {
    const api = {
      ...feedbackApi,
      getAgent: vi.fn().mockResolvedValue(view()),
      prepare: vi
        .fn()
        .mockRejectedValue(new RuntimeApiError('DEPENDENCY_UNAVAILABLE', null, 'NONE', 501)),
    };
    const { c, store } = setup(api);
    await select(c);
    await c.prepare();
    expect(c.state.notice).toContain('尚未启用');
    expect(store.read()).toEqual([]);
    expect(c.state.result).toBeNull();
  });
  it('storage quota prevents prepare, and later storage failure prevents execute', async () => {
    const api = {
      ...feedbackApi,
      getAgent: vi.fn().mockResolvedValue(view()),
      prepare: vi.fn().mockResolvedValue(prepared()),
      confirm: vi.fn(),
      execute: vi.fn(),
    };
    const { c, memory } = setup(api);
    await select(c);
    memory.fail = true;
    await c.prepare();
    expect(api.prepare).not.toHaveBeenCalled();
    memory.fail = false;
    await c.prepare();
    await c.confirm();
    memory.fail = true;
    await c.execute();
    expect(api.execute).not.toHaveBeenCalled();
    expect(c.state.notice).toContain('阻止');
  });
  it('two originals survive reload; resolving A never drops B, 404 never clears B', async () => {
    const api = {
      ...feedbackApi,
      get: vi
        .fn()
        .mockResolvedValueOnce(result('A', 'task_A'))
        .mockRejectedValueOnce(new RuntimeApiError('NOT_FOUND', null, 'NONE', 404)),
    };
    const { c, store } = setup(api);
    store.ensureOriginal({ task_id: 'task_A', operation_id: 'A' });
    store.ensureOriginal({ task_id: 'task_B', operation_id: 'B' });
    c.reset();
    await c.query('A', 'task_A');
    expect(store.read().map((r) => r.operation_id)).toEqual(['B']);
    await c.query('B', 'task_B');
    expect(store.read().map((r) => r.operation_id)).toEqual(['B']);
    expect(c.canPrepare()).toBe(false);
  });
  it.each(['unmount', 'session', 'selection'] as const)(
    'late prepare after %s changes neither UI nor persistent records',
    async (mode) => {
      const wait = deferred<FeedbackPrepared>();
      const api = {
        ...feedbackApi,
        getAgent: vi.fn().mockResolvedValue(view()),
        prepare: vi.fn().mockReturnValue(wait.promise),
      };
      const { c, store, memory, notify, changeSession } = setup(api);
      await select(c);
      const pending = c.prepare();
      await vi.waitFor(() => expect(api.prepare).toHaveBeenCalledTimes(1));
      if (mode === 'unmount') c.deactivate();
      else if (mode === 'session') changeSession();
      else c.select(0, 1, 'MINE');
      store.ensureOriginal({ task_id: 'new_task', operation_id: 'new_O' });
      const prior = JSON.stringify([...memory.rows]),
        updates = notify.mock.calls.length;
      wait.resolve(prepared());
      await pending;
      expect(JSON.stringify([...memory.rows])).toBe(prior);
      expect(notify.mock.calls.length).toBe(updates);
    },
  );
  it.each([true, false])(
    'late GET success/failure after unmount never overwrites new component recovery',
    async (success) => {
      const wait = deferred<ReturnType<typeof result>>();
      const api = { ...feedbackApi, get: vi.fn().mockReturnValue(wait.promise) };
      const { c, store, memory } = setup(api);
      store.ensureOriginal({ task_id: 'task_A', operation_id: 'A' });
      const pending = c.query('A', 'task_A');
      await vi.waitFor(() => expect(api.get).toHaveBeenCalledTimes(1));
      c.deactivate();
      store.ensureOriginal({ task_id: 'task_B', operation_id: 'B' });
      const prior = JSON.stringify([...memory.rows]);
      if (success) wait.resolve(result('A', 'task_A'));
      else wait.reject(new Error('late 503'));
      await pending;
      expect(JSON.stringify([...memory.rows])).toBe(prior);
    },
  );
  it('session generation changes before a click cannot reuse old confirmation', async () => {
    const api = {
      ...feedbackApi,
      getAgent: vi.fn().mockResolvedValue(view()),
      prepare: vi.fn().mockResolvedValue(prepared()),
      confirm: vi.fn(),
    };
    const { c, changeSession } = setup(api);
    await select(c);
    await c.prepare();
    changeSession();
    await c.confirm();
    expect(api.confirm).not.toHaveBeenCalled();
    expect(c.state.prepared).toBeNull();
  });
  it('malformed stored rows block writes without deleting originals', () => {
    const { c, memory } = setup();
    memory.rows.set(feedbackRecoveryPrefix + 'bad', '{"task_id":"B"}');
    c.reset();
    expect(c.state.storageError).toBe(true);
    expect(c.canPrepare()).toBe(false);
    expect(memory.rows.size).toBe(1);
  });
});
