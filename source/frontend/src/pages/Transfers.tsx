import { useEffect, useRef, useState } from 'react';
import { RuntimeApp } from '../runtime/RuntimeApp';
import '../runtime/styles.css';
import {
  ArrowLeftRight,
  ArrowRight,
  Check,
  Clock3,
  FilePenLine,
  Plus,
  Save,
  X,
} from 'lucide-react';
import { useWorkbench } from '../context';
import { accounts, payees } from '../domain/fixtures';
import { draftStatus, formatMoney } from '../domain/model';
import type { Draft } from '../domain/model';
import {
  DetailRows,
  draftChangeLabel,
  Empty,
  Field,
  Mark,
  Modal,
  Notice,
  PageTitle,
  SearchField,
  SectionTitle,
  Tag,
} from '../components';

const labels = { DRAFT: '待核对', CANCELLED: '已取消', EXPIRED: '已到期' };
export default function Transfers() {
  const { preset } = useWorkbench();
  const [mode, setMode] = useState<'local' | 'service'>(() => {
    try {
      return sessionStorage.getItem('safeflow.bank.transfer-mode.v1') === 'service'
        ? 'service'
        : 'local';
    } catch {
      return 'local';
    }
  });
  function chooseMode(next: 'local' | 'service') {
    setMode(next);
    try {
      sessionStorage.setItem('safeflow.bank.transfer-mode.v1', next);
    } catch {
      /* The current tab remains usable. */
    }
  }
  useEffect(() => {
    if (preset) setMode('local');
  }, [preset]);
  return (
    <>
      <div className="bank-transfer-tabs" role="group" aria-label="转账办理方式">
        <button
          className="button secondary"
          aria-pressed={mode === 'local'}
          onClick={() => chooseMode('local')}
        >
          本机草案
        </button>
        <button
          className="button secondary"
          aria-pressed={mode === 'service'}
          onClick={() => chooseMode('service')}
        >
          在线模拟付款
        </button>
      </div>
      {mode === 'local' ? (
        <LocalTransfers />
      ) : (
        <>
          <PageTitle title="在线模拟付款" description="核对草案、确认付款信息，再单独确认付款。" />
          <RuntimeApp customer />
        </>
      )}
    </>
  );
}
function DraftDetail({
  draft,
  onClose,
  onEdit,
}: {
  draft: Draft;
  onClose: () => void;
  onEdit: (draft: Draft) => void;
}) {
  const { adapter, notify, now } = useWorkbench();
  const [confirmCancel, setConfirmCancel] = useState(false);
  const status = draftStatus(draft, now.getTime());
  const account = accounts.find((a) => a.id === draft.accountId)!;
  const payee = payees.find((p) => p.id === draft.payeeId)!;
  return (
    <Modal title="核对转账草案" onClose={onClose}>
      <div className="draft-detail-top">
        <Tag tone={status === 'DRAFT' ? 'amber' : 'neutral'}>{labels[status]}</Tag>
        <span>{draftChangeLabel(draft.version)}</span>
      </div>
      <div className="transfer-route">
        <div>
          <Mark text="薪" />
          <strong>{account.name}</strong>
          <small>尾号 {account.tail}</small>
        </div>
        <ArrowRight size={22} />
        <div>
          <Mark text={payee.initial} color={payee.color} />
          <strong>{payee.name}</strong>
          <small>尾号 {payee.tail}</small>
        </div>
      </div>
      <div className="draft-amount">
        <small>草案金额 · CNY</small>
        <strong>¥{formatMoney(draft.amount)}</strong>
      </div>
      <DetailRows
        rows={[
          ['付款账户', `${account.name} · ${account.tail}`],
          ['收款信息', `${payee.name} · ${payee.bank} · ${payee.tail}`],
          ['备注', draft.note || '无备注'],
          ['有效期至', new Date(draft.expiresAt).toLocaleString('zh-CN')],
        ]}
      />
      {confirmCancel ? (
        <>
          <Notice tone="amber">
            取消后此草案不可继续编辑。取消只影响本地草案，不涉及资金或银行撤销。
          </Notice>
          <div className="modal-actions">
            <button className="button secondary" onClick={() => setConfirmCancel(false)}>
              保留草案
            </button>
            <button
              className="button danger"
              onClick={() => {
                try {
                  adapter.cancelDraft(draft.id, draft.version);
                  notify('本地草案已取消，未发生资金操作');
                  onClose();
                } catch (err) {
                  notify((err as Error).message, true);
                  setConfirmCancel(false);
                }
              }}
            >
              <X size={16} />
              取消这份草案
            </button>
          </div>
        </>
      ) : (
        <>
          <Notice>草案已保存在本机，尚未转账。真实转账服务暂未开通。</Notice>
          <button
            className="button unavailable"
            disabled
            title="真实银行确认尚未开通，当前不允许资金操作"
          >
            确认并转账 · 尚未接入
          </button>
          <div className="modal-actions">
            <button
              className="button secondary"
              onClick={() => setConfirmCancel(true)}
              disabled={status !== 'DRAFT'}
              title={status !== 'DRAFT' ? '草案已失效' : '取消本地草案'}
            >
              取消草案
            </button>
            <button
              className="button primary"
              onClick={() => onEdit(draft)}
              disabled={status !== 'DRAFT'}
              title={status !== 'DRAFT' ? '已到期或已取消，不可编辑' : '修改并重新保存草案'}
            >
              <FilePenLine size={17} />
              修改草案
            </button>
          </div>
        </>
      )}
    </Modal>
  );
}

function LocalTransfers() {
  const { state, adapter, preset, now, notify } = useWorkbench();
  const [accountId, setAccountId] = useState('account_001');
  const [payeeId, setPayeeId] = useState('');
  const [payeeSearch, setPayeeSearch] = useState('');
  const [amount, setAmount] = useState('');
  const [note, setNote] = useState('');
  const [editing, setEditing] = useState<Draft>();
  const [detailId, setDetailId] = useState('');
  const [error, setError] = useState('');
  const [filter, setFilter] = useState('ALL');
  const firstInput = useRef<HTMLSelectElement>(null);
  useEffect(() => {
    if (preset) {
      setAmount(preset.amount ?? '');
      setPayeeSearch(preset.payeeQuery ?? '');
      setPayeeId(preset.payeeId ?? '');
      setEditing(undefined);
      setError('');
    }
  }, [preset]);
  const matchedPayees = payees.filter((p) => `${p.name} ${p.bank} ${p.tail}`.includes(payeeSearch));
  const selectedPayee = payees.find((p) => p.id === payeeId);
  const detail = state.drafts.find((d) => d.id === detailId);
  const drafts = state.drafts.filter(
    (d) => filter === 'ALL' || draftStatus(d, now.getTime()) === filter,
  );
  function reset() {
    setEditing(undefined);
    setAmount('');
    setNote('');
    setPayeeId('');
    setPayeeSearch('');
    setError('');
    firstInput.current?.focus();
  }
  function edit(draft: Draft) {
    setEditing(draft);
    setAccountId(draft.accountId);
    setPayeeId(draft.payeeId);
    setPayeeSearch('');
    setAmount(draft.amount);
    setNote(draft.note);
    setError('');
    setDetailId('');
    firstInput.current?.focus();
    firstInput.current?.scrollIntoView({ block: 'center', behavior: 'smooth' });
  }
  return (
    <>
      <PageTitle
        title="转账汇款"
        description="先写好，再核对。这里不会转出资金。"
        action={
          <button className="button secondary" onClick={reset}>
            <Plus size={17} />
            新建草案
          </button>
        }
      />
      <div className="transfer-workspace">
        <section className="draft-editor">
          <SectionTitle
            title={editing ? `修改草案 · ${draftChangeLabel(editing.version)}` : '填写转账信息'}
            description="本机保存 · 30 分钟有效"
          />
          <form
            noValidate
            onSubmit={(e) => {
              e.preventDefault();
              setError('');
              try {
                const input = { accountId, payeeId, amount, note };
                const draft = editing
                  ? adapter.updateDraft(editing.id, editing.version, input)
                  : adapter.createDraft(input);
                reset();
                setDetailId(draft.id);
                notify(`草案已保存 · ${draftChangeLabel(draft.version)}，未执行转账`);
              } catch (err) {
                setError((err as Error).message);
              }
            }}
          >
            <Field label="付款账户">
              <select
                ref={firstInput}
                aria-label="付款账户"
                value={accountId}
                onChange={(e) => setAccountId(e.target.value)}
              >
                {accounts.map((a) => (
                  <option key={a.id} value={a.id}>
                    {a.name} · {a.tail} · 可用 ¥{formatMoney(a.balance)}
                  </option>
                ))}
              </select>
            </Field>
            <div className="field payee-field">
              <span id="payee-label">收款人</span>
              <SearchField
                label="搜索收款人"
                placeholder="姓名、银行或尾号"
                value={payeeSearch}
                onChange={setPayeeSearch}
              />
              <div className="payee-options" role="radiogroup" aria-labelledby="payee-label">
                {matchedPayees.map((p) => (
                  <label
                    className={`payee-option ${p.id === payeeId ? 'selected' : ''}`}
                    key={p.id}
                  >
                    <input
                      type="radio"
                      name="payee"
                      value={p.id}
                      checked={payeeId === p.id}
                      onChange={() => setPayeeId(p.id)}
                    />
                    <Mark text={p.initial} color={p.color} small />
                    <span>
                      <strong>{p.name}</strong>
                      <small>
                        {p.bank} · {p.tail}
                      </small>
                    </span>
                    {p.id === payeeId && <Check size={17} />}
                  </label>
                ))}
              </div>
              {!matchedPayees.length && <p className="inline-empty">未找到已有收款人</p>}
              {selectedPayee && (
                <small>
                  已选：{selectedPayee.name} · {selectedPayee.bank} · {selectedPayee.tail}
                </small>
              )}
              <small>同名收款人需按银行和尾号明确选择。</small>
            </div>
            <Field label="转账金额" hint="使用两位小数，例如 200.00">
              <span className="amount-input">
                <span>¥</span>
                <input
                  aria-label="转账金额"
                  aria-invalid={Boolean(error)}
                  aria-describedby={error ? 'draft-error' : undefined}
                  placeholder="0.00"
                  inputMode="decimal"
                  value={amount}
                  onChange={(e) => {
                    setAmount(e.target.value);
                    setError('');
                  }}
                  maxLength={15}
                />
                <small>CNY</small>
              </span>
            </Field>
            <Field label="备注（选填）">
              <input
                placeholder="例如：周末聚餐"
                aria-label="转账备注"
                value={note}
                onChange={(e) => setNote(e.target.value)}
                maxLength={100}
              />
            </Field>
            {error && (
              <p id="draft-error" className="form-error" role="alert">
                {error}
              </p>
            )}
            <Notice>仅生成本地草案。真实银行确认与资金执行暂未开通。</Notice>
            <div className="editor-actions">
              {editing && (
                <button className="button secondary" type="button" onClick={reset}>
                  放弃修改
                </button>
              )}
              <button className="button primary" type="submit">
                <Save size={17} />
                {editing ? '保存修改' : '保存并核对草案'}
              </button>
            </div>
          </form>
        </section>
        <section className="draft-list-section">
          <SectionTitle
            title="我的本地草案"
            description={`${state.drafts.length} 份 · 刷新后可读回`}
          />
          <div className="segments draft-filters">
            {[
              ['ALL', '全部'],
              ['DRAFT', '待核对'],
              ['CANCELLED', '已取消'],
              ['EXPIRED', '已到期'],
            ].map(([value, label]) => (
              <button
                key={value}
                className={filter === value ? 'active' : ''}
                aria-pressed={filter === value}
                onClick={() => setFilter(value)}
              >
                {label}
              </button>
            ))}
          </div>
          {drafts.length ? (
            <div className="draft-list">
              {drafts.map((d) => {
                const p = payees.find((p) => p.id === d.payeeId)!;
                const status = draftStatus(d, now.getTime());
                return (
                  <button
                    key={d.id}
                    className="draft-row"
                    onClick={() => setDetailId(d.id)}
                    aria-label={`查看${p.name} ${d.amount}元草案 · ${draftChangeLabel(d.version)}`}
                  >
                    <div className="draft-row-top">
                      <Mark text={p.initial} color={p.color} />
                      <div>
                        <strong>
                          {p.name}
                          <small> · {p.tail}</small>
                        </strong>
                        <span>{p.bank}</span>
                      </div>
                      <Tag tone={status === 'DRAFT' ? 'amber' : 'neutral'}>{labels[status]}</Tag>
                    </div>
                    <div className="draft-row-middle">
                      <strong>¥{formatMoney(d.amount)}</strong>
                      <span>
                        {draftChangeLabel(d.version)}
                        <ArrowLeftRight size={15} />
                      </span>
                    </div>
                    <div className="draft-row-footer">
                      <Clock3 size={13} />
                      <span>
                        {status === 'DRAFT' ? '有效期至' : '原有效期'}{' '}
                        {new Date(d.expiresAt).toLocaleTimeString('zh-CN', {
                          hour: '2-digit',
                          minute: '2-digit',
                        })}
                      </span>
                      <ArrowRight size={16} />
                    </div>
                  </button>
                );
              })}
            </div>
          ) : (
            <Empty
              title="这里还没有草案"
              detail={filter === 'ALL' ? '填写左侧信息，保存第一份本地草案。' : '该状态暂无草案。'}
            />
          )}
          <p className="data-footnote">本地草案不是交易记录，不改变余额或账单。</p>
        </section>
      </div>
      {detail && (
        <DraftDetail key={detail.id} draft={detail} onClose={() => setDetailId('')} onEdit={edit} />
      )}
    </>
  );
}
