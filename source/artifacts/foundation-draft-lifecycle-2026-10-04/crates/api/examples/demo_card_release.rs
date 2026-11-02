//! Fixed organizer's single explicit release; no HTTP or model tool.
//! stdin contains the private administrator DSN; never print SQL or secrets.
use serde::Deserialize;
use serde_json::{json,Value};
use sha2::{Digest,Sha256};
use sqlx::{Connection,PgConnection,Postgres,Transaction};
use std::io::{self,Read};
use zeroize::Zeroizing;
type Error=Box<dyn std::error::Error>;
const NS:&str="sf_unified_noon_acceptance";
const DB:&str="billing_test_noon_acceptance_20261029";
const ID:&str="sf-card-release-20261101-user001-v1";
const TASK:&str="admin_card_release_20261101_user001_v1";
const CARD:&str="demo_card_salary_v1";
const APPROVAL:&str="NEXT_DEMO_CARD_UNFREEZE_RISK_CHECK_2026-11-01:USER_REQUESTED:TAIL6628:USER_SELF_ONLY:ONCE";
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {database_url:String,plan:Option<Value>,approval:Option<String>}
fn require(v:bool)->Result<(),Error>{if v{Ok(())}else{Err("demo card release rejected".into())}}
fn digest(v:&Value)->Result<String,Error>{Ok(format!("{:x}",Sha256::digest(serde_json::to_vec(v)?)))}
fn migration()->sqlx::migrate::Migration{sqlx::migrate::Migration::new(45,"organizer demo card release".into(),sqlx::migrate::MigrationType::Simple,include_str!("../../storage/maintenance/0045_demo_card_release.sql").replace("__SCHEMA__",NS).replace("\r\n","\n").into(),false)}
fn m44()->sqlx::migrate::Migration{sqlx::migrate::Migration::new(44,"simulation balance and draft intent".into(),sqlx::migrate::MigrationType::Simple,include_str!("../../storage/maintenance/0044_simulation_balance.sql").replace("__SCHEMA__",NS).replace("\r\n","\n").into(),false)}
async fn identity(c:&mut PgConnection)->Result<(),Error>{
 let good:bool=sqlx::query_scalar("SELECT current_database()=$1 AND current_schema()=$2 AND current_user='postgres' AND session_user='postgres' AND (SELECT rolsuper FROM pg_roles WHERE rolname=current_user) AND EXISTS(SELECT 1 FROM public_unified_fictional_source_v1 WHERE singleton=1 AND active AND source_id='safeflow-team-simulation' AND source_revision='safeflow-team-simulation-20261026-v1' AND implementation_revision='foundation-unified-fictional-v1' AND artifact_sha256='85b6fc1b3c708c617586afc3e8ea61a78e525d0811439f267c93620238e8c49d' AND encode(sha256(original_bytes),'hex')=artifact_sha256)")
 .bind(DB).bind(NS).fetch_one(&mut *c).await?;require(good)?;
 let mut expected:Value=serde_json::from_str(include_str!("noon_authority_migrations.json"))?;
 let m43=sqlx::migrate::Migration::new(43,"noon new authority".into(),sqlx::migrate::MigrationType::Simple,include_str!("../../storage/maintenance/0043_noon_new_authority.sql").replace("__SCHEMA__",NS).replace("\r\n","\n").into(),false);
 expected.as_array_mut().ok_or("versions")?.push(json!({"version":43,"success":true,"checksum":format!("{:x}",sha2::Sha384::digest(m43.sql.as_bytes()))}));
 let actual:Value=sqlx::query_scalar("SELECT jsonb_agg(jsonb_build_object('version',version,'success',success,'checksum',encode(checksum,'hex')) ORDER BY version) FROM _sqlx_migrations WHERE version<=43").fetch_one(&mut *c).await?;
 require(actual==expected)?;
 let good:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM payment_authority_continuations_v1 WHERE singleton=1 AND run_id='f0c0a930-8ded-4bc1-9ad0-9165ef7cdf30' AND ends_at='2026-11-11T00:00:00+08:00'::timestamptz) AND EXISTS(SELECT 1 FROM public_card_final_deployment_v1 WHERE public_card_final_expected_digest_v1(deployment_id,object_digest)=public_card_final_objects_v1())").fetch_one(c).await?;require(good)
}

async fn verify(c:&mut PgConnection,version:i64)->Result<(),Error>{
 identity(c).await?;
 let max:i64=sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations").fetch_one(&mut *c).await?;require(max==version)?;
 let checksum:Vec<u8>=sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version=44 AND success").fetch_one(&mut *c).await?;require(checksum==m44().checksum.as_ref())?;
 if version==45 {let checksum:Vec<u8>=sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version=45 AND success").fetch_one(&mut *c).await?;require(checksum==migration().checksum.as_ref())?;}
 Ok(())
}
async fn protected(c:&mut PgConnection)->Result<Value,Error>{
 let tables:Vec<String>=sqlx::query_scalar("SELECT tablename::text FROM pg_tables WHERE schemaname=current_schema() AND tablename NOT IN('_sqlx_migrations','demo_card_releases45_v1','public_card_final_forward45_v1') ORDER BY tablename").fetch_all(&mut *c).await?;
 let mut hashes=serde_json::Map::new();
 for t in tables {require(t.bytes().all(|b|b.is_ascii_lowercase()||b.is_ascii_digit()||b==b'_'))?;
 let hash:String=sqlx::query_scalar(&format!("SELECT encode(sha256(convert_to(coalesce(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text),'[]'::jsonb)::text,'UTF8')),'hex') FROM {t} t")).fetch_one(&mut *c).await?;hashes.insert(t,json!(hash));}
 Ok(Value::Object(hashes))
}
async fn upgrade_plan(c:&mut PgConnection)->Result<Value,Error>{
 verify(c,44).await?;
 let objects:String=sqlx::query_scalar("SELECT public_card_final_objects_v1()").fetch_one(&mut *c).await?;
 let catalog:Value=sqlx::query_scalar("SELECT jsonb_build_object('roles',(SELECT jsonb_agg(to_jsonb(r) ORDER BY oid) FROM pg_roles r WHERE rolname LIKE 'sf\\_%' ESCAPE '\\'),'acl',(SELECT jsonb_agg(jsonb_build_array(oid,relacl::text,relowner) ORDER BY oid) FROM pg_class WHERE relnamespace=(SELECT oid FROM pg_namespace WHERE nspname=current_schema())),'membership',(SELECT coalesce(jsonb_agg(to_jsonb(m) ORDER BY roleid,member),'[]') FROM pg_auth_members m))").fetch_one(&mut *c).await?;
 let readers:Vec<String>=sqlx::query_scalar("SELECT role_name FROM public_card_final_logins_v1 ORDER BY role_name").fetch_all(&mut *c).await?;
 Ok(json!({"schema":"DEMO_CARD_RELEASE_UPGRADE_PLAN_V1","database":DB,"namespace":NS,"predecessor":44,"version":45,"checksum":format!("{:x}",sha2::Sha384::digest(migration().sql.as_bytes())),"objects":objects,"permissions_digest":digest(&catalog)?,"readers":readers,"protected":protected(c).await?}))
}
async fn upgrade(tx:&mut Transaction<'_,Postgres>,expected:&Value)->Result<Value,Error>{
 let installed:bool=sqlx::query_scalar("SELECT to_regclass('public_card_final_forward45_v1') IS NOT NULL").fetch_one(&mut **tx).await?;
 if installed {verify(tx,45).await?;let old:Value=sqlx::query_scalar("SELECT upgrade_plan FROM public_card_final_forward45_v1").fetch_one(&mut **tx).await?;require(&old==expected)?;return Ok(json!({"upgraded":false,"version":45,"idempotent":true}))}
 let tables:Vec<String>=sqlx::query_scalar("SELECT tablename::text FROM pg_tables WHERE schemaname=current_schema() ORDER BY tablename").fetch_all(&mut **tx).await?;
 for t in &tables {require(t.bytes().all(|b|b.is_ascii_lowercase()||b.is_ascii_digit()||b==b'_'))?;}
 sqlx::raw_sql(&format!("LOCK TABLE {} IN ACCESS EXCLUSIVE MODE",tables.join(","))).execute(&mut **tx).await?;
 let plan=upgrade_plan(tx).await?;require(&plan==expected)?;
 let body:String=sqlx::query_scalar("SELECT prosrc FROM pg_proc WHERE oid='public_card_final_expected_digest_v1(uuid,text)'::regprocedure").fetch_one(&mut **tx).await?;
 let owner:String=sqlx::query_scalar("SELECT pg_get_userbyid(proowner)::text FROM pg_proc WHERE oid='public_card_final_expected_digest_v1(uuid,text)'::regprocedure").fetch_one(&mut **tx).await?;
 require(owner.starts_with("sf_")&&owner.bytes().all(|b|b.is_ascii_lowercase()||b.is_ascii_digit()||b==b'_'))?;
 let safe:bool=sqlx::query_scalar("SELECT NOT rolcanlogin AND NOT rolsuper AND NOT rolcreatedb AND NOT rolcreaterole AND NOT rolbypassrls FROM pg_roles WHERE rolname=$1").bind(&owner).fetch_one(&mut **tx).await?;require(safe)?;
 sqlx::raw_sql(&migration().sql).execute(&mut **tx).await?;
 // Replace ONLY the previously absolute history guard in the invoker member
 // function. The original migration and all its other clauses stay intact.
 let mut freeze:String=sqlx::query_scalar("SELECT pg_get_functiondef('card_save_freeze_v1(text,text,text,text,text,boolean,text,text,text,text,text,text,text,text,text,text)'::regprocedure)").fetch_one(&mut **tx).await?;
 let old="AND state='RELEASED')";require(freeze.matches(old).count()==1)?;
 freeze=freeze.replacen(old,"AND state='RELEASED') AND NOT (p_source='USER_SELF' AND card_fresh_self_after_release45_v1(p_card))",1);
 sqlx::raw_sql(&freeze).execute(&mut **tx).await?;
 let mut revision:String=sqlx::query_scalar("SELECT pg_get_functiondef('simulation_balance_revision44_v1()'::regprocedure)").fetch_one(&mut **tx).await?;
 require(revision.matches("IS DISTINCT FROM 44").count()==1)?;
 revision=revision.replacen("IS DISTINCT FROM 44","IS DISTINCT FROM 45",1);
 sqlx::raw_sql(&revision).execute(&mut **tx).await?;
 let next=format!("CREATE OR REPLACE FUNCTION public_card_final_expected_digest_v1(deployment UUID,original_digest TEXT) RETURNS TEXT LANGUAGE sql STABLE SECURITY DEFINER SET search_path TO pg_catalog,{NS},pg_temp AS $forward45$ SELECT coalesce((SELECT n.to_digest FROM public_card_final_forward45_v1 n JOIN _sqlx_migrations p ON p.version=44 AND p.success AND p.checksum=n.predecessor_checksum JOIN _sqlx_migrations m ON m.version=45 AND m.success AND m.checksum=n.migration_checksum WHERE n.deployment_id=$1 AND n.original_digest=$2 AND n.from_digest=previous.digest),previous.digest) FROM ({body}) previous(digest) $forward45$");
 sqlx::raw_sql(&next).execute(&mut **tx).await?;
 for t in ["demo_card_releases45_v1","public_card_final_forward45_v1"]{sqlx::raw_sql(&format!("ALTER TABLE {t} OWNER TO {owner}")).execute(&mut **tx).await?;}
 for f in ["demo_card_release_assert45_v1(text)","demo_card_release_complete45_v1()","card_fresh_self_after_release45_v1(text)"]{sqlx::raw_sql(&format!("ALTER FUNCTION {f} OWNER TO {owner}")).execute(&mut **tx).await?;}
 sqlx::raw_sql(&format!("GRANT SELECT ON card_locks_v1,card_freeze_operations_v1,card_catalog_v1,card_account_versions_v1,transfer_accounts_v1,tasks,audit_events,public_audit_outbox TO {owner}")).execute(&mut **tx).await?;
 for r in plan["readers"].as_array().ok_or("readers")? {let role=r.as_str().ok_or("role")?;require(role.starts_with("sf_")&&role.bytes().all(|b|b.is_ascii_lowercase()||b.is_ascii_digit()||b==b'_'))?;sqlx::raw_sql(&format!("GRANT EXECUTE ON FUNCTION card_fresh_self_after_release45_v1(text) TO {role}")).execute(&mut **tx).await?;}
 let m=migration();sqlx::query("INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time) VALUES(45,'organizer demo card release',true,$1,0)").bind(m.checksum.as_ref()).execute(&mut **tx).await?;
 sqlx::query("INSERT INTO public_card_final_forward45_v1(deployment_id,original_digest,from_digest,to_digest,predecessor_checksum,migration_checksum,upgrade_plan) SELECT d.deployment_id,d.object_digest,$1,public_card_final_objects_v1(),p.checksum,m.checksum,$2 FROM public_card_final_deployment_v1 d CROSS JOIN _sqlx_migrations p CROSS JOIN _sqlx_migrations m WHERE p.version=44 AND m.version=45").bind(&plan["objects"].as_str().ok_or("objects")?).bind(&plan).execute(&mut **tx).await?;
 require(protected(tx).await?==plan["protected"])?;verify(tx,45).await?;
 Ok(json!({"upgraded":true,"version":45,"historical_rows_preserved":true}))
}
fn task_plan()->Result<Value,Error>{
 use safeflow_contracts::*;
 Ok(json!(AgentPlan::new(TaskId::parse(TASK)?,1,vec![PlanNode::UserDecision{id:NodeId::parse("organizer_release")?,depends_on:vec![]}])?))
}
async fn snapshot(c:&mut PgConnection)->Result<Value,Error>{
 verify(c,45).await?;
 // Releasing a card must not silently resume an already accepted/unknown
 // payment. Old unexecuted drafts/claims remain intact and acquire no authority.
 let pending:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM transfer_fund_orders_v1 WHERE payer_account_id='account_001' AND state IN('PROCESSING','SUBMITTED','UNKNOWN')) OR EXISTS(SELECT 1 FROM operations o JOIN tasks t USING(task_id) WHERE t.owner_id='user_001' AND o.state IN('SUBMITTED','PROCESSING','UNKNOWN'))").fetch_one(&mut *c).await?;require(!pending)?;
 let value:Value=sqlx::query_scalar("SELECT jsonb_build_object('database_oid',(SELECT oid::bigint FROM pg_database WHERE datname=current_database()),'schema_oid',(SELECT oid::bigint FROM pg_namespace WHERE nspname=current_schema()),'card',(SELECT to_jsonb(c) FROM card_catalog_v1 c WHERE card_id=$1),'owner',(SELECT owner_id FROM transfer_accounts_v1 WHERE account_id='account_001'),'version',(SELECT revision FROM card_account_versions_v1 WHERE account_id='account_001'),'locks',(SELECT coalesce(jsonb_agg(jsonb_build_object('row',to_jsonb(l),'xmin',l.xmin::text) ORDER BY lock_id),'[]') FROM card_locks_v1 l WHERE card_id=$1),'records',(SELECT coalesce(jsonb_agg(to_jsonb(f) ORDER BY record_id),'[]') FROM card_freeze_operations_v1 f WHERE card_id=$1),'results',(SELECT coalesce(jsonb_agg(to_jsonb(p) ORDER BY operation_id),'[]') FROM public_card_results_v1 p WHERE lock_id IN(SELECT lock_id FROM card_locks_v1 WHERE card_id=$1)))").bind(CARD).fetch_one(&mut *c).await?;
 require(value["card"]==json!({"card_id":CARD,"account_id":"account_001","last_four":"6628"})&&value["owner"]=="user_001")?;
 let locks=value["locks"].as_array().ok_or("locks")?;
 let active:Vec<_>=locks.iter().filter(|l|l["row"]["source"]=="USER_SELF"&&l["row"]["state"]=="ACTIVE").collect();require(active.len()==1)?;
 let lock=active[0]["row"]["lock_id"].as_str().ok_or("lock")?;
 // This card is demonstrably linked to a complete original public freeze.
 let ops:Vec<String>=sqlx::query_scalar("SELECT operation_id FROM public_card_results_v1 WHERE lock_id=$1 ORDER BY operation_id").bind(lock).fetch_all(&mut *c).await?;require(!ops.is_empty())?;
 for op in ops {sqlx::query("SELECT public_card_final_assert_v1($1)").bind(op).execute(&mut *c).await?;}
 let records:Vec<_>=value["records"].as_array().ok_or("records")?.iter().filter(|f|f["lock_id"]==lock&&f["source"]=="USER_SELF"&&f["owner_id"]=="user_001"&&f["account_id"]=="account_001").collect();require(!records.is_empty())?;
 Ok(json!({"schema":"DEMO_CARD_RELEASE_PLAN_V1","request_id":ID,"database":DB,"namespace":NS,"owner_id":"user_001","account_id":"account_001","card_id":CARD,"last_four":"6628","lock_id":lock,"record_id":records[0]["record_id"],"before_revision":value["version"],"expected_digest":digest(&value)?,"approval":APPROVAL,"task_plan":task_plan()?,"payment_authorized":false}))
}
async fn query(c:&mut PgConnection)->Result<Option<Value>,Error>{
 verify(c,45).await?;
 let old:Option<Value>=sqlx::query_scalar("SELECT to_jsonb(r) FROM demo_card_releases45_v1 r WHERE request_id=$1").bind(ID).fetch_optional(&mut *c).await?;
 if old.is_some(){sqlx::query("SELECT demo_card_release_assert45_v1($1)").bind(ID).execute(&mut *c).await?;}
 Ok(old)
}
async fn apply(tx:&mut Transaction<'_,Postgres>,expected:&Value,approval:&str)->Result<Value,Error>{
 verify(tx,45).await?;require(approval==APPROVAL)?;
 sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(current_schema() || $1,0))").bind(json!(["public-owner-execution-v1","user_001"]).to_string()).execute(&mut **tx).await?;
 let _:Vec<String>=sqlx::query_scalar("SELECT account_id FROM transfer_accounts_v1 WHERE owner_id='user_001' ORDER BY account_id FOR UPDATE").fetch_all(&mut **tx).await?;
 if let Some(old)=query(tx).await? {require(old["plan"]==*expected)?;return Ok(json!({"applied":false,"idempotent":true,"release":old}))}
 let plan=snapshot(tx).await?;require(&plan==expected)?;
 let before=plan["before_revision"].as_i64().ok_or("version")?;let after=before.checked_add(1).ok_or("overflow")?;
 let transaction:i64=sqlx::query_scalar("SELECT txid_current()").fetch_one(&mut **tx).await?;
 let event=format!("event_{}",uuid::Uuid::new_v4());
 let receipt=json!({"schema":"DEMO_CARD_USER_SELF_RELEASED_V1","request_id":ID,"owner_id":"user_001","account_id":"account_001","card_id":CARD,"lock_id":plan["lock_id"],"record_id":plan["record_id"],"before_revision":before,"after_revision":after,"transaction_id":transaction,"plan_digest":digest(&plan)?,"payment_authorized":false,"original_freeze_preserved":true});
 sqlx::query("INSERT INTO tasks(task_id,owner_id,contract_version,state,revision,plan_version,facts_version,plan,creation_digest,checkpoint,completed_nodes) VALUES($1,'user_001',1,'COMPLETED',1,1,1,$2,$3,'{\"kind\":\"FINISHED\"}','[\"organizer_release\"]')").bind(TASK).bind(&plan["task_plan"]).bind(digest(&plan)?).execute(&mut **tx).await?;
 sqlx::query("INSERT INTO audit_events(event_id,task_id,kind,task_revision,details) VALUES($1,$2,'CHECKPOINT_SAVED',1,$3)").bind(&event).bind(TASK).bind(&receipt).execute(&mut **tx).await?;
 sqlx::query("INSERT INTO demo_card_releases45_v1(request_id,owner_id,account_id,card_id,lock_id,record_id,before_revision,after_revision,transaction_id,plan,receipt,task_id,event_id) VALUES($1,'user_001','account_001',$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)").bind(ID).bind(CARD).bind(plan["lock_id"].as_str().ok_or("lock")?).bind(plan["record_id"].as_str().ok_or("record")?).bind(before).bind(after).bind(transaction).bind(&plan).bind(&receipt).bind(TASK).bind(event).execute(&mut **tx).await?;
 let n=sqlx::query("UPDATE card_locks_v1 SET state='RELEASED' WHERE lock_id=$1 AND card_id=$2 AND state='ACTIVE' AND source='USER_SELF'").bind(plan["lock_id"].as_str().ok_or("lock")?).bind(CARD).execute(&mut **tx).await?.rows_affected();require(n==1)?;
 let version:i64=sqlx::query_scalar("SELECT revision FROM card_account_versions_v1 WHERE account_id='account_001'").fetch_one(&mut **tx).await?;require(version==after)?;
 Ok(json!({"applied":true,"idempotent":false,"release":query(tx).await?}))
}
async fn run()->Result<(),Error>{
 let args:Vec<String>=std::env::args().collect();require(args.len()==2&&matches!(args[1].as_str(),"upgrade-plan"|"upgrade"|"plan"|"apply"|"query"))?;
 let mut raw=Zeroizing::new(String::new());io::stdin().take(131073).read_to_string(&mut raw)?;require(raw.len()<=131072)?;
 let input:Input=serde_json::from_str(&raw)?;drop(raw);let url=Zeroizing::new(input.database_url);
 let options=url.parse::<sqlx::postgres::PgConnectOptions>()?;let options=sqlx::ConnectOptions::disable_statement_logging(options);
 let mut c=PgConnection::connect_with(&options).await?;
 sqlx::raw_sql(&format!("SET search_path TO {NS},pg_catalog;SET statement_timeout='45s';SET lock_timeout='8s';SET log_statement='none';SET log_min_error_statement='panic'")).execute(&mut c).await?;
 let mut tx=c.begin().await?;
 if !matches!(args[1].as_str(),"upgrade"|"apply"){sqlx::raw_sql("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await?;require(input.plan.is_none()&&input.approval.is_none())?;}
 let value=match args[1].as_str(){"upgrade-plan"=>upgrade_plan(&mut tx).await?,"upgrade"=>{require(input.approval.is_none())?;upgrade(&mut tx,input.plan.as_ref().ok_or("plan")?).await?},"plan"=>snapshot(&mut tx).await?,"apply"=>apply(&mut tx,input.plan.as_ref().ok_or("plan")?,input.approval.as_deref().ok_or("approval")?).await?,_=>json!({"release":query(&mut tx).await?})};
 tx.commit().await?;c.close().await?;println!("{}",serde_json::to_string(&value)?);Ok(())
}
#[tokio::main]
async fn main(){if run().await.is_err(){eprintln!("DEMO_CARD_RELEASE_REJECTED; no SQL or credential details emitted; query original release after an uncertain response");std::process::exit(1)}}
