//! Separate, approved calendar and least-privilege reminder identity; no HTTP/model entry.
//! Private administrator DSN and captured plan arrive on stdin only.
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{Connection, PgConnection, Postgres, Transaction};
use std::io::{self, Read};
use zeroize::Zeroizing;
use safeflow_storage::{TEAM_CALENDAR_NEXT_SHA,TEAM_CALENDAR_NEXT_REVISION,TEAM_CALENDAR_NEXT_BYTES,TEAM_CONSUMPTION_SHA};
type Error = Box<dyn std::error::Error>;
const NS: &str = "sf_unified_noon_acceptance";
const DB: &str = "billing_test_noon_acceptance_20261029";
const ID: &str = "sf-calendar-reminder-20261101-v1";
const TASK: &str = "admin_calendar_reminder_20261101_v1";
const RUN: &str = "f0c0a930-8ded-4bc1-9ad0-9165ef7cdf30";
const BEFORE: &str = "2026-11-11T00:00:00+08:00";
const UNTIL: &str = "2026-11-29T00:00:00+08:00";
const APPROVAL: &str = "DEMO_SUBSCRIPTION_CALENDAR_NOV28_PROPOSAL_2026-11-01:USER_ADOPTED_NEXT_PERIOD_PRESERVE_HISTORY";
const ROLE:&str="sf_noon_reminder_20261101";
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input { database_url: String, plan: Option<Value>, approval: Option<String>,new_role_password:Option<String> }
#[track_caller]
fn require(v: bool) -> Result<(), Error> { if v { Ok(()) } else { Err(format!("bounded extension guard {}",std::panic::Location::caller().line()).into()) } }
fn safe_name(s: &str) -> Result<(), Error> { require(s.starts_with("sf_") && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')) }
fn digest(v: &Value) -> Result<String, Error> { Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(v)?))) }
fn maintenance(version: i64) -> sqlx::migrate::Migration {
 let sql = match version {
  43 => include_str!("../../storage/maintenance/0043_noon_new_authority.sql"),
  44 => include_str!("../../storage/maintenance/0044_simulation_balance.sql"),
  45 => include_str!("../../storage/maintenance/0045_demo_card_release.sql"),
  46 => include_str!("../../storage/maintenance/0046_demo_authority_extension.sql"),
  47 => include_str!("../../storage/maintenance/0047_subscription_calendar_reminder.sql"),
  _ => unreachable!(),
 };
 sqlx::migrate::Migration::new(version, "bounded demo maintenance".into(), sqlx::migrate::MigrationType::Simple, sql.replace("__SCHEMA__", NS).replace("\r\n", "\n").into(), false)
}
async fn verify(c: &mut PgConnection, version: i64) -> Result<(), Error> {
 let good: bool = sqlx::query_scalar("SELECT current_database()=$1 AND current_schema()=$2 AND current_user='postgres' AND session_user='postgres' AND (SELECT rolsuper FROM pg_roles WHERE rolname=current_user) AND (SELECT pg_get_userbyid(datdba)='sf_noon_owner_20261029' FROM pg_database WHERE datname=current_database()) AND (SELECT pg_get_userbyid(nspowner)='sf_noon_owner_20261029' FROM pg_namespace WHERE nspname=current_schema()) AND EXISTS(SELECT 1 FROM public_unified_fictional_source_v1 WHERE singleton=1 AND active AND source_id='safeflow-team-simulation' AND source_revision='safeflow-team-simulation-20261026-v1' AND implementation_revision='foundation-unified-fictional-v1' AND artifact_commit='7c03f03b832aa17c174b1bbdd4a6ad3282e01422' AND artifact_sha256='85b6fc1b3c708c617586afc3e8ea61a78e525d0811439f267c93620238e8c49d' AND encode(sha256(original_bytes),'hex')=artifact_sha256) AND EXISTS(SELECT 1 FROM payment_authority_continuations_v1 WHERE singleton=1 AND run_id=$3::uuid AND ends_at=$4::timestamptz) AND EXISTS(SELECT 1 FROM public_card_final_deployment_v1 WHERE singleton=1 AND active AND public_card_final_expected_digest_v1(deployment_id,object_digest)=public_card_final_objects_v1())")
  .bind(DB).bind(NS).bind(RUN).bind(BEFORE).fetch_one(&mut *c).await?;
 require(good)?;
 let mut expected: Value = serde_json::from_str(include_str!("noon_authority_migrations.json"))?;
 for v in 43..=version { expected.as_array_mut().ok_or("versions")?.push(json!({"version":v,"success":true,"checksum":format!("{:x}",sha2::Sha384::digest(maintenance(v).sql.as_bytes()))})); }
 let actual: Value = sqlx::query_scalar("SELECT jsonb_agg(jsonb_build_object('version',version,'success',success,'checksum',encode(checksum,'hex')) ORDER BY version) FROM _sqlx_migrations").fetch_one(c).await?;
 require(actual == expected)
}
async fn catalog(c: &mut PgConnection) -> Result<Value, Error> {
 Ok(sqlx::query_scalar("SELECT jsonb_build_object('roles',(SELECT jsonb_agg(to_jsonb(r)-'rolpassword' ORDER BY oid) FROM pg_roles r WHERE rolname LIKE 'sf\\_%' ESCAPE '\\'),'acl',(SELECT jsonb_agg(jsonb_build_array(oid,relacl::text,relowner) ORDER BY oid) FROM pg_class WHERE relnamespace=(SELECT oid FROM pg_namespace WHERE nspname=current_schema())),'functions',(SELECT jsonb_agg(jsonb_build_array(oid,proacl::text,proowner) ORDER BY oid) FROM pg_proc WHERE pronamespace=(SELECT oid FROM pg_namespace WHERE nspname=current_schema())),'db',(SELECT jsonb_build_array(datacl::text,datdba) FROM pg_database WHERE datname=current_database()),'namespace',(SELECT jsonb_build_array(nspacl::text,nspowner) FROM pg_namespace WHERE nspname=current_schema()),'membership',(SELECT coalesce(jsonb_agg(to_jsonb(m) ORDER BY roleid,member),'[]') FROM pg_auth_members m))").fetch_one(c).await?)
}
async fn protected(c: &mut PgConnection) -> Result<Value, Error> {
 let tables: Vec<String> = sqlx::query_scalar("SELECT tablename::text FROM pg_tables WHERE schemaname=current_schema() AND tablename NOT IN('_sqlx_migrations','subscription_calendar_adoptions47_v1','public_card_final_forward47_v1') ORDER BY tablename").fetch_all(&mut *c).await?;
 let mut hashes = serde_json::Map::new();
 for t in tables {
  require(t.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'))?;
  // Exclude only this administration's new rows and normalize its four expiry
  // fields. Every other historical row, including old tasks/claims, is hashed.
  let (row, filter) = match t.as_str() {
   "tasks" | "payment_authority_accepted_tasks_v1" | "draft_intent_fences44_v1" => ("to_jsonb(t)".to_owned(), format!("task_id<>'{TASK}'")),
   "audit_events" => ("to_jsonb(t)".to_owned(), format!("task_id IS DISTINCT FROM '{TASK}'")),
   "public_audit_outbox" => ("to_jsonb(t)".to_owned(), format!("NOT EXISTS(SELECT 1 FROM audit_events e WHERE e.event_id=t.event_id AND e.task_id='{TASK}')")),
   _ => ("to_jsonb(t)".to_owned(), "true".into()),
  };
  let hash: String = sqlx::query_scalar(&format!("SELECT encode(sha256(convert_to(coalesce(jsonb_agg(v ORDER BY v::text),'[]'::jsonb)::text,'UTF8')),'hex') FROM (SELECT {row} v FROM {t} t WHERE {filter}) rows")).fetch_one(&mut *c).await?;
  hashes.insert(t, json!(hash));
 }
 Ok(Value::Object(hashes))
}
async fn permissions(c: &mut PgConnection) -> Result<String, Error> {
 let base = catalog(c).await?;
 let extra: Value = sqlx::query_scalar("SELECT jsonb_build_object('policies',(SELECT coalesce(jsonb_agg(to_jsonb(p) ORDER BY p.oid),'[]') FROM pg_policy p JOIN pg_class c ON c.oid=p.polrelid WHERE c.relnamespace=(SELECT oid FROM pg_namespace WHERE nspname=current_schema())),'rls',(SELECT jsonb_agg(jsonb_build_array(oid,relrowsecurity,relforcerowsecurity,reloptions) ORDER BY oid) FROM pg_class WHERE relnamespace=(SELECT oid FROM pg_namespace WHERE nspname=current_schema())),'columns',(SELECT jsonb_agg(jsonb_build_array(attrelid,attnum,attacl::text) ORDER BY attrelid,attnum) FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid WHERE c.relnamespace=(SELECT oid FROM pg_namespace WHERE nspname=current_schema()) AND attnum>0),'defaults',(SELECT coalesce(jsonb_agg(to_jsonb(a) ORDER BY oid),'[]') FROM pg_default_acl a))").fetch_one(c).await?;
 digest(&json!({"base":base,"extra":extra}))
}
async fn lock_all(tx: &mut Transaction<'_, Postgres>) -> Result<(), Error> {
 // Serialize native extension attempts; table locks also fence revocations and
 // business writers while capturing/rechecking the bounded maintenance plan.
 sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))").bind(format!("{NS}:{ID}")).execute(&mut **tx).await?;
 sqlx::raw_sql("LOCK TABLE pg_catalog.pg_authid,pg_catalog.pg_auth_members IN SHARE ROW EXCLUSIVE MODE").execute(&mut **tx).await?;
 let tables: Vec<String> = sqlx::query_scalar("SELECT tablename::text FROM pg_tables WHERE schemaname=current_schema() ORDER BY tablename").fetch_all(&mut **tx).await?;
 for t in &tables { require(t.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'))?; }
 sqlx::raw_sql(&format!("LOCK TABLE {} IN ACCESS EXCLUSIVE MODE", tables.join(","))).execute(&mut **tx).await?;
 Ok(())
}
fn task_plan()->Result<Value,Error>{use safeflow_contracts::*;Ok(json!(AgentPlan::new(TaskId::parse(TASK)?,1,vec![PlanNode::UserDecision{id:NodeId::parse("organizer_calendar")?,depends_on:vec![]}])?))}
async fn upgrade_plan(c:&mut PgConnection)->Result<Value,Error>{
 verify(c,46).await?;
 let extended:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM demo_authority_extensions46_v1 WHERE request_id='sf-demo-validity-20261101-nov28-v1' AND target_ends_at=$1::timestamptz) AND clock_timestamp()<$1::timestamptz").bind(UNTIL).fetch_one(&mut *c).await?;require(extended)?;
 let objects:String=sqlx::query_scalar("SELECT public_card_final_objects_v1()").fetch_one(&mut *c).await?;
 let readers:Vec<String>=sqlx::query_scalar("SELECT role_name FROM public_card_final_logins_v1 ORDER BY role_name").fetch_all(&mut *c).await?;
 Ok(json!({"schema":"CALENDAR_REMINDER_UPGRADE_PLAN_V1","database":DB,"namespace":NS,"predecessor":46,"version":47,"objects":objects,"checksum":format!("{:x}",sha2::Sha384::digest(maintenance(47).sql.as_bytes())),"permissions_digest":permissions(c).await?,"protected":protected(c).await?,"readers":readers}))
}
async fn upgrade(tx:&mut Transaction<'_,Postgres>,expected:&Value)->Result<Value,Error>{
 lock_all(tx).await?;
 let installed:bool=sqlx::query_scalar("SELECT to_regclass('public_card_final_forward47_v1') IS NOT NULL").fetch_one(&mut **tx).await?;
 if installed {verify(tx,47).await?;let old:Value=sqlx::query_scalar("SELECT upgrade_plan FROM public_card_final_forward47_v1").fetch_one(&mut **tx).await?;require(&old==expected)?;return Ok(json!({"upgraded":false,"idempotent":true,"version":47}));}
 let plan=upgrade_plan(tx).await?;require(&plan==expected)?;
 let body:String=sqlx::query_scalar("SELECT prosrc FROM pg_proc WHERE oid='public_card_final_expected_digest_v1(uuid,text)'::regprocedure").fetch_one(&mut **tx).await?;
 let owner="sf_noon_owner_20261029";
 let safe:bool=sqlx::query_scalar("SELECT NOT rolcanlogin AND NOT rolsuper AND NOT rolcreatedb AND NOT rolcreaterole AND NOT rolbypassrls AND NOT rolreplication FROM pg_roles WHERE rolname=$1").bind(owner).fetch_one(&mut **tx).await?;require(safe)?;
 sqlx::raw_sql(&maintenance(47).sql).execute(&mut **tx).await?;
 for t in ["subscription_calendar_adoptions47_v1","public_card_final_forward47_v1"]{sqlx::raw_sql(&format!("ALTER TABLE {t} OWNER TO {owner}")).execute(&mut **tx).await?;}
 for f in ["subscription_calendar_assert47_v1()","subscription_calendar_current47_v1()","demo_reminder_authority47_v1()","demo_reminder_write_guard47_v1()","subscription_calendar_complete47_v1()"]{sqlx::raw_sql(&format!("ALTER FUNCTION {f} OWNER TO {owner}")).execute(&mut **tx).await?;}
 for reader in plan["readers"].as_array().ok_or("readers")?{let r=reader.as_str().ok_or("reader")?;safe_name(r)?;sqlx::raw_sql(&format!("GRANT EXECUTE ON FUNCTION subscription_calendar_current47_v1() TO {r}")).execute(&mut **tx).await?;}
 let mut revision:String=sqlx::query_scalar("SELECT pg_get_functiondef('simulation_balance_revision44_v1()'::regprocedure)").fetch_one(&mut **tx).await?;require(revision.matches("IS DISTINCT FROM 46").count()==1)?;revision=revision.replacen("IS DISTINCT FROM 46","IS DISTINCT FROM 47",1);sqlx::raw_sql(&revision).execute(&mut **tx).await?;
 let next=format!("CREATE OR REPLACE FUNCTION public_card_final_expected_digest_v1(deployment UUID,original_digest TEXT) RETURNS TEXT LANGUAGE sql STABLE SECURITY DEFINER SET search_path TO pg_catalog,{NS},pg_temp AS $forward47$ SELECT coalesce((SELECT n.to_digest FROM public_card_final_forward47_v1 n JOIN _sqlx_migrations p ON p.version=46 AND p.success AND p.checksum=n.predecessor_checksum JOIN _sqlx_migrations m ON m.version=47 AND m.success AND m.checksum=n.migration_checksum WHERE n.deployment_id=$1 AND n.original_digest=$2 AND n.from_digest=previous.digest),previous.digest) FROM ({body}) previous(digest) $forward47$");sqlx::raw_sql(&next).execute(&mut **tx).await?;
 sqlx::query("INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time) VALUES(47,'approved next calendar and bounded reminder',true,$1,0)").bind(maintenance(47).checksum.as_ref()).execute(&mut **tx).await?;
 sqlx::query("INSERT INTO public_card_final_forward47_v1(deployment_id,original_digest,from_digest,to_digest,predecessor_checksum,migration_checksum,upgrade_plan) SELECT d.deployment_id,d.object_digest,$1,public_card_final_objects_v1(),p.checksum,m.checksum,$2 FROM public_card_final_deployment_v1 d CROSS JOIN _sqlx_migrations p CROSS JOIN _sqlx_migrations m WHERE p.version=46 AND m.version=47").bind(plan["objects"].as_str().ok_or("objects")?).bind(&plan).execute(&mut **tx).await?;
 require(protected(tx).await?==plan["protected"])?;verify(tx,47).await?;Ok(json!({"upgraded":true,"version":47,"calendar_adopted":false,"role_created":false}))
}
async fn snapshot(c:&mut PgConnection)->Result<Value,Error>{
 verify(c,47).await?;
 let ready:bool=sqlx::query_scalar("SELECT clock_timestamp()<$1::timestamptz AND NOT EXISTS(SELECT 1 FROM pg_roles WHERE rolname=$2) AND NOT EXISTS(SELECT 1 FROM subscription_calendar_adoptions47_v1) AND EXISTS(SELECT 1 FROM demo_authority_extensions46_v1 WHERE request_id='sf-demo-validity-20261101-nov28-v1' AND target_ends_at=$1::timestamptz) AND EXISTS(SELECT 1 FROM subscription_runtime_records WHERE owner_id='user_001') AND EXISTS(SELECT 1 FROM subscription_host_generations_v1 WHERE owner_id='user_001') AND EXISTS(SELECT 1 FROM public_subscription_a2_sources_v1 WHERE owner_id='user_001' AND source_revision='billing-team-consumption-proposed-20261027-v1' AND database_name=current_database() AND namespace=current_schema()) AND NOT EXISTS(SELECT 1 FROM card_safety_account_fences_v1 WHERE owner_id='user_001' AND account_id='account_001' AND revoked)").bind(UNTIL).bind(ROLE).fetch_one(&mut *c).await?;require(ready)?;
 Ok(json!({"schema":"CALENDAR_REMINDER_ADOPTION_PLAN_V1","request_id":ID,"database":DB,"namespace":NS,"role_name":ROLE,"target_ends_at":UNTIL,"artifact_sha256":TEAM_CALENDAR_NEXT_SHA,"revision":TEAM_CALENDAR_NEXT_REVISION,"parent_sha256":TEAM_CONSUMPTION_SHA,"protected":protected(c).await?,"permissions_digest":permissions(c).await?,"approval":APPROVAL,"task_plan":task_plan()?,"payment_authorized":false,"notification_sent":false}))
}
async fn query(c:&mut PgConnection)->Result<Value,Error>{
 verify(c,47).await?;
 let row:Option<Value>=sqlx::query_scalar("SELECT to_jsonb(a)-'artifact_bytes' FROM subscription_calendar_adoptions47_v1 a").fetch_optional(&mut *c).await?;
 if row.is_some(){sqlx::query("SELECT subscription_calendar_assert47_v1()").execute(&mut *c).await?;}
 let role:Option<Value>=sqlx::query_scalar("SELECT to_jsonb(r)-'rolpassword' FROM pg_roles r WHERE rolname=$1").bind(ROLE).fetch_optional(&mut *c).await?;
 Ok(json!({"adoption":row,"current_role":role,"notification_sent":false,"payment_authorized":false}))
}
async fn grant_reminder(tx:&mut Transaction<'_,Postgres>)->Result<(),Error>{
 // No task/operations/confirmation/permit/card/funds writers or payment tool
 // functions. Source lock-column UPDATEs cannot change source facts (v38).
 sqlx::raw_sql(&format!("GRANT CONNECT ON DATABASE {DB} TO {ROLE};GRANT USAGE ON SCHEMA {NS} TO {ROLE}")).execute(&mut **tx).await?;
 for table in ["_sqlx_migrations","public_unified_fictional_source_v1","transfer_accounts_v1","transfer_payee_routes_v1","transfer_ledger_v1","transfer_fund_orders_v1","simulation_balance_adjustments_v1","simulation_source_versions_v1","simulation_source_rows_v1","simulation_source_heads_v1","subscription_runtime_records","subscription_host_generations_v1","public_subscription_a2_sources_v1","subscription_reminder_attempts_v1","subscription_simulated_notifications_v1","card_catalog_v1","card_account_versions_v1","card_locks_v1","public_card_catalog_adoptions_v1","public_card_final_deployment_v1"] {
  sqlx::raw_sql(&format!("GRANT SELECT ON {table} TO {ROLE}")).execute(&mut **tx).await?;
 }
 sqlx::raw_sql(&format!("GRANT UPDATE(journal,storage_version) ON subscription_runtime_records TO {ROLE};GRANT INSERT ON subscription_reminder_attempts_v1,subscription_simulated_notifications_v1 TO {ROLE};GRANT UPDATE(generation) ON simulation_source_heads_v1 TO {ROLE};GRANT UPDATE(singleton) ON public_unified_fictional_source_v1 TO {ROLE};GRANT EXECUTE ON FUNCTION subscription_calendar_current47_v1(),demo_reminder_authority47_v1(),simulation_balance_net_v1(text),simulation_balance_revision44_v1(),public_card_final_objects_v1(),verify_subscription_a2_v1(text) TO {ROLE}")).execute(&mut **tx).await?;
 sqlx::raw_sql(&format!("GRANT SELECT ON public_recipient_profile_versions_v1 TO {ROLE};GRANT UPDATE(generation) ON subscription_host_generations_v1 TO {ROLE}")).execute(&mut **tx).await?;
 Ok(())
}
async fn apply(tx:&mut Transaction<'_,Postgres>,expected:&Value,approval:&str,password:Option<String>)->Result<Value,Error>{
 require(approval==APPROVAL)?;lock_all(tx).await?;verify(tx,47).await?;let old=query(tx).await?;
 if !old["adoption"].is_null(){require(old["adoption"]["plan"]==*expected&&password.is_none())?;return Ok(json!({"applied":false,"idempotent":true,"query":old}));}
 let plan=snapshot(tx).await?;require(&plan==expected)?;
 let password=Zeroizing::new(password.ok_or("new role password required")?);require(password.len()==64&&password.bytes().all(|b|b.is_ascii_hexdigit()))?;
 let pd:String=sqlx::query_scalar("SELECT encode(sha256(convert_to($1::jsonb::text,'UTF8')),'hex')").bind(&plan).fetch_one(&mut **tx).await?;
 let tid:i64=sqlx::query_scalar("SELECT txid_current()").fetch_one(&mut **tx).await?;let event=format!("event_{}",uuid::Uuid::new_v4());
 let receipt=json!({"schema":"SUBSCRIPTION_CALENDAR_REMINDER_ADOPTED_V1","request_id":ID,"revision":TEAM_CALENDAR_NEXT_REVISION,"artifact_sha256":TEAM_CALENDAR_NEXT_SHA,"parent_sha256":TEAM_CONSUMPTION_SHA,"role_name":ROLE,"ends_at":UNTIL,"plan_digest":pd,"transaction_id":tid,"notification_sent":false,"payment_authorized":false,"original_settings_preserved":true});
 sqlx::query("INSERT INTO tasks(task_id,owner_id,contract_version,state,revision,plan_version,facts_version,plan,creation_digest,checkpoint,completed_nodes) VALUES($1,'user_001',1,'COMPLETED',1,1,1,$2,$3,'{\"kind\":\"FINISHED\"}','[\"organizer_calendar\"]')").bind(TASK).bind(&plan["task_plan"]).bind(&pd).execute(&mut **tx).await?;
 sqlx::query("INSERT INTO audit_events(event_id,task_id,kind,task_revision,details) VALUES($1,$2,'CHECKPOINT_SAVED',1,$3)").bind(&event).bind(TASK).bind(&receipt).execute(&mut **tx).await?;
 sqlx::query("INSERT INTO subscription_calendar_adoptions47_v1(request_id,revision,artifact_bytes,artifact_sha256,parent_sha256,role_name,ends_at,transaction_id,plan,receipt,task_id,event_id) VALUES($1,$2,$3,$4,$5,$6,$7::timestamptz,$8,$9,$10,$11,$12)").bind(ID).bind(TEAM_CALENDAR_NEXT_REVISION).bind(TEAM_CALENDAR_NEXT_BYTES).bind(TEAM_CALENDAR_NEXT_SHA).bind(TEAM_CONSUMPTION_SHA).bind(ROLE).bind(UNTIL).bind(tid).bind(&plan).bind(receipt).bind(TASK).bind(event).execute(&mut **tx).await?;
 sqlx::raw_sql(&format!("CREATE ROLE {ROLE} LOGIN NOSUPERUSER NOINHERIT NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS CONNECTION LIMIT 16 PASSWORD '{}' VALID UNTIL '{UNTIL}'",password.as_str())).execute(&mut **tx).await?;drop(password);
 grant_reminder(tx).await?;require(protected(tx).await?==plan["protected"])?;sqlx::raw_sql("SET CONSTRAINTS ALL IMMEDIATE").execute(&mut **tx).await?;
 Ok(json!({"applied":true,"query":query(tx).await?}))
}
async fn run()->Result<(),Error>{
 let args:Vec<String>=std::env::args().collect();require(args.len()==2&&matches!(args[1].as_str(),"upgrade-plan"|"upgrade"|"plan"|"apply"|"query"))?;
 let mut raw=Zeroizing::new(String::new());io::stdin().take(1048577).read_to_string(&mut raw)?;require(raw.len()<=1048576)?;let input:Input=serde_json::from_str(&raw)?;drop(raw);let url=Zeroizing::new(input.database_url);
 let options=url.parse::<sqlx::postgres::PgConnectOptions>()?;require(options.get_database()==Some(DB)&&options.get_username()=="postgres"&&matches!(options.get_host(),"127.0.0.1"|"/var/run/postgresql"))?;
 let mut c=PgConnection::connect_with(&sqlx::ConnectOptions::disable_statement_logging(options)).await?;sqlx::raw_sql(&format!("SET search_path TO {NS},pg_catalog;SET TimeZone='UTC';SET statement_timeout='45s';SET lock_timeout='8s';SET log_statement='none';SET log_min_error_statement='panic'")).execute(&mut c).await?;let mut tx=c.begin().await?;
 if !matches!(args[1].as_str(),"upgrade"|"apply"){sqlx::raw_sql("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await?;require(input.plan.is_none()&&input.approval.is_none()&&input.new_role_password.is_none())?;}
 let value=match args[1].as_str(){"upgrade-plan"=>upgrade_plan(&mut tx).await?,"upgrade"=>{require(input.approval.is_none()&&input.new_role_password.is_none())?;upgrade(&mut tx,input.plan.as_ref().ok_or("plan")?).await?},"plan"=>snapshot(&mut tx).await?,"apply"=>apply(&mut tx,input.plan.as_ref().ok_or("plan")?,input.approval.as_deref().ok_or("approval")?,input.new_role_password).await?,_=>query(&mut tx).await?};tx.commit().await?;c.close().await?;println!("{}",serde_json::to_string(&value)?);Ok(())
}
#[tokio::main]
async fn main(){if let Err(e)=run().await{let code=if let Some(sqlx::Error::Database(d))=e.downcast_ref::<sqlx::Error>(){d.code().map(|c|c.into_owned()).unwrap_or_default()}else if e.to_string().starts_with("bounded extension guard "){e.to_string()}else{"INPUT_OR_CONNECTION".into()};eprintln!("CALENDAR_REMINDER_REJECTED [{code}]; no SQL or credential details emitted; query original adoption after an uncertain response");std::process::exit(1);}}
