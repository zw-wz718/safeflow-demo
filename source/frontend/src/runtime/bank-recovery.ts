import { z } from 'zod';
import { request, RuntimeTransportError } from './client';
import { lifecycleSchema, intentTerminationSchema } from './agent-client';
import { paymentSchema, fundsResultSchema, fundsMatches, paymentBase } from './payment-schema';

const id = z.string().regex(/^[A-Za-z0-9_.-]{1,200}$/);
// All recovery HTTP reads share this budget, including multiple preparations
// and simultaneously mounted views. No settled response is cached as authority.
const readLimit = 6;
let activeReads = 0;
const waiting = new Set<() => void>();
async function read(path: string, signal?: AbortSignal) {
  await new Promise<void>((resolve, reject) => {
    const cancel = () => {
      waiting.delete(start);
      reject(new RuntimeTransportError('ABORTED'));
    };
    const start = () => {
      if (signal?.aborted) return cancel();
      if (activeReads >= readLimit) return;
      waiting.delete(start);
      signal?.removeEventListener('abort', cancel);
      activeReads++;
      resolve();
    };
    signal?.addEventListener('abort', cancel, { once: true });
    waiting.add(start);
    start();
  });
  try {
    return await request(path, 'GET', undefined, signal);
  } finally {
    activeReads--;
    for (const next of waiting) {
      next();
      if (activeReads >= readLimit) break;
    }
  }
}
async function parallel<T, R>(items: T[], work: (item: T) => Promise<R>, limit = 4) {
  const values: R[] = new Array(items.length);
  let next = 0;
  await Promise.all(
    Array.from({ length: Math.min(limit, items.length) }, async () => {
      while (next < items.length) {
        const index = next++;
        values[index] = await work(items[index]);
      }
    }),
  );
  return values;
}
const directorySchema = z
  .object({
    context_ref: z.string().regex(/^[a-f0-9]{64}$/),
    payment_tasks: z.array(id).max(256),
    subscription_nonces: z.array(z.string().uuid()).max(256),
    closed_nonfunding_tasks:z.array(id).max(256).optional().default([]),
    ended_draft_intents:z.array(intentTerminationSchema).max(256).optional().default([]),
  })
  .strict().superRefine((v,c)=>{
    if(new Set(v.payment_tasks).size!==v.payment_tasks.length||new Set(v.closed_nonfunding_tasks).size!==v.closed_nonfunding_tasks.length||
      v.closed_nonfunding_tasks.some(id=>!v.payment_tasks.includes(id))) c.addIssue({code:'custom',message:'invalid closure directory'});
  });
export async function recoveryDirectory(signal?: AbortSignal) {
  z.object({contract_version:z.literal(1),expired_draft_intents:z.array(id),payment_cancelled:z.literal(false),server_time:z.literal(true)}).strict().parse(await request('/api/bank/expire-draft-intents','POST',{contract_version:2,scope:'MOCK_DRAFT_LIFECYCLE_ONLY'},signal));
  return directorySchema.parse(await read('/api/bank/recovery', signal));
}
/** Reads every preparation: a cancelled draft cannot hide a submitted payment. */
export async function readPaymentTask(task: string, signal?: AbortSignal) {
  const [rawDraft, rawPayments] = await Promise.all([
    read(`/api/sim/draft-lifecycles/${encodeURIComponent(task)}`, signal),
    read(`/api/sim/payment-preparations/${encodeURIComponent(task)}`, signal),
  ]);
  const draft = lifecycleSchema.parse(rawDraft);
  if (draft.task_id !== task) throw Error('草案原号不匹配');
  const payments = z.array(paymentSchema).max(256).parse(rawPayments);
  const results = await parallel(payments, async (p) => {
    if (p.task_id !== task) throw Error('付款原号不匹配');
    const funds = fundsResultSchema.parse(await read(paymentBase(p) + '/funds-result', signal));
    if (!fundsMatches(p, funds)) throw Error('资金回执与原单不匹配');
    return { payment: p, funds };
  });
  const terminal =
    results.length > 0
      ? results.every(({ funds }) => ['SUCCEEDED', 'FAILED'].includes(funds.state))
      : ['CANCELLED', 'FAILED'].includes(draft.task_state);
  return { task, draft, results, terminal, nonfundingClosed:false };
}
/** No terminal business result is inferred. Both the complete server proof
 * and fresh reads of every preparation must agree before releasing a blocker. */
export function verifiedNonfundingClosure(value:Awaited<ReturnType<typeof readPaymentTask>>,closed:readonly string[],ended:readonly z.infer<typeof intentTerminationSchema>[] = []) {
  const d=value.draft;
  const note=d.intent_termination;
  const preparation=note?.proof.preparation;
  const fixedTasks=['bank_payment_a2719992-8baf-450e-b84d-b976afa1800c','bank_payment_3f89e576-5cbd-426b-9e55-4f7a7e4d593d','bank_chat_b2056dd9-dd20-4200-89c5-32430a2e3cf7_2'];
  const endedPreparation=note?.proof.scope==='EXPIRED_UNSUBMITTED_PREPARATION'&&note.reason==='ABANDONED'&&!!preparation&&fixedTasks.includes(value.task)&&value.results.length===1&&value.results.every(({payment:p,funds:f})=>
    p.task_id===value.task&&p.operation.operation_id===preparation.operation_id&&p.snapshot_digest===preparation.snapshot_digest&&p.expires_at===preparation.expires_at&&
    p.snapshot.transfer.draft_id===preparation.draft_id&&p.snapshot.transfer.draft_version===preparation.draft_version&&f.state==='UNSUBMITTED'&&!f.submitted&&fundsMatches(p,f));
  if(note) return (note.proof.scope==='EXPIRED_UNSUBMITTED_PREPARATION'?endedPreparation:value.results.length===0)&&d.task_id===value.task&&['CREATE_MOCK_DRAFT','MODIFY_MOCK_DRAFT'].includes(d.action)&&d.next_action.kind==='FINISHED'&&d.execution_status!=='RESULT_UNKNOWN'&&ended.some(n=>n.task_id===value.task&&n.operation_id===d.operation_id&&JSON.stringify(n)===JSON.stringify(note));
  return closed.includes(value.task)&&value.results.length===0&&d.task_id===value.task&&
    d.execution_status==='RESULT_UNKNOWN'&&d.action==='CREATE_MOCK_DRAFT'&&
    d.task_state==='RUNNING'&&d.operation_state==='DRAFT'&&d.observed_draft===null&&
    d.parameters.replaces==null;
}
export type RecoveredPayment = Awaited<ReturnType<typeof readPaymentTask>>;

/** User supplies the new amount explicitly. Other fields stay visibly bound
 * to the selected original recipient/account; this never submits a payment. */
export function replacementIntent(draft:z.infer<typeof lifecycleSchema>,amount:string) {
  if(!/^\d+\.\d{2}$/.test(amount)||!/[1-9]/.test(amount))throw Error('请填写新的转账金额，保留两位小数');
  const fields=draft.action==='CREATE_MOCK_DRAFT'?draft.parameters:{...draft.observed_draft?.parameters,...(draft.parameters.changes as object)};
  const p=z.object({payer_account_id:id,payee_id:id,currency:z.literal('CNY'),remark:z.string().max(1000)}).passthrough().parse(fields);
  return {task_id:`bank_replace_${crypto.randomUUID()}`,facts_version:draft.facts_version,parameters:{payer_account_id:p.payer_account_id,payee_id:p.payee_id,amount,currency:p.currency,remark:p.remark,replaces:null}};
}

export async function recoverPaymentTasks(
  tasks: string[],
  signal: AbortSignal,
  onResult?: (task: string, value?: RecoveredPayment, error?: unknown) => void,
) {
  const values = new Map<string, RecoveredPayment>();
  const failures = new Map<string, unknown>();
  await parallel([...new Set(tasks)], async (task) => {
    if (signal.aborted) return;
    try {
      const value = await readPaymentTask(task, signal);
      if (!signal.aborted) {
        values.set(task, value);
        onResult?.(task, value);
      }
    } catch (error) {
      if (!signal.aborted) {
        failures.set(task, error);
        onResult?.(task, undefined, error);
      }
    }
  });
  return { values, failures };
}


// A 404 or a missing directory entry is never this proof. Exact user context
// and request namespace must match a fresh authenticated server response.
const canonicalUuid = '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}';
const chatRequest = new RegExp(`^bank_chat_(?:v2_${canonicalUuid}_)?${canonicalUuid}_[1-9][0-9]*$`);
const uncreatedSchema = z.object({
  contract_version: z.literal(1), scope: z.literal('UNCREATED_CHAT_REQUEST_FENCED'),
  task_id: id, context_ref: z.string().regex(/^[a-f0-9]{64}$/),
  creation_disabled: z.literal(true), task_created: z.literal(false), payment_cancelled: z.literal(false),
  proof_source: z.enum(['COMPLETED_SERVER_TURN', 'RETIRED_REQUEST_NAMESPACE']),
}).strict();
export async function readUncreatedChatRequest(task: string, context: string, signal?: AbortSignal) {
  if (!chatRequest.test(task)) throw Error('不支持的聊天原号，仍须核查原任务');
  const proof = uncreatedSchema.parse(await read(`/api/bank/uncreated-chat-requests/${encodeURIComponent(task)}`, signal));
  if (proof.task_id !== task || proof.context_ref !== context ||
      proof.proof_source !== (task.startsWith('bank_chat_v2_') ? 'COMPLETED_SERVER_TURN' : 'RETIRED_REQUEST_NAMESPACE'))
    throw Error('未创建请求的恢复证明不匹配');
  return proof;
}
