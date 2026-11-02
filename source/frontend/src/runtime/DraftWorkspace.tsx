import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import {
  ArrowRight,
  CircleAlert,
  CircleCheck,
  FileCheck2,
  FilePenLine,
  Play,
  RefreshCw,
  Search,
  ShieldCheck,
  WifiOff,
  X,
} from 'lucide-react';
import { RuntimeApiError } from './client';
import {
  createDraftRequest,
  getDraftRequest,
  prepareDraftRequest,
  recordDraftConfirmation,
} from './draft-client';
import type { DraftConfirmationView } from './draft-schema';
import { DraftExecutionPanel } from './DraftExecutionPanel';
import { hasDraftExecutionAttemptForTask } from './draft-execution-journal';
import type { DraftExecutionReference } from './draft-execution-schema';

const taskKey = 'safeflow.runtime.draft.last-task-id.v1';
const uncertainKey = 'safeflow.runtime.draft.uncertain-create.v1';
export type DraftCommand =
  'draft_create' | 'draft_prepare' | 'draft_confirm' | 'draft_reject' | 'draft_execute';
type Notice = { kind: 'error' | 'info'; text: string } | null;
type Props = {
  authenticated: boolean;
  generation: number;
  busy: string | null;
  sessionControls: ReactNode;
  runRequest: <T>(action: (signal: AbortSignal) => Promise<T>) => Promise<T>;
  getGeneration: () => number;
  isBusy: () => boolean;
  beginCommand: (command: DraftCommand) => boolean;
  finishCommand: (command: DraftCommand) => void;
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
    /* Task authority does not depend on browser storage. */
  }
}
export function clearDraftReferences() {
  persist(taskKey, null);
  persist(uncertainKey, null);
}
function safeError(error: unknown) {
  if (error instanceof RuntimeApiError) {
    switch (error.reason) {
      case 'NOT_AUTHENTICATED':
        return '模拟会话未登录或已失效。';
      case 'NOT_FOUND':
        return '草案请求不存在或不属于当前会话。';
      case 'STALE_PLAN':
        return '确认来源已过期或变化。';
      case 'IDEMPOTENCY_CONFLICT':
        return '本次确认与已有记录冲突。';
      case 'FORBIDDEN':
        return '请求被公共安全校验拒绝。';
      case 'DEPENDENCY_UNAVAILABLE':
        return '草案请求依赖暂不可用。';
      default:
        return '请求未通过校验。';
    }
  }
  return '草案请求结果暂未核实。';
}
function timeLabel(value: string) {
  return new Date(value).toLocaleString('zh-CN', { hour12: false });
}

export function DraftWorkspace({
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
  const [taskId, setTaskId] = useState(() => stored(taskKey) ?? '');
  const [view, setView] = useState<DraftConfirmationView | null>(null);
  const [fresh, setFresh] = useState(false);
  const [validatedAt, setValidatedAt] = useState<number | null>(null);
  const [querying, setQuerying] = useState(false);
  const [notice, setNotice] = useState<Notice>(null);
  const [uncertainCreate, setUncertainCreate] = useState(() => stored(uncertainKey) === '1');
  const selectedId = useRef(stored(taskKey) ?? '');
  const currentView = useRef<DraftConfirmationView | null>(null);
  const authenticatedRef = useRef(authenticated);
  authenticatedRef.current = authenticated;
  const mounted = useRef(false);
  const queryInFlight = useRef(false);

  const clearView = useCallback(() => {
    currentView.current = null;
    setView(null);
    setFresh(false);
    setValidatedAt(null);
  }, []);
  const handleExecutionAttempt = useCallback(
    (reference: DraftExecutionReference) => {
      if (currentView.current?.task_id === reference.task_id) clearView();
    },
    [clearView],
  );
  const rememberId = useCallback((id: string, updateInput: boolean) => {
    selectedId.current = id;
    persist(taskKey, id);
    if (updateInput) setTaskId(id);
  }, []);
  const acceptView = useCallback(
    (received: DraftConfirmationView, updateInput: boolean) => {
      if (
        currentView.current?.task_id === received.task_id &&
        currentView.current.task_revision > received.task_revision
      )
        return;
      currentView.current = received;
      setView(received);
      setFresh(true);
      setValidatedAt(Date.now());
      rememberId(received.task_id, updateInput);
    },
    [rememberId],
  );

  const queryOriginal = useCallback(
    async (id: string, quiet = false) => {
      if (!id || !authenticatedRef.current || queryInFlight.current) return null;
      if (hasDraftExecutionAttemptForTask(id)) {
        clearView();
        if (!quiet) setNotice({ kind: 'info', text: '该任务已有保存尝试，请查询原操作保存结果。' });
        return null;
      }
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
        const received = await runRequest((signal) => getDraftRequest(id, signal));
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

  useEffect(() => {
    mounted.current = true;
    const timer = setInterval(() => {
      if (
        !isBusy() &&
        authenticatedRef.current &&
        selectedId.current &&
        !hasDraftExecutionAttemptForTask(selectedId.current)
      )
        void queryOriginal(selectedId.current, true);
    }, 4000);
    return () => {
      mounted.current = false;
      clearInterval(timer);
    };
  }, [isBusy, queryOriginal]);

  useEffect(() => {
    clearView();
    selectedId.current = stored(taskKey) ?? '';
    if (!selectedId.current) setTaskId('');
    setUncertainCreate(stored(uncertainKey) === '1');
    if (authenticated && selectedId.current) void queryOriginal(selectedId.current, true);
  }, [authenticated, generation, clearView, queryOriginal]);

  async function execute(command: Exclude<DraftCommand, 'draft_execute'>) {
    const original = currentView.current;
    if (queryInFlight.current || !authenticated || (command === 'draft_create' && uncertainCreate))
      return;
    if (command !== 'draft_create' && (!original || !fresh)) return;
    if (command === 'draft_prepare' && original?.next_action.kind !== 'PREPARE') return;
    if (
      (command === 'draft_confirm' || command === 'draft_reject') &&
      original?.next_action.kind !== 'CONFIRM'
    )
      return;
    if (!beginCommand(command)) return;
    const startedGeneration = getGeneration();
    setNotice(null);
    setFresh(false);
    if (command === 'draft_create') {
      clearView();
      selectedId.current = '';
      setTaskId('');
      persist(taskKey, null);
      persist(uncertainKey, '1');
      setUncertainCreate(true);
    }
    try {
      const received = await runRequest((signal) => {
        if (command === 'draft_create') return createDraftRequest(signal);
        if (command === 'draft_prepare') return prepareDraftRequest(original!.task_id, signal);
        return recordDraftConfirmation(
          original!,
          command === 'draft_confirm' ? 'CONFIRM' : 'REJECT',
          signal,
        );
      });
      if (!mounted.current || startedGeneration !== getGeneration()) return;
      acceptView(received, true);
      if (command === 'draft_create') {
        persist(uncertainKey, null);
        setUncertainCreate(false);
      }
    } catch (error) {
      if (!mounted.current || startedGeneration !== getGeneration()) return;
      clearView();
      const knownId =
        error instanceof RuntimeApiError ? (error.taskId ?? original?.task_id) : original?.task_id;
      if (knownId) rememberId(knownId, true);
      if (command === 'draft_create' && error instanceof RuntimeApiError && error.validated) {
        persist(uncertainKey, null);
        setUncertainCreate(false);
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
            : '新建草案请求结果未明，暂无任务号。',
      });
      if (knownId) {
        const recovered = await queryOriginal(knownId, true);
        if (recovered && !(error instanceof RuntimeApiError))
          setNotice({ kind: 'info', text: '原号已核验。' });
      }
    } finally {
      finishCommand(command);
    }
  }

  const confirmationStatus = fresh ? view?.confirmation?.status : undefined;
  const canConfirm = authenticated && fresh && view?.next_action.kind === 'CONFIRM';
  const canPrepare = authenticated && fresh && view?.next_action.kind === 'PREPARE';
  const attemptedTask = !!selectedId.current && hasDraftExecutionAttemptForTask(selectedId.current);

  return (
    <div className="r-workspace">
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
        <section className="r-new-task" aria-labelledby="draft-new-title">
          <div className="r-section-heading">
            <h2 id="draft-new-title">草案请求</h2>
            <FilePenLine size={18} />
          </div>
          <div className="r-plan-label">
            <span className="r-readonly">模拟</span>
            <strong>保存草案请求</strong>
          </div>
          <button
            className="r-command primary"
            disabled={!authenticated || !!busy || querying || uncertainCreate}
            onClick={() => void execute('draft_create')}
          >
            <Play size={16} />
            {busy === 'draft_create' ? '创建中' : '新建草案请求'}
          </button>
          {uncertainCreate && busy !== 'draft_create' && (
            <p className="r-uncertain">
              <CircleAlert size={15} />
              新建结果未明 · 暂无任务号
            </p>
          )}
        </section>
      </aside>
      <section className="r-task-area" aria-labelledby="draft-task-title">
        <div className="r-section-heading">
          <h2 id="draft-task-title">原草案请求</h2>
          <span>服务端状态</span>
        </div>
        <form
          className="r-query"
          onSubmit={(event) => {
            event.preventDefault();
            void queryOriginal(taskId.trim());
          }}
        >
          <label htmlFor="draft-task-id">任务号</label>
          <div className="r-query-input">
            <input
              id="draft-task-id"
              value={taskId}
              placeholder="task_id"
              autoComplete="off"
              spellCheck={false}
              disabled={!!busy || querying}
              onChange={(event) => setTaskId(event.target.value)}
            />
            <button
              className="r-icon"
              title="查询原草案请求"
              aria-label="查询原草案请求"
              disabled={!authenticated || !taskId.trim() || !!busy || querying}
            >
              <Search size={18} />
            </button>
          </div>
        </form>
        {!view ? (
          <div className="r-empty">
            <FilePenLine size={32} />
            <h3>
              {busy === 'draft_create'
                ? '正在创建草案请求'
                : querying
                  ? '正在查询原号'
                  : !authenticated
                    ? '等待模拟会话'
                    : attemptedTask
                      ? '本页已记录保存尝试'
                      : uncertainCreate
                        ? '新建结果未明'
                        : '暂无已核验草案请求'}
            </h3>
            <p>
              {!authenticated
                ? '未登录'
                : attemptedTask
                  ? '确认状态不代表保存结果'
                  : uncertainCreate
                    ? '暂无任务号'
                    : '服务端请求尚未读取'}
            </p>
          </div>
        ) : (
          <div className="r-task-detail">
            <div className="r-task-status">
              <span
                className={`r-state ${fresh ? (confirmationStatus === 'CONFIRMED' ? 'CONFIRMED' : confirmationStatus === 'REVOKED' ? 'CANCELLED' : 'PREPARED') : 'UNSYNCED'}`}
              >
                {!fresh ? (
                  <WifiOff size={17} />
                ) : confirmationStatus === 'CONFIRMED' ? (
                  <CircleCheck size={17} />
                ) : (
                  <FileCheck2 size={17} />
                )}
                {!fresh
                  ? '状态待核实'
                  : confirmationStatus === 'CONFIRMED'
                    ? '草案请求已确认'
                    : confirmationStatus === 'REVOKED'
                      ? '本次确认已撤销'
                      : view.next_action.kind === 'PREPARE'
                        ? '等待重新准备'
                        : '等待明确确认'}
              </span>
              <button
                className="r-icon"
                title="刷新原草案请求"
                aria-label="刷新原草案请求"
                disabled={!authenticated || !!busy || querying}
                onClick={() => void queryOriginal(view.task_id)}
              >
                <RefreshCw size={17} className={querying ? 'r-spinning' : ''} />
              </button>
            </div>
            <h3>
              {!fresh
                ? '正在核对原号，当前确认结果未核实。'
                : confirmationStatus === 'CONFIRMED'
                  ? '草案请求已确认，尚未保存草案'
                  : confirmationStatus === 'REVOKED'
                    ? '本次确认已撤销，业务任务未取消'
                    : '仅确认保存模拟草案的请求'}
            </h3>
            <code className="r-task-id">{view.task_id}</code>
            <dl className="r-draft-parameters" aria-label="草案请求参数">
              <div>
                <dt>付款账户</dt>
                <dd>
                  <code>{view.parameters.payer_account_id}</code>
                </dd>
              </div>
              <div>
                <dt>收款人</dt>
                <dd>
                  <code>{view.parameters.payee_id}</code>
                </dd>
              </div>
              <div>
                <dt>金额</dt>
                <dd>
                  {view.parameters.amount} <span>{view.parameters.currency}</span>
                </dd>
              </div>
              <div>
                <dt>备注</dt>
                <dd>{view.parameters.remark || '空'}</dd>
              </div>
            </dl>
            <dl className="r-draft-versions">
              <div>
                <dt>任务状态</dt>
                <dd>{view.task_state}</dd>
              </div>
              <div>
                <dt>操作状态</dt>
                <dd>{view.operation_state ?? '尚未准备'}</dd>
              </div>
              <div>
                <dt>任务修订</dt>
                <dd>{view.task_revision}</dd>
              </div>
              <div>
                <dt>操作修订</dt>
                <dd>{view.operation_revision ?? '-'}</dd>
              </div>
              <div>
                <dt>计划版本</dt>
                <dd>{view.plan_version}</dd>
              </div>
              <div>
                <dt>事实版本</dt>
                <dd>{view.facts_version}</dd>
              </div>
            </dl>
            {fresh && view.confirmation && (
              <div className="r-draft-time">
                <span>
                  {confirmationStatus === 'CONFIRMED'
                    ? '确认时间'
                    : confirmationStatus === 'REVOKED'
                      ? '撤销时间'
                      : '确认期限'}
                </span>
                <time>
                  {timeLabel(
                    confirmationStatus === 'REVOKED'
                      ? view.confirmation.revoked_at!
                      : (view.confirmation.confirmed_at ?? view.confirmation.expires_at),
                  )}
                </time>
              </div>
            )}
            {view.next_action.kind === 'PREPARE' && (
              <div className="r-continue">
                <button
                  className="r-command primary"
                  disabled={!canPrepare || !!busy || querying}
                  onClick={() => void execute('draft_prepare')}
                >
                  <RefreshCw size={16} />
                  {busy === 'draft_prepare' ? '准备中' : '重新准备并展示'}
                </button>
              </div>
            )}
            {view.next_action.kind === 'CONFIRM' && (
              <div className="r-draft-actions">
                <button
                  className="r-command primary"
                  disabled={!canConfirm || !!busy || querying}
                  onClick={() => void execute('draft_confirm')}
                >
                  <ArrowRight size={16} />
                  {busy === 'draft_confirm' ? '核验中' : '确认草案请求'}
                </button>
                <button
                  className="r-command secondary"
                  disabled={!canConfirm || !!busy || querying}
                  onClick={() => void execute('draft_reject')}
                >
                  <X size={16} />
                  {busy === 'draft_reject' ? '撤销中' : '撤销本次确认'}
                </button>
              </div>
            )}
            <div className="r-verification">
              <ShieldCheck size={16} />
              <span>仅草案请求确认</span>
              <span>资金执行未启用</span>
            </div>
          </div>
        )}
        <DraftExecutionPanel
          authenticated={authenticated}
          generation={generation}
          busy={busy}
          confirmationQuerying={querying}
          confirmedView={fresh && view?.confirmation?.status === 'CONFIRMED' ? view : null}
          confirmationValidatedAt={fresh ? validatedAt : null}
          runRequest={runRequest}
          getGeneration={getGeneration}
          isBusy={isBusy}
          beginCommand={beginCommand}
          finishCommand={finishCommand}
          onAuthenticationLost={onAuthenticationLost}
          onAttempt={handleExecutionAttempt}
        />
      </section>
    </div>
  );
}
