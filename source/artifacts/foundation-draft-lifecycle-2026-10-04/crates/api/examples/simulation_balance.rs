//! One native administrator action, no HTTP/model route. Secrets on stdin only.
use serde::Deserialize;
use serde_json::{json,Value};
use sha2::{Digest,Sha256};
use sqlx::{Connection,PgConnection,Postgres,Transaction};
use std::io::{self,Read};
use zeroize::Zeroizing;
type Error=Box<dyn std::error::Error>;
const NS:&str="sf_unified_noon_acceptance";
const DB:&str="billing_test_noon_acceptance_20261029";
const ID:&str="sf-million-20261031-user001-v1";
const TASK:&str="admin_balance_sf_million_20261031_user001_v1";
const TARGET:i64=100_000_000;
const RAW:&str=include_str!("../../storage/maintenance/0044_simulation_balance.sql");
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {database_url:String,plan:Option<Value>}
fn require(v:bool)->Result<(),Error>{if v{Ok(())}else{Err("simulation balance rejected".into())}}
fn digest(v:&Value)->Result<String,Error>{Ok(format!("{:x}",Sha256::digest(serde_json::to_vec(v)?)))}
fn migration()->sqlx::migrate::Migration{sqlx::migrate::Migration::new(44,"simulation balance and draft intent".into(),sqlx::migrate::MigrationType::Simple,RAW.replace("__SCHEMA__",NS).replace("\r\n","\n").into(),false)}
async fn installed(c:&mut PgConnection)->Result<bool,Error>{Ok(sqlx::query_scalar("SELECT to_regclass('simulation_balance_adjustments_v1') IS NOT NULL").fetch_one(c).await?)}
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
async fn protected(c:&mut PgConnection)->Result<Value,Error>{
 let tables:Vec<String>=sqlx::query_scalar("SELECT tablename::text FROM pg_tables WHERE schemaname=current_schema() AND tablename NOT IN('_sqlx_migrations','simulation_balance_adjustments_v1','public_card_final_forward44_v1','draft_intent_fences44_v1') ORDER BY tablename").fetch_all(&mut *c).await?;
 let mut hashes=serde_json::Map::new();
 for t in tables {
  require(t.bytes().all(|b|b.is_ascii_lowercase()||b.is_ascii_digit()||b==b'_'))?;
  let hash:String=sqlx::query_scalar(&format!("SELECT encode(sha256(convert_to(coalesce(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text),'[]'::jsonb)::text,'UTF8')),'hex') FROM {t} t")).fetch_one(&mut *c).await?;
  hashes.insert(t,json!(hash));
 }
 Ok(Value::Object(hashes))
}
async fn upgrade_plan(c:&mut PgConnection)->Result<Value,Error>{
 identity(c).await?;
 let max:i64=sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations").fetch_one(&mut *c).await?;require(max==43)?;
 let binding:String=sqlx::query_scalar("SELECT public_card_final_objects_v1()").fetch_one(&mut *c).await?;
 let roles:Vec<String>=sqlx::query_scalar("SELECT rolname::text FROM pg_roles WHERE rolname LIKE 'sf\\_%' ESCAPE '\\' AND NOT rolsuper AND has_schema_privilege(oid,current_schema(),'USAGE') AND has_table_privilege(oid,'transfer_accounts_v1','SELECT') ORDER BY rolname").fetch_all(&mut *c).await?;
 let catalog:Value=sqlx::query_scalar("SELECT jsonb_build_object('roles',(SELECT jsonb_agg(to_jsonb(r) ORDER BY oid) FROM pg_roles r WHERE rolname LIKE 'sf\\_%' ESCAPE '\\'),'acl',(SELECT jsonb_agg(jsonb_build_array(oid,relacl::text,relowner) ORDER BY oid) FROM pg_class WHERE relnamespace=(SELECT oid FROM pg_namespace WHERE nspname=current_schema())),'membership',(SELECT coalesce(jsonb_agg(to_jsonb(m) ORDER BY roleid,member),'[]') FROM pg_auth_members m))").fetch_one(&mut *c).await?;
 Ok(json!({"schema":"SIMULATION_BALANCE_UPGRADE_PLAN_V1","database":DB,"namespace":NS,"predecessor":43,"version":44,"checksum":format!("{:x}",sha2::Sha384::digest(migration().sql.as_bytes())),"objects":binding,"permissions_digest":digest(&catalog)?,"readers":roles,"protected":protected(c).await?}))
}
async fn verify44(c:&mut PgConnection)->Result<(),Error>{
 identity(c).await?;
 let checksum:Vec<u8>=sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version=44 AND success").fetch_one(&mut *c).await?;
 require(checksum==migration().checksum.as_ref())?;
 let max:i64=sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations").fetch_one(&mut *c).await?;require(max==44)?;
 Ok(())
}
async fn upgrade(tx:&mut Transaction<'_,Postgres>,expected:&Value)->Result<Value,Error>{
 if installed(tx).await? {
  verify44(tx).await?;
  let saved:Value=sqlx::query_scalar("SELECT upgrade_plan FROM public_card_final_forward44_v1").fetch_one(&mut **tx).await?;
  require(&saved==expected)?;return Ok(json!({"upgraded":false,"idempotent":true,"version":44}))
 }
 // Lock the whole schema's existing business tables for this short native
 // maintenance transaction. A live writer must quiesce; no re-provision/arm.
 let tables:Vec<String>=sqlx::query_scalar("SELECT tablename::text FROM pg_tables WHERE schemaname=current_schema() ORDER BY tablename").fetch_all(&mut **tx).await?;
 for t in &tables {require(t.bytes().all(|b|b.is_ascii_lowercase()||b.is_ascii_digit()||b==b'_'))?;}
 sqlx::raw_sql(&format!("LOCK TABLE {} IN ACCESS EXCLUSIVE MODE",tables.join(","))).execute(&mut **tx).await?;
 let plan=upgrade_plan(tx).await?;require(&plan==expected)?;
 let old:String=sqlx::query_scalar("SELECT public_card_final_objects_v1()").fetch_one(&mut **tx).await?;
 let def_owner:String=sqlx::query_scalar("SELECT pg_get_userbyid(proowner)::text FROM pg_proc WHERE oid='public_card_final_expected_digest_v1(uuid,text)'::regprocedure").fetch_one(&mut **tx).await?;
 require(def_owner.starts_with("sf_")&&def_owner.bytes().all(|b|b.is_ascii_lowercase()||b.is_ascii_digit()||b==b'_'))?;
 let safe:bool=sqlx::query_scalar("SELECT NOT rolcanlogin AND NOT rolsuper AND NOT rolcreatedb AND NOT rolcreaterole AND NOT rolbypassrls FROM pg_roles WHERE rolname=$1").bind(&def_owner).fetch_one(&mut **tx).await?;require(safe)?;
 sqlx::raw_sql(&migration().sql).execute(&mut **tx).await?;
 let raw43=include_str!("../../storage/maintenance/0043_noon_new_authority.sql").replace("__SCHEMA__",NS).replace("\r\n","\n");
 let oldbody=raw43.rsplit_once(" AS $$").ok_or("43 body")?.1.trim().strip_suffix("$$;").ok_or("43 suffix")?.trim();
 let next=format!("CREATE OR REPLACE FUNCTION public_card_final_expected_digest_v1(deployment UUID,original_digest TEXT) RETURNS TEXT LANGUAGE sql STABLE SECURITY DEFINER SET search_path TO pg_catalog,{NS},pg_temp AS $forward$ SELECT coalesce((SELECT n.to_digest FROM public_card_final_forward44_v1 n JOIN _sqlx_migrations p ON p.version=43 AND p.success AND p.checksum=n.predecessor_checksum JOIN _sqlx_migrations m ON m.version=44 AND m.success AND m.checksum=n.migration_checksum WHERE n.deployment_id=$1 AND n.original_digest=$2 AND n.from_digest=previous.digest),previous.digest) FROM ({oldbody}) previous(digest) $forward$");
 sqlx::raw_sql(&next).execute(&mut **tx).await?;
 for table in ["simulation_balance_adjustments_v1","public_card_final_forward44_v1","draft_intent_fences44_v1"]{sqlx::raw_sql(&format!("ALTER TABLE {table} OWNER TO {def_owner}")).execute(&mut **tx).await?;}
 for function in ["simulation_balance_net_v1(text)","simulation_balance_revision44_v1()","draft_intent_write_guard_v1()","draft_intent_end_fence44_v1()"]{sqlx::raw_sql(&format!("ALTER FUNCTION {function} OWNER TO {def_owner}")).execute(&mut **tx).await?;}
 for role in plan["readers"].as_array().ok_or("readers")? {
  let role=role.as_str().ok_or("role")?;require(role.starts_with("sf_")&&role.bytes().all(|b|b.is_ascii_lowercase()||b.is_ascii_digit()||b==b'_'))?;
  sqlx::raw_sql(&format!("GRANT SELECT ON simulation_balance_adjustments_v1,public_card_final_forward44_v1 TO {role};GRANT EXECUTE ON FUNCTION simulation_balance_net_v1(text),simulation_balance_revision44_v1() TO {role}")).execute(&mut **tx).await?;
 }
 // The NOLOGIN definer validates adjustment/audit/Outbox provenance, never writes funds.
 sqlx::raw_sql(&format!("GRANT SELECT ON tasks,audit_events,public_audit_outbox,_sqlx_migrations,public_card_final_deployment_v1 TO {def_owner};GRANT UPDATE(task_id) ON tasks TO {def_owner}")).execute(&mut **tx).await?;
 let actors:Vec<String>=sqlx::query_scalar("SELECT role_name FROM payment_background_principals_v1 WHERE active ORDER BY role_name").fetch_all(&mut **tx).await?;
 for role in actors {require(role.starts_with("sf_")&&role.bytes().all(|b|b.is_ascii_lowercase()||b.is_ascii_digit()||b==b'_'))?;sqlx::raw_sql(&format!("GRANT EXECUTE ON FUNCTION simulation_balance_revision44_v1() TO {role}")).execute(&mut **tx).await?;}
 let m=migration();sqlx::query("INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time) VALUES(44,'simulation balance and draft intent',true,$1,0)").bind(m.checksum.as_ref()).execute(&mut **tx).await?;
 sqlx::query("INSERT INTO public_card_final_forward44_v1(deployment_id,original_digest,from_digest,to_digest,predecessor_checksum,migration_checksum,upgrade_plan) SELECT d.deployment_id,d.object_digest,$1,public_card_final_objects_v1(),p.checksum,m.checksum,$2 FROM public_card_final_deployment_v1 d CROSS JOIN _sqlx_migrations p CROSS JOIN _sqlx_migrations m WHERE p.version=43 AND m.version=44").bind(old).bind(&plan).execute(&mut **tx).await?;
 require(protected(tx).await?==plan["protected"])?;verify44(tx).await?;
 Ok(json!({"upgraded":true,"version":44,"original43_preserved":true,"business_preserved":true}))
}
async fn snapshot(c:&mut PgConnection)->Result<Value,Error>{
 verify44(c).await?;
 let value:Value=sqlx::query_scalar("SELECT jsonb_build_object('database_oid',(SELECT oid::bigint FROM pg_database WHERE datname=current_database()),'schema_oid',(SELECT oid::bigint FROM pg_namespace WHERE nspname=current_schema()),'accounts',(SELECT jsonb_agg(jsonb_build_object('row',to_jsonb(a),'version',a.xmin::text) ORDER BY account_id) FROM transfer_accounts_v1 a),'funds',(SELECT coalesce(jsonb_agg(to_jsonb(f) ORDER BY owner_id,operation_id),'[]') FROM transfer_fund_orders_v1 f),'ledger',(SELECT coalesce(jsonb_agg(to_jsonb(l) ORDER BY transaction_id),'[]') FROM transfer_ledger_v1 l),'requests',(SELECT coalesce(jsonb_agg(to_jsonb(e) ORDER BY operation_id),'[]') FROM public_execution_requests e WHERE owner_id='user_001'),'adjustments',(SELECT coalesce(jsonb_agg(to_jsonb(s) ORDER BY adjustment_id),'[]') FROM simulation_balance_adjustments_v1 s))").fetch_one(&mut *c).await?;
 let funds=safeflow_transfer_integration::funds_store::FundsStore::funds(c,&safeflow_contracts::UserId::parse("user_001")?,&safeflow_contracts::ResourceId::parse("account_001")?).await?;
 let booked=funds.booked.as_str().replace('.',"").parse::<i64>()?;
 require(funds.reserved.as_str()=="0.00")?;
 let pending:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public_execution_requests r JOIN operations o USING(task_id,operation_id) WHERE r.owner_id='user_001' AND o.state IN('DRAFT','READY','SUBMITTED','PROCESSING')) OR EXISTS(SELECT 1 FROM transfer_fund_orders_v1 WHERE state='PROCESSING' AND (payer_account_id='account_001' OR recipient_account_id='account_001'))").fetch_one(c).await?;require(!pending)?;
 Ok(json!({"schema":"SIMULATION_BALANCE_TARGET_PLAN_V1","adjustment_id":ID,"owner_id":"user_001","account_id":"account_001","target_cents":TARGET,"before_cents":booked,"delta_cents":TARGET.checked_sub(booked).ok_or("overflow")?,"reserved_cents":0,"expected_digest":digest(&value)?,"currency":"CNY","source":"safeflow-team-simulation","payment_authorized":false}))
}
async fn query(c:&mut PgConnection)->Result<Option<Value>,Error>{
 verify44(c).await?;
 let row:Option<Value>=sqlx::query_scalar("SELECT to_jsonb(s) FROM simulation_balance_adjustments_v1 s WHERE adjustment_id=$1").bind(ID).fetch_optional(&mut *c).await?;
 if row.is_some(){let _:String=sqlx::query_scalar("SELECT simulation_balance_net_v1('account_001')::text").fetch_one(c).await?;}
 Ok(row)
}
async fn apply(tx:&mut Transaction<'_,Postgres>,expected:&Value)->Result<Value,Error>{
 verify44(tx).await?;
 // Same owner/advisory and sorted account lock order as final transfer writes.
 sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(current_schema() || $1,0))").bind(json!(["public-owner-execution-v1","user_001"]).to_string()).execute(&mut **tx).await?;
 let _:Vec<String>=sqlx::query_scalar("SELECT account_id FROM transfer_accounts_v1 ORDER BY account_id FOR UPDATE").fetch_all(&mut **tx).await?;
 if let Some(old)=query(tx).await? {require(old["plan"]==*expected)?;return Ok(json!({"applied":false,"idempotent":true,"adjustment":old}))}
 let plan=snapshot(tx).await?;require(&plan==expected)?;
 let node=safeflow_contracts::NodeId::parse("administrative_receipt")?;
 let task=safeflow_contracts::TaskId::parse(TASK)?;
 let plan_record=safeflow_contracts::AgentPlan::new(task.clone(),1,vec![safeflow_contracts::PlanNode::UserDecision{id:node.clone(),depends_on:vec![]}])?;
 let event=format!("event_{}",uuid::Uuid::new_v4());
 let receipt=json!({"schema":"SIMULATION_BALANCE_ADJUSTED_V1","adjustment_id":ID,"owner_id":"user_001","account_id":"account_001","before_cents":plan["before_cents"],"target_cents":TARGET,"delta_cents":plan["delta_cents"],"expected_digest":plan["expected_digest"],"currency":"CNY","payment_authorized":false,"original_opening_preserved":true});
 sqlx::query("INSERT INTO tasks(task_id,owner_id,contract_version,state,revision,plan_version,facts_version,plan,creation_digest,checkpoint,completed_nodes) VALUES($1,'user_001',1,'COMPLETED',1,1,1,$2,$3,'{\"kind\":\"FINISHED\"}',$4)").bind(TASK).bind(json!(plan_record)).bind(digest(&plan)?).bind(json!([node])).execute(&mut **tx).await?;
 sqlx::query("INSERT INTO audit_events(event_id,task_id,kind,task_revision,details) VALUES($1,$2,'CHECKPOINT_SAVED',1,$3)").bind(&event).bind(TASK).bind(&receipt).execute(&mut **tx).await?;
 sqlx::query("INSERT INTO simulation_balance_adjustments_v1(adjustment_id,owner_id,account_id,before_cents,target_cents,delta_cents,expected_digest,plan,task_id,event_id,receipt) VALUES($1,'user_001','account_001',$2,$3,$4,$5,$6,$7,$8,$9)").bind(ID).bind(plan["before_cents"].as_i64().ok_or("before")?).bind(TARGET).bind(plan["delta_cents"].as_i64().ok_or("delta")?).bind(plan["expected_digest"].as_str().ok_or("digest")?).bind(&plan).bind(TASK).bind(&event).bind(&receipt).execute(&mut **tx).await?;
 let n=sqlx::query("UPDATE transfer_accounts_v1 SET booked_cents=$1 WHERE account_id='account_001' AND owner_id='user_001' AND booked_cents=$2").bind(TARGET).bind(plan["before_cents"].as_i64().ok_or("before")?).execute(&mut **tx).await?.rows_affected();require(n==1)?;
 let check=snapshot(tx).await?;require(check["before_cents"]==TARGET)?;
 Ok(json!({"applied":true,"idempotent":false,"adjustment":query(tx).await?}))
}
async fn run()->Result<(),Error>{
 let args:Vec<String>=std::env::args().collect();require(args.len()==2&&matches!(args[1].as_str(),"upgrade-plan"|"upgrade"|"plan"|"apply"|"query"))?;
 let mut raw=Zeroizing::new(String::new());io::stdin().take(131073).read_to_string(&mut raw)?;require(raw.len()<=131072)?;
 let input:Input=serde_json::from_str(&raw)?;drop(raw);let url=Zeroizing::new(input.database_url);
 let options=url.parse::<sqlx::postgres::PgConnectOptions>()?;let options=sqlx::ConnectOptions::disable_statement_logging(options);
 let mut c=PgConnection::connect_with(&options).await?;
 sqlx::raw_sql(&format!("SET search_path TO {NS},pg_catalog;SET statement_timeout='45s';SET lock_timeout='8s';SET log_statement='none';SET log_min_error_statement='panic'")).execute(&mut c).await?;
 let mut tx=c.begin().await?;
 if !matches!(args[1].as_str(),"upgrade"|"apply"){sqlx::raw_sql("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await?;require(input.plan.is_none())?;}
 let value=match args[1].as_str(){"upgrade-plan"=>upgrade_plan(&mut tx).await?,"upgrade"=>upgrade(&mut tx,input.plan.as_ref().ok_or("plan required")?).await?,"plan"=>snapshot(&mut tx).await?,"apply"=>apply(&mut tx,input.plan.as_ref().ok_or("plan required")?).await?,_=>json!({"adjustment":query(&mut tx).await?})};
 tx.commit().await?;c.close().await?;println!("{}",serde_json::to_string(&value)?);Ok(())
}
#[tokio::main]
async fn main(){if run().await.is_err(){eprintln!("SIMULATION_BALANCE_REJECTED; no SQL or credential details emitted; query original adjustment after an uncertain response");std::process::exit(1)}}
