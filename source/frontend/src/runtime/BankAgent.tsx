import { useEffect, useRef, useState } from 'react';
import {
  agentStatus,
  getAgent,
  runRead,
  sendAgent,
  type AgentStatus,
  type AgentView,
} from './agent-client';
import { ResultRows } from './AgentResults';
import { request, RuntimeApiError } from './client';
import { z } from 'zod';

const selectorKey = 'safeflow.bank.agent.read-task.v1';
type Props = {
  accounts: { account_id: string; display_name: string }[];
  disabled: boolean;
  run: (work: () => Promise<void>) => Promise<void>;
};
function savedTask() {
  try {
    return sessionStorage.getItem(selectorKey) || '';
  } catch {
    return '';
  }
}
function retainTask(id: string) {
  sessionStorage.setItem(selectorKey, id);
  if (savedTask() !== id) throw Error('无法保存查询原号，已停止执行。');
}
export default function BankAgent({ accounts, disabled, run }: Props) {
  const [ownedAccounts, setOwnedAccounts] = useState(accounts);
  const [status, setStatus] = useState<AgentStatus | null>(null);
  const [view, setView] = useState<AgentView | null>(null);
  const [message, setMessage] = useState('');
  const [account, setAccount] = useState('');
  const [task, setTask] = useState(savedTask);
  const [notice, setNotice] = useState('正在读取模型配置状态…');
  const lifetime = useRef<AbortController | null>(null);
  async function refresh(signal: AbortSignal) {
    const [next, raw] = await Promise.all([
      agentStatus(signal),
      request('/api/bank/overview', 'GET', undefined, signal),
    ]);
    const overview = z
      .object({
        accounts: z.array(
          z
            .object({
              account: z
                .object({ account_id: z.string().min(1), display_name: z.string().min(1) })
                .passthrough(),
            })
            .passthrough(),
        ),
      })
      .passthrough()
      .parse(raw);
    if (!signal.aborted) {
      setOwnedAccounts(overview.accounts.map((a) => a.account));
      setStatus(next);
      setNotice(
        next.configured
          ? '真实模型已配置；连接结果以本次请求为准。'
          : '模型未启用，手工银行功能仍可使用。',
      );
    }
  }
  useEffect(() => {
    const controller = new AbortController();
    lifetime.current = controller;
    refresh(controller.signal).catch(() => {
      if (!controller.signal.aborted) setNotice('模型或本人账户读取失败，请刷新状态。');
    });
    return () => controller.abort();
  }, []);
  async function action(work: (signal: AbortSignal) => Promise<AgentView>) {
    await run(async () => {
      const signal = lifetime.current!.signal;
      setNotice('正在请求服务端，请勿重复提交…');
      try {
        const next = await work(signal);
        if (signal.aborted) return;
        if (next.kind !== 'READ' && next.kind !== 'CLARIFY')
          throw Error('响应超出银行聊天只读范围，已停止。');
        if (next.kind === 'READ') {
          retainTask(next.task.task_id);
          setTask(next.task.task_id);
        }
        setView(next);
        setNotice(
          next.kind === 'CLARIFY'
            ? '请核对模型澄清；不支持的操作请使用手工业务页面。'
            : '真实模型已返回受检计划；银行结果仅以下方实际工具回执为准。',
        );
      } catch (error) {
        if (signal.aborted) return;
        if (error instanceof RuntimeApiError && error.taskId) {
          retainTask(error.taskId);
          setTask(error.taskId);
        }
        const labels: Record<string, string> = {
          LLM_NOT_CONFIGURED: '模型未配置。',
          LLM_GATEWAY_UNAVAILABLE: '模型调用失败或超时，请稍后重新提出只读请求。',
          LLM_PROPOSAL_REJECTED: '模型计划未通过安全校验，未执行。',
          FORBIDDEN: '请求超出当前账户或只读能力范围，未执行。',
          DEPENDENCY_UNAVAILABLE: '所需银行数据暂不可用。',
          NOT_AUTHENTICATED: '登录已失效，请重新登录。',
        };
        setNotice(
          error instanceof RuntimeApiError
            ? labels[error.reason] || '查询失败，请按原号读取。'
            : '响应未核实，保留查询原号；没有自动重试。',
        );
        throw error;
      }
    });
  }
  const selected = ownedAccounts.some((a) => a.account_id === account)
    ? account
    : ownedAccounts[0]?.account_id || '';
  return (
    <section className="bank-panel bank-agent" aria-label="银行智能助手">
      <h2>银行智能助手</h2>
      <p>Command Code · DeepSeek v4.1 Flash 真实模型规划；银行账户与资金均为虚构演示数据。</p>
      <p>
        支持已注册的只读银行查询。转账、订阅变更和卡片操作请使用原业务页面，仍需逐笔确认及模拟验证码。聊天不会扣款或解除冻结。
      </p>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          setView(null);
          void action((signal) =>
            sendAgent(
              message,
              { account_id: selected || null, payee_id: null, payee_query: null },
              null,
              signal,
            ),
          );
        }}
      >
        <label>
          本人账户
          <select value={selected} onChange={(e) => setAccount(e.target.value)} disabled={disabled}>
            {ownedAccounts.map((a) => (
              <option key={a.account_id} value={a.account_id}>
                {a.display_name} · {a.account_id}
              </option>
            ))}
          </select>
        </label>
        <label>
          你想查询什么？
          <textarea
            value={message}
            maxLength={4096}
            onChange={(e) => setMessage(e.target.value)}
            placeholder="例如：查询我当前账户的可用余额"
            disabled={disabled}
          />
        </label>
        <button
          className="primary"
          disabled={disabled || !status?.configured || !message.trim() || !selected}
        >
          发送给真实模型
        </button>
      </form>
      <p role="status">{notice}</p>
      <button
        disabled={disabled}
        onClick={() =>
          void run(async () => {
            await refresh(lifetime.current!.signal);
          })
        }
      >
        刷新模型状态
      </button>
      {task && (
        <>
          <p>只读任务原号：{task}</p>
          <button
            disabled={disabled}
            onClick={() => void action((signal) => getAgent(task, signal))}
          >
            读取原查询结果
          </button>
        </>
      )}
      {view?.kind === 'CLARIFY' && <p>{view.clarification.question}</p>}
      {view?.kind === 'READ' && (
        <>
          <p>
            任务状态：{view.task.task_state} · 规划来源：{view.planning_source}
          </p>
          {(view.can_start || view.task.next_action.kind === 'CONTINUE') && (
            <button
              disabled={disabled}
              onClick={() => void action((signal) => runRead(view, !view.can_start, signal))}
            >
              {view.can_start ? '执行只读查询' : '继续原只读任务'}
            </button>
          )}
          {view.results?.map((r) => (
            <ResultRows key={r.operation_id} result={r} />
          ))}
        </>
      )}
    </section>
  );
}
