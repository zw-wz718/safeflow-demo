//! No new database authority: fence uncreated chat requests, never money tasks.
use crate::*;
use safeflow_contracts::{TaskId,UserId};
use sqlx::{Postgres,Transaction};
use serde_json::{json,Value};
use std::{collections::HashSet,sync::Mutex};
pub(crate) struct ChatRequests { epoch:String, closed:Mutex<HashSet<String>> }
impl Default for ChatRequests {fn default()->Self{Self{epoch:uuid::Uuid::new_v4().to_string(),closed:Mutex::new(HashSet::new())}}}
fn uuid(v:&str)->bool{uuid::Uuid::parse_str(v).is_ok_and(|id|id.to_string()==v)}
fn revision(v:&str)->bool{v.parse::<u64>().is_ok_and(|n|n>0&&n.to_string()==v)}
pub(crate) fn chat_parts(id:&str)->Option<(Option<&str>,&str,u64)>{
 let p:Vec<_>=id.strip_prefix("bank_chat_")?.split('_').collect();
 match p.as_slice(){
  [conversation,rev] if uuid(conversation)&&revision(rev)=>Some((None,conversation,rev.parse().ok()?)),
  ["v2",epoch,conversation,rev] if uuid(epoch)&&uuid(conversation)&&revision(rev)=>Some((Some(epoch),conversation,rev.parse().ok()?)),
  _=>None,
 }
}
async fn lock(tx:&mut Transaction<'_,Postgres>,id:&TaskId)->Result<(),StorageError>{
 sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended(current_schema() || $1,0))").bind(json!(["chat-request-creation-v1",id]).to_string()).execute(&mut **tx).await?;Ok(())
}
impl PgRepository {
 pub fn chat_request_epoch_for_backend(&self)->&str{&self.chat_requests.epoch}
 pub fn chat_request_id_for_backend(&self,conversation:&str,revision:u64)->Result<TaskId,StorageError>{
  if !uuid(conversation)||revision==0{return Err(StorageError::Invalid("chat request identity"))}
  TaskId::parse(format!("bank_chat_v2_{}_{}_{}",self.chat_requests.epoch,conversation,revision)).map_err(|_|StorageError::Invalid("chat request identity"))
 }
 // The two generic task INSERT paths hold this same global ID lock until COMMIT.
 // Old IDs stay readable but cannot be created, even by a delayed manual POST.
 pub(crate) async fn check_chat_creation(&self,tx:&mut Transaction<'_,Postgres>,owner:&UserId,id:&TaskId)->Result<(),StorageError>{
  let Some((epoch,_,_))=chat_parts(id.as_str()) else{return Ok(())};
  crate::daily_usage::protect_owner(tx,owner).await?;lock(tx,id).await?;
  if epoch!=Some(self.chat_requests.epoch.as_str())||self.chat_requests.closed.lock().map_err(|_|StorageError::DependencyUnavailable)?.contains(id.as_str()){return Err(StorageError::IdConflict)}
  Ok(())
 }
 // Called only after API verifies no same-epoch model Turn is running. An
 // absent task alone is not sufficient: future creation must also be fenced.
 pub async fn uncreated_chat_request_for_backend(&self,owner:&UserId,id:&TaskId,completed_turn:bool)->Result<Value,StorageError>{
  let (epoch,_,_)=chat_parts(id.as_str()).ok_or(StorageError::ExecutionOperationConflict)?;
  let current=epoch==Some(self.chat_requests.epoch.as_str());
  if epoch.is_some()&&(!current||!completed_turn){return Err(StorageError::ExecutionOperationConflict)}
  let mut tx=self.pool.begin().await?;
  sqlx::query("SET TRANSACTION READ ONLY").execute(&mut *tx).await?;
  crate::daily_usage::protect_owner(&mut tx,owner).await?;lock(&mut tx,id).await?;
  let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tasks WHERE task_id=$1) OR EXISTS(SELECT 1 FROM operations WHERE task_id=$1) OR EXISTS(SELECT 1 FROM mock_draft_lifecycle_claims WHERE task_id=$1) OR EXISTS(SELECT 1 FROM transfer_drafts_v1 WHERE task_id=$1) OR EXISTS(SELECT 1 FROM payment_preparations WHERE task_id=$1) OR EXISTS(SELECT 1 FROM public_execution_requests WHERE task_id=$1) OR EXISTS(SELECT 1 FROM payment_funds_execution_v1 WHERE task_id=$1) OR EXISTS(SELECT 1 FROM transfer_business_orders_v1 WHERE request->>'task_id'=$1) OR EXISTS(SELECT 1 FROM audit_events WHERE task_id=$1)").bind(id.as_str()).fetch_one(&mut *tx).await?;
  if exists{return Err(StorageError::ExecutionOperationConflict)}
  if current {
   let mut closed=self.chat_requests.closed.lock().map_err(|_|StorageError::DependencyUnavailable)?;
   if !closed.contains(id.as_str())&&closed.len()>=256{return Err(StorageError::DependencyUnavailable)}
   closed.insert(id.as_str().to_owned());
  }
  let database:String=sqlx::query_scalar("SELECT current_database()").fetch_one(&mut *tx).await?;
  use sha2::{Digest,Sha256};
  let context_ref=format!("{:x}",Sha256::digest(format!("bank-recovery-v1:{database}:{}:{}",self.namespace,owner.as_str()).as_bytes()));
  let proof=json!({"contract_version":1,"scope":"UNCREATED_CHAT_REQUEST_FENCED","task_id":id,"context_ref":context_ref,"creation_disabled":true,"task_created":false,"payment_cancelled":false,"proof_source":if current{"COMPLETED_SERVER_TURN"}else{"RETIRED_REQUEST_NAMESPACE"}});
  tx.commit().await?;Ok(proof)
 }
}
