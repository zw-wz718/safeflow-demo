import {test,expect,type Page,type Route} from '@playwright/test';
import base from '../src/runtime/bank-recovery.fixture.json' with {type:'json'};
import {guardReplayApi,assertReplayIsolated} from './isolated-api';
const context='a'.repeat(64),originals='safeflow.bank.chat-originals.v1.'+context,legacy='safeflow.bank.chat.v1.'+context;
const reply=(r:Route,v:unknown,status=200)=>r.fulfill({status,contentType:'application/json',body:JSON.stringify(v)});
async function setup(page:Page,tasks:()=>string[]){
 await guardReplayApi(page);
 await page.route('**/api/bank/expire-draft-intents',r=>reply(r,{contract_version:1,expired_draft_intents:[],payment_cancelled:false,server_time:true}));
 await page.route('**/api/bank/recovery',r=>reply(r,{context_ref:context,payment_tasks:tasks(),subscription_nonces:[]}));
 await page.route('**/api/agent/status',r=>reply(r,{contract_version:1,mode:'PROJECT_SIMULATION',configured:true,connectivity:'NOT_VERIFIED',tools:[],draft_enabled:true,planning_mode:'OPENAI_COMPATIBLE'}));
 await page.route('**/api/bank/overview',r=>reply(r,{accounts:[]}));
 await page.route('**/api/agent/tasks/*',r=>reply(r,{},404));
 await page.route('**/api/sim/draft-lifecycles/*',r=>reply(r,base.draft));
 await page.route('**/api/sim/payment-preparations/*',r=>reply(r,{},503));
}
async function open(page:Page){await page.goto('/e2e/site-repairs.html?view=chat');await expect(page.getByRole('button',{name:'清空聊天',exact:true})).toBeEnabled();}
async function blocked(page:Page){await expect(page.getByRole('button',{name:'继续业务办理',exact:true})).toBeDisabled();expect(await page.evaluate(key=>JSON.parse(sessionStorage.getItem(key)!).blockers,originals)).toContain(base.draft.task_id);}
test.afterEach(async({context:browser})=>{for(const page of browser.pages())assertReplayIsolated(page);});

for(const mode of ['clear-omit','cross-tab-omit','clear-directory-fail','refresh-omit'] as const)test('CCH01 persists an interrupted scan through '+mode+' and releases only matching terminal proof',async({page,context:browser},info)=>{
 const id=base.draft.task_id;let tasks=[id];await setup(page,()=>tasks);
 let reached!:()=>void;const arrived=new Promise<void>(r=>reached=r);let release!:()=>void;const gate=new Promise<void>(r=>release=r);
 await page.route('**/api/sim/payment-preparations/*',async r=>{reached();await gate;await reply(r,{},503).catch(()=>{});});
 await open(page);await arrived;await blocked(page);tasks=[];
 if(mode==='cross-tab-omit'){const second=await browser.newPage();await setup(second,()=>[]);await open(second);await second.getByRole('button',{name:'清空聊天',exact:true}).click();}
 else if(mode==='refresh-omit')await page.reload();
 else {if(mode==='clear-directory-fail')await page.route('**/api/bank/recovery',r=>reply(r,{},503));await page.getByRole('button',{name:'清空聊天',exact:true}).click();}
 if(mode==='clear-directory-fail'){
  await expect(page.getByRole('alert')).toBeVisible();expect(await page.evaluate(key=>JSON.parse(sessionStorage.getItem(key)!).blockers,originals)).toContain(id);
  await page.route('**/api/bank/recovery',r=>reply(r,{context_ref:context,payment_tasks:[],subscription_nonces:[]}));await page.reload();
 }
 await expect(page.getByText('今天想办理什么？')).toBeVisible();release();await expect(page.getByLabel('向银行助手发送消息')).toBeEnabled();await blocked(page);
 // A new generation cannot be relaxed by an old pending read or omission.
 await page.reload();await expect(page.getByLabel('向银行助手发送消息')).toBeEnabled();await blocked(page);
 // All preparations freshly empty + the SAME cancelled nonfunding original may release it.
 await page.route('**/api/sim/draft-lifecycles/*',r=>reply(r,{...base.draft,task_state:'CANCELLED'}));await page.route('**/api/sim/payment-preparations/*',r=>reply(r,[]));await page.reload();
 await expect(page.getByRole('button',{name:'继续业务办理',exact:true})).toBeEnabled();const after=await page.evaluate(key=>JSON.parse(sessionStorage.getItem(key)!),originals);expect(after.blockers).toEqual([]);expect(after.tasks).toContain(id);await info.attach('resolved-original',{body:JSON.stringify(after),contentType:'application/json'});
});

test('CCH01 records the first directory before even the initial overview finishes',async({page})=>{
 let tasks=[base.draft.task_id];await setup(page,()=>tasks);let release!:()=>void;const gate=new Promise<void>(r=>release=r);
 await page.route('**/api/bank/overview',async r=>{await gate;await reply(r,{accounts:[]}).catch(()=>{});});await open(page);
 expect(await page.evaluate(key=>JSON.parse(sessionStorage.getItem(key)!).blockers,originals)).toContain(base.draft.task_id);tasks=[];await page.getByRole('button',{name:'清空聊天',exact:true}).click();release();await expect(page.getByLabel('向银行助手发送消息')).toBeEnabled();await blocked(page);
});

test('CCH01 release of one proved original preserves a newly observed blocker and stale scopes cannot release it',async({page})=>{
 await setup(page,()=>[]);await open(page);const result=await page.evaluate(async context=>{
  const path='/src/runtime/chat-history.ts';const module=await import(/* @vite-ignore */ path);const history=await module.prepareChatHistory(context,['first']);
  history.retainBlockers(['second']);history.releaseBlockers(['first']);const before=history.originals();
  const next=await module.clearChatHistory(context);let rejected=false;try{history.releaseBlockers(['second']);}catch{rejected=true;}
  return {before,after:next.originals(),rejected};
 },context);expect(result.before.blockers).toEqual(['second']);expect(result.after.blockers).toEqual(['second']);expect(result.before.tasks).toEqual(['first','second']);expect(result.rejected).toBe(true);
});

test('CCH02 rejects invalid string/non-string active, malformed pending/tasks before deleting ANY source',async({page,context:browser})=>{
 const variants=[{active:'invalid/id',pending:true,tasks:[]},{active:42,pending:true,tasks:[]},{active:null,pending:true,tasks:[]},{active:{id:'original'},pending:true,tasks:[]},{pending:true,tasks:[]},{active:'original',pending:'yes',tasks:[]},{active:'original',pending:false,tasks:['bad/id']}];
 for(const [index,bad] of variants.entries()){
  const target=index===0?page:await browser.newPage();await setup(target,()=>[]);
  await target.addInitScript(({context,legacy,bad})=>{
   const validKey='safeflow.bank.chat.v2.'+context+'.00000000-0000-4000-8000-000000000000';sessionStorage.setItem(validKey,JSON.stringify({tasks:['keep_original'],active:'',pending:false,messages:[]}));sessionStorage.setItem(legacy,JSON.stringify({...bad,messages:[]}));
  },{context,legacy,bad});
  await target.goto('/e2e/site-repairs.html?view=chat');await expect(target.getByRole('alert')).toBeVisible();
  expect(await target.evaluate(key=>JSON.parse(sessionStorage.getItem(key)!),legacy)).toEqual({...bad,messages:[]});expect(await target.evaluate(context=>sessionStorage.getItem('safeflow.bank.chat.v2.'+context+'.00000000-0000-4000-8000-000000000000'),context)).not.toBeNull();
 }
});

for(const active of ['', 'valid_original'])test('CCH02 permits legitimate selection '+(active||'empty')+' without turning history into permanent money blockers',async({page})=>{
 await setup(page,()=>[]);await page.addInitScript(({legacy,active})=>{if(!sessionStorage.getItem('seed')){sessionStorage.setItem('seed','1');sessionStorage.setItem(legacy,JSON.stringify({tasks:['historical_read'],active,pending:false,messages:[]}));}},{legacy,active});await open(page);await expect(page.getByRole('button',{name:'继续业务办理',exact:true})).toBeEnabled();expect(await page.evaluate(key=>sessionStorage.getItem(key),legacy)).toBeNull();const after=await page.evaluate(key=>JSON.parse(sessionStorage.getItem(key)!),originals);expect(after.blockers).toEqual([]);expect(after.tasks).toContain('historical_read');if(active)expect(after.tasks).toContain(active);
});
