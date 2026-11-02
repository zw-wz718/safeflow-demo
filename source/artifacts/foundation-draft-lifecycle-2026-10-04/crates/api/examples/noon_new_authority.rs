//! One approved successor cohort. Never updates old principals/runs/orders.
use serde::Deserialize;
use serde_json::{json,Value};
use sha2::{Digest,Sha256};
use sqlx::{postgres::PgConnectOptions,Connection,PgConnection,Postgres,Transaction};
use std::{io::{self,Read},str::FromStr};
use zeroize::Zeroizing;
#[path="noon_new_authority/permissions.rs"]
mod permissions;
#[path="noon_native_profiles/mod.rs"]
mod native_profiles;
#[path="noon_new_authority/sender.rs"]
mod sender;
#[path="noon_new_authority/runtime_creator.rs"]
mod runtime_creator;
type Error=Box<dyn std::error::Error>;
const DB:&str="billing_test_noon_acceptance_20261029";
const NS:&str="sf_unified_noon_acceptance";
const OWNER:&str="sf_noon_owner_20261029";
const OLD_RUN:&str="8bb1e389-1bb9-47ec-8ce4-bfe247b0bc06";
const NEW_RUN:&str="f0c0a930-8ded-4bc1-9ad0-9165ef7cdf30";
const PRODUCER:&str="sf_noon_producer_20261030";
const WORKER:&str="sf_noon_worker_20261030";
const UNTIL:&str="2026-11-11T00:00:00+08:00";
const SQL:&str=include_str!("../../storage/maintenance/0043_noon_new_authority.sql");
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {database_url:String,producer_password:Option<String>,worker_password:Option<String>,plan:Option<Value>}
fn require(ok:bool)->Result<(),Error>{if ok{Ok(())}else{Err("new authority rejected".into())}}
fn digest(v:&Value)->Result<String,Error>{Ok(format!("{:x}",Sha256::digest(serde_json::to_vec(v)?)))}
fn migration()->sqlx::migrate::Migration{sqlx::migrate::Migration::new(43,"noon new authority".into(),sqlx::migrate::MigrationType::Simple,SQL.replace("__SCHEMA__",NS).replace("\r\n","\n").into(),false)}

async fn snapshot(tx:&mut Transaction<'_,Postgres>)->Result<Value,Error>{
 let identity:Value=sqlx::query_scalar("SELECT jsonb_build_object('database',d.datname,'database_oid',d.oid::bigint,'owner',pg_get_userbyid(d.datdba),'schema',n.nspname,'schema_oid',n.oid::bigint,'schema_owner',pg_get_userbyid(n.nspowner),'user',current_user,'session',session_user,'superuser',r.rolsuper) FROM pg_database d JOIN pg_namespace n ON n.nspname=$1 JOIN pg_roles r ON r.rolname=current_user WHERE d.datname=current_database()")
 .bind(NS).fetch_one(&mut **tx).await?;
 require(identity["database"]==DB && identity["owner"]==OWNER && identity["schema_owner"]==OWNER && identity["user"]=="postgres" && identity["session"]=="postgres" && identity["superuser"]==true)?;
 let migrations:Value=sqlx::query_scalar("SELECT jsonb_agg(jsonb_build_object('version',version,'success',success,'checksum',encode(checksum,'hex')) ORDER BY version) FROM _sqlx_migrations WHERE version<=42").fetch_one(&mut **tx).await?;
 let ms=migrations.as_array().ok_or("migrations")?;require(ms.len()==42 && ms.iter().enumerate().all(|(i,m)|m["version"]==json!(i+1)&&m["success"]==true))?;
 require(migrations==serde_json::from_str::<Value>(include_str!("noon_authority_migrations.json"))?)?;
 let installed=existing(tx).await?;
 permissions::validate(tx,installed.is_some()).await?;
 let sender=sender::registered(tx,installed.as_ref().map(|r|&r["predecessor_snapshot"]["sender"])).await?;
 let catalog=permissions::catalog(tx).await?;
 let predecessor_catalog=if let Some(record)=&installed{
  require(record["post_catalog_sha256"]==catalog)?;
  record["predecessor_snapshot"]["authority_catalog_sha256"].as_str().ok_or("predecessor catalog")?.to_owned()
 }else{catalog};
 let source:Value=sqlx::query_scalar("SELECT to_jsonb(s)-'original_bytes' FROM public_unified_fictional_source_v1 s WHERE singleton=1 AND active AND source_id='safeflow-team-simulation' AND source_revision='safeflow-team-simulation-20261026-v1' AND implementation_revision='foundation-unified-fictional-v1' AND artifact_sha256='85b6fc1b3c708c617586afc3e8ea61a78e525d0811439f267c93620238e8c49d' AND encode(sha256(original_bytes),'hex')=artifact_sha256").fetch_one(&mut **tx).await?;
 let prior:Value=sqlx::query_scalar("SELECT jsonb_build_object('run',(SELECT to_jsonb(r) FROM payment_auto_runs_v1 r WHERE run_id=$1::uuid AND enabled AND producer_role='sf_noon_producer_20261029' AND worker_role='sf_noon_worker_20261029'),'principals',(SELECT jsonb_agg(to_jsonb(p) ORDER BY role_name) FROM payment_background_principals_v1 p WHERE role_name IN('sf_noon_producer_20261029','sf_noon_worker_20261029')),'fences',(SELECT jsonb_agg(to_jsonb(f) ORDER BY authority_id) FROM payment_completion_candidate_fences f WHERE authority_id IN('sf_noon_producer_20261029','sf_noon_worker_20261029')),'roles',(SELECT jsonb_agg(jsonb_build_object('oid',oid::bigint,'name',rolname,'login',rolcanlogin,'super',rolsuper,'inherit',rolinherit,'createdb',rolcreatedb,'createrole',rolcreaterole,'bypassrls',rolbypassrls,'replication',rolreplication,'valid_until',rolvaliduntil,'memberships',(SELECT count(*) FROM pg_auth_members WHERE member=pg_roles.oid OR roleid=pg_roles.oid),'config',rolconfig) ORDER BY rolname) FROM pg_roles WHERE rolname IN('sf_noon_producer_20261029','sf_noon_worker_20261029')),'completion',(SELECT to_jsonb(l) FROM payment_completion_logins_v1 l WHERE role_name='sf_noon_worker_20261029' AND writer_version='completion-final-v1'),'forward42',(SELECT jsonb_agg(to_jsonb(f) ORDER BY deployment_id) FROM public_card_final_forward42_v1 f),'deployment',(SELECT to_jsonb(d) FROM public_card_final_deployment_v1 d WHERE singleton=1 AND reader_version='card-final-v1' AND source_id='safeflow-team-simulation' AND implementation='foundation-unified-fictional-v1' AND database_oid=(SELECT oid FROM pg_database WHERE datname=current_database()) AND schema_oid=(SELECT oid FROM pg_namespace WHERE nspname=current_schema())))")
 .bind(OLD_RUN).fetch_one(&mut **tx).await?;
 require(!prior["run"].is_null() && !prior["completion"].is_null() && !prior["deployment"].is_null() && prior["forward42"].as_array().is_some_and(|a|a.len()==1))?;
 for (key,enabled) in [("principals","active"),("fences","enabled")]{
  let a=prior[key].as_array().ok_or("old authorities")?;require(a.len()==2 && a.iter().all(|p|p[enabled]==true&&p["epoch"]==1))?;
 }
 let expired:bool=sqlx::query_scalar("SELECT (SELECT count(*)=2 FROM payment_background_principals_v1 WHERE role_name IN('sf_noon_producer_20261029','sf_noon_worker_20261029') AND active AND epoch=1 AND source_id='safeflow-team-simulation' AND implementation='foundation-unified-fictional-v1' AND valid_until<clock_timestamp()) AND (SELECT count(*)=2 FROM payment_completion_candidate_fences WHERE authority_id IN('sf_noon_producer_20261029','sf_noon_worker_20261029') AND enabled AND epoch=1 AND valid_until<clock_timestamp()) AND NOT EXISTS(SELECT 1 FROM pg_database d CROSS JOIN LATERAL aclexplode(coalesce(d.datacl,acldefault('d',d.datdba))) a WHERE d.datname=current_database() AND a.grantee=0 AND a.privilege_type='TEMPORARY')").fetch_one(&mut **tx).await?;
 require(expired)?;
  // permissions::validate has checked the exact zero/one creator-edge shape
  // against registered OIDs. Retain the raw count in the approved snapshot.
  let roles=prior["roles"].as_array().ok_or("old roles")?;require(roles.len()==2&&roles.iter().all(|r|r["login"]==true&&r["super"]==false&&r["inherit"]==false&&r["createdb"]==false&&r["createrole"]==false&&r["bypassrls"]==false&&r["replication"]==false&&r["memberships"].as_u64().is_some_and(|n|n<=1)))?;
 let valid:bool=sqlx::query_scalar("SELECT public_card_final_expected_digest_v1(deployment_id,object_digest)=public_card_final_objects_v1() FROM public_card_final_deployment_v1 WHERE singleton=1").fetch_one(&mut **tx).await?;require(valid)?;
 let owners:Value=sqlx::query_scalar("SELECT jsonb_agg(jsonb_build_object('signature',p.oid::regprocedure::text,'oid',p.oid::bigint,'owner',pg_get_userbyid(p.proowner),'acl',p.proacl::text) ORDER BY p.oid) FROM pg_proc p WHERE p.oid IN('payment_background_original_v1(text,text)'::regprocedure,'payment_completion_authority_v1(text,boolean)'::regprocedure,'payment_auto_enroll_v1()'::regprocedure,'public_card_final_expected_digest_v1(uuid,text)'::regprocedure)").fetch_one(&mut **tx).await?;
 Ok(json!({"identity":identity,"migrations":migrations,"source":source,"predecessor":prior,"function_owners":owners,"sender":sender,"authority_catalog_sha256":predecessor_catalog}))
}
async fn existing(tx:&mut Transaction<'_,Postgres>)->Result<Option<Value>,Error>{
 let exists:bool=sqlx::query_scalar("SELECT to_regclass('payment_authority_continuations_v1') IS NOT NULL").fetch_one(&mut **tx).await?;
 if !exists{return Ok(None)}
 Ok(sqlx::query_scalar("SELECT to_jsonb(a) FROM payment_authority_continuations_v1 a WHERE singleton=1").fetch_optional(&mut **tx).await?)
}
async fn inspect(tx:&mut Transaction<'_,Postgres>)->Result<Value,Error>{
 permissions::validate(tx,true).await?;
 let live:bool=sqlx::query_scalar("SELECT payment_authority_window_v1()").fetch_one(&mut **tx).await?;require(live)?;
 let identities:Value=sqlx::query_scalar("SELECT jsonb_agg(jsonb_build_object('role',p.role_name,'kind',p.kind,'epoch',p.epoch,'active',p.active,'until',p.valid_until,'fence_epoch',f.epoch,'enabled',f.enabled,'fence_until',f.valid_until,'login',r.rolcanlogin,'super',r.rolsuper,'inherit',r.rolinherit,'create_db',r.rolcreatedb,'create_role',r.rolcreaterole,'replication',r.rolreplication,'bypassrls',r.rolbypassrls,'connection_limit',r.rolconnlimit,'role_until_matches',r.rolvaliduntil=p.valid_until,'config',r.rolconfig,'memberships',(SELECT count(*) FROM pg_auth_members WHERE member=r.oid OR roleid=r.oid)) ORDER BY p.role_name) FROM payment_background_principals_v1 p JOIN payment_completion_candidate_fences f ON f.kind=p.kind AND f.authority_id=p.role_name JOIN pg_roles r ON r.rolname=p.role_name WHERE p.role_name IN($1,$2)").bind(PRODUCER).bind(WORKER).fetch_one(&mut **tx).await?;
 let a=identities.as_array().ok_or("successors")?;require(a.len()==2)?;
 for r in a{require(r["epoch"]==1&&r["fence_epoch"]==1&&r["active"]==true&&r["enabled"]==true&&r["until"]==r["fence_until"]&&r["login"]==true&&r["super"]==false&&r["inherit"]==false&&r["create_db"]==false&&r["create_role"]==false&&r["replication"]==false&&r["bypassrls"]==false&&r["role_until_matches"]==true&&r["connection_limit"]==4&&r["memberships"]==0&&r["config"].is_null())?;}
 let bad:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace CROSS JOIN LATERAL aclexplode(c.relacl) a WHERE a.grantee=(SELECT oid FROM pg_roles WHERE rolname=$1) OR (a.grantee=(SELECT oid FROM pg_roles WHERE rolname=$2) AND (n.nspname<>current_schema() OR c.relname NOT IN('tasks','operations','public_execution_requests','payment_preparations','payment_funds_execution_v1','server_confirmations','execution_permits','mock_draft_lifecycle_plans','transfer_drafts_v1','transfer_business_orders_v1','transfer_fund_orders_v1','transfer_accounts_v1','transfer_ledger_v1','audit_events') OR a.privilege_type<>'SELECT' OR a.is_grantable))) OR has_database_privilege($1,current_database(),'CREATE,TEMP') OR has_database_privilege($2,current_database(),'CREATE,TEMP') OR has_schema_privilege($1,current_schema(),'CREATE') OR has_schema_privilege($2,current_schema(),'CREATE')").bind(PRODUCER).bind(WORKER).fetch_one(&mut **tx).await?;require(!bad)?;
 let exact:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM payment_auto_runs_v1 WHERE run_id=$1::uuid AND enabled AND producer_role=$2 AND worker_role=$3) AND EXISTS(SELECT 1 FROM payment_completion_logins_v1 WHERE role_name=$3 AND writer_version='completion-final-v1') AND (SELECT count(*)=1 FROM _sqlx_migrations WHERE version=43 AND success AND checksum=$4)").bind(NEW_RUN).bind(PRODUCER).bind(WORKER).bind(migration().checksum.as_ref()).fetch_one(&mut **tx).await?;require(exact)?;
 Ok(json!({"identities":identities,"new_run":NEW_RUN,"completion":"completion-final-v1","old_authorities_unchanged":true}))
}
async fn run()->Result<(),Error>{
 let args:Vec<String>=std::env::args().collect();require(args.len()==2&&matches!(args[1].as_str(),"check"|"plan"|"apply"))?;
 let mut raw=Zeroizing::new(String::new());io::stdin().take(65537).read_to_string(&mut raw)?;require(raw.len()<=65536)?;
 let input:Input=serde_json::from_str(&raw)?;drop(raw);
 let url=Zeroizing::new(input.database_url);let producer=Zeroizing::new(input.producer_password.unwrap_or_default());let worker=Zeroizing::new(input.worker_password.unwrap_or_default());
 let options=PgConnectOptions::from_str(&url)?;require(options.get_database()==Some(DB)&&options.get_username()=="postgres"&&matches!(options.get_host(),"127.0.0.1"|"/var/run/postgresql"))?;
 let mut conn=PgConnection::connect_with(&options).await?;let mut tx=conn.begin().await?;
 sqlx::raw_sql("SET LOCAL lock_timeout='5s';SET LOCAL statement_timeout='20s';SET LOCAL search_path=sf_unified_noon_acceptance,pg_catalog,pg_temp").execute(&mut *tx).await?;
 if args[1]=="apply"{
  sqlx::query("SELECT pg_advisory_xact_lock(20261030,4300)").execute(&mut *tx).await?;
  sqlx::raw_sql("LOCK TABLE _sqlx_migrations,payment_auto_runs_v1,payment_completion_candidate_fences,payment_background_principals_v1,public_card_final_deployment_v1 IN SHARE ROW EXCLUSIVE MODE").execute(&mut *tx).await?;
 }else{sqlx::raw_sql("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await?;}
 if args[1]=="apply"{
  permissions::lock_rls(&mut tx).await?;
 }
 let s=snapshot(&mut tx).await?;let old=existing(&mut tx).await?;
 let max:i64=sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations").fetch_one(&mut *tx).await?;require(max==if old.is_some(){43}else{42})?;
 let approval=format!("{:x}",Sha256::digest(b"NEXT_NOON_NEW_AUTHORITY_CONTINUATION_2026-10-30:USER_APPROVED:NEW_POST_EFFECTIVE_BUSINESS_ONLY:2026-11-11T00:00:00+08:00:NO_OLD_REVIVAL_OR_MIGRATION"));
 let plan=json!({"format":"SAFEFLOW_NOON_NEW_AUTHORITY_PLAN_V1","predecessor":s,"approval_sha256":approval,"new_run":NEW_RUN,"producer":PRODUCER,"worker":WORKER,"starts":"AT_ATOMIC_APPLY","until":UNTIL,"epoch":1,"migration43_checksum":format!("{:x}",sha2::Sha384::digest(migration().sql.as_bytes())),"excludes":"ALL_PRE_EFFECTIVE_TASKS_CONFIRMATIONS_CODES_GRANTS_AND_OLD_ORIGINALS"});
 let hash=digest(&plan)?;let before=plan["predecessor"].clone();
 let output=if args[1]=="apply"{
  require(input.plan.as_ref()==Some(&plan))?;
  if let Some(old)=old{
   require(old["plan_sha256"]==hash&&old["predecessor_snapshot"]==before&&old["approval_sha256"]==approval)?;
   json!({"applied":false,"idempotent":true,"authorization":old,"inspection":inspect(&mut tx).await?})
  }else{
   for p in [&producer,&worker]{require((40..=200).contains(&p.len())&&p.bytes().all(|b|b.is_ascii_alphanumeric()))?;}
   require(producer.as_str()!=worker.as_str())?;
   let clear:bool=sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname IN($1,$2)) AND NOT EXISTS(SELECT 1 FROM payment_auto_runs_v1 WHERE run_id=$3::uuid) AND clock_timestamp()<$4::timestamptz").bind(PRODUCER).bind(WORKER).bind(NEW_RUN).bind(UNTIL).fetch_one(&mut *tx).await?;require(clear)?;
   let from:String=sqlx::query_scalar("SELECT public_card_final_objects_v1()").fetch_one(&mut *tx).await?;
   let m=migration();sqlx::raw_sql(&m.sql).execute(&mut *tx).await?;
   // New functions/tables belong to the existing non-login resource owner.
   for table in ["payment_authority_continuations_v1","payment_authority_accepted_tasks_v1","public_card_final_forward43_v1"]{sqlx::query(&format!("ALTER TABLE {NS}.{table} OWNER TO {OWNER}")).execute(&mut *tx).await?;}
   for f in ["payment_authority_window_v1()","payment_authority_task_accept_v1()","payment_authority_accept_guard_v1()","payment_authority_original_guard_v1(text,text)"]{sqlx::query(&format!("ALTER FUNCTION {NS}.{f} OWNER TO {OWNER}")).execute(&mut *tx).await?;}
   let owners:Vec<String>=sqlx::query_scalar("SELECT DISTINCT pg_get_userbyid(proowner)::text FROM pg_proc WHERE oid IN('payment_background_original_v1(text,text)'::regprocedure,'payment_completion_authority_v1(text,boolean)'::regprocedure,'payment_auto_enroll_v1()'::regprocedure,'public_card_final_expected_digest_v1(uuid,text)'::regprocedure)").fetch_all(&mut *tx).await?;
   for owner in owners{
    require(owner.len()<=63&&owner.starts_with("sf_")&&owner.bytes().all(|b|b.is_ascii_lowercase()||b.is_ascii_digit()||b==b'_'))?;
    let no_login:bool=sqlx::query_scalar("SELECT NOT rolcanlogin AND NOT rolsuper AND NOT rolcreatedb AND NOT rolcreaterole AND NOT rolbypassrls FROM pg_roles WHERE rolname=$1").bind(&owner).fetch_one(&mut *tx).await?;require(no_login)?;
    sqlx::raw_sql(&format!("GRANT EXECUTE ON FUNCTION {NS}.payment_authority_original_guard_v1(text,text),{NS}.payment_authority_window_v1() TO {owner};GRANT SELECT ON {NS}.payment_authority_continuations_v1,{NS}.public_card_final_forward43_v1 TO {owner}")).execute(&mut *tx).await?;
   }
   sqlx::raw_sql("SET LOCAL log_statement='none';SET LOCAL log_min_error_statement='panic';SET LOCAL password_encryption='scram-sha-256'").execute(&mut *tx).await?;
   for (role,kind,password) in [(PRODUCER,"PRODUCER",producer.as_str()),(WORKER,"WORKER",worker.as_str())]{
    sqlx::query(&format!("CREATE ROLE {role} LOGIN NOINHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS CONNECTION LIMIT 4 PASSWORD '{password}' VALID UNTIL '{UNTIL}'")).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO payment_completion_candidate_fences(kind,authority_id,epoch,enabled,valid_until,scope,adoption_ref) VALUES($1,$2,1,true,$3::timestamptz,'{\"accounts\":[{\"owner_id\":\"user_001\",\"account_id\":\"account_001\"}]}','sf-noon-new-authority-20261030-v1')").bind(kind).bind(role).bind(UNTIL).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO payment_background_principals_v1(role_name,kind,source_id,implementation,epoch,active,valid_until) VALUES($1,$2,'safeflow-team-simulation','foundation-unified-fictional-v1',1,true,$3::timestamptz)").bind(role).bind(kind).bind(UNTIL).execute(&mut *tx).await?;
    let function=if kind=="PRODUCER"{"payment_background_decide_v1(text,text,text)"}else{"payment_background_worker_v1(text)"};
    sqlx::raw_sql(&format!("GRANT CONNECT ON DATABASE {DB} TO {role};GRANT USAGE ON SCHEMA {NS} TO {role};GRANT EXECUTE ON FUNCTION {NS}.{function},{NS}.payment_background_identity_v1(),{NS}.payment_auto_scan_v1(uuid,text,integer) TO {role}")).execute(&mut *tx).await?;
   }
   sqlx::query("INSERT INTO payment_completion_logins_v1(role_name,writer_version) VALUES($1,'completion-final-v1')").bind(WORKER).execute(&mut *tx).await?;
   for table in ["tasks","operations","public_execution_requests","payment_preparations","payment_funds_execution_v1","server_confirmations","execution_permits","mock_draft_lifecycle_plans","transfer_drafts_v1","transfer_business_orders_v1","transfer_fund_orders_v1","transfer_accounts_v1","transfer_ledger_v1","audit_events"]{
    sqlx::query(&format!("GRANT SELECT ON {NS}.{table} TO {WORKER}")).execute(&mut *tx).await?;
   }
   sqlx::raw_sql(&format!("GRANT UPDATE(booked_cents) ON {NS}.transfer_accounts_v1 TO {WORKER};GRANT UPDATE(state) ON {NS}.transfer_fund_orders_v1 TO {WORKER};GRANT UPDATE(latest_receipt,latest_recorded_at) ON {NS}.transfer_business_orders_v1 TO {WORKER};GRANT INSERT(business_owner_id,operation_id,account_id,direction,amount_cents,completed_at) ON {NS}.transfer_ledger_v1 TO {WORKER};GRANT EXECUTE ON FUNCTION {NS}.payment_completion_begin_v1(text,bigint),{NS}.payment_completion_finish_v1(text,bigint),{NS}.payment_completion_recover_v1(text),{NS}.payment_completion_outgoing_v1(text),{NS}.payment_completion_final_guard_v1(text,bigint) TO {WORKER}")).execute(&mut *tx).await?;
   sqlx::query("INSERT INTO payment_auto_runs_v1(run_id,producer_role,worker_role) VALUES($1::uuid,$2,$3)").bind(NEW_RUN).bind(PRODUCER).bind(WORKER).execute(&mut *tx).await?;
   let post_catalog=permissions::catalog(&mut tx).await?;
   sqlx::query("INSERT INTO payment_authority_continuations_v1(singleton,predecessor_run,run_id,producer_role,worker_role,epoch,starts_at,ends_at,approval_sha256,plan_sha256,predecessor_snapshot,post_catalog_sha256,applied_by) VALUES(1,$1::uuid,$2::uuid,$3,$4,1,clock_timestamp(),$5::timestamptz,$6,$7,$8,$9,'postgres')").bind(OLD_RUN).bind(NEW_RUN).bind(PRODUCER).bind(WORKER).bind(UNTIL).bind(&approval).bind(&hash).bind(&before).bind(&post_catalog).execute(&mut *tx).await?;
   sqlx::query("INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time) VALUES(43,'noon new authority',true,$1,0)").bind(m.checksum.as_ref()).execute(&mut *tx).await?;
   // Values do not affect the catalog digest. Record the new exact assembly,
   // chaining from verified42 without changing any old object_digest/forward.
   sqlx::query("INSERT INTO public_card_final_forward43_v1(deployment_id,original_digest,from_digest,to_digest,predecessor_version,predecessor_checksum,migration_version,migration_checksum) SELECT d.deployment_id,d.object_digest,$1,public_card_final_objects_v1(),42,p.checksum,43,m.checksum FROM public_card_final_deployment_v1 d CROSS JOIN _sqlx_migrations p CROSS JOIN _sqlx_migrations m WHERE p.version=42 AND m.version=43 AND p.success AND m.success").bind(&from).execute(&mut *tx).await?;
   require(snapshot(&mut tx).await?==before)?;
   json!({"applied":true,"idempotent":false,"authorization":existing(&mut tx).await?,"inspection":inspect(&mut tx).await?})
  }
 }else{
  require(input.plan.is_none()&&producer.is_empty()&&worker.is_empty())?;
  if args[1]=="plan"{require(old.is_none())?;plan}else{json!({"check":true,"plan":plan,"existing":old,"inspection":if old.is_some(){Some(inspect(&mut tx).await?)}else{None}})}
 };
 tx.commit().await?;println!("{}",serde_json::to_string(&output)?);conn.close().await?;Ok(())
}
#[tokio::main]
async fn main(){if let Err(e)=run().await{let code=e.downcast_ref::<sqlx::Error>().and_then(|e|e.as_database_error()).and_then(|e|e.code());let code=code.as_deref().filter(|s|s.len()==5&&s.bytes().all(|b|b.is_ascii_alphanumeric())).unwrap_or("CHECK");eprintln!("NOON_NEW_AUTHORITY_REJECTED; state={code}; no SQL/credentials emitted");std::process::exit(1)}}
