import { useEffect, useRef, useState } from 'react';
import PaymentRiskNotice from './runtime/PaymentRiskNotice';
import {
  ArrowLeftRight,
  Bell,
  Building2,
  CreditCard,
  LayoutDashboard,
  ReceiptText,
  Repeat2,
  ShieldCheck,
  LogOut,
  RefreshCw,
} from 'lucide-react';
import {
  request,
  getSession,
  login,
  logout,
  RuntimeApiError,
  observeSessionExpired,
} from './runtime/client';
import { runtimeErrorText, type ServiceScope } from './runtime/runtime-feedback';
import RequestActivity from './runtime/RequestActivity';
import {
  lifecycleSchema,
  selectSavedDraftCancellation,
  prepareAgent,
  intentTerminationSchema,
} from './runtime/agent-client';
import {
  recoveryContext,
  recoveryPrepare,
  recoveryConfirm,
  recoveryExecute,
  recoveryRead,
  type RecoveryCreated,
} from './runtime/billing-recovery-client';
import './bank.css';
import DemoVerification from './runtime/DemoVerification';
import BankAgent from './runtime/BankConversation';
import { cardReadSchema } from './runtime/card-read';
import { subscriptionRecordId } from './runtime/subscription-display';
import { rebindSubscriptionRequest, candidateIdentity, canLeaveSubscriptionPreparation } from './subscription-recovery';
import { FeedbackHistory } from './runtime/FeedbackHistory';
import {
  recoveryDirectory,
  recoverPaymentTasks,
  verifiedNonfundingClosure,
  replacementIntent,
  type RecoveredPayment,
} from './runtime/bank-recovery';
import { paymentSchema, fundsResultSchema, fundsMatches } from './runtime/payment-schema';
// Wire views come only from checked backend routes. Browser storage contains
// selectors, never authorization, balances, confirmation tickets or results.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
type View = Record<string, any>;
type PaymentLocator = { preparation?: string; execution?: string };
type Selectors = {
  paymentHistory?: string[];
  paymentNonce?: string;
  paymentTask?: string;
  preparation?: string;
  execution?: string;
  paymentOriginals?: Record<string, PaymentLocator>;
  // Old flat records may already contain a mixed task/preparation pair. Keep
  // them for diagnosis, without treating their association as verified.
  paymentLegacyOriginals?: Array<PaymentLocator & { task?: string }>;
  cardNonce?: string;
  cardOperation?: string;
  subscriptionNonce?: string;
  subscriptionOperation?: string;
  reportTask?: string;
  feedbackNonce?: string;
  feedbackRealm?: string;
  feedbackHistory?: Array<{ nonce: string; realm: string }>;
  subscriptionHistory?: unknown;
  /** A prepare that never reached the service remains retryable with the same nonce. */
  subscriptionPending?: { nonce: string; request: View; candidate?: string; label?: string };
  subscriptionError?: string;
};
const key = 'safeflow.bank.selectors.v1';
function paymentLocator(v: Selectors, task = v.paymentTask): PaymentLocator {
  if (!task || !v.paymentOriginals || !Object.hasOwn(v.paymentOriginals, task)) return {};
  const entry = v.paymentOriginals[task];
  const valid = (id: unknown): id is string =>
    typeof id === 'string' && /^[A-Za-z0-9_.-]{1,200}$/.test(id);
  return entry && valid(entry.preparation)
    ? {
        preparation: entry.preparation,
        execution: valid(entry.execution) ? entry.execution : undefined,
      }
    : {};
}
function migratePaymentSelectors(v: Selectors): Selectors {
  if (v.paymentOriginals !== undefined) return v;
  return {
    ...v,
    paymentOriginals: {},
    paymentLegacyOriginals: [
      ...(v.paymentLegacyOriginals ?? []),
      ...(v.preparation || v.execution
        ? [{ task: v.paymentTask, preparation: v.preparation, execution: v.execution }]
        : []),
    ],
  };
}
function load(key: string): Selectors {
  try {
    const value: unknown = JSON.parse(localStorage.getItem(key) || '{}');
    return value && typeof value === 'object' && !Array.isArray(value) ? value : {};
  } catch {
    return {};
  }
}
function persist(key: string, v: Selectors) {
  const previous = localStorage.getItem(key);
  if (previous) {
    let parsed: unknown;
    try {
      parsed = JSON.parse(previous);
    } catch {
      throw Error('本地记录格式待核对，已保留原记录并阻止覆盖。');
    }
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed))
      throw Error('本地记录格式待核对，已保留原记录并阻止覆盖。');
  }
  localStorage.setItem(key, JSON.stringify(v));
  if (localStorage.getItem(key) !== JSON.stringify(v)) throw Error('无法保存原号，已阻止写入。');
}
function subscriptionHistory(value: unknown): Array<{ nonce: string }> {
  if (!Array.isArray(value)) return [];
  const nonces = new Set<string>();
  return value.flatMap((entry) => {
    if (!entry || typeof entry.nonce !== 'string' || !entry.nonce.trim() || nonces.has(entry.nonce))
      return [];
    nonces.add(entry.nonce);
    return [{ nonce: entry.nonce }];
  });
}
const tabs = [
  ['agent', '智能助手', Bell],
  ['home', '账户总览', LayoutDashboard],
  ['transfer', '转账汇款', ArrowLeftRight],
  ['bills', '我的账单', ReceiptText],
  ['cards', '银行卡', CreditCard],
  ['subscriptions', '订阅管理', Repeat2],
] as const;
const lc = { contract_version: 2, scope: 'MOCK_DRAFT_LIFECYCLE_ONLY' };
const pay = { contract_version: 1, scope: 'PAYMENT_INTENT_ONLY' };
const bg = { contract_version: 1, scope: 'EXPLICIT_PAYMENT_BACKGROUND_V1' };
const path = (task: string, op: string) =>
  `/api/sim/payment-preparations/${encodeURIComponent(task)}/${encodeURIComponent(op)}`;
const life = (v: View) => `/api/sim/draft-lifecycles/${v.task_id}/operations/${v.operation_id}`;
function text(v: unknown) {
  return typeof v === 'string' || typeof v === 'number' ? String(v) : '待核对';
}
function state(v: unknown) {
  const labels: Record<string, string> = {
    UNSUBMITTED: '尚未提交',
    REGISTERED_UNSUBMITTED: '已登记，尚未提交',
    SOURCE_CONFLICT: '来源有变化，请保留原号核对',
    READY: '可查看',
    PREPARED: '待确认',
    DRAFT: '待确认',
    PENDING: '待处理',
    CONFIRMED: '已确认',
    UNCONFIRMED: '未确认',
    SUCCEEDED: '办理成功',
    COMPLETED: '已完成',
    PROCESSING: '处理中',
    RUNNING: '处理中',
    SAVED: '已保存',
    RECORDED: '已记录',
    FAILED: '办理未成功',
    REJECTED: '已拒绝',
    CANCELLED: '已取消',
    REVOKED: '已撤回',
    EXPIRED: '已过期',
    UNKNOWN: '结果未明，请按原号查询',
    RECONCILE_REQUIRED: '结果待核对，请保留原号',
    NEEDS_REVIEW: '需要人工核对',
    NOT_TRIGGERED: '未触发该规则',
    INSUFFICIENT_EVIDENCE: '证据不足',
    DATA_UNAVAILABLE: '资料不可用',
    ACTIVE: '本地已登记',
    CANCELLING: '已记录取消意图',
    NOT_CONFIGURED: '未设置',
    CONFIGURED: '已设置',
    PERSISTED_SIMULATION: '已保存本机模拟提醒',
    NOT_OBSERVED: '尚未观察到渠道记录',
    AVAILABLE: '可用',
    REGISTRATION: '登记本地订阅',
    REMINDER_SETTINGS: '设置本机提醒',
    RECORD_CANCELLATION: '记录本地取消意图',
    REVOKE_CANCELLATION: '撤回本地取消意图',
  };
  return typeof v === 'string' && labels[v] ? labels[v] : '状态待核对';
}
export function draftStatus(v: unknown) {
  const labels: Record<string, string> = {
    NOT_CLAIMED: '待核对',
    PREPARED: '待确认',
    CONFIRMED: '已确认，待保存草案',
    SUCCEEDED: '草案已保存，尚未付款',
    FAILED: '草案保存未成功',
    CANCELLED: '草案已取消，尚未付款',
    RESULT_UNKNOWN: '草案结果待核对',
  };
  return typeof v === 'string' && labels[v] ? labels[v] : '草案状态待核对';
}
function when(v: unknown) {
  if (typeof v !== 'string') return '未提供';
  const date = new Date(v);
  return Number.isFinite(date.getTime())
    ? date.toLocaleString('zh-CN', { timeZone: 'Asia/Shanghai', hour12: false })
    : '时间待核对';
}
function subscriptionMetadata(value: View, capture?: View) {
  if (!capture) return value.display?.version === 1 ? value.display : undefined;
  const matches =
    capture.display?.version === 1 && Array.isArray(capture.display.groups)
      ? capture.display.groups.filter(
          (entry: View) => entry.selection_index === value.selection_index,
        )
      : [];
  return matches.length === 1 && matches[0].metadata?.version === 1
    ? matches[0].metadata
    : undefined;
}
function subscriptionName(value: View, capture?: View) {
  const name = subscriptionMetadata(value, capture)?.name;
  return typeof name === 'string' && name.trim() ? name : '服务待确认';
}
function subscriptionStatus(value: View, candidate = false, capture?: View) {
  if (!candidate || value.disposition === 'ALREADY_MANAGED') return '已管理';
  return value.disposition === 'NOT_IN_LEDGER' && subscriptionMetadata(value, capture)?.name
    ? '待添加管理'
    : '服务待确认';
}
function subscriptionPeriod(value?: View) {
  return value?.cycle === 'MONTHLY' &&
    Number.isInteger(value.billing_day) &&
    value.billing_day >= 1 &&
    value.billing_day <= 31
    ? `每月${value.billing_day}日`
    : '周期待确认';
}
function subscriptionDate(value: unknown) {
  if (typeof value !== 'string') return '待确认';
  const date = new Date(value);
  return Number.isFinite(date.getTime())
    ? date.toLocaleDateString('zh-CN', {
        timeZone: 'Asia/Shanghai',
        year: 'numeric',
        month: 'long',
        day: 'numeric',
      }) + '（北京时间）'
    : '待确认';
}
function subscriptionFee(value?: View) {
  return value?.cycle === 'MONTHLY' &&
    typeof value.amount === 'string' &&
    /^(0|[1-9][0-9]*)\.[0-9]{2}$/.test(value.amount) &&
    value.currency === 'CNY'
    ? '¥' + value.amount + '/月'
    : '待确认';
}
function feedbackState(value: View) {
  const labels: Record<string, string> = {
    PREPARED_OPEN: '待确认，尚未保存',
    EXECUTION_ADMITTED: '处理中，请按原号查询',
    SAVED: '已保存',
    CLOSED_ROLLBACK: '已回退，未保存',
    ABANDONED: '已放弃，未保存',
  };
  return typeof value.state === 'string' && labels[value.state]
    ? labels[value.state]
    : '状态待核对';
}
function Info({ rows }: { rows: Array<[string, unknown]> }) {
  return (
    <dl className="bank-info">
      {rows.map(([label, value]) => (
        <div key={label}>
          <dt>{label}</dt>
          <dd>{text(value)}</dd>
        </div>
      ))}
    </dl>
  );
}
function Arrangement({ value }: { value: View }) {
  return (
    <>
      <Info
        rows={[
          ['本次操作', state(value.action)],
          ...(value.service ? [['服务名称', value.service] as [string, unknown]] : []),
          ...(value.schedule
            ? ([
                ['预计扣费日期', subscriptionDate(value.schedule.expected_date)],
                ['提醒日期', subscriptionDate(value.reminder_date)],
                ['提前天数', value.schedule.days_before],
                ['时区', '北京时间'],
                ['时间承诺', '仅按日期安排，不承诺精确发送时刻'],
              ] as Array<[string, unknown]>)
            : []),
        ]}
      />
      {value.candidate?.charges && (
        <details>
          <summary>查看扣费依据</summary>
          {value.candidate.charges.map((c: View, i: number) => (
            <p key={i}>
              {subscriptionDate(c.completed_at)} · ¥{(c.amount_minor_units / 100).toFixed(2)}
            </p>
          ))}
        </details>
      )}
      <p>{value.message || '仅保存本机模拟设置，不表示真实通知已经发送。'}</p>
    </>
  );
}
export default function BankApp() {
  const [page, setPage] = useState(() => window.location.hash.slice(1) || 'home'),
    [auth, setAuth] = useState(false),
    [secret, setSecret] = useState(''),
    [busy, setBusy] = useState(false),
    [notice, setNotice] = useState('正在检查本机会话…');
  const [overview, setOverview] = useState<View>(),
    [draft, setDraft] = useState<View>(),
    [payment, setPayment] = useState<View>(),
    [delegation, setDelegation] = useState<View>(),
    [delegationConfirmed, setDelegationConfirmed] = useState(false),
    [verified, setVerified] = useState(false),
    [funds, setFunds] = useState<View>();
  const [replacementAmount,setReplacementAmount]=useState('');
  const [draftSeconds,setDraftSeconds]=useState<number>();
  const [card, setCard] = useState<View>(),
    [cardResult, setCardResult] = useState<View>(),
    [cardConfirmed, setCardConfirmed] = useState(false),
    [cardVerified, setCardVerified] = useState(false),
    [challenge, setChallenge] = useState(''),
    [code, setCode] = useState('');
  const [report, setReport] = useState<View>(),
    [feedback, setFeedback] = useState<RecoveryCreated>(),
    [feedbackConfirmed, setFeedbackConfirmed] = useState(false),
    [feedbackResult, setFeedbackResult] = useState<unknown>();
  const [ledger, setLedger] = useState<View>(),
    [capture, setCapture] = useState<View>(),
    [sub, setSub] = useState<View>(),
    [subConfirmed, setSubConfirmed] = useState(false),
    [subResult, setSubResult] = useState<View>(),
    [subscriptionReadNotice, setSubscriptionReadNotice] = useState(''),
    [attempts, setAttempts] = useState<View>(),
    [subscriptionError, setSubscriptionError] = useState('');
  const selectorKey = useRef(''),
    requests = useRef(new AbortController());
  const [agentService, setAgentService] = useState('');
  const [diagnostic, setDiagnostic] = useState(''),
    [paymentCheckedAt, setPaymentCheckedAt] = useState('');
  const displayScope = useRef<ServiceScope>('home');
  const navigationEpoch = useRef(0);
  displayScope.current = (
    auth ? (page === 'agent' ? agentService || 'agent' : page) : 'login'
  ) as ServiceScope;
  const paymentReadController = useRef<AbortController | undefined>(undefined);
  const [recoveryReady, setRecoveryReady] = useState(false),
    [paymentSelectionVerified, setPaymentSelectionVerified] = useState(false),
    [paymentTasks, setPaymentTasks] = useState<string[]>([]),
    [openPayments, setOpenPayments] = useState<string[]>([]);
  const paymentRecovery = useRef(new Map<string, RecoveredPayment>());
  const paymentReadEpoch = useRef(0);
  const [cardCatalog, setCardCatalog] = useState<View[]>([]),
    [selectedCard, setSelectedCard] = useState(''),
    [cardRead, setCardRead] = useState<View>();
  const refs = useRef<Selectors>({}),
    epoch = useRef(0),
    locked = useRef(false),
    [saved, setSaved] = useState(refs.current);
  function save(v: Partial<Selectors>) {
    const next = { ...refs.current, ...v };
    const original = paymentLocator(next);
    next.preparation = original.preparation;
    next.execution = original.execution;
    if (!selectorKey.current) throw Error('本人会话尚未核对');
    persist(selectorKey.current, next);
    refs.current = next;
    setSaved(next);
  }
  function rememberPayment(task: string, preparation?: string, execution?: string) {
    save({
      paymentOriginals: {
        ...refs.current.paymentOriginals,
        [task]: { preparation, execution },
      },
    });
  }
  function clearViews() {
    navigationEpoch.current++;
    epoch.current++;
    requests.current.abort();
    paymentReadController.current?.abort();
    requests.current = new AbortController();
    setAgentService('');
    setRecoveryReady(false);
    setPaymentSelectionVerified(false);
    setSubscriptionError('');
    setOpenPayments([]);
    setPaymentTasks([]);
    paymentRecovery.current.clear();
    setCardCatalog([]);
    setSelectedCard('');
    setCardRead(undefined);
    setOverview(undefined);
    setDraft(undefined);
    setPayment(undefined);
    setDelegation(undefined);
    setDelegationConfirmed(false);
    setVerified(false);
    setFunds(undefined);
    setCard(undefined);
    setCardResult(undefined);
    setCardConfirmed(false);
    setCardVerified(false);
    setReport(undefined);
    setFeedback(undefined);
    setFeedbackConfirmed(false);
    setFeedbackResult(undefined);
    setLedger(undefined);
    setCapture(undefined);
    setSub(undefined);
    setSubConfirmed(false);
    setSubResult(undefined);
    setAttempts(undefined);
    setSubscriptionReadNotice('');
    setSecret('');
    setCode('');
    setChallenge('');
  }
  async function call(p: string, body?: unknown): Promise<View> {
    const generation = epoch.current,
      signal = requests.current.signal;
    let value;
    try {
      value = await request(p, body === undefined ? 'GET' : 'POST', body, signal);
    } catch (error) {
      if (signal.aborted || generation !== epoch.current)
        throw new DOMException('旧会话已结束', 'AbortError');
      throw error;
    }
    if (signal.aborted || generation !== epoch.current)
      throw new DOMException('旧会话已结束', 'AbortError');
    if (!value || typeof value !== 'object') throw Error('服务响应不完整；保留原号查询。');
    return value as View;
  }
  async function run(work: () => Promise<void>) {
    if (locked.current) return;
    locked.current = true;
    setBusy(true);
    const e = epoch.current;
    const navigation = navigationEpoch.current;
    // One chat action may move between service cards. Its originating page is
    // stable; the old card's scope must not hide that action's current failure.
    const scope: ServiceScope = page === 'agent' ? 'agent' : displayScope.current;
    const current = () => e === epoch.current && navigation === navigationEpoch.current &&
      (scope === 'agent' ? window.location.hash === '#agent' : scope === displayScope.current);
    setDiagnostic('');
    setNotice('正在核对服务端实际状态…');
    try {
      await work();
      if (current())
        setNotice(scope === 'agent' ? '' : '本次处理已结束，请核对页面中的结果与下一步提示。');
    } catch (error) {
      if (!current()) return;
      if (
        e !== epoch.current &&
        ((error instanceof DOMException && error.name === 'AbortError') ||
          (error instanceof Error && error.message === 'ABORTED'))
      )
        return;
      if (error instanceof RuntimeApiError && error.status === 401) {
        clearViews();
        setAuth(false);
      }
      if (
        current() ||
        (error instanceof RuntimeApiError && error.status === 401)
      ) {
        setNotice(runtimeErrorText(error, scope));
        setDiagnostic(
          error instanceof RuntimeApiError
            ? `HTTP ${error.status} · ${error.reason}`
            : error instanceof Error
              ? error.name
              : 'UNVERIFIED',
        );
      }
    } finally {
      locked.current = false;
      setBusy(false);
    }
  }
  useEffect(() => {
    setNotice('');
    setDiagnostic('');
    if (page !== 'transfer') paymentReadController.current?.abort();
  }, [page, agentService]);
  useEffect(
    () =>
      observeSessionExpired(() => {
        clearViews();
        setAuth(false);
        setNotice(
          '登录已失效，请重新登录。办理编号和历史记录已保留，旧确认、验证码和许可不会恢复。',
        );
      }),
    [],
  );
  useEffect(() => {
    if (!auth) return;
    const c = new AbortController();
    const check = () => {
      if (document.visibilityState !== 'visible') return;
      void getSession(c.signal)
        .then((v) => {
          if (!c.signal.aborted && !v.authenticated) {
            clearViews();
            setAuth(false);
            setNotice('登录已过期，请重新登录后查询原办理进度。');
          }
        })
        .catch(() => {
          /* Network failure is not evidence of logout. */
        });
    };
    window.addEventListener('focus', check);
    document.addEventListener('visibilitychange', check);
    const timer = setInterval(check, 60000);
    return () => {
      c.abort();
      clearInterval(timer);
      window.removeEventListener('focus', check);
      document.removeEventListener('visibilitychange', check);
    };
  }, [auth]);
  async function bindSelectors() {
    const generation = epoch.current;
    const session = await call('/api/card-read/session');
    const directory = await recoveryDirectory(requests.current.signal);
    if (generation !== epoch.current) throw new DOMException('旧会话', 'AbortError');
    if (!session.authenticated || !session.context_ref) throw Error('本人会话未核实');
    selectorKey.current = key + '.' + directory.context_ref;
    const legacy = migratePaymentSelectors(load(key + '.' + session.context_ref));
    const stable = migratePaymentSelectors(load(selectorKey.current));
    refs.current = {
      ...legacy,
      ...stable,
      paymentOriginals: { ...legacy.paymentOriginals, ...stable.paymentOriginals },
      paymentLegacyOriginals: [
        ...(legacy.paymentLegacyOriginals ?? []),
        ...(stable.paymentLegacyOriginals ?? []),
      ],
    };
    const history = [
      ...new Set([
        ...(refs.current.paymentHistory ?? []),
        ...(refs.current.paymentTask ? [refs.current.paymentTask] : []),
        ...directory.payment_tasks,
      ]),
    ];
    if (history.length > 256) throw Error('原号记录已满，请保留已有记录');
    const previous = refs.current.subscriptionHistory;
    const known = new Set(subscriptionHistory(previous).map((r) => r.nonce));
    // Preserve damaged/legacy history verbatim. Server discovery can add
    // selectors, but must never silently discard a stored original entry.
    const mergedHistory =
      previous !== undefined && !Array.isArray(previous)
        ? previous
        : [
            ...(Array.isArray(previous) ? previous : []),
            ...directory.subscription_nonces
              .filter((nonce) => !known.has(nonce))
              .map((nonce) => ({ nonce })),
          ];
    save({ paymentHistory: history, subscriptionHistory: mergedHistory });
    setPaymentTasks(history);
    setOpenPayments(history);
    setSubscriptionError(refs.current.subscriptionError ?? '');
  }
  async function readCards() {
    const catalog = await call('/api/bank/cards');
    if (!Array.isArray(catalog.items)) throw Error('可信卡目录未取得');
    setCardCatalog(catalog.items);
    const target = catalog.items.find((c: View) => c.card_id === selectedCard) || catalog.items[0];
    if (target) {
      setSelectedCard(target.card_id);
      setCardRead(
        cardReadSchema.parse(
          await call('/api/card-read', {
            kind: 'QUERY',
            account_id: target.account_id,
            request_ref: crypto.randomUUID(),
          }),
        ),
      );
    }
  }
  async function openAgentService(
    service: 'BILLS' | 'CARDS' | 'SUBSCRIPTIONS',
    signal: AbortSignal,
  ) {
    if (signal.aborted) return;
    setAgentService('');
    if (service === 'SUBSCRIPTIONS') {
      const nextLedger = await readSubscriptionLedger();
      if (signal.aborted) return;
      const nextCapture = await call('/api/subscriptions/change', { kind: 'CAPTURE' });
      if (signal.aborted) return;
      setLedger(nextLedger);
      setCapture(nextCapture);
      setSubscriptionReadNotice('');
      setSub(undefined);
      setSubConfirmed(false);
      setSubResult(undefined);
    } else if (service === 'BILLS') await loadReport();
    else await readCards();
    if (signal.aborted) return;
    setAgentService({ BILLS: 'bills', CARDS: 'cards', SUBSCRIPTIONS: 'subscriptions' }[service]);
  }
  async function balances() {
    const v = await call('/api/bank/overview');
    setOverview(v);
  }
  async function readSubscriptionLedger() {
    const value = await call('/api/subscriptions', { kind: 'LIST' });
    if (!Array.isArray(value.items)) throw Error('订阅来源未返回可核对的记录列表');
    return value;
  }
  async function subscriptions() {
    // A committed change or an explicit refresh makes old version-bound
    // captures unusable. Retire them, rather than guessing new dispositions.
    setCapture(undefined);
    setLedger(undefined);
    setSubscriptionReadNotice('旧候选已归档；如需登记其他订阅，请重新识别。');
    try {
      setLedger(await readSubscriptionLedger());
    } catch (error) {
      setSubscriptionReadNotice(
        '订阅列表读取失败。已取得的办理结果和编号已保留，请重新读取列表或查询办理结果；不要重复提交。',
      );
      throw error;
    }
  }
  async function refreshSubscriptionResultViews() {
    try {
      await subscriptions();
    } catch (error) {
      // An auxiliary read failure must not turn an already returned business
      // result into a failed operation or restore its old execution controls.
      if (error instanceof RuntimeApiError && error.status === 401) throw error;
    }
  }
  async function refreshSubscriptionCapture() {
    // A recovery retry must observe the current ledger and obtain a fresh
    // capture/version. The prior capture may have expired or been consumed.
    await subscriptions();
    const next = await call('/api/subscriptions/change', { kind: 'CAPTURE' });
    setCapture(next);
    return next;
  }
  async function recoverPayment(selected?: string) {
    setRecoveryReady(false);
    setPaymentSelectionVerified(false);
    const generation = epoch.current;
    const readGeneration = ++paymentReadEpoch.current;
    paymentReadController.current?.abort();
    const controller = new AbortController();
    paymentReadController.current = controller;
    const sessionSignal = requests.current.signal;
    const stop = () => controller.abort();
    sessionSignal.addEventListener('abort', stop, { once: true });
    const signal = controller.signal;
    const current = () =>
      generation === epoch.current &&
      readGeneration === paymentReadEpoch.current &&
      !signal.aborted;
    // A failed re-read must not leave another task's confirmation or result
    // displayed beneath the newly selected original number.
    setDraft(undefined);
    setPayment(undefined);
    setFunds(undefined);
    setDelegation(undefined);
    setDelegationConfirmed(false);
    setVerified(false);
    setCode('');
    // Select before the directory read as that read itself may fail. save()
    // projects only this task's own cache; another task's IDs cannot follow it.
    if (selected) save({ paymentTask: selected });
    try {
      const directory = await recoveryDirectory(signal);
      if (!current()) return;
      const ids = [
        ...new Set([
          ...(refs.current.paymentHistory ?? []),
          ...(refs.current.paymentTask ? [refs.current.paymentTask] : []),
          ...directory.payment_tasks,
        ]),
      ];
      save({
        paymentHistory: ids,
        paymentTask: selected ?? refs.current.paymentTask ?? ids.at(-1),
      });
      setPaymentTasks(ids);
      setOpenPayments(ids);
      const preferred = selected ?? refs.current.paymentTask;
      const ordered = preferred ? [preferred, ...ids.filter((id) => id !== preferred)] : ids;
      const { values: recovered, failures } = await recoverPaymentTasks(
        ordered,
        signal,
        (id, value) => {
          if (current() && value) {
            const original =
              value.results.find((r) => !['SUCCEEDED', 'FAILED'].includes(r.funds.state)) ??
              value.results.at(-1);
            rememberPayment(
              id,
              original?.payment.operation.operation_id,
              original?.funds.execution_operation_id ?? undefined,
            );
            if (id === preferred) {
              setPaymentSelectionVerified(true);
              setDraft(value.draft);
              setPayment(original?.payment);
              setFunds(original?.funds);
            }
          }
        },
      );
      if (!current()) return;
      for (const error of failures.values())
        if (error instanceof RuntimeApiError && error.status === 401) throw error;
      paymentRecovery.current = recovered;
      for(const value of recovered.values()) value.nonfundingClosed=failures.size===0&&verifiedNonfundingClosure(value,directory.closed_nonfunding_tasks,directory.ended_draft_intents);
      const open = ids.filter((id) => {const value=recovered.get(id);return !value?.terminal&&!value?.nonfundingClosed;});
      setOpenPayments(open);
      const id = selected ?? refs.current.paymentTask ?? open.at(-1) ?? ids.at(-1);
      if (id && recovered.has(id)) {
        const value = recovered.get(id)!;
        // Surface unresolved funds ahead of a later terminal preparation.
        const latest =
          value.results.find((r) => !['SUCCEEDED', 'FAILED'].includes(r.funds.state)) ??
          value.results.at(-1);
        save({ paymentTask: id });
        setPaymentSelectionVerified(true);
        setDraft(value.draft);
        setPayment(latest?.payment);
        setFunds(latest?.funds);
      }
      setRecoveryReady(failures.size === 0);
      setPaymentCheckedAt(new Date().toISOString());
      await balances();
      // Successful originals remain visible, while failed ones keep their number
      // and occupy the new-payment gate. newDraft also stops on this exception.
      if (failures.size)
        throw Error('部分原付款读取失败，原号及占用已保留；已选原单可独立查看，请稍后重查失败原单');
    } finally {
      sessionSignal.removeEventListener('abort', stop);
    }
  }
  useEffect(() => {
    let active = true;
    requests.current = new AbortController();
    locked.current = true;
    setBusy(true);
    getSession()
      .then(async (v) => {
        if (!active) return;
        setAuth(v.authenticated);
        setNotice(v.authenticated ? '已登录本机模拟银行。' : '请使用本机模拟口令登录。');
        if (!v.authenticated) return;
        try {
          await bindSelectors();
          if (window.location.hash === '#transfer') await recoverPayment();
          else await balances();
          if (active) setNotice('已登录模拟银行，已从服务端恢复办理目录。');
        } catch (error) {
          if (active)
            setNotice(
              runtimeErrorText(error, window.location.hash === '#transfer' ? 'transfer' : 'home'),
            );
        }
      })
      .catch(() => {
        if (active) setNotice('本机服务未连接，请先按启动说明运行候选。');
      })
      .finally(() => {
        if (active) {
          locked.current = false;
          setBusy(false);
        }
      });
    return () => {
      active = false;
      requests.current.abort();
    };
  }, []);
  async function signIn() {
    await login(secret);
    clearViews();
    await bindSelectors();
    setAuth(true);
    if (page === 'transfer') await recoverPayment();
    else await balances();
  }
  async function signOut() {
    clearViews();
    await logout();
    setAuth(false);
    setNotice('已退出。已受理的独立委托按原合同处理；重新登录后只读查询原号。');
  }
  async function newDraft() {
    await recoverPayment();
    if ([...paymentRecovery.current.values()].some((v) => !v.terminal&&!v.nonfundingClosed))
      throw Error('已有未结束的付款任务，请先按原号核对');
    setDraft(undefined);
    setPayment(undefined);
    setFunds(undefined);
    setDelegation(undefined);
    setVerified(false);
    setPaymentSelectionVerified(false);
    const nonce = crypto.randomUUID();
    const task = 'bank_payment_' + nonce;
    save({
      paymentNonce: nonce,
      paymentTask: task,
      paymentHistory: [...(refs.current.paymentHistory ?? []), task],
    });
    setPaymentTasks(refs.current.paymentHistory ?? []);
    setOpenPayments([task]);
    setDraft(await call('/api/bank/payment-draft', { nonce }));
  }
  async function refreshDraftRecipient() {
    if (!draft || draft.next_action?.kind !== 'RECONFIRM_RECIPIENT') return;
    const next = await call('/api/sim/draft-lifecycles/' + draft.task_id + '/actions', {
      ...lc,
      selection_ticket: draft.selection_ticket,
      task_revision: draft.task_revision,
      plan_version: draft.plan_version,
      facts_version: draft.facts_version,
      action: draft.action,
      parameters: draft.parameters,
    });
    setDraft(await call(life(next) + '/prepare', lc));
  }
  async function prepareDraft() {
    if (!draft) return;
    setDraft(await call(life(draft) + '/prepare', lc));
  }
  async function confirmDraft() {
    if (!draft) return;
    setDraft(
      await call(life(draft) + '/confirm', {
        ...lc,
        confirmation_ref: draft.confirmation.confirmation_ref,
        display_ticket: draft.display_ticket,
        action: 'CONFIRM',
      }),
    );
  }
  async function executeDraft() {
    if (!draft) return;
    const next = await call(life(draft) + '/execute', {
      ...lc,
      confirmation_ref: draft.confirmation.confirmation_ref,
      task_revision: draft.task_revision,
      operation_revision: draft.operation_revision,
      plan_version: draft.plan_version,
      facts_version: draft.facts_version,
    });
    setDraft(next);
    if (next.task_state === 'CANCELLED') await recoverPayment();
  }
  async function preparePayment() {
    if (!draft || draft.action === 'CANCEL_MOCK_DRAFT' || draft.task_state === 'CANCELLED') return;
    const p = paymentSchema.parse(
      await call('/api/sim/payment-preparations/' + draft.task_id, {
        ...pay,
        source: {
          selection_ticket: draft.selection_ticket,
          task_revision: draft.task_revision,
          plan_version: draft.plan_version,
          facts_version: draft.facts_version,
          draft_id: draft.receipt.draft_id,
          draft_version: draft.receipt.draft_version,
        },
      }),
    );
    if (p.task_id !== draft.task_id || p.task_id !== refs.current.paymentTask)
      throw Error('付款原号不匹配，请按原号查询');
    rememberPayment(p.task_id, p.operation.operation_id);
    setPaymentSelectionVerified(true);
    setPayment(p);
  }
  function currentPayment() {
    const p = paymentSchema.parse(payment);
    if (
      p.task_id !== refs.current.paymentTask ||
      p.operation.operation_id !== paymentLocator(refs.current).preparation
    )
      throw Error('当前原付款尚未核实，请按原号查询');
    return p;
  }
  async function confirmPayment() {
    if (!payment) return;
    const p = currentPayment();
    const confirmed = paymentSchema.parse(
      await call(path(p.task_id, p.operation.operation_id) + '/confirm', {
        ...pay,
        confirmation_ref: p.confirmation_ref,
        display_ticket: p.display_ticket,
        action: 'CONFIRM',
      }),
    );
    if (
      confirmed.task_id !== p.task_id ||
      confirmed.operation.operation_id !== p.operation.operation_id ||
      confirmed.snapshot_digest !== p.snapshot_digest
    )
      throw Error('确认回执与原付款不匹配，请按原号查询');
    setPayment(confirmed);
  }
  async function background(action: View) {
    const p = currentPayment();
    return call(path(p.task_id, p.operation.operation_id) + '/background', {
      ...bg,
      action,
    });
  }
  async function loadReport() {
    const v = await call('/api/team-demo/billing', {});
    const r = v.results?.find((r: View) => r.data?.monthly_report)?.data;
    if (!r) throw Error('未取得月度报告');
    save({ reportTask: v.task.task_id });
    setReport(r);
  }
  async function recoverReport() {
    if (!refs.current.reportTask) return;
    const v = await call('/api/agent/tasks/' + refs.current.reportTask);
    const r = v.results?.find((r: View) => r.data?.monthly_report)?.data;
    if (!r) throw Error('原报告尚不可读');
    setReport(r);
  }
  async function feedbackWire<T>(work: (signal: AbortSignal) => Promise<T>): Promise<T> {
    const generation = epoch.current,
      signal = requests.current.signal;
    const value = await work(signal);
    if (signal.aborted || generation !== epoch.current)
      throw new DOMException('旧会话已结束', 'AbortError');
    return value;
  }
  async function prepareFeedback(
    reference: View,
    choice: 'MINE' | 'NOT_MINE' | 'UNSURE' = 'NOT_MINE',
  ) {
    const c = await feedbackWire((signal) => recoveryContext(signal));
    if (!c.available || !c.recovery_realm) throw Error('反馈恢复未启用');
    if (refs.current.feedbackNonce) throw Error('已有反馈原号，请只读核对');
    const realm = c.recovery_realm;
    const nonce = crypto.randomUUID();
    const history =
      refs.current.feedbackHistory ??
      (refs.current.feedbackNonce && refs.current.feedbackRealm
        ? [{ nonce: refs.current.feedbackNonce, realm: refs.current.feedbackRealm }]
        : []);
    if (history.length >= 256) throw Error('反馈原号记录已满，请查询已有记录');
    save({
      feedbackNonce: nonce,
      feedbackRealm: c.recovery_realm,
      feedbackHistory: [...history, { nonce, realm: c.recovery_realm }],
    });
    const v = await feedbackWire((signal) =>
      recoveryPrepare(
        realm,
        nonce,
        {
          input_version: 1,
          report_id: report!.report.metadata.report_id,
          transaction: reference as {
            source_id: string;
            account_id: string;
            transaction_id: string;
          },
          choice,
        },
        signal,
      ),
    );
    if (v.kind !== 'CREATED') throw Error('已存在准备，仅可原号查询');
    setFeedback(v);
  }
  async function recoverFeedback() {
    const r = refs.current;
    if (r.feedbackNonce && r.feedbackRealm)
      setFeedbackResult(
        await feedbackWire((signal) => recoveryRead(r.feedbackRealm!, r.feedbackNonce!, signal)),
      );
    setFeedback(undefined);
    setFeedbackConfirmed(false);
  }
  async function prepareCard() {
    if (refs.current.cardNonce) throw Error('已有卡操作原号，请查询');
    if (!selectedCard || !cardCatalog.some((c) => c.card_id === selectedCard))
      throw Error('请先读取可信卡目录并选择银行卡');
    const nonce = crypto.randomUUID();
    save({ cardNonce: nonce });
    const v = await call('/api/card-safety', {
      kind: 'PREPARE',
      account_id: cardCatalog.find((c) => c.card_id === selectedCard)?.account_id,
      card_id: selectedCard,
      nonce,
    });
    save({ cardOperation: v.operation_id });
    setCard(v);
    setCardResult(undefined);
    setCardConfirmed(false);
    setCardVerified(false);
  }
  async function recoverCard() {
    const r = refs.current;
    const v = await call(
      '/api/card-safety',
      r.cardOperation
        ? { kind: 'QUERY', operation_id: r.cardOperation }
        : { kind: 'QUERY_NONCE', nonce: r.cardNonce },
    );
    save({ cardOperation: v.operation_id });
    setCard(v);
    setCardResult(
      await call('/api/card-safety', {
        kind: 'QUERY_ORIGINAL',
        operation_id: v.operation_id,
      }),
    );
    setCardConfirmed(false);
    setCardVerified(false);
  }
  async function prepareSubscription(input: View) {
    if (refs.current.subscriptionOperation) throw Error('已有订阅原号，请先核对');
    const pending = refs.current.subscriptionPending;
    const nonce = pending?.nonce ?? refs.current.subscriptionNonce ?? crypto.randomUUID();
    let request = pending?.request ?? input;
    const candidate =
      pending?.candidate ??
      (input.kind === 'REGISTRATION'
        ? candidateIdentity(
            capture?.preview?.recognition?.groups?.find(
              (g: View) => g.selection_index === input.candidate_index,
            ),
          )
        : undefined);
    const label =
      pending?.label ??
      (input.kind === 'REGISTRATION'
        ? subscriptionName({ selection_index: input.candidate_index }, capture)
        : undefined);
    const previous = refs.current.subscriptionHistory;
    if (previous !== undefined && !Array.isArray(previous))
      throw Error('办理历史格式待核对，原记录已保留，请先核对。');
    try {
      if (pending) {
        // A lost PREPARE response may already have committed. Never replace
        // its capture until the server has explicitly confirmed NOT_FOUND.
        try {
          const found = await call('/api/subscriptions/change', { kind: 'QUERY_NONCE', nonce });
          save({
            subscriptionOperation: found.operation_id,
            subscriptionPending: undefined,
            subscriptionError: undefined,
          });
          setSub(undefined);
          setSubConfirmed(false);
          setSubResult(found.result);
          setSubscriptionError('');
          await refreshSubscriptionResultViews();
          return;
        } catch (error) {
          if (
            !(error instanceof RuntimeApiError) ||
            error.status !== 404 ||
            error.reason !== 'NOT_FOUND'
          )
            throw error;
        }
        setSub(undefined);
        setSubConfirmed(false);
        setSubResult(undefined);
        const fresh = await refreshSubscriptionCapture();
        if (typeof fresh.capture_ref !== 'string') throw Error('服务端未返回新的订阅捕获号');
        request = rebindSubscriptionRequest(
          pending,
          fresh as Parameters<typeof rebindSubscriptionRequest>[1],
        );
      }
      const history = Array.isArray(previous) ? previous : [];
      if (history.length >= 256) throw Error('订阅原号记录已满，请查询已有记录');
      save({
        subscriptionNonce: nonce,
        subscriptionHistory: history.some((r) => r.nonce === nonce)
          ? history
          : [...history, { nonce }],
        subscriptionPending: { nonce, request, candidate, label },
        subscriptionError: undefined,
      });
      setSub(undefined);
      setSubConfirmed(false);
      setSubResult(undefined);
      setCapture(undefined);
      setSubscriptionReadNotice('旧候选已归档；如需登记其他订阅，请重新识别。');
      setSubscriptionError('');
      const v = await call('/api/subscriptions/change', { kind: 'PREPARE', nonce, request });
      save({
        subscriptionOperation: v.operation_id,
        subscriptionPending: undefined,
        subscriptionError: undefined,
      });
      setSubscriptionError('');
      setSub(v);
    } catch (error) {
      const reason = error instanceof RuntimeApiError ? error.reason : 'UNAVAILABLE';
      save({ subscriptionError: reason });
      setSubscriptionError(reason);
      throw error;
    }
  }
  async function recoverSub() {
    const r = refs.current;
    setSub(undefined);
    setSubConfirmed(false);
    setCapture(undefined);
    try {
      if (r.subscriptionOperation) {
        const result = await call('/api/subscriptions/change', {
          kind: 'QUERY',
          operation_id: r.subscriptionOperation,
        });
        save({ subscriptionPending: undefined, subscriptionError: undefined });
        setSubscriptionError('');
        setSubResult(result);
      } else if (r.subscriptionNonce) {
        const v = await call('/api/subscriptions/change', {
          kind: 'QUERY_NONCE',
          nonce: r.subscriptionNonce,
        });
        save({
          subscriptionOperation: v.operation_id,
          subscriptionPending: undefined,
          subscriptionError: undefined,
        });
        setSubscriptionError('');
        setSubResult(v.result);
      }
    } catch (error) {
      const reason = error instanceof RuntimeApiError ? error.reason : 'UNAVAILABLE';
      setSubscriptionError(
        reason === 'NOT_FOUND' && r.subscriptionPending && r.subscriptionError
          ? `${r.subscriptionError}；NOT_FOUND`
          : reason,
      );
      throw error;
    }
    setSub(undefined);
    setSubConfirmed(false);
    await refreshSubscriptionResultViews();
  }
  const disable = busy || !auth;
  const monthly = report?.monthly_report;
  const feedbackEvidence =
    feedback && report?.report?.metadata?.report_id === feedback.parameters.report_id
      ? report.report.evidence?.find((row: View) =>
          ['source_id', 'account_id', 'transaction_id'].every(
            (k) =>
              row.reference?.[k] ===
              feedback.parameters.transaction[k as keyof typeof feedback.parameters.transaction],
          ),
        )
      : undefined;
  // Submitted preparation views are historical observations. Only the actual
  // original funds result describes current payment progress.
  const paymentReadOnly =
    payment?.business_recovery_required ||
    !!payment?.invalid_reason ||
    payment?.confirmation_status === 'REVOKED' ||
    funds?.submitted === true ||
    [
      'SUCCEEDED',
      'FAILED',
      'PROCESSING',
      'UNKNOWN',
      'RECONCILE_REQUIRED',
      'SOURCE_CONFLICT',
    ].includes(funds?.state);
  const paymentClosed = recoveryReady&&paymentRecovery.current.get(saved.paymentTask??'')?.nonfundingClosed===true;
  const paymentTerminal = ['SUCCEEDED', 'FAILED'].includes(funds?.state);
  const paymentCanStartNew = recoveryReady && openPayments.length === 0;
  useEffect(()=>{
    if(!auth||page!=='transfer'||!draft?.intent_expires_at||draft.intent_termination||payment){setDraftSeconds(undefined);return;}
    let requested=false;
    const tick=()=>{
      const remaining=Math.max(0,Math.ceil((Date.parse(draft.intent_expires_at)-Date.now())/1000));setDraftSeconds(remaining);
      if(remaining===0&&!busy&&!requested){requested=true;void run(()=>recoverPayment(draft.task_id));}
    };
    tick();const timer=setInterval(tick,1000);return()=>clearInterval(timer);
  },[auth,page,draft?.task_id,draft?.intent_expires_at,!!draft?.intent_termination,!!payment,busy]);
  const businessPage = page === 'agent' ? agentService : page;
  const businessPanels = (
    <>
      {auth && businessPage === 'home' && (
        <>
          <section className="bank-panel bank-balance">
            <p>本人账户 · 服务端实时读取</p>
            <h2>
              {text(
                overview?.accounts?.[0]?.balance?.available_balance ??
                  overview?.accounts?.[0]?.balance?.balance,
              )}
            </h2>
            <p>人民币 CNY · 可用余额</p>
            <button disabled={disable} onClick={() => void run(balances)}>
              刷新账户余额
            </button>
          </section>
          <section className="bank-grid">
            {overview?.accounts?.map((v: View, i: number) => (
              <section className="bank-panel" key={i}>
                <h2>我的账户</h2>
                <p>{text(v.account.masked_account_no ?? v.account.account_id)}</p>
                <Info
                  rows={[
                    ['账户名称', v.account.display_name],
                    ['可用余额（元）', v.balance.available_balance],
                    ['余额读取时间', when(v.balance.as_of)],
                  ]}
                />
              </section>
            ))}
            <section className="bank-panel">
              <h2>办理服务</h2>
              <p>模拟转账100元、本人主动冻结、月度账单和订阅管理。</p>
              <button onClick={() => setPage('transfer')}>去转账</button>
              <button onClick={() => setPage('bills')}>查看账单</button>
            </section>
          </section>
        </>
      )}
      {auth && businessPage === 'transfer' && (
        <section className="bank-panel">
          <h2>模拟转账</h2>
          <p>
            工资账户 →
            已登记收款人。先核对并确认付款，再明确同意本笔独立后台委托；页面提供本次模拟验证码，需你手动填写验证后提交。
          </p>
          <div className="bank-actions">
            <button
              className="primary"
              disabled={disable || !paymentCanStartNew}
              onClick={() => void run(newDraft)}
            >
              {saved.paymentTask && paymentCanStartNew ? '创建新的100元草稿' : '创建100元草稿'}
            </button>
            <button
              disabled={disable || !saved.paymentTask}
              onClick={() => void run(() => recoverPayment())}
            >
              按原号查询付款
            </button>
          </div>
          {!recoveryReady && <p role="status">原付款尚未全部核实，只允许按原号查询。</p>}
          {paymentCheckedAt && (
            <p>
              上次核查：{when(paymentCheckedAt)}；待核实或未结束 {openPayments.length}{' '}
              笔。余额、账单和卡片可独立查看。
            </p>
          )}
          {paymentTasks.map((id) => (
            <button key={id} disabled={disable} onClick={() => void run(() => recoverPayment(id))}>
              查询原任务 {id}
            </button>
          ))}
          {saved.paymentTask && (
            <p className="bank-ref">
              原任务 {saved.paymentTask}
              <br />
              原付款 {saved.preparation || '待核实'}
              {saved.execution && (
                <>
                  <br />
                  原执行 {saved.execution}
                </>
              )}
              {!paymentSelectionVerified && (
                <>
                  <br />
                  当前原号待核实，仅用于查询定位
                </>
              )}
              {paymentTerminal && (
                <>
                  <br />
                  上一笔已结束，可创建独立新任务
                </>
              )}
            </p>
          )}
          {draft && (
            <>
              <h3>1. 原草稿资料{paymentReadOnly ? '（历史，只读）' : '与确认'}</h3>
              <p>
                金额 {text(draft.parameters?.amount)} · 状态{' '}
                {draft.next_action?.kind === 'EXECUTE'
                  ? '已确认，待保存草案'
                  : draftStatus(draft.task_state === 'CANCELLED' ? 'CANCELLED' : draft.execution_status)}
              </p>
              {draft.confirmation_recipient && (
                <Info
                  rows={[
                    ['确认收款人', draft.confirmation_recipient.display_name],
                    ['收款银行', draft.confirmation_recipient.bank_name],
                    ['模拟卡号', draft.confirmation_recipient.simulation_card_number],
                  ]}
                />
              )}
              {draft.intent_expires_at&&!draft.intent_termination&&<p>草案有效至 {new Date(draft.intent_expires_at).toLocaleString('zh-CN')}；服务器核验到期后失效。</p>}
              {draftSeconds!==undefined&&!draft.intent_termination&&<p>{draftSeconds>0?`本机估计剩余 ${draftSeconds} 秒`:'正在核查到期状态'}；以服务器核验为准。</p>}
              {draft.intent_termination&&<p>{draft.intent_termination.reason==='EXPIRED'?'未付款草案已过期':'旧未付款意图已结束'}；原记录保留，可创建新的草案。</p>}
              {!paymentClosed&&!draft.intent_termination&&['CREATE_MOCK_DRAFT','MODIFY_MOCK_DRAFT'].includes(draft.action)&&draft.execution_status!=='RESULT_UNKNOWN'&&!payment&&<>
                <button disabled={disable||!paymentSelectionVerified} onClick={()=>void run(async()=>{
                  intentTerminationSchema.parse(await call(`/api/sim/draft-lifecycles/${encodeURIComponent(draft.task_id)}/operations/${encodeURIComponent(draft.operation_id)}/end-intent`,{contract_version:2,scope:'MOCK_DRAFT_LIFECYCLE_ONLY',acknowledgement:'END_THIS_PROVEN_UNPAID_INTENT_ONLY',task_revision:draft.task_revision,operation_revision:draft.operation_revision,plan_version:draft.plan_version,facts_version:draft.facts_version}));await recoverPayment(draft.task_id);
                })}>放弃这份未付款草案</button>
                {!draft.intent_expires_at&&<p>尚未生成确认期限；可明确放弃，原号记录保留。</p>}
                <label>改办金额（元，保留两位小数；沿用本草案账户和收款人）<input aria-label="改办转账金额" inputMode="decimal" value={replacementAmount} onChange={e=>setReplacementAmount(e.target.value)} disabled={disable}/></label>
                <button disabled={disable||!paymentSelectionVerified} onClick={()=>void run(async()=>{
                const replacement=replacementIntent(lifecycleSchema.parse(draft),replacementAmount);
                const ended=intentTerminationSchema.parse(await call(`/api/sim/draft-lifecycles/${encodeURIComponent(draft.task_id)}/operations/${encodeURIComponent(draft.operation_id)}/replace-intent`,{contract_version:2,scope:'MOCK_DRAFT_LIFECYCLE_ONLY',acknowledgement:'REPLACE_UNEXECUTED_INTENT_ONLY',task_revision:draft.task_revision,operation_revision:draft.operation_revision,plan_version:draft.plan_version,facts_version:draft.facts_version,replacement}));
                if(ended.replacement?.task_id!==replacement.task_id)throw Error('替代任务原号不匹配');
                await recoverPayment(replacement.task_id);setReplacementAmount('');
              })}>放弃此未付款草案，改办新转账</button></>}
              {draft.execution_status === 'RESULT_UNKNOWN' && (
                <p>
                  查询已成功，但原操作结果仍未知。
                  {draft.action === 'CREATE_MOCK_DRAFT'
                    ? '未知发生在创建模拟草稿阶段，尚无可核对草稿；不代表付款已提交或失败。'
                    : '原业务结果尚未核清。'}
                  {paymentClosed?'已明确关闭此非资金草案；原 UNKNOWN 仍保留。全部原单核清后，可另行创建新草案。':'重复查询不等于恢复。只有证据完整、确认已过期的初始非资金草案可明确关闭；资金未知仍不可重办。'}
                </p>
              )}
              {draft.execution_status==='RESULT_UNKNOWN'&&draft.action==='CREATE_MOCK_DRAFT'&&!paymentClosed&&<button disabled={disable||!paymentSelectionVerified} onClick={()=>void run(async()=>{
                const v=await call(`/api/sim/draft-lifecycles/${encodeURIComponent(draft.task_id)}/operations/${encodeURIComponent(draft.operation_id)}/close-nonfunding`,{
                  contract_version:2,scope:'MOCK_DRAFT_LIFECYCLE_ONLY',acknowledgement:'PRESERVE_UNKNOWN_CLOSE_NONFUNDING_DRAFT',
                  task_revision:draft.task_revision,operation_revision:draft.operation_revision,plan_version:draft.plan_version,facts_version:draft.facts_version,
                });
                if(v.task_id!==draft.task_id||v.operation_id!==draft.operation_id||v.closed_nonfunding_draft!==true||v.original_result!=='UNKNOWN'||v.original_preserved!==true||v.funding_authorized!==false)throw Error('关闭证明不完整');
                await recoverPayment(draft.task_id);
              })}>保留未知原件，关闭这份非资金草案</button>}
              {draft.next_action?.kind === 'RECONFIRM_RECIPIENT' && (
                <>
                  <p>收款资料已更新，旧确认不能继续使用。</p>
                  <button
                    disabled={disable || paymentReadOnly}
                    onClick={() => void run(refreshDraftRecipient)}
                  >
                    更新收款资料并重新核对
                  </button>
                </>
              )}
              {draft.execution_status === 'SUCCEEDED' &&
                draft.next_action?.kind === 'SELECT_ACTION' &&
                draft.action !== 'CANCEL_MOCK_DRAFT' &&
                !payment && (
                  <button
                    disabled={disable || paymentReadOnly}
                    onClick={() =>
                      void run(async () => {
                        const cancel = await selectSavedDraftCancellation(
                          lifecycleSchema.parse(draft),
                          requests.current.signal,
                        );
                        const prepared = await prepareAgent(cancel, requests.current.signal);
                        if (prepared.kind === 'DRAFT') setDraft(prepared.draft);
                      })
                    }
                  >
                    核对并准备取消未付款草案
                  </button>
                )}
              <div className="bank-actions">
                <button
                  disabled={
                    disable ||
                    paymentReadOnly ||
                    draft.next_action?.kind !== 'PREPARE_OR_REPLACE_ACTION'
                  }
                  onClick={() => void run(prepareDraft)}
                >
                  准备草稿确认
                </button>
                <button
                  disabled={disable || paymentReadOnly || draft.next_action?.kind !== 'CONFIRM'}
                  onClick={() => void run(confirmDraft)}
                >
                  {draft.action === 'CANCEL_MOCK_DRAFT' ? '确认取消草案' : '确认创建草稿'}
                </button>
                <button
                  disabled={disable || paymentReadOnly || draft.next_action?.kind !== 'EXECUTE'}
                  onClick={() => void run(executeDraft)}
                >
                  {draft.action === 'CANCEL_MOCK_DRAFT' ? '执行已确认的草案取消' : '保存模拟草稿'}
                </button>
                <button
                  disabled={
                    disable ||
                    paymentReadOnly ||
                    draft.execution_status !== 'SUCCEEDED' ||
                    draft.action === 'CANCEL_MOCK_DRAFT' ||
                    draft.task_state === 'CANCELLED' ||
                    !!saved.preparation
                  }
                  onClick={() => void run(preparePayment)}
                >
                  准备付款
                </button>
              </div>
            </>
          )}
          {payment && (
            <>
              <h3>
                2. 原付款准备资料
                {paymentReadOnly ? '（历史，只读）' : '与确认'}
              </h3>
              <Info
                rows={[
                  ['付款账户', payment.snapshot?.display_details?.account?.display_name],
                  ['付款卡号', payment.snapshot?.display_details?.account?.masked_account_no],
                  ['收款人', payment.snapshot?.display_details?.payee?.display_name],
                  ['收款账号', payment.snapshot?.display_details?.payee?.masked_account_no],
                  ['转账金额（元）', payment.snapshot?.display_details?.amount],
                  ['备注', payment.snapshot?.display_details?.remark || '无'],
                ]}
              />
              <PaymentRiskNotice decisions={payment.security_decisions} />
              <p>
                付款进度{' '}
                {funds && !['UNSUBMITTED', 'REGISTERED_UNSUBMITTED'].includes(funds.state)
                  ? state(funds.state)
                  : verified
                    ? '已验证，待提交'
                    : payment.confirmation_status === 'CONFIRMED'
                      ? '已确认，待验证'
                      : '待确认'}{' '}
                · 原确认记录 {state(payment.confirmation_status)}
              </p>
              <div className="bank-actions">
                <button
                  disabled={
                    disable || paymentReadOnly || payment.confirmation_status === 'CONFIRMED'
                  }
                  onClick={() => void run(confirmPayment)}
                >
                  确认此笔付款
                </button>
                <button
                  disabled={disable || paymentReadOnly}
                  onClick={() =>
                    void run(async () => {
                      setDelegation(await background({ kind: 'DISPLAY' }));
                      setDelegationConfirmed(false);
                      setVerified(false);
                    })
                  }
                >
                  查看独立后台委托
                </button>
              </div>
            </>
          )}
          {delegation && !paymentReadOnly && (
            <>
              <h3>3. 明确委托与模拟验证</h3>
              <p>{delegation.display?.explicit_text}</p>
              <Info
                rows={[
                  ['委托时长（分钟）', delegation.display?.duration_minutes],
                  ['委托范围', '仅此原单；未完成时可在委托期内继续办理'],
                ]}
              />
              <div className="bank-actions">
                <button
                  disabled={disable}
                  onClick={() =>
                    void run(async () => {
                      await background({
                        kind: 'CONFIRM',
                        shown: delegation,
                      });
                      setDelegationConfirmed(true);
                      setCode('');
                    })
                  }
                >
                  明确同意此笔后台委托
                </button>
              </div>
              {delegationConfirmed && (
                <DemoVerification
                  key={delegation.consent_id}
                  operation={{
                    kind: 'PAYMENT',
                    task_id: payment!.task_id,
                    operation_id: payment!.operation.operation_id,
                    consent_id: delegation.consent_id,
                  }}
                  disabled={disable || paymentReadOnly}
                />
              )}
              <label>
                付款专用模拟验证码
                <input value={code} autoComplete="off" onChange={(e) => setCode(e.target.value)} />
              </label>
              <button
                disabled={disable || !code}
                onClick={() =>
                  void run(async () => {
                    const v = await background({
                      kind: 'VERIFY_CODE',
                      consent_id: delegation.consent_id,
                      code,
                    });
                    setCode('');
                    setVerified(v.verified === true);
                    if (v.verified !== true) throw Error('本次验证码未通过，请查询原付款状态，不要另建重付');
                  })
                }
              >
                核验付款验证码
              </button>
              <button
                className="primary"
                disabled={disable || !verified}
                onClick={() =>
                  void run(async () => {
                    await background({
                      kind: 'EXECUTE',
                      consent_id: delegation.consent_id,
                      confirmation_ref: payment!.confirmation_ref,
                      display_ticket: payment!.display_ticket,
                    });
                    setVerified(false);
                    await recoverPayment();
                  })
                }
              >
                提交{payment!.snapshot.transfer.amount.replace(/\.00$/, '')}元模拟付款
              </button>
            </>
          )}
          {funds && (
            <>
              <h3>当前原付款结果</h3>
              <Info
                rows={[
                  ['办理状态', state(funds.state)],
                  ['转账金额（元）', funds.transfer?.amount],
                  ['自动重新付款', '不允许；结果未明时按原号查询'],
                ]}
              />
              <p>处理中或结果未明时保留原号，不再创建付款。显示余额只在服务端查回后刷新。</p>
              {saved.preparation && !paymentTerminal && funds.submitted === true && (
                <button
                  disabled={disable}
                  onClick={() =>
                    void run(async () => {
                      const p = currentPayment();
                      const v = fundsResultSchema.parse(
                        await call(path(p.task_id, p.operation.operation_id) + '/funds-result'),
                      );
                      if (!fundsMatches(p, v)) throw Error('资金回执与原付款不匹配');
                      const op = v.execution_operation_id;
                      if (!op) throw Error('尚无已受理执行号');
                      await background({
                        kind: 'REVOKE',
                        execution_operation_id: op,
                      });
                      await recoverPayment();
                    })
                  }
                >
                  撤销原后台续办
                </button>
              )}
            </>
          )}
        </section>
      )}
      {auth && businessPage === 'bills' && (
        <>
          <section className="bank-panel">
            <h2>月度消费分析</h2>
            <p>固定演示截止 2026-10-27 00:00（上海）。退款按发生期列示，净额保留正负。</p>
            <div className="bank-actions">
              <button className="primary" disabled={disable} onClick={() => void run(loadReport)}>
                读取三窗月度账单
              </button>
              <button
                disabled={disable || !saved.reportTask}
                onClick={() => void run(recoverReport)}
              >
                读取原报告
              </button>
            </div>
            {monthly && (
              <>
                <p className="bank-ref">
                  报告 {monthly.metadata.report_id}
                  <br />
                  反馈使用本期报告 {monthly.current?.metadata.report_id}
                </p>

                <section className="bank-grid">
                  {(['current', 'previous'] as const).map((period) => (
                    <section className="bank-stat" key={period}>
                      <p>{period === 'current' ? '本期' : '上期'}净消费</p>
                      <h2>{text(monthly.comparison?.[period]?.net)}</h2>
                      <p>
                        毛额 {text(monthly.comparison?.[period]?.gross)} · 退款{' '}
                        {text(monthly.comparison?.[period]?.refunds)}
                      </p>
                    </section>
                  ))}
                </section>
                <p>
                  变化{' '}
                  {monthly.comparison?.percent == null
                    ? '不计算百分比 · ' + text(monthly.comparison?.percent_reason)
                    : monthly.comparison.percent + '%'}{' '}
                  · {state(monthly.status)}
                </p>
                <Info
                  rows={(['current', 'previous', 'historical'] as const).map((period) => [
                    {
                      current: '本期',
                      previous: '上期',
                      historical: '历史比较期',
                    }[period],
                    `${when(monthly.plan[period].start_inclusive)} 至 ${when(monthly.plan[period].end_exclusive)}（不含结束时刻）${period === 'historical' ? '' : ' · 26天'}`,
                  ])}
                />
                <h3>异常线索</h3>
                {monthly.anomalies?.map((a: View, i: number) => (
                  <p key={i}>
                    {state(a.status)} · ¥{a.amount} · {a.message}
                  </p>
                ))}
                <p>
                  未触发不代表安全；证据不足和不可用保留原状态。已保存反馈可在原记录下更正、撤销或重新提交，每次都须重新确认；保留全部历史，不撤销交易，也不授权冻结。
                </p>
                <h3>本期原交易反馈</h3>
                {report?.report.evidence?.map((row: View, i: number) => (
                  <div className="bank-row" key={i}>
                    <span>
                      {when(row.transaction.completed_at)} · {text(row.transaction.amount)}
                    </span>
                    {(
                      [
                        ['MINE', '本人交易'],
                        ['NOT_MINE', '非本人交易'],
                        ['UNSURE', '不确定'],
                      ] as const
                    ).map(([choice, label]) => (
                      <button
                        key={choice}
                        disabled={disable || !!saved.feedbackNonce}
                        onClick={() => void run(() => prepareFeedback(row.reference, choice))}
                      >
                        {label}
                      </button>
                    ))}
                  </div>
                ))}
              </>
            )}
          </section>
          <section className="bank-panel">
            <h2>反馈确认与原号恢复</h2>
            {feedback && (
              <>
                <p>
                  确认将所选本期交易标记为“
                  {feedback.parameters.choice === 'MINE'
                    ? '本人'
                    : feedback.parameters.choice === 'NOT_MINE'
                      ? '非本人'
                      : '不确定'}
                  ”。该判断仅保存本地，不代表银行争议处理或冻结授权。
                </p>
                {feedbackEvidence ? (
                  <Info
                    rows={[
                      ['交易日期', when(feedbackEvidence.transaction.completed_at)],
                      ['交易金额', text(feedbackEvidence.transaction.amount)],
                      ['商户标识', text(feedbackEvidence.transaction.merchant_id)],
                    ]}
                  />
                ) : (
                  <p>
                    这笔交易的日期、金额与商户尚未重新取得。请先读取原报告核对，不能仅凭参考号确认。
                  </p>
                )}
                <p className="bank-ref">原反馈参考号 {saved.feedbackNonce}</p>
                <button
                  disabled={disable || feedbackConfirmed || !feedbackEvidence}
                  onClick={() =>
                    void run(async () => {
                      await feedbackWire((signal) => recoveryConfirm(feedback, signal));
                      setFeedbackConfirmed(true);
                    })
                  }
                >
                  确认保存本人判断
                </button>
                <button
                  disabled={disable || !feedbackConfirmed || !feedbackEvidence}
                  onClick={() =>
                    void run(async () => {
                      setFeedbackResult(
                        await feedbackWire((signal) => recoveryExecute(feedback, signal)),
                      );
                      setFeedback(undefined);
                      setFeedbackConfirmed(false);
                    })
                  }
                >
                  保存反馈
                </button>
              </>
            )}
            <button
              disabled={disable || !saved.feedbackNonce}
              onClick={() => void run(recoverFeedback)}
            >
              只读查询原反馈
            </button>
            {feedbackResult != null && (
              <>
                <p className="bank-result">
                  反馈
                  {feedbackState(feedbackResult as View)}
                </p>
                <p>此反馈不授权冻结，也未向真实银行发起操作。</p>
                <p className="bank-ref">原反馈参考号 {saved.feedbackNonce}</p>
                {(feedbackResult as View).state === 'SAVED' && (
                  <FeedbackHistory key={`${(feedbackResult as View).operation_id}`} root={(feedbackResult as View).operation_id} disabled={disable} />
                )}
                {(feedbackResult as View).state === 'SAVED' && (
                  <button
                    disabled={disable}
                    onClick={() => {
                      save({ feedbackNonce: undefined, feedbackRealm: undefined });
                      setFeedbackResult(undefined);
                    }}
                  >
                    反馈已保存，准备下一条
                  </button>
                )}
              </>
            )}
            {!!saved.feedbackHistory?.length && (
              <details>
                <summary>反馈原号记录</summary>
                {saved.feedbackHistory.map((r) => (
                  <button
                    key={r.nonce}
                    disabled={disable}
                    onClick={() =>
                      void run(async () => {
                        save({ feedbackNonce: r.nonce, feedbackRealm: r.realm });
                        await recoverFeedback();
                      })
                    }
                  >
                    只读查询反馈 {r.nonce}
                  </button>
                ))}
              </details>
            )}
          </section>
        </>
      )}
      {auth && businessPage === 'cards' && (
        <section className="bank-panel">
          <h2>本人银行卡</h2>
          <button disabled={disable} onClick={() => void run(readCards)}>
            查询银行卡当前状态
          </button>
          <label>
            选择银行卡
            <select
              value={selectedCard}
              disabled={disable || !!saved.cardNonce}
              onChange={(e) => setSelectedCard(e.target.value)}
            >
              {cardCatalog.map((c) => (
                <option key={c.card_id} value={c.card_id}>
                  {c.account_display_name} · 尾号{c.last_four}
                </option>
              ))}
            </select>
          </label>
          {cardRead && (
            <div>
              {cardRead.cards?.map((c: View) => (
                <p key={c.position}>
                  尾号{c.last_four} · {c.card_locked ? '已冻结' : '未发现有效锁'} ·{' '}
                  {c.public_reasons?.join('、')}
                </p>
              ))}
              <p>
                {cardRead.account_outgoing_restricted
                  ? '关联账户出账受限'
                  : '本次读取未发现卡锁导致的账户出账限制'}
              </p>
              <p>{cardRead.unlock_instruction}</p>
            </div>
          )}
          <p>本人主动冻结。独立确认及卡片模拟验证码不复用付款授权。</p>
          <button
            className="primary"
            disabled={disable || !!saved.cardNonce || !selectedCard}
            onClick={() => void run(prepareCard)}
          >
            准备本人主动冻结
          </button>
          <button disabled={disable || !saved.cardNonce} onClick={() => void run(recoverCard)}>
            查询原卡操作
          </button>
          {card && (
            <>
              <Info
                rows={[
                  ['银行卡', '尾号 ' + text(card.parameters?.display?.last_four)],
                  ['冻结原因', card.parameters?.display?.reason],
                  ['冻结影响', card.parameters?.display?.impact],
                  ['解除方式', card.parameters?.display?.unlock_instruction],
                ]}
              />
              <p>
                原操作 {card.operation_id} ·{' '}
                {cardResult
                  ? cardResult.original_lock_active === true &&
                    cardResult.account_outgoing_restricted === true
                    ? '冻结已生效'
                    : state(cardResult.state)
                  : cardVerified
                    ? '已验证，待冻结'
                    : cardConfirmed
                      ? '已确认，待验证'
                      : state(card.state)}
              </p>
              <button
                disabled={disable || !!cardResult || cardConfirmed}
                onClick={() =>
                  void run(async () => {
                    setCard(
                      await call('/api/card-safety', {
                        kind: 'CONFIRM',
                        operation_id: card.operation_id,
                        confirmation_ref: card.confirmation_ref,
                      }),
                    );
                    setCardConfirmed(true);
                  })
                }
              >
                确认冻结此卡
              </button>
              {cardConfirmed && !cardResult && (
                <DemoVerification
                  key={card.operation_id}
                  operation={{ kind: 'CARD', operation_id: card.operation_id }}
                  disabled={disable}
                />
              )}
              <p>
                在演示验证区域领取后，手动输入卡片专用挑战号与验证码；本机环境仍可使用私有控制台。
              </p>
              <label>
                卡片挑战号
                <input value={challenge} onChange={(e) => setChallenge(e.target.value)} />
              </label>
              <label>
                卡片模拟验证码
                <input value={code} onChange={(e) => setCode(e.target.value)} autoComplete="off" />
              </label>
              <button
                disabled={disable || !!cardResult || !cardConfirmed || !challenge || !code}
                onClick={() =>
                  void run(async () => {
                    const v = await call('/api/card-safety', {
                      kind: 'VERIFY_CODE',
                      operation_id: card.operation_id,
                      challenge_id: challenge,
                      code,
                    });
                    setCardVerified(v.verified === true);
                    setCode('');
                  })
                }
              >
                核验卡片验证码
              </button>
              <button
                className="primary"
                disabled={disable || !!cardResult || !cardVerified}
                onClick={() =>
                  void run(async () => {
                    await call('/api/card-safety', {
                      kind: 'ISSUE_PERMIT',
                      operation_id: card.operation_id,
                    });
                    setCardResult(
                      await call('/api/card-safety', {
                        kind: 'EXECUTE_FREEZE',
                        operation_id: card.operation_id,
                      }),
                    );
                    setCardVerified(false);
                    setCardResult(
                      await call('/api/card-safety', {
                        kind: 'QUERY_ORIGINAL',
                        operation_id: card.operation_id,
                      }),
                    );
                    await readCards();
                    await balances();
                  })
                }
              >
                执行本人主动冻结
              </button>
            </>
          )}
          {cardResult && (
            <>
              <h3>原冻结结果</h3>
              <p className="bank-result">
                {cardResult.original_lock_active === true &&
                cardResult.account_outgoing_restricted === true
                  ? '本机模拟冻结已生效，关联账户出账受限'
                  : state(cardResult.state)}
              </p>
              <Info
                rows={[
                  ['记录时间', when(cardResult.occurred_at)],
                  ['原操作参考号', cardResult.operation_id],
                ]}
              />
            </>
          )}
        </section>
      )}
      {auth && businessPage === 'subscriptions' && (
        <>
          <section className="bank-panel">
            <h2>订阅与提醒</h2>
            {!ledger && !subscriptionReadNotice && <p>尚未读取订阅记录，请点击“读取本人订阅”。</p>}
            {ledger && Array.isArray(ledger.items) && ledger.items.length === 0 && (
              <p role="status">已成功读取，当前暂无已管理订阅。可以识别候选后逐项确认登记。</p>
            )}
            <p>取消意图仅记入本地；商户退订请到官方渠道。模拟提醒不表示真实通知已发送。</p>
            <div className="bank-actions">
              <button disabled={disable} onClick={() => void run(subscriptions)}>
                读取本人订阅
              </button>
              <button
                disabled={disable}
                onClick={() =>
                  void run(async () => {
                    setCapture(undefined);
                    setCapture(
                      await call('/api/subscriptions/change', {
                        kind: 'CAPTURE',
                      }),
                    );
                    setSubscriptionReadNotice('');
                  })
                }
              >
                识别可登记订阅
              </button>
              <button
                disabled={disable}
                onClick={() =>
                  void run(async () =>
                    setAttempts(
                      await call('/api/subscriptions', {
                        kind: 'REMINDER_ATTEMPTS',
                      }),
                    ),
                  )
                }
              >
                <Bell size={16} />
                查询模拟提醒原尝试
              </button>
            </div>
            {subscriptionReadNotice && !subResult && <p role="status">{subscriptionReadNotice}</p>}
            {capture?.preview?.recognition?.groups?.map((g: View, i: number) => (
              <div className="bank-row" key={i}>
                <span>
                  <strong>{subscriptionName(g, capture)}</strong>
                  <p>
                    {subscriptionFee(subscriptionMetadata(g, capture))} ·{' '}
                    {subscriptionPeriod(subscriptionMetadata(g, capture))}
                  </p>
                  <p>
                    下次扣款：{subscriptionDate(subscriptionMetadata(g, capture)?.next_charge_at)} ·
                    到期：{subscriptionDate(subscriptionMetadata(g, capture)?.expires_at)}
                  </p>
                  <br />
                  <span className="bank-subscription-status">
                    {subscriptionStatus(g, true, capture)}
                  </span>
                  <details>
                    <summary>查看识别依据</summary>
                    <p>
                      {g.charges
                        ?.map(
                          (charge: View) =>
                            `${subscriptionDate(charge.completed_at)} · ¥${(charge.amount_minor_units / 100).toFixed(2)}`,
                        )
                        .join(' / ')}
                    </p>
                    <p>{g.message}</p>
                  </details>
                </span>
                <button
                  aria-label={`准备登记候选${g.selection_index + 1}`}
                  disabled={
                    disable ||
                    !!saved.subscriptionOperation ||
                    (saved.subscriptionNonce &&
                      saved.subscriptionPending?.request.candidate_index !== g.selection_index) ||
                    g.disposition !== 'NOT_IN_LEDGER'
                  }
                  onClick={() =>
                    void run(() =>
                      prepareSubscription({
                        kind: 'REGISTRATION',
                        capture_ref: capture.capture_ref,
                        candidate_index: g.selection_index,
                      }),
                    )
                  }
                >
                  准备登记
                </button>
              </div>
            ))}
            {ledger?.items?.map((item: View, i: number) => (
              <div className="bank-subscription" key={i}>
                <h3>{subscriptionName(item)}</h3>
                <Info
                  rows={[
                    ['状态', subscriptionStatus(item)],
                    ['本地状态', state(item.current_state)],
                    ['月费', subscriptionFee(subscriptionMetadata(item))],
                    ['扣款周期', subscriptionPeriod(subscriptionMetadata(item))],
                    ['下次扣款日期', subscriptionDate(subscriptionMetadata(item)?.next_charge_at)],
                    ['到期日期', subscriptionDate(subscriptionMetadata(item)?.expires_at)],
                    ['提醒设置', state(item.reminder_state)],
                    ...(item.reminder
                      ? ([
                          ['预计扣费日期', subscriptionDate(item.reminder.expected_date)],
                          ['提前提醒天数', item.reminder.days_before],
                        ] as Array<[string, unknown]>)
                      : []),
                  ]}
                />
                <p>{item.message}</p>
                <div className="bank-actions">
                  <button
                    disabled={disable || !!saved.subscriptionNonce}
                    onClick={() =>
                      void run(() =>
                        prepareSubscription({
                          kind: 'REMINDER_SETTINGS',
                          selection_ref: item.selection_ref,
                          enabled: true,
                          days_before: 1,
                        }),
                      )
                    }
                  >
                    设置提前1天提醒
                  </button>
                  <button
                    disabled={disable || !!saved.subscriptionNonce}
                    onClick={() =>
                      void run(() =>
                        prepareSubscription({
                          kind: 'RECORD_CANCELLATION',
                          selection_ref: item.selection_ref,
                        }),
                      )
                    }
                  >
                    记录取消意图
                  </button>
                  <button
                    disabled={disable || !!saved.subscriptionNonce}
                    onClick={() =>
                      void run(() =>
                        prepareSubscription({
                          kind: 'REVOKE_CANCELLATION',
                          selection_ref: item.selection_ref,
                        }),
                      )
                    }
                  >
                    撤回取消意图
                  </button>
                </div>
              </div>
            ))}
            {sub && (
              <>
                <h3>核对安排后确认</h3>
                <p className="bank-ref">
                  办理编号{' '}
                  {saved.subscriptionNonce
                    ? subscriptionRecordId(saved.subscriptionNonce)
                    : '待确认'}
                </p>
                <Arrangement value={sub.summary} />
                <button
                  disabled={disable || subConfirmed}
                  onClick={() =>
                    void run(async () => {
                      await call('/api/subscriptions/change', {
                        kind: 'CONFIRM',
                        task_id: sub.task_id,
                        confirmation_ref: sub.confirmation_ref,
                        explicitly_confirm: true,
                      });
                      setSubConfirmed(true);
                    })
                  }
                >
                  确认此项订阅操作
                </button>
                <button
                  className="primary"
                  disabled={disable || !subConfirmed}
                  onClick={() =>
                    void run(async () => {
                      setSubResult(
                        await call('/api/subscriptions/change', {
                          kind: 'EXECUTE',
                          task_id: sub.task_id,
                          operation_id: sub.operation_id,
                          confirmation_ref: sub.confirmation_ref,
                        }),
                      );
                      setSub(undefined);
                      setSubConfirmed(false);
                      await refreshSubscriptionResultViews();
                    })
                  }
                >
                  保存订阅操作
                </button>
              </>
            )}
            {subscriptionError && (
              <p className="bank-result" role="alert">
                订阅准备未完成：
                {subscriptionError.includes('NOT_FOUND')
                  ? '服务端本次查询未找到原号；此前准备错误和候选已保留，可重新读取后重试。'
                  : subscriptionError === 'DEPENDENCY_UNAVAILABLE'
                    ? '依赖暂不可用，已保留候选和原操作上下文；恢复后可重试。'
                    : '服务暂不可用，已保留候选和原操作上下文；请按原号查询或稍后重试。'}
              </p>
            )}
            {saved.subscriptionPending && (
              <>
                <p className="bank-ref">
                  待核对候选：{text(saved.subscriptionPending.label ?? '服务待确认')} ·{' '}
                  {subscriptionRecordId(saved.subscriptionPending.nonce)}
                </p>
                <button
                  disabled={disable || !!saved.subscriptionOperation}
                  onClick={() =>
                    void run(() => prepareSubscription(saved.subscriptionPending!.request))
                  }
                >
                  核对原号并重新准备同一候选
                </button>
              </>
            )}
            <button
              disabled={disable || !saved.subscriptionNonce}
              onClick={() => void run(recoverSub)}
            >
              查询办理结果
            </button>
            {subResult && (
              <>
                {subscriptionReadNotice && <p role="status">{subscriptionReadNotice}</p>}
                <p className="bank-result">{subResult.original?.message || '原操作结果待核对'}</p>
                {canLeaveSubscriptionPreparation(subResult, saved.subscriptionOperation) && (
                  <p role="status">这份旧准备已因服务重启失效，未提交执行。原记录继续保留；可明确结束当前选择后重新识别，重新准备和确认。</p>
                )}
                <p className="bank-ref">
                  办理编号{' '}
                  {saved.subscriptionNonce
                    ? subscriptionRecordId(saved.subscriptionNonce)
                    : '待确认'}
                </p>
                {subResult.confirmed_arrangement && (
                  <Arrangement value={subResult.confirmed_arrangement} />
                )}
                <button
                  disabled={disable || (subResult.domain_audit?.status !== 'VERIFIED_CANDIDATE_V1' && !canLeaveSubscriptionPreparation(subResult, saved.subscriptionOperation))}
                  onClick={() =>
                    void run(async () => {
                      const original = refs.current.subscriptionOperation;
                      const nonce = refs.current.subscriptionNonce;
                      if (!original) throw Error('缺少原办理号，请先查询');
                      const fresh = await call('/api/subscriptions/change', {kind: 'QUERY', operation_id: original});
                      if (refs.current.subscriptionOperation !== original || refs.current.subscriptionNonce !== nonce)
                        throw Error('当前选择已变化，请重新核对');
                      if (fresh.domain_audit?.status !== 'VERIFIED_CANDIDATE_V1' && !canLeaveSubscriptionPreparation(fresh, original))
                        throw Error('原操作尚未核清，原记录已保留');
                      save({
                        subscriptionNonce: undefined,
                        subscriptionOperation: undefined,
                        subscriptionPending: undefined,
                        subscriptionError: undefined,
                      });
                      setSubscriptionError('');
                      setSubResult(undefined);
                    })
                  }
                >
                  {canLeaveSubscriptionPreparation(subResult, saved.subscriptionOperation) ? '保留失效原记录，重新选择' : '已核对记录，准备下一项'}
                </button>
              </>
            )}
            {saved.subscriptionHistory !== undefined && (
              <details>
                <summary>办理记录</summary>
                {subscriptionHistory(saved.subscriptionHistory).map((r) => (
                  <button
                    key={r.nonce}
                    disabled={disable}
                    onClick={() =>
                      void run(async () => {
                        save({ subscriptionNonce: r.nonce, subscriptionOperation: undefined });
                        await recoverSub();
                      })
                    }
                  >
                    查询办理结果 · {subscriptionRecordId(r.nonce)}
                  </button>
                ))}
                {(!Array.isArray(saved.subscriptionHistory) ||
                  subscriptionHistory(saved.subscriptionHistory).length !==
                    saved.subscriptionHistory.length) && (
                  <p>部分旧记录格式待核对，原始记录已保留；仅可查询有完整原编号的记录。</p>
                )}
              </details>
            )}
            {attempts && (
              <>
                <h3>持久模拟渠道原记录</h3>
                {attempts.items?.length === 0 ? (
                  <p>当前没有持久模拟提醒记录。尚未发送真实通知。</p>
                ) : (
                  attempts.items?.map((attempt: View) => (
                    <Info
                      key={attempt.attempt_ref}
                      rows={[
                        ['原尝试参考号', attempt.attempt_ref],
                        ['模拟渠道状态', state(attempt.channel_state)],
                        ['保存时间', when(attempt.persisted_at)],
                        ['真实通知', '未发送'],
                      ]}
                    />
                  ))
                )}
              </>
            )}
          </section>
        </>
      )}
    </>
  );
  return (
    <div className={'bank-app' + (auth && page === 'agent' ? ' bank-conversation-page' : '')}>
      <aside className="bank-sidebar">
        <a className="bank-brand" href="#home">
          <Building2 />
          <strong>
            SafeFlow <small>本机模拟银行</small>
          </strong>
        </a>
        <nav>
          {tabs.map(([id, label, Icon]) => (
            <button
              key={id}
              className={page === id ? 'selected' : ''}
              onClick={() => {
                if (id !== page) navigationEpoch.current++;
                setPage(id);
                setNotice('');
                setDiagnostic('');
                if (id === 'transfer' && page !== 'transfer') void run(() => recoverPayment());
                window.location.hash = id;
              }}
            >
              <Icon size={19} />
              {label}
            </button>
          ))}
        </nav>
        <div className="bank-trust">
          <ShieldCheck />
          <p>
            操作由服务端核验
            <br />
            仅使用虚构资料与模拟渠道
          </p>
        </div>
      </aside>
      <main className="bank-main">
        <header className="bank-top">
          <span>个人银行 / {tabs.find((t) => t[0] === page)?.[1]}</span>
          <div>
            <span className="bank-pill">隔离演示</span>
            {auth && (
              <button disabled={busy} onClick={() => void run(signOut)}>
                <LogOut size={16} />
                退出
              </button>
            )}
          </div>
        </header>
        <section className="bank-content">
          <div className="bank-title">
            <div>
              <p className="bank-eyebrow">YOUR MONEY, UNDER YOUR CONTROL</p>
              <h1>{tabs.find((t) => t[0] === page)?.[1]}</h1>
              <p>每笔操作明确确认，办理结果按原号查询。</p>
            </div>
            <ShieldCheck className="bank-title-icon" />
          </div>
          {!auth ? (
            <section className="bank-panel bank-login">
              <h2>登录本机模拟银行</h2>
              <p>使用启动时设置的本机口令。此处不会连接真实银行。</p>
              <form
                onSubmit={(e) => {
                  e.preventDefault();
                  void run(signIn);
                }}
              >
                <label>
                  模拟登录口令
                  <input
                    type="password"
                    autoComplete="current-password"
                    value={secret}
                    onChange={(e) => setSecret(e.target.value)}
                  />
                </label>
                <button className="primary" disabled={busy || !secret}>
                  登录
                </button>
              </form>
            </section>
          ) : null}
          <p className="bank-notice" role="status">
            {busy && <RefreshCw size={16} />} {notice}
          </p>
          {diagnostic && (
            <details className="bank-diagnostic">
              <summary>诊断信息</summary>
              <code>{diagnostic}</code>
            </details>
          )}
          <RequestActivity />
          {auth && page === 'agent' && (
            <BankAgent
              accounts={(overview?.accounts || []).map((v: View) => ({
                account_id: String(v.account.account_id),
                display_name: String(v.account.display_name),
              }))}
              disabled={disable}
              key={epoch.current}
              run={run}
              businessPanel={agentService ? businessPanels : null}
              businessPanelService={
                agentService === 'bills'
                  ? 'BILLS'
                  : agentService === 'cards'
                    ? 'CARDS'
                    : agentService === 'subscriptions'
                      ? 'SUBSCRIPTIONS'
                      : undefined
              }
              openService={openAgentService}
            />
          )}
          {page !== 'agent' && businessPanels}
        </section>
        <footer>SafeFlow · 新建隔离演示候选 · 数据与办理结果以服务端查回为准</footer>
      </main>
    </div>
  );
}
