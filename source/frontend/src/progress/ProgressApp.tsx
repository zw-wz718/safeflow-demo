import { useCallback, useEffect, useId, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import {
  Activity,
  ArrowRight,
  Check,
  CheckCheck,
  ChevronRight,
  CircleAlert,
  ClipboardList,
  Download,
  FileText,
  GitBranch,
  Layers3,
  Link2,
  RefreshCw,
  Search,
  ShieldCheck,
  Users,
  WifiOff,
  X,
} from 'lucide-react';
import {
  exportProgress,
  fetchProgress,
  initialSyncState,
  syncFailed,
  syncSucceeded,
} from './client';
import {
  deliveryDate,
  deliveryLabels,
  filterTasks,
  filterDependencies,
  memberCounts,
  orderedDeliveries,
  statusLabels,
  statuses,
} from './schema';
import type { Progress, Source, Task } from './schema';

type View = 'overview' | 'members' | 'deliveries' | 'dependencies';
type Selection = { kind: 'task' | 'delivery' | 'dependency'; id: string };
const views = [
  { id: 'overview', label: '总览', icon: Layers3 },
  { id: 'members', label: '成员与阶段', icon: Users },
  { id: 'deliveries', label: '交付台账', icon: ClipboardList },
  { id: 'dependencies', label: '待对接', icon: Link2 },
] as const;
const foundationStageNames: Record<string, string> = {
  'BASE-01': '共同骨架',
  'BASE-02': '字段与合同',
  'BASE-03': '计划与安全',
  'BASE-04': '迁移与事务',
  'BASE-05': '恢复与 HTTP',
  'BASE-06': 'LLM 与页面',
};
const sourceLabels: Record<Source['kind'], string> = {
  independent: '独立验收通过',
  review: '独立检查 · 待修复',
  'member-report': '组员报告完成',
  implementation: '实现说明',
  plan: 'PRD 计划依据',
  handoff: '接入合同',
};
const dateText = (date: string | null) => date ?? '日期未确认';
const timeText = (date: string | null) =>
  date
    ? new Intl.DateTimeFormat('zh-CN', {
        timeZone: 'Asia/Shanghai',
        month: '2-digit',
        day: '2-digit',
        hour: '2-digit',
        minute: '2-digit',
        second: '2-digit',
        hour12: false,
      }).format(new Date(date))
    : '尚无成功读取';
function Badge({ status, children }: { status: string; children: ReactNode }) {
  return <span className={`p-badge ${status}`}>{children}</span>;
}
function Checks({
  id,
  implemented,
  accepted,
}: {
  id: string;
  implemented: boolean;
  accepted: boolean;
}) {
  return (
    <div className="p-checks">
      {(['实现', '验收'] as const).map((label, i) => (
        <label
          key={label}
          title={`${label}：${(i === 0 ? implemented : accepted) ? '已记录' : '未完成或未收到证据'}`}
        >
          <input
            type="checkbox"
            aria-label={`${id} ${label}`}
            checked={i === 0 ? implemented : accepted}
            disabled
            readOnly
          />
          {(i === 0 ? implemented : accepted) && <Check size={12} aria-hidden="true" />}
          <span className="p-sr">{label}</span>
        </label>
      ))}
    </div>
  );
}
function Dialog({
  title,
  onClose,
  children,
}: {
  title: string;
  onClose: () => void;
  children: ReactNode;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  const titleId = useId();
  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    const node = ref.current!;
    node.showModal();
    return () => {
      node.close();
      previous?.focus();
    };
  }, []);
  return (
    <dialog
      ref={ref}
      className="p-dialog"
      aria-labelledby={titleId}
      onCancel={(event) => {
        event.preventDefault();
        onClose();
      }}
    >
      <div className="p-dialog-header">
        <h2 id={titleId}>{title}</h2>
        <button className="p-icon" aria-label="关闭详情" title="关闭详情" onClick={onClose}>
          <X size={20} />
        </button>
      </div>
      <div className="p-dialog-body">{children}</div>
    </dialog>
  );
}
function Evidence({ sources }: { sources: Source[] }) {
  const [selected, setSelected] = useState<Source | null>(null);
  const [content, setContent] = useState<string | null>(null);
  const [error, setError] = useState(false);
  useEffect(() => {
    if (!selected) return;
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 8000);
    setContent(null);
    setError(false);
    fetch(`/api/progress/evidence/${encodeURIComponent(selected.id)}`, {
      signal: controller.signal,
      cache: 'no-store',
    })
      .then(async (response) => {
        if (!response.ok) throw new Error();
        const body: unknown = await response.json();
        if (
          !body ||
          typeof body !== 'object' ||
          !('sourceId' in body) ||
          body.sourceId !== selected.id ||
          !('content' in body) ||
          typeof body.content !== 'string'
        )
          throw new Error();
        if (!controller.signal.aborted) setContent(body.content);
      })
      .catch(() => {
        if (!controller.signal.aborted) setError(true);
        else if (!disposed) setError(true);
      });
    let disposed = false;
    return () => {
      disposed = true;
      clearTimeout(timeout);
      controller.abort();
    };
  }, [selected]);
  return (
    <section className="p-evidence">
      <h3>来源与证据</h3>
      {sources.map((source) => (
        <div className="p-source" key={source.id}>
          <div>
            <Badge status={source.kind}>{sourceLabels[source.kind]}</Badge>
            <strong>{source.title}</strong>
            <p>{source.scope}</p>
            <code>{source.path}</code>
            <small>
              {dateText(source.date)} · {source.version ?? '版本未确认'}
            </small>
          </div>
          <button
            className="p-icon"
            aria-label={`读取${source.title}`}
            title={`读取${source.title}`}
            onClick={() => setSelected(source)}
          >
            <FileText size={18} />
          </button>
          {source.url && (
            <a className="p-source-link" href={source.url} target="_blank" rel="noreferrer">
              打开分支 <ArrowRight size={14} />
            </a>
          )}
        </div>
      ))}
      {selected && (
        <div className="p-evidence-content">
          <div className="p-subheading">
            <strong>{selected.title}</strong>
            <button
              className="p-icon"
              aria-label="关闭证据内容"
              title="关闭证据内容"
              onClick={() => setSelected(null)}
            >
              <X size={18} />
            </button>
          </div>
          {error ? (
            <p role="alert">证据暂时无法读取。请检查已登记文件后重试。</p>
          ) : content === null ? (
            <p role="status">正在读取证据…</p>
          ) : (
            <pre tabIndex={0}>{content}</pre>
          )}
        </div>
      )}
    </section>
  );
}
function TaskRows({
  tasks,
  select,
  childStatus,
}: {
  tasks: Task[];
  select: (task: Task) => void;
  childStatus: string;
}) {
  return (
    <div className="p-task-list">
      <div className="p-table-heading">
        <span>实现 / 验收</span>
        <span>任务与范围</span>
        <span>当前状态</span>
        <span>交付版本</span>
      </div>
      {tasks.map((task) => (
        <div className="p-task-group" key={task.id}>
          <article className="p-task-row">
            <Checks id={task.id} implemented={task.implemented} accepted={task.accepted} />
            <button className="p-task-title" onClick={() => select(task)}>
              <span className="p-task-code">
                {task.id}
                <em>{task.priority}</em>
              </span>
              <strong>{task.title}</strong>
              <p>{task.scope}</p>
            </button>
            <Badge status={task.status}>{statusLabels[task.status]}</Badge>
            <code className="p-version">{task.version ?? '未收到版本'}</code>
            <button
              className="p-icon p-row-detail"
              aria-label={`查看 ${task.id} 详情`}
              title={`查看 ${task.id} 详情`}
              onClick={() => select(task)}
            >
              <ChevronRight size={18} />
            </button>
          </article>
          {task.subtasks
            .filter((sub) =>
              childStatus === 'accepted'
                ? sub.accepted
                : childStatus === 'pending-review'
                  ? sub.implemented && !sub.accepted
                  : true,
            )
            .map((sub) => (
              <div className="p-list-subtask" key={sub.id}>
                <Checks
                  id={`${task.id} ${sub.title}`}
                  implemented={sub.implemented}
                  accepted={sub.accepted}
                />
                <div>
                  <strong>{sub.title}</strong>
                  <p>{sub.scope}</p>
                </div>
                <Badge
                  status={sub.accepted ? 'accepted' : sub.implemented ? 'member-report' : 'unknown'}
                >
                  {sub.accepted ? '独立验收通过' : sub.implemented ? '已实现 · 待验收' : '待补证据'}
                </Badge>
              </div>
            ))}
        </div>
      ))}
    </div>
  );
}
function Detail({ selection, data }: { selection: Selection; data: Progress }) {
  const task =
    selection.kind === 'task' ? data.tasks.find((t) => t.id === selection.id) : undefined;
  const delivery =
    selection.kind === 'delivery' ? data.deliveries.find((d) => d.id === selection.id) : undefined;
  const dependency =
    selection.kind === 'dependency'
      ? data.dependencies.find((d) => d.id === selection.id)
      : undefined;
  const item = task ?? delivery ?? dependency;
  if (!item) return <p>该记录在最新台账中已变化，请关闭后重新查看。</p>;
  const sourceIds = new Set([
    ...item.sourceIds,
    ...(task ? [task.planSourceId, ...task.subtasks.flatMap((s) => s.sourceIds)] : []),
  ]);
  return (
    <>
      {task && (
        <>
          <div className="p-detail-meta">
            <Badge status={task.status}>{statusLabels[task.status]}</Badge>
            <span>{data.members.find((m) => m.id === task.ownerId)?.name}</span>
            <span>{task.requirements.join(' / ')}</span>
          </div>
          <p>{task.scope}</p>
          <dl>
            <dt>交付版本</dt>
            <dd>
              <code>{task.version ?? '未收到版本'}</code>
            </dd>
          </dl>
          <h3>已做</h3>
          {task.completed.length ? (
            <ul>
              {task.completed.map((text) => (
                <li key={text}>{text}</li>
              ))}
            </ul>
          ) : (
            <p>未收到本任务的完成证据。</p>
          )}
          {task.subtasks.length > 0 && (
            <section>
              <div className="p-subheading">
                <h3>必要子项</h3>
                <small>实现 / 验收</small>
              </div>
              {task.subtasks.map((sub) => (
                <div className="p-subtask" key={sub.id}>
                  <Checks
                    id={`${task.id} ${sub.title}`}
                    implemented={sub.implemented}
                    accepted={sub.accepted}
                  />
                  <div>
                    <strong>{sub.title}</strong>
                    <p>{sub.scope}</p>
                    {sub.implemented && !sub.accepted && (
                      <Badge status="member-report">组员报告 · 待验收</Badge>
                    )}
                  </div>
                </div>
              ))}
            </section>
          )}
          <h3>仍缺</h3>
          <ul>
            {task.gaps.map((text) => (
              <li key={text}>{text}</li>
            ))}
          </ul>
          <h3>下一步</h3>
          <p>{task.next}</p>
        </>
      )}
      {delivery && (
        <>
          <Badge status={delivery.state}>{deliveryLabels[delivery.state]}</Badge>
          <dl>
            <dt>负责人</dt>
            <dd>{data.members.find((m) => m.id === delivery.ownerId)?.name}</dd>
            <dt>版本</dt>
            <dd>
              <code>{delivery.version}</code>
            </dd>
            <dt>位置 / 分支</dt>
            <dd>
              <code>{delivery.location}</code>
            </dd>
            <dt>收到日期</dt>
            <dd>{dateText(delivery.receivedAt)}</dd>
            <dt>{delivery.state === 'changes-requested' ? '复核日期' : '验收日期'}</dt>
            <dd>{dateText(delivery.reviewedAt)}</dd>
            <dt>登记日期</dt>
            <dd>{delivery.recordedAt}</dd>
            <dt>影响任务</dt>
            <dd>{delivery.taskIds.join(' / ')}</dd>
          </dl>
          <h3>声明完成</h3>
          <p>{delivery.declared}</p>
          <h3>接收与验收结果</h3>
          <p>{delivery.result}</p>
          <h3>范围边界</h3>
          <p>{delivery.scope}</p>
        </>
      )}
      {dependency && (
        <>
          <Badge status="pending-integration">待对接</Badge>
          <dl>
            <dt>负责人</dt>
            <dd>{dependency.ownerNote}</dd>
            <dt>影响任务</dt>
            <dd>{dependency.taskIds.join(' / ')}</dd>
          </dl>
          <h3>缺少材料</h3>
          <p>{dependency.material}</p>
          <h3>影响</h3>
          <p>{dependency.impact}</p>
          <h3>恢复条件</h3>
          <p>{dependency.resumeWhen}</p>
        </>
      )}
      <Evidence sources={data.sources.filter((s) => sourceIds.has(s.id))} />
    </>
  );
}
export function ProgressApp() {
  const [sync, setSync] = useState(initialSyncState);
  const [busy, setBusy] = useState(false);
  const [view, setView] = useState<View>(() =>
    views.some((v) => v.id === location.hash.slice(1))
      ? (location.hash.slice(1) as View)
      : 'overview',
  );
  const [search, setSearch] = useState('');
  const [owner, setOwner] = useState('all');
  const [status, setStatus] = useState('all');
  const [selection, setSelection] = useState<Selection | null>(null);
  const mounted = useRef(false);
  const active = useRef<AbortController | null>(null);
  const refresh = useCallback(async () => {
    if (active.current) return;
    const controller = new AbortController();
    active.current = controller;
    setBusy(true);
    const timer = setTimeout(() => controller.abort(), 8000);
    try {
      const data = await fetchProgress(controller.signal);
      if (mounted.current) setSync(syncSucceeded(data));
    } catch {
      if (mounted.current) setSync((previous) => syncFailed(previous));
    } finally {
      clearTimeout(timer);
      active.current = null;
      if (mounted.current) setBusy(false);
    }
  }, []);
  useEffect(() => {
    mounted.current = true;
    void refresh();
    const timer = setInterval(() => void refresh(), 15000);
    return () => {
      mounted.current = false;
      clearInterval(timer);
      active.current?.abort();
    };
  }, [refresh]);
  useEffect(() => {
    const changed = () => {
      const next = location.hash.slice(1);
      if (views.some((v) => v.id === next)) setView(next as View);
    };
    addEventListener('hashchange', changed);
    return () => removeEventListener('hashchange', changed);
  }, []);
  const navigate = (next: View) => {
    setView(next);
    location.hash = next;
    setSelection(null);
    setStatus('all');
  };
  const reset = () => {
    setSearch('');
    setOwner('all');
    setStatus('all');
  };
  const data = sync.data;
  const currentReview = data?.deliveries.find(
    (delivery) =>
      delivery.state === 'accepted' &&
      delivery.reviewedAt === data.current.reviewedOn &&
      delivery.sourceIds.includes(data.current.sourceId),
  );
  const tasks = data ? filterTasks(data, search, owner, status) : [];
  const scopedSet = new Set(data ? filterTasks(data, '', owner, status).map((t) => t.id) : []);
  const dependencies = data ? filterDependencies(data, search, owner, status) : [];
  const deliveries = data
    ? orderedDeliveries(data).filter(
        (d) =>
          (owner === 'all' || d.ownerId === owner) &&
          (status === 'all' ||
            (view === 'deliveries'
              ? d.state === status
              : d.taskIds.some((id) => scopedSet.has(id)))) &&
          (!search.trim() ||
            `${d.title} ${d.version} ${d.location} ${d.scope} ${d.taskIds.join(' ')} ${data.members.find((m) => m.id === d.ownerId)?.name}`
              .toLocaleLowerCase()
              .includes(search.trim().toLocaleLowerCase())),
      )
    : [];
  const selectTask = (task: Task) => setSelection({ kind: 'task', id: task.id });
  const title =
    selection && data
      ? ((selection.kind === 'task'
          ? data.tasks.find((t) => t.id === selection.id)?.title
          : selection.kind === 'delivery'
            ? data.deliveries.find((d) => d.id === selection.id)?.title
            : data.dependencies.find((d) => d.id === selection.id)?.title) ?? '记录详情')
      : '';
  return (
    <div className="p-app">
      <header className="p-topbar">
        <div className="p-brand">
          <ShieldCheck size={27} aria-hidden="true" />
          <div>
            <strong>SafeFlow</strong>
            <span>项目进度</span>
          </div>
          <span className="p-local">本地工作台</span>
        </div>
        <div className="p-sync-tools">
          <div className={`p-connection ${sync.status}`} role="status">
            {sync.status === 'synced' ? <Activity size={14} /> : <WifiOff size={14} />}
            {sync.status === 'synced'
              ? '台账已同步'
              : sync.status === 'loading'
                ? '读取中'
                : '未同步'}
          </div>
          <button
            className={`p-icon ${busy ? 'p-refreshing' : ''}`}
            aria-label="立即刷新"
            title="立即刷新"
            disabled={busy}
            onClick={() => void refresh()}
          >
            <RefreshCw size={18} />
          </button>
          <button
            className="p-icon"
            aria-label="导出 JSON"
            title="导出 JSON"
            disabled={!data}
            onClick={() => data && exportProgress(data)}
          >
            <Download size={18} />
          </button>
        </div>
      </header>
      <div className="p-nav-wrap">
        <nav aria-label="进度视图">
          {views.map(({ id, label, icon: Icon }) => (
            <button
              key={id}
              aria-current={view === id ? 'page' : undefined}
              className={view === id ? 'active' : ''}
              onClick={() => navigate(id)}
            >
              <Icon size={17} />
              {label}
              {id === 'dependencies' && data && <span>{data.dependencies.length}</span>}
            </button>
          ))}
        </nav>
        <span className="p-update">台账更新 {data ? timeText(data.updatedAt) : '尚未读取'}</span>
      </div>
      <main>
        {sync.status === 'unsynced' && (
          <div className="p-sync-error" role="alert">
            <CircleAlert size={18} />
            <div>
              <strong>未同步 · {data ? '显示上次成功读取的台账' : '台账暂时不可用'}</strong>
              <p>最后成功读取：{timeText(sync.lastSuccessAt)}。请检查本地服务与台账文件。</p>
            </div>
          </div>
        )}
        {!data ? (
          <div className="p-empty">
            <FileText size={30} />
            <h1>{sync.status === 'loading' ? '正在读取项目进度' : '无法读取项目台账'}</h1>
            <p>本地台账：docs/progress/project-progress.json</p>
            {sync.status === 'unsynced' && (
              <button className="p-command" onClick={() => void refresh()} disabled={busy}>
                <RefreshCw size={16} />
                重新读取
              </button>
            )}
          </div>
        ) : (
          <>
            <section className="p-filterbar" aria-label="进度筛选">
              <label className="p-search">
                <Search size={17} />
                <input
                  aria-label="搜索进度"
                  placeholder="搜索任务、版本或缺口"
                  value={search}
                  onChange={(event) => setSearch(event.target.value)}
                />
                {search && (
                  <button aria-label="清除搜索" title="清除搜索" onClick={() => setSearch('')}>
                    <X size={16} />
                  </button>
                )}
              </label>
              <select
                aria-label="筛选负责人"
                value={owner}
                onChange={(event) => setOwner(event.target.value)}
              >
                <option value="all">全部负责人</option>
                {data.members.map((member) => (
                  <option key={member.id} value={member.id}>
                    {member.name}
                  </option>
                ))}
              </select>
              <select
                aria-label="筛选状态"
                value={status}
                onChange={(event) => setStatus(event.target.value)}
              >
                <option value="all">全部状态</option>
                {(view === 'deliveries'
                  ? Object.entries(deliveryLabels)
                  : statuses.map((item) => [item, statusLabels[item]])
                ).map(([item, label]) => (
                  <option value={item} key={item}>
                    {label}
                  </option>
                ))}
              </select>
            </section>
            {view === 'overview' && (
              <>
                <section className="p-current">
                  <div>
                    <div className="p-eyebrow">
                      <Badge status="accepted">
                        <CheckCheck size={14} />
                        最新独立验收 {data.current.reviewedOn}
                      </Badge>
                      <span>底座 · 限定范围</span>
                    </div>
                    <h1>{data.current.headline}</h1>
                    <ul>
                      {data.current.limits.map((limit) => (
                        <li key={limit}>{limit}</li>
                      ))}
                    </ul>
                  </div>
                  <div className="p-test-summary">
                    <span>最新回归快照</span>
                    <div>
                      <strong>{data.current.tests.ordinary}</strong>
                      <span>普通</span>
                      <strong>{data.current.tests.postgres}</strong>
                      <span>真实 PG</span>
                      <strong>{data.current.tests.doc}</strong>
                      <span>文档</span>
                      <strong>{data.current.tests.independent}</strong>
                      <span>独立</span>
                    </div>
                    <button
                      className="p-text-command"
                      disabled={!currentReview}
                      title={currentReview ? '查看本轮独立验收范围' : '当前验收交付未登记'}
                      onClick={() =>
                        currentReview && setSelection({ kind: 'delivery', id: currentReview.id })
                      }
                    >
                      查看验收范围 <ArrowRight size={15} />
                    </button>
                  </div>
                </section>
                <section className="p-stages" aria-label="底座原任务阶段">
                  {data.tasks
                    .filter((t) => t.ownerId === 'foundation')
                    .map((task) => (
                      <button
                        key={task.id}
                        title={task.title}
                        aria-label={`查看 ${task.id} ${task.title}`}
                        onClick={() => selectTask(task)}
                      >
                        <span>
                          {task.id}
                          <ChevronRight size={14} />
                        </span>
                        <strong>{foundationStageNames[task.id] ?? task.title}</strong>
                        <Badge status={task.status}>{statusLabels[task.status]}</Badge>
                      </button>
                    ))}
                </section>
                <div className="p-section-heading">
                  <h2>五人概览</h2>
                  <span>实现 / 独立验收分开记录</span>
                </div>
                <section className="p-members" aria-label="成员概览">
                  {data.members
                    .filter((member) => tasks.some((t) => t.ownerId === member.id))
                    .map((member) => {
                      const owned = data.tasks.filter((task) => task.ownerId === member.id);
                      const counts = memberCounts(owned);
                      const version = owned.find((t) => t.version)?.version;
                      return (
                        <button
                          className="p-member"
                          key={member.id}
                          onClick={() => {
                            setOwner(member.id);
                            navigate('members');
                          }}
                        >
                          <div className="p-member-top">
                            <span className={`p-avatar ${member.id}`}>
                              {member.name.slice(0, 1)}
                            </span>
                            <div>
                              <h3>
                                {member.name}
                                <ChevronRight size={14} />
                              </h3>
                              <p>{member.role}</p>
                            </div>
                          </div>
                          <strong className="p-member-stage">{member.stage}</strong>
                          <div className="p-member-meter">
                            <progress
                              max={counts.total || 1}
                              value={counts.accepted}
                              aria-label={`${member.name} 已验收子项`}
                            />
                            <span>
                              实现 {counts.implemented} · 验收 {counts.accepted} / {counts.total}
                            </span>
                          </div>
                          <small>{version ?? '未收到版本'}</small>
                        </button>
                      );
                    })}
                </section>
                {!tasks.length && <Empty reset={reset} />}
                <div className="p-lower-grid">
                  <section>
                    <div className="p-section-heading">
                      <h2>最近交付</h2>
                      <button className="p-text-command" onClick={() => navigate('deliveries')}>
                        全部台账 <ArrowRight size={14} />
                      </button>
                    </div>
                    {deliveries.slice(0, 4).map((delivery) => (
                      <div className="p-recent" key={delivery.id}>
                        <Checks
                          id={`交付 ${delivery.title}`}
                          implemented={
                            delivery.state === 'accepted' || delivery.state === 'member-report'
                          }
                          accepted={delivery.state === 'accepted'}
                        />
                        <button
                          className="p-recent-title"
                          onClick={() => setSelection({ kind: 'delivery', id: delivery.id })}
                        >
                          <strong>{delivery.title}</strong>
                          <p>
                            {data.members.find((m) => m.id === delivery.ownerId)?.name} ·{' '}
                            {delivery.version}
                          </p>
                        </button>
                        <Badge status={delivery.state}>{deliveryLabels[delivery.state]}</Badge>
                      </div>
                    ))}
                  </section>
                  <section>
                    <div className="p-section-heading">
                      <h2>当前对接缺口</h2>
                      <button className="p-text-command" onClick={() => navigate('dependencies')}>
                        查看全部 <ArrowRight size={14} />
                      </button>
                    </div>
                    {dependencies.slice(0, 3).map((d) => (
                      <button
                        className="p-gap-summary"
                        key={d.id}
                        onClick={() => setSelection({ kind: 'dependency', id: d.id })}
                      >
                        <span className="p-amber-dot" />
                        <div>
                          <strong>{d.title}</strong>
                          <p>{d.ownerNote}</p>
                        </div>
                        <ChevronRight size={17} />
                      </button>
                    ))}
                  </section>
                </div>
              </>
            )}
            {view === 'members' && (
              <>
                <div className="p-section-heading">
                  <h1>成员与阶段</h1>
                  <span>{tasks.length} 个任务</span>
                </div>
                {data.members.map((member) => {
                  const owned = tasks.filter((task) => task.ownerId === member.id);
                  if (!owned.length) return null;
                  return (
                    <section className="p-owner-section" key={member.id}>
                      <div className="p-owner-heading">
                        <span className={`p-avatar ${member.id}`}>{member.name.slice(0, 1)}</span>
                        <div>
                          <h2>
                            {member.name}
                            <span>{member.role}</span>
                          </h2>
                          <p>下一步：{member.next}</p>
                        </div>
                      </div>
                      <TaskRows tasks={owned} select={selectTask} childStatus={status} />
                    </section>
                  );
                })}
                {!tasks.length && <Empty reset={reset} />}
              </>
            )}
            {view === 'deliveries' && (
              <>
                <div className="p-section-heading">
                  <h1>交付台账</h1>
                  <span>{deliveries.length} 条记录 · 保留历史</span>
                </div>
                <div className="p-deliveries">
                  {deliveries.map((delivery) => (
                    <article key={delivery.id} className="p-delivery">
                      <div className="p-delivery-date">
                        <span>{dateText(deliveryDate(delivery).value)}</span>
                        <small>{deliveryDate(delivery).label}</small>
                      </div>
                      <div className="p-delivery-body">
                        <div className="p-subheading">
                          <strong>{delivery.title}</strong>
                          <Badge status={delivery.state}>{deliveryLabels[delivery.state]}</Badge>
                        </div>
                        <p>{delivery.scope}</p>
                        <code>
                          <GitBranch size={14} />
                          {delivery.version}
                        </code>
                        <small>
                          {data.members.find((m) => m.id === delivery.ownerId)?.name} ·{' '}
                          {delivery.location}
                        </small>
                        <p className="p-delivery-result">{delivery.result}</p>
                      </div>
                      <button
                        className="p-icon"
                        aria-label={`查看 ${delivery.title} 交付`}
                        title="查看交付详情"
                        onClick={() => setSelection({ kind: 'delivery', id: delivery.id })}
                      >
                        <ChevronRight size={18} />
                      </button>
                    </article>
                  ))}
                </div>
                {!deliveries.length && <Empty reset={reset} />}
              </>
            )}
            {view === 'dependencies' && (
              <>
                <div className="p-section-heading">
                  <h1>待对接</h1>
                  <span>{dependencies.length} 个缺口</span>
                </div>
                <div className="p-dependencies">
                  {dependencies.map((d) => (
                    <article className="p-dependency" key={d.id}>
                      <div className="p-subheading">
                        <h2>{d.title}</h2>
                        <Badge status="pending-integration">待对接</Badge>
                      </div>
                      <p className="p-dependency-owner">{d.ownerNote}</p>
                      <dl>
                        <dt>缺少材料</dt>
                        <dd>{d.material}</dd>
                        <dt>影响</dt>
                        <dd>{d.impact}</dd>
                      </dl>
                      <div className="p-dependency-bottom">
                        <span>{d.taskIds.join(' · ')}</span>
                        <button
                          className="p-text-command"
                          onClick={() => setSelection({ kind: 'dependency', id: d.id })}
                        >
                          恢复条件与证据 <ArrowRight size={14} />
                        </button>
                      </div>
                    </article>
                  ))}
                </div>
                {!dependencies.length && <Empty reset={reset} />}
              </>
            )}
            <footer>
              <span>本地进度台账 · 不等于业务接通</span>
              <span>
                台账更新 {timeText(data.updatedAt)} · 最后成功读取 {timeText(sync.lastSuccessAt)}
              </span>
            </footer>
          </>
        )}
      </main>
      {selection && data && (
        <Dialog title={title} onClose={() => setSelection(null)}>
          <Detail selection={selection} data={data} />
        </Dialog>
      )}
    </div>
  );
}
function Empty({ reset }: { reset: () => void }) {
  return (
    <div className="p-empty">
      <Search size={28} />
      <h2>没有匹配的结果</h2>
      <p>当前筛选下没有已登记记录。</p>
      <button className="p-command" onClick={reset}>
        <RefreshCw size={16} />
        清除筛选
      </button>
    </div>
  );
}
