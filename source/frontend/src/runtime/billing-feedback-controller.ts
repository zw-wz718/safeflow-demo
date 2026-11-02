import { getAgent } from './agent-client';
import { RuntimeApiError, RuntimeTransportError } from './client';
import { cardContext } from './card-read';
import {
  prepareBillingFeedback,
  confirmBillingFeedback,
  executeBillingFeedback,
  getBillingFeedback,
  feedbackEligible,
  feedbackError,
  type BillingResult,
  type FeedbackIntent,
  type FeedbackPrepared,
  type FeedbackResult,
} from './billing-feedback-client';
import { FeedbackRecoveryStore, type FeedbackRecovery } from './billing-feedback-store';

export type FeedbackState = {
  reports: BillingResult[];
  reportTask: string;
  selected: number | null;
  evidence: number | null;
  choice: FeedbackIntent['choice'] | '';
  prepared: FeedbackPrepared | null;
  confirmed: boolean;
  result: FeedbackResult | null;
  busy: boolean;
  notice: string;
  recovery: FeedbackRecovery[];
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
  context: cardContext,
  getAgent,
  prepare: prepareBillingFeedback,
  confirm: confirmBillingFeedback,
  execute: executeBillingFeedback,
  get: getBillingFeedback,
};
type Ticket = { epoch: number; generation: number; controller: AbortController };

// A narrow page controller. Every asynchronous continuation checks both the live
// host session and its page/request epoch BEFORE any UI or recovery mutation.
export class BillingFeedbackController {
  state = empty();
  private active = false;
  private epoch = 0;
  private generation = -1;
  private pending: AbortController | null = null;
  private attempt: string | null = null;
  private context: string | null = null;
  constructor(
    private readonly getGeneration: () => number,
    private readonly notify: (state: FeedbackState) => void,
    private readonly authLost: () => void,
    private readonly store = new FeedbackRecoveryStore(),
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
    this.attempt = null;
    this.context = null;
    this.state = empty();
  }
  reset() {
    this.epoch++;
    this.pending?.abort();
    this.pending = null;
    this.attempt = null;
    this.context = null;
    this.generation = this.getGeneration();
    this.state = empty();
    this.refreshRecovery();
    this.emit();
  }
  private emit() {
    if (this.active) this.notify({ ...this.state });
  }
  private refreshRecovery() {
    try {
      this.state.recovery = this.store.read();
      this.state.storageError = false;
    } catch (e) {
      this.state.storageError = true;
      this.state.notice = feedbackError(e);
    }
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
      this.refreshRecovery();
      this.emit();
    }
  }
  private async verifyContext(t: Ticket) {
    if (!this.valid(t)) throw new RuntimeTransportError('ABORTED');
    let current: string | null;
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
    if (!current || (this.context !== null && current !== this.context)) {
      this.reset();
      this.state.notice =
        '服务端会话已变化，旧报告、选择和确认已清除；未核清原号保留，请在当前会话只读核对。';
      this.emit();
      if (!current) this.authLost();
      throw new RuntimeTransportError('ABORTED');
    }
    this.context = current;
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
    this.attempt = null;
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
    this.refreshRecovery();
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
    this.refreshRecovery();
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
      this.store.reserve(attempt);
      this.attempt = attempt;
      const p = await this.observe(t, () => this.api.prepare(intent, t.controller.signal));
      if (!this.valid(t)) return;
      this.store.bind(attempt, p);
      this.state.prepared = p;
      this.state.confirmed = false;
      this.state.notice = '仅已准备，尚未确认或保存。请核对服务端原参数。';
      this.emit();
    } catch (e) {
      if (!this.valid(t)) return;
      // 501 is returned before service creation; other failures may follow a
      // partial prepare. There is no prepare idempotency/reverse lookup contract.
      if (e instanceof RuntimeApiError && e.validated && e.status === 501) {
        try {
          this.store.remove(attempt);
        } catch {
          /* retain blocking storage error */
        }
      }
      this.fail(t, e, ' 若未取得操作号，无法通过现有协议恢复准备结果；不会自动重试。');
    } finally {
      this.finish(t);
    }
  }
  private writable(p: FeedbackPrepared) {
    if (!this.attempt || Date.parse(p.confirmation.expires_at) <= Date.now())
      throw new Error('确认已过期或不属于本次页面动作，只能查询原号。');
    this.store.verifyOnly(this.attempt, p);
  }
  async confirm() {
    const p = this.state.prepared;
    if (!p || this.state.confirmed) return;
    const t = this.start();
    if (!t) return;
    try {
      await this.verifyContext(t);
      if (!this.valid(t)) return;
      this.writable(p);
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
      this.writable(p);
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
    this.store.resolveOriginal(result);
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
}
