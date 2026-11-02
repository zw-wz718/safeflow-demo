// Dev-only test mount; excluded from the frontend production entry/build.
import {useState} from 'react';
import {createRoot} from 'react-dom/client';
import BankConversation from '../src/runtime/BankConversation';
import {FeedbackHistory} from '../src/runtime/FeedbackHistory';
import BankApp from '../src/BankApp';
import '../src/bank.css';
function ChatMount(){const [panel,setPanel]=useState('');return <BankConversation accounts={[]} disabled={false} run={async work=>{try{await work();}catch{/* component reports errors */}}} businessPanel={panel?<div>{panel}</div>:null} businessPanelService="CARDS" openService={async(_service,signal)=>{if(!new URLSearchParams(location.search).has('slowpanel'))return;const response=await fetch('/api/e2e/service',{signal});setPanel(await response.text());}}/>;}
function Mount(){const initialRoot=new URLSearchParams(location.search).get('root')??'root_a';const [root,setRoot]=useState(initialRoot);
  const mode=new URLSearchParams(location.search).get('view');
  if(mode==='bank')return <BankApp/>;
  if(mode==='chat')return <ChatMount/>;
  return <><button onClick={()=>setRoot(root===initialRoot?'root_b':initialRoot)}>切换核对原反馈</button><FeedbackHistory root={root}/></>;
}
createRoot(document.getElementById('root')!).render(<Mount/>);
