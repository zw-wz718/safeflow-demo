//! Append-only end of a proven nonfunding draft intent, never cancellation of a payment.
use super::*;
use crate::repository::{audit, lock_task_for_mode, operations};
use safeflow_contracts::{UserId, ConfirmationRef};
pub(super) const EXPIRED_PREPARATION_SCOPE:&str="EXPIRED_UNSUBMITTED_PREPARATION";
pub(super) const EXPIRED_PREPARATION_TASKS:[&str;3]=[
 "bank_payment_a2719992-8baf-450e-b84d-b976afa1800c",
 "bank_payment_3f89e576-5cbd-426b-9e55-4f7a7e4d593d",
 "bank_chat_b2056dd9-dd20-4200-89c5-32430a2e3cf7_2",
];
const SCHEMA:&str="UNEXECUTED_DRAFT_TERMINATED_V1";
/// Only called with a note whose complete original proof has been verified.
pub(super) fn ended_error(note:&Value)->StorageError {
 match note["reason"].as_str() {
  Some("EXPIRED")=>StorageError::DraftIntentExpired,
  Some("REPLACED")=>StorageError::DraftIntentReplaced,
  Some("ABANDONED")=>StorageError::DraftIntentAbandoned,
  _=>StorageError::CorruptRecord,
 }
}
#[derive(Clone,Debug,serde::Serialize,serde::Deserialize,PartialEq)]
#[serde(deny_unknown_fields)]
pub struct IntentReplacement {pub task_id:TaskId,pub parameters:Value,pub facts_version:u64}

pub(crate) async fn note(tx:&mut Transaction<'_,Postgres>,owner:&UserId,id:&TaskId)->Result<Option<Value>,StorageError>{
 let rows=sqlx::query("SELECT e.details,e.occurred_at,EXISTS(SELECT 1 FROM public_audit_outbox o WHERE o.event_id=e.event_id AND o.envelope->'public_event'->'details'=e.details) enqueued FROM audit_events e JOIN tasks t USING(task_id) WHERE e.task_id=$1 AND t.owner_id=$2 AND e.kind='CHECKPOINT_SAVED' AND e.details->>'schema'=$3")
  .bind(id.as_str()).bind(owner.as_str()).bind(SCHEMA).fetch_all(&mut **tx).await?;
 if rows.is_empty(){return Ok(None)}
 if rows.len()!=1||!rows[0].try_get::<bool,_>("enqueued")?{return Err(StorageError::CorruptRecord)}
 let value:Value=rows[0].try_get("details")?;
 let task=crate::repository::task_row(&sqlx::query("SELECT * FROM tasks WHERE task_id=$1 AND owner_id=$2").bind(id.as_str()).bind(owner.as_str()).fetch_one(&mut **tx).await?)?;
 let ops=operations(tx,id).await?;
 let proof=proof(tx,&task,&ops).await?.ok_or(StorageError::CorruptRecord)?;
 let active=ops.iter().find(|o|o.plan_version==task.plan.plan_version).ok_or(StorageError::CorruptRecord)?;
 if value["proof"]!=proof||value["task_id"]!=json!(id)||value["operation_id"]!=json!(active.operation_id)||!matches!(value["reason"].as_str(),Some("EXPIRED"|"REPLACED"|"ABANDONED"))||value["original_preserved"]!=true||value["payment_cancelled"]!=false||value["funding_authorized"]!=false {return Err(StorageError::CorruptRecord)}
 if proof["scope"]==EXPIRED_PREPARATION_SCOPE {
  if value["reason"]!="ABANDONED" {return Err(StorageError::CorruptRecord)}
  for expires in [&proof["expires_at"],&proof["preparation"]["expires_at"]] {
   let deadline:chrono::DateTime<chrono::Utc>=serde_json::from_value(expires.clone()).map_err(|_|StorageError::CorruptRecord)?;
   if rows[0].try_get::<chrono::DateTime<chrono::Utc>,_>("occurred_at")?<deadline{return Err(StorageError::CorruptRecord)}
  }
 }
 if value["reason"]=="EXPIRED" {
  let deadline:chrono::DateTime<chrono::Utc>=serde_json::from_value(proof["expires_at"].clone()).map_err(|_|StorageError::CorruptRecord)?;
  if rows[0].try_get::<chrono::DateTime<chrono::Utc>,_>("occurred_at")?<deadline{return Err(StorageError::CorruptRecord)}
 }
 if value["reason"]=="REPLACED" {
  let r:IntentReplacement=serde_json::from_value(value["replacement"].clone()).map_err(|_|StorageError::CorruptRecord)?;
  if r.task_id==*id{return Err(StorageError::CorruptRecord)}
  let row=sqlx::query("SELECT l.initial_task,l.initial_digest FROM mock_draft_lifecycle_tasks l JOIN tasks t USING(task_id) WHERE l.task_id=$1 AND t.owner_id=$2").bind(r.task_id.as_str()).bind(owner.as_str()).fetch_optional(&mut **tx).await?.ok_or(StorageError::CorruptRecord)?;
  let initial:TaskRecord=serde_json::from_value(row.try_get("initial_task")?).map_err(|_|StorageError::CorruptRecord)?;
  if hash(&initial)?.as_str()!=row.try_get::<String,_>("initial_digest")?||initial.task_id!=r.task_id||initial.owner_id!=*owner||initial.facts_version!=r.facts_version||initial.plan.nodes.first().and_then(|n|if let PlanNode::Tool{arguments,..}=n {Some(arguments)}else{None})!=Some(&r.parameters){return Err(StorageError::CorruptRecord)}
 }else if value["replacement"]!=Value::Null{return Err(StorageError::CorruptRecord)}
 Ok(Some(value))
}

pub(super) async fn proof(tx:&mut Transaction<'_,Postgres>,task:&TaskRecord,ops:&[OperationRecord])->Result<Option<Value>,StorageError>{
 checked_history(tx,task,ops).await?;
 let claims=checked_claims(tx,task,ops).await?;
 if task.state.is_terminal()||claims.iter().any(|c|c.receipt.is_none()){return Ok(None)}
 let active:String=sqlx::query_scalar("SELECT active_operation FROM mock_draft_lifecycle_tasks WHERE task_id=$1").bind(task.task_id.as_str()).fetch_one(&mut **tx).await?;
 let op=ops.iter().find(|o|o.operation_id.as_str()==active).ok_or(StorageError::CorruptRecord)?;
 let rows=sqlx::query("SELECT * FROM mock_draft_lifecycle_actions WHERE task_id=$1 ORDER BY plan_version").bind(task.task_id.as_str()).fetch_all(&mut **tx).await?;
 // checked_history checked every original/action/plan digest; checked_claims
 // checked every durable tool receipt. Unknown claims are never timed out.
 if rows.is_empty()||rows[0].try_get::<String,_>("action")?!="CREATE_MOCK_DRAFT"||rows[0].try_get::<Option<Value>,_>("observed_draft")?.is_some(){return Ok(None)}
 let initial:Value=rows[0].try_get("parameters")?;
 if initial["replaces"]!=Value::Null{return Ok(None)}
 for row in &rows {
  if !matches!(row.try_get::<String,_>("action")?.as_str(),"CREATE_MOCK_DRAFT"|"MODIFY_MOCK_DRAFT"){return Ok(None)}
  let id:String=row.try_get("operation_id")?;
  let actual=ops.iter().find(|o|o.operation_id.as_str()==id).ok_or(StorageError::CorruptRecord)?;
  let claimed=claims.iter().any(|c|c.operation==actual.operation_id);
  if actual.operation_id!=op.operation_id && !claimed && actual.state!=OperationState::Cancelled{return Ok(None)}
 }
 let parameters:Vec<Value>=rows.iter().map(|r|r.try_get("parameters")).collect::<Result<_,_>>()?;
 // Complete positive lifecycle provenance above is required. These absence
 // checks are EXTRA guards, never the proof that an UNKNOWN payment did not run.
 let associated:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public_execution_requests WHERE task_id=$1) OR EXISTS(SELECT 1 FROM payment_execution_links WHERE task_id=$1) OR EXISTS(SELECT 1 FROM payment_funds_execution_v1 WHERE task_id=$1)").bind(task.task_id.as_str()).fetch_one(&mut **tx).await?;
 if associated{return Ok(None)}
 let (scope,deadline,receipt)=if claims.is_empty(){
  if task.state!=TaskState::Running||op.state!=OperationState::Draft||op.business_operation_ref.is_some(){return Ok(None)}
  let drafts:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM transfer_drafts_v1 WHERE task_id=$1)").bind(task.task_id.as_str()).fetch_one(&mut **tx).await?;
  if drafts{return Ok(None)}
  // No extra draft TTL is invented. Until an original server confirmation
  // exists, this branch allows explicit replacement only, never timed expiry.
  let references:Vec<String>=sqlx::query_scalar("SELECT confirmation_ref FROM server_confirmations WHERE task_id=$1 AND operation_id=$2 ORDER BY prepared_at").bind(task.task_id.as_str()).bind(op.operation_id.as_str()).fetch_all(&mut **tx).await?;
  let mut deadline=None;
  for reference in references {
   let r=ConfirmationRef::parse(reference).map_err(|_|StorageError::CorruptRecord)?;
   let c=crate::confirmation_repository::read_historical_confirmation(tx,&task.task_id,&r).await?;
   deadline=Some(deadline.map_or(c.record.expires_at,|d:chrono::DateTime<chrono::Utc>|d.min(c.record.expires_at)));
  }
  ("NEVER_CLAIMED",deadline,Value::Null)
 }else {
  let receipt=claims.last().and_then(|c|c.receipt.as_ref()).ok_or(StorageError::CorruptRecord)?;
  let active_claimed=claims.iter().any(|c|c.operation==op.operation_id);
  if active_claimed {
   if task.state!=TaskState::WaitingUser||op.state!=OperationState::Succeeded||receipt.operation_id!=op.operation_id{return Ok(None)}
  }else if task.state!=TaskState::Running||op.state!=OperationState::Draft||op.business_operation_ref.is_some(){return Ok(None)}
  let draft=sqlx::query("SELECT * FROM transfer_drafts_v1 WHERE task_id=$1 AND owner_id=$2 AND draft_id=$3").bind(task.task_id.as_str()).bind(task.owner_id.as_str()).bind(receipt.draft_id.as_str()).fetch_optional(&mut **tx).await?;
  let Some(draft)=draft else{return Err(StorageError::CorruptRecord)};
  if draft.try_get::<bool,_>("submitted")?||!matches!(draft.try_get::<String,_>("status")?.as_str(),"READY"|"EXPIRED")||draft.try_get::<i64,_>("business_version")?!=i64::from(receipt.draft_version)||draft.try_get::<String,_>("operation_id")?!=receipt.business_operation_id.as_str(){return Ok(None)}
  let expires=chrono::DateTime::from_timestamp(draft.try_get("expires_at")?,0).ok_or(StorageError::CorruptRecord)?;
  if json!(expires)!=receipt.result.data["expires_at"]{return Err(StorageError::CorruptRecord)}
  ("DRAFT_CREATED_NO_PAYMENT_ACCEPTANCE",Some(expires),json!(crate::models::hash(receipt)?))
 };
 let mut proof=json!({"scope":scope,"task_digest":hash(task)?,"operations_digest":hash(&ops)?,"parameters_digest":hash(&parameters)?,"receipt_digest":receipt,"expires_at":deadline});
 let preparations:Vec<String>=sqlx::query_scalar("SELECT operation_id FROM payment_preparations WHERE task_id=$1 ORDER BY operation_id").bind(task.task_id.as_str()).fetch_all(&mut **tx).await?;
 if !preparations.is_empty() {
  if scope!="DRAFT_CREATED_NO_PAYMENT_ACCEPTANCE"||preparations.len()!=1||task.owner_id.as_str()!="user_001"||!EXPIRED_PREPARATION_TASKS.contains(&task.task_id.as_str()){return Ok(None)}
  let id=OperationId::parse(preparations[0].clone()).map_err(|_|StorageError::CorruptRecord)?;
  let Some(preparation)=expired_unsubmitted_preparation(tx,task,&id,claims.last().and_then(|c|c.receipt.as_ref()).ok_or(StorageError::CorruptRecord)?).await? else{return Ok(None)};
  proof["scope"]=json!(EXPIRED_PREPARATION_SCOPE);
  proof["preparation"]=preparation;
 }
 Ok(Some(proof))
}


// Positive, immutable preparation provenance supplements the checked original
// lifecycle receipt. Absence of a ledger row alone never proves non-execution.
async fn expired_unsubmitted_preparation(tx:&mut Transaction<'_,Postgres>,task:&TaskRecord,id:&OperationId,receipt:&LifecycleReceipt)->Result<Option<Value>,StorageError>{
 use safeflow_transfer_integration::{funds_store::FundsStore,mock_execution::MockTransferRequest};
 let saved=crate::payment_preparation::saved_payment(tx,task,id,false).await?;
 let t=&saved.snapshot.transfer;
 let draft=FundsStore::draft(tx,&task.owner_id,&t.draft_id).await.map_err(crate::transfer_sql::business_error)?;
 let row=sqlx::query("SELECT submitted FROM transfer_drafts_v1 WHERE owner_id=$1 AND task_id=$2 AND draft_id=$3").bind(task.owner_id.as_str()).bind(task.task_id.as_str()).bind(t.draft_id.as_str()).fetch_one(&mut **tx).await?;
 let now:chrono::DateTime<chrono::Utc>=sqlx::query_scalar("SELECT clock_timestamp()").fetch_one(&mut **tx).await?;
 let expiry=i64::try_from(draft.expires_at).map_err(|_|StorageError::CorruptRecord)?;
 if task.state!=TaskState::WaitingUser||receipt.operation_id.as_str()!=sqlx::query_scalar::<_,String>("SELECT active_operation FROM mock_draft_lifecycle_tasks WHERE task_id=$1").bind(task.task_id.as_str()).fetch_one(&mut **tx).await?
  ||row.try_get::<bool,_>("submitted")?||now.timestamp()<expiry||now<saved.stored.record.expires_at
  ||receipt.draft_id!=t.draft_id||receipt.draft_version!=t.draft_version||receipt.business_operation_id.as_str()!=t.operation_id.as_str()
  ||draft.task_id!=task.task_id||draft.operation_id!=t.operation_id||draft.plan_version!=t.draft_version||saved.snapshot.draft_expires_at!=draft.expires_at
  ||saved.op.plan_version!=task.plan.plan_version||saved.op.facts_version!=task.facts_version||saved.base_digest!=hash(&task.plan)?
  ||!matches!(draft.status,safeflow_transfer_integration::DraftStatus::Expired)
 {return Ok(None)}
 let details=draft.details.as_ref().ok_or(StorageError::CorruptRecord)?;
 let original=MockTransferRequest{task_id:draft.task_id.clone(),operation_id:draft.operation_id.clone(),draft_id:draft.draft_id.clone(),draft_version:draft.plan_version,payer_account_id:details.account.account_id.clone(),payee_id:details.payee.payee_id.clone(),amount:details.amount.clone(),currency:details.currency,remark:details.remark.clone()};
 if original!=*t||serde_json::to_value(&draft.details).map_err(|_|StorageError::CorruptRecord)?!=saved.snapshot.display_details{return Err(StorageError::CorruptRecord)}
 let accepted:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public_execution_requests WHERE task_id=$1 OR (owner_id=$2 AND business_operation_ref=$3)) OR EXISTS(SELECT 1 FROM payment_execution_links WHERE task_id=$1 OR preparation_operation_id=$4) OR EXISTS(SELECT 1 FROM payment_funds_execution_v1 WHERE task_id=$1 OR preparation_operation_id=$4) OR EXISTS(SELECT 1 FROM transfer_fund_orders_v1 WHERE owner_id=$2 AND operation_id=$3) OR EXISTS(SELECT 1 FROM transfer_ledger_v1 WHERE business_owner_id=$2 AND operation_id=$3)")
  .bind(task.task_id.as_str()).bind(task.owner_id.as_str()).bind(t.operation_id.as_str()).bind(id.as_str()).fetch_one(&mut **tx).await?;
 if accepted||FundsStore::query(tx,&task.owner_id,&t.operation_id).await.map_err(crate::transfer_sql::business_error)?.is_some(){return Ok(None)}
 Ok(Some(json!({"operation_id":id,"snapshot_digest":saved.digest,"expires_at":saved.stored.record.expires_at,"draft_id":t.draft_id,"draft_version":t.draft_version})))
}

impl LifecycleService<'_>{
 /// Explicit authenticated maintenance/replacement, with exact old versions.
 /// No task/operation/claim/result/receipt is rewritten.
 pub async fn terminate_unexecuted_intent(&self,session:&SessionRef,id:&TaskId,operation:&OperationId,expected:&MockDraftExecutionVersion,replacement:Option<IntentReplacement>)->Result<Value,StorageError>{
  self.end_intent(session,id,operation,expected,replacement,false).await
 }
 /// Explicit abandonment of this one proven nonfunding intent. This allows
 /// multiple historical unconfirmed drafts to be ended without creating more.
 pub async fn abandon_unexecuted_intent(&self,session:&SessionRef,id:&TaskId,operation:&OperationId,expected:&MockDraftExecutionVersion)->Result<Value,StorageError>{
  self.end_intent(session,id,operation,expected,None,true).await
 }
 async fn end_intent(&self,session:&SessionRef,id:&TaskId,operation:&OperationId,expected:&MockDraftExecutionVersion,replacement:Option<IntentReplacement>,abandon:bool)->Result<Value,StorageError>{
  if !self.transfer_sql_bridge{return Err(StorageError::DependencyUnavailable)}
  let identity=self.identity(session)?;
  let replace=replacement.is_some();
  if let Some(r)=&replacement {
   if r.task_id==*id||r.parameters["replaces"]!=Value::Null{return Err(StorageError::PlanConflict)}
   // A new transfer is fixed before the original intent is ended. It remains
   // an unconfirmed draft and goes through all existing confirmation gates.
   crate::models::db_version(r.facts_version)?;
  }
  let mut tx=self.repository.pool.begin().await?;
  let task=lock_task_for_mode(&mut tx,&identity.user_id,id,true).await?;
  let ops=operations(&mut tx,id).await?;
  let op=ops.iter().find(|o|o.operation_id==*operation).ok_or(StorageError::NotFound)?;
  if op.plan_version!=task.plan.plan_version{return Err(StorageError::StalePlan)}
  super::execution::compare(expected,&task,op).map_err(|_|StorageError::RevisionConflict)?;
  if let Some(note)=note(&mut tx,&identity.user_id,id).await? {
   if note["reason"]!=if abandon{"ABANDONED"}else if replace{"REPLACED"}else{"EXPIRED"}||note["replacement"]!=json!(replacement){return Err(StorageError::DecisionConflict)}
   self.guard(session,&identity)?;tx.commit().await?;return Ok(note)
  }
  let proof=proof(&mut tx,&task,&ops).await?.ok_or(StorageError::ExecutionOperationConflict)?;
  if proof["scope"]==EXPIRED_PREPARATION_SCOPE&&!abandon{return Err(StorageError::ExecutionOperationConflict)}
  let now:chrono::DateTime<chrono::Utc>=sqlx::query_scalar("SELECT clock_timestamp()").fetch_one(&mut *tx).await?;
  if !replace&&!abandon {
   let deadline:chrono::DateTime<chrono::Utc>=serde_json::from_value(proof["expires_at"].clone()).map_err(|_|StorageError::ConfirmationRequired)?;
   if now<deadline{return Err(StorageError::ConfirmationRequired)}
  }
  if let Some(r)=&replacement {self.insert_new_intent(&mut tx,session,&r.task_id,r.parameters.clone(),r.facts_version).await?;}
  let value=json!({"schema":SCHEMA,"task_id":id,"operation_id":operation,"reason":if abandon{"ABANDONED"}else if replace{"REPLACED"}else{"EXPIRED"},"replacement":replacement,"proof":proof,"original_preserved":true,"payment_cancelled":false,"funding_authorized":false});
  audit(&mut tx,&task,Some(op),PersistentAuditKind::CheckpointSaved,value.clone()).await?;
  self.guard(session,&identity)?;tx.commit().await.map_err(|_|StorageError::ReconcileRequired)?;self.guard(session,&identity)?;
  Ok(value)
 }
 /// On-demand server reconciliation. Each task independently acquires the
 /// existing owner/task lock. A changed/ineligible intent stays a blocker.
 pub async fn expire_unexecuted_intents(&self,session:&SessionRef)->Result<Value,StorageError>{
  let identity=self.identity(session)?;
  let ids:Vec<String>=sqlx::query_scalar("SELECT t.task_id FROM tasks t JOIN mock_draft_lifecycle_tasks l USING(task_id) WHERE owner_id=$1 ORDER BY t.task_id LIMIT 257").bind(identity.user_id.as_str()).fetch_all(&self.repository.pool).await?;
  if ids.len()>256{return Err(StorageError::DependencyUnavailable)}
  let mut ended=Vec::new();
  for id in ids {
   let id=TaskId::parse(id).map_err(|_|StorageError::CorruptRecord)?;
   let view=self.query(session,&id,None).await?;
   let expected=MockDraftExecutionVersion{task_revision:view.task.revision,operation_revision:view.operation.revision,plan_version:view.task.plan.plan_version,facts_version:view.task.facts_version};
   match self.terminate_unexecuted_intent(session,&id,&view.operation.operation_id,&expected,None).await {
    Ok(_)=>ended.push(id),
    Err(StorageError::ConfirmationRequired|StorageError::ExecutionOperationConflict|StorageError::RevisionConflict|StorageError::DecisionConflict)=>{},
    Err(e)=>return Err(e),
   }
  }
  self.guard(session,&identity)?;
  Ok(json!({"contract_version":1,"expired_draft_intents":ended,"payment_cancelled":false,"server_time":true}))
 }
}
