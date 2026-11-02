import { useEffect, useRef, useState } from 'react';
import { z } from 'zod';
import { request } from './client';
import { type LifecycleView } from './agent-client';
import {
  paymentBase,
  paymentSchema,
  fundsResultSchema,
  fundsMatches,
  originalResultSchema,
  originalMatches,
  type Payment,
  type PaymentFunds,
} from './payment-schema';
import DemoVerification from './DemoVerification';
import PaymentRiskNotice from './PaymentRiskNotice';

const displaySchema = z
  .object({
    contract_version: z.literal(1),
    consent_id: z.string().min(1),
    display: z
      .object({ explicit_text: z.string().min(1), duration_minutes: z.number().int().positive() })
      .passthrough(),
    display_digest: z.string().regex(/^[a-f0-9]{64}$/),
  })
  .strict();
const labels: Record<string, string> = {
  UNSUBMITTED: '尚未提交',
  REGISTERED_UNSUBMITTED: '已登记，尚未提交',
  PROCESSING: '已受理，正在等待模拟到账',
  SUCCEEDED: '模拟转账已完成',
  FAILED: '模拟转账未完成',
  RECONCILE_REQUIRED: '原单结果待核对',
  SOURCE_CONFLICT: '来源不一致，等待核对',
  UNKNOWN: '结果未知，请查询原单',
};
export default function ChatPayment({
  draft,
  disabled,
  onBusy,
  onSubmitted,
  onOutcome,
  onVerified,
  onUncertain,
}: {
  draft: LifecycleView;
  disabled: boolean;
  onBusy: (work: (signal: AbortSignal) => Promise<void>) => Promise<void>;
  onSubmitted: () => void;
  onOutcome: (funds: PaymentFunds, completedAt?: string) => void;
  onVerified: (state: PaymentFunds['state'] | 'UNKNOWN') => void;
  onUncertain: () => void;
}) {
  const [payment, setPayment] = useState<Payment>(),
    [funds, setFunds] = useState<PaymentFunds>(),
    [completedAt, setCompletedAt] = useState<string>(),
    [delegation, setDelegation] = useState<z.infer<typeof displaySchema>>(),
    [consented, setConsented] = useState(false),
    [verified, setVerified] = useState(false),
    [code, setCode] = useState(''),
    [notice, setNotice] = useState(''),
    [loading, setLoading] = useState(true);
  const attemptedKey = `safeflow.chat.payment.attempted.${draft.task_id}`;
  const [attempted, setAttempted] = useState(() => sessionStorage.getItem(attemptedKey) === '1');
  const preparationKey = `safeflow.chat.payment.preparing.${draft.task_id}`;
  const [prepareUnknown, setPrepareUnknown] = useState(
    () => sessionStorage.getItem(preparationKey) === '1',
  );
  function check(p: Payment) {
    if (p.task_id !== draft.task_id) throw Error('付款原号不匹配，已停止');
    return p;
  }
  async function read(signal: AbortSignal) {
    const list = z
      .array(paymentSchema)
      .parse(
        await request(
          `/api/sim/payment-preparations/${encodeURIComponent(draft.task_id)}`,
          'GET',
          undefined,
          signal,
        ),
      );
    const p = list.at(-1);
    if (signal.aborted) return;
    if (p) {
      check(p);
      const f = fundsResultSchema.parse(
        await request(paymentBase(p) + '/funds-result', 'GET', undefined, signal),
      );
      if (!fundsMatches(p, f)) throw Error('资金原号不匹配');
      if (!signal.aborted) {
        setPayment(p);
        setFunds(f);
        setPrepareUnknown(false);
        const unresolvedAttempt =
          sessionStorage.getItem(attemptedKey) === '1' &&
          ['UNSUBMITTED', 'REGISTERED_UNSUBMITTED'].includes(f.state);
        if (unresolvedAttempt) setNotice('原付款提交尚未核实，只查询原单。');
        onVerified(unresolvedAttempt ? 'UNKNOWN' : f.state);
      }
    } else {
      // An unresolved local write marker can only restrict, never authorize.
      // Empty server results cannot silently release a lost preparation/submit.
      const unresolved =
        sessionStorage.getItem(attemptedKey) === '1' ||
        sessionStorage.getItem(preparationKey) === '1';
      if (unresolved) {
        setPrepareUnknown(true);
        setNotice('原付款准备或提交尚未核实，只查询原单。');
      }
      onVerified(unresolved ? 'UNKNOWN' : 'UNSUBMITTED');
    }
    if (!signal.aborted) setLoading(false);
  }
  useEffect(() => {
    const c = new AbortController();
    void read(c.signal).catch(() => {
      if (!c.signal.aborted) {
        setNotice('原付款读取失败；请查询原号，暂不允许新提交');
        setPrepareUnknown(true);
        setLoading(false);
      }
    });
    return () => c.abort();
  }, [draft.task_id]);
  // Polling is GET-only and stops on unmount, terminal or unknown results.
  useEffect(() => {
    if (!payment || funds?.state !== 'PROCESSING') return;
    const c = new AbortController();
    let timer: ReturnType<typeof setTimeout>;
    async function poll() {
      try {
        const f = fundsResultSchema.parse(
          await request(paymentBase(payment!) + '/funds-result', 'GET', undefined, c.signal),
        );
        if (!fundsMatches(payment!, f)) throw Error('原号不符');
        if (!c.signal.aborted) {
          setFunds(f);
          if (f.state === 'PROCESSING') timer = setTimeout(() => void poll(), 2000);
        }
      } catch {
        if (!c.signal.aborted) setNotice('自动查询暂停，请手动查询原单。');
      }
    }
    timer = setTimeout(() => void poll(), 2000);
    return () => {
      clearTimeout(timer);
      c.abort();
    };
  }, [payment?.operation.operation_id, funds?.state]);
  useEffect(() => {
    setCompletedAt(undefined);
    if (!payment || funds?.state !== 'SUCCEEDED') return;
    const c = new AbortController();
    void request(paymentBase(payment) + '/original-result', 'GET', undefined, c.signal)
      .then((raw) => {
        const original = originalResultSchema.parse(raw);
        if (
          !originalMatches(payment, original) ||
          original.state !== 'SUCCEEDED' ||
          original.binding.execution_operation_id !== funds.execution_operation_id
        )
          throw Error('完成回执原号不匹配');
        const time = original.verified_business_result?.latest_receipt.completed_at;
        if (!c.signal.aborted && time) setCompletedAt(time);
      })
      .catch(() => {
        /* Funds remain authoritative; never invent a completion time. */
      });
    return () => c.abort();
  }, [payment, funds?.state, funds?.execution_operation_id]);
  const reported = useRef('');
  useEffect(() => {
    if (!funds) return;
    const key = JSON.stringify([
      funds.task_id,
      funds.execution_operation_id,
      funds.state,
      completedAt,
    ]);
    if (reported.current === key) return;
    reported.current = key;
    onOutcome(funds, completedAt);
  }, [funds, completedAt, onOutcome]);
  function act(work: (signal: AbortSignal) => Promise<void>) {
    void onBusy(async (signal) => {
      try {
        await work(signal);
      } catch (e) {
        if (!signal.aborted) {
          setNotice(e instanceof Error ? e.message : '响应未核实，只查询原单');
          if (sessionStorage.getItem(attemptedKey) === '1') onUncertain();
          setDelegation(undefined);
          setConsented(false);
          setVerified(false);
          setCode('');
        }
        throw e;
      }
    });
  }
  async function background(action: unknown, signal: AbortSignal) {
    return request(
      paymentBase(payment!) + '/background',
      'POST',
      { contract_version: 1, scope: 'EXPLICIT_PAYMENT_BACKGROUND_V1', action },
      signal,
      35000,
    );
  }
  const p = payment,
    d = p?.snapshot.display_details;
  const readonly =
    attempted ||
    p?.business_recovery_required ||
    !!p?.invalid_reason ||
    funds?.submitted ||
    !!(funds && !['UNSUBMITTED', 'REGISTERED_UNSUBMITTED'].includes(funds.state));
  const expired = p ? Date.parse(p.expires_at) <= Date.now() : false;
  const blocked = disabled || loading || !!readonly || expired;
  const terminal = funds?.state === 'SUCCEEDED' || funds?.state === 'FAILED';
  const outcome =
    funds?.state === 'SUCCEEDED'
      ? 'success'
      : funds?.state === 'FAILED'
        ? 'failure'
        : funds?.state === 'PROCESSING'
          ? 'processing'
          : funds && ['UNSUBMITTED', 'REGISTERED_UNSUBMITTED'].includes(funds.state)
            ? 'unsubmitted'
            : 'unknown';
  const heading =
    funds?.state === 'SUCCEEDED'
      ? '转账完成'
      : funds?.state === 'FAILED'
        ? '转账未完成'
        : funds?.state === 'PROCESSING'
          ? '转账处理中'
          : funds && outcome === 'unknown'
            ? '转账结果待核对'
            : '付款办理';
  return (
    <section
      className={'chat-business-card' + (terminal ? ' chat-payment-result' : '')}
      aria-label="聊天付款办理"
    >
      <h3>{heading}</h3>
      {notice && <p role="status">{notice}</p>}
      {p && (
        <>
          <strong className="chat-amount">¥{d?.amount ?? p.snapshot.transfer.amount}</strong>
          <dl className="chat-facts">
            {!terminal && (
              <>
                <dt>付款账户</dt>
                <dd>
                  {d?.account.display_name} {d?.account.masked_account_no}
                </dd>
              </>
            )}
            <dt>收款人</dt>
            <dd>
              {d?.payee.display_name} · {d?.payee.bank_name} · {d?.payee.masked_account_no}
            </dd>
            {!terminal && (
              <>
                <dt>备注</dt>
                <dd>{d?.remark || '无'}</dd>
              </>
            )}
          </dl>
        </>
      )}
      {funds && (
        <p className={`chat-outcome ${outcome}`} role="status">
          {labels[funds.state] ?? '状态尚未核实'}
          {funds.state === 'SUCCEEDED' ? '，以此原单资金回执为准。' : ''}
        </p>
      )}
      {!readonly && p && (
        <p role="status">
          {verified
            ? '已验证，待提交付款'
            : consented
              ? '已确认委托，待验证'
              : p.confirmation_status === 'CONFIRMED'
                ? '付款已确认，待办理委托和验证'
                : '付款待确认'}
        </p>
      )}
      {funds?.state === 'SUCCEEDED' && (
        <p>
          完成时间：
          {completedAt
            ? new Date(completedAt).toLocaleString('zh-CN', {
                timeZone: 'Asia/Shanghai',
                hour12: false,
              }) + '（北京时间）'
            : '暂未取得原回执时间'}
        </p>
      )}
      {expired && !readonly && <p>本次付款确认已过期，不能继续使用。</p>}
      {p?.invalid_reason && <p>付款准备已失效，请回查当前草案。</p>}
      {!p && !loading && (
        <button
          className="primary"
          disabled={
            disabled ||
            readonly ||
            prepareUnknown ||
            draft.execution_status !== 'SUCCEEDED' ||
            !draft.receipt
          }
          onClick={() =>
            act(async (signal) => {
              sessionStorage.setItem(preparationKey, '1');
              if (sessionStorage.getItem(preparationKey) !== '1')
                throw Error('不能保存付款准备原号');
              setPrepareUnknown(true);
              const v = check(
                paymentSchema.parse(
                  await request(
                    `/api/sim/payment-preparations/${encodeURIComponent(draft.task_id)}`,
                    'POST',
                    {
                      contract_version: 1,
                      scope: 'PAYMENT_INTENT_ONLY',
                      source: {
                        selection_ticket: draft.selection_ticket,
                        task_revision: draft.task_revision,
                        plan_version: draft.plan_version,
                        facts_version: draft.facts_version,
                        draft_id: draft.receipt!.draft_id,
                        draft_version: draft.receipt!.draft_version,
                      },
                    },
                    signal,
                  ),
                ),
              );
              if (!signal.aborted) {
                setPayment(v);
                sessionStorage.removeItem(preparationKey);
                setPrepareUnknown(false);
              }
            })
          }
        >
          查看付款信息并准备独立确认
        </button>
      )}
      {p && !readonly && (
        <>
          <p>草案已保存。付款、后台委托和模拟验证码分别确认。</p>
          <PaymentRiskNotice decisions={p.security_decisions} />
          <button
            disabled={blocked || p.confirmation_status !== 'PREPARED'}
            onClick={() =>
              act(async (signal) => {
                const next = check(
                  paymentSchema.parse(
                    await request(
                      paymentBase(p) + '/confirm',
                      'POST',
                      {
                        contract_version: 1,
                        scope: 'PAYMENT_INTENT_ONLY',
                        confirmation_ref: p.confirmation_ref,
                        display_ticket: p.display_ticket,
                        action: 'CONFIRM',
                      },
                      signal,
                    ),
                  ),
                );
                if (
                  next.operation.operation_id !== p.operation.operation_id ||
                  next.snapshot_digest !== p.snapshot_digest
                )
                  throw Error('付款版本不一致');
                if (!signal.aborted) setPayment(next);
              })
            }
          >
            确认这笔付款信息
          </button>
          <button
            disabled={blocked || p.confirmation_status !== 'CONFIRMED'}
            onClick={() =>
              act(async (signal) => {
                const shown = displaySchema.parse(await background({ kind: 'DISPLAY' }, signal));
                if (!signal.aborted) {
                  setDelegation(shown);
                  setConsented(false);
                  setVerified(false);
                  setCode('');
                }
              })
            }
          >
            查看本笔后台委托
          </button>
          {delegation && (
            <>
              <p>{delegation.display.explicit_text}</p>
              <p>委托时长：{delegation.display.duration_minutes} 分钟，仅适用于此笔原单。</p>
              <button
                disabled={blocked || consented}
                onClick={() =>
                  act(async (signal) => {
                    z.object({
                      delegation_confirmed: z.literal(true),
                      business_executed: z.literal(false),
                    })
                      .strict()
                      .parse(await background({ kind: 'CONFIRM', shown: delegation }, signal));
                    if (!signal.aborted) setConsented(true);
                  })
                }
              >
                明确同意此笔后台委托
              </button>
              {consented && (
                <>
                  <DemoVerification
                    key={delegation.consent_id}
                    operation={{
                      kind: 'PAYMENT',
                      task_id: p.task_id,
                      operation_id: p.operation.operation_id,
                      consent_id: delegation.consent_id,
                    }}
                    disabled={blocked}
                  />
                  <label>
                    手动填写本次付款模拟验证码
                    <input
                      value={code}
                      onChange={(e) => setCode(e.target.value)}
                      inputMode="numeric"
                      maxLength={6}
                      autoComplete="off"
                      disabled={blocked}
                    />
                  </label>
                  <button
                    disabled={blocked || !/^\d{6}$/.test(code)}
                    onClick={() =>
                      act(async (signal) => {
                        const v = z
                          .object({
                            verified: z.boolean(),
                            mode: z.literal('SIMULATED_PAYMENT_AUTH'),
                            business_executed: z.literal(false),
                          })
                          .strict()
                          .parse(
                            await background(
                              { kind: 'VERIFY_CODE', consent_id: delegation.consent_id, code },
                              signal,
                            ),
                          );
                        if (!signal.aborted) {
                          setVerified(v.verified);
                          setCode('');
                          if (!v.verified) setNotice('本次验证码未通过，请查询原付款状态，不要另建重付。');
                        }
                      })
                    }
                  >
                    核验模拟验证码
                  </button>
                  <button
                    className="primary"
                    disabled={blocked || !verified}
                    onClick={() =>
                      act(async (signal) => {
                        sessionStorage.setItem(attemptedKey, '1');
                        if (sessionStorage.getItem(attemptedKey) !== '1')
                          throw Error('无法保存原单恢复入口');
                        setAttempted(true);
                        setVerified(false);
                        setCode('');
                        onSubmitted();
                        await background(
                          {
                            kind: 'EXECUTE',
                            consent_id: delegation.consent_id,
                            confirmation_ref: p.confirmation_ref,
                            display_ticket: p.display_ticket,
                          },
                          signal,
                        );
                        if (!signal.aborted) {
                          setDelegation(undefined);
                          setConsented(false);
                          await read(signal);
                        }
                      })
                    }
                  >
                    提交 ¥{p.snapshot.transfer.amount} 模拟付款
                  </button>
                </>
              )}
            </>
          )}
        </>
      )}
      <button
        disabled={disabled || loading}
        onClick={() =>
          act(async (signal) => {
            await read(signal);
            if (!signal.aborted) {
              setDelegation(undefined);
              setConsented(false);
              setVerified(false);
              setCode('');
              setNotice('已重新读取原单；旧委托和验证码不会恢复。');
            }
          })
        }
      >
        只读查询此原单
      </button>
      <details>
        <summary>原单及审计说明</summary>
        <p>
          付款账户：{d?.account.display_name} {d?.account.masked_account_no}；备注：
          {d?.remark || '无'}
          <br />
          任务 {draft.task_id}
          <br />
          付款 {p?.operation.operation_id ?? '准备前待核对'}
          <br />
          {funds?.state === 'SUCCEEDED'
            ? '业务资金回执已核实。'
            : '确认、验证或受理均不表示到账。'}{' '}
          审计是否已 ACK 需另查实际发送记录。
        </p>
      </details>
    </section>
  );
}
