import { useEffect, useRef, useState } from 'react';
import { ArrowRight, Bot, CirclePlus, ReceiptText, Send, Sparkles, Wallet } from 'lucide-react';
import { useWorkbench } from '../context';
import { Empty, Modal, Notice, PageTitle, Tag } from '../components';
import { assistantMessageText } from '../domain/assistant-copy';

const suggestions = [
  { icon: Wallet, text: '查账户余额' },
  { icon: ReceiptText, text: '查询本月账单' },
  { icon: Send, text: '给小李转 200 元' },
];
export default function Agent() {
  const { state, adapter, navigate, notify } = useWorkbench();
  const [text, setText] = useState('');
  const [busy, setBusy] = useState(false);
  const [clear, setClear] = useState(false);
  const bottom = useRef<HTMLDivElement>(null);
  const textArea = useRef<HTMLTextAreaElement>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const submitting = useRef(false);
  useEffect(
    () => () => {
      clearTimeout(timer.current);
    },
    [],
  );
  useEffect(() => {
    if (state.messages.length)
      bottom.current?.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
  }, [state.messages.length, busy]);
  function send(input: string) {
    if (submitting.current || !input.trim()) return;
    submitting.current = true;
    setBusy(true);
    timer.current = setTimeout(() => {
      try {
        adapter.sendMessage(input);
        setText('');
      } catch (err) {
        notify((err as Error).message, true);
      }
      submitting.current = false;
      setBusy(false);
      textArea.current?.focus();
    }, 450);
  }
  return (
    <>
      <PageTitle
        title="智能助手"
        description="从一句话开始，把账户事务理清楚。"
        action={
          <button
            className="button secondary"
            onClick={() => setClear(true)}
            disabled={busy || !state.messages.length}
            title={!state.messages.length ? '当前没有对话可清空' : '清空本地对话'}
          >
            <CirclePlus size={17} />
            新对话
          </button>
        }
      />
      <div className="chat-workspace">
        <div className="chat-topline">
          <span>
            <span className="status-dot" />
            SafeFlow 助手
          </span>
          <Tag tone="blue">演示助手 · 未接入真实模型</Tag>
        </div>
        <div className="chat-history" aria-label="对话历史" aria-live="polite">
          {!state.messages.length ? (
            <div className="chat-welcome">
              <span className="chat-emblem">
                <Sparkles size={30} strokeWidth={1.5} />
              </span>
              <h2>今天想了解什么？</h2>
              <p>查余额、看账单，或准备一份转账草案。</p>
              <div className="suggestion-grid">
                {suggestions.map((s) => (
                  <button key={s.text} onClick={() => send(s.text)} disabled={busy}>
                    <s.icon size={21} />
                    <strong>{s.text}</strong>
                    <ArrowRight size={16} />
                  </button>
                ))}
              </div>
            </div>
          ) : (
            state.messages.map((m) => (
              <div key={m.id} className={`message ${m.role}`}>
                <span className={`message-avatar ${m.role === 'assistant' ? 'green' : ''}`}>
                  {m.role === 'assistant' ? <Bot size={19} /> : 'E'}
                </span>
                <div className="message-main">
                  <div className="message-meta">
                    <strong>{m.role === 'assistant' ? 'SafeFlow' : 'Echo'}</strong>
                    <span>
                      {new Date(m.at).toLocaleTimeString('zh-CN', {
                        hour: '2-digit',
                        minute: '2-digit',
                      })}
                    </span>
                  </div>
                  <div className="message-body">{assistantMessageText(m)}</div>
                  {m.action && (
                    <button
                      className="message-action"
                      onClick={() =>
                        navigate(
                          m.action === 'bills'
                            ? 'bills'
                            : m.action === 'transfers'
                              ? 'transfers'
                              : 'overview',
                          m.action === 'transfers'
                            ? { amount: m.amount, payeeQuery: m.payeeQuery }
                            : undefined,
                        )
                      }
                    >
                      {m.action === 'transfers'
                        ? '选择收款人并核对草案'
                        : m.action === 'bills'
                          ? '查看账单'
                          : '查看账户'}
                      <ArrowRight size={16} />
                    </button>
                  )}
                </div>
              </div>
            ))
          )}
          {busy && (
            <div className="message assistant">
              <span className="message-avatar green">
                <Bot size={19} />
              </span>
              <div className="processing" role="status">
                <span />
                <span />
                <span />
                <small>正在处理你的问题…</small>
              </div>
            </div>
          )}
          <div ref={bottom} />
        </div>
        <div className="chat-compose">
          <form
            onSubmit={(e) => {
              e.preventDefault();
              send(text);
            }}
          >
            <textarea
              ref={textArea}
              aria-label="对话输入"
              placeholder="输入你的问题…"
              rows={2}
              maxLength={1000}
              value={text}
              disabled={busy}
              onChange={(e) => setText(e.target.value)}
            />
            <button
              className="send-button"
              type="submit"
              aria-label="发送消息"
              title="发送消息"
              disabled={!text.trim() || busy}
            >
              <ArrowRight size={21} />
            </button>
          </form>
          <p>本地演示 · 不执行资金操作 · {text.length}/1000</p>
        </div>
      </div>
      <Notice>
        目前仅支持演示余额、本月账单和转账草案引导。同名收款人需要你明确选择，助手不会自行确认或转账。
      </Notice>
      {clear && (
        <Modal title="开始新对话" onClose={() => setClear(false)}>
          <Empty title="清空本地对话记录？" detail="已保存的草案、反馈和操作记录会保留。" />
          <div className="modal-actions">
            <button className="button secondary" onClick={() => setClear(false)}>
              保留对话
            </button>
            <button
              className="button primary"
              onClick={() => {
                try {
                  adapter.clearMessages();
                  setClear(false);
                  notify('已清空本地对话');
                  textArea.current?.focus();
                } catch (err) {
                  notify((err as Error).message, true);
                }
              }}
            >
              开始新对话
            </button>
          </div>
        </Modal>
      )}
    </>
  );
}
