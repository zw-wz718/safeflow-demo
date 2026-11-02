import {expect,test,type Page,type Route} from '@playwright/test';
import base from '../src/runtime/bank-recovery.fixture.json' with {type:'json'};
import compat from '../src/runtime/agent-backend-compat.fixture.json' with {type:'json'};
import {guardReplayApi,assertReplayIsolated} from './isolated-api';
const context='a'.repeat(64), other='b'.repeat(64);
const legacy='safeflow.bank.chat.v1.'+context;
const marker='safeflow.bank.chat-display-generation.v2.'+context;
const originals='safeflow.bank.chat-originals.v1.'+context;
const reply=(r:Route,v:unknown,status=200)=>r.fulfill({status,contentType:'application/json',body:JSON.stringify(v)});
const clarify=(question:string)=>({...compat.responses[0],clarification:{question}});
const original=(id:string)=>({...base.draft,task_id:id,operation_id:'operation_'+id,task_state:'RUNNING',task_revision:1,operation_state:'DRAFT',operation_revision:1,execution_status:'RESULT_UNKNOWN',receipt:null,receipt_scope:null,unknown_operation_id:'operation_'+id,next_action:{kind:'QUERY_ORIGINAL_OPERATION',operation_id:'operation_'+id}});
async function setup(page:Page,tasks:string[]=[]) {
 await guardReplayApi(page);const calls:string[]=[];
 await page.route('**/api/bank/expire-draft-intents',r=>reply(r,{contract_version:1,expired_draft_intents:[],payment_cancelled:false,server_time:true}));
 await page.route('**/api/agent/status',r=>reply(r,{contract_version:1,mode:'PROJECT_SIMULATION',configured:true,connectivity:'NOT_VERIFIED',tools:[],draft_enabled:true,planning_mode:'OPENAI_COMPATIBLE'}));
 await page.route('**/api/bank/overview',r=>reply(r,{accounts:[]}));
 await page.route('**/api/bank/recovery',r=>reply(r,{context_ref:context,payment_tasks:tasks,subscription_nonces:[]}));
 await page.route('**/api/sim/payment-preparations/*',r=>reply(r,[]));
 await page.route('**/api/sim/draft-lifecycles/*',r=>reply(r,original(new URL(r.request().url()).pathname.split('/').at(-1)!)));
 await page.route('**/api/agent/tasks/*',r=>{calls.push(r.request().url());return reply(r,{},404);});
 return calls;
}
async function open(page:Page,suffix=''){await page.goto('/e2e/site-repairs.html?view=chat'+suffix);await expect(page.getByRole('button',{name:'清空聊天',exact:true})).toBeEnabled();await expect(page.getByLabel('向银行助手发送消息')).toBeEnabled();}
async function send(page:Page,text:string){await page.getByLabel('向银行助手发送消息').fill(text);await page.getByRole('button',{name:'发送消息',exact:true}).click();}
async function history(page:Page,id:string){await page.getByText(/原付款核查 ·/).click();await page.getByText('全部办理原号（包括未核实请求）').click();await page.getByRole('button',{name:'查询 '+id,exact:true}).click();}
async function clean(page:Page){await page.getByRole('button',{name:'清空聊天',exact:true}).click();await expect(page.getByText('今天想办理什么？')).toBeVisible();}
test.afterEach(async({context:browser})=>{for(const page of browser.pages())assertReplayIsolated(page);});

test('migrates old text/context/selection only, keeps business stores and original IDs, never auto-selects a draft',async({page},info)=>{
 const calls=await setup(page,['old_draft']);
 await page.addInitScript(({legacy,other})=>{if(sessionStorage.getItem('seeded'))return;sessionStorage.setItem('seeded','1');sessionStorage.setItem(legacy,JSON.stringify({messages:[{role:'assistant',text:'OLD_PRIVATE_CHAT'}],tasks:['old_draft'],active:'old_draft',conversation:crypto.randomUUID(),revision:2,pending:false,payeeSelection:{query:'old',id:'payee_old'}}));sessionStorage.setItem(legacy+'.read.v1','OLD_READ_CONTEXT');sessionStorage.setItem('safeflow.bank.chat.v1.'+other,'OTHER_CONTEXT');localStorage.setItem('safeflow.bank.selectors.v1.'+'a'.repeat(64),'BUSINESS_SELECTOR');const request=indexedDB.open('safeflow.billing-feedback.recovery.v2',1);request.onupgradeneeded=()=>request.result.createObjectStore('originals');request.onsuccess=()=>{request.result.transaction('originals','readwrite').objectStore('originals').put('BUSINESS_FEEDBACK','original');};},{legacy,other});
 await open(page);await expect(page.getByText('OLD_PRIVATE_CHAT')).toHaveCount(0);expect(calls).toEqual([]);await expect(page.getByRole('region',{name:'转账草案'})).toHaveCount(0);
 const stored=await page.evaluate(({legacy,originals,other})=>({old:sessionStorage.getItem(legacy),read:sessionStorage.getItem(legacy+'.read.v1'),other:sessionStorage.getItem('safeflow.bank.chat.v1.'+other),originals:JSON.parse(sessionStorage.getItem(originals)!),selector:localStorage.getItem('safeflow.bank.selectors.v1.'+'a'.repeat(64))}),{legacy,originals,other});
 expect(stored).toEqual({old:null,read:null,other:'OTHER_CONTEXT',originals:{tasks:['old_draft'],blockers:['old_draft']},selector:'BUSINESS_SELECTOR'});
 await clean(page);expect(await page.evaluate(()=>new Promise(resolve=>{const r=indexedDB.open('safeflow.billing-feedback.recovery.v2');r.onsuccess=()=>{const q=r.result.transaction('originals').objectStore('originals').get('original');q.onsuccess=()=>resolve(q.result);};}))).toBe('BUSINESS_FEEDBACK');
 await expect(page.getByRole('button',{name:'新任务',exact:true})).toBeDisabled();await expect(page.getByRole('button',{name:'继续业务办理',exact:true})).toBeDisabled();
 await expect(page.getByRole('region',{name:'转账草案'})).toHaveCount(0);await page.screenshot({path:info.outputPath('clean-page.png'),fullPage:true});expect(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth)).toBe(true);
});

test('new messages survive refresh/new tabs; explicit clear synchronizes both tabs and rotates model UUID',async({page,context:browser})=>{
 await setup(page);const ids:string[]=[];const model=async(r:Route)=>{const b=r.request().postDataJSON();ids.push(b.conversation_id);return reply(r,{conversation_id:b.conversation_id,revision:b.revision+1,task_id:null,view:clarify('FRESH_ANSWER'),uncertain:false,payee_query:null});};
 await page.route('**/api/bank/conversation',model);await open(page);await send(page,'fresh message');await expect(page.getByText('FRESH_ANSWER',{exact:true})).toBeVisible();
 const generation=await page.evaluate(key=>localStorage.getItem(key),marker);const second=await browser.newPage();await setup(second);await open(second);expect(await second.evaluate(key=>localStorage.getItem(key),marker)).toBe(generation);await expect(page.getByText('FRESH_ANSWER',{exact:true})).toBeVisible();
 await page.reload();await expect(page.getByText('FRESH_ANSWER',{exact:true})).toBeVisible();await clean(second);await expect(page.getByText('FRESH_ANSWER',{exact:true})).toHaveCount(0);await expect(page.getByText('今天想办理什么？')).toBeVisible();
 await send(page,'brand new message');await expect(page.getByText('FRESH_ANSWER',{exact:true})).toBeVisible();expect(new Set(ids).size).toBe(2);
 await page.reload();await expect(page.getByText('brand new message',{exact:true})).toBeVisible();
});

for(const channel of ['model','read','history','panel'] as const)test('clear prevents late '+channel+' response from resurrecting old content',async({page})=>{
 await setup(page,channel==='history'?['old_draft']:[]);let release!:()=>void;const gate=new Promise<void>(r=>release=r);let reached!:()=>void;const arrived=new Promise<void>(r=>reached=r);
 const endpoint={model:'**/api/bank/conversation',read:'**/api/bank/read-conversation',history:'**/api/agent/tasks/*',panel:'**/api/e2e/service'}[channel];
 await page.route(endpoint,async r=>{reached();await gate;const b=channel==='model'||channel==='read'?r.request().postDataJSON():{};const value=channel==='model'?{conversation_id:b.conversation_id,revision:1,task_id:null,view:clarify('LATE_OLD_RESPONSE'),uncertain:false,payee_query:null}:channel==='read'?{conversation_id:b.conversation_id,revision:1,view:clarify('LATE_OLD_RESPONSE'),uncertain:false}:{contract_version:1,mode:'PROJECT_SIMULATION',kind:'DRAFT',task:null,draft:original('old_draft'),results:null,clarification:null,can_start:false};await (channel==='panel'?r.fulfill({body:'LATE_OLD_PANEL'}):reply(r,value)).catch(()=>{});});
 await open(page,channel==='panel'?'&slowpanel=1':'');
 if(channel==='read'){await page.getByRole('button',{name:'独立查询',exact:true}).click();await send(page,'old independent question');}
 else if(channel==='history')await history(page,'old_draft');
 else if(channel==='panel')await page.getByRole('button',{name:'银行卡',exact:true}).click();
 else await send(page,'old model message');
 await arrived;await clean(page);release();await page.evaluate(()=>new Promise<void>(r=>requestAnimationFrame(()=>requestAnimationFrame(()=>r()))));
 await expect(page.getByText('LATE_OLD_RESPONSE',{exact:false})).toHaveCount(0);await expect(page.getByText('LATE_OLD_PANEL',{exact:false})).toHaveCount(0);await expect(page.getByRole('region',{name:'转账草案'})).toHaveCount(0);
 await page.reload();await expect(page.getByText('今天想办理什么？')).toBeVisible();await expect(page.getByText('LATE_OLD_RESPONSE',{exact:false})).toHaveCount(0);
});

test('local uncertain missing original stays blocked; only fresh typed READ releases a local-only blocker',async({page})=>{
 await setup(page);await page.addInitScript(({originals})=>{if(!sessionStorage.getItem('seed')){sessionStorage.setItem('seed','1');sessionStorage.setItem(originals,JSON.stringify({tasks:['pending_read'],blockers:['pending_read']}));}},{originals});await open(page);await expect(page.getByRole('button',{name:'继续业务办理',exact:true})).toBeDisabled();
 const read=structuredClone(compat.responses.find(v=>v.kind==='READ')!);read.task!.task_id='pending_read';
 await page.route('**/api/agent/tasks/pending_read',r=>reply(r,read));await page.reload();await expect(page.getByRole('button',{name:'继续业务办理',exact:true})).toBeEnabled();expect(await page.evaluate(key=>JSON.parse(sessionStorage.getItem(key)!).blockers,originals)).toEqual([]);
});

test('failed fresh payment reads remain blocked after clear; no business write requested',async({page})=>{
 await setup(page,['old_draft']);await page.route('**/api/sim/payment-preparations/*',r=>reply(r,{},503));await open(page);await expect(page.getByRole('button',{name:'新任务',exact:true})).toBeDisabled();await clean(page);await expect(page.getByRole('button',{name:'继续业务办理',exact:true})).toBeDisabled();
});

for(const state of ['PROCESSING','RECONCILE_REQUIRED'] as const)test('real '+state+' funds stay protected across chat clear and reload',async({page})=>{
 const id=base.draft.task_id;await setup(page,[id]);const methods:string[]=[];page.on('request',r=>{if(r.url().includes('/api/'))methods.push(r.method()+' '+new URL(r.url()).pathname);});
 await page.route('**/api/sim/draft-lifecycles/*',r=>reply(r,base.draft));await page.route('**/api/sim/payment-preparations/*',r=>reply(r,[base.payment]));
 await page.route('**/funds-result',r=>reply(r,{...base.funds,state,submitted:true}));
 await open(page);await expect(page.getByRole('button',{name:'新任务',exact:true})).toBeDisabled();await clean(page);await page.reload();await expect(page.getByText('今天想办理什么？')).toBeVisible();await expect(page.getByRole('button',{name:'继续业务办理',exact:true})).toBeDisabled();
 expect(methods.filter(v=>!v.startsWith('GET ')&&v!=='POST /api/bank/expire-draft-intents')).toEqual([]);expect(await page.evaluate(key=>JSON.parse(sessionStorage.getItem(key)!).blockers,originals)).toEqual([id]);
});

test('strict terminated draft appears only on explicit original query and offers no stale confirm/payment actions',async({page})=>{
 const d=structuredClone(base.draft),id=d.task_id;await setup(page,[id]);const note={schema:'UNEXECUTED_DRAFT_TERMINATED_V1',task_id:id,operation_id:d.operation_id,reason:'ABANDONED',proof:{scope:'DRAFT_CREATED_NO_PAYMENT_ACCEPTANCE',task_digest:'a'.repeat(64),operations_digest:'b'.repeat(64),parameters_digest:'c'.repeat(64),receipt_digest:'d'.repeat(64),expires_at:'2026-10-31T01:00:00Z'},original_preserved:true,payment_cancelled:false,funding_authorized:false,replacement:null};
 const ended={...d,active_operation_id:d.operation_id,next_action:{kind:'FINISHED'},intent_termination:note};
 await page.route('**/api/bank/recovery',r=>reply(r,{context_ref:context,payment_tasks:[id],subscription_nonces:[],ended_draft_intents:[note]}));await page.route('**/api/sim/draft-lifecycles/*',r=>reply(r,ended));await page.route('**/api/agent/tasks/*',r=>reply(r,{contract_version:1,mode:'PROJECT_SIMULATION',kind:'DRAFT',task:null,draft:ended,results:null,clarification:null,can_start:false}));
 await open(page);await expect(page.getByRole('button',{name:'继续业务办理',exact:true})).toBeEnabled();await expect(page.getByRole('region',{name:'转账草案'})).toHaveCount(0);await history(page,id);await expect(page.getByRole('heading',{name:'草案已结束',exact:true})).toBeVisible();await expect(page.getByRole('region',{name:'转账草案'}).getByRole('button')).toHaveCount(0);await expect(page.getByRole('region',{name:'聊天付款办理'})).toHaveCount(0);await expect(page.getByRole('button',{name:'新任务',exact:true})).toBeEnabled();await page.reload();await expect(page.getByRole('region',{name:'转账草案'})).toHaveCount(0);
});
