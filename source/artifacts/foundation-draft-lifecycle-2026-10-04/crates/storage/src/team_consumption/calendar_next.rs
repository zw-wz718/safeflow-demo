//! Separately approved next-period evidence. Never rewrites the member source.
use super::*;
pub const REVISION: &str = "safeflow-demo-service-calendar-20261101-next-v1";
pub const SHA: &str = "fe66da754e616c719a4b18d7d1ef3910f14498202a3aaa6599aa25cff436e6bb";
pub const BYTES: &[u8] = include_bytes!("../../../../fixtures/team-calendar-next/approved-next-20261101.json");
#[derive(Clone)]
pub(super) struct Version { data: Value }
impl Version {
 pub(super) fn fixed() -> Result<Self, StorageError> {
  if format!("{:x}",Sha256::digest(BYTES)) != SHA { return Err(StorageError::CorruptRecord); }
  let data: Value=serde_json::from_slice(BYTES).map_err(|_|StorageError::CorruptRecord)?;
  if data["revision"]!=REVISION || data["parent_consumption_sha256"]!=TEAM_CONSUMPTION_SHA || data["fictional"]!=true || data["notification_sent"]!=false {return Err(StorageError::CorruptRecord);}
  Ok(Self{data})
 }
 pub(super) async fn load(c:&mut sqlx::PgConnection) -> Result<Option<Self>,StorageError> {
  let installed:bool=sqlx::query_scalar("SELECT to_regprocedure('subscription_calendar_current47_v1()') IS NOT NULL").fetch_one(&mut *c).await?;
  if !installed { return Ok(None); }
  let raw:Option<Vec<u8>>=sqlx::query_scalar("SELECT subscription_calendar_current47_v1()").fetch_one(c).await?;
  match raw {None=>Ok(None),Some(raw) if raw==BYTES=>Ok(Some(Self::fixed()?)),Some(_)=>Err(StorageError::FactsConflict)}
 }
 pub(super) fn provenance(&self)->Value {json!({"revision":REVISION,"artifact_sha256":SHA,"approval":self.data["approval"],"fictional":true,"notification_sent":false})}
 pub(super) fn full(&self,owner:&str)->Value {
  let entries:Vec<_>=self.data["entries"].as_array().into_iter().flatten().filter(|e|e["owner_user_id"]==owner).cloned().collect();
  json!({"entries":entries,"adoption":self.provenance()})
 }
 pub(super) fn select(&self,original:&Value,owner:&str,now:chrono::DateTime<chrono::Utc>)->Value {
  let mut chosen=original.clone();
  if let Some(entries)=chosen.as_array_mut() {
   for old in entries.iter_mut().filter(|e|e["owner_user_id"]==owner) {
    let old_future=old["next_charge_at"].as_str().and_then(|t|chrono::DateTime::parse_from_rfc3339(t).ok()).is_some_and(|t|t>=now);
    if old_future {continue;}
    let candidates:Vec<_>=self.data["entries"].as_array().into_iter().flatten().filter(|e|e["owner_user_id"]==owner && e["account_id"]==old["account_id"] && e["merchant_id"]==old["merchant_id"] && e["service_ref"]==old["service_ref"] && e["next_charge_at"].as_str().and_then(|t|chrono::DateTime::parse_from_rfc3339(t).ok()).is_some_and(|t|t>=now)).collect();
    if candidates.len()==1 {*old=candidates[0].clone();}
   }
  }
  chosen
 }
}
#[cfg(test)]
mod tests {
 use super::*;
 #[test]
 fn approved_period_selection_preserves_original_and_owner_boundaries() {
  let v=Version::fixed().unwrap();let doc=TeamConsumptionDocument::fixed_adopted().unwrap();let old=doc.data.service_calendar.clone();
  for (stamp,video,music) in [("2026-11-01T00:00:00Z","2026-11-05T10:00:00+08:00","2026-11-15T10:00:00+08:00"),("2026-11-05T02:00:00Z","2026-11-05T10:00:00+08:00","2026-11-15T10:00:00+08:00"),("2026-11-05T02:00:00.000001Z","2026-12-05T10:00:00+08:00","2026-11-15T10:00:00+08:00"),("2026-11-28T15:59:59Z","2026-12-05T10:00:00+08:00","2026-12-15T10:00:00+08:00")] {
   let selected=v.select(&old,"user_001",chrono::DateTime::parse_from_rfc3339(stamp).unwrap().with_timezone(&chrono::Utc));
   let entries=selected.as_array().unwrap();assert_eq!(entries.len(),2);assert_eq!(entries[0]["next_charge_at"],video);assert_eq!(entries[1]["next_charge_at"],music);
   for (i,e) in entries.iter().enumerate(){for k in ["owner_user_id","account_id","merchant_id","service_ref","transaction_ids","amount","currency","cycle","billing_day"]{assert_eq!(e[k],old[i][k]);}}
  }
  assert_eq!(old,doc.data.service_calendar);assert_eq!(v.select(&old,"user_002",chrono::Utc::now()),old);
  assert_eq!(v.full("user_002")["entries"],json!([]));
 }
}
