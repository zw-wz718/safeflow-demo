import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import {
  Bot,
  CheckCheck,
  CircleAlert,
  FileCheck,
  LoaderCircle,
  Plus,
  RefreshCw,
  Search,
  Send,
  X,
} from 'lucide-react';
import { RuntimeApiError, RuntimeTransportError } from './client';
import { FundsResult, PaymentPanel, ResultRows } from './AgentResults';
import {
  originalRequiresRecovery,
  fundsReference,
  fundsMatches,
  fundsRequireRecovery,
  sameFundsReference,
  type FundsReference,
  type PaymentFunds,
  type PaymentOriginal,
} from './payment-schema';
import { retain, observe, isResolved, type RecoveryEntry } from './payment-recovery';
import { PaymentRecoveryStore } from './payment-recovery-store';
import {
  actDraft,
  actPayment,
  queryPaymentOriginal,
  queryPaymentFunds,
  executePayment,
  agentStatus,
  getAgent,
  prepareAgent,
  runRead,
  searchPayees,
  sendAgent,
  sampleAgent,
  viewId,
  type AgentStatus,
  type AgentView,
  type LifecycleView,
  type FixedSample,
  type Payee,
} from './agent-client';

const key = 'safeflow.agent.last-task.v1';
const uncertainKey = 'safeflow.agent.uncertain.v1';
function originalFunds(original: PaymentOriginal): PaymentFunds {
  return {
    contract_version: 1,
    scope: 'PAYMENT_FUNDS_RESULT_ONLY',
    task_id: original.task_id,
    preparation_operation_id: original.binding.preparation_operation_id,
    snapshot_digest: original.binding.preparation_arguments_hash,
    transfer: original.binding.normalized_arguments,
    state: original.state,
    submitted: !!(original.first_public_submission || original.verified_business_result),
    automatic_retry: false,
  };
}
const sampleLabels: Record<FixedSample, string> = {
  READ_WAIT: '样例：查询余额并等待继续',
  CREATE_200: '样例：生成200元模拟草案',
  MODIFY_300: '样例：同任务修改为300元',
  CANCEL_DRAFT: '样例：取消当前草案',
  EXECUTE_TRANSFER: '样例：准备当前草案付款',
};
const customerSampleLabels: Record<FixedSample, string> = {
  READ_WAIT: '查询余额',
  CREATE_200: '保存200元模拟草案',
  MODIFY_300: '将草案修改为300元',
  CANCEL_DRAFT: '取消草案',
  EXECUTE_TRANSFER: '准备草案付款',
};
function stored(k: string) {
  try {
    return sessionStorage.getItem(k) ?? '';
  } catch {
    return '';
  }
}
function save(k: string, value: string) {
  try {
    if (value) sessionStorage.setItem(k, value);
    else sessionStorage.removeItem(k);
  } catch {
    /* Server state remains authoritative. */
  }
}
export function clearAgentReferences() {
  save(key, '');
  save(uncertainKey, '');
}
export type AgentCommand = 'agent_send' | 'agent_action' | 'agent_search';
type Props = {
  customer?: boolean;
  authenticated: boolean;
  generation: number;
  busy: string | null;
  sessionControls: ReactNode;
  runRequest: <T>(action: (signal: AbortSignal) => Promise<T>) => Promise<T>;
  getGeneration: () => number;
  isBusy: () => boolean;
  beginCommand: (command: AgentCommand) => boolean;
  finishCommand: (command: AgentCommand) => void;
  onAuthenticationLost: () => void;
};
const labels: Record<string, string> = {
  RUNNING: '运行中',
  WAITING_USER: '等待下一步',
  WAITING_EXTERNAL: '等待外部结果',
  COMPLETED: '已完成',
  FAILED: '已失败',
  CANCELLED: '已取消',
};
function errorText(e: unknown) {
  if (e instanceof RuntimeApiError) {
    const labels: Record<string, string> = {
      LLM_NOT_CONFIGURED: '模型服务尚未配置。',
      LLM_GATEWAY_UNAVAILABLE: '模型服务暂不可用。',
      LLM_PROPOSAL_REJECTED: '计划提案未通过校验。',
      DEPENDENCY_UNAVAILABLE: '所需能力或数据暂不可用。',
      NOT_FOUND: '任务不存在或不属于当前会话。',
      NOT_AUTHENTICATED: '会话已失效。',
      STALE_PLAN: '任务版本已变化，请刷新原任务。',
      IDEMPOTENCY_CONFLICT: '操作与原记录冲突，请刷新原任务。',
      FORBIDDEN: '请求被安全校验拒绝。',
      VALIDATION_ERROR: '请求未通过校验。',
    };
    return labels[e.reason] ?? '请求失败。';
  }
  return '服务响应未核实，请查询原任务。';
}
export function AgentWorkspace(p: Props) {
  const [fixedSample, setFixedSample] = useState<FixedSample>(
    p.customer ? 'CREATE_200' : 'READ_WAIT',
  );
  const [status, setStatus] = useState<AgentStatus | null>(null);
  const [view, setView] = useState<AgentView | null>(null);
  const [message, setMessage] = useState('');
  const [account, setAccount] = useState('');
  const [payeeQuery, setPayeeQuery] = useState('');
  const [lookupQuery, setLookupQuery] = useState('');
  const [payees, setPayees] = useState<Payee[]>([]);
  const [payeeId, setPayeeId] = useState('');
  const [taskId, setTaskId] = useState(() => stored(key));
  const [uncertain, setUncertain] = useState(() => stored(uncertainKey) === '1');
  const [fresh, setFresh] = useState(false);
  const [notice, setNotice] = useState('');
  const [querying, setQuerying] = useState(false);
  const [messages, setMessages] = useState<string[]>([]);
  const [original, setOriginal] = useState<PaymentOriginal | null>(null);
  const [now, setNow] = useState(Date.now());
  const [recoveryStore] = useState(() => new PaymentRecoveryStore(() => sessionStorage));
  const [recoveryState, setRecoveryState] = useState(recoveryStore.state);
  const recovery = useRef(recoveryState);
  const [funds, setFunds] = useState<PaymentFunds | null>(null);
  const knownFunds = useRef<PaymentFunds | null>(null);
  const mounted = useRef(false);
  const lifecycle = useRef(0);
  const active = useRef(p.authenticated);
  active.current = p.authenticated;
  // A boolean mount flag can become true again during StrictMode effect replay.
  // Capture this lifetime as well as the parent's synchronous session generation.
  const captureValidity = useCallback(() => {
    const lifetime = lifecycle.current;
    const epoch = p.getGeneration();
    return () =>
      mounted.current &&
      active.current &&
      lifetime === lifecycle.current &&
      epoch === p.getGeneration();
  }, [p.getGeneration]);
  // Each known original has its own selector and current-session outcome. Viewing
  // or resolving one original never removes another original from this ledger.
  const hasUnresolvedAttempt = useCallback(
    () => recovery.current.damaged || recovery.current.entries.some((e) => !isResolved(e)),
    [],
  );
  const tracks = (reference: FundsReference) =>
    recovery.current.entries.some((e) => sameFundsReference(e.reference, reference));
  const syncRecovery = useCallback(() => {
    recovery.current = recoveryStore.state;
    setRecoveryState(recoveryStore.state);
  }, [recoveryStore]);
  const publishRecovery = useCallback(
    (
      entries: RecoveryEntry[],
      completedTask?: string,
      completedReference?: FundsReference,
      nonPaymentProjection = false,
    ) => {
      if (!mounted.current || !active.current) return false;
      const saved = recoveryStore.commit(
        entries,
        completedTask,
        completedReference,
        nonPaymentProjection,
      );
      syncRecovery();
      return saved;
    },
    [recoveryStore, syncRecovery],
  );
  const beginRecoveryRead = useCallback(
    (id: string, reference: FundsReference | null = null) => {
      if (!mounted.current || !active.current) return false;
      const ready = recoveryStore.beginRead(id, reference);
      syncRecovery();
      if (!ready)
        setNotice('无法保存原单查询的恢复入口，请先恢复本页存储；已保留原号仍可只读查询。');
      return ready;
    },
    [recoveryStore, syncRecovery],
  );
  const readAgent = useCallback(
    (id: string, signal: AbortSignal, valid: () => boolean) => {
      if (!valid() || signal.aborted) throw new RuntimeTransportError('ABORTED');
      if (!beginRecoveryRead(id)) throw new Error('Payment recovery storage unavailable');
      return getAgent(id, signal).catch((error) => {
        // This failed task projection revealed no new original. A pending exact
        // funds query is distinct and cannot be cleared by this generic GET.
        // Check BEFORE writing: an unmounted store may hold an older ledger than
        // its replacement. Interrupted/invalidated reads remain for replay.
        if (
          valid() &&
          !signal.aborted &&
          !(error instanceof RuntimeApiError && error.reason === 'NOT_AUTHENTICATED') &&
          !(error instanceof RuntimeTransportError && ['ABORTED', 'TIMEOUT'].includes(error.reason))
        )
          publishRecovery(recovery.current.entries, id);
        throw error;
      });
    },
    [beginRecoveryRead, publishRecovery],
  );
  const rememberAttempt = useCallback(
    (reference: FundsReference) => {
      publishRecovery(retain(recovery.current.entries, reference));
      return !recovery.current.damaged;
    },
    [publishRecovery],
  );
  const observeRecovery = useCallback(
    (result: PaymentFunds) => {
      const entries = observe(recovery.current.entries, result);
      publishRecovery(entries, result.task_id, {
        task_id: result.task_id,
        operation: { operation_id: result.preparation_operation_id },
        snapshot_digest: result.snapshot_digest,
        snapshot: { transfer: result.transfer },
      });
      if (entries.some((e) => fundsMatches(e.reference, result))) {
        if (['PROCESSING', 'SUCCEEDED', 'FAILED'].includes(result.state)) {
          setUncertain(false);
          save(uncertainKey, '');
        }
      }
    },
    [publishRecovery],
  );
  const recordFunds = useCallback(
    (result: PaymentFunds) => {
      observeRecovery(result);
      knownFunds.current = result;
      setFunds(result);
    },
    [observeRecovery],
  );
  const current = useRef<AgentView | null>(null);
  // A later stale projection must not erase an observed business order.
  const recoveryOriginals = useRef(new Map<string, PaymentOriginal>());
  const draftContext = useRef<LifecycleView | null>(null);
  const selected = useRef(stored(key));
  const queryingRef = useRef(false);
  const accept = useCallback(
    (v: AgentView) => {
      if (
        v.kind === 'DRAFT' &&
        !v.planning_source &&
        current.current?.kind === 'DRAFT' &&
        current.current.draft.operation_id === v.draft.operation_id
      ) {
        v = { ...v, planning_source: current.current.planning_source };
      }
      const id = viewId(v);
      if (id) {
        selected.current = id;
        save(key, id);
        setTaskId(id);
      }
      current.current = v;
      if (v.kind === 'DRAFT' || v.kind === 'PAYMENT') draftContext.current = v.draft;
      else if (v.kind === 'READ') draftContext.current = null;
      setView(v);
      if (v.kind === 'PAYMENT' && v.payment_flow.funds_result)
        recordFunds(v.payment_flow.funds_result);
      else if (knownFunds.current?.task_id !== id) {
        knownFunds.current = null;
        setFunds(null);
      }
      if (v.kind === 'PAYMENT' && originalRequiresRecovery(v.payment_flow.original)) {
        recoveryOriginals.current.set(id!, v.payment_flow.original!);
        if (!v.payment_flow.funds_result) observeRecovery(originalFunds(v.payment_flow.original!));
      }
      setOriginal(id ? (recoveryOriginals.current.get(id) ?? null) : null);
      setFresh(true);
      publishRecovery(recovery.current.entries, id ?? undefined, undefined, v.kind !== 'PAYMENT');
      if (!recovery.current.entries.length && !recovery.current.damaged) {
        setUncertain(false);
        save(uncertainKey, '');
      }
    },
    [recordFunds, observeRecovery, publishRecovery],
  );
  const fail = (e: unknown) => {
    setNotice(errorText(e));
    setFresh(false);
    if (e instanceof RuntimeApiError && e.reason === 'NOT_AUTHENTICATED') p.onAuthenticationLost();
  };
  const refresh = useCallback(
    async (id: string) => {
      if (!active.current || !id || queryingRef.current || p.isBusy()) return;
      const valid = captureValidity();
      queryingRef.current = true;
      setQuerying(true);
      setFresh(false);
      try {
        const received = await p.runRequest((s) => readAgent(id, s, valid));
        if (valid()) {
          accept(received);
          // Reload may have interrupted a task GET before its original could
          // be journalled. A stale PAYMENT projection is insufficient: replay
          // its read-only original lookup before completing that intent.
          if (
            received.kind === 'PAYMENT' &&
            recovery.current.reads.some((r) => r.task_id === id && !r.reference)
          ) {
            if (!beginRecoveryRead(id, received.payment)) return;
            if (received.payment.execution) {
              const result = await p.runRequest((s) => queryPaymentFunds(received.payment, s));
              if (valid()) recordFunds(result);
            } else {
              const result = await p.runRequest((s) => queryPaymentOriginal(received, s));
              if (valid()) {
                if (originalRequiresRecovery(result)) observeRecovery(originalFunds(result));
                else publishRecovery(recovery.current.entries, id, received.payment);
              }
            }
          }
          if (valid()) setNotice('');
        }
      } catch (e) {
        if (valid()) {
          setNotice(errorText(e));
          if (e instanceof RuntimeApiError && e.reason === 'NOT_AUTHENTICATED')
            p.onAuthenticationLost();
        }
      } finally {
        if (valid()) {
          queryingRef.current = false;
          setQuerying(false);
        }
      }
    },
    [
      p.getGeneration,
      p.isBusy,
      p.runRequest,
      p.onAuthenticationLost,
      captureValidity,
      accept,
      readAgent,
      beginRecoveryRead,
      recordFunds,
      observeRecovery,
      publishRecovery,
    ],
  );
  useEffect(() => {
    lifecycle.current += 1;
    mounted.current = true;
    return () => {
      mounted.current = false;
      lifecycle.current += 1;
      queryingRef.current = false;
    };
  }, []);
  useEffect(() => {
    queryingRef.current = false;
    current.current = null;
    recoveryOriginals.current.clear();
    draftContext.current = null;
    setView(null);
    setFresh(false);
    setQuerying(false);
    setStatus(null);
    setFixedSample(p.customer ? 'CREATE_200' : 'READ_WAIT');
    setMessage('');
    setMessages([]);
    setPayees([]);
    setPayeeId('');
    setAccount('');
    setNotice('');
    setOriginal(null);
    setFunds(null);
    knownFunds.current = null;
    recoveryStore.resetOutcomes();
    syncRecovery();
    if (!p.authenticated) return;
    const valid = captureValidity();
    void p
      .runRequest((s) => agentStatus(s))
      .then((s) => {
        if (valid()) setStatus(s);
      })
      .catch((e) => {
        if (valid()) fail(e);
      });
    if (selected.current) void refresh(selected.current);
  }, [p.authenticated, p.generation]);
  useEffect(() => {
    if (
      (view?.kind === 'DRAFT' || view?.kind === 'PAYMENT') &&
      view.draft.next_action.kind === 'SELECT_ACTION'
    )
      setFixedSample('MODIFY_300');
  }, [view]);
  useEffect(() => {
    if (view?.kind !== 'PAYMENT') return;
    setNow(Date.now());
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [view]);
  async function command(name: AgentCommand, action: (valid: () => boolean) => Promise<void>) {
    if (!p.authenticated || queryingRef.current || !p.beginCommand(name)) return;
    const valid = captureValidity();
    setNotice('');
    try {
      await action(valid);
    } catch (e) {
      if (valid()) {
        fail(e);
        if (!valid()) return;
        if (e instanceof RuntimeApiError && e.taskId) {
          selected.current = e.taskId;
          setTaskId(e.taskId);
          save(key, e.taskId);
        }
        if (
          (name === 'agent_send' ||
            (name === 'agent_action' && current.current?.kind === 'PAYMENT')) &&
          !(e instanceof RuntimeApiError && e.validated && !e.taskId)
        ) {
          if (
            !knownFunds.current ||
            !['PROCESSING', 'SUCCEEDED', 'FAILED'].includes(knownFunds.current.state)
          ) {
            setUncertain(true);
            save(uncertainKey, '1');
          }
        }
      }
    } finally {
      if (valid()) p.finishCommand(name);
    }
  }
  async function send() {
    const fixed = status?.planning_mode === 'FIXED_PLAN';
    const text = fixed
      ? (p.customer ? customerSampleLabels : sampleLabels)[fixedSample]
      : message.trim();
    if (
      !text ||
      uncertain ||
      hasUnresolvedAttempt() ||
      recovery.current.entries.some((e) => e.reference.task_id === selected.current)
    )
      return;
    const previous = current.current;
    if (
      previous &&
      (recoveryOriginals.current.has(viewId(previous) ?? '') ||
        (previous.kind === 'PAYMENT' && previous.payment.business_recovery_required))
    )
      return;
    const planningText =
      previous?.kind === 'CLARIFY' ? [...messages.slice(-8), text].join('\n') : text;
    if (new TextEncoder().encode(planningText).length > 4096) {
      setNotice('补充内容过长，请新建任务后重新提交。');
      return;
    }
    if (previous && previous.kind !== 'CLARIFY' && (!fresh || previous.kind === 'READ')) return;
    await command('agent_send', async (valid) => {
      if (selected.current && !beginRecoveryRead(selected.current)) return;
      setFresh(false);
      setMessages((m) => [...m.slice(-15), text]);
      setMessage('');
      let received = await p.runRequest((s) => {
        const selection = {
          account_id: account.trim() || null,
          payee_query: payeeId ? lookupQuery : null,
          payee_id: payeeId || null,
        };
        const context =
          previous?.kind === 'DRAFT' || previous?.kind === 'PAYMENT'
            ? previous.draft
            : previous?.kind === 'CLARIFY'
              ? draftContext.current
              : null;
        return fixed
          ? sampleAgent(fixedSample, selection, context, s)
          : sendAgent(planningText, selection, context, s);
      });
      if (!valid()) return;
      accept(received);
      if (received.kind === 'READ' && received.can_start) {
        received = await p.runRequest((s) =>
          runRead(received as Extract<AgentView, { kind: 'READ' }>, false, s),
        );
        if (valid()) accept(received);
      } else if (
        received.kind === 'DRAFT' &&
        received.draft.next_action.kind === 'PREPARE_OR_REPLACE_ACTION'
      ) {
        const prepared = await p.runRequest((s) => prepareAgent(received.draft!, s));
        if (valid()) accept(prepared);
      }
    });
  }
  async function draftAction(action: 'CONFIRM' | 'REJECT' | 'EXECUTE') {
    const v = current.current;
    if (
      !fresh ||
      hasUnresolvedAttempt() ||
      v?.kind !== 'DRAFT' ||
      recoveryOriginals.current.has(viewId(v) ?? '') ||
      recovery.current.entries.some((e) => e.reference.task_id === viewId(v))
    )
      return;
    await command('agent_action', async (valid) => {
      if (!beginRecoveryRead(v.draft.task_id)) return;
      setFresh(false);
      let next = await p.runRequest((s) => actDraft(v.draft, action, s));
      if (!valid()) return;
      accept(next);
      if (
        action === 'CONFIRM' &&
        next.kind === 'DRAFT' &&
        next.draft.next_action.kind === 'EXECUTE'
      ) {
        next = await p.runRequest((s) => actDraft(next.draft!, 'EXECUTE', s));
        if (valid()) accept(next);
      }
    });
  }
  const draft = view?.kind === 'DRAFT' ? view.draft : null;
  const paymentView = view?.kind === 'PAYMENT' ? view : null;
  const paymentExpired = !!paymentView && Date.parse(paymentView.payment.expires_at) <= now;
  const recoveringOriginal =
    hasUnresolvedAttempt() ||
    (!!paymentView &&
      !!funds &&
      fundsMatches(paymentView.payment, funds) &&
      fundsRequireRecovery(funds)) ||
    originalRequiresRecovery(original) ||
    recoveryState.entries.some((e) => e.reference.task_id === (view ? viewId(view) : null));
  async function readFunds(reference: FundsReference) {
    await command('agent_action', async (valid) => {
      if (!beginRecoveryRead(reference.task_id, reference)) return;
      const received = await p.runRequest((s) => queryPaymentFunds(reference, s));
      if (!valid()) return;
      if (current.current?.kind !== 'PAYMENT' || fundsMatches(current.current.payment, received))
        recordFunds(received);
      else observeRecovery(received);
      // Read history first. A failed Agent refresh cannot erase an original.
      if (current.current?.kind === 'PAYMENT' && reference.task_id === selected.current) {
        const next = await p.runRequest((s) => readAgent(reference.task_id, s, valid));
        if (valid()) accept(next);
      }
    });
  }
  async function pay() {
    const v = current.current;
    if (
      !fresh ||
      uncertain ||
      hasUnresolvedAttempt() ||
      v?.kind !== 'PAYMENT' ||
      (!!knownFunds.current &&
        fundsMatches(v.payment, knownFunds.current) &&
        fundsRequireRecovery(knownFunds.current)) ||
      tracks(v.payment) ||
      !status?.funds_execution_enabled
    )
      return;
    await command('agent_action', async (valid) => {
      // Persist the recovery selector before starting POST. It survives response
      // loss, reload and session changes; an old Agent view cannot clear it.
      const ref = fundsReference(v.payment);
      if (!rememberAttempt(ref)) return;
      setFunds(null);
      knownFunds.current = null;
      setFresh(false);
      setUncertain(true);
      save(uncertainKey, '1');
      const received = await p.runRequest((s) => executePayment(v, s));
      if (!valid()) return;
      recordFunds(received);
      const original = await p.runRequest((s) => queryPaymentFunds(v.payment, s));
      if (!valid()) return;
      recordFunds(original);
      const next = await p.runRequest((s) => readAgent(v.payment.task_id, s, valid));
      if (valid()) accept(next);
    });
  }
  async function paymentAction(action: 'CONFIRM' | 'REJECT' | 'EVALUATE') {
    const v = current.current;
    if (
      !fresh ||
      uncertain ||
      hasUnresolvedAttempt() ||
      v?.kind !== 'PAYMENT' ||
      (!!knownFunds.current &&
        fundsMatches(v.payment, knownFunds.current) &&
        fundsRequireRecovery(knownFunds.current)) ||
      recoveryOriginals.current.has(viewId(v) ?? '') ||
      tracks(v.payment)
    )
      return;
    await command('agent_action', async (valid) => {
      if (!beginRecoveryRead(v.payment.task_id)) return;
      setFresh(false);
      const next = await p.runRequest((s) => actPayment(v, action, s));
      if (valid()) accept(next);
    });
  }
  const fixed = status?.planning_mode === 'FIXED_PLAN';
  const planningAvailable = fixed || status?.configured;
  const enabled = p.authenticated && !p.busy && !querying;
  const unresolvedPayment = hasUnresolvedAttempt();
  const canStartNew = enabled && !uncertain && !unresolvedPayment;
  const writable =
    !recoveringOriginal &&
    (paymentView
      ? fresh &&
        paymentView.draft.next_action.kind === 'SELECT_ACTION' &&
        !paymentView.payment.business_recovery_required
      : !draft || (draft.next_action.kind === 'SELECT_ACTION' && fresh));
  const fields = draft?.target_parameters ?? draft?.parameters;
  const confirmationLabel =
    draft?.action === 'CANCEL_MOCK_DRAFT'
      ? '确认取消草案'
      : draft?.action === 'MODIFY_MOCK_DRAFT'
        ? '确认修改草案'
        : '确认保存草案';
  return (
    <div className="r-workspace a-workspace">
      <aside className="r-controls">
        {p.sessionControls}
        <section className="a-selection">
          <h2>{p.customer ? '付款对象' : '操作对象'}</h2>
          <label htmlFor="agent-account">{p.customer ? '付款账户编号' : '账户编号'}</label>
          <input
            id="agent-account"
            value={account}
            maxLength={128}
            disabled={!enabled}
            onChange={(e) => setAccount(e.target.value)}
            autoComplete="off"
          />
          <label htmlFor="agent-payee-query">收款人</label>
          <form
            className="a-search"
            onSubmit={(e) => {
              e.preventDefault();
              void command('agent_search', async (valid) => {
                const query = payeeQuery.trim();
                const found = await p.runRequest((s) => searchPayees(query, s));
                if (valid()) {
                  setPayees(found.candidates);
                  setLookupQuery(query);
                  setPayeeId('');
                }
              });
            }}
          >
            <input
              id="agent-payee-query"
              maxLength={64}
              value={payeeQuery}
              disabled={!enabled || !status?.draft_enabled}
              onChange={(e) => {
                setPayeeQuery(e.target.value);
                setPayeeId('');
                setPayees([]);
              }}
            />
            <button
              className="r-icon"
              title="查找收款人"
              aria-label="查找收款人"
              disabled={!enabled || !payeeQuery.trim() || !status?.draft_enabled}
            >
              <Search size={17} />
            </button>
          </form>
          {payees.length > 0 && (
            <select
              aria-label="选择收款人"
              value={payeeId}
              disabled={!enabled}
              onChange={(e) => setPayeeId(e.target.value)}
            >
              <option value="">请选择</option>
              {payees.map((payee) => (
                <option key={payee.payee_id} value={payee.payee_id}>
                  {payee.display_name} · {payee.bank_name} · {payee.masked_account_no}
                </option>
              ))}
            </select>
          )}
        </section>
        <section className="a-model-status">
          <Bot size={17} />
          <span>
            {fixed
              ? p.customer
                ? '固定办理选项 · 模拟模式'
                : '固定计划 / MockLlm'
              : status
                ? status.configured
                  ? '模型已配置'
                  : '模型尚未配置'
                : '规划模式未核实'}
          </span>
          {status && !status.draft_enabled && <span>草案能力未启用</span>}
          {!p.customer && status && !status.tools.includes('analyze_local_basic_spending') && (
            <span>账单报告来源未接入</span>
          )}
        </section>
      </aside>
      <section className="r-task-area a-conversation">
        <div className="r-section-heading">
          <h2>{p.customer ? '办理模拟付款' : 'Agent 助手'}</h2>
          <button
            className="r-icon"
            aria-label="新任务"
            title="新任务"
            disabled={!canStartNew}
            onClick={() => {
              if (!canStartNew || hasUnresolvedAttempt()) return;
              if (!publishRecovery([])) return;
              selected.current = '';
              current.current = null;
              draftContext.current = null;
              save(key, '');
              save(uncertainKey, '');
              setView(null);
              setOriginal(null);
              setFunds(null);
              knownFunds.current = null;
              setTaskId('');
              setFresh(false);
              setUncertain(false);
              setMessages([]);
              setFixedSample(p.customer ? 'CREATE_200' : 'READ_WAIT');
              setNotice('');
            }}
          >
            <Plus size={19} />
          </button>
        </div>
        <form
          className="r-query"
          onSubmit={(e) => {
            e.preventDefault();
            void refresh(taskId.trim());
          }}
        >
          <label htmlFor="agent-task">{p.customer ? '办理编号' : '任务号'}</label>
          <div className="r-query-input">
            <input
              id="agent-task"
              value={taskId}
              disabled={!enabled}
              onChange={(e) => {
                setTaskId(e.target.value);
                setFresh(false);
              }}
            />
            <button
              className="r-icon"
              title="查询原任务"
              aria-label="查询原任务"
              disabled={!enabled || !taskId.trim()}
            >
              <RefreshCw size={17} />
            </button>
          </div>
        </form>
        {notice && (
          <div className="r-notice error" role="alert">
            <CircleAlert size={18} />
            <p>{notice}</p>
          </div>
        )}
        {uncertain && !unresolvedPayment && (
          <p className="r-uncertain">提交结果未明{taskId ? ' · 请查询原任务' : ' · 暂无任务号'}</p>
        )}
        <div className="a-thread" aria-live="polite">
          {!p.customer && view?.planning_source && (
            <p className="a-expiry">
              规划来源：
              {view.planning_source === 'MOCK_LLM'
                ? '固定计划 / MockLlm'
                : view.planning_source === 'OPENAI_COMPATIBLE'
                  ? 'OpenAI-compatible Gateway'
                  : '未指定'}
            </p>
          )}
          {messages.map((text, i) => (
            <p className="a-user-message" key={i}>
              {text}
            </p>
          ))}
          {!view && !messages.length && (
            <div className="r-empty">
              <Bot size={32} />
              <h3>{p.authenticated ? '新的任务' : '尚未登录'}</h3>
            </div>
          )}
          {view?.kind === 'CLARIFY' && (
            <p className="a-clarification">{view.clarification.question}</p>
          )}
          {view?.kind === 'READ' && (
            <div className="a-read-result">
              <span className={`r-state ${view.task.task_state}`}>
                {view.task.task_state === 'COMPLETED' && <CheckCheck size={16} />}{' '}
                {labels[view.task.task_state]}
              </span>
              {view.results?.map((result) => (
                <div key={result.result_ref} className="a-result">
                  <ResultRows result={result} />
                  <code>{result.result_ref}</code>
                </div>
              ))}
              {view.task.next_action.kind === 'CONTINUE' && (
                <button
                  className="r-command primary"
                  disabled={!enabled || !fresh}
                  onClick={() =>
                    void command('agent_action', async (valid) => {
                      setFresh(false);
                      const next = await p.runRequest((s) => runRead(view, true, s));
                      if (valid()) accept(next);
                    })
                  }
                >
                  继续
                </button>
              )}
              {view.can_start && (
                <button
                  className="r-command primary"
                  disabled={!enabled || !fresh}
                  onClick={() =>
                    void command('agent_action', async (valid) => {
                      setFresh(false);
                      const next = await p.runRequest((s) => runRead(view, false, s));
                      if (valid()) accept(next);
                    })
                  }
                >
                  执行查询
                </button>
              )}
            </div>
          )}
          {paymentView && (
            <PaymentPanel
              view={paymentView}
              original={original}
              enabled={enabled}
              fresh={fresh}
              expired={paymentExpired}
              customer={p.customer}
              attempted={tracks(paymentView.payment)}
              writeBlocked={unresolvedPayment || uncertain}
              funds={funds && fundsMatches(paymentView.payment, funds) ? funds : null}
              onExecute={() => void pay()}
              onAction={(action) => void paymentAction(action)}
              onRefresh={() => void refresh(paymentView.payment.task_id)}
              onOriginal={() =>
                paymentView.payment.execution || tracks(paymentView.payment)
                  ? void readFunds(paymentView.payment)
                  : void command('agent_action', async (valid) => {
                      if (!beginRecoveryRead(paymentView.payment.task_id, paymentView.payment))
                        return;
                      const received = await p.runRequest((s) =>
                        queryPaymentOriginal(paymentView, s),
                      );
                      if (!valid()) return;
                      if (originalRequiresRecovery(received)) {
                        observeRecovery(originalFunds(received));
                        recoveryOriginals.current.set(paymentView.payment.task_id, received);
                        setOriginal(received);
                        setFresh(false);
                        const next = await p.runRequest((s) =>
                          readAgent(paymentView.payment.task_id, s, valid),
                        );
                        if (valid()) accept(next);
                      } else if (!recoveryOriginals.current.has(paymentView.payment.task_id)) {
                        setOriginal(received);
                        publishRecovery(
                          recovery.current.entries,
                          paymentView.payment.task_id,
                          paymentView.payment,
                        );
                      }
                    })
              }
            />
          )}
          {recoveryState.damaged && (
            <div role="alert">
              <p>
                付款恢复记录无法完整读取或保存。请保留原号并查询原任务，当前不能确认所有付款已核清，暂不可新办理。
              </p>
              <button disabled={!enabled} onClick={() => publishRecovery(recovery.current.entries)}>
                重试保存付款恢复记录
              </button>
            </div>
          )}
          {recoveryState.reads.map((read, index) => (
            <div key={`${read.task_id}-${index}`}>
              <p>待恢复查询：{read.task_id}</p>
              <button
                disabled={!enabled}
                onClick={() =>
                  read.reference ? void readFunds(read.reference) : void refresh(read.task_id)
                }
              >
                恢复原单查询
              </button>
            </div>
          ))}
          {recoveryState.entries.some(
            (e) => !paymentView || !sameFundsReference(e.reference, paymentView.payment),
          ) && (
            <section aria-label="付款恢复">
              <p>
                已保留付款原号。还有{recoveryState.entries.filter((e) => !isResolved(e)).length}
                笔尚未核清；每笔须分别查询，查看其他任务不会解除付款保护。
              </p>
              {recoveryState.entries
                .filter(
                  (e) => !paymentView || !sameFundsReference(e.reference, paymentView.payment),
                )
                .map((entry) => (
                  <article
                    aria-label={`保留付款 ${entry.reference.snapshot.transfer.operation_id}`}
                    key={JSON.stringify(entry.reference)}
                  >
                    <p>
                      {entry.reference.snapshot.transfer.amount} CNY · 原号{' '}
                      {entry.reference.snapshot.transfer.operation_id} ·{' '}
                      {isResolved(entry) ? '已核清' : '尚未核清'}
                    </p>
                    <button
                      className="r-command"
                      disabled={!enabled}
                      onClick={() => void readFunds(entry.reference)}
                    >
                      查询保留的付款原单
                    </button>
                    {entry.outcome && <FundsResult result={entry.outcome} />}
                  </article>
                ))}
            </section>
          )}
          {draft && (
            <section className="a-draft-result">
              <span className={`r-state ${draft.task_state}`}>
                {draft.execution_status === 'RESULT_UNKNOWN'
                  ? '结果未知'
                  : labels[draft.task_state]}
              </span>
              <h3>
                {draft.action === 'CANCEL_MOCK_DRAFT'
                  ? '取消模拟草案'
                  : draft.action === 'MODIFY_MOCK_DRAFT'
                    ? '修改模拟草案'
                    : '保存模拟草案'}
              </h3>
              <dl className="a-draft-fields">
                {[
                  ['payer_account_id', '付款账户'],
                  ['payee_id', '收款人'],
                  ['amount', '金额'],
                  ['currency', '币种'],
                  ['remark', '备注'],
                ].map(([key, label]) =>
                  fields && key in fields ? (
                    <div key={key}>
                      <dt>{label}</dt>
                      <dd>{String(fields[key] ?? '')}</dd>
                    </div>
                  ) : null,
                )}
                {!p.customer && (
                  <div>
                    <dt>计划版本</dt>
                    <dd>{draft.plan_version}</dd>
                  </div>
                )}
                {!p.customer && (
                  <div>
                    <dt>操作号</dt>
                    <dd>
                      <code>{draft.operation_id}</code>
                    </dd>
                  </div>
                )}
              </dl>
              {draft.receipt && (
                <p className="a-receipt">
                  <CheckCheck size={17} />
                  历史回执 ·{' '}
                  {draft.action === 'CANCEL_MOCK_DRAFT'
                    ? '草案已取消'
                    : draft.action === 'MODIFY_MOCK_DRAFT'
                      ? '草案已修改'
                      : '草案已保存'}{' '}
                  · 草案版本 {draft.receipt.draft_version}
                  {!p.customer && <code>{draft.receipt.draft_id}</code>}
                </p>
              )}
              {draft.confirmation && (
                <p className="a-expiry">
                  确认有效至 {new Date(draft.confirmation.expires_at).toLocaleString('zh-CN')}
                </p>
              )}
              {draft.next_action.kind === 'CONFIRM' && (
                <div className="a-actions">
                  <button
                    className="r-command primary"
                    disabled={!enabled || !fresh || recoveringOriginal}
                    onClick={() => void draftAction('CONFIRM')}
                  >
                    <FileCheck size={17} />
                    {confirmationLabel}
                  </button>
                  <button
                    className="r-command"
                    disabled={!enabled || !fresh || recoveringOriginal}
                    onClick={() => void draftAction('REJECT')}
                  >
                    <X size={17} />
                    拒绝本次操作
                  </button>
                </div>
              )}
              {draft.next_action.kind === 'EXECUTE' && (
                <button
                  className="r-command primary"
                  disabled={!enabled || !fresh || recoveringOriginal}
                  onClick={() => void draftAction('EXECUTE')}
                >
                  <FileCheck size={17} />
                  执行已确认操作
                </button>
              )}
              {draft.next_action.kind === 'PREPARE_OR_REPLACE_ACTION' && (
                <button
                  className="r-command"
                  disabled={!enabled || !fresh || recoveringOriginal}
                  onClick={() =>
                    void command('agent_action', async (valid) => {
                      if (hasUnresolvedAttempt() || recoveryOriginals.current.has(draft.task_id))
                        return;
                      setFresh(false);
                      const next = await p.runRequest((s) => prepareAgent(draft, s));
                      if (valid()) accept(next);
                    })
                  }
                >
                  核对确认单
                </button>
              )}
            </section>
          )}
        </div>
        <form
          className="a-composer"
          onSubmit={(e) => {
            e.preventDefault();
            void send();
          }}
        >
          <label htmlFor="agent-message">
            {fixed
              ? p.customer
                ? '选择办理事项'
                : '固定计划样例'
              : draft
                ? '下一步请求'
                : '任务请求'}
          </label>
          <div>
            {fixed ? (
              <select
                id="agent-message"
                value={fixedSample}
                disabled={!enabled || !writable || view?.kind === 'READ' || uncertain}
                onChange={(e) => setFixedSample(e.target.value as FixedSample)}
              >
                {(draftContext.current?.receipt
                  ? ([
                      'MODIFY_300',
                      'CANCEL_DRAFT',
                      ...(status?.payment_planning_enabled &&
                      draftContext.current.action !== 'CANCEL_MOCK_DRAFT'
                        ? ['EXECUTE_TRANSFER' as const]
                        : []),
                    ] as const)
                  : p.customer
                    ? (['CREATE_200'] as const)
                    : (['READ_WAIT', 'CREATE_200'] as const)
                ).map((sample) => (
                  <option
                    key={sample}
                    value={sample}
                    disabled={sample === 'CREATE_200' && !status?.draft_enabled}
                  >
                    {(p.customer ? customerSampleLabels : sampleLabels)[sample]}
                  </option>
                ))}
              </select>
            ) : (
              <textarea
                id="agent-message"
                value={message}
                maxLength={4096}
                rows={3}
                disabled={
                  !enabled || !planningAvailable || !writable || view?.kind === 'READ' || uncertain
                }
                onChange={(e) => setMessage(e.target.value)}
              />
            )}
            <button
              className="r-icon"
              title={fixed ? (p.customer ? '办理所选事项' : '运行样例') : '发送请求'}
              aria-label={fixed ? (p.customer ? '办理所选事项' : '运行样例') : '发送请求'}
              disabled={
                !enabled ||
                (!fixed && !message.trim()) ||
                !planningAvailable ||
                !writable ||
                view?.kind === 'READ' ||
                uncertain
              }
            >
              {p.busy === 'agent_send' ? (
                <LoaderCircle className="r-spinning" size={20} />
              ) : (
                <Send size={20} />
              )}
            </button>
          </div>
        </form>
      </section>
    </div>
  );
}
