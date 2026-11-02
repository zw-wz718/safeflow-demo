'use strict';
// Offline ONLY: compiles existing acceptance harness but never runs database/Docker commands.
// No patching its migrator/authentication, public numbering, or source lockfile.
const fs=require('node:fs'),path=require('node:path'),crypto=require('node:crypto'),assert=require('node:assert/strict');
const {spawnSync}=require('node:child_process');
const repo=path.resolve(__dirname,'..'),v={};
for(let i=2;i<process.argv.length;i++){
 const k=process.argv[i];assert(['--foundation-root','--foundation-label','--output-root','--cargo'].includes(k)&&!v[k]&&process.argv[i+1],'INVALID_ARGUMENT');v[k]=process.argv[++i];
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
const moduleFiles=[...collect(repo,'src'),...collect(repo,'tests'),...collect(repo,'examples'),...collect(repo,'tools'),...collect(repo,'sql'),'Cargo.toml','Cargo.lock','fixtures/discovery_ready_input.v1.json'];
const referenceFiles=['Cargo.toml','Cargo.lock',...collect(foundation,'crates'),...collect(foundation,'features/transfer')];
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
fs.writeFileSync(path.join(project,'src/main.rs'),generated+'\n'+fs.readFileSync(path.join(repo,extraTemplate),'utf8')+'\n'+fs.readFileSync(path.join(repo,reminderTemplate),'utf8')+'\n'+fs.readFileSync(path.join(repo,queryTemplate),'utf8')+'\n'+fs.readFileSync(path.join(repo,initializationTemplate),'utf8')+'\n'+fs.readFileSync(path.join(repo,businessQueryTemplate),'utf8'));
const q=p=>JSON.stringify(p.replaceAll('\\','/'));
const manifest=`[package]
name = "safeflow-subscription-shared-write"
version = "0.1.0"
edition = "2021"
publish = false
[workspace]
[dependencies]
safeflow-subscription-evidence = { path = ${q(repo)}, features = ["postgres-mock-store"] }
safeflow-storage = { path = ${q(path.join(foundation,'crates/storage'))} }
safeflow-contracts = { path = ${q(path.join(foundation,'crates/contracts'))} }
safeflow-transfer-integration = { path = ${q(path.join(foundation,'crates/transfer'))}, features = ["postgres-store"] }
sqlx = { version = "=0.7.4", default-features = false, features = ["runtime-tokio-rustls", "postgres", "json"] }
tokio = { version = "=1.53.2", features = ["macros", "rt-multi-thread"] }
serde_json = "1.0"
`;
fs.writeFileSync(path.join(project,'Cargo.toml'),manifest);fs.copyFileSync(path.join(foundation,'Cargo.lock'),path.join(project,'Cargo.lock'));
const cargo=v['--cargo']||'cargo';
const env={...process.env,CARGO_TARGET_DIR:target,CARGO_INCREMENTAL:'0',SAFEFLOW_SUBSCRIPTION_PROPOSED_SQL:path.join(repo,'sql/shared_runtime_storage_v1.proposed.sql'),SAFEFLOW_SUBSCRIPTION_DISCOVERY_FIXTURE:path.join(repo,'fixtures/discovery_ready_input.v1.json')};
delete env.SAFEFLOW_SUBSCRIPTION_MOCK_DATABASE_URL;delete env.SAFEFLOW_SUBSCRIPTION_WRITE_TEST_URL;delete env.SAFEFLOW_SUBSCRIPTION_WRITE_SCHEMA;
for(const key of Object.keys(env)) if(/^SAFEFLOW.*(?:DATABASE_URL|QUERY_URL|WRITE_TEST_URL)$/.test(key)) delete env[key];
env.SQLX_OFFLINE='true';
env.SAFEFLOW_SUBSCRIPTION_QUERY_DIR=out;env.SAFEFLOW_SUBSCRIPTION_QUERY_EXE=path.join(target,'debug/examples/query_shared_subscription_reminders_mock'+(process.platform==='win32'?'.exe':''));
const password=crypto.randomBytes(32).toString('hex');env.POSTGRES_PASSWORD=password;
const report={task:'SUB-DISCOVERY-SHARED-WORKFLOW-OFFLINE',started_at:new Date().toISOString(),verified:false,
 foundation_label:v['--foundation-label'],foundation_root:foundation,foundation_source_hashes:referenceBefore,module_source_hashes:before,
 foundation_migration_files:migrations,foundation_source_or_public_migrations_modified:false,formal_codec_adopted:false,formal_subscription_integrated:false,production_subscription_business_write_executed:false,fictional_shared_storage_writes_executed:false,notification_sent:false,merchant_cancellation_confirmed:false,
 actual_database_executed:false,public_permit_transaction_executed:false,public_tool_policy_or_auth_connected:false,public_outbox_mapping_connected:false,
 funds_submit_or_real_bank_executed:false,online_release_enabled:false,checks:[],cleanup_completed:false};

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
 const ordinary=run('ordinary-tests',cargo,['test','--locked','--offline','--manifest-path',mm,'--all-features']);
 const parser=run('query-parser-tests',cargo,['test','--locked','--offline','--manifest-path',mm,'--all-features','--example','query_subscription_runtime_mock']);
 const userParser=run('user-view-parser-tests',cargo,['test','--locked','--offline','--manifest-path',mm,'--all-features','--example','query_subscription_user_view_mock']);
 run('build-discovery-example',cargo,['build','--locked','--offline','--manifest-path',mm,'--all-features','--example','prepare_shared_discovery_mock']);
 run('build-shared-query-example',cargo,['build','--locked','--offline','--manifest-path',mm,'--all-features','--example','query_shared_subscription_reminders_mock']);
 report.ordinary_tests_passed=[...((ordinary+'\n'+parser+'\n'+userParser).matchAll(/test result: ok\. (\d+) passed;/g))].reduce((n,m)=>n+Number(m[1]),0);assert(report.ordinary_tests_passed>=240,'ORDINARY_TEST_COUNT_MISMATCH');
 run('build',cargo,['build','--locked','--offline','--manifest-path',m]);
 assert.equal(sha(fs.readFileSync(path.join(project,'src/main.rs'))),report.generated_source_sha256);
 assert.deepEqual(hashes(repo,moduleFiles),before,'MODULE_SOURCE_CHANGED_DURING_TEST');
 assert.deepEqual(hashes(foundation,referenceFiles),referenceBefore,'FOUNDATION_SOURCE_CHANGED_DURING_TEST');
 report.foundation_source_not_modified=true;
 report.offline_verified=true;report.database_tests_deferred=true;report.database_restart_verified=false;
 report.actual_checks=0;report.existing_database_probe_cases_deferred=111;
 report.new_database_acceptance_deferred=['recognition_and_date_shared_read','selected_draft_rollback_and_commit_readback','stale_selection_after_concurrent_write','same_snapshot_details','process_and_database_restart_readback'];
}catch(e){report.error=e.code||(/^[A-Z0-9_]+$/.test(e.message)?e.message:'OFFLINE_CHECK_FAILED');report.failed_check=report.checks.find(c=>c.exit_code!==0)?.label;process.exitCode=1;}
finally{
 report.cleanup_completed=true;
 report.finished_at=new Date().toISOString();const file=path.join(out,'verification.json');fs.writeFileSync(file,JSON.stringify(report,null,2)+'\n');
 console.log(JSON.stringify({offline_verified:report.offline_verified===true,verified:report.verified,actual_database_executed:false,actual_checks:0,ordinary_tests_passed:report.ordinary_tests_passed,failed_check:report.failed_check,error:report.error,verification_file:file}));
}}
main();
