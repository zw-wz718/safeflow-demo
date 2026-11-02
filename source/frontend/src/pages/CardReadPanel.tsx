import { useEffect, useRef, useState } from 'react';
import { Notice, SectionTitle, Tag } from '../components';
import { SessionPanel } from '../runtime/SessionPanel';
import { login, logout, request, RuntimeApiError } from '../runtime/client';
import { cardContext, cardRead, CardReadEpoch, type CardReadView } from '../runtime/card-read';
import './CardReadPanel.css';

export default function CardReadPanel() {
  const epoch = useRef(new CardReadEpoch());
  const context = useRef<string | null>(null);
  const alive = useRef(false);
  const changingSession = useRef(false);
  const pending = useRef<string | null>(null);
  const retained = useRef<string | null>(null);
  const [authenticated, setAuthenticated] = useState(false);
  const [secret, setSecret] = useState('');
  const [account, setAccount] = useState('account_001');
  const [feedbackOperation, setFeedbackOperation] = useState('');
  const [view, setView] = useState<CardReadView>();
  const [busy, setBusy] = useState<string | null>(null);
  const [message, setMessage] = useState('请登录项目模拟会话，再查询本人账户。页面不会自动选卡。');
  const cancelPending = () => {
    const id = pending.current;
    pending.current = null;
    if (id)
      void request('/api/card-read', 'POST', { kind: 'CANCEL_REQUEST', request_ref: id }).catch(
        () => {},
      );
  };
  const discard = () => {
    cancelPending();
    const id = retained.current;
    retained.current = null;
    if (id)
      void request('/api/card-read', 'POST', { kind: 'CANCEL', step_ref: id }).catch(() => {});
  };
  const reset = () => {
    epoch.current.invalidate();
    discard();
    setView(undefined);
    setBusy(null);
  };
  useEffect(() => {
    alive.current = true;
    let sequence = 0;
    const check = async () => {
      if (changingSession.current) return;
      const seq = ++sequence;
      const expected = context.current;
      try {
        const current = await cardContext();
        if (
          !alive.current ||
          changingSession.current ||
          seq !== sequence ||
          context.current !== expected
        )
          return;
        if (current !== context.current) {
          reset();
          context.current = current;
          setMessage('会话已变化，请重新查询并明确选卡。');
        }
        setAuthenticated(current !== null);
      } catch {
        if (
          !alive.current ||
          changingSession.current ||
          seq !== sequence ||
          context.current !== expected
        )
          return;
        context.current = null;
        reset();
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
      epoch.current.invalidate();
      discard();
      clearInterval(timer);
      window.removeEventListener('focus', check);
    };
  }, []);
  async function run(input: unknown) {
    cancelPending();
    const step = epoch.current.next();
    const requestRef = crypto.randomUUID();
    pending.current = requestRef;
    retained.current = null;
    const expected = context.current;
    setView(undefined);
    setBusy('query');
    setMessage('正在核验本人账户及完整资料…');
    try {
      if (!expected || (await cardContext(step.signal)) !== expected) throw new Error('SESSION');
      if (!step.current()) return;
      const next = await cardRead({ ...(input as object), request_ref: requestRef }, step.signal);
      const current = await cardContext(step.signal);
      if (!step.current()) return;
      if (current !== expected) {
        context.current = current;
        setAuthenticated(current !== null);
        throw new Error('SESSION');
      }
      pending.current = null;
      retained.current = next.step_ref;
      setView(next);
      setMessage('本次只读观察；不代表账户已获准出账。');
    } catch (error) {
      if (!step.current()) return;
      cancelPending();
      if (error instanceof RuntimeApiError && error.status === 401) {
        context.current = null;
        setAuthenticated(false);
      }
      setView(undefined);
      setMessage(
        '无法核实当前卡片或保护记录。旧选择已失效，请重新查询；不能据此判断无卡、无锁或办理成功。',
      );
    } finally {
      if (step.current()) setBusy(null);
    }
  }
  async function changeSession(signIn: boolean) {
    changingSession.current = true;
    reset();
    context.current = null;
    setAuthenticated(false);
    const step = epoch.current.next();
    setBusy(signIn ? 'login' : 'logout');
    const password = secret;
    setSecret('');
    try {
      if (signIn) await login(password, step.signal);
      else await logout(step.signal);
      const next = await cardContext(step.signal);
      if (!step.current()) return;
      context.current = next;
      setAuthenticated(next !== null);
      setMessage('会话已更新，请重新查询本人账户。');
    } catch {
      if (step.current()) setMessage('登录或退出未能确认，请重新核验会话。');
    } finally {
      changingSession.current = false;
      if (step.current()) setBusy(null);
    }
  }
  return (
    <section className="card-safety card-read-panel" aria-label="本人卡片只读查询">
      <SectionTitle
        title="本人卡片与保护记录"
        description="项目模拟会话 · 读取已采用资料，不连接真实银行。"
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
      <label>
        查询账户
        <input
          aria-label="查询账户"
          value={account}
          maxLength={128}
          onChange={(e) => {
            reset();
            setAccount(e.target.value);
            setMessage('账户已变更，请重新查询。');
          }}
        />
      </label>
      <div className="card-item-actions">
        <button
          className="button primary"
          disabled={!authenticated || !account.trim()}
          onClick={() => void run({ kind: 'QUERY', account_id: account.trim() })}
        >
          {busy === 'query' ? '重新查询' : '查询 / 刷新卡片'}
        </button>
        <button
          className="button secondary"
          onClick={() => {
            reset();
            setMessage('已清除本页选择，后续需重新查询。');
          }}
        >
          清除选择
        </button>
      </div>
      <Notice>{message}</Notice>
      <label>
        原反馈操作号
        <input
          aria-label="原反馈操作号"
          value={feedbackOperation}
          maxLength={200}
          onChange={(e) => {
            reset();
            setFeedbackOperation(e.target.value);
          }}
        />
      </label>
      <p>使用原账单反馈的操作号核对对应卡；反馈保存成功不代表已办理保护。</p>
      <button
        className="button secondary"
        disabled={!authenticated || !feedbackOperation.trim()}
        onClick={() =>
          void run({ kind: 'QUERY_FEEDBACK', feedback_operation_id: feedbackOperation.trim() })
        }
      >
        核对对应卡的保护记录
      </button>
      {view && (
        <div data-testid="card-read-result">
          <p>
            <Tag tone={view.account_outgoing_restricted ? 'amber' : 'green'}>
              {view.account_outgoing_restricted
                ? '当前该账户全部出账受卡锁限制'
                : '本次未查到有效卡锁；其他出账条件另行核验'}
            </Tag>
          </p>
          {view.feedback && (
            <Notice>
              {view.feedback.message}
              <br />
              {view.feedback.scope_explanation}
            </Notice>
          )}
          {view.feedback ? null : view.cards.length === 0 ? (
            <p>本次完整查询未发现卡片，不代表其他出账条件已满足。</p>
          ) : (
            view.cards.map((c) => (
              <article key={c.position} className="safety-row">
                <div>
                  <h3>
                    卡片选项 {c.position} · 尾号 {c.last_four}
                  </h3>
                  <p>
                    {c.ambiguous_tail ? '有相同尾号，请按本次选项明确选择。' : ''}
                    {c.card_locked ? '当前这张卡有有效锁' : '本次未查到这张卡的有效锁'}
                  </p>
                  <p>{c.public_reasons.join('、')}</p>
                </div>
                <button
                  className="button secondary"
                  onClick={() =>
                    void run({ kind: 'SELECT_CARD', step_ref: view.step_ref, position: c.position })
                  }
                >
                  选择卡片 {c.position}，查看保护记录
                </button>
              </article>
            ))
          )}
          {view.history && (
            <section aria-label="所选卡保护记录">
              <h3>所选卡尾号 {view.history.last_four} · 保护记录</h3>
              {view.feedback && view.original?.state === 'NEEDS_REVIEW' && (
                <Notice>
                  下方锁状态、账户限制与动作均为上次清单保留时点的观察，不是本次当前结果；旧选择已停用。
                </Notice>
              )}
              <p>{view.history.message}</p>
              <p>{view.history.ordering_explanation}</p>
              <p>
                {view.feedback && view.original?.state === 'NEEDS_REVIEW'
                  ? '清单保留时点：'
                  : '当前'}
                {view.history.card_locked ? '所选卡有有效锁' : '所选卡未查到有效锁'}；
                {view.history.account_outgoing_restricted
                  ? '该账户全部出账仍受限'
                  : '本次未查到该账户有效卡锁'}
              </p>
              {view.history.locks.map((l) => (
                <article key={l.position}>
                  <h4>
                    保护锁选项 {l.position} ·{' '}
                    {view.feedback && view.original?.state === 'NEEDS_REVIEW'
                      ? '清单保留时点：'
                      : '当前'}
                    {l.active ? '有效' : '已无效'}
                  </h4>
                  <p>{l.public_reason}</p>
                  {l.records.map((r) => (
                    <div key={r.position}>
                      <p>
                        {r.original_action} {r.public_reason}
                      </p>
                      {!(view.feedback && view.original?.state === 'NEEDS_REVIEW') && (
                        <button
                          className="button secondary"
                          onClick={() =>
                            void run({
                              kind: 'SELECT_RECORD',
                              step_ref: view.step_ref,
                              position: r.position,
                            })
                          }
                        >
                          核对原记录 {r.position}
                        </button>
                      )}
                    </div>
                  ))}
                </article>
              ))}
            </section>
          )}
          {view.original && (
            <section aria-label="原记录核对结果">
              <h3>原记录核对结果</h3>
              <Notice>{view.original.message}</Notice>
              {view.feedback && view.original.original_action && (
                <p>{view.original.original_action}</p>
              )}
            </section>
          )}
          <p>{view.unlock_instruction}</p>
        </div>
      )}
      <p>查卡、选卡与查看历史均不会冻结或解锁。解除请到线下银行办理。</p>
      <button className="button unavailable" disabled>
        冻结服务暂未开通
      </button>
    </section>
  );
}
