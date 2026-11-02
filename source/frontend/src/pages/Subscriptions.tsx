import { useState } from 'react';
import SubscriptionReadPanel from './SubscriptionReadPanel';
import { ArrowRight, Bell, CalendarDays, Repeat2 } from 'lucide-react';
import { useWorkbench } from '../context';
import { formatMoney, moneyFromCents, parseMoney } from '../domain/model';
import type { Subscription } from '../domain/model';
import {
  DetailRows,
  Empty,
  Mark,
  Modal,
  Notice,
  PageTitle,
  SearchField,
  SectionTitle,
  Tag,
} from '../components';

export default function Subscriptions() {
  const { fixtures, state, adapter, notify } = useWorkbench();
  const [filter, setFilter] = useState('all');
  const [search, setSearch] = useState('');
  const [selected, setSelected] = useState<Subscription>();
  const [cancel, setCancel] = useState(false);
  const pref = (id: string) =>
    state.subscriptionPrefs[id] ?? { reminder: true, cancelRequested: false };
  const rows = fixtures.subscriptions.filter(
    (s) =>
      `${s.name} ${s.category}`.toLowerCase().includes(search.toLowerCase()) &&
      (filter === 'all' ||
        (filter === 'pending' ? pref(s.id).cancelRequested : !pref(s.id).cancelRequested)),
  );
  const total = moneyFromCents(
    fixtures.subscriptions.reduce((n, s) => n + parseMoney(s.amount), 0n),
  );
  return (
    <>
      <PageTitle title="订阅管理" description="让周期性支出更有数。" action={<Tag>演示台账</Tag>} />
      <SubscriptionReadPanel />
      <SectionTitle title="界面操作样例" description="以下为本机样例，按钮只影响样例，不修改上方本人订阅资料。" />
      <div className="subscription-summary">
        <div>
          <span className="eyebrow">每月订阅合计</span>
          <strong>
            ¥{formatMoney(total)}
            <small>/ 月</small>
          </strong>
          <p>{fixtures.subscriptions.length} 项样例订阅 · 商户状态未查询</p>
        </div>
        <div className="subscription-summary-detail">
          <span>
            <CalendarDays size={18} />
            最近续费日期
          </span>
          <strong>
            {new Date(fixtures.subscriptions[0].nextDate).toLocaleDateString('zh-CN', {
              month: 'long',
              day: 'numeric',
            })}
          </strong>
          <small>网易云音乐 · ¥18.00</small>
        </div>
        <Repeat2 className="subscription-decoration" size={64} strokeWidth={1} />
      </div>
      <section className="subscriptions-section">
        <SectionTitle title="订阅台账" description="样例提醒与取消意图仅演示本页交互。" />
        <div className="filter-bar">
          <div className="segments">
            {[
              ['all', '全部'],
              ['active', '未登记取消'],
              ['pending', '取消意图已登记'],
            ].map(([value, label]) => (
              <button
                key={value}
                className={filter === value ? 'active' : ''}
                aria-pressed={filter === value}
                onClick={() => setFilter(value)}
              >
                {label}
              </button>
            ))}
          </div>
          <SearchField
            label="搜索订阅"
            placeholder="搜索订阅名称"
            value={search}
            onChange={setSearch}
          />
        </div>
        <div className="subscription-table-head">
          <span>订阅 / 周期</span>
          <span>下次日期</span>
          <span>金额</span>
          <span>订阅提醒</span>
          <span />
        </div>
        {rows.length ? (
          rows.map((s) => (
            <div className="subscription-row" key={s.id}>
              <button
                className="subscription-name"
                onClick={() => {
                  setSelected(s);
                  setCancel(false);
                }}
              >
                <Mark text={s.mark} color={s.color} />
                <span>
                  <strong>{s.name}</strong>
                  <small>
                    {s.category} · {s.cycle}
                  </small>
                  {pref(s.id).cancelRequested && <em>取消意图已登记</em>}
                </span>
              </button>
              <span className="subscription-date">
                {new Date(s.nextDate).toLocaleDateString('zh-CN', {
                  month: '2-digit',
                  day: '2-digit',
                })}
                <small>商户仍有效（样例）</small>
              </span>
              <strong className="subscription-money">¥{formatMoney(s.amount)}</strong>
              <label className="toggle-control">
                <input
                  type="checkbox"
                  role="switch"
                  aria-label={`${s.name}订阅提醒`}
                  checked={pref(s.id).reminder}
                  onChange={(e) => {
                    try {
                      adapter.setReminder(s.id, e.target.checked);
                      notify(`订阅提醒已${e.target.checked ? '开启' : '关闭'}`);
                    } catch (err) {
                      notify((err as Error).message, true);
                    }
                  }}
                />
                <span className="switch" />
                <span className="sr-only">订阅提醒</span>
              </label>
              <button
                className="icon-button"
                aria-label={`查看${s.name}订阅详情`}
                title="订阅详情"
                onClick={() => {
                  setSelected(s);
                  setCancel(false);
                }}
              >
                <ArrowRight size={18} />
              </button>
            </div>
          ))
        ) : (
          <Empty />
        )}
      </section>
      <Notice>关闭提醒或登记取消意图，不会取消商户订阅。实际取消请前往对应商户办理。</Notice>
      {selected && (
        <Modal title={cancel ? '登记取消意图' : '订阅详情'} onClose={() => setSelected(undefined)}>
          <div className="detail-hero">
            <Mark text={selected.mark} color={selected.color} />
            <h3>{selected.name}</h3>
            <strong>
              ¥{formatMoney(selected.amount)}
              <small> / 月</small>
            </strong>
            <Tag>商户仍有效 · 样例</Tag>
          </div>
          <DetailRows
            rows={[
              ['周期', selected.cycle],
              ['下次日期（样例）', new Date(selected.nextDate).toLocaleDateString('zh-CN')],
              ['订阅提醒', pref(selected.id).reminder ? '开启' : '关闭'],
              ['取消意图', pref(selected.id).cancelRequested ? '已本地登记' : '未登记'],
            ]}
          />
          {cancel ? (
            <>
              <Notice tone="amber">
                仅登记你的取消意图，不向商户发送请求。该订阅的实际状态保持不变。
              </Notice>
              <div className="modal-actions">
                <button className="button secondary" onClick={() => setCancel(false)}>
                  返回详情
                </button>
                <button
                  className="button primary"
                  onClick={() => {
                    try {
                      adapter.requestSubscriptionCancel(selected.id);
                      notify('取消意图已本地登记，商户订阅未取消');
                      setCancel(false);
                    } catch (err) {
                      notify((err as Error).message, true);
                    }
                  }}
                >
                  登记取消意图
                </button>
              </div>
            </>
          ) : (
            <div className="modal-actions">
              <button className="button secondary" onClick={() => setSelected(undefined)}>
                关闭
              </button>
              <button
                className="button secondary"
                disabled={pref(selected.id).cancelRequested}
                title={
                  pref(selected.id).cancelRequested
                    ? '取消意图已登记'
                    : '仅在本地登记，不取消商户订阅'
                }
                onClick={() => setCancel(true)}
              >
                <Bell size={16} />
                {pref(selected.id).cancelRequested ? '取消意图已登记' : '登记取消意图'}
              </button>
            </div>
          )}
        </Modal>
      )}
    </>
  );
}
