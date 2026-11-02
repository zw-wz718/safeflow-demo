import { useState } from 'react';
import { ArrowUpRight, CreditCard, LockKeyhole, ShieldCheck } from 'lucide-react';
import { useWorkbench } from '../context';
import { accounts, cards } from '../domain/fixtures';
import type { DemoCard } from '../domain/model';
import { BankCard, DetailRows, Modal, Notice, PageTitle, SectionTitle, Tag } from '../components';
import CardReadPanel from './CardReadPanel';

export default function Cards() {
  const { notify } = useWorkbench();
  const [selected, setSelected] = useState<DemoCard>();
  const [lock, setLock] = useState(false);
  const account = selected && accounts.find((a) => a.id === selected.accountId)!;
  return (
    <>
      <PageTitle
        title="银行卡"
        description="账户与卡片，一目了然。"
        action={<Tag>2 张模拟借记卡</Tag>}
      />
      <CardReadPanel />
      <Notice>固定卡片样例。当前没有银行连接，卡片状态不会随本地操作改变。</Notice>
      <div className="cards-grid">
        {cards.map((card) => {
          const account = accounts.find((a) => a.id === card.accountId)!;
          return (
            <article className="card-item" key={card.id}>
              <BankCard card={card} account={account} />
              <div className="card-caption">
                <div>
                  <h2>{card.name}</h2>
                  <p>
                    {account.name} · {card.type}
                  </p>
                </div>
                <Tag tone="green">样例：正常</Tag>
              </div>
              <div className="card-item-meta">
                <span>
                  <CreditCard size={15} />
                  尾号 {card.tail}
                </span>
                <span>CNY · 人民币</span>
              </div>
              <div className="card-item-actions">
                <button
                  className="button secondary"
                  onClick={() => {
                    setSelected(card);
                    setLock(false);
                  }}
                >
                  卡片详情
                  <ArrowUpRight size={16} />
                </button>
                <button
                  className="button secondary"
                  onClick={() => {
                    setSelected(card);
                    setLock(true);
                  }}
                >
                  <LockKeyhole size={16} />
                  锁卡核对
                </button>
              </div>
            </article>
          );
        })}
      </div>
      <section className="card-safety">
        <SectionTitle title="卡片安全" description="锁卡需单独核对并办理。" />
        <div className="safety-row">
          <ShieldCheck size={22} />
          <div>
            <h3>反馈与锁卡分别处理</h3>
            <p>账单异常反馈仅保存在本机。锁卡服务暂未开通，不会改变卡片状态。</p>
          </div>
          <button
            className="text-button"
            onClick={() => notify('锁卡服务暂未开通，当前仅可查看演示卡片，卡片状态不会改变。')}
          >
            查看说明
            <ArrowUpRight size={16} />
          </button>
        </div>
      </section>
      {selected && account && (
        <Modal title={lock ? '核对锁卡对象' : '卡片详情'} onClose={() => setSelected(undefined)}>
          <div className="mini-card-header">
            <CreditCard size={30} />
            <div>
              <h3>{selected.name}</h3>
              <span>尾号 {selected.tail} · 固定样例</span>
            </div>
            <Tag tone="green">正常</Tag>
          </div>
          <DetailRows
            rows={[
              ['卡号', `•••• •••• •••• ${selected.tail}`],
              ['关联账户', `${account.name} · ${account.tail}`],
              ['卡片类型', selected.type],
              ['币种', 'CNY'],
              ['当前状态', '固定样例：正常'],
            ]}
          />
          {lock ? (
            <>
              <Notice tone="amber">
                锁卡服务暂未开通。当前只能核对卡片信息，不会锁卡或改变卡片状态。
              </Notice>
              <button className="button unavailable" disabled title="锁卡服务暂未开通">
                确认锁卡 · 暂未开通
              </button>
              <div className="modal-actions">
                <button className="button secondary" onClick={() => setSelected(undefined)}>
                  关闭核对
                </button>
              </div>
            </>
          ) : (
            <div className="modal-actions">
              <button className="button secondary" onClick={() => setSelected(undefined)}>
                关闭
              </button>
              <button className="button primary" onClick={() => setLock(true)}>
                <LockKeyhole size={17} />
                核对锁卡对象
              </button>
            </div>
          )}
        </Modal>
      )}
    </>
  );
}
