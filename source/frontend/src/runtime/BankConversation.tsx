import { useEffect, useRef, useState, type ReactNode } from 'react';
import { z } from 'zod';
import { Send, Plus, ShieldCheck } from 'lucide-react';
import {
  agentStatus,
  agentSchema,
  getAgent,
  runRead,
  prepareAgent,
  refreshRecipient,
  selectSavedDraftCancellation,
  intentTerminationSchema,
  actDraft,
  searchPayees,
  viewId,
  type AgentView,
  type AgentStatus,
  type Payee,
} from './agent-client';
import { ResultRows } from './AgentResults';
import { request, RuntimeApiError } from './client';
import ChatPayment from './ChatPayment';
import { recoveryDirectory, recoverPaymentTasks, verifiedNonfundingClosure, replacementIntent } from './bank-recovery';
import { runtimeErrorText } from './runtime-feedback';
import { type PaymentFunds } from './payment-schema';
import { prepareChatHistory, clearChatHistory, type ChatHistory } from './chat-history';

type Service = 'BILLS' | 'CARDS' | 'SUBSCRIPTIONS';
type Progress = {
  scope: string;
  state: PaymentFunds['state'] | 'SUBMITTED' | 'CHECKING' | 'UNKNOWN';
};
function scopeOf(view?: AgentView) {
  return view?.kind === 'DRAFT' || view?.kind === 'PAYMENT'
    ? `${view.draft.task_id}:${view.draft.plan_version}`
    : null;
}
type Message = {
  role: 'user' | 'assistant';
  text: string;
  task?: string;
  outcome?: string;
  panel?: Service;
  panelId?: string;
  panelState?: 'LOADING' | 'READY' | 'FAILED';
  readTurn?: string;
  readView?: AgentView;
};
type Memory = {
  messages: Message[];
  tasks: string[];
  active: string;
  conversation: string;
  revision: number;
  pending: boolean;
  payeeSelection?: { query: string; id: string };
};
const memorySchema = z
  .object({
    messages: z
      .array(
        z
          .object({
            role: z.enum(['user', 'assistant']),
            text: z.string().max(4096),
            task: z.string().optional(),
            outcome: z.string().optional(),
            panel: z.enum(['BILLS', 'CARDS', 'SUBSCRIPTIONS']).optional(),
            panelId: z.string().uuid().optional(),
            panelState: z.enum(['LOADING', 'READY', 'FAILED']).optional(),
            readTurn: z.string().uuid().optional(),
            readView: agentSchema.refine((v) => v.kind === 'READ').optional(),
          })
          .strict(),
      )
      .max(80),
    tasks: z.array(z.string()).max(256),
    active: z.string(),
    conversation: z.string().uuid(),
    revision: z.number().int().nonnegative(),
    pending: z.boolean(),
    payeeSelection: z
      .object({ query: z.string().max(4096), id: z.string().max(200) })
      .strict()
      .optional(),
  })
  .strict();
const responseSchema = z
  .object({
    conversation_id: z.string().uuid(),
    revision: z.number().int().positive(),
    task_id: z.string().nullable(),
    view: agentSchema.nullable(),
    uncertain: z.boolean(),
    payee_query: z.string().nullable(),
  })
  .strict();
const empty = (): Memory => ({
  messages: [],
  tasks: [],
  active: '',
  conversation: crypto.randomUUID(),
  revision: 0,
  pending: false,
});
function describe(v: AgentView) {
  if (v.kind === 'CLARIFY') return v.clarification.question;
  if (v.kind === 'DRAFT' && v.draft.intent_termination)
    return '这份未付款草案已结束；原记录保留，不再恢复旧确认。';
  if (v.kind === 'DRAFT')
    return v.draft.task_state === 'CANCELLED'
      ? '草案已取消；付款状态仍以原资金回执核对。'
      : v.draft.execution_status === 'SUCCEEDED'
        ? '草案已保存。请核对付款信息，另行确认付款和后台委托。'
        : '请核对这笔草案。确认只保存草案，尚未付款。';
  if (v.kind === 'PAYMENT') return '请按原单核对付款状态；确认、验证和资金结果分别显示。';
  if (v.task.task_state === 'COMPLETED') {
    const balances = (v.results ?? []).flatMap((r) =>
      r.tool_name === 'get_account_balance'
        ? [
            `${r.data.account.display_name}（${r.data.account.masked_account_no}）可用余额为 ¥${r.data.available_balance}，数据时间 ${new Date(r.data.as_of).toLocaleString('zh-CN', { timeZone: 'Asia/Shanghai', hour12: false })}（北京时间）`,
          ]
        : [],
    );
    if (balances.length) return balances.join('；') + '。';
    const found = v.results?.find((r) => r.tool_name === 'find_payees');
    if (found?.tool_name === 'find_payees')
      return found.data.candidates.length
        ? '已查到以下已登记收款人，请选择实际对象后继续。'
        : '本次可信查询没有返回匹配收款人，请核对姓名重新说明。';
  }
  return v.task.task_state === 'COMPLETED'
    ? '查询已完成，请核对下方的查询明细。'
    : v.task.task_state === 'FAILED'
      ? '查询未完成，请查看原因。'
      : '正在读取原查询的实际结果。';
}
function errorText(e: unknown) {
  return runtimeErrorText(e, 'agent');
}
type Props = {
  accounts: { account_id: string; display_name: string }[];
  disabled: boolean;
  run: (work: () => Promise<void>) => Promise<void>;
  businessPanel: ReactNode;
  businessPanelService?: Service;
  openService: (service: Service, signal: AbortSignal) => Promise<void>;
};
export default function BankConversation(props: Props) {
  const [session, setSession] = useState<{ history: ChatHistory; directory: Awaited<ReturnType<typeof recoveryDirectory>> }>();
  const [error, setError] = useState('');
  const [clearing, setClearing] = useState(false);
  const scope = useRef<ChatHistory | undefined>(undefined);
  useEffect(() => {
    const c = new AbortController();
    const load = async () => {
      const directory = await recoveryDirectory(c.signal);
      const history = await prepareChatHistory(directory.context_ref, directory.payment_tasks);
      if (c.signal.aborted || !history.current()) return;
      scope.current = history;
      setSession(previous => previous?.history.generation === history.generation ? previous : { history, directory });
      setError('');
    };
    const synchronize = () => {
      if (scope.current && !scope.current.current()) {
        setSession(undefined);
        void load().catch(() => { if (!c.signal.aborted) setError('聊天清理同步未完成，请刷新重试；未开始新请求。'); });
      }
    };
    void load().catch(() => { if (!c.signal.aborted) setError('聊天清理未完成，请刷新重试；办理原号和业务记录保留。'); });
    const changed = (event: StorageEvent) => { if (!event.key || event.key === scope.current?.marker) synchronize(); };
    window.addEventListener('storage', changed);
    window.addEventListener('focus', synchronize);
    window.addEventListener('pageshow', synchronize);
    document.addEventListener('visibilitychange', synchronize);
    return () => { c.abort(); window.removeEventListener('storage', changed); window.removeEventListener('focus', synchronize); window.removeEventListener('pageshow', synchronize); document.removeEventListener('visibilitychange', synchronize); };
  }, []);
  async function clear() {
    if (!session || clearing) return;
    setClearing(true);
    try {
      const history = await clearChatHistory(session.history.context);
      scope.current = history;
      // The changed key unmounts all old async operations and service/payment cards.
      setSession(undefined);
      const directory = await recoveryDirectory();
      if (history.current()) {
        if (directory.context_ref !== history.context) throw Error('本人聊天范围不匹配');
        history.retainBlockers(directory.payment_tasks);
        setSession({ history, directory });
      }
      setError('');
    } catch { setError('未能完整清理聊天，请重试；业务记录和恢复编号保留。'); }
    finally { setClearing(false); }
  }
  return <>
    <div className="chat-service-actions">
      <button onClick={() => void clear()} disabled={!session || clearing}>{clearing ? '正在清空聊天…' : '清空聊天'}</button>
      <small>仅清除聊天和提示；业务记录仍可按原号查询。</small>
    </div>
    {error && <p role="alert">{error}</p>}
    {session && !error && <ConversationBody key={session.history.generation} {...props} history={session.history} initialDirectory={session.directory} />}
  </>;
}
function ConversationBody({
  accounts,
  disabled,
  run,
  businessPanel,
  businessPanelService,
  openService,
  history,
  initialDirectory,
}: Props & { history: ChatHistory; initialDirectory: Awaited<ReturnType<typeof recoveryDirectory>> }) {
  const [owned, setOwned] = useState(accounts),
    [status, setStatus] = useState<AgentStatus>(),
    [view, setView] = useState<AgentView>(),
    [text, setText] = useState(''),
    [account, setAccount] = useState(''),
    [query, setQuery] = useState(''),
    [candidateQuery, setCandidateQuery] = useState(''),
    [payees, setPayees] = useState<Payee[]>([]),
    [payee, setPayee] = useState<{ query: string; value: Payee }>(),
    [messages, setMessages] = useState<Message[]>([]),
    [notice, setNotice] = useState('正在读取本人会话…'),
    [ready, setReady] = useState(false),
    [busy, setBusy] = useState(false),
    [uncertain, setUncertain] = useState(false),
    [progress, setProgress] = useState<Progress>(),
    [collapsedPanels, setCollapsedPanels] = useState<Set<string>>(new Set()),
    [activePanelId, setActivePanelId] = useState<string>(),
    [readEpoch, setReadEpoch] = useState(0),
    [recoveryChecked, setRecoveryChecked] = useState(false),
    [openTasks, setOpenTasks] = useState<string[]>([]);
  const [checkedAt, setCheckedAt] = useState(''),
    [hasNewResult, setHasNewResult] = useState(false);
  const [replacementAmount,setReplacementAmount]=useState('');
  const [draftSeconds,setDraftSeconds]=useState<number>();
  const [readBusy, setReadBusy] = useState(false),
    [readNotice, setReadNotice] = useState(''),
    [readMode, setReadMode] = useState(false),
    [scanBusy, setScanBusy] = useState(false);
  const readController = useRef<AbortController | null>(null);
  const activeReadTurn = useRef('');
  const readState = useRef<{ conversation_id: string; revision: number; pending: boolean }>({ conversation_id: crypto.randomUUID(), revision: 0, pending: false });
  const readResponseSchema = z.object({
    conversation_id: z.string().uuid(), revision: z.number().int().positive(),
    view: agentSchema.refine((v) => v.kind === 'READ' || v.kind === 'CLARIFY').nullable(),
    uncertain: z.boolean(),
  }).strict();
  function saveRead(change: Partial<typeof readState.current>) {
    if (!history.current()) throw new DOMException('旧聊天已清空', 'AbortError');
    readState.current = { ...readState.current, ...change };
    sessionStorage.setItem(storageKey.current + '.read.v1', JSON.stringify(readState.current));
  }
  const verification = useRef<Promise<false | { complete: boolean; open: string[] }> | undefined>(
    undefined,
  );
  const pendingPayments = useRef(new Set<string>());
  const recoveryEpoch = useRef(0);
  const intentEpoch = useRef(0);
  const initialRestore = useRef<AbortController | null>(null);
  const lifetime = useRef<AbortController | null>(null),
    locked = useRef(false),
    memory = useRef<Memory>(empty()),
    storageKey = useRef(''),
    bottom = useRef<HTMLDivElement>(null),
    stream = useRef<HTMLDivElement>(null),
    nearBottom = useRef(true),
    userScrollUntil = useRef(0),
    finishedTasks = useRef(new Set<string>()),
    activeScope = useRef<string | null>(null),
    activeEpoch = useRef(0),
    ime = useRef(false);
  function save(change: Partial<Memory>) {
    if (!history.current()) throw new DOMException('旧聊天已清空', 'AbortError');
    const next = { ...memory.current, ...change };
    if (!storageKey.current) throw Error('会话尚未核实');
    history.retainTasks(next.tasks);
    sessionStorage.setItem(storageKey.current, JSON.stringify(next));
    memory.current = memorySchema.parse(
      JSON.parse(sessionStorage.getItem(storageKey.current) || 'null'),
    );
    setMessages(memory.current.messages);
  }
  function append(message: Message) {
    save({ messages: [...memory.current.messages, message].slice(-80) });
  }
  function remember(id: string) {
    const tasks = [...new Set([...memory.current.tasks, id])];
    if (tasks.length > 256) throw Error('原号记录已满，请保留当前记录并查询已有原单。');
    save({ active: id, tasks });
  }
  async function openServiceTurn(service: Service, userInitiated = false) {
    const label = service === 'BILLS' ? '账单与反馈' : service === 'CARDS' ? '银行卡' : '订阅管理';
    const panelId = crypto.randomUUID();
    const intent = intentEpoch.current;
    const signal = lifetime.current!.signal;
    const current = () => !signal.aborted && intent === intentEpoch.current;
    const update = (panelState: Message['panelState'], text: string) => {
      if (current())
        save({ messages: memory.current.messages.map((m) => m.panelId === panelId ? { ...m, panelState, text } : m) });
    };
    setActivePanelId(panelId);
    // Only an action taken now may opt into following the new turn. A delayed
    // planner response must preserve any scrolling done while it was pending.
    if (userInitiated) { nearBottom.current = true; userScrollUntil.current = 0; }
    // Persist the anchor before the async read so a slow response cannot move
    // the card to the end of the conversation or attach it to a later turn.
    append({
      role: 'assistant',
      text: `正在读取${label}，请等待实际结果。`,
      panel: service,
      panelId,
      panelState: 'LOADING',
    });
    try {
      await openService(service, signal);
      update('READY', `已打开${label}，请核对下方实际数据并选择操作。`);
    } catch (error) {
      update('FAILED', `${label}读取未完成。${runtimeErrorText(error, { BILLS: 'bills', CARDS: 'cards', SUBSCRIPTIONS: 'subscriptions' }[service] as 'bills' | 'cards' | 'subscriptions')}`);
      throw error;
    }
  }
  // All GET/action/restore paths replace task-scoped display state together.
  // A late callback from an unmounted payment can never update another task.
  function showView(next?: AgentView) {
    const scope = scopeOf(next);
    activeScope.current = scope;
    // Even the same task/plan must re-read its manual payment preparation.
    // Agent GET may legitimately be DRAFT with no embedded payment_flow.
    setReadEpoch(++activeEpoch.current);
    setView(next);
    setProgress(
      scope &&
        (next?.kind === 'DRAFT' || next?.kind === 'PAYMENT') &&
        next.draft.execution_status === 'SUCCEEDED' &&
        !next.draft.intent_termination &&
        !['CANCELLED', 'FAILED'].includes(next.draft.task_state) &&
        next.draft.action !== 'CANCEL_MOCK_DRAFT'
        ? { scope, state: 'CHECKING' }
        : undefined,
    );
  }
  async function accept(
    next: AgentView,
    signal: AbortSignal,
    automatic = true,
    boundTask?: string | null,
  ) {
    if (signal.aborted) return;
    if (next.kind !== 'CLARIFY') {
      remember(viewId(next)!);
      if (scopeOf(next)) {
        history.retainBlockers([viewId(next)!]);
        pendingPayments.current.add(viewId(next)!);
        setOpenTasks([...pendingPayments.current]);
      }
      showView(next);
    } else {
      if (boundTask) {
        const current = await getAgent(boundTask, signal);
        if (signal.aborted) return;
        if (viewId(current) !== boundTask || !scopeOf(current)) throw Error('澄清原任务不匹配');
        remember(boundTask);
        showView(current);
      } else showView(undefined);
      if (next.bank_service) await openServiceTurn(next.bank_service);
    }
    if (automatic && next.kind === 'READ' && next.can_start) {
      const result = await runRead(next, false, signal);
      if (signal.aborted) return;
      showView(result);
      append({ role: 'assistant', text: describe(result), task: viewId(result)! });
      return;
    }
    if (
      automatic &&
      next.kind === 'DRAFT' &&
      next.draft.next_action.kind === 'PREPARE_OR_REPLACE_ACTION'
    ) {
      next = await prepareAgent(next.draft, signal);
      if (signal.aborted) return;
      showView(next);
    }
    if (
      scopeOf(next) &&
      (next.kind === 'DRAFT' || next.kind === 'PAYMENT') &&
      ['CANCELLED', 'FAILED'].includes(next.draft.task_state)
    )
      await verifyPayments(signal);
    append({
      role: 'assistant',
      text: describe(next),
      ...(next.kind !== 'CLARIFY' ? { task: viewId(next)! } : boundTask ? { task: boundTask } : {}),
    });
  }
  async function action(work: (signal: AbortSignal) => Promise<void>) {
    if (locked.current || disabled || !ready) return;
    // Initialization may have made controls ready before its original GET ends.
    // Every accepted user action owns subsequent display/cache/error writes.
    intentEpoch.current++;
    initialRestore.current?.abort();
    locked.current = true;
    setBusy(true);
    setNotice('');
    const signal = lifetime.current!.signal;
    try {
      await run(async () => {
        try {
          await work(signal);
        } catch (e) {
          if (signal.aborted) return;
          if (e instanceof RuntimeApiError && e.taskId) remember(e.taskId);
          setNotice(errorText(e));
          setUncertain(true);
          throw e;
        }
      });
    } finally {
      locked.current = false;
      if (!signal.aborted) setBusy(false);
    }
  }
  async function verifyPayments(
    signal: AbortSignal,
    knownDirectory?: Awaited<ReturnType<typeof recoveryDirectory>>,
    fresh = false,
  ): Promise<false | { complete: boolean; open: string[] }> {
    if (verification.current) {
      const pending = verification.current;
      if (!fresh) return pending;
      await pending;
      if (signal.aborted) return false;
    }
    setScanBusy(true);
    const result = verifyPaymentScan(signal, knownDirectory);
    verification.current = result;
    try {
      return await result;
    } finally {
      if (verification.current === result) {
        verification.current = undefined;
        if (!signal.aborted) setScanBusy(false);
      }
    }
  }
  const [closedDrafts,setClosedDrafts]=useState<string[]>([]);
  async function verifyPaymentScan(
    signal: AbortSignal,
    knownDirectory?: Awaited<ReturnType<typeof recoveryDirectory>>,
  ) {
    const generation = ++recoveryEpoch.current;
    const intent = intentEpoch.current;
    const current = () => !signal.aborted && history.current() && generation === recoveryEpoch.current;
    const canNotify = () => current() && intent === intentEpoch.current;
    history.retainBlockers([...pendingPayments.current]);
    setRecoveryChecked(false);
    setClosedDrafts([]);
    let directory;
    try {
      directory = knownDirectory ?? (await recoveryDirectory(signal));
    } catch (error) {
      if (!current()) return false;
      if (error instanceof RuntimeApiError && error.status === 401) throw error;
      if (canNotify())
        setNotice('本人付款目录读取失败，原号及占用已保留；请重新核对，暂不开始新任务。');
      return false;
    }
    if (!current()) return false;
    save({ tasks: [...new Set([...memory.current.tasks, ...directory.payment_tasks])] });
    // Include previously observed unresolved originals even if a later directory
    // omits one. Only reading every preparation can release its task.
    const open = new Set([...history.originals().blockers, ...pendingPayments.current, ...directory.payment_tasks]);
    history.retainBlockers([...open]);
    const resolved = new Set<string>();
    // A cached uncertain model request can be a completed READ, not a payment.
    // Only a fresh typed read response can release such a local-only blocker.
    for (const id of [...open].filter(id => !directory.payment_tasks.includes(id))) {
      try {
        const original = await getAgent(id, signal);
        if (!current()) return false;
        if (original.kind === 'READ') { open.delete(id); resolved.add(id); }
      } catch (error) {
        if (error instanceof RuntimeApiError && error.status === 401) throw error;
        // Missing/unavailable never proves an unpaid or terminal money task.
      }
    }
    pendingPayments.current = new Set(open);
    setOpenTasks([...open]);
    const verifiedClosed:string[]=[];
    const { failures } = await recoverPaymentTasks([...open], signal, (id, value) => {
      if (!current()) return;
      if(value&&verifiedNonfundingClosure(value,directory.closed_nonfunding_tasks,directory.ended_draft_intents)) {
        open.delete(id);resolved.add(id);verifiedClosed.push(id);
      } else if (value?.terminal) { open.delete(id); resolved.add(id); }
      setOpenTasks([...open]);
    });
    if (!current()) return false;
    for (const error of failures.values())
      if (error instanceof RuntimeApiError && error.status === 401) throw error;
    const complete = failures.size === 0;
    history.releaseBlockers([...resolved]);
    const remaining = new Set([...open, ...history.originals().blockers]);
    pendingPayments.current = remaining;
    setOpenTasks([...remaining]);
    setRecoveryChecked(complete);
    setClosedDrafts(complete?verifiedClosed:[]);
    setCheckedAt(new Date().toLocaleString('zh-CN', { timeZone: 'Asia/Shanghai', hour12: false }));
    if (!complete && canNotify()) setNotice('部分原付款读取失败，原号及占用已保留；其他原号仍可独立查询。');
    return { complete, open: [...remaining] };
  }
  useEffect(() => {
    const c = new AbortController();
    const restore = new AbortController();
    initialRestore.current = restore;
    const intent = ++intentEpoch.current;
    const current = () => !c.signal.aborted && intent === intentEpoch.current;
    lifetime.current = c;
    void (async () => {
      const [s, overview, directory] = await Promise.all([
        agentStatus(c.signal),
        request('/api/bank/overview', 'GET', undefined, c.signal),
        Promise.resolve(initialDirectory),
      ]);
      const data = z
        .object({
          accounts: z.array(
            z
              .object({
                account: z
                  .object({ account_id: z.string(), display_name: z.string() })
                  .passthrough(),
              })
              .passthrough(),
          ),
        })
        .passthrough()
        .parse(overview);
      if (c.signal.aborted || !history.current()) return;
      if (directory.context_ref !== history.context) throw Error('本人聊天范围不匹配');
      storageKey.current = history.key;
      const raw = sessionStorage.getItem(storageKey.current);
      memory.current = raw ? memorySchema.parse(JSON.parse(raw)) : empty();
      if (memory.current.pending && memory.current.active)
        history.retainBlockers([...new Set([...history.originals().blockers, memory.current.active])]);
      saveRead({ conversation_id: crypto.randomUUID(), revision: 0, pending: false });
      // Cached read results are text only; refresh does not treat them as fresh bank facts.
      memory.current.messages = memory.current.messages.map((m) => m.readView
        ? { ...m, readView: undefined, text: m.text + '（历史查询；最新数据请重新查询。）' } : m);
      // Cached text never promises an in-flight read survived a page/session.
      memory.current.messages = memory.current.messages.map((m) => m.panelState === 'LOADING'
        ? { ...m, panelState: 'FAILED', text: '上次服务读取未完成，请重新打开对应服务核对。' }
        : m);
      setOwned(data.accounts.map((a) => a.account));
      setStatus(s);
      save({ ...empty(), payeeSelection: undefined, messages: memory.current.messages,
        tasks: [...new Set([...history.originals().tasks, ...memory.current.tasks, ...directory.payment_tasks])] });
      pendingPayments.current = new Set([...history.originals().blockers, ...directory.payment_tasks]);
      setOpenTasks([...pendingPayments.current]);
      setUncertain(memory.current.pending);
      setNotice('');
      // A mount always starts new model contexts and never selects an old task.
      // The aggregate scan below still owns the independent money safety guard.
      setReady(true);
      await verifyPayments(c.signal, directory);
      if (current()) setReady(true);
    })().catch(() => {
      if (current() && !restore.signal.aborted) {
        setNotice(
          '澄清会话或原任务恢复未完成。可按下方原号查询；若澄清已丢失，请点新任务重新说明，旧原号仍保留。',
        );
        setReady(true);
        setUncertain(true);
      }
    });
    return () => {
      intentEpoch.current++;
      recoveryEpoch.current++;
      restore.abort();
      if (initialRestore.current === restore) initialRestore.current = null;
      readController.current?.abort();
      c.abort();
    };
  }, []);
  useEffect(() => {
    if (nearBottom.current && stream.current)
      stream.current.scrollTo({ top: stream.current.scrollHeight, behavior: 'auto' });
    else if (messages.length) setHasNewResult(true);
  }, [messages.length, busy, view, businessPanel]);
  useEffect(() => {
    const element = stream.current;
    if (!element) return;
    // Collapsing a service card or updating the request status can resize the
    // stream after React's message effect. Follow only if the user stayed near
    // the bottom; reading earlier history never opts back in automatically.
    const observer = new ResizeObserver(() => {
      if (nearBottom.current) element.scrollTo({ top: element.scrollHeight, behavior: 'auto' });
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);
  const selected = owned.some((a) => a.account_id === account)
    ? account
    : owned[0]?.account_id || '';
  const draft = view?.kind === 'DRAFT' || view?.kind === 'PAYMENT' ? view.draft : null;
  useEffect(()=>{
    if(!draft?.intent_expires_at||draft.intent_termination){setDraftSeconds(undefined);return;}
    const c=new AbortController();let running=false;
    setDraftSeconds(Math.max(0,Math.ceil((Date.parse(draft.intent_expires_at)-Date.now())/1000)));
    const timer=setInterval(()=>{
      setDraftSeconds(Math.max(0,Math.ceil((Date.parse(draft.intent_expires_at!)-Date.now())/1000)));
      if(running||Date.now()<Date.parse(draft.intent_expires_at!))return;
      running=true;
      void verifyPayments(c.signal,undefined,true).then(async()=>{
        const next=await getAgent(draft.task_id,c.signal);if(!c.signal.aborted)showView(next);
      }).catch(()=>{running=false;});
    },1000);
    return ()=>{clearInterval(timer);c.abort();};
  },[draft?.task_id,draft?.intent_expires_at,!!draft?.intent_termination]);
  const currentProgress = progress?.scope === scopeOf(view) ? progress.state : undefined;
  const settled = currentProgress === 'SUCCEEDED' || currentProgress === 'FAILED';
  const activeClosed=!!draft&&recoveryChecked&&closedDrafts.includes(draft.task_id);
  const draftLive=!!draft&&!activeClosed&&!draft.intent_termination&&!['CANCELLED','FAILED'].includes(draft.task_state);
  const submitted =
    (draft?.execution_status === 'RESULT_UNKNOWN'&&!activeClosed) ||
    (!!currentProgress &&
      !settled &&
      !['UNSUBMITTED', 'REGISTERED_UNSUBMITTED'].includes(currentProgress));
  const latestPanelIndex = messages.reduce(
    (latest, message, index) => (message.panel ? index : latest),
    -1,
  );
  const isOriginalQuery = (value: string) =>
    /^(重新读取原任务|重新查询原任务|查询原任务|查询原单|查询办理进度)[。！!？?]?$/.test(
      value.trim(),
    );
  const isBalanceQuery = (value: string) =>
    /^(查一下我的余额|查询余额|查看余额|查余额)[。！!？?]?$/.test(value.trim());
  async function rereadOriginal(id: string, signal: AbortSignal) {
    const next = await getAgent(id, signal);
    if (signal.aborted) return;
    if (viewId(next) !== id) throw Error('查询返回的办理编号不匹配');
    remember(id);
    save({ conversation: crypto.randomUUID(), revision: 0, pending: false });
    showView(next);
    append({ role: 'assistant', text: describe(next), task: id });
    // Display the selected result immediately. The independent aggregate still
    // controls new payments, even if a different original is slow/unavailable.
    setUncertain(false);
    setNotice('已按原编号读取实际状态；正在单独核对全部原付款，可继续独立查询。旧确认和验证码不会恢复。');
    backgroundScan(signal);
  }
  function backgroundScan(signal = lifetime.current!.signal) {
    const intent = intentEpoch.current;
    // The scan keeps ownership of the write guard, not the ordinary read controls.
    void verifyPayments(signal).catch((e) => {
      if (!signal.aborted && intent === intentEpoch.current && !(e instanceof RuntimeApiError && e.status === 401))
        setNotice('原付款核查未完成，原号及占用已保留；可继续独立查询。');
    });
  }
  async function readBalance(signal: AbortSignal) {
    const result = z
      .object({
        accounts: z.array(
          z
            .object({
              account: z
                .object({ display_name: z.string(), masked_account_no: z.string() })
                .passthrough(),
              balance: z
                .object({ available_balance: z.string().regex(/^-?\d+\.\d{2}$/) })
                .passthrough(),
            })
            .passthrough(),
        ),
      })
      .passthrough()
      .parse(await request('/api/bank/overview', 'GET', undefined, signal));
    if (!signal.aborted)
      append({
        role: 'assistant',
        text:
          result.accounts
            .map(
              (v) =>
                `${v.account.display_name}（${v.account.masked_account_no}）可用余额 ¥${v.balance.available_balance}`,
            )
            .join('；') + '。本次独立查询不经过模型，不改变待核付款状态。',
      });
  }
  const moneyBlocked = uncertain || submitted || !recoveryChecked || openTasks.some((id) => id !== draft?.task_id);
  async function independentQuery(message?: string) {
    if (readController.current || disabled || busy || !ready) return;
    const c = new AbortController();
    const life = lifetime.current!.signal;
    const cancel = () => c.abort();
    life.addEventListener('abort', cancel, { once: true });
    readController.current = c;
    const id = readState.current.conversation_id;
    const revision = readState.current.revision;
    const turn = crypto.randomUUID();
    activeReadTurn.current = turn;
    const current = () => !c.signal.aborted && readController.current === c && readState.current.conversation_id === id;
    setReadBusy(true);
    setReadMode(true);
    const update = (text: string, readView?: AgentView) => {
      if (current()) save({ messages: memory.current.messages.map((m) => m.readTurn === turn ? { ...m, text, readView } : m) });
    };
    try {
      if (message && readState.current.pending) {
        setReadNotice('上次查询尚未核实，请读取查询结果，或点击新查询后重新说明；没有自动重发。');
        return;
      }
      if (message) {
        intentEpoch.current++;
        initialRestore.current?.abort();
        nearBottom.current = true;
        userScrollUntil.current = 0;
        append({ role: 'user', text: message });
        setText('');
        saveRead({ pending: true });
      }
      append({ role: 'assistant', readTurn: turn, text: message ? '正在理解查询，并读取你的银行数据…' : '正在读取上次查询的实际结果…' });
      const raw = await request('/api/bank/read-conversation' + (message ? '' : '/' + id), message ? 'POST' : 'GET',
        message ? { conversation_id: id, revision, message, account_id: selected || null } : undefined, c.signal, message ? 250000 : 35000);
      const response = readResponseSchema.parse(raw);
      if (!current()) return;
      if (response.conversation_id !== id || (message && response.revision !== revision + 1)) throw Error('查询轮次不匹配');
      saveRead({ revision: response.revision, pending: response.uncertain });
      if (!response.view || response.uncertain) {
        update('本次查询尚未取得完整结果。可稍后读取，或点击新查询重新说明；原付款未改变。');
      } else if (response.view.kind === 'CLARIFY') {
        update(response.view.clarification.question + ' 本查询不会办理转账、冻结或保存反馈；原付款请按原号核对。');
      } else {
        update(describe(response.view), response.view);
      }
      setReadNotice('');
    } catch (e) {
      if (!current()) return;
      const detail = e instanceof RuntimeApiError && e.status === 422
        ? '本次请求未通过只读查询检查，未办理任何业务。请只提出余额、账单等查询；付款请核对原单。'
        : '本次查询未完成。' + errorText(e);
      update(detail);
      setReadNotice(detail + ' 可读取已有查询结果，或点击新查询；不会自动重发。');
    } finally {
      life.removeEventListener('abort', cancel);
      if (readController.current === c) {
        readController.current = null;
        if (!life.aborted) setReadBusy(false);
      }
    }
  }
  function cancelRead() {
    readController.current?.abort();
    readController.current = null;
    if (activeReadTurn.current && storageKey.current)
      save({ messages: memory.current.messages.map((m) => m.readTurn === activeReadTurn.current && !m.readView && /^正在/.test(m.text)
        ? { ...m, text: '已停止等待本次查询，可单独读取已有结果；原付款保持。' } : m) });
    activeReadTurn.current = '';
    setReadBusy(false);
    setReadNotice('已停止等待本次查询；可单独读取结果。原付款与安全核查保持。');
  }
  function newRead() {
    cancelRead();
    saveRead({ conversation_id: crypto.randomUUID(), revision: 0, pending: false });
    setReadMode(true);
    setReadNotice('已开始新的独立查询，请说明要查什么；原付款保持。');
  }
  async function send(override?: string, chosen?: { query: string; value: Payee }) {
    const message = (override ?? text).trim();
    if (isOriginalQuery(message) || isBalanceQuery(message)) {
      await action(async (signal) => {
        append({ role: 'user', text: message });
        setText('');
        if (isBalanceQuery(message)) await readBalance(signal);
        else if (memory.current.active) await rereadOriginal(memory.current.active, signal);
        else setNotice('尚无选中的办理任务，请从历史办理记录选择要查询的任务。');
      });
      return;
    }
    if (!chosen && message && (readMode || moneyBlocked)) {
      await independentQuery(message);
      return;
    }
    const selectedPayee = chosen ?? payee;
    if (
      !message ||
      uncertain ||
      submitted ||
      !recoveryChecked ||
      openTasks.some((id) => id !== draft?.task_id)
    )
      return;
    await action(async (signal) => {
      // Recheck at the action boundary: another login/tab may have created an
      // unresolved payment since this conversation was mounted. An existing
      // draft may still be clarified/cancelled within its original task.
      const verified = await verifyPayments(signal, undefined, true);
      const bound = draft && !settled ? draft.task_id : undefined;
      if (
        !verified ||
        !verified.complete ||
        verified.open.some((id) => id !== bound) ||
        signal.aborted
      ) {
        if (!signal.aborted) setNotice('原付款尚未全部核清，未发送新请求；请先按原号查询。');
        return;
      }
      if (memory.current.tasks.length >= 254)
        throw Error('原号记录容量已满。请查询已有原单，不能再创建请求。');
      const original =
        (draft && !settled ? draft.task_id : null) ??
        `bank_chat_${memory.current.conversation}_${memory.current.revision + 1}`;
      remember(original);
      save({
        pending: true,
        ...(selectedPayee
          ? { payeeSelection: { query: selectedPayee.query, id: selectedPayee.value.payee_id } }
          : {}),
      });
      setUncertain(true);
      nearBottom.current = true;
      userScrollUntil.current = 0;
      if (activePanelId) setCollapsedPanels((previous) => new Set([...previous, activePanelId]));
      append({ role: 'user', text: message });
      setText('');
      setNotice('正在理解你的请求…');
      const context =
        draft && !submitted && !settled
          ? {
              task_id: draft.task_id,
              task_revision: draft.task_revision,
              plan_version: draft.plan_version,
              facts_version: draft.facts_version,
              selection_ticket: draft.selection_ticket,
            }
          : null;
      const response = responseSchema.parse(
        await request(
          '/api/bank/conversation',
          'POST',
          {
            conversation_id: memory.current.conversation,
            revision: memory.current.revision,
            message,
            selection: {
              account_id: selected || null,
              payee_id: selectedPayee?.value.payee_id ?? null,
              payee_query: selectedPayee?.query ?? null,
            },
            context,
          },
          signal,
          250000,
        ),
      );
      if (signal.aborted) return;
      if (
        response.conversation_id !== memory.current.conversation ||
        response.revision !== memory.current.revision + 1 ||
        !response.view
      )
        throw Error('对话轮次不匹配');
      save({ revision: response.revision, pending: false });
      setUncertain(false);
      setNotice('');
      if (response.payee_query) setCandidateQuery(response.payee_query);
      if (response.view.kind === 'CLARIFY' && !response.task_id) save({ active: '' });
      await accept(response.view, signal, true, response.task_id);
    });
  }
  async function newTask() {
    if (busy || disabled || !recoveryChecked || uncertain || submitted || openTasks.length) return;
    await action(async (signal) => {
      const verified = await verifyPayments(signal, undefined, true);
      if (!verified || !verified.complete || verified.open.length || signal.aborted) {
        if (!signal.aborted)
          setNotice('原付款尚未全部核清，已保留原号；请先查询，暂未开始新任务。');
        return;
      }
      setActivePanelId(undefined);
      setReadMode(false);
      save({
        active: '',
        conversation: crypto.randomUUID(),
        revision: 0,
        pending: false,
        payeeSelection: undefined,
      });
      showView(undefined);
      setUncertain(false);
      setPayee(undefined);
      setNotice('');
      append({ role: 'assistant', text: '已开始新任务；历史原单仍可在记录中查询。' });
    });
  }
  function outcome(result: PaymentFunds, completedAt?: string) {
    const scope = scopeOf(view);
    if (
      !draft ||
      !scope ||
      activeScope.current !== scope ||
      activeEpoch.current !== readEpoch ||
      result.task_id !== draft.task_id ||
      lifetime.current?.signal.aborted
    )
      return;
    if (['UNSUBMITTED', 'REGISTERED_UNSUBMITTED'].includes(result.state)) return;
    // A child reports ONE preparation/execution, never the whole task. Hold
    // the task until an independent aggregate read verifies every preparation.
    recoveryEpoch.current++;
    pendingPayments.current.add(result.task_id);
    history.retainBlockers([...pendingPayments.current]);
    setOpenTasks([...pendingPayments.current]);
    setRecoveryChecked(false);
    const terminal = result.state === 'SUCCEEDED' || result.state === 'FAILED';
    const person =
      payee?.value.payee_id === result.transfer.payee_id
        ? `${payee.value.display_name}（${payee.value.masked_account_no}）`
        : '本笔已核对收款人';
    const time = completedAt
      ? `完成时间：${new Date(completedAt).toLocaleString('zh-CN', { timeZone: 'Asia/Shanghai', hour12: false })}（北京时间）。`
      : '原回执完成时间暂未取得。';
    const summary =
      result.state === 'SUCCEEDED'
        ? `模拟转账完成：¥${result.transfer.amount}，收款人${person}。${time}本条资金结果已按原号核实，审计 ACK 需另查；其他未结原单仍须核对。`
        : result.state === 'FAILED'
          ? `模拟转账未完成：¥${result.transfer.amount}，收款人${person}。本条原单状态为失败，未自动重新付款；其他未结原单仍须核对。`
          : result.state === 'PROCESSING'
            ? `已受理 ¥${result.transfer.amount} 模拟付款，正在按原单查询后台结果；尚未确认到账，请勿重复提交。`
            : `¥${result.transfer.amount} 原单结果待核对；保留原号，只查询，不自动重新付款。`;
    const key = [result.task_id, result.execution_operation_id, result.state].join(':');
    const messages = [...memory.current.messages];
    const index = messages.findIndex((m) => m.outcome === key);
    const message: Message = {
      role: 'assistant',
      text: summary,
      task: result.task_id,
      outcome: key,
    };
    if (index < 0) messages.push(message);
    else messages[index] = message;
    save({ messages: messages.slice(-80) });
    setProgress({ scope, state: result.state });
    if (!memory.current.pending) setUncertain(false);
    if (terminal) {
      const generation = activeEpoch.current;
      const signal = lifetime.current!.signal;
      const conversation = memory.current.conversation;
      const revision = memory.current.revision;
      void verifyPayments(signal, undefined, true)
        .then(() => {
          if (
            signal.aborted ||
            activeScope.current !== scope ||
            activeEpoch.current !== generation ||
            memory.current.pending ||
            memory.current.conversation !== conversation ||
            memory.current.revision !== revision ||
            pendingPayments.current.has(result.task_id) ||
            finishedTasks.current.has(result.task_id)
          )
            return;
          finishedTasks.current.add(result.task_id);
          save({ conversation: crypto.randomUUID(), revision: 0, pending: false });
        })
        .catch(() => {
          if (!signal.aborted) setNotice('原付款汇总尚未核实，原号及占用已保留，请重新查询。');
        });
    } else finishedTasks.current.delete(result.task_id);
  }
  const block = disabled || busy || !ready;
  const readMessageIndex = messages.reduce(
    (last, m, i) => (view?.kind === 'READ' && m.task === view.task.task_id ? i : last),
    -1,
  );
  const readPanel = view?.kind === 'READ' && (
    <div className="chat-business-card">
      <h3>{view.task.task_state === 'COMPLETED' ? '查询结果' : '原查询状态'}</h3>
      {view.results?.map((r) =>
        r.tool_name === 'find_payees' ? (
          <div className="chat-payees" key={r.operation_id}>
            <p>请选择已登记收款人，再继续原转账请求。同名时请核对银行和尾号。</p>
            {!r.data.candidates.length && <p>未找到匹配收款人，请补充姓名。</p>}
            {r.data.candidates.map((p) => (
              <button
                key={p.payee_id}
                disabled={block || !candidateQuery}
                onClick={() => {
                  const chosen = { query: candidateQuery, value: p };
                  setPayee(chosen);
                  void send('使用我选择的已登记收款人，继续刚才的转账请求。', chosen);
                }}
              >
                {p.display_name} · {p.bank_name} · {p.masked_account_no}
              </button>
            ))}
          </div>
        ) : (
          <ResultRows key={r.operation_id} result={r} />
        ),
      )}
      {view.task.next_action.kind === 'CONTINUE' && (
        <button
          disabled={block}
          onClick={() =>
            void action(async (signal) => accept(await runRead(view, true, signal), signal, false))
          }
        >
          继续原查询
        </button>
      )}
    </div>
  );
  return (
    <section className="bank-chat" aria-label="银行智能助手">
      <header className="chat-header">
        <div>
          <h2>银行智能助手</h2>
          <p>
            <ShieldCheck size={14} /> 虚构银行演示 · 办理前由你确认
          </p>
        </div>
        <button
          onClick={() => void newTask()}
          disabled={block || !recoveryChecked || uncertain || submitted || openTasks.length > 0}
        >
          <Plus size={16} />
          新任务
        </button>
      </header>
        <div className="chat-service-actions">
          <button disabled={block} title="直接读取银行余额，不经过模型" onClick={() => void action(readBalance)}>
            查询余额
          </button>
          {(
            [
              ['BILLS', '账单与反馈'],
              ['CARDS', '银行卡'],
              ['SUBSCRIPTIONS', '订阅管理'],
            ] as const
          ).map(([s, label]) => (
            <button
              key={s}
              disabled={block}
              onClick={() => void action(() => openServiceTurn(s, true))}
            >
              {label}
            </button>
          ))}
        </div>
      <div
        className="chat-stream"
        ref={stream}
        tabIndex={0}
        onWheel={() => { userScrollUntil.current = performance.now() + 1500; }}
        onTouchMove={() => { userScrollUntil.current = performance.now() + 1500; }}
        onPointerDown={() => { userScrollUntil.current = performance.now() + 1500; }}
        onKeyDown={(event) => {
          if (['ArrowUp', 'ArrowDown', 'PageUp', 'PageDown', 'Home', 'End', ' '].includes(event.key))
            userScrollUntil.current = performance.now() + 1500;
        }}
        onScroll={() => {
          const el = stream.current;
          // Layout and automatic following also emit scroll events. Only an
          // actual wheel/touch/scrollbar/keyboard gesture changes the user's
          // choice to read history; sending a new message resets that intent.
          if (el && performance.now() <= userScrollUntil.current)
            nearBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 100;
        }}
        role="log"
        aria-label="连续对话记录"
        aria-live="polite"
      >
        <details className="chat-recovery-controls">
          <summary>原付款核查 · 待核实或未结束 {openTasks.length} 笔</summary>
        {(notice || busy) && (
          <p role="status" className="chat-status">
            {busy ? '正在处理，请稍候… ' : ''}
            {notice}
          </p>
        )}
        {uncertain && memory.current.active && (
          <button
            disabled={block}
            onClick={() =>
              void action(async (signal) => {
                await rereadOriginal(memory.current.active, signal);
              })
            }
          >
            只读恢复原任务
          </button>
        )}
        {(!recoveryChecked || openTasks.length > 0) && (
          <p role="status">有待核对或未结束的原付款，核清前不能新建付款任务。</p>
        )}
        {checkedAt && (
          <p className="chat-recovery-summary">
            上次核查：{checkedAt}（北京时间）；待核实或未结束 {openTasks.length}{' '}
            笔。可独立查询余额、账单、卡片和订阅。
          </p>
        )}
        <button
          disabled={block}
          onClick={() => backgroundScan()}
        >
          {scanBusy ? '正在核对全部原付款…' : '重新核对全部原付款'}
        </button>
        {!!memory.current.tasks.length && (
          <details className="chat-originals">
            <summary>全部办理原号（包括未核实请求）</summary>
            {memory.current.tasks.map((id) => (
              <button
                key={id}
                disabled={block}
                onClick={() =>
                  void action(async (signal) => {
                    await rereadOriginal(id, signal);
                  })
                }
              >
                查询 {id}
              </button>
            ))}
          </details>
        )}
        </details>
        {!messages.length && (
          <div className="chat-welcome">
            <h3>今天想办理什么？</h3>
            <p>可以查余额、准备转账，也可以核对账单、卡片和订阅。</p>
            <div className="chat-examples">
              {['查一下我的余额', '给已登记收款人转100元', '查看月度账单'].map((t) => (
                <button key={t} disabled={block} onClick={() => setText(t)}>
                  {t}
                </button>
              ))}
            </div>
          </div>
        )}
        {messages.map((m, i) => (
          <div className="chat-turn" key={m.panelId ?? `${i}-${m.role}`}>
            <article className={'chat-message ' + m.role}>
              <span className="chat-role">{m.role === 'user' ? '你' : '银行助手'}</span>
              <p>{m.text}</p>
              {m.task && (
                <details>
                  <summary>查看原任务</summary>
                  <button
                    disabled={block}
                    onClick={() =>
                      void action(async (signal) => {
                        await rereadOriginal(m.task!, signal);
                      })
                    }
                  >
                    按此原号只读查询
                  </button>
                  <small>{m.task}</small>
                </details>
              )}
            </article>
            {m.readView?.kind === 'READ' && (
              <div className="chat-business-card" aria-label="独立查询结果">
                <h3>本次查询明细</h3>
                {m.readView.results?.map((r) => <ResultRows key={r.operation_id} result={r} />)}
                {m.readView.task.task_state !== 'COMPLETED' && <p>本次查询尚未完成；可读取已有结果，或开启新查询。没有自动重新办理。</p>}
              </div>
            )}
            {i === readMessageIndex && readPanel}
            {m.panel && (
              <div className="chat-service-turn" data-service={m.panel}>
                {i === latestPanelIndex &&
                m.panelId === activePanelId &&
                businessPanel &&
                businessPanelService === m.panel ? (
                  <details
                    className="chat-service-card"
                    open={!collapsedPanels.has(m.panelId!)}
                    onToggle={(event) => {
                      const open = event.currentTarget.open;
                      setCollapsedPanels((previous) => {
                        const next = new Set(previous);
                        if (open) next.delete(m.panelId!);
                        else next.add(m.panelId!);
                        return next;
                      });
                    }}
                  >
                    <summary>
                      {m.panel === 'SUBSCRIPTIONS'
                        ? '订阅管理卡片'
                        : m.panel === 'BILLS'
                          ? '账单与反馈卡片'
                          : '银行卡卡片'}
                      <span>（点击收起或展开）</span>
                    </summary>
                    <div className="chat-service-panel">{businessPanel}</div>
                  </details>
                ) : (
                  <p className="chat-service-placeholder">
                    {m.panelState === 'LOADING'
                      ? '正在读取服务数据，尚未取得可办理结果。'
                      : m.panelState === 'FAILED'
                        ? '本次未取得可办理数据，请重新打开对应服务；已有原号与结果保留。'
                        : `这张${m.panel === 'SUBSCRIPTIONS' ? '订阅' : m.panel === 'BILLS' ? '账单' : '银行卡'}卡片已归档；如需办理，请重新打开对应服务。`}
                  </p>
                )}
              </div>
            )}
          </div>
        ))}
        {readMessageIndex < 0 && readPanel}
        {draft && !settled && (
          <section className="chat-business-card" aria-label="转账草案">
            <h3>{draft.task_state === 'CANCELLED' ? '草案已取消' : draft.intent_termination || activeClosed ? '草案已结束' : '核对转账草案'}</h3>
            <strong className="chat-amount">
              ¥{String(draft.target_parameters?.amount ?? draft.parameters.amount ?? '待补充')}
            </strong>
            <dl className="chat-facts">
              <dt>付款账户</dt>
              <dd>
                {owned.find(
                  (a) =>
                    a.account_id ===
                    (draft.parameters.payer_account_id ??
                      draft.target_parameters?.payer_account_id),
                )?.display_name ?? '按原账户核对'}
              </dd>
              <dt>收款人</dt>
              <dd>
                {payee &&
                payee.value.payee_id ===
                  (draft.target_parameters?.payee_id ?? draft.parameters.payee_id)
                  ? `${payee.value.display_name} · ${payee.value.bank_name} · ${payee.value.masked_account_no}`
                  : '收款信息以付款确认卡的可信详情为准'}
              </dd>
              {draft.confirmation_recipient && (
                <>
                  <dt>确认收款人</dt>
                  <dd>
                    {draft.confirmation_recipient.display_name} ·{' '}
                    {draft.confirmation_recipient.bank_name}
                  </dd>
                  <dt>模拟卡号</dt>
                  <dd className="chat-simulation-card-number">
                    {draft.confirmation_recipient.simulation_card_number}
                  </dd>
                </>
              )}
              <dt>备注</dt>
              <dd>{String(draft.target_parameters?.remark ?? draft.parameters.remark ?? '无')}</dd>
            </dl>
            <p>
              {draft.intent_termination || activeClosed
                ? '草案已结束，旧确认不能继续使用；原付款仍按原号独立核查。'
                : draft.task_state === 'CANCELLED'
                ? '没有提交此草案付款。'
                : draft.action === 'CANCEL_MOCK_DRAFT'
                  ? '确认后将取消尚未提交的草案。'
                  : '修改金额或收款人后必须重新核对，旧确认不能继续使用。'}
            </p>
            {draft.intent_expires_at&&!draft.intent_termination&&<p>草案有效至 {new Date(draft.intent_expires_at).toLocaleString('zh-CN')}；到期以服务器核验为准。</p>}
            {draftSeconds!==undefined&&!draft.intent_termination&&<p role="status">{draftSeconds>0?`按本机时间估计剩余 ${draftSeconds} 秒`:'正在按原号核查是否已到期'}；实际期限由服务器核验。</p>}
            {draft.intent_termination&&<p role="status">{draft.intent_termination.reason==='EXPIRED'?'未付款草案已过期':'旧意图已结束，可以改办新转账'}。原记录保留；这不会撤销已提交的付款。</p>}
            {draftLive&&['CREATE_MOCK_DRAFT','MODIFY_MOCK_DRAFT'].includes(draft.action)&&draft.execution_status!=='RESULT_UNKNOWN'&&!submitted&&(<>
              <button disabled={block||uncertain} onClick={()=>void action(async signal=>{
                intentTerminationSchema.parse(await request(`/api/sim/draft-lifecycles/${encodeURIComponent(draft.task_id)}/operations/${encodeURIComponent(draft.operation_id)}/end-intent`,'POST',{
                  contract_version:2,scope:'MOCK_DRAFT_LIFECYCLE_ONLY',acknowledgement:'END_THIS_PROVEN_UNPAID_INTENT_ONLY',task_revision:draft.task_revision,operation_revision:draft.operation_revision,plan_version:draft.plan_version,facts_version:draft.facts_version,
                },signal));
                const next=await getAgent(draft.task_id,signal);if(!signal.aborted)showView(next);await verifyPayments(signal,undefined,true);
              })}>放弃这份未付款草案</button>
              {!draft.intent_expires_at&&<p>这份草案尚未生成确认期限；可明确放弃。多份旧草案请从办理记录逐条核对处理，未核清的付款仍保留。</p>}
              <label>改办金额（元，保留两位小数；沿用本草案的付款账户和收款人）<input aria-label="改办转账金额" inputMode="decimal" value={replacementAmount} onChange={e=>setReplacementAmount(e.target.value)} disabled={block}/></label>
              <button disabled={block||uncertain} onClick={()=>void action(async signal=>{
                const replacement=replacementIntent(draft,replacementAmount);
                const ended=intentTerminationSchema.parse(await request(`/api/sim/draft-lifecycles/${encodeURIComponent(draft.task_id)}/operations/${encodeURIComponent(draft.operation_id)}/replace-intent`,'POST',{
                  contract_version:2,scope:'MOCK_DRAFT_LIFECYCLE_ONLY',acknowledgement:'REPLACE_UNEXECUTED_INTENT_ONLY',task_revision:draft.task_revision,operation_revision:draft.operation_revision,plan_version:draft.plan_version,facts_version:draft.facts_version,
                  replacement,
                },signal));
                if(!ended.replacement||ended.replacement.task_id!==replacement.task_id)throw Error('替代任务原号不匹配');
                remember(replacement.task_id);
                const next=await getAgent(replacement.task_id,signal);if(!signal.aborted)showView(next);
                await verifyPayments(signal,undefined,true);if(!signal.aborted){setReplacementAmount('');setNotice('旧意图已结束，新转账草案已生成。请核对新金额、账户及收款人，并重新确认；尚未付款。');}
              })}>放弃此未付款草案，改办新转账</button>
            </>)}
            {draft.execution_status === 'RESULT_UNKNOWN' && (
              <p role="status">
                查询已成功，但原操作结果仍未知。
                {draft.action === 'CREATE_MOCK_DRAFT'
                  ? '未知发生在创建模拟草稿阶段，尚无可核对草稿；这不代表付款已提交或已失败。'
                  : '原业务结果尚未核清。'}
                {activeClosed?'已明确关闭这份非资金草案；未知原件仍保留。可点击新任务，另行核对和确认新的办理。':'请保留原号；重复查询不等于恢复。资金结果未知时仍不可重办。只有证据完整、确认已过期的初始非资金草案可明确关闭。'}
              </p>
            )}
            {draftLive&&draft.execution_status==='RESULT_UNKNOWN'&&draft.action==='CREATE_MOCK_DRAFT'&&(
              <button disabled={block} onClick={()=>void action(async signal=>{
                const value=z.object({contract_version:z.literal(1),task_id:z.literal(draft.task_id),operation_id:z.literal(draft.operation_id),
                  closed_nonfunding_draft:z.literal(true),original_result:z.literal('UNKNOWN'),original_preserved:z.literal(true),funding_authorized:z.literal(false),automatic_retry:z.literal(false)}).strict().parse(
                  await request(`/api/sim/draft-lifecycles/${encodeURIComponent(draft.task_id)}/operations/${encodeURIComponent(draft.operation_id)}/close-nonfunding`,'POST',{
                    contract_version:2,scope:'MOCK_DRAFT_LIFECYCLE_ONLY',acknowledgement:'PRESERVE_UNKNOWN_CLOSE_NONFUNDING_DRAFT',
                    task_revision:draft.task_revision,operation_revision:draft.operation_revision,plan_version:draft.plan_version,facts_version:draft.facts_version,
                  },signal));
                if(signal.aborted||value.task_id!==draft.task_id)return;
                await verifyPayments(signal,undefined,true);
                if(!signal.aborted){setUncertain(false);setNotice('关闭注记已保存，原 UNKNOWN 未改。须全部原单核对完成后，再明确点击新任务。');}
              })}>保留未知原件，关闭这份非资金草案</button>
            )}
            {draftLive&&draft.execution_status === 'SUCCEEDED' &&
              draft.next_action.kind === 'SELECT_ACTION' &&
              draft.action !== 'CANCEL_MOCK_DRAFT' &&
              !submitted && (
                <button
                  disabled={block || uncertain}
                  onClick={() =>
                    void action(async (signal) => {
                      const cancel = await selectSavedDraftCancellation(draft, signal);
                      await accept(await prepareAgent(cancel, signal), signal, false);
                    })
                  }
                >
                  核对并准备取消未付款草案
                </button>
              )}
            {draftLive&&draft.next_action.kind === 'RECONFIRM_RECIPIENT' && (
              <>
                <p>收款资料已有新版本；旧草案及确认保留，必须重新核对后才能继续。</p>
                <button
                  disabled={block || uncertain || submitted}
                  onClick={() =>
                    void action(async (signal) => {
                      const refreshed = await refreshRecipient(draft, signal);
                      if (!signal.aborted)
                        await accept(await prepareAgent(refreshed, signal), signal, false);
                    })
                  }
                >
                  更新收款资料并重新核对
                </button>
              </>
            )}
            {draftLive&&draft.next_action.kind === 'CONFIRM' && !submitted && (
              <button
                className="primary"
                disabled={
                  block ||
                  uncertain ||
                  (draft.action !== 'CANCEL_MOCK_DRAFT' &&
                    (!payee ||
                      payee.value.payee_id !==
                        (draft.target_parameters?.payee_id ?? draft.parameters.payee_id)))
                }
                onClick={() =>
                  void action(async (signal) => {
                    const confirmed = await actDraft(draft, 'CONFIRM', signal);
                    if (signal.aborted) return;
                    const next =
                      confirmed.kind === 'DRAFT' && confirmed.draft.next_action.kind === 'EXECUTE'
                        ? await actDraft(confirmed.draft, 'EXECUTE', signal)
                        : confirmed;
                    await accept(next, signal, false);
                  })
                }
              >
                {draft.action === 'CANCEL_MOCK_DRAFT' ? '确认取消草案' : '确认并保存此草案'}
              </button>
            )}
            {draftLive&&draft.next_action.kind === 'EXECUTE' && !submitted && (
              <button
                disabled={block || uncertain}
                onClick={() =>
                  void action(async (signal) =>
                    accept(await actDraft(draft, 'EXECUTE', signal), signal, false),
                  )
                }
              >
                继续保存已确认草案
              </button>
            )}
            {draftLive&&draft.next_action.kind === 'PREPARE_OR_REPLACE_ACTION' && !submitted && (
              <button
                disabled={block || uncertain}
                onClick={() =>
                  void action(async (signal) =>
                    accept(await prepareAgent(draft, signal), signal, false),
                  )
                }
              >
                重新核对草案
              </button>
            )}
            <details>
              <summary>草案状态及版本</summary>
              <p>
                {draft.task_id} · 版本 {draft.plan_version} · {draft.execution_status}
              </p>
            </details>
          </section>
        )}
        {draftLive&&draft?.execution_status === 'SUCCEEDED' &&
          draft.task_state !== 'CANCELLED' &&
          draft.action !== 'CANCEL_MOCK_DRAFT' && (
            <ChatPayment
              key={draft.task_id + ':' + draft.plan_version + ':' + readEpoch}
              draft={draft}
              disabled={block || uncertain}
              onBusy={action}
              onSubmitted={() => {
                const scope = scopeOf(view);
                if (
                  scope &&
                  activeScope.current === scope &&
                  activeEpoch.current === readEpoch &&
                  !finishedTasks.current.has(draft.task_id)
                )
                  setProgress({ scope, state: 'SUBMITTED' });
              }}
              onVerified={(state) => {
                const scope = scopeOf(view);
                if (scope && activeScope.current === scope && activeEpoch.current === readEpoch)
                  setProgress({ scope, state });
              }}
              onOutcome={outcome}
              onUncertain={() =>
                append({
                  role: 'assistant',
                  text: '本笔付款提交或查询响应未核实，结果未知。保留原号，只读查询；没有自动重新付款。',
                  task: draft.task_id,
                })
              }
            />
          )}
        <div ref={bottom} />
      </div>
      {hasNewResult && (
        <button
          className="chat-latest"
          onClick={() => {
            nearBottom.current = true;
            setHasNewResult(false);
            stream.current?.scrollTo({ top: stream.current.scrollHeight, behavior: 'smooth' });
          }}
        >
          查看最新结果 ↓
        </button>
      )}
      <div className="chat-composer">
        <details>
          <summary>办理账户与已登记收款人</summary>
          <label>
            本人账户
            <select
              value={selected}
              disabled={block || !!draft}
              onChange={(e) => setAccount(e.target.value)}
            >
              {owned.map((a) => (
                <option key={a.account_id} value={a.account_id}>
                  {a.display_name}
                </option>
              ))}
            </select>
          </label>
          <label>
            查找已登记收款人
            <input value={query} disabled={block} onChange={(e) => setQuery(e.target.value)} />
          </label>
          <button
            disabled={block || !query.trim()}
            onClick={() =>
              void action(async (signal) => {
                const candidates = await searchPayees(query.trim(), signal);
                if (!signal.aborted) {
                  setPayees(candidates.candidates);
                  setCandidateQuery(query.trim());
                  setPayee(undefined);
                }
              })
            }
          >
            查询收款人
          </button>
          {payees.map((p) => (
            <button
              key={p.payee_id}
              disabled={block}
              aria-pressed={payee?.value.payee_id === p.payee_id}
              onClick={() => {
                setPayee({ query: candidateQuery, value: p });
                save({ payeeSelection: { query: candidateQuery, id: p.payee_id } });
              }}
            >
              {p.display_name} · {p.bank_name} · {p.masked_account_no}
            </button>
          ))}
          {payee && (
            <p>
              已选：{payee.value.display_name} · {payee.value.masked_account_no}
            </p>
          )}
        </details>
        <div className="chat-read-controls" aria-label="独立查询控制">
          <button disabled={block || readBusy} aria-pressed={readMode || moneyBlocked} onClick={() => setReadMode(true)}>独立查询</button>
          <button disabled={block || readBusy || moneyBlocked} aria-pressed={!readMode && !moneyBlocked} onClick={() => setReadMode(false)}>继续业务办理</button>
          <button disabled={block || readBusy || !readState.current.revision && !readState.current.pending} onClick={() => void independentQuery()}>读取查询结果</button>
          <button disabled={block} onClick={newRead}>新查询</button>
          {readBusy && <button onClick={cancelRead}>停止等待查询</button>}
          <p role="status">{readBusy ? '正在独立查询银行数据…' : readNotice || ((readMode || moneyBlocked) ? '当前可独立查询；原付款、草案与确认保持，尚未核清的原单仍阻止新付款。' : '办理业务前仍须核对原单，并单独确认和验证。')}</p>
        </div>
        <form
          onSubmit={(e) => {
            e.preventDefault();
            void send();
          }}
        >
          <label className="sr-only" htmlFor="bank-chat-input">
            向银行助手发送消息
          </label>
          <textarea
            id="bank-chat-input"
            value={text}
            maxLength={2400}
            rows={2}
            placeholder={
              draft?.execution_status === 'RESULT_UNKNOWN'
                ? '原单结果待核对，请勿重发同一笔转账'
                : submitted
                  ? '付款已提交，请查询原单；新办理请点新任务'
                  : '说说你想办理什么…'
            }
            onChange={(e) => setText(e.target.value)}
            disabled={block || readBusy}
            onCompositionStart={() => {
              ime.current = true;
            }}
            onCompositionEnd={() => {
              ime.current = false;
            }}
            onKeyDown={(e) => {
              if (
                e.key === 'Enter' &&
                !e.shiftKey &&
                !e.nativeEvent.isComposing &&
                !ime.current &&
                e.keyCode !== 229
              ) {
                e.preventDefault();
                void send();
              }
            }}
          />
          <button
            className="primary"
            aria-label="发送消息"
            disabled={
              block ||
              readBusy ||
              (!(isOriginalQuery(text) || isBalanceQuery(text)) && !status?.configured) ||
              !text.trim()
            }
          >
            <Send size={18} />
            发送
          </button>
        </form>
        <small>Enter 发送 · Shift+Enter 换行。模拟验证码只填业务验证框，请勿发到聊天。</small>
      </div>
    </section>
  );
}
