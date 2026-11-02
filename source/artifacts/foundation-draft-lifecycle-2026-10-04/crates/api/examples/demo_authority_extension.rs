//! One bounded extension of the active original cohort. No HTTP/model entry.
//! Private administrator DSN and captured plan arrive on stdin only.
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{Connection, PgConnection, Postgres, Transaction};
use std::io::{self, Read};
use zeroize::Zeroizing;
type Error = Box<dyn std::error::Error>;
const NS: &str = "sf_unified_noon_acceptance";
const DB: &str = "billing_test_noon_acceptance_20261029";
const ID: &str = "sf-demo-validity-20261101-nov28-v1";
const TASK: &str = "admin_demo_validity_20261101_nov28_v1";
const RUN: &str = "f0c0a930-8ded-4bc1-9ad0-9165ef7cdf30";
const BEFORE: &str = "2026-11-11T00:00:00+08:00";
const UNTIL: &str = "2026-11-29T00:00:00+08:00";
const APPROVAL: &str = "NEXT_DEMO_VALIDITY_TO_NOV28_2026-11-01:ACTIVE_F0C0_ONLY:UNTIL_2026-11-29T00:00:00+08:00:ONCE";
const ACTORS: [&str; 2] = ["sf_noon_producer_20261030", "sf_noon_worker_20261030"];
const FUNCTIONS: [&str; 5] = ["payment_authority_window_v1()", "payment_authority_task_accept_v1()", "payment_authority_accept_guard_v1()", "payment_auto_enroll_v1()", "payment_authority_original_guard_v1(text,text)"];
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input { database_url: String, plan: Option<Value>, approval: Option<String> }
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
 let tables: Vec<String> = sqlx::query_scalar("SELECT tablename::text FROM pg_tables WHERE schemaname=current_schema() AND tablename NOT IN('_sqlx_migrations','demo_authority_extensions46_v1','public_card_final_forward46_v1') ORDER BY tablename").fetch_all(&mut *c).await?;
 let mut hashes = serde_json::Map::new();
 for t in tables {
  require(t.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'))?;
  // Exclude only this administration's new rows and normalize its four expiry
  // fields. Every other historical row, including old tasks/claims, is hashed.
  let (row, filter) = match t.as_str() {
   "payment_background_principals_v1" => ("CASE WHEN t.role_name IN('sf_noon_producer_20261030','sf_noon_worker_20261030') THEN to_jsonb(t)-'valid_until' ELSE to_jsonb(t) END".into(), "true".into()),
   "payment_completion_candidate_fences" => ("CASE WHEN t.authority_id IN('sf_noon_producer_20261030','sf_noon_worker_20261030') THEN to_jsonb(t)-'valid_until' ELSE to_jsonb(t) END".into(), "true".into()),
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
async fn upgrade_plan(c: &mut PgConnection) -> Result<Value, Error> {
 verify(c, 45).await?;
 let objects: String = sqlx::query_scalar("SELECT public_card_final_objects_v1()").fetch_one(&mut *c).await?;
 Ok(json!({"schema":"DEMO_AUTHORITY_EXTENSION_UPGRADE_PLAN_V1","database":DB,"namespace":NS,"predecessor":45,"version":46,"checksum":format!("{:x}",sha2::Sha384::digest(maintenance(46).sql.as_bytes())),"objects":objects,"permissions_digest":permissions(c).await?,"protected":protected(c).await?}))
}
async fn upgrade(tx: &mut Transaction<'_, Postgres>, expected: &Value) -> Result<Value, Error> {
 lock_all(tx).await?;
 let installed: bool = sqlx::query_scalar("SELECT to_regclass('public_card_final_forward46_v1') IS NOT NULL").fetch_one(&mut **tx).await?;
 if installed {
  verify(tx, 46).await?;
  let old: Value = sqlx::query_scalar("SELECT upgrade_plan FROM public_card_final_forward46_v1").fetch_one(&mut **tx).await?;
  require(&old == expected)?; return Ok(json!({"upgraded":false,"version":46,"idempotent":true}));
 }
 let plan = upgrade_plan(tx).await?; require(&plan == expected)?;
 let body: String = sqlx::query_scalar("SELECT prosrc FROM pg_proc WHERE oid='public_card_final_expected_digest_v1(uuid,text)'::regprocedure").fetch_one(&mut **tx).await?;
 let owner: String = sqlx::query_scalar("SELECT pg_get_userbyid(proowner)::text FROM pg_proc WHERE oid='public_card_final_expected_digest_v1(uuid,text)'::regprocedure").fetch_one(&mut **tx).await?;
 safe_name(&owner)?; require(owner == "sf_noon_owner_20261029")?;
 let safe: bool = sqlx::query_scalar("SELECT NOT rolcanlogin AND NOT rolsuper AND NOT rolcreatedb AND NOT rolcreaterole AND NOT rolbypassrls FROM pg_roles WHERE rolname=$1").bind(&owner).fetch_one(&mut **tx).await?; require(safe)?;
 sqlx::raw_sql(&maintenance(46).sql).execute(&mut **tx).await?;
 for f in FUNCTIONS {
  let mut definition: String = sqlx::query_scalar("SELECT pg_get_functiondef($1::regprocedure)").bind(f).fetch_one(&mut **tx).await?;
  require(definition.matches("FROM payment_authority_continuations_v1").count() == 1)?;
  definition = definition.replacen("FROM payment_authority_continuations_v1", "FROM payment_authority_current46_v1()", 1);
  sqlx::raw_sql(&definition).execute(&mut **tx).await?;
  let reader: String = sqlx::query_scalar("SELECT pg_get_userbyid(proowner)::text FROM pg_proc WHERE oid=$1::regprocedure").bind(f).fetch_one(&mut **tx).await?;
  safe_name(&reader)?; require(reader == owner)?;
  sqlx::raw_sql(&format!("GRANT EXECUTE ON FUNCTION payment_authority_current46_v1() TO {reader}")).execute(&mut **tx).await?;
 }
 // The original monotone guard's exception requires the same unexpired, fixed
 // extension evidence as the additional expiry trigger; no epoch bump or reset.
 let mut monotone: String = sqlx::query_scalar("SELECT pg_get_functiondef('completion_candidate_monotone_v1()'::regprocedure)").fetch_one(&mut **tx).await?;
 let needle = "IF TG_TABLE_NAME='payment_completion_candidate_fences' THEN";
 require(monotone.matches(needle).count() == 1)?;
 monotone = monotone.replacen(needle, "IF TG_TABLE_NAME='payment_completion_candidate_fences' THEN\n        IF NEW.epoch=OLD.epoch AND NEW.valid_until IS DISTINCT FROM OLD.valid_until THEN\n            IF demo_authority_expiry_allowed46_v1(to_jsonb(OLD),to_jsonb(NEW),TG_TABLE_NAME) THEN RETURN NEW; END IF;\n        END IF;", 1);
 sqlx::raw_sql(&monotone).execute(&mut **tx).await?;
 let mut revision: String = sqlx::query_scalar("SELECT pg_get_functiondef('simulation_balance_revision44_v1()'::regprocedure)").fetch_one(&mut **tx).await?;
 require(revision.matches("IS DISTINCT FROM 45").count() == 1)?;
 revision = revision.replacen("IS DISTINCT FROM 45", "IS DISTINCT FROM 46", 1); sqlx::raw_sql(&revision).execute(&mut **tx).await?;
 let next = format!("CREATE OR REPLACE FUNCTION public_card_final_expected_digest_v1(deployment UUID,original_digest TEXT) RETURNS TEXT LANGUAGE sql STABLE SECURITY DEFINER SET search_path TO pg_catalog,{NS},pg_temp AS $forward46$ SELECT coalesce((SELECT n.to_digest FROM public_card_final_forward46_v1 n JOIN _sqlx_migrations p ON p.version=45 AND p.success AND p.checksum=n.predecessor_checksum JOIN _sqlx_migrations m ON m.version=46 AND m.success AND m.checksum=n.migration_checksum WHERE n.deployment_id=$1 AND n.original_digest=$2 AND n.from_digest=previous.digest),previous.digest) FROM ({body}) previous(digest) $forward46$");
 sqlx::raw_sql(&next).execute(&mut **tx).await?;
 for t in ["demo_authority_extensions46_v1", "public_card_final_forward46_v1"] { sqlx::raw_sql(&format!("ALTER TABLE {t} OWNER TO {owner}")).execute(&mut **tx).await?; }
 for f in ["payment_authority_current46_v1()", "demo_authority_expiry_allowed46_v1(jsonb,jsonb,text)", "demo_authority_expiry_writer46_v1()", "demo_authority_extension_assert46_v1(text)", "demo_authority_extension_complete46_v1()"] { sqlx::raw_sql(&format!("ALTER FUNCTION {f} OWNER TO {owner}")).execute(&mut **tx).await?; }
 sqlx::raw_sql(&format!("GRANT SELECT ON payment_authority_continuations_v1,payment_background_principals_v1,payment_completion_candidate_fences,tasks,audit_events,public_audit_outbox TO {owner}")).execute(&mut **tx).await?;
 sqlx::query("INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time) VALUES(46,'bounded original cohort extension',true,$1,0)").bind(maintenance(46).checksum.as_ref()).execute(&mut **tx).await?;
 sqlx::query("INSERT INTO public_card_final_forward46_v1(deployment_id,original_digest,from_digest,to_digest,predecessor_checksum,migration_checksum,upgrade_plan) SELECT d.deployment_id,d.object_digest,$1,public_card_final_objects_v1(),p.checksum,m.checksum,$2 FROM public_card_final_deployment_v1 d CROSS JOIN _sqlx_migrations p CROSS JOIN _sqlx_migrations m WHERE p.version=45 AND m.version=46").bind(plan["objects"].as_str().ok_or("objects")?).bind(&plan).execute(&mut **tx).await?;
 require(protected(tx).await? == plan["protected"])?; verify(tx, 46).await?;
 Ok(json!({"upgraded":true,"version":46,"historical_rows_preserved":true,"authority_extended":false}))
}
fn task_plan() -> Result<Value, Error> {
 use safeflow_contracts::*;
 Ok(json!(AgentPlan::new(TaskId::parse(TASK)?,1,vec![PlanNode::UserDecision{id:NodeId::parse("organizer_extension")?,depends_on:vec![]}])?))
}
async fn active(c: &mut PgConnection, until: &str) -> Result<bool, Error> {
 Ok(sqlx::query_scalar("SELECT payment_authority_window_v1() AND EXISTS(SELECT 1 FROM payment_auto_runs_v1 WHERE run_id=$1::uuid AND enabled AND producer_role='sf_noon_producer_20261030' AND worker_role='sf_noon_worker_20261030') AND (SELECT count(*) FROM payment_background_principals_v1 p JOIN payment_completion_candidate_fences f ON f.kind=p.kind AND f.authority_id=p.role_name JOIN pg_roles r ON r.rolname=p.role_name WHERE p.role_name IN('sf_noon_producer_20261030','sf_noon_worker_20261030') AND p.active AND f.enabled AND p.epoch=1 AND f.epoch=1 AND p.valid_until=$2::timestamptz AND f.valid_until=p.valid_until AND r.rolvaliduntil=p.valid_until AND clock_timestamp()<p.valid_until AND p.source_id='safeflow-team-simulation' AND p.implementation='foundation-unified-fictional-v1' AND f.scope='{\"accounts\":[{\"owner_id\":\"user_001\",\"account_id\":\"account_001\"}]}'::jsonb AND f.adoption_ref='sf-noon-new-authority-20261030-v1' AND r.rolcanlogin AND NOT r.rolsuper AND NOT r.rolinherit AND NOT r.rolcreatedb AND NOT r.rolcreaterole AND NOT r.rolbypassrls AND NOT r.rolreplication AND r.rolconnlimit=4 AND r.rolconfig IS NULL AND NOT EXISTS(SELECT 1 FROM pg_auth_members m WHERE m.member=r.oid OR m.roleid=r.oid) AND NOT has_schema_privilege(r.rolname,current_schema(),'CREATE') AND NOT has_database_privilege(r.rolname,current_database(),'TEMP'))=2 AND EXISTS(SELECT 1 FROM card_safety_account_fences_v1 WHERE owner_id='user_001' AND account_id='account_001' AND NOT revoked AND epoch=1) AND EXISTS(SELECT 1 FROM payment_completion_logins_v1 WHERE role_name='sf_noon_worker_20261030' AND writer_version='completion-final-v1')")
  .bind(RUN).bind(until).fetch_one(c).await?)
}
async fn snapshot(c: &mut PgConnection) -> Result<Value, Error> {
 verify(c, 46).await?; require(active(c, BEFORE).await?)?;
 let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM demo_authority_extensions46_v1)").fetch_one(&mut *c).await?; require(!exists)?;
 let before: Value = sqlx::query_scalar("SELECT jsonb_build_object('database_oid',(SELECT oid::bigint FROM pg_database WHERE datname=current_database()),'schema_oid',(SELECT oid::bigint FROM pg_namespace WHERE nspname=current_schema()),'continuation',(SELECT to_jsonb(c) FROM payment_authority_continuations_v1 c WHERE singleton=1),'run',(SELECT to_jsonb(r) FROM payment_auto_runs_v1 r WHERE run_id=$1::uuid),'payment_background_principals_v1',(SELECT jsonb_agg(to_jsonb(p) ORDER BY role_name) FROM payment_background_principals_v1 p WHERE role_name IN('sf_noon_producer_20261030','sf_noon_worker_20261030')),'payment_completion_candidate_fences',(SELECT jsonb_agg(to_jsonb(f) ORDER BY authority_id) FROM payment_completion_candidate_fences f WHERE authority_id IN('sf_noon_producer_20261030','sf_noon_worker_20261030')),'roles',(SELECT jsonb_agg(to_jsonb(r)-'rolpassword' ORDER BY rolname) FROM pg_roles r WHERE rolname IN('sf_noon_producer_20261030','sf_noon_worker_20261030')),'account_fence',(SELECT to_jsonb(f) FROM card_safety_account_fences_v1 f WHERE owner_id='user_001' AND account_id='account_001'))").bind(RUN).fetch_one(&mut *c).await?;
 Ok(json!({"schema":"DEMO_AUTHORITY_EXTENSION_PLAN_V1","request_id":ID,"database":DB,"namespace":NS,"run_id":RUN,"previous_ends_at":BEFORE,"target_ends_at":UNTIL,"before":before,"permissions_digest":permissions(c).await?,"protected":protected(c).await?,"approval":APPROVAL,"task_plan":task_plan()?,"payment_authorized":false}))
}
async fn query(c: &mut PgConnection) -> Result<Value, Error> {
 verify(c, 46).await?;
 let old: Option<Value> = sqlx::query_scalar("SELECT to_jsonb(e) FROM demo_authority_extensions46_v1 e WHERE request_id=$1").bind(ID).fetch_optional(&mut *c).await?;
 if old.is_some() { sqlx::query("SELECT demo_authority_extension_assert46_v1($1)").bind(ID).execute(&mut *c).await?; }
 let until = if old.is_some() { UNTIL } else { BEFORE };
 Ok(json!({"extension":old,"currently_active":active(c,until).await?,"effective_ends_at":until,"original_ends_at":BEFORE,"payment_authorized":false}))
}
async fn apply(tx: &mut Transaction<'_, Postgres>, expected: &Value, approval: &str) -> Result<Value, Error> {
 require(approval == APPROVAL)?; lock_all(tx).await?; verify(tx, 46).await?;
 let old = query(tx).await?;
 if !old["extension"].is_null() { require(old["extension"]["plan"] == *expected)?; return Ok(json!({"applied":false,"idempotent":true,"query":old})); }
 let plan = snapshot(tx).await?; require(&plan == expected)?;
 // PostgreSQL JSONB text, exactly matching the deferred SQL receipt assertion.
 let pd: String = sqlx::query_scalar("SELECT encode(sha256(convert_to($1::jsonb::text,'UTF8')),'hex')").bind(&plan).fetch_one(&mut **tx).await?;
 let transaction: i64 = sqlx::query_scalar("SELECT txid_current()").fetch_one(&mut **tx).await?;
 let event = format!("event_{}", uuid::Uuid::new_v4());
 let receipt = json!({"schema":"DEMO_AUTHORITY_EXTENDED_V1","request_id":ID,"run_id":RUN,"previous_ends_at":BEFORE,"target_ends_at":UNTIL,"transaction_id":transaction,"plan_digest":pd,"payment_authorized":false,"per_payment_minutes":30,"original_continuation_preserved":true});
 sqlx::query("INSERT INTO tasks(task_id,owner_id,contract_version,state,revision,plan_version,facts_version,plan,creation_digest,checkpoint,completed_nodes) VALUES($1,'user_001',1,'COMPLETED',1,1,1,$2,$3,'{\"kind\":\"FINISHED\"}','[\"organizer_extension\"]')").bind(TASK).bind(&plan["task_plan"]).bind(&pd).execute(&mut **tx).await?;
 sqlx::query("INSERT INTO audit_events(event_id,task_id,kind,task_revision,details) VALUES($1,$2,'CHECKPOINT_SAVED',1,$3)").bind(&event).bind(TASK).bind(&receipt).execute(&mut **tx).await?;
 sqlx::query("INSERT INTO demo_authority_extensions46_v1(request_id,run_id,previous_ends_at,target_ends_at,transaction_id,plan,receipt,task_id,event_id) VALUES($1,$2::uuid,$3::timestamptz,$4::timestamptz,$5,$6,$7,$8,$9)").bind(ID).bind(RUN).bind(BEFORE).bind(UNTIL).bind(transaction).bind(&plan).bind(&receipt).bind(TASK).bind(event).execute(&mut **tx).await?;
 for t in ["payment_background_principals_v1", "payment_completion_candidate_fences"] {
  let col = if t == "payment_background_principals_v1" { "role_name" } else { "authority_id" };
  let changed = sqlx::query(&format!("UPDATE {t} SET valid_until=$1::timestamptz WHERE {col} IN($2,$3) AND valid_until=$4::timestamptz")).bind(UNTIL).bind(ACTORS[0]).bind(ACTORS[1]).bind(BEFORE).execute(&mut **tx).await?.rows_affected(); require(changed == 2)?;
 }
 for role in ACTORS { sqlx::raw_sql(&format!("ALTER ROLE {role} VALID UNTIL '{UNTIL}'")).execute(&mut **tx).await?; }
 require(protected(tx).await? == plan["protected"])?;
 require(active(tx, UNTIL).await?)?;
 sqlx::raw_sql("SET CONSTRAINTS ALL IMMEDIATE").execute(&mut **tx).await?;
 Ok(json!({"applied":true,"idempotent":false,"query":query(tx).await?}))
}
async fn run() -> Result<(), Error> {
 let args: Vec<String> = std::env::args().collect(); require(args.len()==2 && matches!(args[1].as_str(),"upgrade-plan"|"upgrade"|"plan"|"apply"|"query"))?;
 let mut raw = Zeroizing::new(String::new()); io::stdin().take(1048577).read_to_string(&mut raw)?; require(raw.len()<=1048576)?;
 let input: Input = serde_json::from_str(&raw)?; drop(raw); let url = Zeroizing::new(input.database_url);
 let options = url.parse::<sqlx::postgres::PgConnectOptions>()?;
 require(options.get_database()==Some(DB) && options.get_username()=="postgres" && matches!(options.get_host(),"127.0.0.1"|"/var/run/postgresql"))?;
 let options = sqlx::ConnectOptions::disable_statement_logging(options); let mut c = PgConnection::connect_with(&options).await?;
 sqlx::raw_sql(&format!("SET search_path TO {NS},pg_catalog;SET TimeZone TO 'UTC';SET statement_timeout='45s';SET lock_timeout='8s';SET log_statement='none';SET log_min_error_statement='panic'")).execute(&mut c).await?;
 let mut tx = c.begin().await?;
 if !matches!(args[1].as_str(),"upgrade"|"apply") { sqlx::raw_sql("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await?; require(input.plan.is_none() && input.approval.is_none())?; }
 let value = match args[1].as_str() {
  "upgrade-plan"=>upgrade_plan(&mut tx).await?,
  "upgrade"=>{require(input.approval.is_none())?;upgrade(&mut tx,input.plan.as_ref().ok_or("plan")?).await?},
  "plan"=>snapshot(&mut tx).await?,
  "apply"=>apply(&mut tx,input.plan.as_ref().ok_or("plan")?,input.approval.as_deref().ok_or("approval")?).await?,
  _=>query(&mut tx).await?,
 };
 tx.commit().await?; c.close().await?; println!("{}",serde_json::to_string(&value)?); Ok(())
}
#[tokio::main]
async fn main() { if let Err(e)=run().await { let code=if let Some(sqlx::Error::Database(d))=e.downcast_ref::<sqlx::Error>() {d.code().map(|c|c.into_owned()).unwrap_or_default()}else if e.to_string().starts_with("bounded extension guard "){e.to_string()}else{"INPUT_OR_CONNECTION".into()}; eprintln!("DEMO_AUTHORITY_EXTENSION_REJECTED [{code}]; no SQL or credential details emitted; query original extension after an uncertain response"); std::process::exit(1); } }
