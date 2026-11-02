import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import {
  ArrowRight,
  Bot,
  CheckCheck,
  CircleAlert,
  Clock3,
  LoaderCircle,
  MessageCircleQuestion,
  RefreshCw,
  Search,
  Send,
  ShieldCheck,
  WifiOff,
} from 'lucide-react';
import { continueTask, RuntimeApiError } from './client';
import { executePlanning, getPlannerStatus, getPlanning, submitPlanning } from './planning-client';
import type { PlannerStatus, PlanningView } from './planning-schema';

const taskKey = 'safeflow.runtime.planning.last-task-id.v1';
const uncertainKey = 'safeflow.runtime.planning.uncertain-create.v1';
export type PlanningCommand = 'planning_submit' | 'planning_execute' | 'planning_continue';
type Notice = { kind: 'error' | 'info'; text: string } | null;
type Props = {
  authenticated: boolean;
  generation: number;
  busy: string | null;
  sessionControls: ReactNode;
  runRequest: <T>(action: (signal: AbortSignal) => Promise<T>) => Promise<T>;
  getGeneration: () => number;
  isBusy: () => boolean;
  beginCommand: (command: PlanningCommand) => boolean;
  finishCommand: (command: PlanningCommand) => void;
  onAuthenticationLost: () => void;
};

function stored(key: string) {
  try {
    return sessionStorage.getItem(key);
  } catch {
    return null;
  }
}
function persist(key: string, value: string | null) {
  try {
    if (value === null) sessionStorage.removeItem(key);
    else sessionStorage.setItem(key, value);
  } catch {
    /* Input and model content remain in memory only. */
  }
}
export function clearPlanningReferences() {
  persist(taskKey, null);
  persist(uncertainKey, null);
}
function safeError(error: unknown) {
  if (error instanceof RuntimeApiError) {
    switch (error.reason) {
      case 'LLM_NOT_CONFIGURED':
        return '模型规划服务尚未配置。';
      case 'LLM_GATEWAY_UNAVAILABLE':
        return '模型规划服务暂不可用。';
      case 'LLM_PROPOSAL_REJECTED':
        return '模型提案未通过安全接收校验。';
      case 'NOT_AUTHENTICATED':
        return '模拟会话未登录或已失效。';
      case 'NOT_FOUND':
        return '规划任务不存在或不属于当前会话。';
      case 'STALE_PLAN':
        return '计划版本或来源已变化。';
      case 'IDEMPOTENCY_CONFLICT':
        return '请求与已记录的执行冲突。';
      case 'FORBIDDEN':
        return '请求被公共安全校验拒绝。';
      case 'DEPENDENCY_UNAVAILABLE':
        return '规划任务依赖暂不可用。';
      default:
        return '请求未通过校验。';
    }
  }
  return '服务响应暂未核实。';
}

export function PlanningSummary({ view }: { view: PlanningView }) {
  if (view.kind === 'CLARIFY')
    return (
      <div className="r-planning-clarification">
        <span className="r-state PREPARED">
          <MessageCircleQuestion size={17} />
          模型需要澄清
        </span>
        <p>{view.clarification.question}</p>
      </div>
    );
  const task = view.task;
  const state = view.can_start
    ? '计划已接收'
    : task.task_state === 'COMPLETED'
      ? '已完成'
      : task.task_state === 'WAITING_USER'
        ? '等待继续'
        : task.task_state === 'FAILED'
          ? '已失败'
          : task.task_state === 'CANCELLED'
            ? '已取消'
            : task.task_state === 'WAITING_EXTERNAL'
              ? '等待外部结果'
              : '运行中';
  const description = view.can_start
    ? '计划已接收，尚未执行'
    : task.task_state === 'COMPLETED'
      ? '只读计划已完成'
      : task.task_state === 'WAITING_USER'
        ? '只读计划等待继续'
        : task.task_state === 'FAILED'
          ? '只读任务已停止'
          : task.task_state === 'CANCELLED'
            ? '只读任务已取消'
            : '当前进度以服务端记录为准';
  return (
    <>
      <span className={`r-state ${view.can_start ? 'PLAN_ACCEPTED' : task.task_state}`}>
        {task.task_state === 'COMPLETED' ? <CheckCheck size={17} /> : <Clock3 size={17} />}
        {state}
      </span>
      <h3 className="r-planning-heading">{description}</h3>
      <code className="r-task-id">{task.task_id}</code>
      <dl className="r-task-facts">
        <div>
          <dt>任务修订</dt>
          <dd>{task.revision}</dd>
        </div>
        <div>
          <dt>计划版本</dt>
          <dd>{task.plan_version}</dd>
        </div>
        <div>
          <dt>事实版本</dt>
          <dd>{task.facts_version}</dd>
        </div>
      </dl>
      {task.recovery.kind === 'AWAIT_USER' && (
        <div className="r-waiting-node">
          <span>等待节点</span>
          <code>{task.recovery.node_id}</code>
        </div>
      )}
      {!view.can_start && task.next_action.kind === 'REVIEW_ORIGINAL_TASK' && (
        <p className="r-recovery">
          <RefreshCw size={16} />
          等待核对原任务结果
        </p>
      )}
    </>
  );
}

export function PlanningWorkspace({
  authenticated,
  generation,
  busy,
  sessionControls,
  runRequest,
  getGeneration,
  isBusy,
  beginCommand,
  finishCommand,
  onAuthenticationLost,
}: Props) {
  const [message, setMessage] = useState('');
  const [taskId, setTaskId] = useState(() => stored(taskKey) ?? '');
  const [view, setView] = useState<PlanningView | null>(null);
  const [status, setStatus] = useState<PlannerStatus | null>(null);
  const [fresh, setFresh] = useState(false);
  const [querying, setQuerying] = useState(false);
  const [statusChecking, setStatusChecking] = useState(false);
  const [notice, setNotice] = useState<Notice>(null);
  const [uncertainCreate, setUncertainCreate] = useState(() => stored(uncertainKey) === '1');
  const selectedId = useRef(stored(taskKey) ?? '');
  const currentView = useRef<PlanningView | null>(null);
  const authenticatedRef = useRef(authenticated);
  authenticatedRef.current = authenticated;
  const mounted = useRef(false);
  const queryInFlight = useRef(false);
  const statusInFlight = useRef(false);

  const clearView = useCallback(() => {
    currentView.current = null;
    setView(null);
    setFresh(false);
  }, []);
  const rememberId = useCallback((id: string, updateInput: boolean) => {
    selectedId.current = id;
    persist(taskKey, id);
    if (updateInput) setTaskId(id);
  }, []);
  const acceptView = useCallback(
    (received: PlanningView, updateInput: boolean) => {
      const previous = currentView.current;
      if (
        previous?.kind === 'TASK' &&
        received.kind === 'TASK' &&
        previous.task.task_id === received.task.task_id &&
        previous.task.revision > received.task.revision
      )
        return;
      currentView.current = received;
      setView(received);
      setFresh(true);
      if (received.kind === 'TASK') rememberId(received.task.task_id, updateInput);
      else {
        selectedId.current = '';
        persist(taskKey, null);
        if (updateInput) setTaskId('');
      }
    },
    [rememberId],
  );

  const queryOriginal = useCallback(
    async (id: string, quiet = false) => {
      if (!id || !authenticatedRef.current || queryInFlight.current) return null;
      const startedGeneration = getGeneration();
      queryInFlight.current = true;
      setQuerying(true);
      if (selectedId.current !== id) clearView();
      rememberId(id, !quiet);
      if (!quiet) {
        setFresh(false);
        setNotice(null);
      }
      try {
        const received = await runRequest((signal) => getPlanning(id, signal));
        if (!mounted.current || startedGeneration !== getGeneration() || selectedId.current !== id)
          return null;
        acceptView(received, !quiet);
        return received;
      } catch (error) {
        if (!mounted.current || startedGeneration !== getGeneration()) return null;
        clearView();
        if (error instanceof RuntimeApiError && error.reason === 'NOT_AUTHENTICATED') {
          if (error.taskId) rememberId(error.taskId, !quiet);
          onAuthenticationLost();
        } else if (error instanceof RuntimeApiError && error.reason === 'NOT_FOUND') {
          selectedId.current = '';
          persist(taskKey, null);
          if (!quiet) setTaskId('');
        }
        setNotice({ kind: 'error', text: safeError(error) });
        return null;
      } finally {
        queryInFlight.current = false;
        if (mounted.current) setQuerying(false);
      }
    },
    [acceptView, clearView, getGeneration, onAuthenticationLost, rememberId, runRequest],
  );

  const refreshStatus = useCallback(async () => {
    if (!authenticatedRef.current || statusInFlight.current) return;
    const startedGeneration = getGeneration();
    statusInFlight.current = true;
    setStatusChecking(true);
    try {
      const received = await runRequest(getPlannerStatus);
      if (!mounted.current || startedGeneration !== getGeneration()) return;
      setStatus(received);
    } catch (error) {
      if (!mounted.current || startedGeneration !== getGeneration()) return;
      setStatus(null);
      if (error instanceof RuntimeApiError && error.reason === 'NOT_AUTHENTICATED') {
        clearView();
        onAuthenticationLost();
      }
      setNotice({ kind: 'error', text: safeError(error) });
    } finally {
      statusInFlight.current = false;
      if (mounted.current) setStatusChecking(false);
    }
  }, [clearView, getGeneration, onAuthenticationLost, runRequest]);

  useEffect(() => {
    mounted.current = true;
    const timer = setInterval(() => {
      if (!isBusy() && authenticatedRef.current && selectedId.current)
        void queryOriginal(selectedId.current, true);
    }, 4000);
    return () => {
      mounted.current = false;
      clearInterval(timer);
    };
  }, [isBusy, queryOriginal]);

  useEffect(() => {
    clearView();
    setStatus(null);
    setMessage('');
    selectedId.current = stored(taskKey) ?? '';
    if (!selectedId.current) setTaskId('');
    setUncertainCreate(stored(uncertainKey) === '1');
    if (authenticated) {
      void refreshStatus();
      if (selectedId.current) void queryOriginal(selectedId.current, true);
    }
  }, [authenticated, generation, clearView, queryOriginal, refreshStatus]);

  async function execute(command: PlanningCommand) {
    const original = currentView.current;
    const byteLength = new TextEncoder().encode(message).byteLength;
    if (queryInFlight.current || !authenticated) return;
    if (
      command === 'planning_submit' &&
      (!status?.configured || uncertainCreate || !message.trim() || byteLength > 4096)
    )
      return;
    if (command !== 'planning_submit' && (!fresh || original?.kind !== 'TASK')) return;
    if (command === 'planning_execute' && original?.kind === 'TASK' && !original.can_start) return;
    if (
      command === 'planning_continue' &&
      (original?.kind !== 'TASK' || original.task.next_action.kind !== 'CONTINUE')
    )
      return;
    if (!beginCommand(command)) return;
    const startedGeneration = getGeneration();
    setNotice(null);
    setFresh(false);
    if (command === 'planning_submit') {
      clearView();
      selectedId.current = '';
      setTaskId('');
      persist(taskKey, null);
      persist(uncertainKey, '1');
      setUncertainCreate(true);
    }
    try {
      const received = await runRequest(async (signal) => {
        if (command === 'planning_submit') return submitPlanning(message, signal);
        if (command === 'planning_execute') return executePlanning(original!, signal);
        if (original?.kind !== 'TASK') throw new Error('NO_PLANNING_TASK');
        await continueTask(original.task, signal);
        return getPlanning(original.task.task_id, signal);
      });
      if (!mounted.current || startedGeneration !== getGeneration()) return;
      acceptView(received, true);
      if (command === 'planning_submit') {
        persist(uncertainKey, null);
        setUncertainCreate(false);
      }
    } catch (error) {
      if (!mounted.current || startedGeneration !== getGeneration()) return;
      clearView();
      const originalId =
        command !== 'planning_submit' && original?.kind === 'TASK'
          ? original.task.task_id
          : undefined;
      const knownId = error instanceof RuntimeApiError ? (error.taskId ?? originalId) : originalId;
      if (knownId) rememberId(knownId, true);
      if (command === 'planning_submit' && error instanceof RuntimeApiError && error.validated) {
        persist(uncertainKey, null);
        setUncertainCreate(false);
        if (error.reason === 'LLM_NOT_CONFIGURED') setStatus(null);
      }
      if (error instanceof RuntimeApiError && error.reason === 'NOT_AUTHENTICATED') {
        onAuthenticationLost();
        setNotice({ kind: 'error', text: safeError(error) });
        return;
      }
      setNotice({
        kind: 'error',
        text:
          knownId || (error instanceof RuntimeApiError && error.validated)
            ? safeError(error)
            : '规划结果未明，暂无任务号。',
      });
      if (knownId) {
        const recovered = await queryOriginal(knownId, true);
        if (recovered && !(error instanceof RuntimeApiError))
          setNotice({ kind: 'info', text: '规划原号已核验。' });
      }
    } finally {
      finishCommand(command);
    }
  }

  const byteLength = new TextEncoder().encode(message).byteLength;
  const canSubmit =
    authenticated && status?.configured && message.trim() && byteLength <= 4096 && !uncertainCreate;
  const canStart = authenticated && fresh && view?.kind === 'TASK' && view.can_start;
  const canContinue =
    authenticated && fresh && view?.kind === 'TASK' && view.task.next_action.kind === 'CONTINUE';

  return (
    <div className="r-workspace r-planning-workspace">
      {notice && (
        <div
          className={`r-notice r-draft-notice ${notice.kind}`}
          role={notice.kind === 'error' ? 'alert' : 'status'}
        >
          <CircleAlert size={18} />
          <p>{notice.text}</p>
        </div>
      )}
      <aside className="r-controls">
        {sessionControls}
        <section aria-labelledby="planning-input-title">
          <div className="r-section-heading">
            <h2 id="planning-input-title">模型规划</h2>
            <Bot size={18} />
          </div>
          <div className="r-planner-status">
            <span className={status?.configured ? 'configured' : ''}>
              {!authenticated
                ? '等待模拟会话'
                : statusChecking
                  ? '读取模型状态'
                  : !status
                    ? '模型状态未核验'
                    : status.configured
                      ? '已配置 · 连接未核验'
                      : '模型未配置'}
            </span>
            <button
              className="r-icon"
              title="刷新模型状态"
              aria-label="刷新模型状态"
              disabled={!authenticated || !!busy || statusChecking}
              onClick={() => void refreshStatus()}
            >
              <RefreshCw size={16} className={statusChecking ? 'r-spinning' : ''} />
            </button>
          </div>
          <form
            onSubmit={(event) => {
              event.preventDefault();
              void execute('planning_submit');
            }}
          >
            <label className="r-planning-input-label" htmlFor="planning-message">
              请求
            </label>
            <textarea
              id="planning-message"
              rows={6}
              value={message}
              disabled={!authenticated || !!busy}
              onChange={(event) => setMessage(event.target.value)}
              aria-describedby="planning-input-size"
            />
            <div
              className={`r-planning-input-size ${byteLength > 4096 ? 'over-limit' : ''}`}
              id="planning-input-size"
            >
              {byteLength} / 4096 字节
            </div>
            <button className="r-command primary" disabled={!canSubmit || !!busy || querying}>
              {busy === 'planning_submit' ? (
                <LoaderCircle size={16} className="r-spinning" />
              ) : (
                <Send size={16} />
              )}
              {busy === 'planning_submit' ? '规划中' : '提交规划'}
            </button>
          </form>
          {uncertainCreate && busy !== 'planning_submit' && (
            <p className="r-uncertain">
              <CircleAlert size={15} />
              规划结果未明 · 暂无任务号
            </p>
          )}
        </section>
      </aside>
      <section className="r-task-area" aria-labelledby="planning-result-title">
        <div className="r-section-heading">
          <h2 id="planning-result-title">规划结果</h2>
          <span>仅余额只读计划</span>
        </div>
        <form
          className="r-query"
          onSubmit={(event) => {
            event.preventDefault();
            void queryOriginal(taskId.trim());
          }}
        >
          <label htmlFor="planning-task-id">任务号</label>
          <div className="r-query-input">
            <input
              id="planning-task-id"
              value={taskId}
              placeholder="task_id"
              autoComplete="off"
              spellCheck={false}
              disabled={!!busy || querying}
              onChange={(event) => setTaskId(event.target.value)}
            />
            <button
              className="r-icon"
              title="查询规划原任务"
              aria-label="查询规划原任务"
              disabled={!authenticated || !taskId.trim() || !!busy || querying}
            >
              <Search size={18} />
            </button>
          </div>
        </form>
        {!view ? (
          <div className="r-empty">
            <Bot size={32} />
            <h3>
              {busy === 'planning_submit'
                ? '正在接收模型规划'
                : querying
                  ? '正在查询规划原号'
                  : !authenticated
                    ? '等待模拟会话'
                    : uncertainCreate
                      ? '规划结果未明'
                      : status && !status.configured
                        ? '模型规划尚未配置'
                        : '暂无受检规划'}
            </h3>
            <p>
              {busy === 'planning_submit'
                ? '尚未执行'
                : uncertainCreate
                  ? '暂无任务号'
                  : '资金执行未启用'}
            </p>
          </div>
        ) : (
          <div className="r-task-detail">
            {fresh ? (
              <PlanningSummary view={view} />
            ) : (
              <>
                <span className="r-state UNSYNCED">
                  <WifiOff size={17} />
                  状态待核实
                </span>
                <h3 className="r-planning-heading">正在核对规划原号</h3>
              </>
            )}
            {view.kind === 'TASK' && (
              <>
                <div className="r-planning-task-tools">
                  <button
                    className="r-icon"
                    title="刷新规划原任务"
                    aria-label="刷新规划原任务"
                    disabled={!authenticated || !!busy || querying}
                    onClick={() => void queryOriginal(view.task.task_id)}
                  >
                    <RefreshCw size={17} className={querying ? 'r-spinning' : ''} />
                  </button>
                </div>
                {view.can_start && (
                  <div className="r-continue">
                    <button
                      className="r-command primary"
                      disabled={!canStart || !!busy || querying}
                      onClick={() => void execute('planning_execute')}
                    >
                      <ArrowRight size={16} />
                      {busy === 'planning_execute' ? '核验运行中' : '执行只读计划'}
                    </button>
                  </div>
                )}
                {!view.can_start && view.task.next_action.kind === 'CONTINUE' && (
                  <div className="r-continue">
                    <button
                      className="r-command primary"
                      disabled={!canContinue || !!busy || querying}
                      onClick={() => void execute('planning_continue')}
                    >
                      <ArrowRight size={16} />
                      {busy === 'planning_continue' ? '继续中' : '继续只读计划'}
                    </button>
                  </div>
                )}
              </>
            )}
            <div className="r-verification">
              <ShieldCheck size={16} />
              <span>余额只读范围</span>
              <span>资金执行未启用</span>
            </div>
          </div>
        )}
      </section>
    </div>
  );
}
