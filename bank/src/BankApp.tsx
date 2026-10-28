import { useEffect, useRef, useState } from 'react';
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
} from './runtime/client';
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
// Wire views come only from checked backend routes. Browser storage contains
// selectors, never authorization, balances, confirmation tickets or results.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
type View = Record<string, any>;
type Selectors = {
  paymentNonce?: string;
  paymentTask?: string;
  preparation?: string;
  execution?: string;
  cardNonce?: string;
  cardOperation?: string;
  subscriptionNonce?: string;
  subscriptionOperation?: string;
  reportTask?: string;
  feedbackNonce?: string;
  feedbackRealm?: string;
};
const key = 'safeflow.bank.selectors.v1';
function load(): Selectors {
  try {
    return JSON.parse(localStorage.getItem(key) || '{}');
  } catch {
    return {};
  }
}
function persist(v: Selectors) {
  localStorage.setItem(key, JSON.stringify(v));
  if (localStorage.getItem(key) !== JSON.stringify(v))
    throw Error('无法保存原号，已阻止写入。');
}
const tabs = [
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
const life = (v: View) =>
  `/api/sim/draft-lifecycles/${v.task_id}/operations/${v.operation_id}`;
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
function when(v: unknown) {
  if (typeof v !== 'string') return '未提供';
  const date = new Date(v);
  return Number.isFinite(date.getTime())
    ? date.toLocaleString('zh-CN', { timeZone: 'Asia/Shanghai', hour12: false })
    : '时间待核对';
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
          ...(value.service
            ? [['服务名称', value.service] as [string, unknown]]
            : []),
          ...(value.schedule
            ? ([
                ['预计扣费日期', value.schedule.expected_date],
                ['提醒日期', value.reminder_date],
                ['提前天数', value.schedule.days_before],
                ['时区', '北京时间'],
                ['时间承诺', '仅按日期安排，不承诺精确发送时刻'],
              ] as Array<[string, unknown]>)
            : []),
        ]}
      />
      {value.candidate?.charges?.map((c: View, i: number) => (
        <p key={i}>
          {c.completed_at?.slice(0, 10)} · ¥
          {(c.amount_minor_units / 100).toFixed(2)}
        </p>
      ))}
      <p>{value.message || '仅保存本机模拟设置，不表示真实通知已经发送。'}</p>
    </>
  );
}
export default function BankApp() {
  const [page, setPage] = useState(
      () => window.location.hash.slice(1) || 'home',
    ),
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
    [attempts, setAttempts] = useState<View>();
  const refs = useRef<Selectors>(load()),
    epoch = useRef(0),
    locked = useRef(false),
    [saved, setSaved] = useState(refs.current);
  function save(v: Partial<Selectors>) {
    const next = { ...refs.current, ...v };
    persist(next);
    refs.current = next;
    setSaved(next);
  }
  function clearViews() {
    epoch.current++;
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
    setSecret('');
    setCode('');
    setChallenge('');
  }
  async function call(p: string, body?: unknown): Promise<View> {
    const value = await request(p, body === undefined ? 'GET' : 'POST', body);
    if (!value || typeof value !== 'object')
      throw Error('服务响应不完整；保留原号查询。');
    return value as View;
  }
  async function run(work: () => Promise<void>) {
    if (locked.current) return;
    locked.current = true;
    setBusy(true);
    const e = epoch.current;
    setNotice('正在核对服务端实际状态…');
    try {
      await work();
      if (e === epoch.current) setNotice('服务端已返回。请核对下方实际状态。');
    } catch (error) {
      if (error instanceof RuntimeApiError && error.status === 401) {
        clearViews();
        setAuth(false);
      }
      setNotice(
        error instanceof Error
          ? `${error.message}。保留原号；结果未明时只查询，不重新付款。`
          : '服务不可用，保留原号。',
      );
    } finally {
      locked.current = false;
      setBusy(false);
    }
  }
  async function balances() {
    const v = await call('/api/bank/overview');
    setOverview(v);
  }
  async function subscriptions() {
    setLedger(await call('/api/subscriptions', { kind: 'LIST' }));
  }
  async function recoverPayment() {
    const r = refs.current;
    if (!r.paymentTask) return;
    if (r.preparation) {
      const p = await call(path(r.paymentTask, r.preparation));
      setPayment(p);
      const f = await call(
        path(r.paymentTask, r.preparation) + '/funds-result',
      );
      setFunds(f);
      if (f.execution_operation_id)
        save({ execution: f.execution_operation_id });
    } else {
      setDraft(await call(`/api/sim/draft-lifecycles/${r.paymentTask}`));
      const list = await call(`/api/sim/payment-preparations/${r.paymentTask}`);
      if (Array.isArray(list) && list.length) {
        const p = list[0];
        save({ preparation: p.operation.operation_id });
        setPayment(p);
        setFunds(
          await call(
            path(r.paymentTask, p.operation.operation_id) + '/funds-result',
          ),
        );
      }
    }
    // Recovery is read only; old confirmation/verification does not become authority.
    setDelegation(undefined);
    setVerified(false);
    await balances();
  }
  useEffect(() => {
    let active = true;
    getSession()
      .then((v) => {
        if (active) {
          setAuth(v.authenticated);
          setNotice(
            v.authenticated
              ? '已登录本机模拟银行。'
              : '请使用本机模拟口令登录。',
          );
        }
      })
      .catch(() => {
        if (active) setNotice('本机服务未连接，请先按启动说明运行候选。');
      });
    return () => {
      active = false;
    };
  }, []);
  async function signIn() {
    await login(secret);
    clearViews();
    setAuth(true);
    await balances();
  }
  async function signOut() {
    await logout();
    clearViews();
    setAuth(false);
    setNotice('已退出。已受理的独立委托按原合同处理；重新登录后只读查询原号。');
  }
  async function newDraft() {
    if (refs.current.paymentTask) throw Error('已有原付款记录，请先查询');
    const nonce = crypto.randomUUID();
    const task = 'bank_payment_' + nonce;
    save({ paymentNonce: nonce, paymentTask: task });
    setDraft(await call('/api/bank/payment-draft', { nonce }));
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
    setDraft(
      await call(life(draft) + '/execute', {
        ...lc,
        confirmation_ref: draft.confirmation.confirmation_ref,
        task_revision: draft.task_revision,
        operation_revision: draft.operation_revision,
        plan_version: draft.plan_version,
        facts_version: draft.facts_version,
      }),
    );
  }
  async function preparePayment() {
    if (!draft) return;
    const p = await call('/api/sim/payment-preparations/' + draft.task_id, {
      ...pay,
      source: {
        selection_ticket: draft.selection_ticket,
        task_revision: draft.task_revision,
        plan_version: draft.plan_version,
        facts_version: draft.facts_version,
        draft_id: draft.receipt.draft_id,
        draft_version: draft.receipt.draft_version,
      },
    });
    save({ preparation: p.operation.operation_id });
    setPayment(p);
  }
  async function confirmPayment() {
    if (!payment) return;
    setPayment(
      await call(
        path(payment.task_id, payment.operation.operation_id) + '/confirm',
        {
          ...pay,
          confirmation_ref: payment.confirmation_ref,
          display_ticket: payment.display_ticket,
          action: 'CONFIRM',
        },
      ),
    );
  }
  async function background(action: View) {
    if (!payment) throw Error('请先读取原付款');
    return call(
      path(payment.task_id, payment.operation.operation_id) + '/background',
      { ...bg, action },
    );
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
  async function prepareFeedback(reference: View) {
    const c = await recoveryContext();
    if (!c.available || !c.recovery_realm) throw Error('反馈恢复未启用');
    if (refs.current.feedbackNonce) throw Error('已有反馈原号，请只读核对');
    const nonce = crypto.randomUUID();
    save({ feedbackNonce: nonce, feedbackRealm: c.recovery_realm });
    const v = await recoveryPrepare(c.recovery_realm, nonce, {
      input_version: 1,
      report_id: report!.report.metadata.report_id,
      transaction: reference as {
        source_id: string;
        account_id: string;
        transaction_id: string;
      },
      choice: 'NOT_MINE',
    });
    if (v.kind !== 'CREATED') throw Error('已存在准备，仅可原号查询');
    setFeedback(v);
  }
  async function recoverFeedback() {
    const r = refs.current;
    if (r.feedbackNonce && r.feedbackRealm)
      setFeedbackResult(await recoveryRead(r.feedbackRealm, r.feedbackNonce));
    setFeedback(undefined);
    setFeedbackConfirmed(false);
  }
  async function prepareCard() {
    if (refs.current.cardNonce) throw Error('已有卡操作原号，请查询');
    const nonce = crypto.randomUUID();
    save({ cardNonce: nonce });
    const v = await call('/api/card-safety', {
      kind: 'PREPARE',
      account_id: 'account_001',
      card_id: 'demo_card_salary_v1',
      nonce,
    });
    save({ cardOperation: v.operation_id });
    setCard(v);
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
    if (refs.current.subscriptionNonce) throw Error('已有订阅原号，请先核对');
    const nonce = crypto.randomUUID();
    save({ subscriptionNonce: nonce });
    const v = await call('/api/subscriptions/change', {
      kind: 'PREPARE',
      nonce,
      request: input,
    });
    save({ subscriptionOperation: v.operation_id });
    setSub(v);
  }
  async function recoverSub() {
    const r = refs.current;
    if (r.subscriptionOperation)
      setSubResult(
        await call('/api/subscriptions/change', {
          kind: 'QUERY',
          operation_id: r.subscriptionOperation,
        }),
      );
    else if (r.subscriptionNonce) {
      const v = await call('/api/subscriptions/change', {
        kind: 'QUERY_NONCE',
        nonce: r.subscriptionNonce,
      });
      save({ subscriptionOperation: v.operation_id });
      setSubResult(v.result);
    }
    setSub(undefined);
    setSubConfirmed(false);
    await subscriptions();
  }
  const disable = busy || !auth;
  const monthly = report?.monthly_report;
  // Submitted preparation views are historical observations. Only the actual
  // original funds result describes current payment progress.
  const paymentReadOnly =
    funds?.submitted === true ||
    [
      'SUCCEEDED',
      'FAILED',
      'PROCESSING',
      'UNKNOWN',
      'RECONCILE_REQUIRED',
      'SOURCE_CONFLICT',
    ].includes(funds?.state);
  const paymentTerminal = ['SUCCEEDED', 'FAILED'].includes(funds?.state);
  return (
    <div className="bank-app">
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
                setPage(id);
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
          {auth && page === 'home' && (
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
                    <p>
                      {text(
                        v.account.masked_account_no ?? v.account.account_id,
                      )}
                    </p>
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
          {auth && page === 'transfer' && (
            <section className="bank-panel">
              <h2>模拟转账 ¥100.00</h2>
              <p>
                工资账户 →
                已登记收款人。草稿、付款确认与独立后台委托分别办理；模拟验证码从本机控制台取得。
              </p>
              <div className="bank-actions">
                <button
                  className="primary"
                  disabled={disable || !!saved.paymentTask}
                  onClick={() => void run(newDraft)}
                >
                  创建100元草稿
                </button>
                <button
                  disabled={disable || !saved.paymentTask}
                  onClick={() => void run(recoverPayment)}
                >
                  按原号查询付款
                </button>
              </div>
              {saved.paymentTask && (
                <p className="bank-ref">
                  原任务 {saved.paymentTask}
                  <br />
                  原付款 {saved.preparation || '受理前待核对'}
                </p>
              )}
              {draft && (
                <>
                  <h3>
                    1. 原草稿资料{paymentReadOnly ? '（历史，只读）' : '与确认'}
                  </h3>
                  <p>
                    金额 {text(draft.parameters?.amount)} · 状态{' '}
                    {state(draft.execution_status)}
                  </p>
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
                      disabled={
                        disable ||
                        paymentReadOnly ||
                        draft.next_action?.kind !== 'CONFIRM'
                      }
                      onClick={() => void run(confirmDraft)}
                    >
                      确认创建草稿
                    </button>
                    <button
                      disabled={
                        disable ||
                        paymentReadOnly ||
                        draft.next_action?.kind !== 'EXECUTE'
                      }
                      onClick={() => void run(executeDraft)}
                    >
                      保存模拟草稿
                    </button>
                    <button
                      disabled={
                        disable ||
                        paymentReadOnly ||
                        draft.execution_status !== 'SUCCEEDED' ||
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
                      [
                        '付款账户',
                        payment.snapshot?.display_details?.account
                          ?.display_name,
                      ],
                      [
                        '付款卡号',
                        payment.snapshot?.display_details?.account
                          ?.masked_account_no,
                      ],
                      [
                        '收款人',
                        payment.snapshot?.display_details?.payee?.display_name,
                      ],
                      [
                        '收款账号',
                        payment.snapshot?.display_details?.payee
                          ?.masked_account_no,
                      ],
                      [
                        '转账金额（元）',
                        payment.snapshot?.display_details?.amount,
                      ],
                      [
                        '备注',
                        payment.snapshot?.display_details?.remark || '无',
                      ],
                    ]}
                  />
                  <p>
                    原准备状态 {state(payment.operation?.state)} · 原确认记录{' '}
                    {state(payment.confirmation_status)}
                  </p>
                  <div className="bank-actions">
                    <button
                      disabled={
                        disable ||
                        paymentReadOnly ||
                        payment.confirmation_status === 'CONFIRMED'
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
                      [
                        '委托时长（分钟）',
                        delegation.display?.duration_minutes,
                      ],
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
                  {delegationConfirmed && <DemoVerification key={delegation.consent_id} operation={{kind:'PAYMENT',task_id:payment!.task_id,operation_id:payment!.operation.operation_id,consent_id:delegation.consent_id}} disabled={disable || paymentReadOnly} />}
                  <label>
                    付款专用模拟验证码
                    <input
                      value={code}
                      autoComplete="off"
                      onChange={(e) => setCode(e.target.value)}
                    />
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
                        const v = await background({
                          kind: 'EXECUTE',
                          consent_id: delegation.consent_id,
                          confirmation_ref: payment!.confirmation_ref,
                          display_ticket: payment!.display_ticket,
                        });
                        setPayment(v);
                        save({ execution: v.operation?.operation_id });
                        setVerified(false);
                        await recoverPayment();
                      })
                    }
                  >
                    提交100元模拟付款
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
                  <p>
                    处理中或结果未明时保留原号，不再创建付款。显示余额只在服务端查回后刷新。
                  </p>
                  {saved.preparation &&
                    !paymentTerminal &&
                    funds.submitted === true && (
                      <button
                        disabled={disable}
                        onClick={() =>
                          void run(async () => {
                            const v = await call(
                              path(saved.paymentTask!, saved.preparation!) +
                                '/funds-result',
                            );
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
          {auth && page === 'bills' && (
            <>
              <section className="bank-panel">
                <h2>月度消费分析</h2>
                <p>
                  固定演示截止 2026-10-27
                  00:00（上海）。退款按发生期列示，净额保留正负。
                </p>
                <div className="bank-actions">
                  <button
                    className="primary"
                    disabled={disable}
                    onClick={() => void run(loadReport)}
                  >
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
                            毛额 {text(monthly.comparison?.[period]?.gross)} ·
                            退款 {text(monthly.comparison?.[period]?.refunds)}
                          </p>
                        </section>
                      ))}
                    </section>
                    <p>
                      变化{' '}
                      {monthly.comparison?.percent == null
                        ? '不计算百分比 · ' +
                          text(monthly.comparison?.percent_reason)
                        : monthly.comparison.percent + '%'}{' '}
                      · {state(monthly.status)}
                    </p>
                    <Info
                      rows={(
                        ['current', 'previous', 'historical'] as const
                      ).map((period) => [
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
                      未触发不代表安全；证据不足和不可用保留原状态。反馈暂不支持更正或撤销，也不能授权冻结。
                    </p>
                    <h3>本期原交易反馈</h3>
                    {report?.report.evidence?.map((row: View, i: number) => (
                      <div className="bank-row" key={i}>
                        <span>
                          {when(row.transaction.completed_at)} ·{' '}
                          {text(row.transaction.amount)}
                        </span>
                        <button
                          disabled={disable || !!saved.feedbackNonce}
                          onClick={() =>
                            void run(() => prepareFeedback(row.reference))
                          }
                        >
                          标记非本人交易
                        </button>
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
                      确认将所选本期交易标记为“非本人”。该判断仅保存本地，不代表银行争议处理或冻结授权。
                    </p>
                    <p className="bank-ref">
                      原反馈参考号 {saved.feedbackNonce}
                    </p>
                    <button
                      disabled={disable || feedbackConfirmed}
                      onClick={() =>
                        void run(async () => {
                          await recoveryConfirm(feedback);
                          setFeedbackConfirmed(true);
                        })
                      }
                    >
                      确认保存本人判断
                    </button>
                    <button
                      disabled={disable || !feedbackConfirmed}
                      onClick={() =>
                        void run(async () => {
                          setFeedbackResult(await recoveryExecute(feedback));
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
                    <p className="bank-ref">
                      原反馈参考号 {saved.feedbackNonce}
                    </p>
                  </>
                )}
              </section>
            </>
          )}
          {auth && page === 'cards' && (
            <section className="bank-panel">
              <h2>工资卡 · 尾号6628</h2>
              <p>本人主动冻结。独立确认及卡片模拟验证码不复用付款授权。</p>
              <button
                className="primary"
                disabled={disable || !!saved.cardNonce}
                onClick={() => void run(prepareCard)}
              >
                准备本人主动冻结
              </button>
              <button
                disabled={disable || !saved.cardNonce}
                onClick={() => void run(recoverCard)}
              >
                查询原卡操作
              </button>
              {card && (
                <>
                  <Info
                    rows={[
                      [
                        '银行卡',
                        '尾号 ' + text(card.parameters?.display?.last_four),
                      ],
                      ['冻结原因', card.parameters?.display?.reason],
                      ['冻结影响', card.parameters?.display?.impact],
                      [
                        '解除方式',
                        card.parameters?.display?.unlock_instruction,
                      ],
                    ]}
                  />
                  <p>
                    原操作 {card.operation_id} · {state(card.state)}
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
                  {cardConfirmed && !cardResult && <DemoVerification key={card.operation_id} operation={{kind:'CARD',operation_id:card.operation_id}} disabled={disable} />}
                  <p>在演示验证区域领取后，手动输入卡片专用挑战号与验证码；本机环境仍可使用私有控制台。</p>
                  <label>
                    卡片挑战号
                    <input
                      value={challenge}
                      onChange={(e) => setChallenge(e.target.value)}
                    />
                  </label>
                  <label>
                    卡片模拟验证码
                    <input
                      value={code}
                      onChange={(e) => setCode(e.target.value)}
                      autoComplete="off"
                    />
                  </label>
                  <button
                    disabled={
                      disable ||
                      !!cardResult ||
                      !cardConfirmed ||
                      !challenge ||
                      !code
                    }
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
          {auth && page === 'subscriptions' && (
            <>
              <section className="bank-panel">
                <h2>订阅与提醒</h2>
                <p>
                  取消意图仅记入本地；商户退订请到官方渠道。模拟提醒不表示真实通知已发送。
                </p>
                <div className="bank-actions">
                  <button
                    disabled={disable}
                    onClick={() => void run(subscriptions)}
                  >
                    读取本人订阅
                  </button>
                  <button
                    disabled={disable}
                    onClick={() =>
                      void run(async () =>
                        setCapture(
                          await call('/api/subscriptions/change', {
                            kind: 'CAPTURE',
                          }),
                        ),
                      )
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
                {capture?.preview?.recognition?.groups?.map(
                  (g: View, i: number) => (
                    <div className="bank-row" key={i}>
                      <span>
                        <strong>
                          {g.service_display_name ??
                            `疑似订阅 ${g.selection_index + 1}`}
                        </strong>
                        <br />
                        {g.charges
                          ?.map(
                            (charge: View) =>
                              `${charge.completed_at?.slice(0, 10)} · ¥${(charge.amount_minor_units / 100).toFixed(2)}`,
                          )
                          .join(' / ')}
                        <br />
                        {g.message}
                      </span>
                      <button
                        aria-label={`准备登记候选${g.selection_index + 1}`}
                        disabled={
                          disable ||
                          !!saved.subscriptionNonce ||
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
                  ),
                )}
                {ledger?.items?.map((item: View, i: number) => (
                  <div className="bank-subscription" key={i}>
                    <h3>
                      {text(
                        item.service_display_name ??
                          item.display_name ??
                          item.service_name ??
                          item.entry?.display_name ??
                          '本人订阅',
                      )}
                    </h3>
                    <Info
                      rows={[
                        ['本地状态', state(item.current_state)],
                        ['提醒设置', state(item.reminder_state)],
                        ...(item.reminder
                          ? ([
                              ['预计扣费日期', item.reminder.expected_date],
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
                          await subscriptions();
                        })
                      }
                    >
                      保存订阅操作
                    </button>
                  </>
                )}
                <button
                  disabled={disable || !saved.subscriptionNonce}
                  onClick={() => void run(recoverSub)}
                >
                  按原号核对订阅操作
                </button>
                {subResult && (
                  <>
                    <p className="bank-result">
                      {subResult.original?.message || '原操作结果待核对'}
                    </p>
                    <p className="bank-ref">
                      原操作参考号 {saved.subscriptionOperation}
                    </p>
                    {subResult.confirmed_arrangement && (
                      <Arrangement value={subResult.confirmed_arrangement} />
                    )}
                    <button
                      disabled={
                        disable ||
                        subResult.domain_audit?.status !==
                          'VERIFIED_CANDIDATE_V1'
                      }
                      onClick={() =>
                        void run(async () => {
                          if (
                            subResult.domain_audit?.status !==
                            'VERIFIED_CANDIDATE_V1'
                          )
                            throw Error('原操作未核清');
                          save({
                            subscriptionNonce: undefined,
                            subscriptionOperation: undefined,
                          });
                          setSubResult(undefined);
                        })
                      }
                    >
                      已核对记录，准备下一项
                    </button>
                  </>
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
        </section>
        <footer>
          SafeFlow · 新建隔离演示候选 · 数据与办理结果以服务端查回为准
        </footer>
      </main>
    </div>
  );
}
