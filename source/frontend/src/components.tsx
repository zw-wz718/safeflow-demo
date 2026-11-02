import { useEffect, useId, useRef } from 'react';
import type { ReactNode, ComponentType } from 'react';
import {
  ArrowDownLeft,
  ArrowUpRight,
  Check,
  ChevronRight,
  Cpu,
  CreditCard,
  Landmark,
  Search,
  ShieldCheck,
  Waves,
  X,
} from 'lucide-react';
import type { Account, DemoCard, Transaction } from './domain/model';
import { formatMoney } from './domain/model';

export function draftChangeLabel(version: number) {
  return version === 1 ? '首次保存' : `第${version - 1}次修改`;
}

export function IconButton({
  icon: Icon,
  label,
  onClick,
  className = '',
  disabled = false,
}: {
  icon: ComponentType<{ size?: number }>;
  label: string;
  onClick: () => void;
  className?: string;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      className={`icon-button ${className}`}
      onClick={onClick}
      aria-label={label}
      title={label}
      disabled={disabled}
    >
      <Icon size={19} />
    </button>
  );
}
export function Tag({ children, tone = 'neutral' }: { children: ReactNode; tone?: string }) {
  return <span className={`tag ${tone}`}>{children}</span>;
}
export function SectionTitle({
  title,
  description,
  action,
}: {
  title: string;
  description?: string;
  action?: ReactNode;
}) {
  return (
    <div className="section-title">
      <div>
        <h2>{title}</h2>
        {description && <p>{description}</p>}
      </div>
      {action}
    </div>
  );
}
export function PageTitle({
  title,
  description,
  action,
}: {
  title: string;
  description: string;
  action?: ReactNode;
}) {
  return (
    <div className="page-title">
      <div>
        <h1>{title}</h1>
        <p>{description}</p>
      </div>
      {action}
    </div>
  );
}
export function Mark({
  text,
  color = 'green',
  small = false,
}: {
  text: string;
  color?: string;
  small?: boolean;
}) {
  return (
    <span className={`merchant-mark ${color} ${small ? 'small' : ''}`} aria-hidden="true">
      {text}
    </span>
  );
}
export function Empty({
  title = '没有匹配的结果',
  detail = '试试其他筛选条件。',
  action,
}: {
  title?: string;
  detail?: string;
  action?: ReactNode;
}) {
  return (
    <div className="empty">
      <Search size={28} />
      <h3>{title}</h3>
      <p>{detail}</p>
      {action}
    </div>
  );
}
export function SearchField({
  value,
  onChange,
  placeholder,
  label,
}: {
  value: string;
  onChange: (value: string) => void;
  placeholder: string;
  label: string;
}) {
  return (
    <label className="search-field">
      <Search size={18} aria-hidden="true" />
      <input
        aria-label={label}
        placeholder={placeholder}
        value={value}
        onChange={(e) => onChange(e.target.value)}
      />
      {value && (
        <button type="button" aria-label={`清除${label}`} title="清除" onClick={() => onChange('')}>
          <X size={16} />
        </button>
      )}
    </label>
  );
}
export function Modal({
  title,
  children,
  onClose,
  wide = false,
}: {
  title: string;
  children: ReactNode;
  onClose: () => void;
  wide?: boolean;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const titleId = useId();
  const close = useRef(onClose);
  close.current = onClose;
  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    const el = dialog.current!;
    el.showModal();
    return () => {
      el.close();
      if (previous?.isConnected) previous.focus();
    };
  }, []);
  return (
    <dialog
      ref={dialog}
      aria-labelledby={titleId}
      className={`modal ${wide ? 'wide' : ''}`}
      onCancel={(e) => {
        e.preventDefault();
        close.current();
      }}
      onClick={(e) => {
        if (e.target === e.currentTarget) close.current();
      }}
      onKeyDown={(e) => {
        if (e.key !== 'Tab') return;
        const focusable = [
          ...e.currentTarget.querySelectorAll<HTMLElement>(
            'button:not([disabled]), a[href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])',
          ),
        ].filter((element) => element.getClientRects().length > 0);
        const first = focusable[0];
        const last = focusable[focusable.length - 1];
        if (!first || !last) {
          e.preventDefault();
          return;
        }
        // Keep Tab in the content instead of cycling to browser chrome at dialog boundaries.
        if (
          e.shiftKey &&
          (document.activeElement === first || !e.currentTarget.contains(document.activeElement))
        ) {
          e.preventDefault();
          last.focus();
        } else if (
          !e.shiftKey &&
          (document.activeElement === last || !e.currentTarget.contains(document.activeElement))
        ) {
          e.preventDefault();
          first.focus();
        }
      }}
    >
      <div className="modal-content">
        <header className="modal-header">
          <h2 id={titleId}>{title}</h2>
          <IconButton icon={X} label="关闭弹窗" onClick={onClose} />
        </header>
        {children}
      </div>
    </dialog>
  );
}
export function Field({
  label,
  children,
  hint,
}: {
  label: string;
  children: ReactNode;
  hint?: string;
}) {
  return (
    <label className="field">
      <span>{label}</span>
      {children}
      {hint && <small>{hint}</small>}
    </label>
  );
}
export function Notice({ children, tone = 'neutral' }: { children: ReactNode; tone?: string }) {
  return (
    <div className={`notice ${tone}`}>
      <ShieldCheck size={18} aria-hidden="true" />
      <p>{children}</p>
    </div>
  );
}
export function DetailRows({ rows }: { rows: [string, ReactNode][] }) {
  return (
    <dl className="detail-rows">
      {rows.map(([label, value]) => (
        <div key={label}>
          <dt>{label}</dt>
          <dd>{value}</dd>
        </div>
      ))}
    </dl>
  );
}
export function BankCard({
  card,
  account,
  balanceHidden = false,
  compact = false,
}: {
  card: DemoCard;
  account: Account;
  balanceHidden?: boolean;
  compact?: boolean;
}) {
  return (
    <div
      className={`bank-card ${card.color} ${compact ? 'compact' : ''}`}
      role="img"
      aria-label={`${card.name}，${card.type}，尾号 ${card.tail}，模拟卡`}
    >
      <div className="bank-card-top">
        <span className="card-wordmark">
          <Landmark size={19} /> SafeFlow
        </span>
        <span>模拟卡</span>
      </div>
      <div className="card-chip">
        <Cpu size={32} strokeWidth={1} />
        <Waves size={22} />
      </div>
      <div className="card-number">•••• &nbsp; •••• &nbsp; •••• &nbsp; {card.tail}</div>
      <div className="bank-card-bottom">
        <div>
          <small>
            {account.name} · {card.type}
          </small>
          <strong>{balanceHidden ? '••••••' : `¥ ${formatMoney(account.balance)}`}</strong>
        </div>
        <span className="card-network">
          UnionPay<small>银联</small>
        </span>
      </div>
    </div>
  );
}
export function TransactionList({
  rows,
  onSelect,
  feedbackIds = [],
}: {
  rows: Transaction[];
  onSelect: (row: Transaction) => void;
  feedbackIds?: string[];
}) {
  return (
    <div className="transaction-list">
      {rows.map((t) => (
        <button
          type="button"
          className="transaction-row"
          key={t.id}
          onClick={() => onSelect(t)}
          aria-label={`查看${t.title}流水详情`}
        >
          <Mark text={t.mark} color={t.color} />
          <div className="transaction-name">
            <strong>{t.title}</strong>
            <span>
              {t.category} ·{' '}
              {new Date(t.date).toLocaleDateString('zh-CN', { month: '2-digit', day: '2-digit' })}
              {feedbackIds.includes(t.id) && <em>已登记反馈</em>}
            </span>
          </div>
          <div className="transaction-amount">
            <strong className={t.direction === 'in' ? 'income' : ''}>
              {t.direction === 'in' ? '+' : '−'}
              {formatMoney(t.amount)}
            </strong>
            <span>
              {t.kind === 'TRANSFER' ? '转账 · 不计消费' : t.kind === 'INCOME' ? '收入' : '消费'}
            </span>
          </div>
          <ChevronRight size={16} className="row-arrow" />
        </button>
      ))}
    </div>
  );
}
export function Metric({
  label,
  amount,
  icon: Icon,
  color = 'green',
  sub,
}: {
  label: string;
  amount: string;
  icon: ComponentType<{ size?: number }>;
  color?: string;
  sub: string;
}) {
  return (
    <div className="metric">
      <div className="metric-label">
        <span>{label}</span>
        <span className={`metric-icon ${color}`}>
          <Icon size={18} />
        </span>
      </div>
      <strong className="metric-value">
        <small>¥</small>
        {formatMoney(amount)}
      </strong>
      <p>{sub}</p>
    </div>
  );
}
export function StatusIcon({ kind }: { kind: 'in' | 'out' | 'card' | 'done' }) {
  return kind === 'in' ? (
    <ArrowDownLeft size={18} />
  ) : kind === 'out' ? (
    <ArrowUpRight size={18} />
  ) : kind === 'card' ? (
    <CreditCard size={18} />
  ) : (
    <Check size={18} />
  );
}
