import {useEffect, useState} from 'react';
import {z} from 'zod';
import {request} from './client';
export type DemoCodeRequest = {kind:'PAYMENT';task_id:string;operation_id:string;consent_id:string} | {kind:'CARD';operation_id:string};
const codeSchema=z.object({mode:z.literal('SAME_DEVICE_SIMULATION_ONLY'),kind:z.enum(['PAYMENT','CARD']),operation_id:z.string(),task_id:z.string().optional(),challenge_id:z.string().optional(),code:z.string().regex(/^\d{6}$/),business_executed:z.literal(false),automatic_approval:z.literal(false)}).strict();
export default function DemoVerification({operation,disabled}:{operation:DemoCodeRequest;disabled:boolean}) {
  const [available,setAvailable]=useState(false),[delivery,setDelivery]=useState<z.infer<typeof codeSchema>>(),[busy,setBusy]=useState(false),[error,setError]=useState('');
  useEffect(()=>{const c=new AbortController();void request('/api/demo-verification','GET',undefined,c.signal).then(v=>{if(!c.signal.aborted)setAvailable(z.object({available:z.boolean()}).strict().parse(v).available);}).catch(()=>{});return()=>c.abort();},[]);
  useEffect(()=>{if(!delivery)return;const t=setTimeout(()=>{setDelivery(undefined);setError('模拟码展示已结束。请保留原号，以服务端过期或原结果提示为准。');},120000);return()=>clearTimeout(t);},[delivery]);
  async function receive(){
    setBusy(true);setError('');
    try{
      const v=codeSchema.parse(await request('/api/demo-verification','POST',operation));
      if(v.kind!==operation.kind || v.operation_id!==operation.operation_id || (operation.kind==='PAYMENT' && v.task_id!==operation.task_id) || (operation.kind==='CARD' && !v.challenge_id))throw Error('原操作不匹配，保留原号查询');
      setDelivery(v);
    }catch{setError('本次模拟码不可领取或已领取；不自动重试。请保留原号查询，过期不能恢复原确认。');}
    finally{setBusy(false);}
  }
  return <section className="bank-panel" aria-label="模拟验证页面">
    <h3>演示验证 · 同设备模拟显码</h3>
    <p>只用于本会话、本次操作，不是银行强认证、真实短信或身份核验。领取不会核验或执行操作。</p>
    <p className="bank-ref">本次操作 {operation.operation_id}</p>
    <button disabled={disabled||busy||!available||!!delivery} onClick={()=>void receive()}>领取本次模拟验证码</button>
    {!available&&<p>此环境未启用页面显码。本机模式请使用原私有控制台。</p>}
    {delivery&&<div role="status"><p>本次模拟验证码：<strong>{delivery.code}</strong></p>{delivery.challenge_id&&<p>卡片挑战号：<span>{delivery.challenge_id}</span></p>}<p>请手动填写下方原验证表单，再点击核验和最终执行。有效期最长2分钟，最终以服务端原确认、过期及失败状态为准；验证码不会自动填入。</p></div>}
    {error&&<p role="status">{error}</p>}
  </section>;
}
