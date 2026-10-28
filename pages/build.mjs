import fs from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
const root=path.dirname(fileURLToPath(import.meta.url));
const output=process.argv[2];
if(!output || !path.isAbsolute(output) || fs.existsSync(output))throw Error('Choose a new absolute output directory');
const raw=process.env.BANK_APP_URL;
let entry='<button class="cta" disabled>体验入口尚未发布</button>';
if(raw){
  const u=new URL(raw);
  if(u.protocol!=='https:' || u.username || u.password || u.port || u.search || u.hash || u.pathname!=='/' || !/^[a-z0-9]+(?:[.-][a-z0-9]+)*\.[a-z]{2,}$/i.test(u.hostname) || /(^|\.)(localhost|local|invalid|test|example)$/.test(u.hostname))throw Error('BANK_APP_URL must be a verified canonical HTTPS production origin');
  if(process.env.BANK_APP_READY==='true')entry=`<a class="cta" href="${u.origin}/" rel="noopener noreferrer">体验银行 Agent <span aria-hidden="true">↗</span></a>`;
}
const html=fs.readFileSync(path.join(root,'index.html'),'utf8');
if(html.split('__BANK_ENTRY__').length!==2)throw Error('Expected one entry marker');
fs.mkdirSync(output,{recursive:true});
fs.writeFileSync(path.join(output,'index.html'),html.replace('__BANK_ENTRY__',entry));
fs.copyFileSync(path.join(root,'site.css'),path.join(output,'site.css'));
fs.writeFileSync(path.join(output,'.nojekyll'),'');
console.log(JSON.stringify({files:3,bank_entry:raw&&process.env.BANK_APP_READY==='true'?'OPERATOR_MARKED_EXPERIENCE_VERIFIED':'NOT_READY_DISABLED',project_base:'/safeflow-demo/'}));
