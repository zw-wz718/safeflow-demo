import { useEffect, useRef, useState } from 'react';
import { feedbackError } from './billing-feedback-client';
import { RuntimeApiError } from './client';
import { subscriptionRecordId } from './subscription-display';
import { getFeedbackHistory, prepareRevision, confirmRevision, executeRevision, readRevisionIntent, cancelRevisionIntent, revisionIntentSchema,
  type FeedbackHistory as History, type RevisionIntent, type RevisionPrepared } from './feedback-history-client';

const label=(v:unknown)=>({MINE:'是本人交易',NOT_MINE:'不是本人交易',UNSURE:'暂不确定',INITIAL:'首次反馈',CORRECT:'更正',REVOKE:'撤销',RESUBMIT:'重新提交'}[String(v)]??'已撤销');
/** Confirmation is deliberately memory-only. The persisted selector cannot authorize a write. */
export function FeedbackHistory({root,disabled=false}:{root:string;disabled?:boolean}){
  const [head,setHead]=useState<History>();const [pending,setPending]=useState<RevisionIntent>();
  const [prepared,setPrepared]=useState<RevisionPrepared>();const [confirmed,setConfirmed]=useState(false);
  const [choice,setChoice]=useState<'MINE'|'NOT_MINE'|'UNSURE'>('UNSURE');const [notice,setNotice]=useState('');
  const [busy,setBusy]=useState(false);const active=useRef(false);const life=useRef<AbortController | undefined>(undefined);
  const key='safeflow.feedback-history.v1.'+root;
  useEffect(()=>{const c=new AbortController();life.current=c;active.current=false;
    setHead(undefined);setPending(undefined);setPrepared(undefined);setConfirmed(false);setNotice('');setBusy(false);
    try{const raw=sessionStorage.getItem(key);if(raw){const p=revisionIntentSchema.parse(JSON.parse(raw));if(p.root_operation_id!==root)throw Error('原号不一致');setPending(p);}}
    catch{setNotice('本机修改原号记录待核对，已阻止准备新的修改。');active.current=true;}
    void getFeedbackHistory(root,c.signal).then(v=>{if(!c.signal.aborted)setHead(v);}).catch(e=>{if(!c.signal.aborted)setNotice(feedbackError(e));});
    return()=>c.abort();},[root,key]);
  const run=async(work:(s:AbortSignal)=>Promise<void>)=>{const c=life.current;if(disabled||active.current||!c||c.signal.aborted)return;
    active.current=true;setBusy(true);setNotice('');
    try{await work(c.signal);}catch(e){if(!c.signal.aborted&&life.current===c){setConfirmed(false);setNotice(feedbackError(e));}}
    finally{if(life.current===c){active.current=false;if(!c.signal.aborted)setBusy(false);}}};
  const refresh=async(s:AbortSignal)=>{const v=await getFeedbackHistory(root,s);if(!s.aborted)setHead(v);return v;};
  const begin=async(action:RevisionIntent['action'],s:AbortSignal)=>{
    if(pending||!head)throw Error('请先按原号核对此次修改');
    const h=await refresh(s);if(s.aborted)return;
    const intent:RevisionIntent={input_version:1,root_operation_id:root,expected_revision:h.revision,action,nonce:crypto.randomUUID(),...(action==='REVOKE'?{}:{choice})};
    sessionStorage.setItem(key,JSON.stringify(intent));setPending(intent);setPrepared(undefined);setConfirmed(false);
    const p=await prepareRevision(intent,s);if(!s.aborted)setPrepared(p);
  };
  const recover=async(s:AbortSignal)=>{if(!pending)return;setPrepared(undefined);setConfirmed(false);
    const v=await readRevisionIntent(pending,s);
    if(s.aborted)return;
    await refresh(s);if(s.aborted)return;
    if(v.saved){setNotice(`原修改已保存：版本 ${v.saved.revision} · ${label(v.saved.action)}。`);sessionStorage.removeItem(key);setPending(undefined);}
    else setNotice('原准备未显示保存。可明确重新确认原选择；仍须重新确认。');
  };
  const off=disabled||busy;
  return <section aria-label="反馈修改历史" className="bank-panel">
    <h3>当前个人意见与修改历史</h3>
    <p>保留最初反馈及每次修改；不撤销交易，不自动冻结或解冻银行卡。</p>
    {head&&<><p className="bank-result">版本 {head.revision} · {head.active?label(head.choice):'已撤销，当前无有效意见'}</p>
      <ol>{head.history.map(r=><li key={r.revision}>版本 {r.revision} · {label(r.action)} · {r.active?label(r.choice):'无有效意见'}<details><summary>原办理号</summary>{r.operation_id}</details></li>)}</ol>
      {!pending&&<><label>新的本人判断<select disabled={off} value={choice} onChange={e=>setChoice(e.target.value as typeof choice)}>
        <option value="MINE">是本人交易</option><option value="NOT_MINE">不是本人交易</option><option value="UNSURE">暂不确定</option></select></label>
        <button disabled={off||(head.active&&choice===head.choice)} onClick={()=>void run(s=>begin(head.active?'CORRECT':'RESUBMIT',s))}>{head.active?'准备更正':'准备重新提交'}</button>
        {head.active&&<button disabled={off} onClick={()=>void run(s=>begin('REVOKE',s))}>准备撤销反馈</button>}</>}
    </>}
    {pending&&<><p>修改编号 反馈-{subscriptionRecordId(pending.nonce).slice(4)} · {label(pending.action)}</p><details><summary>核对详情</summary><p>原始编号 {pending.nonce} · 基于版本 {pending.expected_revision}</p></details>
      <button disabled={off} onClick={()=>void run(recover)}>查看办理进度</button>
      {!prepared&&<button disabled={off} onClick={()=>void run(async s=>{
        await readRevisionIntent(pending,s).then(v=>{if(v.saved)throw Error('此原号已保存，请只读核对');}).catch(e=>{if(!(e instanceof RuntimeApiError&&e.status===404&&e.reason==='NOT_FOUND'))throw e;});
        const p=await prepareRevision(pending,s);if(!s.aborted){setPrepared(p);setConfirmed(false);}
      })}>重新确认原选择</button>}
    </>}
    {pending&&<button disabled={off} onClick={()=>void run(async s=>{
      const result=await cancelRevisionIntent(pending,s);if(s.aborted)return;
      if(result.kind==='SAVED'){
        setPrepared(undefined);setConfirmed(false);
        await refresh(s);if(s.aborted)return;
        sessionStorage.removeItem(key);setPending(undefined);
        setNotice(`原修改已保存：版本 ${result.saved.revision} · ${label(result.saved.action)}，未取消已保存意见。`);
        return;
      }
      const archiveKey=key+'.ended-intents';const archived:unknown=JSON.parse(sessionStorage.getItem(archiveKey)??'[]');
      if(!Array.isArray(archived))throw Error('已结束修改记录待核对');
      sessionStorage.setItem(archiveKey,JSON.stringify([...archived,result.proof]));
      sessionStorage.removeItem(key);setPending(undefined);setPrepared(undefined);setConfirmed(false);
      await refresh(s);if(!s.aborted)setNotice('已取消本次未保存的修改，原反馈和历史记录保留。需要修改时请重新选择并确认。');
    })}>核对并取消本次修改</button>}
    {prepared&&<><p>请核对：{label(prepared.parameters.revision.action)}；原版本 {prepared.parameters.revision.expected_revision}；{prepared.parameters.revision.action==='REVOKE'?'撤销后无有效意见':label(prepared.parameters.choice)}。确认有效至 {prepared.confirmation.expires_at}。</p>
      <button disabled={off||confirmed} onClick={()=>void run(async s=>{await confirmRevision(prepared,s);if(!s.aborted)setConfirmed(true);})}>本人确认此次{label(prepared.parameters.revision.action)}</button>
      <button disabled={off||!confirmed} onClick={()=>void run(async s=>{await executeRevision(prepared,s);if(s.aborted)return;setPrepared(undefined);setConfirmed(false);await recover(s);})}>保存此次{label(prepared.parameters.revision.action)}</button></>}
    <button disabled={off} onClick={()=>void run(async s=>{await refresh(s);})}>重新读取当前意见</button>
    {notice&&<p role="status">{notice}</p>}
  </section>;
}
