import fs from 'node:fs';
import net from 'node:net';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
export function upstreamOrigin(raw){
  if(!raw)throw Error('Explicit verified HTTPS API origin required');
  const u=new URL(raw),h=u.hostname,ip=net.isIP(h);
  const local=ip===4&&(h.startsWith('127.')||h.startsWith('10.')||h.startsWith('192.168.')||h.startsWith('169.254.')||h==='0.0.0.0'||/^172\.(1[6-9]|2\d|3[01])\./.test(h));
  if(u.protocol!=='https:'||u.username||u.password||u.search||u.hash||u.pathname!=='/'||u.port||local||ip===6||(!ip&&(!/^[a-z0-9]+(?:[.-][a-z0-9]+)*\.[a-z]{2,}$/i.test(h)||/(^|\.)(localhost|local|invalid|test|example)$/.test(h)))||raw!==u.origin)throw Error('Canonical public HTTPS upstream origin required');
  return u.origin;
}
export function configuration(raw){
  const origin=upstreamOrigin(raw);
  return `# Current deployment: Netlify. Proxy response cache behavior requires live verification.\n[build]\n  command = "npm run build"\n  publish = "dist"\n[build.environment]\n  NODE_VERSION = "24.14.0"\n\n# First rule retains /api and precedes SPA fallback. No Origin/authorization spoofing.\n[[redirects]]\n  from = "/api/*"\n  to = "${origin}/api/:splat"\n  status = 200\n  force = true\n\n[[redirects]]\n  from = "/*"\n  to = "/index.html"\n  status = 200\n\n# Static headers are not proof of headers on external proxy responses. Backend sends no-store.\n[[headers]]\n  for = "/api/*"\n  [headers.values]\n    Cache-Control = "no-store"\n\n[[headers]]\n  for = "/assets/*"\n  [headers.values]\n    Cache-Control = "public, max-age=31536000, immutable"\n`;
}
if(process.argv[1]&&path.resolve(process.argv[1])===fileURLToPath(import.meta.url)){
  fs.writeFileSync(new URL('./netlify.toml',import.meta.url),configuration(process.env.SAFEFLOW_PUBLIC_API_ORIGIN),{flag:'wx'});
  console.log('Generated Netlify exact HTTPS /api-preserving rewrite; no deployment performed.');
}
