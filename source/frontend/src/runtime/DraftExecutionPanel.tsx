import { useCallback, useEffect, useRef, useState } from 'react';
import {
  CircleAlert,
  CircleCheck,
  FileCheck2,
  RefreshCw,
  Save,
  Search,
  ShieldCheck,
} from 'lucide-react';
import { RuntimeApiError } from './client';
import type { DraftCommand } from './DraftWorkspace';
import type { DraftConfirmationView } from './draft-schema';
import { executeConfirmedDraft, getDraftExecution } from './draft-execution-client';
import {
  draftExecutionReferenceKey,
  hasDraftExecutionAttempt,
  readDraftExecutionJournal,
  recordDraftExecutionAttempt,
} from './draft-execution-journal';
import type { DraftExecutionReference, DraftExecutionView } from './draft-execution-schema';

type Props = {
  authenticated: boolean;
  generation: number;
  busy: string | null;
  confirmationQuerying: boolean;
  confirmedView: DraftConfirmationView | null;
  confirmationValidatedAt: number | null;
  runRequest: <T>(action: (signal: AbortSignal) => Promise<T>) => Promise<T>;
  getGeneration: () => number;
  isBusy: () => boolean;
  beginCommand: (command: DraftCommand) => boolean;
  finishCommand: (command: DraftCommand) => void;
  onAuthenticationLost: () => void;
  onAttempt: (reference: DraftExecutionReference) => void;
};
type Notice = { kind: 'error' | 'info'; text: string; generation: number };
type QueryEvidence = { key: string; confirmation: string; generation: number };

function confirmationIdentity(view: DraftConfirmationView | null) {
  if (!view || view.confirmation?.status !== 'CONFIRMED' || !view.operation_id) return '';
  return JSON.stringify([
    view.task_id,
    view.operation_id,
    view.confirmation.confirmation_ref,
    view.task_revision,
    view.operation_revision,
    view.plan_version,
    view.facts_version,
  ]);
}

function safeExecutionError(error: unknown) {
  if (error instanceof RuntimeApiError) {
    switch (error.reason) {
      case 'NOT_AUTHENTICATED':
        return '模拟会话未登录或已失效。';
      case 'NOT_FOUND':
        return '原操作不存在、未有领取记录或不属于当前会话。';
      case 'STALE_PLAN':
        return '保存来源已过期或变化，请查询原操作。';
      case 'IDEMPOTENCY_CONFLICT':
        return '原操作已有记录冲突，请查询原操作。';
      case 'FORBIDDEN':
        return '保存请求被公共安全校验拒绝，请查询原操作。';
      case 'DEPENDENCY_UNAVAILABLE':
        return '保存依赖暂不可用，请查询原操作。';
      default:
        return '保存请求未通过校验，请查询原操作。';
    }
  }
  return '保存结果未明，仅可查询原任务与操作。';
}

export function DraftExecutionResult({ view }: { view: DraftExecutionView }) {
  const completed = view.execution_status === 'COMPLETED';
  return (
    <div className="r-execution-result" data-execution-status={view.execution_status}>
      <span className={`r-state ${completed ? 'COMPLETED' : 'WAITING_EXTERNAL'}`}>
        {completed ? <CircleCheck size={17} /> : <CircleAlert size={17} />}
        {completed ? '模拟草案保存已记录' : '原操作保存结果待核实'}
      </span>
      <h3>{completed ? '已取得模拟草案历史回执' : '原操作已领取，结果仍待核实'}</h3>
      <dl className="r-draft-versions" aria-label="原操作执行状态">
        <div>
          <dt>任务状态</dt>
          <dd>{view.task_state}</dd>
        </div>
        <div>
          <dt>操作状态</dt>
          <dd>{view.operation_state}</dd>
        </div>
        <div>
          <dt>任务修订</dt>
          <dd>{view.task_revision}</dd>
        </div>
        <div>
          <dt>操作修订</dt>
          <dd>{view.operation_revision}</dd>
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
      {view.receipt && (
        <dl className="r-execution-receipt" aria-label="模拟草案历史回执">
          <div>
            <dt>草案号</dt>
            <dd>
              <code>{view.receipt.draft_id}</code>
            </dd>
          </div>
          <div>
            <dt>业务操作号</dt>
            <dd>
              <code>{view.receipt.business_operation_id}</code>
            </dd>
          </div>
          <div>
            <dt>草案版本</dt>
            <dd>{view.receipt.draft_version}</dd>
          </div>
          <div>
            <dt>工具结果引用</dt>
            <dd>
              <code>{view.receipt.tool_result_ref}</code>
            </dd>
          </div>
          <div>
            <dt>历史保存时间</dt>
            <dd>
              <time dateTime={view.receipt.executed_at}>
                {new Date(view.receipt.executed_at).toLocaleString('zh-CN', { hour12: false })}
              </time>
            </dd>
          </div>
        </dl>
      )}
      <div className="r-verification">
        <ShieldCheck size={16} />
        <span>历史执行回执</span>
        <span>当前草案未核验</span>
        <span>资金执行未启用</span>
      </div>
      <p className="r-execution-boundary">
        {completed
          ? '历史保存记录不代表重启后草案仍存在。'
          : '保存可能已发生；仅可继续查询原任务与操作。'}
      </p>
    </div>
  );
}

export function DraftExecutionPanel({
  authenticated,
  generation,
  busy,
  confirmationQuerying,
  confirmedView,
  confirmationValidatedAt,
  runRequest,
  getGeneration,
  isBusy,
  beginCommand,
  finishCommand,
  onAuthenticationLost,
  onAttempt,
}: Props) {
  const [journal, setJournal] = useState(readDraftExecutionJournal);
  const [reference, setReference] = useState<DraftExecutionReference>(
    () => journal.references.at(-1) ?? { task_id: '', operation_id: '' },
  );
  const [result, setResult] = useState<{ view: DraftExecutionView; generation: number } | null>(
    null,
  );
  const [noClaim, setNoClaim] = useState<QueryEvidence | null>(null);
  const [notice, setNotice] = useState<Notice | null>(null);
  const [querying, setQuerying] = useState(false);
  const selected = useRef(reference);
  const currentConfirmation = useRef(confirmedView);
  const currentValidatedAt = useRef(confirmationValidatedAt);
  const authenticatedRef = useRef(authenticated);
  const mounted = useRef(false);
  const querySequence = useRef(0);
  const activeQuery = useRef<string | null>(null);
  currentConfirmation.current = confirmedView;
  currentValidatedAt.current = confirmationValidatedAt;
  authenticatedRef.current = authenticated;
  const candidateIdentity = confirmationIdentity(confirmedView);

  const selectReference = useCallback((next: DraftExecutionReference) => {
    if (draftExecutionReferenceKey(selected.current) !== draftExecutionReferenceKey(next)) {
      setResult(null);
      setNoClaim(null);
      setNotice(null);
    }
    selected.current = next;
    setReference(next);
  }, []);

  const queryOriginal = useCallback(
    async (original: DraftExecutionReference) => {
      if (!original.task_id || !original.operation_id || !authenticatedRef.current || isBusy())
        return;
      const key = draftExecutionReferenceKey(original);
      if (activeQuery.current === key) return;
      selectReference(original);
      const startedGeneration = getGeneration();
      const confirmation = confirmationIdentity(currentConfirmation.current);
      const sequence = ++querySequence.current;
      activeQuery.current = key;
      setQuerying(true);
      setNoClaim(null);
      setNotice(null);
      const isCurrent = () =>
        mounted.current &&
        authenticatedRef.current &&
        startedGeneration === getGeneration() &&
        sequence === querySequence.current &&
        draftExecutionReferenceKey(selected.current) === key;
      try {
        const received = await runRequest((signal) => getDraftExecution(original, signal));
        if (!isCurrent()) return;
        recordDraftExecutionAttempt(original);
        setJournal(readDraftExecutionJournal());
        setResult({ view: received, generation: startedGeneration });
        onAttempt(original);
      } catch (error) {
        if (!isCurrent()) return;
        setResult(null);
        if (error instanceof RuntimeApiError && error.reason === 'NOT_AUTHENTICATED') {
          onAuthenticationLost();
          return;
        }
        const attempted = hasDraftExecutionAttempt(original);
        const current = currentConfirmation.current;
        if (
          error instanceof RuntimeApiError &&
          error.validated &&
          error.reason === 'NOT_FOUND' &&
          !attempted &&
          confirmation &&
          confirmation === confirmationIdentity(current) &&
          current?.task_id === original.task_id &&
          current.operation_id === original.operation_id
        ) {
          setNoClaim({ key, confirmation, generation: startedGeneration });
        } else {
          setNotice({
            kind: 'error',
            text:
              attempted && error instanceof RuntimeApiError && error.reason === 'NOT_FOUND'
                ? '原操作历史回执尚未取得，仅可继续查询原号。'
                : safeExecutionError(error),
            generation: startedGeneration,
          });
        }
      } finally {
        if (sequence === querySequence.current) {
          activeQuery.current = null;
          if (mounted.current) setQuerying(false);
        }
      }
    },
    [getGeneration, isBusy, onAttempt, onAuthenticationLost, runRequest, selectReference],
  );

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      querySequence.current++;
    };
  }, []);

  useEffect(() => {
    querySequence.current++;
    activeQuery.current = null;
    setQuerying(false);
    setResult(null);
    setNoClaim(null);
    setNotice(null);
    setJournal(readDraftExecutionJournal());
    if (authenticated) void queryOriginal(selected.current);
  }, [authenticated, generation, queryOriginal]);

  useEffect(() => {
    const current = currentConfirmation.current;
    if (authenticated && candidateIdentity && current?.operation_id)
      void queryOriginal({ task_id: current.task_id, operation_id: current.operation_id });
  }, [authenticated, candidateIdentity, queryOriginal]);

  async function requestSave() {
    const original = currentConfirmation.current;
    const validatedAt = currentValidatedAt.current;
    if (
      !authenticatedRef.current ||
      confirmationQuerying ||
      activeQuery.current ||
      !original?.operation_id ||
      original.confirmation?.status !== 'CONFIRMED' ||
      !validatedAt ||
      Date.now() - validatedAt > 8000 ||
      Date.parse(original.confirmation.expires_at) <= Date.now()
    )
      return;
    const pair = { task_id: original.task_id, operation_id: original.operation_id };
    if (
      hasDraftExecutionAttempt(pair) ||
      noClaim?.generation !== getGeneration() ||
      noClaim.key !== draftExecutionReferenceKey(pair) ||
      noClaim.confirmation !== confirmationIdentity(original) ||
      draftExecutionReferenceKey(selected.current) !== draftExecutionReferenceKey(pair)
    )
      return;
    if (!beginCommand('draft_execute')) return;
    const startedGeneration = getGeneration();
    setNoClaim(null);
    setNotice(null);
    setResult(null);
    // Persist the original pair before POST so interruption can only lead to a GET.
    const recorded = recordDraftExecutionAttempt(pair);
    setJournal(readDraftExecutionJournal());
    onAttempt(pair);
    if (!recorded) {
      setNotice({
        kind: 'error',
        text: '本页无法保存尝试记录，保存请求未发送。',
        generation: startedGeneration,
      });
      finishCommand('draft_execute');
      return;
    }
    try {
      const received = await runRequest((signal) => executeConfirmedDraft(original, signal));
      if (!mounted.current || !authenticatedRef.current || startedGeneration !== getGeneration())
        return;
      setResult({ view: received, generation: startedGeneration });
    } catch (error) {
      if (!mounted.current || !authenticatedRef.current || startedGeneration !== getGeneration())
        return;
      if (error instanceof RuntimeApiError && error.reason === 'NOT_AUTHENTICATED') {
        onAuthenticationLost();
        return;
      }
      setNotice({ kind: 'error', text: safeExecutionError(error), generation: startedGeneration });
      finishCommand('draft_execute');
      await queryOriginal(pair);
    } finally {
      finishCommand('draft_execute');
    }
  }

  const selectedKey = draftExecutionReferenceKey(reference);
  const attempted = journal.references.some(
    (saved) => draftExecutionReferenceKey(saved) === selectedKey,
  );
  const visibleResult = authenticated && result?.generation === generation ? result.view : null;
  const canRequest =
    authenticated &&
    !!candidateIdentity &&
    !!confirmationValidatedAt &&
    Date.now() - confirmationValidatedAt <= 8000 &&
    Date.parse(confirmedView!.confirmation!.expires_at) > Date.now() &&
    confirmedView?.task_id === reference.task_id &&
    confirmedView.operation_id === reference.operation_id &&
    noClaim?.generation === generation &&
    noClaim.key === selectedKey &&
    noClaim.confirmation === candidateIdentity &&
    journal.available &&
    !attempted &&
    !visibleResult;

  return (
    <section className="r-execution-panel" aria-labelledby="draft-execution-title">
      <div className="r-section-heading">
        <h2 id="draft-execution-title">原操作保存结果</h2>
        <FileCheck2 size={18} />
      </div>
      {journal.references.length > 0 && (
        <div className="r-execution-history">
          <label htmlFor="draft-execution-history">已记录原操作</label>
          <select
            id="draft-execution-history"
            value={attempted ? selectedKey : ''}
            disabled={!authenticated || !!busy || querying}
            onChange={(event) => {
              const saved = journal.references.find(
                (item) => draftExecutionReferenceKey(item) === event.target.value,
              );
              if (saved) void queryOriginal(saved);
            }}
          >
            <option value="">选择原操作</option>
            {journal.references.map((saved) => (
              <option
                key={draftExecutionReferenceKey(saved)}
                value={draftExecutionReferenceKey(saved)}
              >
                {saved.task_id} / {saved.operation_id}
              </option>
            ))}
          </select>
        </div>
      )}
      <form
        className="r-execution-query"
        onSubmit={(event) => {
          event.preventDefault();
          void queryOriginal({
            task_id: reference.task_id.trim(),
            operation_id: reference.operation_id.trim(),
          });
        }}
      >
        <div>
          <label htmlFor="draft-execution-task-id">原任务号</label>
          <input
            id="draft-execution-task-id"
            value={reference.task_id}
            placeholder="task_id"
            autoComplete="off"
            spellCheck={false}
            disabled={!!busy || querying}
            onChange={(event) => selectReference({ ...reference, task_id: event.target.value })}
          />
        </div>
        <div>
          <label htmlFor="draft-execution-operation-id">原操作号</label>
          <input
            id="draft-execution-operation-id"
            value={reference.operation_id}
            placeholder="operation_id"
            autoComplete="off"
            spellCheck={false}
            disabled={!!busy || querying}
            onChange={(event) =>
              selectReference({ ...reference, operation_id: event.target.value })
            }
          />
        </div>
        <button
          className="r-icon"
          title="查询原操作保存结果"
          aria-label="查询原操作保存结果"
          disabled={
            !authenticated ||
            !reference.task_id.trim() ||
            !reference.operation_id.trim() ||
            !!busy ||
            querying
          }
        >
          {querying ? <RefreshCw size={18} className="r-spinning" /> : <Search size={18} />}
        </button>
      </form>
      {authenticated && notice?.generation === generation && (
        <div
          className={`r-notice ${notice.kind}`}
          role={notice.kind === 'error' ? 'alert' : 'status'}
        >
          <CircleAlert size={18} />
          <p>{notice.text}</p>
        </div>
      )}
      {!journal.available && authenticated && (
        <p className="r-uncertain">
          <CircleAlert size={15} />
          本页保存尝试记录不可用，暂不能请求保存。
        </p>
      )}
      {canRequest && (
        <div className="r-draft-actions">
          <button
            className="r-command primary"
            disabled={!!busy || querying || confirmationQuerying}
            onClick={() => void requestSave()}
          >
            <Save size={16} />
            请求保存模拟草案
          </button>
        </div>
      )}
      {visibleResult ? (
        <DraftExecutionResult view={visibleResult} />
      ) : (
        <p className="r-execution-empty">
          {!authenticated
            ? '等待模拟会话'
            : busy === 'draft_execute'
              ? '保存结果待核实'
              : querying
                ? '正在查询原操作'
                : attempted
                  ? '已记录保存尝试；仅可查询原任务与操作。'
                  : canRequest
                    ? '尚未保存模拟草案'
                    : '尚未取得原操作历史回执'}
        </p>
      )}
    </section>
  );
}
