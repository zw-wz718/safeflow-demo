import {expect,test,type Page,type Route} from '@playwright/test';
import base from '../src/runtime/bank-recovery.fixture.json' with {type:'json'};
import {guardReplayApi,assertReplayIsolated} from './isolated-api';
test.afterEach(({page})=>assertReplayIsolated(page));
const reply=(r:Route,v:unknown,status=200)=>r.fulfill({status,contentType:'application/json',body:JSON.stringify(v),headers:{'Cache-Control':'no-store'}});
const draft=(id:string)=>({...base.draft,task_id:id,operation_id:'operation_'+id,task_state:'RUNNING',task_revision:1,operation_state:'DRAFT',operation_revision:1,execution_status:'RESULT_UNKNOWN',receipt:null,receipt_scope:null,unknown_operation_id:'operation_'+id,next_action:{kind:'QUERY_ORIGINAL_OPERATION',operation_id:'operation_'+id}});
async function chat(page:Page){
 await guardReplayApi(page);const closed=new Set<string>();const writes:string[]=[];const tasks=['old_claim_a','old_claim_b'];let fail=false;
 await page.route('**/api/bank/expire-draft-intents',r=>reply(r,{contract_version:1,expired_draft_intents:[],payment_cancelled:false,server_time:true}));
 await page.route('**/api/agent/status',r=>reply(r,{contract_version:1,mode:'PROJECT_SIMULATION',configured:true,connectivity:'NOT_VERIFIED',tools:[],draft_enabled:true,planning_mode:'OPENAI_COMPATIBLE'}));
 await page.route('**/api/bank/overview',r=>reply(r,{accounts:[]}));
 await page.route('**/api/bank/recovery',r=>reply(r,{context_ref:'a'.repeat(64),payment_tasks:tasks,subscription_nonces:[],closed_nonfunding_tasks:[...closed]}));
 await page.route('**/api/sim/payment-preparations/*',r=>fail?reply(r,{},503):reply(r,[]));
 await page.route('**/api/sim/draft-lifecycles/*',r=>reply(r,draft(new URL(r.request().url()).pathname.split('/').at(-1)!)));
 await page.route('**/api/agent/tasks/*',r=>{const id=new URL(r.request().url()).pathname.split('/').at(-1)!;return reply(r,{contract_version:1,mode:'PROJECT_SIMULATION',kind:'DRAFT',task:null,draft:draft(id),results:null,clarification:null,can_start:false});});
 await page.route('**/close-nonfunding',r=>{const b=r.request().postDataJSON();expect(b.acknowledgement).toBe('PRESERVE_UNKNOWN_CLOSE_NONFUNDING_DRAFT');const id=new URL(r.request().url()).pathname.split('/')[4];closed.add(id);writes.push(id);return reply(r,{contract_version:1,task_id:id,operation_id:'operation_'+id,closed_nonfunding_draft:true,original_result:'UNKNOWN',original_preserved:true,funding_authorized:false,automatic_retry:false});});
 await page.goto('/e2e/site-repairs.html?view=chat');
 return {closed,writes,failReads:()=>{fail=true;}};
}
test('R1 explicitly closes each original, retains UNKNOWN after reload, and needs a separate new-task action',async({page},info)=>{
 const state=await chat(page);const next=page.getByRole('button',{name:'新任务',exact:true});
 const close=page.getByRole('button',{name:'保留未知原件，关闭这份非资金草案',exact:true});
 await expect(close).toHaveCount(0);await expect(next).toBeDisabled();
 await page.getByText(/原付款核查 ·/).click();await page.getByText('全部办理原号（包括未核实请求）').click();await page.getByRole('button',{name:'查询 old_claim_b',exact:true}).click();
 await expect(close).toBeEnabled();await close.click();
 await expect(page.getByText('已明确关闭这份非资金草案；未知原件仍保留。',{exact:false})).toBeVisible();await expect(next).toBeDisabled();
 await page.getByRole('button',{name:'查询 old_claim_a',exact:true}).click();
 await expect(close).toBeEnabled();await close.click();await expect(next).toBeEnabled();expect(state.writes).toEqual(['old_claim_b','old_claim_a']);
 await page.reload();await expect(next).toBeEnabled();await expect(page.getByRole('region',{name:'转账草案'})).toHaveCount(0);expect(state.writes).toHaveLength(2);
 await page.screenshot({path:info.outputPath('R1-original-unknown-preserved.png'),fullPage:true});
 await next.click();await expect(page.getByText('已开始新任务；历史原单仍可在记录中查询。')).toBeVisible();expect(state.writes).toHaveLength(2);
});
test('R1 proof cannot release a task if fresh payment reads fail',async({page})=>{
 const state=await chat(page);state.closed.add('old_claim_a');state.closed.add('old_claim_b');state.failReads();await page.reload();
 await expect(page.getByRole('button',{name:'新任务',exact:true})).toBeDisabled();expect(state.writes).toHaveLength(0);
});
type Action='CORRECT'|'REVOKE'|'RESUBMIT';
async function feedback(page:Page){
 await guardReplayApi(page);let version=0;let choice:string|null='NOT_MINE';let lose=false;let executes=0;
 const history=[{revision:0,operation_id:'root_a',action:'INITIAL',active:true,choice:'NOT_MINE'}] as Array<{revision:number,operation_id:string,action:string,active:boolean,choice:string|null}>;
 const preps=new Map<string,any>();const saved=new Map<string,any>();
 const head=(root:string)=>({history_contract:'BILLING_FEEDBACK_HISTORY_V1',root_operation_id:root,revision:root==='root_a'?version:0,active:root==='root_a'?choice!==null:true,choice:root==='root_a'?choice:'MINE',history:root==='root_a'?history:[{revision:0,operation_id:root,action:'INITIAL',active:true,choice:'MINE'}],card_head_adopted:false,member_receipt_changed:false,lock_authorized:false});
 await page.route('**/api/billing-feedback/roots/*/head',r=>reply(r,head(new URL(r.request().url()).pathname.split('/')[4])));
 await page.route('**/api/billing-feedback/revisions/prepare',r=>{const i=r.request().postDataJSON();const p={contract_version:1,scope:'BILLING_FEEDBACK_LOCAL_ONLY',task_id:'task_'+i.nonce,operation_id:'op_'+i.nonce,parameters:{input_version:2,report_id:'report',original_digest:'b'.repeat(64),transaction:{source_id:'source',account_id:'account_001',transaction_id:'tx'},choice:i.choice??(choice??'NOT_MINE'),revision:{root_operation_id:i.root_operation_id,expected_revision:i.expected_revision,action:i.action,nonce:i.nonce}},confirmation:{confirmation_ref:'confirmation_'+i.nonce,status:'PREPARED',expires_at:new Date(Date.now()+300000).toISOString()},lock_authorized:false,automatic_retry:false};preps.set(i.nonce,p);return reply(r,p);});
 await page.route('**/api/billing-feedback/tasks/*/operations/*/confirm',r=>{const p=[...preps.values()].find(p=>r.request().url().includes(p.operation_id));return reply(r,{contract_version:1,scope:p.scope,task_id:p.task_id,operation_id:p.operation_id,confirmation_ref:p.confirmation.confirmation_ref,status:'CONFIRMED',lock_authorized:false,automatic_retry:false});});
 await page.route('**/api/billing-feedback/tasks/*/operations/*/execute',r=>{const p=[...preps.values()].find(p=>r.request().url().includes(p.operation_id));executes++;const a=p.parameters.revision.action as Action;version++;choice=a==='REVOKE'?null:p.parameters.choice;const s={history_contract:'BILLING_FEEDBACK_HISTORY_V1',root_operation_id:'root_a',revision:version,previous_revision:p.parameters.revision.expected_revision,action:a,active:choice!==null,choice,parameters:p.parameters,card_head_adopted:false,member_receipt_changed:false};saved.set(p.parameters.revision.nonce,s);history.push({revision:version,operation_id:p.operation_id,action:a,active:choice!==null,choice});if(lose){lose=false;return r.abort();}return reply(r,{contract_version:1,scope:p.scope,task_id:p.task_id,operation_id:p.operation_id,task_state:'COMPLETED',operation_state:'SUCCEEDED',saved:s,replayed:false,domain_audit:'PUBLIC_HISTORY_ONLY_V1',lock_authorized:false,automatic_retry:false});});
 await page.route('**/api/billing-feedback/revisions/preparations/*',r=>{const nonce=new URL(r.request().url()).pathname.split('/').at(-1)!;const p=preps.get(nonce);return reply(r,{history_contract:'BILLING_FEEDBACK_HISTORY_V1',task_id:p.task_id,operation_id:p.operation_id,parameters:p.parameters,task_state:saved.has(nonce)?'COMPLETED':'RUNNING',saved:saved.get(nonce)??null,confirmation_restored:false,automatic_retry:false});});
 await page.goto('/e2e/site-repairs.html?view=feedback');return {lost:()=>{lose=true;},count:()=>executes};
}
test('R4 correct, revoke and resubmit each need a separate confirmation; reload never resubmits',async({page},info)=>{
 const state=await feedback(page);
 await page.getByLabel('新的本人判断').selectOption('MINE');await page.getByRole('button',{name:'准备更正',exact:true}).click();
 await expect(page.getByRole('button',{name:'保存此次更正',exact:true})).toBeDisabled();await page.reload();
 await expect(page.getByRole('button',{name:'本人确认此次更正',exact:true})).toHaveCount(0);await page.getByRole('button',{name:'重新确认原选择'}).click();
 await page.getByRole('button',{name:'本人确认此次更正'}).click();await page.getByRole('button',{name:'保存此次更正'}).click();
 await expect(page.getByText('版本 1 · 是本人交易',{exact:false}).first()).toBeVisible();
 await page.getByRole('button',{name:'准备撤销反馈'}).click();await expect(page.getByRole('button',{name:'保存此次撤销'})).toBeDisabled();
 await page.getByRole('button',{name:'本人确认此次撤销'}).click();await page.getByRole('button',{name:'保存此次撤销'}).click();
 await expect(page.getByText('版本 2 · 已撤销，当前无有效意见')).toBeVisible();await page.getByLabel('新的本人判断').selectOption('UNSURE');
 await page.getByRole('button',{name:'准备重新提交'}).click();await expect(page.getByRole('button',{name:'保存此次重新提交'})).toBeDisabled();
 await page.getByRole('button',{name:'本人确认此次重新提交'}).click();await page.getByRole('button',{name:'保存此次重新提交'}).click();
 await expect(page.locator('ol li')).toHaveCount(4);expect(state.count()).toBe(3);await page.reload();await expect(page.locator('ol li')).toHaveCount(4);expect(state.count()).toBe(3);
 await page.screenshot({path:info.outputPath('R4-complete-opinion-history.png'),fullPage:true});
});
test('R4 lost execute response reads original saved version without repeating execute',async({page})=>{
 const state=await feedback(page);await page.getByLabel('新的本人判断').selectOption('MINE');await page.getByRole('button',{name:'准备更正',exact:true}).click();
 await page.getByRole('button',{name:'本人确认此次更正'}).click();state.lost();await page.getByRole('button',{name:'保存此次更正'}).click();
 await expect(page.getByRole('button',{name:'查看办理进度'})).toBeEnabled();await page.getByRole('button',{name:'查看办理进度'}).click();
 await expect(page.getByText('原修改已保存：版本 1 · 更正。')).toBeVisible();expect(state.count()).toBe(1);
});
test('R4 late old-root response cannot attach a new root or restore confirmation',async({page})=>{
 await feedback(page);let release!:()=>void;const gate=new Promise<void>(r=>release=r);let reached!:()=>void;const arrived=new Promise<void>(r=>reached=r);
 await page.route('**/api/billing-feedback/revisions/prepare',async r=>{reached();await gate;await reply(r,{}).catch(()=>{});});
 await page.getByLabel('新的本人判断').selectOption('MINE');await page.getByRole('button',{name:'准备更正',exact:true}).click();await arrived;
 await page.getByRole('button',{name:'切换核对原反馈'}).click();release();await expect(page.getByText('版本 0 · 是本人交易',{exact:false}).first()).toBeVisible();
 await expect(page.getByRole('button',{name:/本人确认此次/})).toHaveCount(0);await expect(page.getByRole('button',{name:'查看办理进度'})).toHaveCount(0);
});
test('R2 invalidated original needs fresh matching proof to leave; reload retains its history and no write is replayed',async({page})=>{
 await guardReplayApi(page);const nonce='51d28271-684c-4790-822b-26567d5a82a0';const key='safeflow.bank.selectors.v1.'+'a'.repeat(64);let queries=0;const writes:string[]=[];
 await page.addInitScript(({key,nonce})=>{if(!localStorage.getItem(key))localStorage.setItem(key,JSON.stringify({subscriptionNonce:nonce,subscriptionOperation:'old_sub',subscriptionHistory:[{nonce}]}));},{key,nonce});
 await page.route('**/api/sim/session',r=>reply(r,{mode:'PROJECT_SIMULATION',authenticated:true}));
 await page.route('**/api/card-read/session',r=>reply(r,{authenticated:true,context_ref:'owner_session'}));
 await page.route('**/api/bank/recovery',r=>reply(r,{context_ref:'a'.repeat(64),payment_tasks:[],subscription_nonces:[nonce],closed_nonfunding_tasks:[]}));
 await page.route('**/api/bank/overview',r=>reply(r,{accounts:[]}));
 await page.route('**/api/subscriptions',r=>reply(r,{items:[]}));
 await page.route('**/api/subscriptions/change',r=>{const b=r.request().postDataJSON();writes.push(b.kind);if(b.kind==='QUERY'){queries++;return reply(r,{original:{status:'NEEDS_REVIEW',message:'失效准备原记录'},domain_audit:{status:'UNEXECUTED_OR_NEEDS_REVIEW'},preparation_recovery:{version:1,operation_id:queries===2?'wrong_original':'old_sub',may_start_new:true,state:'NON_EXECUTABLE_PREPARATION',reason:'RESTART_INVALIDATED',original_preserved:true,original_writer_generation:6,current_writer_generation:9}});}return reply(r,{});});
 await page.goto('/e2e/site-repairs.html?view=bank#subscriptions');
 await page.getByRole('button',{name:'查询办理结果',exact:true}).click();
 const leave=page.getByRole('button',{name:'保留失效原记录，重新选择',exact:true});await expect(leave).toBeEnabled();await leave.click();
 await expect(leave).toBeEnabled();expect((await page.evaluate(key=>JSON.parse(localStorage.getItem(key)!),key)).subscriptionOperation).toBe('old_sub');
 await leave.click();await expect(leave).toHaveCount(0);
 const stored=await page.evaluate(key=>JSON.parse(localStorage.getItem(key)!),key);expect(stored.subscriptionNonce).toBeUndefined();expect(stored.subscriptionHistory).toEqual([{nonce}]);
 await page.reload();await expect(page.getByRole('button',{name:'查询办理结果',exact:true})).toBeDisabled();expect(writes).toEqual(['QUERY','QUERY','QUERY']);
});
