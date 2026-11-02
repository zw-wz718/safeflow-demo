import { useState } from 'react';
import { ArrowRight, Check, ClipboardList, Search } from 'lucide-react';
import { useWorkbench } from '../context';
import type { LocalRecord } from '../domain/model';
import { assistantRecordText } from '../domain/assistant-copy';
import {
  DetailRows,
  draftChangeLabel,
  Empty,
  Modal,
  Notice,
  PageTitle,
  SearchField,
  SectionTitle,
  Tag,
} from '../components';

const categoryLabels = {
  query: '查询 / 助手',
  draft: '转账汇款',
  feedback: '账单反馈',
  subscription: '订阅管理',
};

// Present the local event without turning internal task/security fields into bank results.
function recordOutcome(record: LocalRecord) {
  if (record.task === 'FAILED') return ['未办理成功', '请核对信息后重试，未取得银行办理结果。'];
  if (record.category === 'draft') {
    return record.task === 'CANCELLED'
      ? ['草案已取消', '仅取消本机草案，未发生资金变动。']
      : ['草案已保存', '仅保存本机草案，尚未转账。'];
  }
  if (record.category === 'feedback') return ['反馈已本地登记', '未提交银行，未触发锁卡。'];
  if (record.category === 'subscription') {
    return record.task === 'WAITING_USER'
      ? ['取消意图已登记', '商户订阅未取消，请前往商户办理。']
      : ['提醒偏好已保存', '设置仅在本机演示中生效，不发送外部通知。'];
  }
  return record.task === 'WAITING_USER'
    ? ['待填写转账信息', '助手仅引导填写草案，不会执行转账。']
    : ['已展示演示信息', '来自演示数据，未查询真实银行。'];
}

function recordTitle(record: LocalRecord) {
  return record.category === 'query' && record.title === '助手本地意图处理'
    ? '智能助手咨询'
    : record.title;
}

function recordDetail(record: LocalRecord) {
  // Translate only the old generated templates; never replace words inside customer feedback.
  if (record.category === 'query') return assistantRecordText(record);
  if (record.category === 'draft' && record.title === '修改转账草案') {
    return record.detail.replace(
      /^版本 v\d+ → v(\d+) · ¥(\d+\.\d{2})，仍未执行。$/,
      (_, version: string, amount: string) =>
        `${draftChangeLabel(Number(version))} · ¥${amount}，仍未执行。`,
    );
  }
  if (record.category === 'draft' && record.title === '保存转账草案') {
    return record.detail.replace(
      /^(.* · ¥\d+\.\d{2}) · v1，仅保存浏览器，未执行转账。$/,
      '$1，仅保存在本机，未执行转账。',
    );
  }
  if (
    record.category === 'subscription' &&
    ['开启订阅提醒', '关闭订阅提醒'].includes(record.title)
  ) {
    return record.detail.replace(
      /^(.+)的工作台提醒已(开启|关闭)，不发送系统或外部通知。$/,
      '$1的订阅提醒已$2，不发送外部通知。',
    );
  }
  return record.detail;
}

export default function Activity() {
  const { state, navigate } = useWorkbench();
  const [search, setSearch] = useState('');
  const [category, setCategory] = useState('all');
  const [selected, setSelected] = useState<LocalRecord>();
  const rows = state.records.filter(
    (r) =>
      (category === 'all' || r.category === category) &&
      `${recordTitle(r)} ${recordDetail(r)} ${r.id} ${r.reference}`
        .toLowerCase()
        .includes(search.toLowerCase()),
  );
  return (
    <>
      <PageTitle
        title="办理记录"
        description="查看这台浏览器上的演示操作。"
        action={<Tag>本地演示记录</Tag>}
      />
      <Notice>这里记录本机演示中的办理过程，不代表银行回执或资金结果。</Notice>
      <section className="activity-section">
        <SectionTitle title="全部记录" description={`${rows.length} 条 · 最多保留最近 200 条`} />
        <div className="filter-bar">
          <SearchField
            label="搜索办理记录"
            placeholder="搜索办理事项或内容"
            value={search}
            onChange={setSearch}
          />
          <label className="select-label">
            <span className="sr-only">记录类型</span>
            <select
              aria-label="记录类型"
              value={category}
              onChange={(e) => setCategory(e.target.value)}
            >
              <option value="all">所有类型</option>
              {Object.entries(categoryLabels).map(([value, label]) => (
                <option key={value} value={value}>
                  {label}
                </option>
              ))}
            </select>
          </label>
        </div>
        {rows.length ? (
          <div className="activity-list">
            {rows.map((r) => (
              <button
                className="activity-row"
                key={r.id}
                onClick={() => setSelected(r)}
                aria-label={`查看${recordTitle(r)}记录`}
              >
                <span
                  className={`activity-icon ${r.task === 'CANCELLED' ? 'neutral' : r.task === 'WAITING_USER' ? 'amber' : 'green'}`}
                >
                  {r.category === 'query' ? (
                    <Search size={19} />
                  ) : r.category === 'draft' ? (
                    <ClipboardList size={19} />
                  ) : (
                    <Check size={19} />
                  )}
                </span>
                <div className="activity-main">
                  <strong>{recordTitle(r)}</strong>
                  <p>{recordDetail(r)}</p>
                  <div className="state-tags">
                    <span>{recordOutcome(r)[0]}</span>
                    <span>来源：本机演示</span>
                  </div>
                </div>
                <div className="activity-time">
                  <span>
                    {new Date(r.at).toLocaleDateString('zh-CN', {
                      month: '2-digit',
                      day: '2-digit',
                    })}
                  </span>
                  <small>
                    {new Date(r.at).toLocaleTimeString('zh-CN', {
                      hour: '2-digit',
                      minute: '2-digit',
                    })}
                  </small>
                </div>
                <ArrowRight size={17} />
              </button>
            ))}
          </div>
        ) : (
          <Empty
            title={search || category !== 'all' ? '没有匹配的记录' : '还没有本地操作记录'}
            detail="保存草案、查询助手或登记反馈后，记录会显示在这里。"
            action={
              <button className="button secondary" onClick={() => navigate('agent')}>
                打开助手
                <ArrowRight size={16} />
              </button>
            }
          />
        )}
      </section>
      {selected && (
        <Modal title="本地记录详情" onClose={() => setSelected(undefined)}>
          <Tag>本地演示记录</Tag>
          <h3 className="record-detail-title">{recordTitle(selected)}</h3>
          <p className="record-detail-text">{recordDetail(selected)}</p>
          <DetailRows
            rows={[
              ['时间', new Date(selected.at).toLocaleString('zh-CN')],
              ['来源', '本机演示'],
              ['办理状态', recordOutcome(selected)[0]],
              ['办理结果', recordOutcome(selected)[1]],
            ]}
          />
          <Notice>这是一份本机办理记录，没有银行回执或真实资金变更。</Notice>
          <div className="modal-actions">
            <button className="button secondary" onClick={() => setSelected(undefined)}>
              关闭详情
            </button>
          </div>
        </Modal>
      )}
    </>
  );
}
