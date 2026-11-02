import { useState } from 'react';
import {
  ArrowDownLeft,
  ArrowLeftRight,
  ArrowRight,
  ArrowUpRight,
  Bot,
  ChevronRight,
  CreditCard,
  Eye,
  EyeOff,
  Plus,
  ReceiptText,
  Send,
  Sparkles,
} from 'lucide-react';
import { useWorkbench } from '../context';
import { accounts, cards } from '../domain/fixtures';
import { billSummary, draftStatus, formatMoney, monthKey } from '../domain/model';
import type { Transaction } from '../domain/model';
import {
  BankCard,
  draftChangeLabel,
  Empty,
  IconButton,
  Metric,
  PageTitle,
  SectionTitle,
  Tag,
  TransactionList,
} from '../components';
import { TransactionDetail } from './Bills';

export default function Overview() {
  const { state, adapter, fixtures, now, navigate, notify } = useWorkbench();
  const [accountId, setAccountId] = useState('account_001');
  const [hidden, setHidden] = useState(false);
  const [input, setInput] = useState('');
  const [selected, setSelected] = useState<Transaction>();
  const account = accounts.find((a) => a.id === accountId)!;
  const rows = fixtures.transactions.filter(
    (t) => t.accountId === accountId && monthKey(new Date(t.date)) === fixtures.currentMonth,
  );
  const summary = billSummary(rows);
  const drafts = state.drafts.filter((d) => draftStatus(d, now.getTime()) === 'DRAFT');
  const reminder = fixtures.subscriptions.find(
    (s) => state.subscriptionPrefs[s.id]?.reminder !== false,
  );
  const greeting = now.getHours() < 12 ? '上午好' : now.getHours() < 18 ? '下午好' : '晚上好';
  function ask(text: string) {
    try {
      adapter.sendMessage(text);
      navigate('agent');
    } catch (err) {
      notify((err as Error).message, true);
    }
  }
  return (
    <>
      <PageTitle
        title={`${greeting}，Echo`}
        description={`${now.toLocaleDateString('zh-CN', { month: 'long', day: 'numeric', weekday: 'long' })} · 看看今天的账户动态。`}
        action={
          <button className="button primary" onClick={() => navigate('transfers')}>
            <Plus size={18} />
            新建草案
          </button>
        }
      />
      <section className="account-band">
        <div className="account-summary">
          <div className="account-selector">
            <label>
              <span className="sr-only">总览账户</span>
              <select
                aria-label="总览账户"
                value={accountId}
                onChange={(e) => setAccountId(e.target.value)}
              >
                {accounts.map((a) => (
                  <option key={a.id} value={a.id}>
                    {a.name} · {a.tail}
                  </option>
                ))}
              </select>
            </label>
            <Tag tone="green">模拟账户</Tag>
          </div>
          <div className="balance-label">
            可用余额{' '}
            <IconButton
              icon={hidden ? EyeOff : Eye}
              label={hidden ? '显示余额' : '隐藏余额'}
              onClick={() => setHidden(!hidden)}
            />
          </div>
          <div className="balance-number">
            <small>¥</small>
            <strong>{hidden ? '••••••' : formatMoney(account.balance)}</strong>
            <span>CNY</span>
          </div>
          <p className="account-source">{account.source} · 非实时银行余额</p>
          <div className="quick-actions">
            <button onClick={() => navigate('transfers')}>
              <ArrowLeftRight size={18} />
              转账汇款
            </button>
            <button onClick={() => navigate('bills')}>
              <ReceiptText size={18} />
              查账单
            </button>
            <button onClick={() => navigate('cards')}>
              <CreditCard size={18} />
              银行卡
            </button>
          </div>
        </div>
        <BankCard
          card={cards.find((c) => c.accountId === accountId)!}
          account={account}
          balanceHidden={hidden}
          compact
        />
      </section>
      <div className="overview-stats">
        <Metric
          label="本月消费"
          amount={summary.expense}
          icon={ArrowUpRight}
          sub={`所选账户 · ${rows.filter((t) => t.kind === 'EXPENSE').length} 笔消费`}
        />
        <Metric
          label="本月收入"
          amount={summary.income}
          icon={ArrowDownLeft}
          color="blue"
          sub="所选账户 · 演示数据"
        />
        <div className="metric">
          <div className="metric-label">
            <span>待处理草案</span>
            <span className="metric-icon coral">
              <ReceiptText size={18} />
            </span>
          </div>
          <strong className="metric-value">
            {String(drafts.length).padStart(2, '0')}
            <small className="after">份</small>
          </strong>
          <p>本地保存，待核对</p>
        </div>
      </div>
      <section className="agent-inline">
        <div className="agent-inline-heading">
          <span className="agent-symbol">
            <Sparkles size={21} />
          </span>
          <div>
            <h2>想处理什么？</h2>
            <p>
              SafeFlow 助手 <span>· 演示查询与草案引导</span>
            </p>
          </div>
          <Tag tone="blue">演示</Tag>
        </div>
        <form
          className="agent-input"
          onSubmit={(e) => {
            e.preventDefault();
            ask(input);
          }}
        >
          <input
            aria-label="询问助手"
            value={input}
            maxLength={1000}
            onChange={(e) => setInput(e.target.value)}
            placeholder="例如：帮我看看这个月花了多少"
          />
          <button
            className="send-button"
            aria-label="发送给助手"
            title="发送"
            disabled={!input.trim()}
          >
            <ArrowRight size={21} />
          </button>
        </form>
        <div className="prompt-links">
          <button onClick={() => ask('查账户余额')}>
            <Bot size={14} />
            查账户余额
          </button>
          <button onClick={() => ask('查询本月账单')}>
            <ReceiptText size={14} />
            本月账单
          </button>
          <button onClick={() => ask('给小李转 200 元')}>
            <Send size={14} />
            给小李转 200 元
          </button>
        </div>
      </section>
      <div className="overview-bottom">
        <section className="recent-section">
          <SectionTitle
            title="近期流水"
            description="演示流水 · 所选账户"
            action={
              <button className="text-button" onClick={() => navigate('bills')}>
                全部账单
                <ArrowRight size={16} />
              </button>
            }
          />
          {rows.length ? (
            <TransactionList
              rows={rows.slice(0, 5)}
              onSelect={setSelected}
              feedbackIds={state.feedback.map((f) => f.transactionId)}
            />
          ) : (
            <Empty />
          )}
        </section>
        <aside className="todo-section">
          <SectionTitle
            title="待办与提醒"
            action={<span className="count-tag">{drafts.length + (reminder ? 1 : 0)}</span>}
          />
          {drafts.slice(0, 2).map((d) => (
            <button className="todo-row" key={d.id} onClick={() => navigate('transfers')}>
              <span className="todo-icon green">
                <ReceiptText size={18} />
              </span>
              <div>
                <strong>核对转账草案</strong>
                <p>
                  ¥{formatMoney(d.amount)} · {draftChangeLabel(d.version)}
                </p>
              </div>
              <ChevronRight size={16} />
            </button>
          ))}
          {reminder && (
            <button className="todo-row" onClick={() => navigate('subscriptions')}>
              <span className="todo-icon amber">
                <CreditCard size={18} />
              </span>
              <div>
                <strong>{reminder.name}</strong>
                <p>下次续费 · ¥{reminder.amount}</p>
              </div>
              <ChevronRight size={16} />
            </button>
          )}
          {!drafts.length && (
            <div className="todo-calm">
              <span className="tiny-line" />
              <p>暂无待核对草案</p>
              <small>新的草案会显示在这里</small>
            </div>
          )}
          <div className="todo-footer">
            <span className="status-dot" />
            提醒仅在本机演示中展示
          </div>
        </aside>
      </div>
      {selected && (
        <TransactionDetail transaction={selected} onClose={() => setSelected(undefined)} />
      )}
    </>
  );
}
