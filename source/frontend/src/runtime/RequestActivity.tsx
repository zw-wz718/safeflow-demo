import { useEffect, useState } from 'react';
import { observeRequests, type RequestTiming } from './client';

const labels = {
  directory: '读取本人办理目录',
  original: '按办理编号核查实际状态',
  model: '等待智能理解与计划校验的完整响应',
  query: '读取资料',
  session: '核对登录状态',
  operation: '等待本次办理响应',
};
export default function RequestActivity() {
  const [pending, setPending] = useState<Record<string, number>>({});
  const [recent, setRecent] = useState<RequestTiming[]>([]);
  const [started, setStarted] = useState(0),
    [now, setNow] = useState(0);
  const count = Object.values(pending).reduce((a, b) => a + b, 0);
  useEffect(
    () =>
      observeRequests((event) => {
        setPending((previous) => ({
          ...previous,
          [event.stage]: Math.max(
            0,
            (previous[event.stage] ?? 0) + (event.phase === 'start' ? 1 : -1),
          ),
        }));
        if (event.phase === 'end') setRecent((previous) => [...previous.slice(-7), event]);
      }),
    [],
  );
  useEffect(() => {
    if (!count) return;
    const start = performance.now();
    setStarted(start);
    setNow(start);
    const timer = setInterval(() => setNow(performance.now()), 250);
    return () => clearInterval(timer);
  }, [count > 0]);
  return (
    <aside className="bank-request-status" aria-label="请求处理状态">
      {count > 0 && (
        <p role="status">
          {pending.model
            ? labels.model
            : pending.original
              ? labels.original
              : pending.directory
                ? labels.directory
                : pending.operation
                  ? labels.operation
                  : labels.query}{' '}
          · 已等待 {Math.floor((now - started) / 1000)} 秒。完整响应核对前不视为办理成功。
        </p>
      )}
      {!!recent.length && (
        <details>
          <summary>查看本次会话耗时</summary>
          <p>
            仅记录阶段、耗时和状态，不记录聊天内容、口令或卡号。网络等待包含服务端处理；内部阶段仅在响应提供计时后显示。
          </p>
          <ul>
            {recent.map((r, i) => (
              <li key={i}>
                {labels[r.stage]}：{Math.round(r.durationMs ?? 0)} 毫秒；等待响应{' '}
                {Math.round(r.responseMs ?? 0)} 毫秒，读取完整响应 {Math.round(r.bodyMs ?? 0)} 毫秒
                {r.status ? ` · HTTP ${r.status}` : ' · 未取得完整响应'}
                {r.serverTiming && <code>{r.serverTiming}</code>}
              </li>
            ))}
          </ul>
        </details>
      )}
    </aside>
  );
}
