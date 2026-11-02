import { z } from 'zod';
import { request, RuntimeApiError, RuntimeTransportError } from './client';
import { choiceSchema, sourceKeySchema, feedbackResultSchema } from './billing-feedback-client';

const id=z.string().min(1).max(200), rev=z.number().int().nonnegative().safe();
export const revisionIntentSchema=z.object({input_version:z.literal(1),root_operation_id:id,
  expected_revision:rev,action:z.enum(['CORRECT','REVOKE','RESUBMIT']),choice:choiceSchema.optional(),nonce:z.string().uuid()}).strict();
export type RevisionIntent=z.infer<typeof revisionIntentSchema>;
export const historySchema=z.object({history_contract:z.literal('BILLING_FEEDBACK_HISTORY_V1'),root_operation_id:id,
  revision:rev,active:z.boolean(),choice:choiceSchema.nullable(),
  history:z.array(z.object({revision:rev,operation_id:id,action:z.enum(['INITIAL','CORRECT','REVOKE','RESUBMIT']),active:z.boolean(),choice:choiceSchema.nullable()}).strict()),
  card_head_adopted:z.literal(false),member_receipt_changed:z.literal(false),lock_authorized:z.literal(false)}).strict()
  .superRefine((v,c)=>{
    if(v.history.length!==v.revision+1 || v.history.some((r,i)=>r.revision!==i || r.active!==(r.choice!==null)) ||
      v.history.at(-1)?.choice!==v.choice || v.history.at(-1)?.active!==v.active)
      c.addIssue({code:'custom',message:'inconsistent complete opinion history'});
  });
export type FeedbackHistory=z.infer<typeof historySchema>;
const parameters=z.object({input_version:z.literal(2),report_id:id,original_digest:z.string().regex(/^[a-f0-9]{64}$/),transaction:sourceKeySchema,choice:choiceSchema,
  revision:z.object({root_operation_id:id,expected_revision:rev,action:z.enum(['CORRECT','REVOKE','RESUBMIT']),nonce:z.string().uuid()}).strict()}).strict();
// Exact public query_revision/execute receipt, after the server strips private bindings.
const savedRevisionSchema=z.object({history_contract:z.literal('BILLING_FEEDBACK_HISTORY_V1'),root_operation_id:id,
  revision:rev,previous_revision:rev,action:z.enum(['CORRECT','REVOKE','RESUBMIT']),active:z.boolean(),choice:choiceSchema.nullable(),
  parameters,card_head_adopted:z.literal(false),member_receipt_changed:z.literal(false)}).strict().superRefine((v,c)=>{
  const r=v.parameters.revision;
  if(v.root_operation_id!==r.root_operation_id||v.previous_revision!==r.expected_revision||v.revision!==v.previous_revision+1||
    v.action!==r.action||v.active!==(r.action!=='REVOKE')||v.choice!==(r.action==='REVOKE'?null:v.parameters.choice))
    c.addIssue({code:'custom',message:'inconsistent original saved receipt'});
});
function savedForParameters(value:unknown,expected:z.infer<typeof parameters>){
  const saved=parse(savedRevisionSchema,value);
  if(JSON.stringify(saved.parameters)!==JSON.stringify(expected))throw new RuntimeTransportError('INVALID_RESPONSE');
  return saved;
}
const preparedSchema=z.object({contract_version:z.literal(1),scope:z.literal('BILLING_FEEDBACK_LOCAL_ONLY'),task_id:id,operation_id:id,parameters,
  confirmation:z.object({confirmation_ref:id,status:z.enum(['PREPARED','CONFIRMED']),expires_at:z.string().datetime({offset:true})}).strict(),
  lock_authorized:z.literal(false),automatic_retry:z.literal(false)}).strict();
export type RevisionPrepared=z.infer<typeof preparedSchema>;
function parse<T>(schema:z.ZodType<T>,value:unknown):T {const v=schema.safeParse(value);if(!v.success)throw new RuntimeTransportError('INVALID_RESPONSE');return v.data;}
export async function getFeedbackHistory(root:string,signal?:AbortSignal){
  const v=parse(historySchema,await request(`/api/billing-feedback/roots/${encodeURIComponent(root)}/head`,'GET',undefined,signal));
  if(v.root_operation_id!==root)throw Error('反馈根号不匹配');return v;
}
export async function prepareRevision(intent:RevisionIntent,signal?:AbortSignal){
  const input=parse(revisionIntentSchema,intent);
  const v=parse(preparedSchema,await request('/api/billing-feedback/revisions/prepare','POST',input,signal));
  const r=v.parameters.revision;
  if(r.root_operation_id!==input.root_operation_id || r.expected_revision!==input.expected_revision || r.action!==input.action || r.nonce!==input.nonce ||
    (input.action!=='REVOKE' && v.parameters.choice!==input.choice))throw Error('修改参数与原选择不一致');
  return v;
}
const base=(p:RevisionPrepared)=>`/api/billing-feedback/tasks/${encodeURIComponent(p.task_id)}/operations/${encodeURIComponent(p.operation_id)}`;
export async function confirmRevision(p:RevisionPrepared,signal?:AbortSignal){
  const schema=z.object({contract_version:z.literal(1),scope:z.literal('BILLING_FEEDBACK_LOCAL_ONLY'),task_id:id,operation_id:id,confirmation_ref:id,status:z.literal('CONFIRMED'),lock_authorized:z.literal(false),automatic_retry:z.literal(false)}).strict();
  const v=parse(schema,await request(base(p)+'/confirm','POST',{confirmation_ref:p.confirmation.confirmation_ref},signal));
  if(v.operation_id!==p.operation_id || v.task_id!==p.task_id || v.confirmation_ref!==p.confirmation.confirmation_ref)throw Error('原确认不匹配');
}
export async function executeRevision(p:RevisionPrepared,signal?:AbortSignal){
  const v=parse(feedbackResultSchema,await request(base(p)+'/execute','POST',{confirmation_ref:p.confirmation.confirmation_ref},signal));
  if(v.operation_id!==p.operation_id || v.task_id!==p.task_id)throw Error('修改回执与原单不匹配');
  return {...v,saved:savedForParameters(v.saved,p.parameters)};
}
export async function abandonRevision(p:RevisionIntent,signal?:AbortSignal){
  const schema=z.object({history_contract:z.literal('BILLING_FEEDBACK_HISTORY_V1'),task_id:id,operation_id:id,nonce:z.literal(p.nonce),root_operation_id:z.literal(p.root_operation_id),preparation_closed:z.literal(true),saved_opinion_changed:z.literal(false),automatic_retry:z.literal(false)}).strict();
  return parse(schema,await request('/api/billing-feedback/revisions/preparations/'+encodeURIComponent(p.nonce)+'/abandon','POST',
    {root_operation_id:p.root_operation_id,acknowledgement:'CLOSE_UNSUBMITTED_REVISION_ONLY'},signal));
}
export async function closeUncreatedRevisionIntent(p:RevisionIntent,signal?:AbortSignal){
  const intent=parse(revisionIntentSchema,p);
  const schema=z.object({history_contract:z.literal('BILLING_FEEDBACK_HISTORY_V1'),intent:revisionIntentSchema,
    task_id:id,operation_id:id,state:z.literal('STALE_UNCREATED_INTENT'),current_revision:rev,
    owner_scoped:z.literal(true),not_created:z.literal(true),may_end_local_intent:z.literal(true),
    original_preserved:z.literal(true),saved_opinion_changed:z.literal(false),automatic_retry:z.literal(false)}).strict();
  const proof=parse(schema,await request('/api/billing-feedback/revisions/uncreated-intents/close','POST',
    {intent,acknowledgement:'END_EXACT_STALE_UNCREATED_INTENT'},signal));
  if(JSON.stringify(proof.intent)!==JSON.stringify(intent) || proof.current_revision<=intent.expected_revision)
    throw new RuntimeTransportError('INVALID_RESPONSE');
  return proof;
}
export async function closePartialRevisionPreparation(p:RevisionIntent,signal?:AbortSignal){
  const intent=parse(revisionIntentSchema,p);
  const schema=z.object({history_contract:z.literal('BILLING_FEEDBACK_HISTORY_V1'),intent:revisionIntentSchema,
    task_id:id,operation_id:id,partial_preparation_closed:z.literal(true),original_preserved:z.literal(true),
    saved_opinion_changed:z.literal(false),automatic_retry:z.literal(false)}).strict();
  const proof=parse(schema,await request('/api/billing-feedback/revisions/partial-preparations/close','POST',
    {intent,acknowledgement:'CLOSE_EXACT_PARTIAL_REVISION_PREPARATION'},signal));
  if(JSON.stringify(proof.intent)!==JSON.stringify(intent))throw new RuntimeTransportError('INVALID_RESPONSE');
  return proof;
}
export async function readRevisionNonce(nonce:string,root:string,signal?:AbortSignal){
  const schema=z.object({history_contract:z.literal('BILLING_FEEDBACK_HISTORY_V1'),task_id:id,operation_id:id,parameters,task_state:z.string(),saved:savedRevisionSchema.nullable(),confirmation_restored:z.literal(false),automatic_retry:z.literal(false)}).strict();
  const v=parse(schema,await request('/api/billing-feedback/revisions/preparations/'+encodeURIComponent(nonce),'GET',undefined,signal));
  if(v.parameters.revision.nonce!==nonce || v.parameters.revision.root_operation_id!==root)throw Error('修改原号不匹配');
  if(v.saved){
    if(v.task_state!=='COMPLETED')throw new RuntimeTransportError('INVALID_RESPONSE');
    savedForParameters(v.saved,v.parameters);
  }
  return v;
}
export async function readRevisionIntent(p:RevisionIntent,signal?:AbortSignal){
  const intent=parse(revisionIntentSchema,p);
  if(intent.action==='REVOKE'&&intent.choice!==undefined)throw new RuntimeTransportError('INVALID_RESPONSE');
  const v=await readRevisionNonce(intent.nonce,intent.root_operation_id,signal),r=v.parameters.revision;
  if(r.expected_revision!==intent.expected_revision||r.action!==intent.action||
    (intent.action!=='REVOKE'&&v.parameters.choice!==intent.choice))throw new RuntimeTransportError('INVALID_RESPONSE');
  return v;
}

/** A fresh original read selects the protocol; only a bound server proof releases it. */
export async function cancelRevisionIntent(p:RevisionIntent,signal?:AbortSignal){
  const intent=parse(revisionIntentSchema,p);
  let original:Awaited<ReturnType<typeof readRevisionNonce>>;
  try { original=await readRevisionIntent(intent,signal); }
  catch(e){
    if(!(e instanceof RuntimeApiError&&e.status===404&&e.reason==='NOT_FOUND'))throw e;
    // A 404 is only a routing hint. The monotone, full-intent proof remains mandatory.
    return {kind:'CLOSED' as const,proof:await closeUncreatedRevisionIntent(intent,signal)};
  }
  if(original.saved){
    return {kind:'SAVED' as const,saved:original.saved};
  }
  let proof;
  try {proof=await closePartialRevisionPreparation(intent,signal);}
  catch(e){
    // No fallback for unknown transport, timeouts, bad schemas, 404 or damaged data.
    if(!(e instanceof RuntimeApiError&&e.status===409&&e.reason==='IDEMPOTENCY_CONFLICT'))throw e;
    proof=await abandonRevision(intent,signal);
  }
  if(proof.task_id!==original.task_id||proof.operation_id!==original.operation_id)
    throw new RuntimeTransportError('INVALID_RESPONSE');
  return {kind:'CLOSED' as const,proof};
}
