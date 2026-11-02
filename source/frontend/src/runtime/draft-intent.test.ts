import {describe,it,expect,vi} from 'vitest';
import fixture from './bank-recovery.fixture.json';
import {intentTerminationSchema,lifecycleSchema} from './agent-client';
import {replacementIntent,verifiedNonfundingClosure} from './bank-recovery';
import {RuntimeApiError} from './client';
import {errorSchema} from './schema';
import {runtimeErrorText} from './runtime-feedback';

const note={schema:'UNEXECUTED_DRAFT_TERMINATED_V1',task_id:fixture.draft.task_id,operation_id:fixture.draft.operation_id,reason:'ABANDONED',
 proof:{scope:'DRAFT_CREATED_NO_PAYMENT_ACCEPTANCE',task_digest:'a'.repeat(64),operations_digest:'b'.repeat(64),parameters_digest:'c'.repeat(64),receipt_digest:'d'.repeat(64),expires_at:'2026-10-31T01:00:00Z'},
 original_preserved:true,payment_cancelled:false,funding_authorized:false,replacement:null};
describe('proven draft termination is distinct from a funds result',()=>{
 it('requires complete matching server directory proof and no payment preparations',()=>{
  const n=intentTerminationSchema.parse(note);
  const draft=lifecycleSchema.parse({...fixture.draft,active_operation_id:fixture.draft.operation_id,next_action:{kind:'FINISHED'},intent_termination:n});
  const value={task:draft.task_id,draft,results:[],terminal:false,nonfundingClosed:false};
  expect(verifiedNonfundingClosure(value,[],[n])).toBe(true);
  expect(verifiedNonfundingClosure(value,[],[])).toBe(false);
  expect(verifiedNonfundingClosure({...value,results:[{payment:fixture.payment,funds:fixture.funds}] as never},[],[n])).toBe(false);
  expect(lifecycleSchema.safeParse({...draft,execution_status:'RESULT_UNKNOWN'}).success).toBe(false);
 });
 it('rejects inconsistent replacement, invented deadline and receipt-less successful scope',()=>{
  expect(intentTerminationSchema.safeParse({...note,reason:'REPLACED'}).success).toBe(false);
  expect(intentTerminationSchema.safeParse({...note,reason:'EXPIRED',proof:{...note.proof,expires_at:null}}).success).toBe(false);
  expect(intentTerminationSchema.safeParse({...note,proof:{...note.proof,receipt_digest:null}}).success).toBe(false);
 });
 it('binds new amount to actual old account and recipient, with a new task id',()=>{
  const d=lifecycleSchema.parse(fixture.draft);
  vi.stubGlobal('crypto',{randomUUID:()=> '12345678-1234-4234-8234-123456789012'});
  try{
   const r=replacementIntent(d,'123.45');expect(r.parameters).toMatchObject({payer_account_id:'account_001',payee_id:'payee_008',amount:'123.45',replaces:null});expect(r.task_id).not.toBe(d.task_id);
   for(const invalid of ['0.00','-1.00','1.001','NaN','1',''])expect(()=>replacementIntent(d,invalid)).toThrow();
  }finally{vi.unstubAllGlobals();}
 });
 it('binds a task termination to the active operation while preserving a selected original receipt',()=>{
  const active='operation_new_modify';
  const raw={...fixture.draft,plan_version:2,active_operation_id:active,is_current_action:false,next_action:{kind:'FINISHED'},intent_termination:{...note,operation_id:active}};
  const historical=lifecycleSchema.parse(raw);
  expect(historical.operation_id).toBe(fixture.draft.operation_id);
  expect(historical.receipt).toEqual(fixture.draft.receipt);
  expect(historical.intent_termination?.operation_id).toBe(active);
  for(const invalid of [
    {...raw,active_operation_id:undefined},
    {...raw,active_operation_id:'unrelated_operation'},
    {...raw,is_current_action:true},
    {...raw,operation_plan_version:2},
    {...raw,intent_termination:{...note,operation_id:raw.operation_id}},
    {...raw,unknown_operation_id:'unknown',execution_status:'RESULT_UNKNOWN'},
    {...raw,intent_termination:{...raw.intent_termination,task_id:'another_task'}},
  ]) expect(lifecycleSchema.safeParse(invalid).success).toBe(false);
 });
 it('parses specific ended errors without describing funding uncertainty or missing dependencies',()=>{
  for(const reason of ['DRAFT_INTENT_EXPIRED','DRAFT_INTENT_REPLACED','DRAFT_INTENT_ABANDONED'] as const){
   const envelope=errorSchema.parse({contract_version:1,error:{reason_code:reason,message:'未付款草案已结束',task_id:'original_task',next_action:'QUERY_ORIGINAL_TASK'}});
   const text=runtimeErrorText(new RuntimeApiError(envelope.error.reason_code,'original_task','QUERY_ORIGINAL_TASK',409),'transfer');
   expect(text).toContain('未付款草案');expect(text).toContain('旧确认不能继续使用');
   expect(text).not.toMatch(/结果未明|不能确认|暂不可用|依赖故障|不要重新付款/);
  }
 });
});

it('releases only the three fixed tasks with complete matching unsubmitted preparation proof',()=>{
 const tasks=['bank_payment_a2719992-8baf-450e-b84d-b976afa1800c','bank_payment_3f89e576-5cbd-426b-9e55-4f7a7e4d593d','bank_chat_b2056dd9-dd20-4200-89c5-32430a2e3cf7_2'];
 for(const task of tasks){
  const p=structuredClone(fixture.payment);p.task_id=task;p.operation.task_id=task;p.snapshot.transfer.task_id=task;
  const f={...structuredClone(fixture.funds),task_id:task,transfer:{...fixture.funds.transfer,task_id:task},state:'UNSUBMITTED',submitted:false};
  const proof={...note.proof,scope:'EXPIRED_UNSUBMITTED_PREPARATION',preparation:{operation_id:p.operation.operation_id,snapshot_digest:p.snapshot_digest,expires_at:p.expires_at,draft_id:p.snapshot.transfer.draft_id,draft_version:p.snapshot.transfer.draft_version}};
  const n=intentTerminationSchema.parse({...note,task_id:task,proof});
  const d=lifecycleSchema.parse({...fixture.draft,task_id:task,active_operation_id:fixture.draft.operation_id,next_action:{kind:'FINISHED'},intent_termination:n});
  const v={task,draft:d,results:[{payment:p,funds:f}],terminal:false,nonfundingClosed:false};
  const verify=(value:unknown,ended:unknown=[n])=>verifiedNonfundingClosure(value as never,[],ended as never);
  expect(verify(v)).toBe(true);expect(verify(v,[])).toBe(false);
  expect(verify({...v,results:[]})).toBe(false);
  expect(verify({...v,results:[...v.results,...v.results]})).toBe(false);
  for(const state of ['REGISTERED_UNSUBMITTED','PROCESSING','RECONCILE_REQUIRED','SUCCEEDED','FAILED'])expect(verify({...v,results:[{payment:p,funds:{...f,state}}]})).toBe(false);
  expect(verify({...v,results:[{payment:p,funds:{...f,submitted:true}}]})).toBe(false);
  expect(verify({...v,results:[{payment:p,funds:{...f,snapshot_digest:'e'.repeat(64)}}]})).toBe(false);
  expect(verify({...v,results:[{payment:{...p,expires_at:'2027-01-01T00:00:00Z'},funds:f}]})).toBe(false);
  expect(verify({...v,task:'unlisted_task'})).toBe(false);
  for(const changed of [{...n,reason:'EXPIRED'},{...n,proof:{...proof,preparation:undefined}},{...n,proof:{...proof,scope:'DRAFT_CREATED_NO_PAYMENT_ACCEPTANCE'}}])expect(intentTerminationSchema.safeParse(changed).success).toBe(false);
 }
});
