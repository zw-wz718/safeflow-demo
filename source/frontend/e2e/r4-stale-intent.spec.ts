import {expect,test,type Page,type Route} from '@playwright/test';
import {guardReplayApi,assertReplayIsolated} from './isolated-api';
test.afterEach(({page})=>assertReplayIsolated(page));
const send=(r:Route,v:unknown,status=200)=>r.fulfill({status,contentType:'application/json',body:JSON.stringify(v)});
const error=(r:Route,status:number,reason:string)=>send(r,{contract_version:1,error:{reason_code:reason,message:'请核对原号',task_id:null,next_action:'NONE'}},status);
const storageKey='safeflow.feedback-history.v1.root_a';
async function fixture(page:Page,mode:'stale'|'lost'|'partial'='stale'){
 await guardReplayApi(page);let version=0,choice:string|null='NOT_MINE',first=true;
 const history:any[]=[{revision:0,operation_id:'root_a',action:'INITIAL',active:true,choice}];
 const preps=new Map<string,any>(),saved=new Map<string,any>();const writes:string[]=[],statuses:number[]=[];
 let failure='';let gate:Promise<void>|undefined;let arrived:(()=>void)|undefined;
 const partial=new Set<string>(),closedPartial=new Set<string>();let losePartialClose=false;
 const head=(root:string)=>({history_contract:'BILLING_FEEDBACK_HISTORY_V1',root_operation_id:root,revision:root==='root_a'?version:0,active:root==='root_a'?choice!==null:true,choice:root==='root_a'?choice:'MINE',history:root==='root_a'?history:[{revision:0,operation_id:root,action:'INITIAL',active:true,choice:'MINE'}],card_head_adopted:false,member_receipt_changed:false,lock_authorized:false});
 await page.route('**/api/billing-feedback/**',async r=>{
  const path=new URL(r.request().url()).pathname;
  if(path.endsWith('/head'))return send(r,head(path.split('/')[4]));
  const body=r.request().method()==='POST'?r.request().postDataJSON():null;
  if(path.endsWith('/partial-preparations/close')){
   writes.push('partial-close');const i=body.intent;expect(body.acknowledgement).toBe('CLOSE_EXACT_PARTIAL_REVISION_PREPARATION');
   arrived?.();if(gate)await gate;
   if(failure==='network')return r.abort();if(failure==='404')return error(r,404,'NOT_FOUND');if(failure==='503')return error(r,503,'DEPENDENCY_UNAVAILABLE');if(failure==='409')return error(r,409,'IDEMPOTENCY_CONFLICT');
   if(!partial.has(i.nonce))return error(r,409,'IDEMPOTENCY_CONFLICT');
   const proof={history_contract:'BILLING_FEEDBACK_HISTORY_V1',intent:{...i},task_id:'task_'+i.nonce,operation_id:'op_'+i.nonce,partial_preparation_closed:true,original_preserved:true,saved_opinion_changed:false,automatic_retry:false};
   if(failure==='nonce')proof.intent.nonce='a3b96596-a712-46d2-86c5-3c9d810f3639';
   if(failure==='choice')proof.intent.choice='NOT_MINE';
   closedPartial.add(i.nonce);if(losePartialClose){losePartialClose=false;return r.abort();}
   return send(r,proof).catch(()=>{});
  }
  if(path.endsWith('/uncreated-intents/close')){
   writes.push('close');const i=body.intent;expect(body.acknowledgement).toBe('END_EXACT_STALE_UNCREATED_INTENT');
   arrived?.();if(gate)await gate;
   if(failure==='network')return r.abort();if(failure==='404')return error(r,404,'NOT_FOUND');if(failure==='503')return error(r,503,'DEPENDENCY_UNAVAILABLE');
   if(preps.has(i.nonce)||version<=i.expected_revision)return error(r,409,'RECONCILE_REQUIRED');
   const proof={history_contract:'BILLING_FEEDBACK_HISTORY_V1',intent:{...i},task_id:'task_'+i.nonce,operation_id:'op_'+i.nonce,state:'STALE_UNCREATED_INTENT',current_revision:version,owner_scoped:true,not_created:true,may_end_local_intent:true,original_preserved:true,saved_opinion_changed:false,automatic_retry:false};
   if(failure==='nonce')proof.intent.nonce='a3b96596-a712-46d2-86c5-3c9d810f3639';
   if(failure==='root')proof.intent.root_operation_id='root_b';
   if(failure==='choice')proof.intent.choice='UNSURE';
   if(failure==='action')proof.intent.action='RESUBMIT';
   if(failure==='expected')proof.intent.expected_revision=1;
   if(failure==='current')proof.current_revision=i.expected_revision;
   return send(r,proof).catch(()=>{});
  }
  if(path.endsWith('/revisions/prepare')){
   writes.push('prepare');const i=body;
   if(first&&mode==='stale'){first=false;version=1;choice='MINE';history.push({revision:1,operation_id:'external',action:'CORRECT',active:true,choice});}
   if(i.expected_revision!==version){statuses.push(409);return error(r,409,'STALE_PLAN');}
   let p=preps.get(i.nonce);
   if(!p){p={contract_version:1,scope:'BILLING_FEEDBACK_LOCAL_ONLY',task_id:'task_'+i.nonce,operation_id:'op_'+i.nonce,parameters:{input_version:2,report_id:'report',original_digest:'a'.repeat(64),transaction:{source_id:'source',account_id:'account_001',transaction_id:'tx'},choice:i.choice??choice,revision:{root_operation_id:i.root_operation_id,expected_revision:i.expected_revision,action:i.action,nonce:i.nonce}},confirmation:{confirmation_ref:'confirmation_'+i.nonce,status:'PREPARED',expires_at:new Date(Date.now()+300000).toISOString()},lock_authorized:false,automatic_retry:false};preps.set(i.nonce,p);}
   if(first&&mode==='partial'){first=false;partial.add(i.nonce);version=1;choice='MINE';history.push({revision:1,operation_id:'external',action:'CORRECT',active:true,choice});return error(r,503,'DEPENDENCY_UNAVAILABLE');}
   if(first&&mode==='lost'){first=false;return r.abort();}return send(r,p);
  }
  if(path.endsWith('/abandon')){
   writes.push('abandon');const nonce=path.split('/').at(-2)!;
   if(!preps.has(nonce)){statuses.push(404);return error(r,404,'NOT_FOUND');}
   if(partial.has(nonce))return error(r,503,'DEPENDENCY_UNAVAILABLE');
   if(saved.has(nonce))return error(r,409,'RECONCILE_REQUIRED');
   return send(r,{history_contract:'BILLING_FEEDBACK_HISTORY_V1',task_id:'task_'+nonce,operation_id:'op_'+nonce,nonce,root_operation_id:'root_a',preparation_closed:true,saved_opinion_changed:false,automatic_retry:false});
  }
  if(path.includes('/revisions/preparations/')){
   const nonce=path.split('/').at(-1)!,p=preps.get(nonce);
   if(failure==='read-network')return r.abort();
   if(!p){statuses.push(404);return error(r,404,'NOT_FOUND');}
   const parameters=structuredClone(p.parameters);if(failure==='read-choice')parameters.choice='UNSURE';
   return send(r,{history_contract:'BILLING_FEEDBACK_HISTORY_V1',task_id:p.task_id,operation_id:p.operation_id,parameters,task_state:closedPartial.has(nonce)?'CANCELLED':saved.has(nonce)?'COMPLETED':'RUNNING',saved:saved.get(nonce)??null,confirmation_restored:false,automatic_retry:false});
  }
  const p=[...preps.values()].find(p=>path.includes(p.operation_id));
  if(path.endsWith('/confirm')){writes.push('confirm');return send(r,{contract_version:1,scope:p.scope,task_id:p.task_id,operation_id:p.operation_id,confirmation_ref:p.confirmation.confirmation_ref,status:'CONFIRMED',lock_authorized:false,automatic_retry:false});}
  if(path.endsWith('/execute')){writes.push('execute');version++;choice=p.parameters.revision.action==='REVOKE'?null:p.parameters.choice;
   const value={history_contract:'BILLING_FEEDBACK_HISTORY_V1',root_operation_id:'root_a',revision:version,previous_revision:p.parameters.revision.expected_revision,action:p.parameters.revision.action,active:choice!==null,choice,parameters:p.parameters,card_head_adopted:false,member_receipt_changed:false};saved.set(p.parameters.revision.nonce,value);history.push({revision:version,operation_id:p.operation_id,action:value.action,active:value.active,choice});
   return send(r,{contract_version:1,scope:p.scope,task_id:p.task_id,operation_id:p.operation_id,task_state:'COMPLETED',operation_state:'SUCCEEDED',saved:value,replayed:false,domain_audit:'PUBLIC_HISTORY_ONLY_V1',lock_authorized:false,automatic_retry:false});}
  throw Error('unexpected mocked API '+path);
 });
 await page.goto('/e2e/site-repairs.html?view=feedback');await page.getByLabel('新的本人判断').selectOption('MINE');await page.getByRole('button',{name:'准备更正',exact:true}).click();
 await expect(page.getByRole('button',{name:'查看办理进度'})).toBeEnabled();
 return {writes,statuses,fail:(v:string)=>{failure=v;},loseClose:()=>{losePartialClose=true;},hold:(promise:Promise<void>,notify:()=>void)=>{gate=promise;arrived=notify;}};
}
const cancel=(page:Page)=>page.getByRole('button',{name:'核对并取消本次修改',exact:true});
const ended='已取消本次未保存的修改，原反馈和历史记录保留。需要修改时请重新选择并确认。';

test('R4 single cancel routes uncreated stale intent only through exact proof and requires a fresh nonce and confirmation',async({page},info)=>{
 const state=await fixture(page);const original=(await page.evaluate(k=>sessionStorage.getItem(k),storageKey))!;
 await page.getByRole('button',{name:'查看办理进度'}).click();await page.getByRole('button',{name:'重新确认原选择'}).click();
 await expect.poll(()=>state.statuses.filter(s=>s===409).length).toBe(2);await expect(cancel(page)).toBeEnabled();
 const before=[...state.writes];await page.reload();await expect(cancel(page)).toBeEnabled();expect(state.writes).toEqual(before);
 await expect(page.getByRole('button',{name:/未创建|部分准备|失效修改/})).toHaveCount(0);
 await expect(page.getByText(JSON.parse(original).nonce,{exact:false})).not.toBeVisible();
 await cancel(page).click();await expect(page.getByText(ended)).toBeVisible();
 const archive=await page.evaluate(k=>JSON.parse(sessionStorage.getItem(k+'.ended-intents')!),storageKey);expect(archive[0].intent).toEqual(JSON.parse(original));expect(archive[0].not_created).toBe(true);
 await page.getByLabel('新的本人判断').selectOption('UNSURE');await page.getByRole('button',{name:'准备更正',exact:true}).click();
 expect(JSON.parse((await page.evaluate(k=>sessionStorage.getItem(k),storageKey))!).nonce).not.toBe(JSON.parse(original).nonce);
 await expect(page.getByRole('button',{name:'保存此次更正',exact:true})).toBeDisabled();await page.getByRole('button',{name:'本人确认此次更正',exact:true}).click();await page.getByRole('button',{name:'保存此次更正',exact:true}).click();
 await expect(page.locator('ol li')).toHaveCount(3);expect(state.writes.filter(v=>v==='execute')).toHaveLength(1);
 await page.screenshot({path:info.outputPath('single-cancel-new-confirmation.png'),fullPage:true});
});

test('R4 no-task arbitrary 404, dependency, network and mismatched proofs never clear pending',async({page})=>{
 const state=await fixture(page);const original=await page.evaluate(k=>sessionStorage.getItem(k),storageKey);
 for(const failure of ['404','503','network','nonce','root','choice','action','expected','current']){
  state.fail(failure);await cancel(page).click();await expect(cancel(page)).toBeEnabled();expect(await page.evaluate(k=>sessionStorage.getItem(k),storageKey)).toBe(original);
 }
 expect(await page.evaluate(k=>sessionStorage.getItem(k+'.ended-intents'),storageKey)).toBeNull();
 await page.reload();await expect(cancel(page)).toBeEnabled();expect(state.writes.filter(v=>v==='prepare')).toHaveLength(1);
});

test('R4 full preparation lost response chooses original abandon without restoring confirmation or automatic writes',async({page})=>{
 const state=await fixture(page,'lost');await page.reload();await expect(cancel(page)).toBeEnabled();expect(state.writes).toEqual(['prepare']);
 await expect(page.getByRole('button',{name:/本人确认此次/})).toHaveCount(0);
 await cancel(page).click();await expect(page.getByText(ended)).toBeVisible();expect(state.writes).toEqual(['prepare','partial-close','abandon']);
 expect(await page.evaluate(k=>sessionStorage.getItem(k),storageKey)).toBeNull();
});

for(const mode of ['stale','partial'] as const){
 test(`R4 late unified cancellation after root switch preserves old selector (${mode})`,async({page})=>{
  const state=await fixture(page,mode);const original=await page.evaluate(k=>sessionStorage.getItem(k),storageKey);
  let release!:()=>void,notify!:()=>void;const gate=new Promise<void>(r=>release=r),arrived=new Promise<void>(r=>notify=r);state.hold(gate,notify);
  await cancel(page).click();await arrived;await page.getByRole('button',{name:'切换核对原反馈'}).click();release();await expect(cancel(page)).toHaveCount(0);
  expect(await page.evaluate(k=>sessionStorage.getItem(k),storageKey)).toBe(original);expect(await page.evaluate(k=>sessionStorage.getItem(k+'.ended-intents'),storageKey)).toBeNull();
  await page.getByRole('button',{name:'切换核对原反馈'}).click();await expect(cancel(page)).toBeEnabled();await expect(page.getByRole('button',{name:/本人确认此次/})).toHaveCount(0);
 });
}

test('R4 partial close response lost then reload replays only exact cancellation, fresh choice still needs confirmation',async({page},info)=>{
 const state=await fixture(page,'partial');const original=(await page.evaluate(k=>sessionStorage.getItem(k),storageKey))!;
 state.loseClose();await cancel(page).click();await expect(cancel(page)).toBeEnabled();expect(await page.evaluate(k=>sessionStorage.getItem(k),storageKey)).toBe(original);
 const before=[...state.writes];await page.reload();await expect(cancel(page)).toBeEnabled();expect(state.writes).toEqual(before);
 await cancel(page).click();await expect(page.getByText(ended)).toBeVisible();const archive=await page.evaluate(k=>JSON.parse(sessionStorage.getItem(k+'.ended-intents')!),storageKey);
 expect(archive[0].intent).toEqual(JSON.parse(original));expect(archive[0].partial_preparation_closed).toBe(true);
 await page.getByLabel('新的本人判断').selectOption('UNSURE');await page.getByRole('button',{name:'准备更正',exact:true}).click();await expect(page.getByRole('button',{name:'保存此次更正',exact:true})).toBeDisabled();
 expect(JSON.parse((await page.evaluate(k=>sessionStorage.getItem(k),storageKey))!).nonce).not.toBe(JSON.parse(original).nonce);
 await page.getByRole('button',{name:'本人确认此次更正',exact:true}).click();await page.getByRole('button',{name:'保存此次更正',exact:true}).click();await expect(page.locator('ol li')).toHaveCount(3);
 await page.screenshot({path:info.outputPath('unified-partial-cancellation.png'),fullPage:true});
});

test('R4 partial cancellation errors and mismatches or fresh-read errors never release pending',async({page})=>{
 const state=await fixture(page,'partial');const original=await page.evaluate(k=>sessionStorage.getItem(k),storageKey);
 for(const failure of ['404','503','409','network','nonce','choice','read-network','read-choice']){
  state.fail(failure);await cancel(page).click();await expect(cancel(page)).toBeEnabled();expect(await page.evaluate(k=>sessionStorage.getItem(k),storageKey)).toBe(original);
 }
 await page.reload();await expect(cancel(page)).toBeEnabled();expect(state.writes.filter(v=>v==='prepare')).toHaveLength(1);
});

test('R4 saved original is displayed and never sent to cancellation protocols',async({page})=>{
 const state=await fixture(page,'lost');const original=(await page.evaluate(k=>sessionStorage.getItem(k),storageKey))!;
 await page.getByRole('button',{name:'重新确认原选择'}).click();await page.getByRole('button',{name:'本人确认此次更正'}).click();await page.getByRole('button',{name:'保存此次更正'}).click();await expect(page.locator('ol li')).toHaveCount(2);
 await page.evaluate(({key,value})=>sessionStorage.setItem(key,value),{key:storageKey,value:original});await page.reload();await expect(cancel(page)).toBeEnabled();
 const before=[...state.writes];await cancel(page).click();await expect(page.getByText(/原修改已保存.*未取消已保存意见/)).toBeVisible();expect(state.writes).toEqual(before);expect(await page.evaluate(k=>sessionStorage.getItem(k),storageKey)).toBeNull();
});
