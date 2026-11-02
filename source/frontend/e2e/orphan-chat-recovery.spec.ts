import {readFileSync} from 'node:fs';
import {test,expect,type Page,type Route} from '@playwright/test';
import {guardReplayApi,assertReplayIsolated} from './isolated-api';
const context='a'.repeat(64),epoch='00000000-0000-4000-8000-000000000002';
const old='bank_chat_00000000-0000-4000-8000-000000000001_1';
const originals='safeflow.bank.chat-originals.v1.'+context;
const reply=(r:Route,v:unknown,status=200)=>r.fulfill({status,contentType:'application/json',body:JSON.stringify(v)});
const proof=(task:string)=>({contract_version:1,scope:'UNCREATED_CHAT_REQUEST_FENCED',task_id:task,context_ref:context,creation_disabled:true,task_created:false,payment_cancelled:false,proof_source:task.startsWith('bank_chat_v2_')?'COMPLETED_SERVER_TURN':'RETIRED_REQUEST_NAMESPACE'});
async function setup(page:Page){
 await guardReplayApi(page);
 await page.route('**/api/bank/expire-draft-intents',r=>reply(r,{contract_version:1,expired_draft_intents:[],payment_cancelled:false,server_time:true}));
 await page.route('**/api/bank/recovery',r=>reply(r,{context_ref:context,payment_tasks:[],subscription_nonces:[]}));
 await page.route('**/api/agent/status',r=>reply(r,{contract_version:1,mode:'PROJECT_SIMULATION',configured:true,connectivity:'NOT_VERIFIED',tools:[],draft_enabled:true,planning_mode:'OPENAI_COMPATIBLE',chat_request_epoch:epoch}));
 await page.route('**/api/bank/overview',r=>reply(r,{accounts:[]}));
 await page.route('**/api/agent/tasks/*',r=>reply(r,{},404));
 await page.route('**/api/sim/draft-lifecycles/*',r=>reply(r,{},404));
 await page.route('**/api/sim/payment-preparations/*',r=>reply(r,{},404));
 await page.route('**/api/bank/uncreated-chat-requests/*',r=>reply(r,{},503));
}
async function open(page:Page){await page.goto('/e2e/site-repairs.html?view=chat');await expect(page.getByLabel('向银行助手发送消息')).toBeEnabled();}
async function blockers(page:Page){return page.evaluate(key=>JSON.parse(sessionStorage.getItem(key)!).blockers as string[],originals);}
test.afterEach(async({page})=>assertReplayIsolated(page));
for(const mismatch of ['503','context','id','version','type'])test('legacy cache remains protected on '+mismatch+' then recovers through exact original entry',async({page},info)=>{
 await setup(page);let valid=false,model=0;
 await page.addInitScript(({context,old})=>{sessionStorage.setItem('safeflow.bank.chat.v1.'+context,JSON.stringify({tasks:[old],active:old,pending:true,messages:[]}));},{context,old});
 await page.route('**/api/bank/read-conversation',r=>{model++;return reply(r,{},503);});
 await page.route('**/api/bank/conversation',r=>{model++;return reply(r,{},503);});
 await page.route('**/api/bank/uncreated-chat-requests/*',r=>{const task=decodeURIComponent(new URL(r.request().url()).pathname.split('/').at(-1)!);let value=proof(task);if(!valid){if(mismatch==='503')return reply(r,{},503);value={...value,...(mismatch==='context'?{context_ref:'b'.repeat(64)}:mismatch==='id'?{task_id:old+'0'}:mismatch==='version'?{contract_version:2}:{scope:'CANCELLED'})};}return reply(r,value);});
 await open(page);await expect(page.getByRole('button',{name:'继续业务办理',exact:true})).toBeDisabled();
 await page.locator('.chat-recovery-controls > summary').click();await expect(page.getByLabel('待核实原号')).toContainText(old);await expect(page.getByLabel('待核实原号')).toContainText('读取失败');
 await page.getByLabel('向银行助手发送消息').fill('给李明远转账100元');await page.getByRole('button',{name:'发送消息',exact:true}).click();await expect(page.getByText('原办理请求尚未核清，本次转账未发起。',{exact:false})).toBeVisible();expect(model).toBe(0);expect(await blockers(page)).toEqual([old]);
 valid=true;await page.getByRole('button',{name:'重新核查原号 '+old,exact:true}).click();await expect(page.getByRole('button',{name:'继续业务办理',exact:true})).toBeEnabled();expect(await blockers(page)).toEqual([]);expect(await page.evaluate(key=>JSON.parse(sessionStorage.getItem(key)!).tasks,originals)).toContain(old);
 await info.attach('recovery-'+mismatch,{body:JSON.stringify({original:old,remaining:await blockers(page),modelCalls:model}),contentType:'application/json'});
});
for(const mode of ['clear','refresh'])test('new v2 delayed CLARIFY after '+mode+' is recovered from completed Turn proof',async({page})=>{
 await setup(page);let task='',completed=false;let reached!:()=>void;const arrived=new Promise<void>(r=>reached=r);let release!:()=>void;const gate=new Promise<void>(r=>release=r);
 await page.route('**/api/bank/conversation',async r=>{const input=r.request().postDataJSON();task=`bank_chat_v2_${epoch}_${input.conversation_id}_${input.revision+1}`;reached();await gate;completed=true;await reply(r,{conversation_id:input.conversation_id,revision:input.revision+1,task_id:null,view:{contract_version:1,mode:'PROJECT_SIMULATION',kind:'CLARIFY',clarification:{question:'请提供金额'},bank_service:null},uncertain:false,payee_query:null}).catch(()=>{});});
 await page.route('**/api/bank/uncreated-chat-requests/*',r=>completed?reply(r,proof(task)):reply(r,{},409));
 await open(page);await expect(page.getByRole('button',{name:'继续业务办理',exact:true})).toBeEnabled();await page.getByLabel('向银行助手发送消息').fill('准备转账');await page.getByRole('button',{name:'发送消息',exact:true}).click();await arrived;
 if(mode==='clear')await page.getByRole('button',{name:'清空聊天',exact:true}).click();else await page.reload();
 await expect(page.getByLabel('向银行助手发送消息')).toBeEnabled();await expect(page.getByRole('button',{name:'继续业务办理',exact:true})).toBeDisabled();expect(await blockers(page)).toEqual([task]);
 release();await expect.poll(()=>completed).toBe(true);await page.locator('.chat-recovery-controls > summary').click();await page.getByRole('button',{name:'重新核查原号 '+task,exact:true}).click();await expect(page.getByRole('button',{name:'继续业务办理',exact:true})).toBeEnabled();expect(await blockers(page)).toEqual([]);await expect(page.getByText('请提供金额',{exact:true})).toHaveCount(0);
});

test('existing ten settled originals and one local orphan recover together without business replay',async({page},info)=>{
 test.skip(!process.env.SAFEFLOW_RECOVERY_REPLAY_FILE,'explicit local read-only sanitized replay input required');
 const snapshot=JSON.parse(readFileSync(process.env.SAFEFLOW_RECOVERY_REPLAY_FILE!,'utf8'));
 expect(snapshot.tasks).toHaveLength(10);await setup(page);
 await page.addInitScript(({context,old})=>{sessionStorage.setItem('safeflow.bank.chat.v1.'+context,JSON.stringify({tasks:[old],active:old,pending:true,messages:[]}));},{context,old});
 await page.route('**/api/bank/recovery',r=>reply(r,{...snapshot.directory,context_ref:context}));
 await page.route('**/api/bank/uncreated-chat-requests/*',r=>reply(r,proof(old)));
 await page.route('**/api/sim/draft-lifecycles/*',r=>{const id=decodeURIComponent(new URL(r.request().url()).pathname.split('/').at(-1)!);const row=snapshot.tasks.find((t:{task_id:string})=>t.task_id===id);return reply(r,row?.draft??{},row?.draft_status??404);});
 await page.route('**/api/sim/payment-preparations/**',r=>{const parts=new URL(r.request().url()).pathname.split('/').filter(Boolean);const row=snapshot.tasks.find((t:{task_id:string})=>t.task_id===decodeURIComponent(parts[3]));if(parts.length===4)return reply(r,row?.payments??{},row?.payments_status??404);const funds=row?.funds.find((f:{status:number,value:{preparation_operation_id:string}})=>f.value.preparation_operation_id===parts[4]);return reply(r,funds?.value??{},funds?.status??404);});
 await open(page);await expect(page.getByRole('button',{name:'继续业务办理',exact:true})).toBeEnabled();expect(await blockers(page)).toEqual([]);
 await info.attach('existing-ten-summary',{body:JSON.stringify({directoryCount:10,extraLocalOrphan:1,remaining:await blockers(page),businessReexecuted:false}),contentType:'application/json'});
});
