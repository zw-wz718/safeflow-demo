import {test,expect,type Page} from '@playwright/test';
import evidence from './fixtures/r4-saved-http.json' with {type:'json'};
import {guardReplayApi,assertReplayIsolated} from './isolated-api';

// The saved receipt below is the unchanged independent owned-PG HTTP capture,
// not the author's former five-field approximation.
const key='safeflow.feedback-history.v1.'+evidence.intent.root_operation_id;
const start='/e2e/site-repairs.html?view=feedback&root='+encodeURIComponent(evidence.intent.root_operation_id);
const cancel=(page:Page)=>page.getByRole('button',{name:'核对并取消本次修改',exact:true});
test.afterEach(({page})=>assertReplayIsolated(page));

async function fixture(page:Page,unsaved=false){
 await guardReplayApi(page);
 let response:any=structuredClone(evidence.original),network=false,loseExecute=false;
 let hold:Promise<void>|undefined,arrived:(()=>void)|undefined;
 const calls:{method:string,path:string}[]=[];
 const prepared={contract_version:1,scope:'BILLING_FEEDBACK_LOCAL_ONLY',task_id:response.task_id,operation_id:response.operation_id,
   parameters:response.parameters,confirmation:{confirmation_ref:'isolated_explicit_confirmation',status:'PREPARED',expires_at:new Date(Date.now()+300000).toISOString()},lock_authorized:false,automatic_retry:false};
 await page.route('**/api/billing-feedback/**',async r=>{
  const method=r.request().method(),path=new URL(r.request().url()).pathname;calls.push({method,path});
  const send=(value:unknown)=>r.fulfill({status:200,contentType:'application/json',body:JSON.stringify(value)}).catch(()=>{});
  if(path.endsWith('/head')){
   if(path.includes('/root_b/'))return send({...evidence.head,root_operation_id:'root_b',revision:0,history:[{revision:0,operation_id:'root_b',action:'INITIAL',active:true,choice:'MINE'}]});
   if(unsaved)return send({...evidence.head,revision:2,choice:'NOT_MINE',history:evidence.head.history.slice(0,3)});
   return send(evidence.head);
  }
  if(method==='GET'){
   arrived?.();if(hold)await hold;if(network)return r.abort();
   return send(unsaved?{...response,task_state:'RUNNING',saved:null}:response);
  }
  // Only the explicit lost-execute scenario permits these writes. Cancellation
  // endpoints are never mocked as success; any unexpected POST fails the test.
  if(loseExecute&&path.endsWith('/prepare'))return send(prepared);
  if(loseExecute&&path.endsWith('/confirm'))return send({contract_version:1,scope:prepared.scope,task_id:prepared.task_id,operation_id:prepared.operation_id,
    confirmation_ref:prepared.confirmation.confirmation_ref,status:'CONFIRMED',lock_authorized:false,automatic_retry:false});
  if(loseExecute&&path.endsWith('/execute')){unsaved=false;return r.abort();}
  throw Error('unexpected write while reading a saved original '+path);
 });
 await page.goto(start);
 await page.evaluate(({key,intent})=>sessionStorage.setItem(key,JSON.stringify(intent)),{key,intent:evidence.intent});
 await page.reload();await expect(cancel(page)).toBeEnabled();
 return {calls,set:(v:any)=>{response=v;},network:(v:boolean)=>{network=v;},loseExecute:()=>{loseExecute=true;},hold:(gate:Promise<void>,notify:()=>void)=>{hold=gate;arrived=notify;}};
}
const writes=(state:Awaited<ReturnType<typeof fixture>>)=>state.calls.filter(c=>c.method!=='GET');

test('real HTTP ten-field saved original: unified cancellation displays result, clears only local wait, and reload never writes',async({page},info)=>{
 const state=await fixture(page);await cancel(page).click();
 await expect(page.getByRole('status')).toContainText('原修改已保存：版本 3 · 更正，未取消已保存意见。');
 expect(await page.evaluate(key=>sessionStorage.getItem(key),key)).toBeNull();
 expect(await page.evaluate(key=>sessionStorage.getItem(key+'.ended-intents'),key)).toBeNull();expect(writes(state)).toEqual([]);
 await expect(page.locator('ol li')).toHaveCount(4);await page.screenshot({path:info.outputPath('actual-saved-receipt-displayed.png'),fullPage:true});
 await page.reload();await expect(cancel(page)).toHaveCount(0);await expect(page.locator('ol li')).toHaveCount(4);expect(writes(state)).toEqual([]);
});

test('real HTTP saved original: existing progress read retains strict compatibility with no writes',async({page})=>{
 const state=await fixture(page);await page.getByRole('button',{name:'查看办理进度',exact:true}).click();
 await expect(page.getByRole('status')).toContainText('原修改已保存：版本 3 · 更正。');
 expect(await page.evaluate(key=>sessionStorage.getItem(key),key)).toBeNull();expect(writes(state)).toEqual([]);
});

test('real HTTP receipt rejects unknown fields, bad semantics, parameter mismatches and self-consistent foreign intent; pending stays',async({page})=>{
 const state=await fixture(page);const mutations:((v:any)=>void)[]=[
  v=>{v.saved.unexpected=true;},v=>{delete v.saved.history_contract;},v=>{v.saved.history_contract='OTHER';},
  v=>{v.saved.root_operation_id='root_b';},v=>{v.saved.previous_revision=1;},v=>{v.saved.revision=4;},
  v=>{v.saved.action='REVOKE';},v=>{v.saved.active=false;},v=>{v.saved.choice='UNSURE';},
  v=>{v.saved.card_head_adopted=true;},v=>{v.saved.member_receipt_changed=true;},v=>{v.task_state='RUNNING';},
  v=>{v.saved.parameters.report_id='other_report';},v=>{v.saved.parameters.original_digest='a'.repeat(64);},
  v=>{v.saved.parameters.transaction.transaction_id='other_transaction';},
  v=>{v.saved.parameters.revision.nonce='a3b96596-a712-46d2-86c5-3c9d810f3639';},
  v=>{v.saved.parameters.revision.expected_revision=1;},
  v=>{v.parameters.choice='NOT_MINE';v.saved.parameters.choice='NOT_MINE';v.saved.choice='NOT_MINE';},
  v=>{v.parameters.revision.action='REVOKE';v.saved.parameters.revision.action='REVOKE';v.saved.action='REVOKE';v.saved.active=false;v.saved.choice=null;},
  v=>{v.parameters.revision.expected_revision=3;v.saved.parameters.revision.expected_revision=3;v.saved.previous_revision=3;v.saved.revision=4;},
  v=>{v.parameters.revision.root_operation_id='root_b';v.saved.parameters.revision.root_operation_id='root_b';v.saved.root_operation_id='root_b';},
  v=>{v.parameters.revision.nonce='a3b96596-a712-46d2-86c5-3c9d810f3639';v.saved.parameters.revision.nonce=v.parameters.revision.nonce;},
 ];
 for(const mutate of mutations){
  const response=structuredClone(evidence.original);mutate(response);state.set(response);await cancel(page).click();await expect(cancel(page)).toBeEnabled();
  await expect(page.getByRole('status')).not.toContainText('原修改已保存');
  expect(await page.evaluate(key=>JSON.parse(sessionStorage.getItem(key)!),key)).toEqual(evidence.intent);expect(writes(state)).toEqual([]);
 }
 state.set(evidence.original);await page.getByRole('button',{name:'查看办理进度',exact:true}).click();await expect(page.getByRole('status')).toContainText('原修改已保存：版本 3');
});

test('real HTTP receipt network unknown and refresh keep pending without confirmation or automatic writes',async({page})=>{
 const state=await fixture(page);state.network(true);await cancel(page).click();await expect(cancel(page)).toBeEnabled();
 expect(await page.evaluate(key=>JSON.parse(sessionStorage.getItem(key)!),key)).toEqual(evidence.intent);expect(writes(state)).toEqual([]);
 await page.reload();await expect(cancel(page)).toBeEnabled();await expect(page.getByRole('button',{name:/本人确认此次/})).toHaveCount(0);expect(writes(state)).toEqual([]);
 state.network(false);await cancel(page).click();await expect(page.getByRole('status')).toContainText('未取消已保存意见');
});

test('real HTTP saved receipt arriving after root switch cannot clear or attach to another root',async({page})=>{
 const state=await fixture(page);let release!:()=>void,notify!:()=>void;const gate=new Promise<void>(r=>release=r),arrived=new Promise<void>(r=>notify=r);state.hold(gate,notify);
 await cancel(page).click();await arrived;await page.getByRole('button',{name:'切换核对原反馈'}).click();release();await expect(cancel(page)).toHaveCount(0);
 expect(await page.evaluate(key=>JSON.parse(sessionStorage.getItem(key)!),key)).toEqual(evidence.intent);expect(writes(state)).toEqual([]);
 await page.getByRole('button',{name:'切换核对原反馈'}).click();await expect(cancel(page)).toBeEnabled();await expect(page.getByRole('status')).toHaveCount(0);
});

for(const entry of ['核对并取消本次修改','查看办理进度']){
 test(`lost execute response recovers unchanged real HTTP saved original using ${entry}, no repeat execute or cancellation`,async({page})=>{
  const state=await fixture(page,true);state.loseExecute();
  await page.getByRole('button',{name:'重新确认原选择',exact:true}).click();
  await page.getByRole('button',{name:'本人确认此次更正',exact:true}).click();await page.getByRole('button',{name:'保存此次更正',exact:true}).click();await expect(cancel(page)).toBeEnabled();
  const before=[...writes(state)];expect(before.map(c=>c.path.split('/').at(-1))).toEqual(['prepare','confirm','execute']);
  await page.reload();await expect(cancel(page)).toBeEnabled();await expect(page.getByRole('button',{name:/本人确认此次/})).toHaveCount(0);
  await page.getByRole('button',{name:entry,exact:true}).click();await expect(page.getByRole('status')).toContainText('原修改已保存：版本 3');
  expect(await page.evaluate(key=>sessionStorage.getItem(key),key)).toBeNull();expect(writes(state)).toEqual(before);
 });
}
