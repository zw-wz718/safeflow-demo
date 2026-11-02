type Decision = { status: string; decision: { minimum_level: string } };

export default function PaymentRiskNotice({ decisions }: { decisions: readonly Decision[] }) {
  const red = decisions.some((d) => d.status === 'ACTIVE' && d.decision.minimum_level === 'RED');
  const current = decisions.some((d) => d.status === 'ACTIVE');
  return (
    <div className="bank-payment-risk" role="status">
      {red && <p>本笔触发大额强验证要求：当日累计（含本笔）超过 ¥1,000.00，需本笔明确确认和强验证。这是验证门槛，付款时仍核对冻结状态、余额及授权。</p>}
      {!current && <p>当前金额检查需重新核验，请先核对原单。</p>}
      <p>付款使用公共最低级别检查；领域风险尚未评估（NOT_EVALUATED），不代表已完成全部风险检查。</p>
    </div>
  );
}
