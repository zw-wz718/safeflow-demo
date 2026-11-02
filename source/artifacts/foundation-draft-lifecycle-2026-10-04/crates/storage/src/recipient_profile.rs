//! Native adopted display metadata only. No route, balance or payment authorization.
use crate::*;
use safeflow_contracts::{ResourceId, UserId};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, Row};

pub const RECIPIENT_PROFILE_REVISION: &str = "safeflow-recipient-profile-20261029-v2";
pub const RECIPIENT_PROFILE_SHA256: &str = "4aabbb6c8ed4bc12f93f7249bbcb38d7f1f939e5e05342de7e33a49192b3f42b";
const RAW: &[u8] = include_bytes!("../../../../../fixtures/transfer/recipient_profile_v2.json");
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmationRecipient {
 pub revision: String,
 pub source_revision: String,
 pub owner_id: UserId,
 pub payee_id: ResourceId,
 pub recipient_account_id: ResourceId,
 pub recipient_owner_id: UserId,
 pub card_id: ResourceId,
 pub display_name: String,
 pub bank_name: String,
 pub masked_account_no: String,
 pub simulation_card_number: String,
}
impl ConfirmationRecipient {
 pub fn fixed() -> Result<Self, StorageError> {
  serde_json::from_slice(RAW).map_err(|_|StorageError::CorruptRecord)
 }
}
impl PgRepository {
 /// Read only the fixed explicitly adopted version. Absence preserves original v1 behavior.
 pub(crate) async fn recipient_profile_on(c:&mut PgConnection,owner:&UserId,payee:&ResourceId)->Result<Option<ConfirmationRecipient>,StorageError>{
  let exists:bool=sqlx::query_scalar("SELECT to_regclass('public_recipient_profile_versions_v1') IS NOT NULL").fetch_one(&mut *c).await?;
  if !exists {return Ok(None);}
  let rows=sqlx::query("SELECT revision,artifact_sha256,original_bytes FROM public_recipient_profile_versions_v1").fetch_all(&mut *c).await?;
  if rows.is_empty(){return Ok(None);}
  if rows.len()!=1 || rows[0].try_get::<String,_>("revision")?!=RECIPIENT_PROFILE_REVISION || rows[0].try_get::<String,_>("artifact_sha256")?!=RECIPIENT_PROFILE_SHA256 || rows[0].try_get::<Vec<u8>,_>("original_bytes")?!=RAW {return Err(StorageError::FactsConflict);}
  let p=ConfirmationRecipient::fixed()?;
  if p.owner_id!=*owner || p.payee_id!=*payee {return Ok(None);}
  let base=sqlx::query("SELECT original_bytes,source_revision,active FROM public_unified_fictional_source_v1 WHERE singleton=1").fetch_one(&mut *c).await?;
  let raw:Vec<u8>=base.try_get("original_bytes")?;
  UnifiedFictionalDocument::read_adopted_v1(&raw)?;
  if !base.try_get::<bool,_>("active")? || base.try_get::<String,_>("source_revision")?!=p.source_revision{return Err(StorageError::FactsConflict);}
  let route:Option<(String,String)>=sqlx::query_as("SELECT r.recipient_account_id,a.owner_id FROM transfer_payee_routes_v1 r JOIN transfer_accounts_v1 a ON a.account_id=r.recipient_account_id WHERE r.owner_id=$1 AND r.payee_id=$2").bind(owner.as_str()).bind(payee.as_str()).fetch_optional(&mut *c).await?;
  if route!=Some((p.recipient_account_id.to_string(),p.recipient_owner_id.to_string())){return Err(StorageError::FactsConflict);}
  Ok(Some(p))
 }
 pub(crate) async fn retain_recipient_profile(&self,c:&mut PgConnection,owner:&UserId,request:&mut serde_json::Value,payee:&serde_json::Value)->Result<(),StorageError>{
  if let Some(id)=payee.as_str(){
   let id=ResourceId::parse(id).map_err(|_|StorageError::CorruptRecord)?;
   if let Some(p)=Self::recipient_profile_on(c,owner,&id).await?{
    // Only a version reference enters action/audit provenance; never send the number to the planner.
    request["recipient_profile_revision"]=serde_json::json!(p.revision);
   }
  }
  Ok(())
 }
 pub(crate) async fn recipient_from_request(c:&mut PgConnection,owner:&UserId,request:&serde_json::Value,payee:&serde_json::Value)->Result<Option<ConfirmationRecipient>,StorageError>{
  let Some(revision)=request.get("recipient_profile_revision") else {return Ok(None);};
  if revision!=RECIPIENT_PROFILE_REVISION{return Err(StorageError::CorruptRecord);}
  let id=ResourceId::parse(payee.as_str().ok_or(StorageError::CorruptRecord)?).map_err(|_|StorageError::CorruptRecord)?;
  Self::recipient_profile_on(c,owner,&id).await?.map(Some).ok_or(StorageError::FactsConflict)
 }
}
impl PgUnifiedFictionalSource {
 /// Read-only preflight, also repeated in the adoption transaction. Unknown
 /// principals or object drift must not cause a migration or implicit rebind.
 pub async fn validate_recipient_profile_maintenance(&self,api_role:&str)->Result<(),StorageError>{
  let repo=self.repository_for_isolated_backend();let mut tx=repo.pool.begin().await?;
  self.check(&mut tx).await?;recipient_maintenance_principals(&mut tx,api_role).await?;
  recipient_card_binding(&mut tx).await?;tx.commit().await?;Ok(())
 }
 /// Maintenance only, with an existing native administrator and registered API login.
 /// Caller first applies normal additive migrations; no seeding, rearming or data rewrite.
 pub async fn adopt_recipient_profile_for_native_backend(&self,api_role:&str)->Result<serde_json::Value,StorageError>{
  if !(3..=63).contains(&api_role.len()) || !api_role.starts_with("sf_") || !api_role.bytes().all(|b|b.is_ascii_lowercase()||b.is_ascii_digit()||b==b'_'){return Err(StorageError::Invalid("API role"));}
  let repo=self.repository_for_isolated_backend();let mut tx=repo.pool.begin().await?;
  self.check(&mut tx).await?;
  let readers=recipient_maintenance_principals(&mut tx,api_role).await?;
  recipient_card_binding(&mut tx).await?;
  sqlx::query("LOCK TABLE public_recipient_profile_versions_v1 IN EXCLUSIVE MODE").execute(&mut *tx).await?;
  let count:i64=sqlx::query_scalar("SELECT count(*) FROM public_recipient_profile_versions_v1").fetch_one(&mut *tx).await?;
  let inserted=if count==0{sqlx::query("INSERT INTO public_recipient_profile_versions_v1(revision,artifact_sha256,original_bytes) VALUES($1,$2,$3)").bind(RECIPIENT_PROFILE_REVISION).bind(RECIPIENT_PROFILE_SHA256).bind(RAW).execute(&mut *tx).await?;true}else{false};
  let p=ConfirmationRecipient::fixed()?;
  PgRepository::recipient_profile_on(&mut tx,&p.owner_id,&p.payee_id).await?.ok_or(StorageError::FactsConflict)?;
  for role in readers {
   sqlx::query(&format!("GRANT SELECT ON \"{}\".public_recipient_profile_versions_v1 TO \"{role}\"",repo.namespace)).execute(&mut *tx).await?;
  }
  tx.commit().await.map_err(|_|StorageError::ReconcileRequired)?;
  Ok(serde_json::json!({"revision":RECIPIENT_PROFILE_REVISION,"sha256":RECIPIENT_PROFILE_SHA256,"inserted":inserted,"existing_data_preserved":true}))
 }
 pub async fn inspect_recipient_profile_for_native_backend(&self)->Result<serde_json::Value,StorageError>{
  let repo=self.repository_for_isolated_backend();let mut tx=repo.pool.begin().await?;self.check(&mut tx).await?;
  recipient_card_binding(&mut tx).await?;
  let p=ConfirmationRecipient::fixed()?;let adopted=PgRepository::recipient_profile_on(&mut tx,&p.owner_id,&p.payee_id).await?.is_some();tx.commit().await?;
  Ok(serde_json::json!({"revision":RECIPIENT_PROFILE_REVISION,"adopted":adopted}))
 }
}
async fn recipient_maintenance_principals(c:&mut PgConnection,api_role:&str)->Result<Vec<String>,StorageError>{
 let registered:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM payment_background_api_logins_v1 a WHERE a.role_name=$1 AND a.active)").bind(api_role).fetch_one(&mut *c).await?;
 if !registered{return Err(StorageError::Invalid("existing active restricted API login required"));}
 let mut roles:Vec<String>=sqlx::query_scalar("SELECT role_name FROM public_card_final_logins_v1 WHERE reader_version='card-final-v1' ORDER BY role_name").fetch_all(&mut *c).await?;
 roles.push(api_role.to_owned());roles.sort();roles.dedup();
 for role in &roles {
  if !(3..=63).contains(&role.len()) || !role.starts_with("sf_") || !role.bytes().all(|b|b.is_ascii_lowercase()||b.is_ascii_digit()||b==b'_'){return Err(StorageError::Invalid("registered reader identifier"));}
  let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_roles r WHERE r.rolname=$1 AND r.rolcanlogin AND NOT r.rolsuper AND NOT r.rolcreatedb AND NOT r.rolcreaterole AND NOT r.rolbypassrls AND NOT r.rolinherit) AND NOT has_schema_privilege($1,current_schema(),'CREATE') AND NOT has_database_privilege($1,current_database(),'TEMP') AND NOT EXISTS(SELECT 1 FROM pg_auth_members WHERE member=(SELECT oid FROM pg_roles WHERE rolname=$1)) AND NOT EXISTS(SELECT 1 FROM payment_background_principals_v1 WHERE role_name=$1)")
    .bind(role).fetch_one(&mut *c).await?;
  if !valid{return Err(StorageError::FormalAuthUnavailable);}
 }
 Ok(roles)
}
async fn recipient_card_binding(c:&mut PgConnection)->Result<(),StorageError>{
 let invalid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public_card_final_deployment_v1 WHERE reader_version IS DISTINCT FROM 'card-final-v1' OR source_id IS DISTINCT FROM $1 OR implementation IS DISTINCT FROM $2 OR schema_oid IS DISTINCT FROM (SELECT oid FROM pg_namespace WHERE nspname=current_schema()) OR database_oid IS DISTINCT FROM (SELECT oid FROM pg_database WHERE datname=current_database()) OR public_card_final_expected_digest_v1(deployment_id,object_digest) IS DISTINCT FROM public_card_final_objects_v1())")
   .bind(UNIFIED_FICTIONAL_SOURCE).bind(UNIFIED_FICTIONAL_IMPLEMENTATION).fetch_one(&mut *c).await?;
 if invalid{return Err(StorageError::DecisionAssemblyChanged);}
 Ok(())
}
