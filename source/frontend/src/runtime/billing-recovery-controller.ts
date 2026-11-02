import { getAgent } from './agent-client';
import { RuntimeApiError, RuntimeTransportError } from './client';
import { cardContext } from './card-read';
import {
  getBillingFeedback,
  feedbackEligible,
  feedbackError,
  type BillingResult,
  type FeedbackIntent,
  type FeedbackResult,
} from './billing-feedback-client';
import { BillingRecoveryStore, type RecoveryRow } from './billing-recovery-store';
import {
  recoveryContext,
  recoveryPrepare,
  recoveryConfirm,
  recoveryExecute,
  recoveryRead,
  recoveryAbandon,
  type RecoveryCreated,
} from './billing-recovery-client';

export type FeedbackState = {
  reports: BillingResult[];
  reportTask: string;
  selected: number | null;
  evidence: number | null;
  choice: FeedbackIntent['choice'] | '';
  prepared: RecoveryCreated | null;
  confirmed: boolean;
  result: FeedbackResult | null;
  busy: boolean;
  notice: string;
  recovery: RecoveryRow[];
  storageError: boolean;
};
const empty = (): FeedbackState => ({
  reports: [],
  reportTask: '',
  selected: null,
  evidence: null,
  choice: '',
  prepared: null,
  confirmed: false,
  result: null,
  busy: false,
  notice: '',
  recovery: [],
  storageError: false,
});
export const feedbackApi = {
  // Existing session context is derived from the same cookie and binding version.
  context: async (signal?: AbortSignal) => {
    const [identity, installation] = await Promise.all([
      cardContext(signal),
      recoveryContext(signal),
    ]);
    return { identity, installation };
  },
  getAgent,
  prepare: recoveryPrepare,
  confirm: recoveryConfirm,
  execute: recoveryExecute,
  get: getBillingFeedback,
  read: recoveryRead,
  abandon: recoveryAbandon,
};
type Ticket = { epoch: number; generation: number; controller: AbortController };

// A narrow page controller. Every asynchronous continuation checks both the live
// host session and its page/request epoch BEFORE any UI or recovery mutation.
export class BillingRecoveryController {
  state = empty();
  private active = false;
  private epoch = 0;
  private generation = -1;
  private pending: AbortController | null = null;
  private row: RecoveryRow | null = null;
  private realm: string | null = null;
  private context: string | null = null;
  constructor(
    private readonly getGeneration: () => number,
    private readonly notify: (state: FeedbackState) => void,
    private readonly authLost: () => void,
    private readonly store = new BillingRecoveryStore(),
    private readonly api = feedbackApi,
  ) {}
  activate() {
    this.active = true;
    this.reset();
  }
  deactivate() {
    this.active = false;
    this.epoch++;
    this.pending?.abort();
    this.pending = null;
    this.row = null;
    this.context = null;
    this.state = empty();
  }
  reset() {
    this.epoch++;
    this.pending?.abort();
    this.pending = null;
    this.row = null;
    this.context = null;
    this.generation = this.getGeneration();
    this.state = empty();
    void this.refreshRecovery();
    this.emit();
  }
  private emit() {
    if (this.active) this.notify({ ...this.state });
  }
  private async refreshRecovery() {
    const epoch = this.epoch;
    const valid = () => this.active && epoch === this.epoch;
    try {
      const rows = await this.store.read(valid);
      if (!valid()) return;
      this.state.recovery = rows;
      if (
        this.row &&
        (rows.length !== 1 ||
          rows[0].local_id !== this.row.local_id ||
          rows[0].row_revision !== this.row.row_revision)
      ) {
        this.row = null;
        this.state.prepared = null;
        this.state.confirmed = false;
      }
      this.state.storageError = false;
    } catch (e) {
      if (!valid()) return;
      this.state.storageError = true;
      this.state.notice = feedbackError(e);
    }
    this.emit();
  }
  private valid(t: Ticket) {
    return (
      this.active &&
      t.epoch === this.epoch &&
      t.generation === this.getGeneration() &&
      !t.controller.signal.aborted
    );
  }
  private start(): Ticket | null {
    if (!this.active || this.state.busy) return null;
    if (this.generation !== this.getGeneration()) {
      this.reset();
      return null;
    }
    const controller = new AbortController();
    this.pending = controller;
    const ticket = { epoch: ++this.epoch, generation: this.getGeneration(), controller };
    this.state.busy = true;
    this.state.notice = '';
    this.emit();
    return ticket;
  }
  private finish(t: Ticket) {
    if (this.valid(t)) {
      this.state.busy = false;
      this.pending = null;
      void this.refreshRecovery();
      this.emit();
    }
  }
  private async verifyContext(t: Ticket) {
    if (!this.valid(t)) throw new RuntimeTransportError('ABORTED');
    let current: Awaited<ReturnType<typeof feedbackApi.context>>;
    try {
      current = await this.api.context(t.controller.signal);
    } catch (e) {
      if (this.valid(t)) {
        this.reset();
        this.state.notice =
          '无法核实服务端会话绑定，已清除报告和确认；恢复原号仍保留。' + feedbackError(e);
        this.emit();
      }
      throw e;
    }
    if (!this.valid(t)) throw new RuntimeTransportError('ABORTED');
    const stamp = JSON.stringify(current);
    if (!current.identity || (this.context !== null && stamp !== this.context)) {
      this.reset();
      this.state.notice =
        '服务端会话已变化，旧报告、选择和确认已清除；未核清原号保留，请在当前会话只读核对。';
      this.emit();
      if (!current.identity) this.authLost();
      throw new RuntimeTransportError('ABORTED');
    }
    this.context = stamp;
    this.realm = current.installation.available ? current.installation.recovery_realm : null;
  }
  private async observe<T>(t: Ticket, action: () => Promise<T>) {
    await this.verifyContext(t);
    try {
      const value = await action();
      await this.verifyContext(t);
      return value;
    } catch (e) {
      if (this.valid(t)) await this.verifyContext(t);
      throw e;
    }
  }
  async watchRecovery() {
    if (!this.active || this.state.busy) return;
    await this.refreshRecovery();
  }
  async watchContext() {
    if (!this.active || this.state.busy || !this.context) return;
    const t = {
      epoch: this.epoch,
      generation: this.getGeneration(),
      controller: new AbortController(),
    };
    try {
      await this.verifyContext(t);
    } catch {
      /* verification already clears unsafe display */
    }
  }
  private fail(t: Ticket, e: unknown, suffix = '') {
    if (!this.valid(t)) return;
    this.state.prepared = null;
    this.state.confirmed = false;
    if (e instanceof RuntimeApiError && (e.status === 401 || e.status === 403)) {
      this.reset();
      this.state.notice = feedbackError(e);
      this.emit();
      if (e.status === 401) this.authLost();
      return;
    }
    this.state.notice = feedbackError(e) + suffix;
    this.emit();
  }
  select(
    report: number | null,
    evidence: number | null = null,
    choice: FeedbackState['choice'] = '',
  ) {
    this.epoch++;
    this.pending?.abort();
    this.pending = null;
    this.row = null;
    this.state = {
      ...this.state,
      selected: report,
      evidence,
      choice,
      prepared: null,
      confirmed: false,
      result: null,
      busy: false,
      notice: '',
    };
    void this.refreshRecovery();
    this.emit();
  }
  async load(task: string) {
    const t = this.start();
    if (!t) return;
    this.state.reports = [];
    this.state.prepared = null;
    this.state.confirmed = false;
    this.state.selected = null;
    this.state.evidence = null;
    this.state.choice = '';
    this.state.result = null;
    this.emit();
    try {
      const view = await this.observe(t, () => this.api.getAgent(task, t.controller.signal));
      if (!this.valid(t)) return;
      this.state.reportTask = task;
      this.state.reports =
        view.kind === 'READ'
          ? (view.results ?? []).filter(
              (r): r is BillingResult => r.tool_name === 'analyze_local_basic_spending',
            )
          : [];
      this.state.notice = this.state.reports.length
        ? '已读取原保存结果，请逐份选择报告。后端仍需核实真实原归档。'
        : '原任务没有可展示的账单报告。';
      this.emit();
    } catch (e) {
      this.fail(t, e);
    } finally {
      this.finish(t);
    }
  }
  canPrepare() {
    const s = this.state,
      report = s.selected === null ? undefined : s.reports[s.selected];
    return (
      !s.busy &&
      !s.storageError &&
      s.recovery.length === 0 &&
      !!report &&
      feedbackEligible(report) &&
      s.evidence !== null &&
      !!report.data.report.evidence?.[s.evidence] &&
      !!s.choice &&
      !s.prepared
    );
  }
  async prepare() {
    await this.refreshRecovery();
    if (!this.canPrepare()) {
      this.emit();
      return;
    }
    const r = this.state.reports[this.state.selected!],
      evidence = r.data.report.evidence![this.state.evidence!];
    const intent: FeedbackIntent = {
      input_version: 1,
      report_id: r.data.report.metadata.report_id,
      transaction: evidence.reference,
      choice: this.state.choice as FeedbackIntent['choice'],
    };
    const t = this.start();
    if (!t) return;
    const attempt = crypto.randomUUID();
    try {
      await this.verifyContext(t);
      if (!this.valid(t)) return;
      if (!this.realm) throw new Error('恢复能力尚未启用，请保留原记录。');
      const realm = this.realm;
      let row = await this.observe(t, () =>
        this.store.reserve(realm, attempt, () => this.valid(t)),
      );
      if (!this.valid(t)) return;
      this.row = row;
      const p = await this.observe(t, () =>
        this.api.prepare(realm, attempt, intent, t.controller.signal),
      );
      if (!this.valid(t)) return;
      if (p.kind !== 'CREATED') {
        this.state.notice = '原准备只读恢复，不能续用旧确认；请逐笔核对或明确放弃。';
        return;
      }
      row = await this.observe(t, () =>
        this.store.change(
          row,
          { task_id: p.task_id, operation_id: p.operation_id, phase: 'BOUND' },
          () => this.valid(t),
        ),
      );
      if (!this.valid(t)) return;
      this.row = row;
      this.state.prepared = p;
      this.state.confirmed = false;
      this.state.notice = '仅已准备，尚未确认或保存。请核对服务端原参数。';
      this.emit();
    } catch (e) {
      if (!this.valid(t)) return;
      this.fail(t, e, ' 原nonce已持久保存；请只读反查或明确放弃，不自动重试。');
    } finally {
      this.finish(t);
    }
  }
  private async writable(p: RecoveryCreated, t: Ticket) {
    if (
      !this.row ||
      this.realm !== p.recovery_realm ||
      this.row.nonce !== p.nonce ||
      Date.parse(p.confirmation.expires_at) <= Date.now()
    )
      throw new Error('确认不属于当前页面或已过期，仅可恢复查询。');
    await this.observe(t, () => this.store.verifyOnly(this.row!, () => this.valid(t)));
  }
  async confirm() {
    const p = this.state.prepared;
    if (!p || this.state.confirmed) return;
    const t = this.start();
    if (!t) return;
    try {
      await this.verifyContext(t);
      if (!this.valid(t)) return;
      await this.writable(p, t);
      if (!this.valid(t)) return;
      await this.observe(t, () => this.api.confirm(p, t.controller.signal));
      if (!this.valid(t)) return;
      this.state.confirmed = true;
      this.state.notice = '已确认，尚未保存。请明确点击保存。';
      this.emit();
    } catch (e) {
      this.fail(t, e, ' 仅可查询原操作。');
    } finally {
      this.finish(t);
    }
  }
  async execute() {
    const p = this.state.prepared;
    if (!p || !this.state.confirmed) return;
    const t = this.start();
    if (!t) return;
    let submitted = false;
    try {
      await this.verifyContext(t);
      if (!this.valid(t)) return;
      await this.writable(p, t);
      if (!this.valid(t)) return;
      this.row = await this.observe(t, () =>
        this.store.change(this.row!, { phase: 'EXECUTION_SENT' }, () => this.valid(t)),
      );
      if (!this.valid(t)) return;
      // Never keep a confirmation reusable after execute starts, including errors.
      this.state.prepared = null;
      this.state.confirmed = false;
      this.emit();
      submitted = true;
      await this.observe(t, () => this.api.execute(p, t.controller.signal));
      if (!this.valid(t)) return;
      await this.readOriginal(t, p.operation_id, p.task_id);
    } catch (e) {
      if (!this.valid(t)) return;
      this.fail(t, e, ' 保存结果未核清，仅查询原操作，不重发写请求。');
      if (submitted && this.valid(t)) {
        try {
          await this.readOriginal(t, p.operation_id, p.task_id);
        } catch (queryError) {
          this.fail(t, queryError, ' 未核清保护仍保留，请稍后只查原号。');
        }
      }
    } finally {
      this.finish(t);
    }
  }
  private async readOriginal(t: Ticket, operation: string, task?: string) {
    const result = await this.observe(t, () => this.api.get(operation, task, t.controller.signal));
    if (!this.valid(t)) return;
    // Complete/consistent GET is the only event that resolves this original.
    const rows = await this.observe(t, () => this.store.read(() => this.valid(t)));
    for (const row of rows.filter(
      (r) => r.operation_id === result.operation_id && r.task_id === result.task_id,
    )) {
      if (row.protocol === 'V2') {
        if (row.recovery_realm !== this.realm) continue;
        const proof = await this.observe(t, () =>
          this.api.read(row.recovery_realm!, row.nonce!, t.controller.signal),
        );
        if (
          proof.state !== 'SAVED' ||
          proof.task_id !== result.task_id ||
          proof.operation_id !== result.operation_id
        )
          throw new Error('保存结果与原nonce关联未核实。');
      }
      await this.observe(t, () =>
        this.store.change(row, { phase: 'RESOLVED' }, () => this.valid(t)),
      );
    }
    if (!this.valid(t)) return;
    this.state.result = result;
    this.state.notice = '已按原操作号查询，反馈已保存。未授权锁卡，不自动重试。';
    this.emit();
  }
  async query(operation: string, task?: string) {
    const t = this.start();
    if (!t) return;
    this.state.prepared = null;
    this.state.confirmed = false;
    this.state.result = null;
    this.emit();
    try {
      await this.verifyContext(t);
      if (!this.valid(t)) return;
      // A read-only lookup is not evidence that a write was attempted. Only
      // reserve/bind around an actual prepare may create blocking recovery.
      await this.readOriginal(t, operation, task);
    } catch (e) {
      this.fail(t, e, ' 本次只读查询未发起写入；已有未核清原号继续保留，不会重写。');
    } finally {
      this.finish(t);
    }
  }
  async recover(row: RecoveryRow, close = false) {
    const t = this.start();
    if (!t) return;
    this.state.prepared = null;
    this.state.confirmed = false;
    this.row = null;
    this.emit();
    try {
      await this.verifyContext(t);
      if (!this.valid(t)) return;
      if (row.protocol === 'LEGACY') {
        if (close || !row.operation_id)
          throw new Error('旧协议缺少nonce和受检关闭屏障，不能自动关闭。请保留原件由后台核对。');
        await this.readOriginal(t, row.operation_id, row.task_id ?? undefined);
        return;
      }
      if (row.recovery_realm !== this.realm)
        throw new Error('恢复环境不符，保留原realm，不向新环境发送旧nonce。');
      let current = row;
      if (close) {
        current = await this.observe(t, () =>
          this.store.change(current, { phase: 'CLOSE_SENT' }, () => this.valid(t)),
        );
        if (!this.valid(t)) return;
        const proof = await this.observe(t, () =>
          this.api.abandon(current.recovery_realm!, current.nonce!, t.controller.signal),
        );
        if (
          (current.task_id !== null &&
            proof.task_id !== null &&
            current.task_id !== proof.task_id) ||
          (current.operation_id !== null &&
            proof.operation_id !== null &&
            current.operation_id !== proof.operation_id)
        )
          throw new Error('关闭证明原号不匹配。');
        await this.observe(t, () =>
          this.store.change(
            current,
            { phase: 'RESOLVED', terminal_fence_revision: proof.fence_revision },
            () => this.valid(t),
          ),
        );
        if (!this.valid(t)) return;
        this.state.notice = '本条原准备已明确放弃；重新准备需再次选择并本人确认。';
      } else {
        const proof = await this.observe(t, () =>
          this.api.read(current.recovery_realm!, current.nonce!, t.controller.signal),
        );
        if (
          (current.task_id !== null &&
            proof.task_id !== null &&
            current.task_id !== proof.task_id) ||
          (current.operation_id !== null &&
            proof.operation_id !== null &&
            current.operation_id !== proof.operation_id)
        )
          throw new Error('恢复响应原号不匹配。');
        if (proof.state === 'ABANDONED') {
          await this.observe(t, () =>
            this.store.change(
              current,
              { phase: 'RESOLVED', terminal_fence_revision: proof.fence_revision },
              () => this.valid(t),
            ),
          );
          this.state.notice = '本条原准备的永久关闭证明已核实。';
        } else if (proof.state === 'SAVED' && proof.operation_id && proof.task_id) {
          if (current.operation_id === null)
            current = await this.observe(t, () =>
              this.store.change(
                current,
                { task_id: proof.task_id, operation_id: proof.operation_id, phase: 'BOUND' },
                () => this.valid(t),
              ),
            );
          await this.readOriginal(t, proof.operation_id, proof.task_id);
        } else {
          if (current.operation_id === null && proof.operation_id && proof.task_id)
            await this.observe(t, () =>
              this.store.change(
                current,
                {
                  task_id: proof.task_id,
                  operation_id: proof.operation_id,
                  phase: proof.state === 'EXECUTION_ADMITTED' ? 'EXECUTION_SENT' : 'BOUND',
                },
                () => this.valid(t),
              ),
            );
          this.state.notice =
            proof.state === 'EXECUTION_ADMITTED'
              ? '原执行已入场，结果尚未核清；只能查询，不可放弃或重办。'
              : '原准备已找到。刷新后不能恢复旧确认；尚未执行时可明确放弃后重新准备。';
        }
      }
      if (this.valid(t)) this.emit();
    } catch (e) {
      this.fail(t, e, ' 本条保护继续保留。');
    } finally {
      this.finish(t);
    }
  }
}
