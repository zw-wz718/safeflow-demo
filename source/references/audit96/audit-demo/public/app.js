import { callApi, loadOptionalReceipt } from './api-client.js';

const $ = (selector) => document.querySelector(selector);
const body = $('#events');
const DRAFT_STATE_KEY = 'safeflow-audit-demo-draft-v1';
let activeOperationId = null;
let activeTaskId = null;
let activeView = 'payer';
let draftSession = null;
let pendingDraftRequest = null;

function restoreDraftState() {
  try {
    const saved = JSON.parse(localStorage.getItem(DRAFT_STATE_KEY) ?? 'null');
    draftSession = saved?.session ?? null;
    pendingDraftRequest = saved?.pending ?? null;
  } catch {
    draftSession = null;
    pendingDraftRequest = null;
  }
}

function persistDraftState() {
  localStorage.setItem(DRAFT_STATE_KEY, JSON.stringify({ session: draftSession, pending: pendingDraftRequest }));
}

async function call(path, options) {
  return callApi(path, options);
}

function formatChinaTime(value) {
  if (!value) return '—';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return '时间格式无效';
  const parts = new Intl.DateTimeFormat('zh-CN', {
    timeZone: 'Asia/Shanghai', year: 'numeric', month: '2-digit', day: '2-digit',
    hour: '2-digit', minute: '2-digit', second: '2-digit', hourCycle: 'h23',
  }).formatToParts(date);
  const part = (type) => parts.find((item) => item.type === type)?.value ?? '';
  return `${part('year')}年${part('month')}月${part('day')}日 ${part('hour')}:${part('minute')}:${part('second')}（北京时间）`;
}

function makeCell(lines) {
  const td = document.createElement('td');
  lines.forEach((line, index) => {
    const div = document.createElement(index === 0 ? 'strong' : 'div');
    div.textContent = line;
    td.append(div);
  });
  return td;
}

function statusClass(status) {
  if (status === 'SUCCEEDED') return 'status-success';
  if (status === 'FAILED') return 'status-failure';
  if (status === 'PROCESSING') return 'status-pending';
  return 'status-neutral';
}

function statusLabel(status) {
  return ({
    SUCCEEDED: '成功', FAILED: '失败', PROCESSING: '处理中', NOT_SUBMITTED: '未提交',
    DRAFT_CREATED: '草案已创建', DRAFT_UPDATED: '草案已修改', DRAFT_CANCELLED: '草案已取消',
  })[status] ?? status ?? '未提供';
}

function isDemoRecord(record) {
  return record.details?.demo_only === true || record.details?.simulated === true;
}

function isFoundationOutboxRecord(record) {
  return record.details?.source === 'foundation-public-audit-outbox-v1';
}

// These labels organize the local display only; they do not certify a business result.
function eventCategory(record) {
  if (isFoundationOutboxRecord(record)) return '底座公共事件（来源字段原样保留）';
  const action = record.action ?? '';
  if (action.startsWith('DRAFT_')) {
    return isDemoRecord(record) || record.details?.projection_source === 'LOCAL_AUDIT_DEMO'
      ? '转账草案（本地投影）' : '草案事件（来源待核）';
  }
  if (action.startsWith('TRANSFER_')) return isDemoRecord(record) ? '模拟资金事件' : '资金事件（来源待核）';
  if (/^(CARD_|LOCK_|UNLOCK_)/.test(action)) return '卡片事件（按动作名分类）';
  if (/^(SUBSCRIPTION_|REMINDER_)/.test(action)) return '订阅或提醒事件（按动作名分类）';
  return '业务类型未确认';
}

function observedResult(record) {
  if (!['SUCCEEDED', 'FAILED'].includes(record.result)) return statusLabel(record.result);
  if (isDemoRecord(record)) return statusLabel(record.result);
  return `来源报告${statusLabel(record.result)}（未核对业务回执）`;
}

function sourceTimeLabel(record) {
  if (isFoundationOutboxRecord(record)) {
    const publicTime = record.details?.foundation?.public_event?.occurred_at;
    return publicTime
      ? `底座公共事件时间：${formatChinaTime(publicTime)}；业务发生时间未提供`
      : '底座公共事件时间：来源未提供；业务发生时间未提供';
  }
  if (record.timestamp == null) return '业务发生时间：来源未提供';
  if (record.details?.demo_only || record.details?.simulated) return `本地模拟事件时间：${formatChinaTime(record.timestamp)}`;
  return `发生时间：${formatChinaTime(record.timestamp)}`;
}

function renderRecords(records) {
  body.replaceChildren();
  if (!records.length) {
    const tr = document.createElement('tr'); const td = document.createElement('td');
    td.colSpan = 6; td.className = 'empty'; td.textContent = '没有找到记录。'; tr.append(td); body.append(tr); return;
  }
  for (const record of records) {
    const tr = document.createElement('tr');
    tr.append(makeCell([sourceTimeLabel(record), eventCategory(record), `${record.kind} · ${record.action ?? '动作映射未提供'}`]));
    tr.append(makeCell([formatChinaTime(record.recorded_at)]));
    const ids = [`任务：${record.task_id}`, `动作号：${record.operation_id ?? '来源未提供'}`, `用户：${record.user_id ?? '来源未提供可信身份'}`];
    if (record.action?.startsWith('DRAFT_')) {
      ids.push(`草案：${record.details?.draft_ref ?? '未提供'}`);
      ids.push(`草案业务号：${record.details?.business_operation_id ?? '旧记录未提供'}`);
      ids.push(`业务版本：${record.details?.draft_version ?? '未提供'} · 金额：${record.details?.draft_amount ?? '未提供'}`);
    }
    tr.append(makeCell(ids));
    tr.append(makeCell([`确认：${record.confirmation ?? '未提供'}`, `认证：${record.auth_result ?? '未提供'}`]));
    const resultCell = makeCell([observedResult(record), `安全结果：${record.security_result ?? '未提供'}`]);
    const demoResult = isDemoRecord(record);
    resultCell.querySelector('strong').className = `status-badge ${statusClass(demoResult ? record.result : null)}`;
    tr.append(resultCell);
    const actionCell = document.createElement('td'); const button = document.createElement('button');
    button.className = 'link'; button.textContent = '查看后台记录';
    button.addEventListener('click', async () => {
      const { record: item } = await call(`/api/audit/${encodeURIComponent(record.audit_id)}`);
      $('#json').textContent = JSON.stringify(item, null, 2); $('#detail').classList.remove('hidden');
    });
    actionCell.append(button); tr.append(actionCell); body.append(tr);
  }
}

function renderReceipt(receipt) {
  const panel = $('#receipt'); panel.replaceChildren();
  $('#receipt-empty').classList.remove('receipt-error');
  if (!receipt) {
    panel.classList.add('hidden');
    $('#receipt-empty').textContent = '当前操作没有本地模拟转账回执；草案或其他来源事件不会被配上虚构的付款/收款人。';
    $('#receipt-empty').classList.remove('hidden');
    return;
  }
  $('#receipt-empty').classList.add('hidden'); panel.classList.remove('hidden');
  panel.classList.toggle('receipt-success', receipt.status === 'SUCCEEDED');
  panel.classList.toggle('receipt-failure', receipt.status === 'FAILED');
  const heading = document.createElement('h3'); heading.textContent = receipt.perspective;
  const badge = document.createElement('span'); badge.className = `status-badge receipt-badge ${statusClass(receipt.status)}`; badge.textContent = statusLabel(receipt.status);
  const grid = document.createElement('dl'); grid.className = 'receipt-grid';
  const fields = [
    ['参考编号', receipt.receipt_ref], ['金额', receipt.amount], ['模拟事件时间', formatChinaTime(receipt.occurred_at)],
    ['付款方', receipt.payer_label], ['收款方', receipt.payee_label],
  ];
  if (receipt.failure_reason) fields.push(['结果说明', receipt.failure_reason]);
  for (const [label, value] of fields) {
    const wrapper = document.createElement('div'); const dt = document.createElement('dt'); const dd = document.createElement('dd');
    dt.textContent = label; dd.textContent = value; wrapper.append(dt, dd); grid.append(wrapper);
  }
  const note = document.createElement('p'); note.className = 'receipt-note'; note.textContent = '本回执为脱敏演示视图，不显示认证、安全内部字段或账号信息。';
  panel.append(heading, badge, grid, note);
}

function showReceiptError(error) {
  const panel = $('#receipt');
  panel.replaceChildren();
  panel.classList.add('hidden');
  const notice = $('#receipt-empty');
  notice.textContent = `回执读取失败：${error.message}。请重试；不能据此判断回执不存在。`;
  notice.classList.add('receipt-error');
  notice.classList.remove('hidden');
}

async function loadReceipt() {
  if (!activeOperationId) return renderReceipt(null);
  await loadOptionalReceipt(
    async () => (await call(`/api/receipt/${encodeURIComponent(activeOperationId)}?view=${activeView}`)).receipt,
    renderReceipt,
  );
}

async function verify() {
  const result = await call('/api/verify-chain');
  $('#chain').className = `chain ${result.valid ? 'valid' : 'invalid'}`;
  $('#chain').textContent = result.valid
    ? `本地链内一致（${result.hash_version}）：已检查 ${result.checked} 条记录；未核对外部可信链头，不能证明没有删尾或整链重写。`
    : `本地链校验失败：${result.reason}；首个断点序号 ${result.first_break_sequence ?? '无法定位'}。`;
}

function renderDraftControls() {
  const active = Boolean(draftSession && !draftSession.cancelled);
  const pending = Boolean(pendingDraftRequest);
  $('#draft-create').disabled = active || pending;
  $('#draft-update').disabled = !active || pending || draftSession?.draft_version >= 2;
  $('#draft-cancel').disabled = !active || pending;
  $('#draft-retry').classList.toggle('hidden', !pending);
  $('#draft-retry').disabled = !pending;
}

function showDraftStatus(text) {
  const summary = $('#draft-summary');
  summary.replaceChildren();
  const status = document.createElement('strong'); status.textContent = text;
  summary.append(status);
  if (draftSession) {
    const ids = document.createElement('div');
    ids.textContent = `task_id：${draftSession.task_id} · 草案编号：${draftSession.draft_ref} · 草案业务号：${draftSession.business_operation_id ?? '旧记录未提供'} · 当前版本：${draftSession.draft_version} · 金额：${draftSession.draft_amount ?? '未提供'}`;
    summary.append(ids);
  }
}

function makeDraftRequest(transition) {
  const creating = transition === 'create';
  const draftSessionForRequest = creating ? null : draftSession;
  if (!creating && !draftSessionForRequest) throw new Error('请先创建草案演示记录');
  return {
    transition,
    event_id: `EVT-${crypto.randomUUID()}`,
    task_id: creating ? `TASK-${crypto.randomUUID()}` : draftSessionForRequest.task_id,
    operation_id: `OP-${crypto.randomUUID()}`,
    draft_ref: creating ? `DRAFT-${crypto.randomUUID()}` : draftSessionForRequest.draft_ref,
    business_operation_id: creating ? `BOP-${crypto.randomUUID()}` : draftSessionForRequest.business_operation_id ?? undefined,
    draft_version: creating ? 1 : transition === 'cancel' ? draftSessionForRequest.draft_version : draftSessionForRequest.draft_version + 1,
    draft_amount: creating ? 'CNY 200.00' : transition === 'update' ? 'CNY 300.00' : draftSessionForRequest.draft_amount ?? undefined,
  };
}

async function sendDraftRequest(request) {
  let result;
  try {
    result = await call('/api/demo-draft', {
      method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(request),
    });
  } catch (error) {
    pendingDraftRequest = request;
    persistDraftState();
    renderDraftControls();
    showDraftStatus(`记录未确认：${error.message}。可用“重试上次记录”按原事件编号补交。`);
    return;
  }
  pendingDraftRequest = null;
  draftSession = {
    task_id: result.task_id,
    draft_ref: result.draft_ref,
    business_operation_id: result.business_operation_id,
    draft_version: result.draft_version,
    draft_amount: result.draft_amount,
    cancelled: result.transition === 'cancel',
  };
  activeTaskId = result.task_id;
  activeOperationId = null;
  persistDraftState();
  renderDraftControls();
  showDraftStatus(`${statusLabel(result.record.result)} · ${result.duplicate ? '重复补交，未重复记账' : '已写入审计记录'}；这不是转账结果。`);
  renderRecords(result.records);
  try {
    await loadReceipt();
    await verify();
  } catch (error) {
    showDraftStatus(`审计已写入，但页面读取或链校验未完成：${error.message}。刷新页面可重试查询。`);
  }
}

function startDraftAction(transition) {
  if (pendingDraftRequest) return;
  try {
    pendingDraftRequest = makeDraftRequest(transition);
    persistDraftState();
    renderDraftControls();
    sendDraftRequest(pendingDraftRequest);
  } catch (error) { showDraftStatus(error.message); }
}

async function makeDemo(outcome) {
  const buttons = [$('#success'), $('#failure')]; buttons.forEach((button) => { button.disabled = true; });
  try {
    const demo = await call('/api/demo-transfer', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ outcome }) });
    activeOperationId = demo.operation_id;
    activeTaskId = demo.task_id;
    const panel = $('#summary'); panel.className = `panel summary ${outcome === 'success' ? 'summary-success' : 'summary-failure'}`; panel.replaceChildren();
    const title = document.createElement('h2'); title.textContent = `模拟转账${outcome === 'success' ? '成功' : '失败'}`;
    title.className = `status-text ${statusClass(demo.outcome)}`;
    const task = document.createElement('p'); task.textContent = `task_id：${demo.task_id}`;
    const operation = document.createElement('p'); operation.textContent = `operation_id：${demo.operation_id}`;
    const count = document.createElement('p'); count.textContent = `已追加 ${demo.records.length} 条审计事件。`;
    const note = document.createElement('p'); note.textContent = outcome === 'failure' ? '模拟失败，未实际提交银行。该失败记录已保留。' : '这是虚构的模拟成功结果，并非真实银行回执。';
    panel.append(title, task, operation, count, note);
    renderRecords(demo.records);
    try { await loadReceipt(); } catch (error) { showReceiptError(error); }
    await verify();
  } catch (error) { alert(error.message); }
  finally { buttons.forEach((button) => { button.disabled = false; }); }
}

function selectView(view) {
  activeView = view;
  $('#payer-view').classList.toggle('selected', view === 'payer');
  $('#payer-view').setAttribute('aria-pressed', String(view === 'payer'));
  $('#recipient-view').classList.toggle('selected', view === 'recipient');
  $('#recipient-view').setAttribute('aria-pressed', String(view === 'recipient'));
  loadReceipt().catch(showReceiptError);
}

$('#draft-create').addEventListener('click', () => startDraftAction('create'));
$('#draft-update').addEventListener('click', () => startDraftAction('update'));
$('#draft-cancel').addEventListener('click', () => startDraftAction('cancel'));
$('#draft-retry').addEventListener('click', () => { if (pendingDraftRequest) sendDraftRequest(pendingDraftRequest); });
$('#success').addEventListener('click', () => makeDemo('success'));
$('#failure').addEventListener('click', () => makeDemo('failure'));
$('#payer-view').addEventListener('click', () => selectView('payer'));
$('#recipient-view').addEventListener('click', () => selectView('recipient'));
$('#verify').addEventListener('click', () => verify().catch((error) => alert(error.message)));
$('#close').addEventListener('click', () => $('#detail').classList.add('hidden'));

restoreDraftState();
renderDraftControls();
if (draftSession) showDraftStatus(draftSession.cancelled ? '演示草案已取消。' : '演示草案可继续修改或取消。');
if (pendingDraftRequest) showDraftStatus('有一条记录等待确认；请重试相同事件，不要重新创建新事件。');

const saved = await call('/api/audit/latest');
if (saved.records.length) {
  const latest = saved.records.at(-1);
  activeOperationId = saved.records.some((record) => record.action === 'TRANSFER_SUBMITTED' || record.action === 'TRANSFER_SUCCEEDED' || record.action === 'TRANSFER_FAILED')
    ? latest.operation_id : null;
  activeTaskId = latest.task_id;
  renderRecords(saved.records);
  try { await loadReceipt(); } catch (error) { showReceiptError(error); }
}
await verify();
