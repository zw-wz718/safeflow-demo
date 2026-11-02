import { useState } from 'react';
import { RuntimeApp } from '../runtime/RuntimeApp';
import '../runtime/styles.css';
import { Download, ArrowDownLeft, ArrowUpRight, ArrowLeftRight, Flag, Check } from 'lucide-react';
import { useWorkbench } from '../context';
import { accounts } from '../domain/fixtures';
import { billSummary, categoryTotals, formatMoney, monthKey } from '../domain/model';
import type { Transaction, TransactionKind } from '../domain/model';
import {
  DetailRows,
  Empty,
  Field,
  Mark,
  Metric,
  Modal,
  Notice,
  PageTitle,
  SearchField,
  SectionTitle,
  Tag,
  TransactionList,
} from '../components';

export function TransactionDetail({
  transaction,
  onClose,
}: {
  transaction: Transaction;
  onClose: () => void;
}) {
  const { state, adapter, notify } = useWorkbench();
  const [feedbackForm, setFeedbackForm] = useState(false);
  const [reason, setReason] = useState('');
  const [error, setError] = useState('');
  const feedback = state.feedback.find((f) => f.transactionId === transaction.id);
  const account = accounts.find((a) => a.id === transaction.accountId)!;
  return (
    <Modal title="流水详情" onClose={onClose}>
      <div className="detail-hero">
        <Mark text={transaction.mark} color={transaction.color} />
        <h3>{transaction.title}</h3>
        <strong>
          {transaction.direction === 'in' ? '+' : '−'} ¥{formatMoney(transaction.amount)}
        </strong>
        <Tag>演示流水</Tag>
      </div>
      <DetailRows
        rows={[
          [
            '类型',
            transaction.kind === 'TRANSFER'
              ? '转账（不计入消费）'
              : transaction.kind === 'INCOME'
                ? '收入'
                : '消费',
          ],
          ['分类', transaction.category],
          ['时间', new Date(transaction.date).toLocaleString('zh-CN')],
          ['账户', `${account.name} · ${account.tail}`],
          ['币种', 'CNY'],
        ]}
      />
      {feedback ? (
        <Notice tone="green">已登记本地反馈：{feedback.reason}。未提交银行，未触发锁卡。</Notice>
      ) : feedbackForm ? (
        <form
          onSubmit={(e) => {
            e.preventDefault();
            try {
              adapter.registerFeedback(transaction.id, reason);
              notify('反馈已在浏览器登记，未提交银行');
              setFeedbackForm(false);
            } catch (err) {
              setError((err as Error).message);
            }
          }}
        >
          <Field label="反馈原因">
            <textarea
              autoFocus
              value={reason}
              maxLength={300}
              rows={3}
              placeholder="请描述这笔样例流水的问题"
              onChange={(e) => {
                setReason(e.target.value);
                setError('');
              }}
              required
            />
          </Field>
          {error && (
            <p className="form-error" role="alert">
              {error}
            </p>
          )}
          <Notice>反馈只保存在此浏览器。卡片状态不会因此改变。</Notice>
          <div className="modal-actions">
            <button
              type="button"
              className="button secondary"
              onClick={() => setFeedbackForm(false)}
            >
              返回详情
            </button>
            <button className="button primary" type="submit">
              <Check size={17} />
              登记本地反馈
            </button>
          </div>
        </form>
      ) : (
        <div className="modal-actions">
          <button className="button secondary" onClick={onClose}>
            关闭
          </button>
          <button className="button secondary" onClick={() => setFeedbackForm(true)}>
            <Flag size={16} />
            反馈异常
          </button>
        </div>
      )}
    </Modal>
  );
}

const chartColors = ['#168562', '#6c91be', '#e19a7c', '#e3bb55', '#9a9eab'];
export function SpendingChart({ transactions }: { transactions: Transaction[] }) {
  const groups = categoryTotals(transactions);
  const summary = billSummary(transactions);
  let offset = 0;
  return (
    <div className="spending-chart">
      <div className="donut">
        <svg
          viewBox="0 0 120 120"
          role="img"
          aria-label={`消费分类图，总额 ${summary.expense} 元；${groups.map((g) => `${g.category} ${g.amount} 元`).join('，')}`}
        >
          <circle cx="60" cy="60" r="47" fill="none" stroke="#edf0ed" strokeWidth="12" />
          {groups.map((g, i) => {
            const start = offset;
            offset += g.percent;
            return (
              <circle
                key={g.category}
                cx="60"
                cy="60"
                r="47"
                fill="none"
                stroke={chartColors[i % chartColors.length]}
                strokeWidth="12"
                pathLength="100"
                strokeDasharray={`${Math.max(0, g.percent - 1)} ${100 - Math.max(0, g.percent - 1)}`}
                strokeDashoffset={-start}
                transform="rotate(-90 60 60)"
              />
            );
          })}
        </svg>
        <div className="donut-center">
          <span>消费合计</span>
          <strong>{formatMoney(summary.expense)}</strong>
          <small>CNY</small>
        </div>
      </div>
      <ul className="chart-legend">
        {groups.length ? (
          groups.map((g, i) => (
            <li key={g.category}>
              <span
                className="legend-dot"
                style={{ background: chartColors[i % chartColors.length] }}
              />
              <span>{g.category}</span>
              <strong>¥{formatMoney(g.amount)}</strong>
              <small>{g.percent}%</small>
            </li>
          ))
        ) : (
          <li>当前筛选没有消费流水</li>
        )}
      </ul>
    </div>
  );
}

export default function Bills() {
  const [online, setOnline] = useState(false);
  return (
    <>
      <div className="bank-transfer-tabs" role="group" aria-label="账单数据来源">
        <button
          className="button secondary"
          aria-pressed={!online}
          onClick={() => setOnline(false)}
        >
          浏览器样例账单
        </button>
        <button className="button secondary" aria-pressed={online} onClick={() => setOnline(true)}>
          后台报告与反馈
        </button>
      </div>
      {online ? <RuntimeApp bills /> : <SampleBills />}
    </>
  );
}
function SampleBills() {
  const { fixtures, state, notify } = useWorkbench();
  const [month, setMonth] = useState(fixtures.currentMonth);
  const [kind, setKind] = useState<'ALL' | TransactionKind>('ALL');
  const [search, setSearch] = useState('');
  const [selected, setSelected] = useState<Transaction>();
  const months = [...new Set(fixtures.transactions.map((t) => monthKey(new Date(t.date))))]
    .sort()
    .reverse();
  const rows = fixtures.transactions.filter(
    (t) =>
      monthKey(new Date(t.date)) === month &&
      (kind === 'ALL' || t.kind === kind) &&
      `${t.title} ${t.category} ${t.id}`.toLowerCase().includes(search.toLowerCase()),
  );
  const summary = billSummary(rows);
  function download() {
    const cells = [
      ['序号', '时间', '名称', '类型', '分类', '金额', '币种', '方向', '来源'],
      ...rows.map((t, index) => [
        String(index + 1),
        new Date(t.date).toLocaleString('zh-CN'),
        t.title,
        t.kind,
        t.category,
        t.amount,
        'CNY',
        t.direction,
        '演示流水',
      ]),
    ];
    const csv =
      '\uFEFF' +
      cells.map((row) => row.map((cell) => `"${cell.replace(/"/g, '""')}"`).join(',')).join('\r\n');
    const url = URL.createObjectURL(new Blob([csv], { type: 'text/csv;charset=utf-8;' }));
    const link = document.createElement('a');
    link.href = url;
    link.download = `SafeFlow_样例账单_${month}.csv`;
    link.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
    notify(`已导出当前筛选的 ${rows.length} 笔样例流水`);
  }
  return (
    <>
      <PageTitle
        title="我的账单"
        description="每一笔收支，清楚有序。"
        action={
          <button
            className="button secondary"
            onClick={download}
            disabled={!rows.length}
            title={!rows.length ? '无流水可导出' : '导出当前筛选流水'}
          >
            <Download size={17} />
            导出 CSV
          </button>
        }
      />
      <div className="bill-summary">
        <Metric
          label="消费支出"
          amount={summary.expense}
          icon={ArrowUpRight}
          sub="转账不计入消费"
        />
        <Metric
          label="收入"
          amount={summary.income}
          icon={ArrowDownLeft}
          color="blue"
          sub="所选月份与筛选范围"
        />
        <Metric
          label="转账"
          amount={summary.transfer}
          icon={ArrowLeftRight}
          color="coral"
          sub="演示流水，与本地草案无关"
        />
      </div>
      <section className="chart-section">
        <SectionTitle
          title="消费分布"
          description={`${month.replace('-', ' 年 ')} 月 · 演示流水`}
        />
        <SpendingChart transactions={rows} />
      </section>
      <section className="ledger-section">
        <SectionTitle
          title="收支明细"
          description={`${rows.length} 笔流水`}
          action={
            <label className="select-label">
              <span className="sr-only">账单月份</span>
              <select
                aria-label="账单月份"
                value={month}
                onChange={(e) => setMonth(e.target.value)}
              >
                {months.map((m) => (
                  <option key={m} value={m}>
                    {m.replace('-', ' 年 ')} 月
                  </option>
                ))}
              </select>
            </label>
          }
        />
        <div className="filter-bar">
          <div className="segments" aria-label="流水类型">
            {(
              [
                ['ALL', '全部'],
                ['EXPENSE', '消费'],
                ['INCOME', '收入'],
                ['TRANSFER', '转账'],
              ] as const
            ).map(([value, title]) => (
              <button
                key={value}
                aria-pressed={kind === value}
                className={kind === value ? 'active' : ''}
                onClick={() => setKind(value)}
              >
                {title}
              </button>
            ))}
          </div>
          <SearchField
            label="搜索流水"
            placeholder="搜索商户、分类"
            value={search}
            onChange={setSearch}
          />
        </div>
        {rows.length ? (
          <TransactionList
            rows={rows}
            onSelect={setSelected}
            feedbackIds={state.feedback.map((f) => f.transactionId)}
          />
        ) : (
          <Empty />
        )}
      </section>
      <p className="data-footnote">演示流水 · 金额汇总不代表账户余额变动</p>
      {selected && (
        <TransactionDetail transaction={selected} onClose={() => setSelected(undefined)} />
      )}
    </>
  );
}
