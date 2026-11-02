import { useEffect, useRef, useState } from 'react';
import { Notice, SectionTitle } from '../components';
import { SessionPanel } from '../runtime/SessionPanel';
import { login, logout } from '../runtime/client';
import {
  subscriptionContext,
  subscriptionRead,
  SubscriptionReadEpoch,
  type SubscriptionReadView,
  subscriptionDetails,
  subscriptionOriginal,
  type SubscriptionDetailView,
  type SubscriptionOriginalView,
} from '../runtime/subscription-read';
import './CardReadPanel.css';

export default function SubscriptionReadPanel() {
  const epoch = useRef(new SubscriptionReadEpoch());
  const alive = useRef(false),
    changing = useRef(false);
  const context = useRef<string | null>(null);
  const sessionGeneration = useRef(0);
  const [authenticated, setAuthenticated] = useState(false);
  const [secret, setSecret] = useState('');
  const [busy, setBusy] = useState<string | null>(null);
  const [view, setView] = useState<SubscriptionReadView>();
  const [detail, setDetail] = useState<{ position: number; view: SubscriptionDetailView }>();
  const [original, setOriginal] = useState<SubscriptionOriginalView>();
  const [originalRef, setOriginalRef] = useState('');
  const [message, setMessage] = useState('登录项目模拟会话后，可查询已有的本人订阅资料。');
  const clear = () => {
    epoch.current.invalidate();
    setView(undefined);
    setDetail(undefined);
    setOriginal(undefined);
    setOriginalRef('');
    setBusy(null);
  };
  useEffect(() => {
    alive.current = true;
    let sequence = 0;
    const check = async () => {
      if (changing.current) return;
      const seq = ++sequence,
        expected = context.current;
      try {
        const current = await subscriptionContext();
        if (!alive.current || changing.current || sequence !== seq || context.current !== expected)
          return;
        if (current !== expected) {
          clear();
          context.current = current;
          setMessage('会话已变化，请重新查询。');
        }
        setAuthenticated(current !== null);
      } catch {
        if (!alive.current || changing.current || sequence !== seq || context.current !== expected)
          return;
        context.current = null;
        clear();
        setAuthenticated(false);
        setMessage('会话暂时无法核验，请重新连接。');
      }
    };
    void check();
    const timer = setInterval(() => void check(), 5000);
    window.addEventListener('focus', check);
    return () => {
      alive.current = false;
      sequence++;
      sessionGeneration.current++;
      changing.current = false;
      epoch.current.invalidate();
      clearInterval(timer);
      window.removeEventListener('focus', check);
    };
  }, []);
  async function run(
    action:
      | { kind: 'LIST' }
      | { kind: 'DETAILS'; ref: string; position: number }
      | { kind: 'ORIGINAL'; ref: string } = { kind: 'LIST' },
  ) {
    const step = epoch.current.next(),
      expected = context.current;
    if (action.kind === 'LIST') setView(undefined);
    setDetail(undefined);
    setOriginal(undefined);
    setBusy('query');
    setMessage('正在核对本人订阅资料…');
    try {
      if (!expected || (await subscriptionContext(step.signal)) !== expected)
        throw Error('SESSION');
      if (!step.current()) return;
      const next =
        action.kind === 'LIST'
          ? await subscriptionRead(step.signal)
          : action.kind === 'DETAILS'
            ? await subscriptionDetails(action.ref, step.signal)
            : await subscriptionOriginal(action.ref, step.signal);
      const current = await subscriptionContext(step.signal);
      if (!step.current()) return;
      if (current !== expected) {
        context.current = current;
        setAuthenticated(current !== null);
        throw Error('SESSION');
      }
      if (action.kind === 'LIST') setView(next as SubscriptionReadView);
      else if (action.kind === 'DETAILS')
        setDetail({ position: action.position, view: next as SubscriptionDetailView });
      else setOriginal(next as SubscriptionOriginalView);
      setMessage(next.message);
    } catch {
      if (!step.current()) return;
      setView(undefined);
      setDetail(undefined);
      setOriginal(undefined);
      setMessage(
        action.kind === 'DETAILS'
          ? '选择已失效、资料已变化或暂时无法核验，请重新查询列表后选择。'
          : '暂时无法核实订阅资料或原结果，请稍后重新查询；不要据此重复提交。',
      );
    } finally {
      if (step.current()) setBusy(null);
    }
  }
  async function changeSession(signIn: boolean) {
    const generation = ++sessionGeneration.current;
    changing.current = true;
    clear();
    context.current = null;
    setAuthenticated(false);
    const step = epoch.current.next();
    const password = secret;
    setSecret('');
    setBusy(signIn ? 'login' : 'logout');
    try {
      if (signIn) await login(password, step.signal);
      else await logout(step.signal);
      const current = await subscriptionContext(step.signal);
      if (!step.current()) return;
      context.current = current;
      setAuthenticated(current !== null);
      setMessage('会话已更新，请重新查询本人订阅。');
    } catch {
      if (step.current()) setMessage('登录或退出未能确认，请重新核验会话。');
    } finally {
      if (generation === sessionGeneration.current) {
        changing.current = false;
        if (step.current()) setBusy(null);
      }
    }
  }
  const status: Record<string, string> = {
    ACTIVE: '本地有效',
    CANCELLING: '本地取消意图已登记',
    ERROR: '待核对',
    KNOWN: '资料已核对',
    UNKNOWN: '资料待核对',
  };
  return (
    <section className="card-safety card-read-panel" aria-label="本人订阅只读查询">
      <SectionTitle
        title="本人订阅与原结果"
        description="项目模拟会话 · 查询已有资料和本地记录，不连接真实商户。"
      />
      <SessionPanel
        authenticated={authenticated}
        checking={false}
        secret={secret}
        busy={busy}
        onSecretChange={setSecret}
        onSignIn={async (e) => {
          e.preventDefault();
          await changeSession(true);
        }}
        onSignOut={() => changeSession(false)}
      />
      <div className="card-item-actions">
        <button
          className="button primary"
          disabled={!authenticated || changing.current}
          onClick={() => void run()}
        >
          {busy === 'query' ? '重新查询' : '查询 / 刷新订阅'}
        </button>
        <button
          className="button secondary"
          onClick={() => {
            clear();
            setMessage('已清除本页查询结果。');
          }}
        >
          清除结果
        </button>
      </div>
      <Notice>{message}</Notice>
      <p>
        登记、取消意图、撤回和提醒修改暂未开放。取消意图不代表商户已退订，提醒设置不代表已发送。
      </p>
      {view && (
        <div data-testid="subscription-read-result">
          {view.status === 'AVAILABLE' && !view.items.length && (
            <p>本次完整本人台账中没有订阅记录。</p>
          )}
          {view.items.map((i) => (
            <article className="safety-row" key={i.position}>
              <h3>
                订阅记录 {i.position} · {status[i.current_state] ?? '状态待核对'}
              </h3>
              <p>{i.message}</p>
              <button
                className="button secondary"
                disabled={!authenticated || changing.current}
                onClick={() =>
                  void run({ kind: 'DETAILS', ref: i.selection_ref, position: i.position })
                }
              >
                查看订阅记录 {i.position} 的详情
              </button>
              {i.reminder ? (
                <p>
                  预计日期 {i.reminder.expected_date} · 提前 {i.reminder.days_before} 天 ·{' '}
                  {i.reminder.enabled ? '提醒设置开启' : '提醒设置关闭'}
                </p>
              ) : (
                <p>尚无可展示的提醒计划。</p>
              )}
            </article>
          ))}
          {view.originals.map((o) => (
            <article className="safety-row" key={o.original_ref}>
              <p>{o.message}</p>
              <button
                className="button secondary"
                onClick={() => void run({ kind: 'ORIGINAL', ref: o.original_ref })}
              >
                核对这笔原结果
              </button>
            </article>
          ))}
          {!!view.pending.length && <p>{view.pending.length} 份待核对的确认内容；当前仅供查看。</p>}
        </div>
      )}
      {detail && (
        <article
          className="safety-row"
          style={{ display: 'block' }}
          data-testid="subscription-detail-result"
        >
          <h3>订阅记录 {detail.position} 的详情</h3>
          <p>{detail.view.message}</p>
          <p>
            服务资料：{detail.view.service_data_state === 'AVAILABLE' ? '已有资料' : '待核对'}
            ；服务名称尚无可展示资料。
          </p>
          <h4>本地取消状态与历史</h4>
          <p>当前状态：{status[detail.view.cancellation.current_state] ?? '待核对'}。</p>
          <p>{detail.view.cancellation.message}</p>
          {!detail.view.cancellation.history.length && <p>未记录本地取消或撤回历史。</p>}
          {detail.view.cancellation.history.map((h, i) => (
            <p key={i}>
              {h.occurred_at} · {h.kind === 'RECORD' ? '记录取消意图' : '撤回取消意图'} ·{' '}
              {status[h.state_after_operation] ?? '状态待核对'}
            </p>
          ))}
          <h4>提醒计划与本地记录</h4>
          <p>{detail.view.reminder.message}</p>
          {detail.view.reminder.schedule ? (
            <p>
              {detail.view.reminder.schedule.expected_date} · 提前{' '}
              {detail.view.reminder.schedule.days_before} 天 ·{' '}
              {detail.view.reminder.schedule.enabled ? '提醒设置开启' : '提醒设置关闭'}
            </p>
          ) : (
            <p>尚无已保存的提醒计划。</p>
          )}
          {detail.view.reminder.current_attempt_count !== null && (
            <p>
              当前已记录尝试 {detail.view.reminder.current_attempt_count} 次；可用重试次数{' '}
              {detail.view.reminder.retries_remaining ?? '待核对'}。
            </p>
          )}
          {detail.view.reminder.current_attempt && (
            <p>
              当前本地尝试状态：
              {(
                {
                  NOT_ATTEMPTED: '尚未尝试',
                  SIMULATED_SUCCESS: '模拟成功',
                  FAILED: '失败',
                  RETRY_EXHAUSTED: '重试已用尽',
                  NEEDS_REVIEW: '待核对',
                } as Record<string, string>
              )[detail.view.reminder.current_attempt] ?? '待核对'}
              ；不代表真实通知已发送。
            </p>
          )}
          {detail.view.reminder.saved_cycle_outcomes.map((a, i) => (
            <p key={i}>
              本地尝试记录 {a.last_attempt_date} · {a.attempt_count} 次 ·{' '}
              {a.state === 'SIMULATED_SUCCESS'
                ? '模拟成功'
                : a.state === 'FAILED'
                  ? '失败'
                  : a.state === 'RETRY_EXHAUSTED'
                    ? '重试已用尽'
                    : '待核对'}
              ；不代表真实通知已发送。
            </p>
          ))}
          {!!detail.view.pending_confirmations.length && (
            <p>{detail.view.pending_confirmations.length} 份相关待核对内容；仅供查看。</p>
          )}
        </article>
      )}
      <form
        onSubmit={(e) => {
          e.preventDefault();
          void run({ kind: 'ORIGINAL', ref: originalRef.trim() });
        }}
      >
        <label>
          原操作编号
          <input
            aria-label="原操作编号"
            value={originalRef}
            onChange={(e) => {
              epoch.current.invalidate();
              setBusy(null);
              setDetail(undefined);
              setOriginal(undefined);
              setOriginalRef(e.target.value);
            }}
            autoComplete="off"
          />
        </label>
        <button
          className="button secondary"
          type="submit"
          disabled={!authenticated || changing.current || !originalRef.trim()}
        >
          查询原操作结果
        </button>
      </form>
      {original && (
        <article
          className="safety-row"
          style={{ display: 'block' }}
          data-testid="subscription-original-result"
        >
          <h3>原操作核对结果</h3>
          <p>{original.message}</p>
          <p>仅查询已有本地事实，不重新执行操作。</p>
        </article>
      )}
    </section>
  );
}
