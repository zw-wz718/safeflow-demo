import { useEffect, useState, type ReactNode } from 'react';
import { BillingReport } from './AgentResults';
import { BillingRecoveryController, type FeedbackState } from './billing-recovery-controller';
import { feedbackEligible, type FeedbackIntent } from './billing-feedback-client';
import { FeedbackHistory } from './FeedbackHistory';

type Props = {
  authenticated: boolean;
  generation: number;
  busy: string | null;
  sessionControls: ReactNode;
  getGeneration: () => number;
  onAuthenticationLost: () => void;
};
export function BillsFeedbackWorkspace({
  authenticated,
  generation,
  busy,
  sessionControls,
  getGeneration,
  onAuthenticationLost,
}: Props) {
  const [state, setState] = useState<FeedbackState | null>(null);
  const [controller] = useState(
    () => new BillingRecoveryController(getGeneration, setState, onAuthenticationLost),
  );
  const [task, setTask] = useState('');
  const [operation, setOperation] = useState('');
  useEffect(() => {
    controller.activate();
    const check = () => {
      void controller.watchContext();
      void controller.watchRecovery();
    };
    const timer = setInterval(check, 3000);
    window.addEventListener('focus', check);
    window.addEventListener('storage', check);
    document.addEventListener('visibilitychange', check);
    return () => {
      clearInterval(timer);
      window.removeEventListener('focus', check);
      window.removeEventListener('storage', check);
      document.removeEventListener('visibilitychange', check);
      controller.deactivate();
    };
  }, [controller, generation, authenticated]);
  const s = state ?? controller.state;
  const disabled = !authenticated || !!busy || s.busy;
  const selected = s.selected === null ? undefined : s.reports[s.selected];
  function changeTask(value: string) {
    controller.reset();
    setTask(value);
  }
  return (
    <section className="b-feedback" aria-label="后台账单报告与反馈">
      <h2>后台报告与反馈</h2>
      <p>读取已保存的后台报告；不使用浏览器样例流水。反馈仅保存本人判断，不授权锁卡。</p>
      {sessionControls}
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (!disabled && task.trim()) void controller.load(task.trim());
        }}
      >
        <label>
          原报告任务号
          <input
            aria-label="原报告任务号"
            value={task}
            maxLength={200}
            onChange={(e) => changeTask(e.target.value)}
          />
        </label>
        <button className="button secondary" disabled={disabled || !task.trim()}>
          读取原报告
        </button>
      </form>
      {authenticated && (
        <>
          <p role="status" className="r-notice">
            {s.notice || (selected ? '请选择原报告交易及本人判断。' : '请输入已保存报告的任务号。')}
          </p>
          {s.storageError && <p role="alert">恢复记录不可用，已阻止写操作。</p>}
          {s.recovery.length > 0 && (
            <section aria-label="未核清反馈" className="b-recovery">
              <h3>未核清反馈 · {s.recovery.length} 项</h3>
              <p>
                保留全部原号。刷新或换会话会丢弃旧确认。可逐笔只读反查；尚未执行的准备可明确放弃后重新准备。已入场但结果未明时只能查询，404不能解除保护。
              </p>
              <ul>
                {s.recovery.map((r) => (
                  <li key={r.local_id}>
                    <code>
                      {r.protocol} · {r.nonce ?? r.local_id} · {r.operation_id ?? '原操作号待核对'}
                    </code>{' '}
                    <button
                      className="button secondary"
                      disabled={disabled}
                      onClick={() => void controller.recover(r)}
                    >
                      只读核对此条
                    </button>{' '}
                    {r.protocol === 'V2' && (
                      <button
                        className="button secondary"
                        disabled={disabled}
                        onClick={() => void controller.recover(r, true)}
                      >
                        明确放弃此准备
                      </button>
                    )}
                    {r.protocol === 'LEGACY' && (
                      <span>旧记录保留原件；无原号时需后台核对，不能自动关闭。</span>
                    )}
                  </li>
                ))}
              </ul>
            </section>
          )}
          {s.reports.length > 0 && (
            <fieldset>
              <legend>明确选择一份后台报告</legend>
              {s.reports.map((r, i) => (
                <label key={`${r.operation_id}/${r.result_ref}/${i}`} className="b-report-choice">
                  <input
                    type="radio"
                    name="billing-report"
                    checked={s.selected === i}
                    onChange={() => controller.select(i)}
                  />
                  报告 {i + 1} · 任务 {s.reportTask}
                  <br />
                  结果 {r.result_ref}
                  <br />
                  原操作 {r.operation_id}
                  <br />
                  报告 {r.data.report.metadata.report_id}
                </label>
              ))}
            </fieldset>
          )}
          {selected && (
            <>
              <BillingReport data={selected.data} />
              {!feedbackEligible(selected) ? (
                <p role="status">
                  报告可阅读；完整性或归档合同不足，不能准备反馈。旧报告不因此升级为可保存。
                </p>
              ) : (
                <>
                  <label>
                    原报告交易引用
                    <select
                      aria-label="原报告交易引用"
                      value={s.evidence ?? ''}
                      onChange={(e) =>
                        controller.select(
                          s.selected,
                          e.target.value === '' ? null : Number(e.target.value),
                          s.choice,
                        )
                      }
                    >
                      <option value="">请选择原报告中的真实引用</option>
                      {selected.data.report.evidence!.map((e, i) => (
                        <option key={i} value={i}>
                          {e.reference.source_id} / {e.reference.account_id} /{' '}
                          {e.reference.transaction_id}
                        </option>
                      ))}
                    </select>
                  </label>
                  <label>
                    本人判断
                    <select
                      aria-label="本人判断"
                      value={s.choice}
                      onChange={(e) =>
                        controller.select(
                          s.selected,
                          s.evidence,
                          e.target.value as FeedbackIntent['choice'] | '',
                        )
                      }
                    >
                      <option value="">请选择</option>
                      <option value="MINE">是本人交易</option>
                      <option value="NOT_MINE">不是本人交易</option>
                      <option value="UNSURE">暂不确定</option>
                    </select>
                  </label>
                  <p>是否有受信原归档由服务端最终核验。不是本人交易也不会自动锁卡。</p>
                  <button
                    className="button primary"
                    disabled={disabled || !controller.canPrepare()}
                    onClick={() => void controller.prepare()}
                  >
                    1. 准备反馈
                  </button>
                </>
              )}
            </>
          )}
          {s.prepared && (
            <section aria-label="核对原反馈参数">
              <h3>核对服务端原参数</h3>
              <p>
                反馈任务：<code>{s.prepared.task_id}</code>
                <br />
                反馈操作：<code>{s.prepared.operation_id}</code>
              </p>
              <pre>{JSON.stringify(s.prepared.parameters, null, 2)}</pre>
              <p>确认有效期至 {s.prepared.confirmation.expires_at}。准备与确认都不代表已保存。</p>
              <button
                className="button secondary"
                disabled={disabled || s.confirmed}
                onClick={() => void controller.confirm()}
              >
                2. 本人确认原参数
              </button>{' '}
              <button
                className="button primary"
                disabled={disabled || !s.confirmed}
                onClick={() => void controller.execute()}
              >
                3. 明确保存反馈
              </button>
            </section>
          )}
          {s.result && (
            <section aria-label="原反馈保存结果">
              <h3>反馈已保存 · 原号查询已核实</h3>
              <p>
                任务 {s.result.task_id}
                <br />
                操作 {s.result.operation_id}
                <br />
                {s.result.task_state} / {s.result.operation_state}
              </p>
              <p>未授权锁卡 · 无自动重试</p>
              <FeedbackHistory key={`${generation}/${s.result.operation_id}`} root={typeof s.result.saved.root_operation_id==='string'?s.result.saved.root_operation_id:s.result.operation_id} disabled={disabled} />
              <details>
                <summary>查看服务端原保存结果</summary>
                <pre>{JSON.stringify(s.result.saved, null, 2)}</pre>
              </details>
            </section>
          )}
        </>
      )}
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (!disabled && operation.trim()) void controller.query(operation.trim());
        }}
      >
        <label>
          已有反馈操作号
          <input
            aria-label="已有反馈操作号"
            value={operation}
            maxLength={200}
            onChange={(e) => {
              controller.select(s.selected, s.evidence, s.choice);
              setOperation(e.target.value);
            }}
          />
        </label>
        <button className="button secondary" disabled={disabled || !operation.trim()}>
          只读查询原反馈
        </button>
      </form>
    </section>
  );
}
