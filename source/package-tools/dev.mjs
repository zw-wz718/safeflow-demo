// Restricted local frontend launcher; verify through actual proxy HTTP before delivery.
// Overrides only in-memory Vite proxy targets; adopted configuration bytes stay intact.
import assert from 'node:assert/strict';
import {fileURLToPath} from 'node:url';
import path from 'node:path';
const args=process.argv.slice(2);assert.equal(args.length,4,'--api-port N --frontend-port N required');
const values={};for(let i=0;i<args.length;i+=2){assert(['--api-port','--frontend-port'].includes(args[i])&&!Object.hasOwn(values,args[i]));values[args[i]]=Number(args[i+1]);}
for(const value of Object.values(values))assert(Number.isInteger(value)&&value>=1024&&value<=65535);
assert(values['--api-port']!==values['--frontend-port']);
const frontend=fileURLToPath(new URL('../frontend/',import.meta.url));
const {createServer,loadConfigFromFile}=await import(new URL('../frontend/node_modules/vite/dist/node/index.js',import.meta.url));
const loaded=await loadConfigFromFile({command:'serve',mode:'development'},path.join(frontend,'vite.config.ts'));
assert(loaded);const config=loaded.config;
const proxy=Object.fromEntries(Object.entries(config.server.proxy).map(([key,value])=>[key,{...value,target:`http://127.0.0.1:${values['--api-port']}`}]))
const server=await createServer({...config,configFile:false,root:frontend,server:{...config.server,proxy,host:'127.0.0.1',port:values['--frontend-port'],strictPort:true}});
await server.listen();server.printUrls();
for(const event of ['SIGINT','SIGTERM'])process.once(event,async()=>{await server.close();process.exit(0);});
