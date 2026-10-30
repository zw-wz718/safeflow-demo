import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { fileURLToPath } from 'node:url';

const root=path.dirname(fileURLToPath(import.meta.url));
const output=process.argv[2];
const preview=process.argv[3]==='--preview';
if(process.argv.length>(preview?4:3))throw Error('Unsupported build argument');
if(!output||!path.isAbsolute(output)||fs.existsSync(output)||path.relative(root,output)===''||!path.relative(root,output).startsWith('..'+path.sep))throw Error('Choose a new absolute output directory outside pages source');
const sha=b=>crypto.createHash('sha256').update(b).digest('hex');
const escape=s=>String(s).replaceAll('&','&amp;').replaceAll('<','&lt;').replaceAll('>','&gt;').replaceAll('"','&quot;').replaceAll("'",'&#39;');
function read(relative){
  const p=path.join(root,relative);
  if(fs.lstatSync(p).isSymbolicLink()||!fs.realpathSync(p).startsWith(fs.realpathSync(root)+path.sep))throw Error('Material must be an ordinary file within pages');
  return fs.readFileSync(p);
}
function inline(text){
  // Raw HTML is text, never executable markup. Links are deliberately narrow.
  let s=escape(text);
  s=s.replace(/`([^`]+)`/g,'<code>$1</code>').replace(/\*\*([^*]+)\*\*/g,'<strong>$1</strong>');
  s=s.replace(/\[([^\]]+)\]\(([^\s)]+)\)/g,(_,label,url)=>{
    if(!/^https:\/\/[a-z0-9.-]+(?:\/[a-z0-9_./%?#=&;:+~-]*)?$/i.test(url))return label+' ('+url+')';
    return '<a href="'+url+'" rel="noopener noreferrer">'+label+'</a>';
  });
  return s;
}
const guideAnchors=[['before',/体验前|开始前|开始体验|准备|登录|演示须知/],['transfer',/转账|付款/],['subscriptions',/订阅/],['cards',/锁卡|冻结|银行卡/],['billing',/账单|反馈/],['recovery',/恢复|中断|异常|原号/]];
function markdown(text,id){
  const lines=text.replaceAll('\r\n','\n').split('\n');let result='',paragraph=[],list='',code=null;const used=new Set();let count=0;
  const flush=()=>{if(paragraph.length){result+='<p>'+inline(paragraph.join(' '))+'</p>\n';paragraph=[];}};
  const closeList=()=>{if(list){result+='</'+list+'>\n';list='';}};
  for(let i=0;i<lines.length;i++){
    const line=lines[i];
    if(/^```/.test(line)){flush();closeList();if(code===null)code=[];else{result+='<pre><code>'+escape(code.join('\n'))+'</code></pre>\n';code=null;}continue;}
    if(code!==null){code.push(line);continue;}
    if(!line.trim()){flush();closeList();continue;}
    const heading=/^(#{1,6})\s+(.+)$/.exec(line);
    if(heading){flush();closeList();const level=Math.max(2,heading[1].length),title=heading[2];let anchor='section-'+(++count);
      if(id==='guide'){const match=guideAnchors.find(([name,pattern])=>!used.has(name)&&pattern.test(title));if(match){anchor=match[0];used.add(anchor);}}
      if(id==='security'&&!used.has('risks')&&/风险|限制/.test(title)){anchor='risks';used.add(anchor);}
      result+='<h'+level+' id="'+anchor+'">'+inline(title)+'</h'+level+'>\n';continue;
    }
    if(line.includes('|')&&/^\s*\|?\s*:?-{3,}/.test(lines[i+1]||'')){
      flush();closeList();const cells=v=>v.trim().replace(/^\|/,'').replace(/\|$/,'').split('|').map(x=>inline(x.trim()));
      const head=cells(line);i++;result+='<div class="table-scroll" tabindex="0" role="region" aria-label="可横向滚动的资料表"><table><thead><tr>'+head.map(c=>'<th scope="col">'+c+'</th>').join('')+'</tr></thead><tbody>';
      while(i+1<lines.length&&lines[i+1].includes('|')&&lines[i+1].trim()){const row=cells(lines[++i]);result+='<tr>'+row.map(c=>'<td>'+c+'</td>').join('')+'</tr>';}
      result+='</tbody></table></div>\n';continue;
    }
    const item=/^\s*(?:[-*+]\s+|\d+[.)、]\s*)(.+)$/.exec(line);
    if(item){flush();const kind=/^\s*\d/.test(line)?'ol':'ul';if(list!==kind){closeList();list=kind;result+='<'+list+'>';}result+='<li>'+inline(item[1])+'</li>';continue;}
    if(/^>\s?/.test(line)){flush();closeList();result+='<blockquote>'+inline(line.replace(/^>\s?/,''))+'</blockquote>';continue;}
    if(/^\s*---+\s*$/.test(line)){flush();closeList();result+='<hr>';continue;}
    closeList();paragraph.push(line.trim());
  }
  flush();closeList();if(code!==null)throw Error('Unclosed document code fence');
  if(id==='guide'&&!preview){for(const [name] of guideAnchors)if(!used.has(name))throw Error('Guide needs a matching section for '+name);}
  if(id==='security'&&!preview&&!used.has('risks'))throw Error('Security document needs explicit known risks');
  return result;
}

const manifest=JSON.parse(read('public-materials.json').toString('utf8'));
const ids=['technical','security','guide'];
if(!Array.isArray(manifest.documents)||manifest.documents.length!==3||new Set(manifest.documents.map(d=>d.id)).size!==3||manifest.documents.some(d=>!ids.includes(d.id)))throw Error('Expected three exact public documents');
if(!preview&&manifest.status!=='FINAL_CONTENT_READY')throw Error('Final public PDFs and same-text Markdown are not ready');
const materials=[],bodies={};let download='';
if(manifest.status==='FINAL_CONTENT_READY'){
  download='<div class="download-grid">';
  for(const doc of manifest.documents){
    if(typeof doc.title!=='string'||typeof doc.description!=='string')throw Error('Document metadata missing');
    for(const [kind,relative] of [['pdf','downloads/safeflow-'+doc.id+'.pdf'],['markdown','content/'+doc.id+'.md']]){
      const expected=doc[kind];if(!expected||expected.path!==relative)throw Error('Unexpected public material path');
      const bytes=read(relative);if(bytes.length!==expected.bytes||sha(bytes)!==expected.sha256)throw Error('Public material digest mismatch: '+doc.id+'/'+kind);
      if(bytes.length===0||bytes.length>50*1024*1024)throw Error('Public document empty or over 50 MiB');
      if(kind==='pdf'){
        if(bytes.subarray(0,5).toString()!=='%PDF-')throw Error('Invalid PDF header');
        materials.push({relative,bytes});
      }else bodies[doc.id]=markdown(bytes.toString('utf8'),doc.id);
    }
    const pdf=doc.pdf;download+='<article class="download-card"><span class="tag">'+escape(doc.id.toUpperCase())+' / PDF</span><h3>'+escape(doc.title)+'</h3><p>'+escape(doc.description)+'</p><span class="file-meta">PDF · '+(pdf.bytes/1024/1024).toFixed(2)+' MB</span><div class="download-actions"><a href="./'+doc.id+'.html">在线阅读 →</a><a href="./'+pdf.path+'" download>下载 PDF ↓</a></div><details><summary>文件校验 SHA-256</summary><code>'+pdf.sha256+'</code></details></article>';
  }
  download+='</div><p class="small">文档版本：'+escape(manifest.version)+'。当前账户、余额、卡锁和候选状态以登录后的实际读取为准。</p>';
}else{
  download='<p class="preparation">本地结构预览：正式公开文稿与 PDF 尚未装入，本预览不提供下载。</p>';
  for(const id of ids)bodies[id]='<p class="preparation">本地结构预览，待装入统筹固定的公开文稿。</p>';
  bodies.guide+=guideAnchors.map(([id])=>'<h2 id="'+id+'">'+escape(id)+'</h2><p>正式使用说明待装入。</p>').join('');
  bodies.security+='<h2 id="risks">已知风险</h2>';
}
const raw=process.env.BANK_APP_URL;let entry='<button class="cta" disabled>体验入口尚未发布</button>';
if(raw){const u=new URL(raw);if(u.protocol!=='https:'||u.username||u.password||u.port||u.search||u.hash||u.pathname!=='/'||!/^[a-z0-9]+(?:[.-][a-z0-9]+)*\.[a-z]{2,}$/i.test(u.hostname)||/(^|\.)(localhost|local|invalid|test|example)$/.test(u.hostname))throw Error('BANK_APP_URL must be a verified canonical HTTPS production origin');if(process.env.BANK_APP_READY==='true')entry='<a class="cta" href="'+u.origin+'/" rel="noopener noreferrer">体验银行 Agent <span aria-hidden="true">↗</span></a>';}
const staged=[];
for(const name of ['index.html','guide.html','technical.html','security.html']){
  let html=read(name).toString('utf8');
  html=html.replaceAll('__BANK_ENTRY__',entry).replaceAll('__DOWNLOADS__',download).replaceAll('__GUIDE_CONTENT__',bodies.guide).replaceAll('__TECHNICAL_CONTENT__',bodies.technical).replaceAll('__SECURITY_CONTENT__',bodies.security);
  if(/__[A-Z_]+__/.test(html)||/<script\b|\son[a-z]+\s*=/i.test(html))throw Error('Unexpected marker or active content');
  staged.push({relative:name,bytes:Buffer.from(html)});
}
for(const relative of ['site.css','favicon.svg'])staged.push({relative,bytes:read(relative)});
staged.push(...materials,{relative:'.nojekyll',bytes:Buffer.from('')});
if(materials.length)staged.push({relative:'downloads/SHA256SUMS.txt',bytes:Buffer.from(materials.map(m=>sha(m.bytes)+'  '+path.basename(m.relative)).join('\n')+'\n')});
fs.mkdirSync(output,{recursive:true});for(const {relative,bytes} of staged){const dest=path.join(output,relative);fs.mkdirSync(path.dirname(dest),{recursive:true});fs.writeFileSync(dest,bytes);}
console.log(JSON.stringify({files:staged.length,preview,materials:materials.length,bank_entry:raw&&process.env.BANK_APP_READY==='true'?'OPERATOR_MARKED_EXPERIENCE_VERIFIED':'NOT_READY_DISABLED',project_base:'/safeflow-demo/',output_files:staged.map(m=>({path:m.relative,bytes:m.bytes.length,sha256:sha(m.bytes)}))}));
