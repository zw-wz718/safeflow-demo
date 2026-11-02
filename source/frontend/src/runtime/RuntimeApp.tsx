import { useCallback, useEffect, useRef, useState, type FormEvent } from 'react';
import {
  Activity,
  ArrowLeft,
  ArrowRight,
  CheckCheck,
  CircleAlert,
  Clock3,
  FlaskConical,
  LoaderCircle,
  FilePenLine,
  Bot,
  PauseCircle,
  Play,
  RefreshCw,
  Search,
  ShieldCheck,
  WifiOff,
  Workflow,
} from 'lucide-react';
import {
  continueTask,
  createTask,
  getSession,
  getTask,
  login,
  logout,
  RuntimeApiError,
  RuntimeTransportError,
} from './client';
import type { ErrorReason, TaskResponse, TaskState } from './schema';
import { DraftWorkspace, clearDraftReferences, type DraftCommand } from './DraftWorkspace';
import { SessionPanel } from './SessionPanel';
import { BillsFeedbackWorkspace } from './BillsFeedbackWorkspace';
import { AgentWorkspace, clearAgentReferences, type AgentCommand } from './AgentWorkspace';
import {
  PlanningWorkspace,
  clearPlanningReferences,
  type PlanningCommand,
} from './PlanningWorkspace';

const lastTaskKey = 'safeflow.runtime.last-task-id.v1';
const uncertainCreateKey = 'safeflow.runtime.uncertain-create.v1';
const taskModeKey = 'safeflow.runtime.task-mode.v1';
type SessionStatus = 'checking' | 'authenticated' | 'anonymous' | 'unavailable';
type WorkspaceCommand = DraftCommand | PlanningCommand | AgentCommand;
type Command = 'login' | 'logout' | 'create' | 'continue' | WorkspaceCommand | null;
type TaskMode = 'read-only' | 'draft' | 'planning' | 'agent';
type Notice = { kind: 'error' | 'info'; text: string } | null;

const statusLabels: Record<TaskState, string> = {
  RUNNING: '运行中',
  WAITING_USER: '等待继续',
  WAITING_EXTERNAL: '等待外部结果',
  COMPLETED: '已完成',
  FAILED: '已失败',
  CANCELLED: '已取消',
};
const errorLabels: Record<ErrorReason, string> = {
  VALIDATION_ERROR: '请求未通过校验。',
  NOT_AUTHENTICATED: '模拟会话未登录或已失效。',
  FORBIDDEN: '请求被公共安全校验拒绝。',
  NOT_FOUND: '任务不存在或不属于当前会话。',
  STALE_PLAN: '任务版本已变化，正在查询原任务。',
  IDEMPOTENCY_CONFLICT: '当前请求与已记录操作冲突，正在查询原任务。',
  DEPENDENCY_UNAVAILABLE: '依赖未接入或暂不可用。',
  DRAFT_INTENT_EXPIRED: '未付款草案已过期，请查询原草案后重新办理。',
  DRAFT_INTENT_REPLACED: '未付款草案已被替代，请核对新草案并重新确认。',
  DRAFT_INTENT_ABANDONED: '未付款草案已放弃，原记录保留。',
  LLM_NOT_CONFIGURED: '模型规划服务尚未配置。',
  LLM_GATEWAY_UNAVAILABLE: '模型规划服务暂不可用。',
  LLM_PROPOSAL_REJECTED: '模型提案未通过安全接收校验。',
  RECONCILE_REQUIRED: '原操作结果未明，请只读查询原号。',
};

function readStored(key: string) {
  try {
    return sessionStorage.getItem(key);
  } catch {
    return null;
  }
}
function store(key: string, value: string | null) {
  try {
    if (value === null) sessionStorage.removeItem(key);
    else sessionStorage.setItem(key, value);
  } catch {
    // Storage is optional; the server remains the source of task state.
  }
}
function errorText(error: unknown) {
  if (error instanceof RuntimeApiError) return errorLabels[error.reason];
  if (error instanceof RuntimeTransportError && error.reason === 'TIMEOUT') {
    return '服务请求超时。';
  }
  if (error instanceof RuntimeTransportError && error.reason === 'INVALID_RESPONSE') {
    return '服务响应未通过合同校验。';
  }
  return '无法连接底座服务。';
}
function taskDescription(task: TaskResponse) {
  switch (task.task_state) {
    case 'WAITING_USER':
      return task.next_action.kind === 'CONTINUE'
        ? '等待你继续当前只读计划。'
        : '等待服务端给出下一步。';
    case 'COMPLETED':
      return '只读模拟任务已完成。';
    case 'FAILED':
      return '任务已停止，请核对原任务状态。';
    case 'CANCELLED':
      return '任务已取消。';
    case 'WAITING_EXTERNAL':
      return '等待外部结果，尚未完成。';
    default:
      return '按服务端计划运行。';
  }
}

export function RuntimeApp({
  customer = false,
  bills = false,
}: {
  customer?: boolean;
  bills?: boolean;
}) {
  const [mode, setMode] = useState<TaskMode>(() => {
    if (customer) return 'agent';
    const saved = readStored(taskModeKey);
    return saved === 'draft' || saved === 'planning' || saved === 'read-only' ? saved : 'agent';
  });
  const modeRef = useRef<TaskMode>(mode);
  const [sessionGeneration, setSessionGeneration] = useState(0);
  const [session, setSession] = useState<SessionStatus>('checking');
  const [secret, setSecret] = useState('');
  const [taskId, setTaskId] = useState(() => readStored(lastTaskKey) ?? '');
  const [task, setTask] = useState<TaskResponse | null>(null);
  const [fresh, setFresh] = useState(false);
  const [notice, setNotice] = useState<Notice>(null);
  const [busy, setBusy] = useState<Command>(null);
  const [querying, setQuerying] = useState(false);
  const [checking, setChecking] = useState(false);
  const [uncertainCreate, setUncertainCreate] = useState(
    () => readStored(uncertainCreateKey) === '1',
  );
  const [updatedAt, setUpdatedAt] = useState<string | null>(null);
  const sessionRef = useRef<SessionStatus>('checking');
  const busyRef = useRef<Command>(null);
  const currentTask = useRef<TaskResponse | null>(null);
  const selectedId = useRef(readStored(lastTaskKey) ?? '');
  const epoch = useRef(0);
  const mounted = useRef(false);
  const requests = useRef(new Set<AbortController>());
  const queryInFlight = useRef(false);
  const sessionInFlight = useRef(false);

  const runRequest = useCallback(async <T,>(action: (signal: AbortSignal) => Promise<T>) => {
    const controller = new AbortController();
    requests.current.add(controller);
    try {
      return await action(controller.signal);
    } finally {
      requests.current.delete(controller);
    }
  }, []);

  const changeSession = useCallback((next: SessionStatus) => {
    sessionRef.current = next;
    setSession(next);
  }, []);

  const clearVisibleTask = useCallback(() => {
    currentTask.current = null;
    setTask(null);
    setFresh(false);
    setUpdatedAt(null);
  }, []);

  const forgetTask = useCallback(
    (clearInput = true) => {
      selectedId.current = '';
      if (clearInput) setTaskId('');
      store(lastTaskKey, null);
      clearVisibleTask();
    },
    [clearVisibleTask],
  );

  const invalidateSession = useCallback(
    (next: SessionStatus, forget: boolean) => {
      epoch.current += 1;
      sessionInFlight.current = false;
      queryInFlight.current = false;
      setChecking(false);
      setQuerying(false);
      setSessionGeneration(epoch.current);
      requests.current.forEach((controller) => controller.abort());
      changeSession(next);
      clearVisibleTask();
      setSecret('');
      busyRef.current = null;
      setBusy(null);
      if (forget) {
        clearDraftReferences();
        clearPlanningReferences();
        clearAgentReferences();
        forgetTask();
        setUncertainCreate(false);
        store(uncertainCreateKey, null);
      }
    },
    [changeSession, clearVisibleTask, forgetTask],
  );

  const acceptTask = useCallback((received: TaskResponse, updateInput = true) => {
    if (
      currentTask.current?.task_id === received.task_id &&
      currentTask.current.revision > received.revision
    ) {
      return;
    }
    currentTask.current = received;
    selectedId.current = received.task_id;
    setTask(received);
    if (updateInput) setTaskId(received.task_id);
    setFresh(true);
    setUpdatedAt(new Date().toLocaleTimeString('zh-CN', { hour12: false }));
    store(lastTaskKey, received.task_id);
  }, []);

  const queryOriginal = useCallback(
    async (id: string, quiet = false) => {
      if (!id || sessionRef.current !== 'authenticated' || queryInFlight.current) return;
      const startedEpoch = epoch.current;
      queryInFlight.current = true;
      setQuerying(true);
      if (selectedId.current !== id) clearVisibleTask();
      selectedId.current = id;
      store(lastTaskKey, id);
      if (!quiet) setTaskId(id);
      if (!quiet) {
        setFresh(false);
        setNotice(null);
      }
      try {
        const received = await runRequest((signal) => getTask(id, signal));
        if (!mounted.current || startedEpoch !== epoch.current || selectedId.current !== id) return;
        acceptTask(received, !quiet);
      } catch (error) {
        if (!mounted.current || startedEpoch !== epoch.current) return;
        if (error instanceof RuntimeApiError && error.reason === 'NOT_AUTHENTICATED') {
          invalidateSession('anonymous', false);
        } else if (error instanceof RuntimeApiError && error.reason === 'NOT_FOUND') {
          forgetTask(!quiet);
        }
        setFresh(false);
        setNotice({ kind: 'error', text: errorText(error) });
      } finally {
        if (mounted.current && startedEpoch === epoch.current) {
          queryInFlight.current = false;
          setQuerying(false);
        }
      }
    },
    [acceptTask, clearVisibleTask, forgetTask, invalidateSession, runRequest],
  );

  const checkSession = useCallback(async () => {
    if (sessionInFlight.current || busyRef.current) return;
    const startedEpoch = epoch.current;
    sessionInFlight.current = true;
    setChecking(true);
    try {
      const received = await runRequest(getSession);
      if (!mounted.current || startedEpoch !== epoch.current) return;
      if (!received.authenticated) {
        invalidateSession('anonymous', false);
      } else {
        const previouslyAuthenticated = sessionRef.current === 'authenticated';
        changeSession('authenticated');
        if (!previouslyAuthenticated && selectedId.current && modeRef.current === 'read-only') {
          await queryOriginal(selectedId.current, true);
        }
      }
    } catch (error) {
      if (!mounted.current || startedEpoch !== epoch.current) return;
      const unauthenticated =
        error instanceof RuntimeApiError && error.reason === 'NOT_AUTHENTICATED';
      invalidateSession(unauthenticated ? 'anonymous' : 'unavailable', false);
      setNotice({ kind: 'error', text: errorText(error) });
    } finally {
      if (mounted.current && startedEpoch === epoch.current) {
        sessionInFlight.current = false;
        setChecking(false);
      }
    }
  }, [changeSession, invalidateSession, queryOriginal, runRequest]);

  useEffect(() => {
    mounted.current = true;
    void checkSession();
    const taskTimer = setInterval(() => {
      if (
        !busyRef.current &&
        selectedId.current &&
        sessionRef.current === 'authenticated' &&
        modeRef.current === 'read-only'
      ) {
        void queryOriginal(selectedId.current, true);
      }
    }, 4000);
    const sessionTimer = setInterval(() => void checkSession(), 15000);
    return () => {
      mounted.current = false;
      epoch.current += 1;
      sessionInFlight.current = false;
      queryInFlight.current = false;
      clearInterval(taskTimer);
      clearInterval(sessionTimer);
      requests.current.forEach((controller) => controller.abort());
    };
  }, [checkSession, queryOriginal]);

  async function signIn(event: FormEvent) {
    event.preventDefault();
    if (!secret || busyRef.current) return;
    epoch.current += 1;
    setSessionGeneration(epoch.current);
    requests.current.forEach((controller) => controller.abort());
    const startedEpoch = epoch.current;
    const enteredSecret = secret;
    setSecret('');
    clearVisibleTask();
    busyRef.current = 'login';
    setBusy('login');
    setNotice(null);
    try {
      const received = await runRequest((signal) => login(enteredSecret, signal));
      if (!mounted.current || startedEpoch !== epoch.current) return;
      if (!received.authenticated) {
        changeSession('anonymous');
        setNotice({ kind: 'error', text: errorLabels.NOT_AUTHENTICATED });
        return;
      }
      changeSession('authenticated');
      if (selectedId.current && modeRef.current === 'read-only')
        await queryOriginal(selectedId.current, true);
    } catch (error) {
      if (!mounted.current || startedEpoch !== epoch.current) return;
      changeSession(error instanceof RuntimeApiError ? 'anonymous' : 'unavailable');
      setNotice({ kind: 'error', text: errorText(error) });
    } finally {
      if (mounted.current && startedEpoch === epoch.current) {
        busyRef.current = null;
        setBusy(null);
      }
    }
  }

  async function signOut() {
    if (busyRef.current) return;
    invalidateSession('anonymous', true);
    const startedEpoch = epoch.current;
    busyRef.current = 'logout';
    setBusy('logout');
    setNotice(null);
    try {
      await runRequest(logout);
    } catch (error) {
      if (!mounted.current || startedEpoch !== epoch.current) return;
      changeSession('unavailable');
      setNotice({ kind: 'error', text: `退出状态未核实。${errorText(error)}` });
    } finally {
      if (mounted.current && startedEpoch === epoch.current) {
        busyRef.current = null;
        setBusy(null);
      }
    }
  }

  async function executeCommand(command: 'create' | 'continue') {
    if (busyRef.current || queryInFlight.current || sessionRef.current !== 'authenticated') return;
    const original = currentTask.current;
    if (command === 'create' && uncertainCreate) return;
    if (command === 'continue' && (!original || !fresh || original.next_action.kind !== 'CONTINUE'))
      return;
    const startedEpoch = epoch.current;
    busyRef.current = command;
    setBusy(command);
    setNotice(null);
    setFresh(false);
    if (command === 'create') {
      clearVisibleTask();
      selectedId.current = '';
      setTaskId('');
      store(lastTaskKey, null);
      store(uncertainCreateKey, '1');
      setUncertainCreate(true);
    }
    try {
      const received = await runRequest((signal) =>
        command === 'create' ? createTask(signal) : continueTask(original!, signal),
      );
      if (!mounted.current || startedEpoch !== epoch.current) return;
      acceptTask(received);
      if (command === 'create') {
        store(uncertainCreateKey, null);
        setUncertainCreate(false);
      }
      if (received.replayed) {
        setNotice({ kind: 'info', text: '本次继续已处理，已显示服务端最新状态。' });
      }
    } catch (error) {
      if (!mounted.current || startedEpoch !== epoch.current) return;
      if (error instanceof RuntimeApiError && error.reason === 'NOT_AUTHENTICATED') {
        if (error.taskId) {
          selectedId.current = error.taskId;
          setTaskId(error.taskId);
          store(lastTaskKey, error.taskId);
        }
        if (command === 'create' && error.validated) {
          store(uncertainCreateKey, null);
          setUncertainCreate(false);
        }
        invalidateSession('anonymous', false);
        setNotice({ kind: 'error', text: errorText(error) });
        return;
      }
      if (error instanceof RuntimeApiError) {
        if (command === 'create') {
          store(uncertainCreateKey, null);
          setUncertainCreate(false);
        }
        const originalId = error.taskId ?? original?.task_id;
        setNotice({ kind: 'error', text: errorText(error) });
        if (originalId && (command === 'continue' || error.nextAction === 'QUERY_ORIGINAL_TASK')) {
          await queryOriginal(originalId, true);
        }
      } else if (command === 'continue' && original) {
        setNotice({ kind: 'error', text: `继续结果未明。${errorText(error)}正在查询原任务。` });
        await queryOriginal(original.task_id, true);
      } else {
        setNotice({ kind: 'error', text: `新建结果未明，暂无任务号。${errorText(error)}` });
      }
    } finally {
      if (mounted.current && startedEpoch === epoch.current) {
        busyRef.current = null;
        setBusy(null);
      }
    }
  }

  const authenticated = session === 'authenticated';
  const canContinue =
    authenticated &&
    fresh &&
    task?.task_state === 'WAITING_USER' &&
    task.next_action.kind === 'CONTINUE';
  const serviceConnected = session === 'authenticated' || session === 'anonymous';
  const getGeneration = useCallback(() => epoch.current, []);
  const isBusy = useCallback(() => !!busyRef.current, []);
  const beginWorkspaceCommand = useCallback((command: WorkspaceCommand) => {
    if (busyRef.current || queryInFlight.current || sessionRef.current !== 'authenticated')
      return false;
    busyRef.current = command;
    setBusy(command);
    return true;
  }, []);
  const finishWorkspaceCommand = useCallback((command: WorkspaceCommand) => {
    if (busyRef.current === command) {
      busyRef.current = null;
      setBusy(null);
    }
  }, []);
  const workspaceAuthenticationLost = useCallback(
    () => invalidateSession('anonymous', false),
    [invalidateSession],
  );
  const sessionControls = (
    <SessionPanel
      authenticated={authenticated}
      checking={session === 'checking'}
      secret={secret}
      busy={busy}
      onSecretChange={setSecret}
      onSignIn={signIn}
      onSignOut={signOut}
    />
  );

  function selectMode(next: TaskMode) {
    if (busyRef.current || next === modeRef.current) return;
    modeRef.current = next;
    store(taskModeKey, next);
    setMode(next);
    setNotice(null);
    if (next === 'read-only') {
      clearVisibleTask();
      if (selectedId.current && sessionRef.current === 'authenticated')
        void queryOriginal(selectedId.current, true);
    }
  }

  if (bills)
    return (
      <div className="r-app bank-payment">
        {notice && (
          <p role="status" className="r-notice">
            {notice.text}
          </p>
        )}
        <BillsFeedbackWorkspace
          authenticated={authenticated}
          generation={sessionGeneration}
          busy={busy}
          sessionControls={sessionControls}
          getGeneration={getGeneration}
          onAuthenticationLost={workspaceAuthenticationLost}
        />
      </div>
    );
  if (customer)
    return (
      <div className="r-app bank-payment" aria-label="银行模拟付款">
        <p className="bank-payment-intro">
          连接演示服务办理草案与付款。此处仅显示服务端结果，使用独立模拟账户，未连接真实银行。
        </p>
        {notice && (
          <p className="r-notice" role="status">
            {notice.text}
          </p>
        )}
        <AgentWorkspace
          customer
          authenticated={authenticated}
          generation={sessionGeneration}
          busy={busy}
          sessionControls={sessionControls}
          runRequest={runRequest}
          getGeneration={getGeneration}
          isBusy={isBusy}
          beginCommand={beginWorkspaceCommand}
          finishCommand={finishWorkspaceCommand}
          onAuthenticationLost={workspaceAuthenticationLost}
        />
      </div>
    );
  return (
    <div className="r-app">
      <header className="r-topbar">
        <a href="/" className="r-brand" aria-label="SafeFlow 银行工作台">
          <ShieldCheck size={26} />
          <strong>SafeFlow</strong>
          <span>底座任务</span>
        </a>
        <div className="r-connection-tools">
          <span className={`r-connection ${serviceConnected ? 'connected' : ''}`} role="status">
            {serviceConnected ? (
              <Activity size={15} />
            ) : session === 'checking' ? (
              <LoaderCircle size={15} />
            ) : (
              <WifiOff size={15} />
            )}
            {serviceConnected ? '服务已连接' : session === 'checking' ? '连接中' : '服务不可用'}
          </span>
          <button
            className="r-icon"
            title="刷新连接"
            aria-label="刷新连接"
            disabled={checking || !!busy}
            onClick={() => void checkSession()}
          >
            <RefreshCw size={17} className={checking ? 'r-spinning' : ''} />
          </button>
        </div>
      </header>
      <nav className="r-navigation" aria-label="工作台导航">
        <a href="/">
          <ArrowLeft size={15} />
          银行工作台
        </a>
        <a href="/runtime.html" aria-current="page">
          <Workflow size={16} />
          底座任务
        </a>
      </nav>
      <div className="r-environment">
        <FlaskConical size={16} />
        <strong>项目模拟</strong>
        <span>
          {mode === 'agent'
            ? '计划调度 · 付款须独立确认'
            : mode === 'planning'
              ? '模型规划 · 无资金执行'
              : 'Mock LLM · 无资金执行'}
        </span>
      </div>
      <main className="r-main">
        <div className="r-page-heading">
          <h1>底座任务</h1>
          <span>公共 Runtime</span>
        </div>
        <div className="r-mode-switch" role="group" aria-label="任务模式">
          <button
            aria-pressed={mode === 'agent'}
            disabled={!!busy}
            onClick={() => selectMode('agent')}
          >
            <Bot size={16} />
            Agent 助手
          </button>
          <button
            aria-pressed={mode === 'read-only'}
            disabled={!!busy}
            onClick={() => selectMode('read-only')}
          >
            <Search size={16} />
            只读任务
          </button>
          <button
            aria-pressed={mode === 'draft'}
            disabled={!!busy}
            onClick={() => selectMode('draft')}
          >
            <FilePenLine size={16} />
            草案请求
          </button>
          <button
            aria-pressed={mode === 'planning'}
            disabled={!!busy}
            onClick={() => selectMode('planning')}
          >
            <Bot size={16} />
            模型规划
          </button>
        </div>
        {notice && (
          <div
            className={`r-notice ${notice.kind}`}
            role={notice.kind === 'error' ? 'alert' : 'status'}
          >
            <CircleAlert size={18} />
            <p>{notice.text}</p>
          </div>
        )}
        {mode === 'agent' ? (
          <AgentWorkspace
            authenticated={authenticated}
            generation={sessionGeneration}
            busy={busy}
            sessionControls={sessionControls}
            runRequest={runRequest}
            getGeneration={getGeneration}
            isBusy={isBusy}
            beginCommand={beginWorkspaceCommand}
            finishCommand={finishWorkspaceCommand}
            onAuthenticationLost={workspaceAuthenticationLost}
          />
        ) : mode === 'planning' ? (
          <PlanningWorkspace
            authenticated={authenticated}
            generation={sessionGeneration}
            busy={busy}
            sessionControls={sessionControls}
            runRequest={runRequest}
            getGeneration={getGeneration}
            isBusy={isBusy}
            beginCommand={beginWorkspaceCommand}
            finishCommand={finishWorkspaceCommand}
            onAuthenticationLost={workspaceAuthenticationLost}
          />
        ) : mode === 'draft' ? (
          <DraftWorkspace
            authenticated={authenticated}
            generation={sessionGeneration}
            busy={busy}
            sessionControls={sessionControls}
            runRequest={runRequest}
            getGeneration={getGeneration}
            isBusy={isBusy}
            beginCommand={beginWorkspaceCommand}
            finishCommand={finishWorkspaceCommand}
            onAuthenticationLost={workspaceAuthenticationLost}
          />
        ) : (
          <div className="r-workspace">
            <aside className="r-controls">
              {sessionControls}
              <section className="r-new-task" aria-labelledby="new-task-title">
                <div className="r-section-heading">
                  <h2 id="new-task-title">只读任务</h2>
                  <Workflow size={18} />
                </div>
                <div className="r-plan-label">
                  <span className="r-readonly">只读</span>
                  <strong>余额查询模拟计划</strong>
                </div>
                <button
                  className="r-command primary"
                  disabled={!authenticated || !!busy || querying || uncertainCreate}
                  onClick={() => void executeCommand('create')}
                >
                  <Play size={16} />
                  {busy === 'create' ? '创建中' : '创建任务'}
                </button>
                {uncertainCreate && busy !== 'create' && (
                  <p className="r-uncertain">
                    <CircleAlert size={15} />
                    新建结果未明 · 暂无任务号
                  </p>
                )}
              </section>
            </aside>
            <section className="r-task-area" aria-labelledby="task-title">
              <div className="r-section-heading">
                <h2 id="task-title">原任务</h2>
                <span>服务端状态</span>
              </div>
              <form
                className="r-query"
                onSubmit={(event) => {
                  event.preventDefault();
                  void queryOriginal(taskId.trim());
                }}
              >
                <label htmlFor="task-id">任务号</label>
                <div className="r-query-input">
                  <input
                    id="task-id"
                    value={taskId}
                    placeholder="task_id"
                    autoComplete="off"
                    spellCheck={false}
                    onChange={(event) => setTaskId(event.target.value)}
                    disabled={!!busy || querying}
                  />
                  <button
                    className="r-icon"
                    title="查询原任务"
                    aria-label="查询原任务"
                    disabled={!authenticated || !taskId.trim() || !!busy || querying}
                  >
                    <Search size={18} />
                  </button>
                </div>
              </form>
              {!task ? (
                <div className="r-empty">
                  <Workflow size={32} />
                  <h3>
                    {busy === 'create'
                      ? '正在创建任务'
                      : querying
                        ? '正在查询原任务'
                        : !authenticated
                          ? '等待模拟会话'
                          : uncertainCreate
                            ? '新建结果未明'
                            : '暂无已核验任务'}
                  </h3>
                  <p>
                    {!authenticated
                      ? '未登录'
                      : uncertainCreate
                        ? '暂无任务号'
                        : '服务端任务尚未读取'}
                  </p>
                </div>
              ) : (
                <div className="r-task-detail">
                  <div className="r-task-status">
                    <span className={`r-state ${fresh ? task.task_state : 'UNSYNCED'}`}>
                      {!fresh ? (
                        <WifiOff size={17} />
                      ) : task.task_state === 'COMPLETED' ? (
                        <CheckCheck size={17} />
                      ) : task.task_state === 'WAITING_USER' ? (
                        <PauseCircle size={17} />
                      ) : task.task_state === 'FAILED' ? (
                        <CircleAlert size={17} />
                      ) : (
                        <Clock3 size={17} />
                      )}
                      {fresh
                        ? statusLabels[task.task_state]
                        : busy === 'continue'
                          ? '继续结果待核实'
                          : querying
                            ? '正在同步'
                            : '状态未同步'}
                    </span>
                    <button
                      className="r-icon"
                      title="刷新原任务"
                      aria-label="刷新原任务"
                      disabled={!authenticated || !!busy || querying}
                      onClick={() => void queryOriginal(task.task_id)}
                    >
                      <RefreshCw size={17} className={querying ? 'r-spinning' : ''} />
                    </button>
                  </div>
                  <h3>{fresh ? taskDescription(task) : '正在核对原任务，当前结果未确认。'}</h3>
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
                  {fresh && task.recovery.kind === 'AWAIT_USER' && (
                    <div className="r-waiting-node">
                      <span>等待节点</span>
                      <code>{task.recovery.node_id}</code>
                    </div>
                  )}
                  {fresh && task.next_action.kind === 'REVIEW_ORIGINAL_TASK' && (
                    <p className="r-recovery">
                      <RefreshCw size={16} />
                      等待核对原任务结果
                    </p>
                  )}
                  {task.next_action.kind === 'CONTINUE' && (
                    <div className="r-continue">
                      <button
                        className="r-command primary"
                        disabled={!canContinue || !!busy || querying}
                        onClick={() => void executeCommand('continue')}
                      >
                        <ArrowRight size={16} />
                        {busy === 'continue' ? '继续中' : '继续只读任务'}
                      </button>
                    </div>
                  )}
                  <div className="r-verification">
                    <ShieldCheck size={16} />
                    <span>当前安全裁决未提供</span>
                    <span>资金执行未启用</span>
                  </div>
                  {updatedAt && (
                    <p className="r-updated">
                      {fresh ? '最近核验' : '上次核验'} {updatedAt}
                    </p>
                  )}
                </div>
              )}
            </section>
          </div>
        )}
      </main>
      <footer className="r-footer">
        <span>SafeFlow · 公共底座</span>
        <span>项目模拟环境</span>
      </footer>
    </div>
  );
}
