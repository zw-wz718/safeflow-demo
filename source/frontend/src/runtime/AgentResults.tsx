import type { z } from 'zod';
import type { resultSchema, AgentView } from './agent-client';
import type { BillingData } from './billing-schema';
import {
  originalRequiresRecovery,
  fundsRequireRecovery,
  type PaymentOriginal,
  type PaymentFunds,
} from './payment-schema';
import { CheckCheck, FileCheck, RefreshCw, Search, X } from 'lucide-react';
import PaymentRiskNotice from './PaymentRiskNotice';

export function ResultRows({ result }: { result: z.infer<typeof resultSchema> }) {
  if (result.tool_name === 'analyze_local_basic_spending')
    return <BillingReport data={result.data} />;
  if (result.tool_name === 'get_account_balance') {
    const d = result.data;
    return (
      <div className="a-balance">
        <span>
          {d.account.display_name} {d.account.masked_account_no}
        </span>
        <strong>
          {d.available_balance} <small>CNY</small>
        </strong>
        <time>{d.as_of}</time>
      </div>
    );
  }
  return (
    <ul className="a-results">
      {result.data.candidates.map((p) => (
        <li key={p.payee_id}>
          <strong>{p.display_name}</strong>
          <span>
            {p.bank_name} {p.masked_account_no}
          </span>
          <code>{p.payee_id}</code>
        </li>
      ))}
    </ul>
  );
}
export function BillingReport({ data }: { data: BillingData }) {
  const r = data.report,
    w = r.request.window;
  const ready =
    r.read.is_complete &&
    r.read.state === 'COMPLETE' &&
    r.calculation_status === 'READY' &&
    r.summary;
  return (
    <section className="a-billing" aria-label="本地账单报告">
      <h3>基础消费报告</h3>
      <p className="a-expiry">
        {w.start_inclusive} 至 {w.end_exclusive}（不含） · {w.timezone}
      </p>
      <p className={`r-state ${ready ? 'COMPLETED' : 'WAITING_EXTERNAL'}`}>
        {ready ? '已取全 · 汇总就绪' : '报告未就绪 · 数据需核对'}
      </p>
      <p>
        读取：<code>{r.read.state}</code> · 计算：<code>{r.calculation_status}</code>
      </p>
      {(r.read.reason || r.calculation_reason) && (
        <p className="a-expiry">
          {r.read.reason} {r.calculation_reason}
        </p>
      )}
      <div className="a-balance">
        <span>消费毛额</span>
        <strong>
          {ready ? r.summary!.gross_consumption : '暂不可用'} {ready && <small>CNY</small>}
        </strong>
      </div>
      {ready && (
        <>
          <h4>分类汇总 · {r.summary!.count} 笔</h4>
          <div className="a-report-table">
            <table>
              <thead>
                <tr>
                  <th>分类</th>
                  <th>笔数</th>
                  <th>金额（元）</th>
                </tr>
              </thead>
              <tbody>
                {r.summary!.categories.map((c) => (
                  <tr key={c.category_code ?? 'uncategorized'}>
                    <td>{c.display_name}</td>
                    <td>{c.count}</td>
                    <td>{c.total}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          <h4>消费前五笔</h4>
          <ReportTransactions rows={r.summary!.top_five} />
        </>
      )}
      <h4>退款 · 单独列示</h4>
      {r.refunds === null ? (
        <p>退款数据暂不可用</p>
      ) : (
        <ReportTransactions rows={r.refunds} refund />
      )}
      {data.adapter_contract === 'LOCAL_BILLING_REPORT_ADAPTER_V2' && (
        <DuplicateCandidates data={data} />
      )}
      <details>
        <summary>报告来源与读取记录</summary>
        <dl className="a-draft-fields">
          <div>
            <dt>报告号</dt>
            <dd>{r.metadata.report_id}</dd>
          </div>
          <div>
            <dt>来源版本</dt>
            <dd>{data.source_revision}</dd>
          </div>
          <div>
            <dt>来源模式</dt>
            <dd>{data.source_mode}</dd>
          </div>
          <div>
            <dt>正式消费来源</dt>
            <dd>未接入</dd>
          </div>
          <div>
            <dt>读取页数</dt>
            <dd>{r.read.pages_read}</dd>
          </div>
          <div>
            <dt>规则版本</dt>
            <dd>{r.rule_version}</dd>
          </div>
          <div>
            <dt>计算时间</dt>
            <dd>{r.metadata.calculated_at}</dd>
          </div>
          <div>
            <dt>净额</dt>
            <dd>未计算 · 退款口径待确认</dd>
          </div>
        </dl>
      </details>
    </section>
  );
}
function DuplicateCandidates({
  data,
}: {
  data: Extract<BillingData, { adapter_contract: 'LOCAL_BILLING_REPORT_ADAPTER_V2' }>;
}) {
  const result = data.duplicate_candidates;
  const rows = new Map(
    (data.report.evidence ?? []).map((row) => [JSON.stringify(row.reference), row.transaction]),
  );
  return (
    <section className="a-duplicates" aria-label="重复消费候选">
      <h4>本地重复消费候选 · 仅供核对</h4>
      {'Err' in result ? (
        <p>
          候选判断不可用 · <code>{result.Err}</code>
        </p>
      ) : (
        <>
          <p>
            {result.Ok.state === 'COMPLETE'
              ? result.Ok.pairs!.length
                ? `${result.Ok.pairs!.length} 组候选 · 待核对`
                : '本规则未发现重复消费候选'
              : result.Ok.state === 'PAIR_LIMIT_EXCEEDED'
                ? '候选超过1000组上限 · 结果不可用'
                : '商户证据不足 · 无法判断全部交易'}
          </p>
          <p className="a-expiry">规则：{result.Ok.rule_version}</p>
          <p className="a-expiry">
            报告：{result.Ok.report_id} · 快照：{result.Ok.snapshot_id} · 来源：
            {result.Ok.source_id}
          </p>
          {!!result.Ok.pairs?.length && (
            <ul className="a-results">
              {result.Ok.pairs.map((pair, i) => (
                <li key={i}>
                  <strong>候选 {i + 1} · 待核对</strong>
                  {[pair.first, pair.second].map((ref) => {
                    const row = rows.get(JSON.stringify(ref))!;
                    return (
                      <span key={JSON.stringify(ref)}>
                        来源 {ref.source_id} · 账户 {ref.account_id} · 交易 {ref.transaction_id}
                        <br />
                        {row.amount} {row.currency} · {row.completed_at}
                      </span>
                    );
                  })}
                </li>
              ))}
            </ul>
          )}
          {!!result.Ok.missing_merchant.length && (
            <>
              <h4>缺少可信商户证据</h4>
              <ul className="a-results">
                {result.Ok.missing_merchant.map((ref) => (
                  <li key={JSON.stringify(ref)}>
                    来源 {ref.source_id} · 账户 {ref.account_id} · 交易 {ref.transaction_id}
                  </li>
                ))}
              </ul>
            </>
          )}
        </>
      )}
    </section>
  );
}
function ReportTransactions({
  rows,
  refund = false,
}: {
  rows: NonNullable<BillingData['report']['evidence']>;
  refund?: boolean;
}) {
  if (!rows.length) return <p>{refund ? '本报告无退款记录' : '本报告无消费记录'}</p>;
  return (
    <ul className="a-results">
      {rows.map(({ reference: ref, transaction: t }) => (
        <li key={JSON.stringify(ref)}>
          <strong>
            {t.amount} {t.currency}
          </strong>
          <span>
            {t.category_code ?? '未分类'} · {t.completed_at ?? '完成时间未提供'}
          </span>
          <span>
            账户 {t.account_id} · 交易 {t.transaction_id}
          </span>
          {refund && <span>原单：{t.original_tx_id ?? '未提供 · 待核对'}</span>}
        </li>
      ))}
    </ul>
  );
}
const flowLabels = {
  WAITING_PAYMENT_CONFIRMATION: '等待独立付款确认',
  WAITING_EXECUTION_CONDITIONS: '付款意愿已确认 · 等待执行条件',
  WAITING_EXPLICIT_PAYMENT: '信息已确认 · 等待确认付款',
  ENDED_INVALIDATED: '付款准备已结束 · 确认失效',
  QUERY_ORIGINAL: '已有原单 · 仅查询恢复',
};
const originalLabels: Record<PaymentOriginal['state'], string> = {
  UNSUBMITTED: '当前快照未发现提交记录',
  REGISTERED_UNSUBMITTED: '内部已登记 · 未提交付款',
  PROCESSING: '原单处理中',
  SUCCEEDED: '原单成功（已核验）',
  FAILED: '原单失败（已核验）',
  RECONCILE_REQUIRED: '原单结果待核对',
  SOURCE_CONFLICT: '原单来源冲突 · 待核对',
};
const freshness: Record<string, string> = {
  NOT_EVALUATED: '尚未评估',
  CURRENT_MINIMUM_ONLY: '最低等级当前有效',
  CURRENT_OBSERVATION_ONLY: '当前观察',
  HISTORICAL_REEVALUATION_REQUIRED: '历史记录 · 需要重新评估',
  FACTS_UNAVAILABLE: '当前事实不可用',
  PREPARATION_INVALIDATED: '付款准备已失效',
  QUERY_ORIGINAL_BUSINESS_OPERATION: '需查询业务原单',
};
export function PaymentPanel({
  view,
  original,
  enabled,
  fresh,
  expired,
  onAction,
  onOriginal,
  onRefresh,
  onExecute,
  funds = null,
  attempted = false,
  customer = false,
  writeBlocked = false,
}: {
  view: Extract<AgentView, { kind: 'PAYMENT' }>;
  original: PaymentOriginal | null;
  enabled: boolean;
  fresh: boolean;
  expired: boolean;
  onAction: (action: 'CONFIRM' | 'REJECT' | 'EVALUATE') => void;
  onOriginal: () => void;
  onRefresh: () => void;
  onExecute?: () => void;
  funds?: PaymentFunds | null;
  attempted?: boolean;
  customer?: boolean;
  writeBlocked?: boolean;
}) {
  const p = view.payment,
    f = view.payment_flow,
    d = p.snapshot.display_details,
    t = p.snapshot.transfer;
  const o = original ?? f.original;
  const recovery =
    attempted ||
    (!!funds && fundsRequireRecovery(funds)) ||
    p.business_recovery_required ||
    f.status === 'QUERY_ORIGINAL' ||
    originalRequiresRecovery(o);
  const actionable =
    enabled && fresh && !expired && !p.invalid_reason && !recovery && !writeBlocked;
  return (
    <section className="a-payment-result" aria-label="独立付款准备">
      <span className="r-state WAITING_EXTERNAL">
        {flowLabels[recovery ? 'QUERY_ORIGINAL' : f.status]}
      </span>
      <h3>付款确认单</h3>
      <dl className="a-draft-fields">
        <div>
          <dt>付款金额</dt>
          <dd className="a-payment-amount">
            {t.amount} {t.currency}
          </dd>
        </div>
        <div>
          <dt>付款账户</dt>
          <dd>
            {d.account.display_name} · {d.account.masked_account_no}
            {!customer && (
              <>
                <br />
                {t.payer_account_id}
              </>
            )}
          </dd>
        </div>
        <div>
          <dt>收款人</dt>
          <dd>
            {d.payee.display_name} · {d.payee.bank_name} · {d.payee.masked_account_no}
            {!customer && (
              <>
                <br />
                {t.payee_id}
              </>
            )}
          </dd>
        </div>
        <div>
          <dt>备注</dt>
          <dd className="a-preserve">{t.remark === '' ? '无' : t.remark}</dd>
        </div>
        {!customer && (
          <div>
            <dt>业务草案</dt>
            <dd>
              {t.draft_id} · 版本 {t.draft_version}
            </dd>
          </div>
        )}
        <div>
          <dt>业务原号</dt>
          <dd>{t.operation_id}</dd>
        </div>
        <div>
          <dt>确认有效至</dt>
          <dd>{new Date(p.expires_at).toLocaleString('zh-CN')}</dd>
        </div>
        <div>
          <dt>草案有效至</dt>
          <dd>{new Date(p.snapshot.draft_expires_at * 1000).toLocaleString('zh-CN')}</dd>
        </div>
      </dl>
      <p className="a-payment-boundary">
        {recovery
          ? '请保留原号查询付款结果。'
          : p.execution?.enabled
            ? '模拟付款 · 确认信息后，仍需单独确认付款。'
            : '付款暂不可用：账户卡片资料尚未采用。可保存并核对草案。'}
      </p>
      {expired && !p.invalid_reason && !recovery && (
        <p role="status">确认已到期 · 请查询原任务核对状态</p>
      )}
      {p.invalid_reason && !recovery && (
        <p role="status">
          {customer ? '本次付款确认已失效，请查询原任务。' : `失效原因：${p.invalid_reason}`}
        </p>
      )}
      {p.confirmed_at && (
        <p className="a-expiry">
          <CheckCheck size={16} /> 付款意愿确认于 {new Date(p.confirmed_at).toLocaleString('zh-CN')}
        </p>
      )}
      <div className="a-actions">
        {!recovery && f.status === 'WAITING_PAYMENT_CONFIRMATION' && (
          <>
            <button
              className="r-command primary"
              disabled={!actionable}
              onClick={() => onAction('CONFIRM')}
            >
              <FileCheck size={17} />
              确认付款信息
            </button>
            <button className="r-command" disabled={!actionable} onClick={() => onAction('REJECT')}>
              <X size={17} />
              拒绝付款
            </button>
          </>
        )}
        {!recovery && f.status === 'WAITING_EXECUTION_CONDITIONS' && (
          <button className="r-command" disabled={!actionable} onClick={() => onAction('EVALUATE')}>
            <RefreshCw size={17} />
            重新评估付款条件
          </button>
        )}
        {!recovery && f.status === 'WAITING_EXPLICIT_PAYMENT' && (
          <button
            className="r-command primary"
            disabled={!actionable || !p.execution?.enabled || !onExecute}
            onClick={onExecute}
          >
            <CheckCheck size={17} />
            确认付款
          </button>
        )}
        <button className="r-command" disabled={!enabled} onClick={onRefresh}>
          <RefreshCw size={17} />
          刷新付款状态
        </button>
        <button
          className="r-command"
          disabled={!enabled || (!fresh && !attempted)}
          onClick={onOriginal}
        >
          <Search size={17} />
          查询付款原单
        </button>
      </div>
      <PaymentRiskNotice decisions={p.security_decisions} />
      {!customer && (
        <div className="a-payment-checks">
          <h4>付款检查</h4>
          <p>最低等级：{freshness[p.decision_freshness]}</p>
          {p.security_decisions.map((s) => (
            <p key={s.stored_decision_id}>
              {s.decision.minimum_level} · {s.status} · 本日累计加本笔{' '}
              {(BigInt(s.assembly_metadata.facts.combined_cents) / 100n).toString()}.
              {(BigInt(s.assembly_metadata.facts.combined_cents) % 100n)
                .toString()
                .padStart(2, '0')}{' '}
              元
            </p>
          ))}
          <p>业务预检：{freshness[p.business_preflight.freshness]}</p>
          {p.business_preflight.observations.slice(-1).flatMap((o) =>
            o.findings.map((f, i) => (
              <p key={`${o.observation_id}-${i}`}>
                {f.kind} · {f.reason_code}
              </p>
            )),
          )}
          <p className="a-expiry">
            {p.execution?.enabled
              ? '最终规则与卡状态在确认付款时重新检查。'
              : '最终付款条件尚未采用。'}
          </p>
        </div>
      )}
      {funds && <FundsResult result={funds} />}
      {attempted && !funds && <p role="status">付款结果待核对 · 请查询付款原单，不要重复付款。</p>}
      {o && !funds && (
        <div className="a-original" aria-label="付款原单结果">
          <h4>{originalLabels[o.state]}</h4>
          <p>{o.reason}</p>
          <p>业务原号：{o.binding.business_operation_id}</p>
          {o.verified_business_result && (
            <p>业务回执：{o.verified_business_result.latest_receipt.status}</p>
          )}
        </div>
      )}
    </section>
  );
}
export function FundsResult({ result: r }: { result: PaymentFunds }) {
  return (
    <div className="a-original" aria-label="付款原单结果">
      <h4>{originalLabels[r.state]}</h4>
      <p>
        {r.transfer.amount} {r.transfer.currency} · 原号 {r.transfer.operation_id}
      </p>
      {r.state === 'PROCESSING' && <p>请稍后查询原单。当前不能重新付款。</p>}
      {['RECONCILE_REQUIRED', 'SOURCE_CONFLICT'].includes(r.state) && (
        <p>结果尚未核实，请保留原号继续查询。</p>
      )}
    </div>
  );
}
