'use strict';
// Builds an isolated fictional write probe against supplied, unchanged foundation code.
// No patching its migrator/authentication, public numbering, or source lockfile.
const fs=require('node:fs'),path=require('node:path'),crypto=require('node:crypto'),assert=require('node:assert/strict');
const {spawnSync}=require('node:child_process');
const repo=path.resolve(__dirname,'..'),v={};
for(let i=2;i<process.argv.length;i++){
 const k=process.argv[i];assert(['--foundation-root','--foundation-label','--output-root','--docker','--cargo'].includes(k)&&!v[k]&&process.argv[i+1],'INVALID_ARGUMENT');v[k]=process.argv[++i];
}
assert(v['--foundation-root']&&v['--foundation-label']&&v['--output-root'],'FOUNDATION_ROOT_LABEL_AND_OUTPUT_REQUIRED');
const foundation=fs.realpathSync(v['--foundation-root']),output=path.resolve(v['--output-root']);
const outside=(root,p)=>{const r=path.relative(root,p);return r&&(r==='..'||r.startsWith('..'+path.sep)||path.isAbsolute(r));};
assert(outside(repo,output)&&outside(foundation,output),'OUTPUT_MUST_BE_OUTSIDE_SOURCE_TREES');
fs.mkdirSync(output,{recursive:true});const out=fs.mkdtempSync(path.join(output,'subscription-shared-business-queries-'));
const project=path.join(out,'project'),target=path.resolve(process.env.CARGO_TARGET_DIR||path.join(out,'target'));
assert(outside(repo,target)&&outside(foundation,target),'TARGET_MUST_BE_OUTSIDE_SOURCE_TREES');fs.mkdirSync(path.join(project,'src'),{recursive:true});
const sha=b=>crypto.createHash('sha256').update(b).digest('hex');
function collect(root,dir){return fs.readdirSync(path.join(root,dir),{withFileTypes:true}).flatMap(e=>{
 const f=dir+'/'+e.name;return e.isDirectory()&&!['target','.git','node_modules'].includes(e.name)?collect(root,f):e.isFile()&&(/\.(rs|sql|toml|cjs|ps1)$/.test(e.name)||e.name==='Cargo.lock')?[f]:[];
});}
const moduleFiles=[...collect(repo,'src'),...collect(repo,'adapters'),...collect(repo,'tests'),...collect(repo,'examples'),...collect(repo,'tools'),...collect(repo,'sql'),'Cargo.toml','Cargo.lock','fixtures/discovery_ready_input.v1.json','fixtures/legacy_reminder_three_failures.v1.json'];
const billingRoot=path.join(foundation,'members/jonathan/backend/src/features/billing');
const referenceFiles=['Cargo.toml','Cargo.lock',...collect(foundation,'crates'),...collect(foundation,'features/transfer'),...collect(foundation,'members/jonathan/backend/src/features/billing/src'),'members/jonathan/backend/src/features/billing/Cargo.toml','members/jonathan/fixtures/billing/consumption_pages.v1.json'];
const hashes=(root,files)=>Object.fromEntries(files.map(f=>[f,sha(fs.readFileSync(path.join(root,f)))]));
const before=hashes(repo,moduleFiles),referenceBefore=hashes(foundation,referenceFiles);
const migrations=fs.readdirSync(path.join(foundation,'crates/storage/migrations')).filter(f=>/^\d{4}_.+\.sql$/.test(f)).sort();assert.equal(migrations.length,21);
assert.deepEqual(migrations.map(f=>Number(f.slice(0,4))),Array.from({length:21},(_,i)=>i+1));
const template='tools/shared_ledger_write_probe.rs',extraTemplate='tools/shared_ledger_workflow_cases.rs',reminderTemplate='tools/shared_ledger_reminder_cases.rs',queryTemplate='tools/shared_ledger_reminder_query_cases.rs',initializationTemplate='tools/shared_ledger_initialization_cases.rs',businessQueryTemplate='tools/shared_subscription_query_cases.rs';
let generated=fs.readFileSync(path.join(repo,template),'utf8');
const call='let records = cases(&mut c, &mut other, &options, &schema, &role, &sample, &base).await?;';
assert(generated.includes(call),'BASE_ACCEPTANCE_CALL_CHANGED');
generated=generated.replace(call,'let mut records = cases(&mut c, &mut other, &options, &schema, &role, &sample, &base).await?;\n    records.extend(workflow_cases(&mut c,&mut other,&schema,&sample,&base).await?);\n    records.extend(reminder_cases(&mut c,&mut other,&schema,&base).await?);\n    records.extend(reminder_query_cases(&mut c,&mut other,&schema,&base).await?);\n    records.extend(initialization_cases(&mut c,&mut other,&schema,&sample,&base).await?);\n    records.extend(shared_subscription_query_cases(&mut c,&mut other,&schema,&sample,&base).await?);');
generated=generated.replace('    let mut current = snapshot(c, schema).await?;','    eprintln!("verified case: {name}");\n    let mut current = snapshot(c, schema).await?;');
const phase='if std::env::var("SAFEFLOW_SUBSCRIPTION_WRITE_PHASE").as_deref() == Ok("after-restart") {';
assert(generated.includes(phase),'BASE_RESTART_PHASE_CHANGED');
generated=generated.replace(phase,phase+'\n        workflow_after_restart(&mut c,&mut other,&schema).await?;\n        reminder_queries_after_restart(&mut c,&schema).await?;\n        reminders_after_restart(&mut c,&mut other,&schema).await?;\n        initialization_after_restart(&mut c,&mut other,&schema).await?;\n        shared_business_queries_after_restart(&mut c,&schema).await?;');
generated=generated.replace('"settings_restored":true,','"shared_business_queries_restored":true,"initialization_restored":true,"reminder_queries_restored":true,"reminders_restored":true,"workflow_restored":true,"settings_restored":true,');
fs.writeFileSync(path.join(project,'src/main.rs'),generated+'\n'+fs.readFileSync(path.join(repo,extraTemplate),'utf8')+'\n'+fs.readFileSync(path.join(repo,reminderTemplate),'utf8')+'\n'+fs.readFileSync(path.join(repo,queryTemplate),'utf8')+'\n'+fs.readFileSync(path.join(repo,initializationTemplate),'utf8')+'\n'+fs.readFileSync(path.join(repo,businessQueryTemplate),'utf8')+'\n'+fs.readFileSync(path.join(repo,'tools/recent_shared_cases.rs'),'utf8'));
const generatedFile=path.join(project,'src/main.rs');
fs.writeFileSync(generatedFile,fs.readFileSync(generatedFile,'utf8').replace('    records.extend(shared_subscription_query_cases(&mut c,&mut other,&schema,&sample,&base).await?);','    records.extend(shared_subscription_query_cases(&mut c,&mut other,&schema,&sample,&base).await?);\n    records.extend(recent_shared_cases(&mut c,&mut other,&schema,&sample,&base).await?);').replace('        workflow_after_restart','        recent_shared_after_restart(&mut c,&mut other,&schema).await?;\n        workflow_after_restart').replace('"shared_business_queries_restored":true,','"recent_shared_restored":true,"shared_business_queries_restored":true,'));
fs.appendFileSync(generatedFile,'\n'+fs.readFileSync(path.join(repo,'tools/reminder_limit_cases.rs'),'utf8'));
fs.writeFileSync(generatedFile,fs.readFileSync(generatedFile,'utf8').replace('    records.extend(recent_shared_cases(&mut c,&mut other,&schema,&sample,&base).await?);','    records.extend(recent_shared_cases(&mut c,&mut other,&schema,&sample,&base).await?);\n    records.extend(limits_cases(&mut c,&mut other,&schema,&base).await?);').replace('        recent_shared_after_restart','        limits_after_restart(&mut c,&mut other,&schema).await?;\n        recent_shared_after_restart').replace('"recent_shared_restored":true,','"limits_restored":true,"recent_shared_restored":true,'));
const asRustPath=p=>JSON.stringify(p.replaceAll('\\','/'));
fs.appendFileSync(generatedFile,'\n#[path='+asRustPath(path.join(repo,'adapters/billing_reader_mock.rs'))+'] mod billing_direct;\n#[path='+asRustPath(path.join(repo,'tools/billing_direct_fixture.rs'))+'] mod billing_fixture;\n'+fs.readFileSync(path.join(repo,'tools/billing_direct_cases.rs'),'utf8'));
fs.writeFileSync(generatedFile,fs.readFileSync(generatedFile,'utf8').replace('    records.extend(limits_cases(&mut c,&mut other,&schema,&base).await?);','    records.extend(limits_cases(&mut c,&mut other,&schema,&base).await?);\n    records.extend(direct_billing_cases(&mut c,&mut other,&schema,&base).await?);'));
const q=p=>JSON.stringify(p.replaceAll('\\','/'));
const manifest=`[package]
name = "safeflow-subscription-shared-write"
version = "0.1.0"
edition = "2021"
publish = false
[workspace]
[dependencies]
serde = { version = "1", features = ["derive"] }
safeflow-billing-source-reader = { path = ${q(billingRoot)} }
safeflow-subscription-evidence = { path = ${q(repo)}, features = ["postgres-mock-store"] }
safeflow-storage = { path = ${q(path.join(foundation,'crates/storage'))} }
safeflow-contracts = { path = ${q(path.join(foundation,'crates/contracts'))} }
safeflow-transfer-integration = { path = ${q(path.join(foundation,'crates/transfer'))}, features = ["postgres-store"] }
sqlx = { version = "=0.7.4", default-features = false, features = ["runtime-tokio-rustls", "postgres", "json"] }
tokio = { version = "=1.53.2", features = ["macros", "rt-multi-thread"] }
serde_json = "1.0"
`;
fs.writeFileSync(path.join(project,'Cargo.toml'),manifest);fs.copyFileSync(path.join(foundation,'Cargo.lock'),path.join(project,'Cargo.lock'));
const docker=v['--docker']||'docker',cargo=v['--cargo']||'cargo',nonce=crypto.randomBytes(10).toString('hex');
const name='safeflow-subscription-write-'+nonce,database='safeflow_subscription_write_test';
const env={...process.env,CARGO_TARGET_DIR:target,CARGO_INCREMENTAL:'0',SAFEFLOW_SUBSCRIPTION_PROPOSED_SQL:path.join(repo,'sql/shared_runtime_storage_v1.proposed.sql'),SAFEFLOW_SUBSCRIPTION_DISCOVERY_FIXTURE:path.join(repo,'fixtures/discovery_ready_input.v1.json')};
env.SAFEFLOW_SUBSCRIPTION_DIRECT_BILLING_FIXTURE=path.join(foundation,'members/jonathan/fixtures/billing/consumption_pages.v1.json');
env.SAFEFLOW_SUBSCRIPTION_LEGACY_RETRY_FIXTURE=path.join(repo,'fixtures/legacy_reminder_three_failures.v1.json');
delete env.SAFEFLOW_SUBSCRIPTION_MOCK_DATABASE_URL;delete env.SAFEFLOW_SUBSCRIPTION_WRITE_TEST_URL;delete env.SAFEFLOW_SUBSCRIPTION_WRITE_SCHEMA;
env.SAFEFLOW_SUBSCRIPTION_QUERY_DIR=out;env.SAFEFLOW_SUBSCRIPTION_QUERY_EXE=path.join(target,'debug/examples/query_shared_subscription_reminders_mock'+(process.platform==='win32'?'.exe':''));
const password=crypto.randomBytes(32).toString('hex');env.POSTGRES_PASSWORD=password;
const report={task:'SUB-DIRECT-BILLING-READER-IDENTIFICATION',started_at:new Date().toISOString(),verified:false,
 foundation_label:v['--foundation-label'],foundation_root:foundation,foundation_source_hashes:referenceBefore,module_source_hashes:before,
 foundation_migration_files:migrations,foundation_source_or_public_migrations_modified:false,formal_codec_adopted:false,formal_subscription_integrated:false,production_subscription_business_write_executed:false,fictional_shared_storage_writes_executed:false,notification_sent:false,merchant_cancellation_confirmed:false,
 actual_database_executed:false,public_permit_transaction_executed:false,public_tool_policy_or_auth_connected:false,public_outbox_mapping_connected:false,
 funds_submit_or_real_bank_executed:false,online_release_enabled:false,checks:[],cleanup_completed:false};
let started=false;
function d(args){const r=spawnSync(docker,args,{env,encoding:'utf8',windowsHide:true,timeout:60000,maxBuffer:2e6});assert.ifError(r.error);assert.equal(r.status,0,'DOCKER_COMMAND_FAILED');return r.stdout.trim();}
function run(label,exe,args,extra={}){
 const r=spawnSync(exe,args,{cwd:project,env:{...env,...extra},encoding:'utf8',windowsHide:true,timeout:300000,maxBuffer:12e6});
 fs.writeFileSync(path.join(out,label+'.log'),((r.stdout||'')+'\n'+(r.stderr||'')).replaceAll(password,'[REDACTED]'));
 report.checks.push({label,exit_code:r.status});assert.ifError(r.error);assert.equal(r.status,0,label+' failed');console.log(label+': PASS');return r.stdout;
}
function registryPackages(file){return fs.readFileSync(file,'utf8').split('[[package]]').slice(1).flatMap(block=>{
 const name=/^name = "([^"]+)"/m.exec(block)?.[1],version=/^version = "([^"]+)"/m.exec(block)?.[1],source=/^source = "([^"]+)"/m.exec(block)?.[1];
 return source?[{name,version,source,key:JSON.stringify([name,version,source])}]:[];
});}
async function main(){try{
 const m=path.join(project,'Cargo.toml');
 run('format-generated',cargo,['fmt','--manifest-path',m]);
 report.generated_source_sha256=sha(fs.readFileSync(path.join(project,'src/main.rs')));
 // The copied input lockfile is updated ONLY in the generated project, offline.
 run('compile-against-actual-foundation',cargo,['check','--offline','--manifest-path',m]);
 const accepted=new Set([...registryPackages(path.join(foundation,'Cargo.lock')),...registryPackages(path.join(repo,'Cargo.lock'))].map(p=>p.key));
 const resolved=registryPackages(path.join(project,'Cargo.lock'));assert(resolved.every(p=>accepted.has(p.key)),'UNREVIEWED_REGISTRY_VERSION_SELECTED');
 report.generated_lockfile_sha256=sha(fs.readFileSync(path.join(project,'Cargo.lock')));report.registry_versions_from_existing_locks_only=true;report.registry_package_versions=resolved.map(({name,version,source})=>({name,version,source}));
 run('format',cargo,['fmt','--manifest-path',m,'--','--check']);
 run('clippy',cargo,['clippy','--locked','--offline','--manifest-path',m,'--all-targets','--','-D','warnings']);
 const tree=run('shared-sqlx-dependency-tree',cargo,['tree','--locked','--offline','--manifest-path',m,'-i','sqlx@0.7.4']);
 for(const crate of ['safeflow-subscription-evidence','safeflow-storage','safeflow-transfer-integration'])assert(tree.includes(crate),'REQUIRED_ACTUAL_DEPENDENCY_NOT_CONNECTED');
 const mm=path.join(repo,'Cargo.toml');
 run('format-module',cargo,['fmt','--manifest-path',mm,'--','--check']);
 run('clippy-module',cargo,['clippy','--locked','--offline','--manifest-path',mm,'--all-targets','--all-features','--','-D','warnings']);
 const directTests=run('billing-direct-tests',cargo,['test','--locked','--offline','--manifest-path',m]);
 const ordinary=run('ordinary-tests',cargo,['test','--locked','--offline','--manifest-path',mm,'--all-features']);
 const parser=run('query-parser-tests',cargo,['test','--locked','--offline','--manifest-path',mm,'--all-features','--example','query_subscription_runtime_mock']);
 const recentParser=run('recent-example-tests',cargo,['test','--locked','--offline','--manifest-path',mm,'--all-features','--example','prepare_shared_subscription_change_mock','--example','run_shared_subscription_workflow_mock']);
 const userParser=run('user-view-parser-tests',cargo,['test','--locked','--offline','--manifest-path',mm,'--all-features','--example','query_subscription_user_view_mock']);
 run('build-shared-workflow',cargo,['build','--locked','--offline','--manifest-path',mm,'--all-features','--example','run_shared_subscription_workflow_mock']);
 env.SAFEFLOW_SUBSCRIPTION_WORKFLOW_EXE=path.join(target,'debug/examples/run_shared_subscription_workflow_mock'+(process.platform==='win32'?'.exe':''));
 run('build-shared-query-example',cargo,['build','--locked','--offline','--manifest-path',mm,'--all-features','--example','query_shared_subscription_reminders_mock']);
 report.ordinary_tests_passed=[...((ordinary+'\n'+parser+'\n'+userParser+'\n'+recentParser+'\n'+directTests).matchAll(/test result: ok\. (\d+) passed;/g))].reduce((n,m)=>n+Number(m[1]),0);assert(report.ordinary_tests_passed>=240,'ORDINARY_TEST_COUNT_MISMATCH');
 run('build',cargo,['build','--locked','--offline','--manifest-path',m]);
 assert.equal(sha(fs.readFileSync(path.join(project,'src/main.rs'))),report.generated_source_sha256);
 report.docker_version=d(['info','--format','{{.ServerVersion}}']);report.postgres_image_id=d(['image','inspect','postgres:16-alpine','--format','{{.Id}}']);
 d(['run','--pull=never','--detach','--rm','--name',name,'--label','safeflow.subscription.write.check='+nonce,'--env','POSTGRES_PASSWORD','--env','POSTGRES_DB='+database,'--publish','127.0.0.1::5432','postgres:16-alpine']);started=true;
 let ready=false;for(let i=0;i<40;i++){
  const r=spawnSync(docker,['exec',name,'pg_isready','-U','postgres','-d',database],{env,encoding:'utf8',windowsHide:true,timeout:10000});if(r.status===0){ready=true;break;}await new Promise(r=>setTimeout(r,500));
 }assert(ready,'DATABASE_NOT_READY');
 const binding=/^127\.0\.0\.1:(\d+)$/.exec(d(['port',name,'5432/tcp']));assert(binding,'LOOPBACK_REQUIRED');
 const url=`postgres://postgres:${password}@127.0.0.1:${binding[1]}/${database}`;
 report.postgresql_version=d(['exec',name,'psql','-U','postgres','-d',database,'-Atc','SHOW server_version']);assert(/^16\./.test(report.postgresql_version));
 const binary=path.join(target,'debug/safeflow-subscription-shared-write'+(process.platform==='win32'?'.exe':''));report.probe_binary_sha256=sha(fs.readFileSync(binary));
 report.actual_database_executed=true;
 const data=JSON.parse(run('actual-foundation-ledger-write',binary,[],{SAFEFLOW_SUBSCRIPTION_WRITE_TEST_URL:url,SAFEFLOW_SUBSCRIPTION_WRITE_SCHEMA:'sub_write_check_'+nonce}));
 assert.equal(data.scope,'ACTUAL_FOUNDATION_PROPOSED_FICTIONAL_SUBSCRIPTION_WRITE');assert(data.verified&&data.actual_foundation_migrator_and_demo);
 for(const k of ['old_mock_schema_installed','public_permit_transaction_executed','formal_subscription_integrated','formal_codec_adopted','notification_sent','merchant_cancellation_confirmed'])assert.equal(data[k],false,k);
 assert.deepEqual(data.migration_versions,Array.from({length:21},(_,i)=>i+1));assert.equal(data.cases.length,140);assert.equal(new Set(data.cases.map(c=>c.case)).size,140);
 assert(data.cases.every(c=>c.verified&&c.actual_postgresql&&c.fictional&&c.foundation_rows_unchanged));
 report.actual_case_file=path.join(out,'actual-foundation-ledger-write.json');fs.writeFileSync(report.actual_case_file,JSON.stringify(data,null,2)+'\n');report.actual_case_sha256=sha(fs.readFileSync(report.actual_case_file));
 report.database_started_before=d(['inspect','--format','{{.State.StartedAt}}',name]);
 d(['restart','--time','5',name]);
 let resumed=false;for(let i=0;i<40;i++){
  const r=spawnSync(docker,['exec',name,'pg_isready','-U','postgres','-d',database],{env,encoding:'utf8',windowsHide:true,timeout:10000});if(r.status===0){resumed=true;break;}await new Promise(r=>setTimeout(r,500));
 }assert(resumed,'RESTART_DATABASE_NOT_READY');
 report.database_started_after=d(['inspect','--format','{{.State.StartedAt}}',name]);assert.notEqual(report.database_started_before,report.database_started_after);
 const resumedBinding=/^127\.0\.0\.1:(\d+)$/.exec(d(['port',name,'5432/tcp']));assert(resumedBinding,'RESTART_LOOPBACK_REQUIRED');
 report.restart_host_port_changed=resumedBinding[1]!==binding[1];
 const restartedUrl=`postgres://postgres:${password}@127.0.0.1:${resumedBinding[1]}/${database}`;
 let reachable=false;for(let i=0;i<40;i++){
  reachable=await new Promise(resolve=>{const socket=require('node:net').createConnection({host:'127.0.0.1',port:Number(resumedBinding[1])});let done=false;const finish=ok=>{if(done)return;done=true;socket.destroy();resolve(ok);};socket.setTimeout(500);socket.once('connect',()=>finish(true));socket.once('error',()=>finish(false));socket.once('timeout',()=>finish(false));});
  if(reachable)break;await new Promise(r=>setTimeout(r,500));
 }assert(reachable,'RESTART_HOST_PORT_NOT_READY');
 const restored=JSON.parse(run('after-actual-database-restart',binary,[],{SAFEFLOW_SUBSCRIPTION_WRITE_TEST_URL:restartedUrl,SAFEFLOW_SUBSCRIPTION_WRITE_SCHEMA:'sub_write_check_'+nonce,SAFEFLOW_SUBSCRIPTION_WRITE_PHASE:'after-restart'}));
 assert(restored.limits_restored&&restored.recent_shared_restored&&restored.verified&&restored.shared_business_queries_restored&&restored.initialization_restored&&restored.reminder_queries_restored&&restored.reminders_restored&&restored.workflow_restored&&restored.settings_restored&&restored.cancellation_restored&&restored.unknown_retained&&restored.pending_invalidated_explicitly&&restored.cleanup_completed&&!restored.notification_sent);report.database_restart_verified=true;report.restart_readback=restored;
 assert.equal(d(['exec',name,'psql','-U','postgres','-d',database,'-Atc',"SELECT count(*) FROM pg_namespace WHERE nspname LIKE 'sub_write_check_%'"]),'0');report.private_schema_removed=true;
 assert.deepEqual(hashes(repo,moduleFiles),before,'MODULE_SOURCE_CHANGED_DURING_TEST');assert.deepEqual(hashes(foundation,referenceFiles),referenceBefore,'FOUNDATION_SOURCE_CHANGED_DURING_TEST');
 report.foundation_source_not_modified=true;report.actual_checks=data.cases.length;report.fictional_shared_storage_writes_executed=true;report.verified=true;
}catch(e){report.error=e.code||(/^[A-Z0-9_]+$/.test(e.message)?e.message:'FOUNDATION_COMPAT_CHECK_FAILED');report.failed_check=report.checks.find(c=>c.exit_code!==0)?.label;process.exitCode=1;}
finally{
 if(started){try{assert.equal(d(['inspect','--format','{{index .Config.Labels "safeflow.subscription.write.check"}}',name]),nonce);d(['stop','--time','5',name]);report.cleanup_completed=d(['ps','-a','--filter','label=safeflow.subscription.write.check='+nonce,'--format','{{.ID}}'])==='';}catch{report.cleanup_completed=false;}}
 else report.cleanup_completed=true;
 if(!report.cleanup_completed){report.verified=false;process.exitCode=1;}
 report.finished_at=new Date().toISOString();const file=path.join(out,'verification.json');fs.writeFileSync(file,JSON.stringify(report,null,2)+'\n');
 console.log(JSON.stringify({verified:report.verified,actual_database_executed:report.actual_database_executed,actual_checks:report.actual_checks,ordinary_tests_passed:report.ordinary_tests_passed,cleanup_completed:report.cleanup_completed,failed_check:report.failed_check,error:report.error,verification_file:file}));
}}
main();
