//! Public personal-opinion history. Member SAVE evidence and card head stay unchanged.
//! The root advisory transaction lock serializes CAS; requests, terminal operations and immutable
//! audit/Outbox records commit together using the existing closed Permit unit.
use super::*;

const HISTORY: &str = "BILLING_FEEDBACK_HISTORY_V1";
const SOURCE: &str = "billing-feedback-revision-v1";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RevisionAction { Correct, Revoke, Resubmit }

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Revision {
    root_operation_id: OperationId,
    expected_revision: u64,
    action: RevisionAction,
    nonce: String,
}

fn ids(owner:&UserId, nonce:&str) -> Result<(TaskId,OperationId),StorageError> {
    recovery::BillingRecoveryNonce::parse(nonce)?;
    let digest=hash(&(HISTORY,owner,nonce))?;
    Ok((TaskId::parse(format!("feedback_revision_{}",digest.as_str())).map_err(|_|StorageError::Invalid("task"))?,
        OperationId::parse(format!("feedback_revision_{}",digest.as_str())).map_err(|_|StorageError::Invalid("operation"))?))
}

// A separate transaction holds this gate across prepare_input's existing
// transactions. Take it BEFORE starting the head snapshot: a prepare waiting
// behind a close proof must never reuse a snapshot from before the wait.
async fn nonce_gate(tx:&mut Transaction<'_,Postgres>,owner:&UserId,nonce:&str)->Result<(),StorageError>{
    sqlx::query("SELECT pg_catalog.pg_advisory_xact_lock(1680000045,pg_catalog.hashtext($1))")
        .bind(format!("{}:{}",owner.as_str(),nonce)).execute(&mut **tx).await?;
    Ok(())
}

struct Head { revision:u64, active:bool, choice:FeedbackChoice, history:Vec<Value> }
fn advance(head:&Head, input:&Input) -> Result<u64,StorageError> {
    let r=input.revision.as_ref().ok_or(StorageError::CorruptRecord)?;
    if !input.valid() || r.expected_revision!=head.revision ||
        !match r.action {
            RevisionAction::Correct=>head.active && input.choice!=head.choice,
            RevisionAction::Revoke=>head.active && input.choice==head.choice,
            RevisionAction::Resubmit=>!head.active,
        } { return Err(StorageError::FactsConflict); }
    head.revision.checked_add(1).ok_or(StorageError::RevisionConflict)
}

async fn root_request(tx:&mut Transaction<'_,Postgres>,owner:&UserId,root:&OperationId,lock:bool) -> Result<ExecutionRequest,StorageError> {
    // The deployed API has SELECT/INSERT, deliberately no UPDATE on this
    // immutable member table. Serialize every public revision with the same
    // transaction advisory lock; no new grants or root mutation are needed.
    if lock {sqlx::query("SELECT pg_advisory_xact_lock(1680000043,hashtext($1))").bind(format!("{}:{}",owner.as_str(),root.as_str())).execute(&mut **tx).await?;}
    let sql="SELECT public_operation_id FROM billing_feedback_v1 WHERE owner_user_id=$1 AND public_operation_id=$2";
    sqlx::query(sql).bind(owner.as_str()).bind(root.as_str()).fetch_optional(&mut **tx).await?.ok_or(StorageError::NotFound)?;
    let row=sqlx::query("SELECT * FROM public_execution_requests WHERE owner_id=$1 AND operation_id=$2").bind(owner.as_str()).bind(root.as_str()).fetch_one(&mut **tx).await?;
    let stored=crate::execution_request::stored_row(&row)?;
    if input(&stored.request)?.revision.is_some() {return Err(StorageError::CorruptRecord);}
    Box::pin(super::verify_result(tx,owner,&stored.request)).await?;
    let op=crate::repository::operation_row(&sqlx::query("SELECT * FROM operations WHERE operation_id=$1").bind(root.as_str()).fetch_one(&mut **tx).await?)?;
    let task=crate::repository::task_row(&sqlx::query("SELECT * FROM tasks WHERE task_id=$1 AND owner_id=$2").bind(op.task_id.as_str()).bind(owner.as_str()).fetch_one(&mut **tx).await?)?;
    validate_checkpoint(&task,std::slice::from_ref(&op))?;
    if op.state!=OperationState::Succeeded || task.state!=TaskState::Completed {return Err(StorageError::CorruptRecord);}
    Ok(stored.request)
}

async fn saved_event(tx:&mut Transaction<'_,Postgres>,owner:&UserId,request:&ExecutionRequest) -> Result<Value,StorageError> {
    let op=&request.binding.snapshot.operation_id;
    let rows=sqlx::query("SELECT e.*,EXISTS(SELECT 1 FROM public_audit_outbox o WHERE o.event_id=e.event_id) AS enqueued FROM audit_events e WHERE e.operation_id=$1 AND e.kind='OPERATION_TRANSITIONED' AND e.details->>'source'=$2")
        .bind(op.as_str()).bind(SOURCE).fetch_all(&mut **tx).await?;
    if rows.len()!=1 {return Err(StorageError::CorruptRecord);}
    let row=&rows[0];
    let details:Value=row.try_get("details")?;
    let saved=details["public_revision"].clone();
    let i=input(request)?;let r=i.revision.as_ref().ok_or(StorageError::CorruptRecord)?;
    let expected=json!({"history_contract":HISTORY,"root_operation_id":r.root_operation_id,
        "revision":r.expected_revision.checked_add(1).ok_or(StorageError::CorruptRecord)?,"previous_revision":r.expected_revision,
        "action":r.action,"active":r.action!=RevisionAction::Revoke,
        "choice":if r.action==RevisionAction::Revoke{Value::Null}else{json!(i.choice)},
        "parameters":i,"request_digest":request_digest(request)?,"owner_id":owner,
        "confirmation_ref":saved["confirmation_ref"],"session_digest":saved["session_digest"],"binding_version":saved["binding_version"],
        "card_head_adopted":false,"member_receipt_changed":false});
    if saved!=expected || details["event"]!="EXECUTION_SUCCEEDED" || details["lock_authorized"]!=false ||
        details["original_digest"]!=json!(i.original_digest) || row.try_get::<String,_>("task_id")?!=request.binding.snapshot.task_id.as_str() ||
        !row.try_get::<bool,_>("enqueued")? {return Err(StorageError::CorruptRecord);}
    Ok(saved)
}

pub(super) async fn verify_result(tx:&mut Transaction<'_,Postgres>,owner:&UserId,request:&ExecutionRequest) -> Result<(),StorageError> {
    let i=input(request)?;let r=i.revision.as_ref().ok_or(StorageError::CorruptRecord)?;
    if !i.valid() || request.payload.tool_name.as_str()!=REVISION_TOOL || request.binding.snapshot.user_id!=*owner ||
        ids(owner,&r.nonce)?.1!=request.binding.snapshot.operation_id || json!(i)!=request.payload.normalized_arguments {return Err(StorageError::CorruptRecord);}
    let root=root_request(tx,owner,&r.root_operation_id,false).await?;
    let original=input(&root)?;
    if i.report_id!=original.report_id || i.original_digest!=original.original_digest || i.transaction!=original.transaction {return Err(StorageError::CorruptRecord);}
    let saved=saved_event(tx,owner,request).await?;
    let row=sqlx::query("SELECT * FROM public_execution_requests WHERE operation_id=$1 AND owner_id=$2").bind(request.binding.snapshot.operation_id.as_str()).bind(owner.as_str()).fetch_one(&mut **tx).await?;
    let stored=crate::execution_request::stored_row(&row)?;
    if stored.request!=*request || stored.first_submission.is_none(){return Err(StorageError::CorruptRecord);}
    let op=crate::repository::operation_row(&sqlx::query("SELECT * FROM operations WHERE operation_id=$1").bind(request.binding.snapshot.operation_id.as_str()).fetch_one(&mut **tx).await?)?;
    let b=&request.binding.snapshot;
    let receipt=stored.first_submission.as_ref().ok_or(StorageError::CorruptRecord)?;
    let task=crate::repository::task_row(&sqlx::query("SELECT * FROM tasks WHERE task_id=$1 AND owner_id=$2").bind(b.task_id.as_str()).bind(owner.as_str()).fetch_one(&mut **tx).await?)?;
    let arguments=task.plan.nodes.iter().find_map(|n|match n {PlanNode::Tool{id,tool,arguments,..} if *id==b.node_id && tool.as_str()==REVISION_TOOL=>Some(arguments),_=>None});
    if op.state!=OperationState::Succeeded || op.task_id!=b.task_id || op.node_id!=b.node_id || op.plan_version!=b.plan_version || op.facts_version!=b.facts_version || op.arguments_hash!=b.arguments_hash ||
        Some(op.revision)!=b.operation_revision.checked_add(2) || op.business_operation_ref.as_ref()!=Some(&request.business_operation_ref) ||
        receipt.operation.state!=OperationState::Submitted || Some(receipt.operation.revision)!=b.operation_revision.checked_add(1) ||
        arguments!=Some(&request.payload.normalized_arguments) || task.plan.plan_version!=b.plan_version {return Err(StorageError::CorruptRecord);}
    let confirmation_ref=ConfirmationRef::parse(saved["confirmation_ref"].as_str().ok_or(StorageError::CorruptRecord)?).map_err(|_|StorageError::CorruptRecord)?;
    let c=crate::confirmation_repository::read_historical_confirmation(tx,&request.binding.snapshot.task_id,&confirmation_ref).await?;
    let rows=sqlx::query("SELECT * FROM execution_permits WHERE operation_id=$1 AND status='CONSUMED'").bind(request.binding.snapshot.operation_id.as_str()).fetch_all(&mut **tx).await?;
    if rows.len()!=1{return Err(StorageError::CorruptRecord);}
    let p=permit_row(&rows[0])?;
    if c.record.binding!=request.binding.snapshot || c.record.confirmed_at.is_none() ||
        saved["session_digest"]!=c.session_digest || saved["binding_version"]!=json!(c.session_binding_version) ||
        p.binding!=request.binding || p.payload!=request.payload || p.confirmation_ref!=confirmation_ref ||
        p.session_digest!=c.session_digest || p.session_version!=c.session_binding_version || p.execution_scope!=Some(VerifiedPermitScope::Common) {
        return Err(StorageError::CorruptRecord);
    }
    Ok(())
}

/// Complete committed chain. Revision numbers, never wall-clock/event order,
/// determine the head. No LIMIT/page tail can silently fall back to an old root.
async fn head(tx:&mut Transaction<'_,Postgres>,owner:&UserId,root:&OperationId,lock:bool)->Result<Head,StorageError>{
    let original=root_request(tx,owner,root,lock).await?;
    let base=input(&original)?;
    let mut h=Head{revision:0,active:true,choice:base.choice.clone(),history:vec![json!({"revision":0,"operation_id":root,"action":"INITIAL","active":true,"choice":base.choice})]};
    // Include committed requests AND any succeeded task claiming this root, so
    // a missing request/event is an error, not a forgotten history item.
    let rows=sqlx::query("SELECT o.* FROM operations o JOIN tasks t USING(task_id) LEFT JOIN public_execution_requests q ON q.operation_id=o.operation_id WHERE t.owner_id=$1 AND ((q.fingerprint_payload->'normalized_arguments'->'revision'->>'root_operation_id'=$2) OR (o.state='SUCCEEDED' AND t.plan->'nodes' @> jsonb_build_array(jsonb_build_object('tool','revise_billing_feedback','arguments',jsonb_build_object('revision',jsonb_build_object('root_operation_id',$2::text))))))")
        .bind(owner.as_str()).bind(root.as_str()).fetch_all(&mut **tx).await?;
    let mut chain=std::collections::BTreeMap::new();
    for row in rows {
        let op=crate::repository::operation_row(&row)?;
        if op.state!=OperationState::Succeeded{return Err(StorageError::CorruptRecord);}
        let q=sqlx::query("SELECT * FROM public_execution_requests WHERE operation_id=$1 AND owner_id=$2").bind(op.operation_id.as_str()).bind(owner.as_str()).fetch_one(&mut **tx).await?;
        let request=crate::execution_request::stored_row(&q)?.request;
        verify_result(tx,owner,&request).await?;
        let i=input(&request)?;let r=i.revision.as_ref().ok_or(StorageError::CorruptRecord)?;
        if r.root_operation_id!=*root || i.report_id!=base.report_id || i.transaction!=base.transaction || i.original_digest!=base.original_digest || chain.insert(r.expected_revision,(op,request,i)).is_some(){return Err(StorageError::CorruptRecord);}
    }
    for (previous,(op,request,i)) in chain {
        if previous!=h.revision {return Err(StorageError::CorruptRecord);}
        let next=advance(&h,&i).map_err(|_|StorageError::CorruptRecord)?;
        let task=crate::repository::task_row(&sqlx::query("SELECT * FROM tasks WHERE task_id=$1 AND owner_id=$2").bind(op.task_id.as_str()).bind(owner.as_str()).fetch_one(&mut **tx).await?)?;
        validate_checkpoint(&task,std::slice::from_ref(&op))?;
        if task.state!=TaskState::Completed{return Err(StorageError::CorruptRecord);}
        let saved=saved_event(tx,owner,&request).await?;
        h.revision=next;h.active=saved["active"]==true;h.choice=i.choice;
        h.history.push(json!({"revision":next,"operation_id":op.operation_id,"action":saved["action"],"active":h.active,"choice":saved["choice"]}));
    }
    Ok(h)
}

pub(super) async fn stage(tx:&mut Transaction<'_,Postgres>,owner:&UserId,i:&Input,request:&ExecutionRequest,confirmation:&ConfirmationRef,session_digest:&str,version:u64)->Result<Value,StorageError>{
    let r=i.revision.as_ref().ok_or(StorageError::CorruptRecord)?;
    let h=head(tx,owner,&r.root_operation_id,true).await?;
    let next=advance(&h,i)?;
    Ok(json!({"history_contract":HISTORY,"root_operation_id":r.root_operation_id,"revision":next,"previous_revision":h.revision,
        "action":r.action,"active":r.action!=RevisionAction::Revoke,"choice":if r.action==RevisionAction::Revoke{Value::Null}else{json!(i.choice)},
        "parameters":i,"request_digest":request_digest(request)?,"owner_id":owner,"confirmation_ref":confirmation,"session_digest":session_digest,
        "binding_version":version,"card_head_adopted":false,"member_receipt_changed":false}))
}

impl BillingFeedbackService<'_> {
    /// Close only the exact first durable task of an interrupted preparation.
    /// No operation/confirmation/permit/request/result may have been admitted.
    /// A task checkpoint event records cancellation, never a fabricated op receipt.
    pub async fn close_partial_revision_preparation(&self,session:&SessionRef,root:&OperationId,expected_revision:u64,action:RevisionAction,choice:Option<FeedbackChoice>,nonce:&str)->Result<Value,StorageError>{
        let identity=self.store.identity(session)?;let (task,operation)=ids(&identity.user_id,nonce)?;
        self.checked_query(session,root).await?.ok_or(StorageError::NotFound)?;
        let mut gate=self.store.repository.revision_gate_pool.begin().await?;nonce_gate(&mut gate,&identity.user_id,nonce).await?;
        let mut tx=self.store.repository.pool.begin().await?;
        // Existing global order: nonce gate -> owner/task -> root. An INSERT
        // whose outcome was uncertain must finish/rollback before this lock.
        let mut record=crate::repository::lock_task(&mut tx,&identity.user_id,&task).await?;
        let h=head(&mut tx,&identity.user_id,root,true).await?;
        let original=root_request(&mut tx,&identity.user_id,root,false).await?;let base=input(&original)?;
        let node=record.plan.nodes.iter().find_map(|n|match n{PlanNode::Tool{tool,arguments,..} if tool.as_str()==REVISION_TOOL=>Some(arguments),_=>None}).ok_or(StorageError::CorruptRecord)?;
        let i:Input=serde_json::from_value(node.clone()).map_err(|_|StorageError::CorruptRecord)?;
        let r=i.revision.as_ref().ok_or(StorageError::CorruptRecord)?;
        if !i.valid() || json!(i)!=*node || i.report_id!=base.report_id || i.transaction!=base.transaction || i.original_digest!=base.original_digest || r.root_operation_id!=*root || r.expected_revision!=expected_revision || r.action!=action || r.nonce!=nonce ||
          (action==RevisionAction::Revoke && choice.is_some()) || (action!=RevisionAction::Revoke && choice.as_ref()!=Some(&i.choice)){return Err(StorageError::CorruptRecord);}
        let prior=h.history.get(usize::try_from(expected_revision).map_err(|_|StorageError::Invalid("revision"))?).ok_or(StorageError::FactsConflict)?;
        let active=prior["active"].as_bool().ok_or(StorageError::CorruptRecord)?;
        let previous_choice=if active{serde_json::from_value(prior["choice"].clone()).map_err(|_|StorageError::CorruptRecord)?}else{base.choice};
        advance(&Head{revision:expected_revision,active,choice:previous_choice,history:vec![]},&i)?;
        let review=NodeId::parse("review_feedback").unwrap();let save=NodeId::parse("save_feedback").unwrap();
        let plan=AgentPlan::new(task.clone(),1,vec![PlanNode::UserDecision{id:review.clone(),depends_on:vec![]},PlanNode::Tool{id:save,tool:ToolName::parse(REVISION_TOOL).unwrap(),arguments:json!(i),depends_on:vec![review.clone()]}]).map_err(|_|StorageError::CorruptRecord)?;
        let digest=hash(&(plan.clone(),1u64))?;
        let stored_digest:String=sqlx::query_scalar("SELECT creation_digest FROM tasks WHERE task_id=$1").bind(task.as_str()).fetch_one(&mut *tx).await?;
        if json!(record.plan)!=json!(plan) || record.facts_version!=1 || stored_digest!=digest.as_str(){return Err(StorageError::CorruptRecord);}
        let unsafe_rows:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM operations WHERE task_id=$1 OR operation_id=$2) OR EXISTS(SELECT 1 FROM public_execution_requests WHERE task_id=$1 OR operation_id=$2) OR EXISTS(SELECT 1 FROM execution_permits WHERE task_id=$1 OR operation_id=$2) OR EXISTS(SELECT 1 FROM server_confirmations WHERE task_id=$1 OR operation_id=$2) OR EXISTS(SELECT 1 FROM public_billing_feedback_results_v1 WHERE operation_id=$2) OR EXISTS(SELECT 1 FROM billing_feedback_v1 WHERE public_operation_id=$2) OR EXISTS(SELECT 1 FROM payment_preparations WHERE task_id=$1) OR EXISTS(SELECT 1 FROM transfer_drafts_v1 WHERE task_id=$1) OR EXISTS(SELECT 1 FROM mock_draft_lifecycle_claims WHERE task_id=$1)").bind(task.as_str()).bind(operation.as_str()).fetch_one(&mut *tx).await?;
        if unsafe_rows{return Err(StorageError::ReconcileRequired);}
        // An ordinary complete preparation legitimately completed review_feedback.
        // It was classified above as inapplicable (409), not corrupt/unavailable.
        // For the actual zero-op protocol, the initial checkpoint stays strict.
        if !record.completed_nodes.is_empty(){return Err(StorageError::CorruptRecord);}
        let mut intent=json!({"input_version":1,"root_operation_id":root,"expected_revision":expected_revision,"action":action,"nonce":nonce});if let Some(c)=choice{intent["choice"]=json!(c);}
        let marker=json!({"schema":"BILLING_FEEDBACK_PARTIAL_PREPARATION_CLOSED_V1","intent":intent,"parameters_digest":hash(&i)?,"creation_digest":digest,"no_operation_created":true,"saved_opinion_changed":false});
        let events=sqlx::query("SELECT e.*,EXISTS(SELECT 1 FROM public_audit_outbox o WHERE o.event_id=e.event_id) AS enqueued FROM audit_events e WHERE e.task_id=$1 OR e.operation_id=$2 ORDER BY e.task_revision,e.event_id").bind(task.as_str()).bind(operation.as_str()).fetch_all(&mut *tx).await?;
        let closed=record.state==TaskState::Cancelled;
        if events.len()!=if closed{2}else{1}{return Err(StorageError::CorruptRecord);}
        for (index,event) in events.iter().enumerate(){
            let expected=if index==0{json!({"plan_version":1,"creation_digest":digest,"planning_origin":null})}else{marker.clone()};
            if event.try_get::<String,_>("task_id")?!=task.as_str() || event.try_get::<Option<String>,_>("operation_id")?.is_some() || event.try_get::<Option<i64>,_>("operation_revision")?.is_some() || event.try_get::<i64,_>("task_revision")?!=index as i64+1 || event.try_get::<String,_>("kind")?!=if index==0{"TASK_CREATED"}else{"CHECKPOINT_SAVED"} || event.try_get::<Value,_>("details")?!=expected || !event.try_get::<bool,_>("enqueued")? {return Err(StorageError::CorruptRecord);}
        }
        if closed{
            if record.revision!=2 || record.checkpoint!=RecoveryPoint::Finished{return Err(StorageError::CorruptRecord);}
        }else{
            if record.state!=TaskState::Running || record.revision!=1 || record.checkpoint!=(RecoveryPoint::AtNode{node_id:review}){return Err(StorageError::CorruptRecord);}
            record.state=record.state.transition(TaskEvent::Cancel).map_err(|_|StorageError::IllegalTransition)?;record.revision=2;record.checkpoint=RecoveryPoint::Finished;
            validate_checkpoint(&record,&[])?;update_task(&mut tx,&record,1).await?;
            audit(&mut tx,&record,None,PersistentAuditKind::CheckpointSaved,marker).await?;
        }
        self.guard(session,&identity)?;tx.commit().await.map_err(|_|StorageError::ReconcileRequired)?;gate.commit().await?;self.guard(session,&identity)?;
        Ok(json!({"history_contract":HISTORY,"intent":intent,"task_id":task,"operation_id":operation,"partial_preparation_closed":true,"original_preserved":true,"saved_opinion_changed":false,"automatic_retry":false}))
    }
    /// This is a proof about the EXACT uncreated intent, not a 404 recovery.
    /// The verified immutable head is strictly newer than its expected version.
    /// Every prepare with this nonce holds the same gate through task creation;
    /// after the proof, the exact old intent can never pass advance again.
    pub async fn close_uncreated_revision_intent(&self,session:&SessionRef,root:&OperationId,expected_revision:u64,action:RevisionAction,choice:Option<FeedbackChoice>,nonce:&str)->Result<Value,StorageError>{
        let identity=self.store.identity(session)?;
        let (task,operation)=ids(&identity.user_id,nonce)?;
        self.checked_query(session,root).await?.ok_or(StorageError::NotFound)?;
        let mut gate=self.store.repository.revision_gate_pool.begin().await?;
        nonce_gate(&mut gate,&identity.user_id,nonce).await?;
        let mut tx=self.store.repository.pool.begin().await?;
        // READ COMMITTED, with the same root lock as the final composer.
        // The complete verified chain cannot advance while this proof is made.
        let h=head(&mut tx,&identity.user_id,root,true).await?;
        if h.revision<=expected_revision{return Err(StorageError::FactsConflict);}
        let original=root_request(&mut tx,&identity.user_id,root,false).await?;
        let mut i=input(&original)?;
        let prior=h.history.get(usize::try_from(expected_revision).map_err(|_|StorageError::Invalid("revision"))?).ok_or(StorageError::CorruptRecord)?;
        let prior_active=prior["active"].as_bool().ok_or(StorageError::CorruptRecord)?;
        let prior_choice=if prior_active {serde_json::from_value(prior["choice"].clone()).map_err(|_|StorageError::CorruptRecord)?}else{i.choice.clone()};
        let intent=json!({"input_version":1,"root_operation_id":root,"expected_revision":expected_revision,"action":action,"nonce":nonce});
        let mut intent=intent.as_object().unwrap().clone();
        if let Some(c)=&choice{intent.insert("choice".into(),json!(c));}
        i.input_version=2;
        i.choice=if action==RevisionAction::Revoke {if choice.is_some(){return Err(StorageError::Invalid("revoke has no replacement opinion"));}prior_choice.clone()}else{choice.ok_or(StorageError::Invalid("opinion required"))?};
        i.revision=Some(Revision{root_operation_id:root.clone(),expected_revision,action,nonce:nonce.into()});
        // Also prove this was a valid intent at its stated historical version.
        advance(&Head{revision:expected_revision,active:prior_active,choice:prior_choice,history:vec![]},&i)?;
        let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tasks WHERE task_id=$1) OR EXISTS(SELECT 1 FROM operations WHERE operation_id=$2 OR task_id=$1) OR EXISTS(SELECT 1 FROM public_execution_requests WHERE operation_id=$2 OR task_id=$1) OR EXISTS(SELECT 1 FROM execution_permits WHERE operation_id=$2 OR task_id=$1) OR EXISTS(SELECT 1 FROM audit_events WHERE operation_id=$2 OR task_id=$1)")
            .bind(task.as_str()).bind(operation.as_str()).fetch_one(&mut *tx).await?;
        if exists{return Err(StorageError::ReconcileRequired);}
        self.guard(session,&identity)?;
        let proof=json!({"history_contract":HISTORY,"intent":intent,"task_id":task,"operation_id":operation,
            "state":"STALE_UNCREATED_INTENT","current_revision":h.revision,"owner_scoped":true,"not_created":true,
            "may_end_local_intent":true,"original_preserved":true,"saved_opinion_changed":false,"automatic_retry":false});
        tx.commit().await?;gate.commit().await?;self.guard(session,&identity)?;Ok(proof)
    }
    /// Explicitly abandon only an unsubmitted revision preparation. The same
    /// owner/task lock as execution excludes an in-flight commit; saved versions
    /// and their original receipts cannot be abandoned here.
    pub async fn abandon_revision_preparation(&self,session:&SessionRef,nonce:&str,root:&OperationId)->Result<Value,StorageError>{
        let identity=self.store.identity(session)?;
        self.checked_query(session,root).await?.ok_or(StorageError::NotFound)?;
        let (task,operation)=ids(&identity.user_id,nonce)?;
        let mut gate=self.store.repository.revision_gate_pool.begin().await?;nonce_gate(&mut gate,&identity.user_id,nonce).await?;
        let mut tx=self.store.repository.pool.begin().await?;
        let mut record=crate::repository::lock_task(&mut tx,&identity.user_id,&task).await?;
        let mut ops=operations(&mut tx,&task).await?;
        if ops.len()!=1 || ops[0].operation_id!=operation{return Err(StorageError::CorruptRecord);}
        let arguments=record.plan.nodes.iter().find_map(|n|match n {PlanNode::Tool{tool,arguments,..} if tool.as_str()==REVISION_TOOL=>Some(arguments),_=>None}).ok_or(StorageError::CorruptRecord)?;
        let i:Input=serde_json::from_value(arguments.clone()).map_err(|_|StorageError::CorruptRecord)?;
        let r=i.revision.as_ref().ok_or(StorageError::CorruptRecord)?;
        if !i.valid() || r.nonce!=nonce || r.root_operation_id!=*root || json!(i)!=*arguments{return Err(StorageError::CorruptRecord);}
        let admitted:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public_execution_requests WHERE operation_id=$1) OR EXISTS(SELECT 1 FROM execution_permits WHERE operation_id=$1)").bind(operation.as_str()).fetch_one(&mut *tx).await?;
        if admitted || ops[0].business_operation_ref.is_some(){return Err(StorageError::ReconcileRequired);}
        let proof=json!({"schema":"BILLING_FEEDBACK_REVISION_PREPARATION_CLOSED_V1","nonce":nonce,"root_operation_id":root,"parameters_digest":hash(&i)?,"saved_opinion_changed":false});
        let markers:Vec<Value>=sqlx::query_scalar("SELECT details FROM audit_events WHERE operation_id=$1 AND details->>'schema'='BILLING_FEEDBACK_REVISION_PREPARATION_CLOSED_V1'").bind(operation.as_str()).fetch_all(&mut *tx).await?;
        if record.state==TaskState::Cancelled&&ops[0].state==OperationState::Cancelled{
            if markers!=vec![proof]{return Err(StorageError::CorruptRecord);}
        }else{
            if !markers.is_empty() || record.state!=TaskState::Running || !matches!(ops[0].state,OperationState::Draft|OperationState::Ready){return Err(StorageError::ExecutionOperationConflict);}
            let old=ops[0].revision;
            ops[0].state=ops[0].state.transition(OperationEvent::Cancel).map_err(|_|StorageError::IllegalTransition)?;
            ops[0].revision=old.checked_add(1).ok_or(StorageError::RevisionConflict)?;
            let changed=sqlx::query("UPDATE operations SET state='CANCELLED',revision=$1,updated_at=clock_timestamp() WHERE operation_id=$2 AND revision=$3").bind(db_version(ops[0].revision)?).bind(operation.as_str()).bind(db_version(old)?).execute(&mut *tx).await?;
            if changed.rows_affected()!=1{return Err(StorageError::RevisionConflict);}
            let old=record.revision;record.revision=old.checked_add(1).ok_or(StorageError::RevisionConflict)?;
            record.state=record.state.transition(TaskEvent::Cancel).map_err(|_|StorageError::IllegalTransition)?;record.checkpoint=RecoveryPoint::Finished;
            validate_checkpoint(&record,&ops)?;update_task(&mut tx,&record,old).await?;
            audit(&mut tx,&record,Some(&ops[0]),PersistentAuditKind::OperationTransitioned,proof).await?;
        }
        self.guard(session,&identity)?;tx.commit().await?;gate.commit().await?;self.guard(session,&identity)?;
        Ok(json!({"history_contract":HISTORY,"task_id":task,"operation_id":operation,"nonce":nonce,"root_operation_id":root,"preparation_closed":true,"saved_opinion_changed":false,"automatic_retry":false}))
    }
    pub async fn revision_preparation(&self,session:&SessionRef,nonce:&str)->Result<Value,StorageError>{
        let identity=self.store.identity(session)?;
        let (task,op)=ids(&identity.user_id,nonce)?;
        let mut gate=self.store.repository.revision_gate_pool.begin().await?;nonce_gate(&mut gate,&identity.user_id,nonce).await?;
        let snapshot=PersistenceService::for_trusted_backend(self.store.repository.clone(),self.store.sessions.clone()).recover(session,&task).await?;
        let node=snapshot.task.plan.nodes.iter().find_map(|n|match n {PlanNode::Tool{tool,arguments,..} if tool.as_str()==REVISION_TOOL=>Some(arguments),_=>None}).ok_or(StorageError::CorruptRecord)?;
        let i:Input=serde_json::from_value(node.clone()).map_err(|_|StorageError::CorruptRecord)?;
        let r=i.revision.as_ref().ok_or(StorageError::CorruptRecord)?;
        if !i.valid() || r.nonce!=nonce || json!(i)!=*node{return Err(StorageError::CorruptRecord);}
        self.checked_query(session,&r.root_operation_id).await?.ok_or(StorageError::NotFound)?;
        let result=self.query_revision(session,&op).await?;
        self.guard(session,&identity)?;gate.commit().await?;
        Ok(json!({"history_contract":HISTORY,"task_id":task,"operation_id":op,"parameters":i,"task_state":snapshot.task.state,"saved":result.map(|v|v.saved),"confirmation_restored":false,"automatic_retry":false}))
    }
    pub async fn feedback_head(&self,session:&SessionRef,root:&OperationId)->Result<Value,StorageError>{
        let identity=self.store.identity(session)?;
        self.checked_query(session,root).await?.ok_or(StorageError::NotFound)?;
        let mut tx=self.store.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await?;
        let h=head(&mut tx,&identity.user_id,root,false).await?;
        tx.commit().await?;
        // Recheck current account/session authority after all history awaits.
        self.checked_query(session,root).await?.ok_or(StorageError::NotFound)?;
        self.guard(session,&identity)?;
        Ok(json!({"history_contract":HISTORY,"root_operation_id":root,"revision":h.revision,"active":h.active,"choice":if h.active{json!(h.choice)}else{Value::Null},"history":h.history,"card_head_adopted":false,"member_receipt_changed":false,"lock_authorized":false}))
    }
    pub async fn prepare_revision(&self,session:&SessionRef,root:&OperationId,expected_revision:u64,action:RevisionAction,choice:Option<FeedbackChoice>,nonce:&str)->Result<BillingFeedbackPreparation,StorageError>{
        let identity=self.store.identity(session)?;
        let (task,op)=ids(&identity.user_id,nonce)?;
        self.checked_query(session,root).await?.ok_or(StorageError::NotFound)?;
        let mut gate=self.store.repository.revision_gate_pool.begin().await?;
        nonce_gate(&mut gate,&identity.user_id,nonce).await?;
        let mut tx=self.store.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await?;
        let cancelled:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tasks WHERE task_id=$1 AND owner_id=$2 AND state='CANCELLED')").bind(task.as_str()).bind(identity.user_id.as_str()).fetch_one(&mut *tx).await?;
        if cancelled{return Err(StorageError::ExecutionOperationConflict);}
        let h=head(&mut tx,&identity.user_id,root,false).await?;
        let original=root_request(&mut tx,&identity.user_id,root,false).await?;
        let mut i=input(&original)?;
        tx.commit().await?;
        i.input_version=2;
        i.choice=if action==RevisionAction::Revoke {if choice.is_some(){return Err(StorageError::Invalid("revoke has no replacement opinion"));}h.choice.clone()}else{choice.ok_or(StorageError::Invalid("opinion required"))?};
        i.revision=Some(Revision{root_operation_id:root.clone(),expected_revision,action,nonce:nonce.into()});
        advance(&h,&i)?;
        self.guard(session,&identity)?;
        let prepared=self.prepare_input(session,i,op,task).await?;
        self.guard(session,&identity)?;gate.commit().await?;Ok(prepared)
    }
    pub(super) async fn query_revision(&self,session:&SessionRef,op:&OperationId)->Result<Option<BillingFeedbackView>,StorageError>{
        let identity=self.store.identity(session)?;
        let row=sqlx::query("SELECT * FROM public_execution_requests WHERE owner_id=$1 AND operation_id=$2").bind(identity.user_id.as_str()).bind(op.as_str()).fetch_optional(&self.store.repository.pool).await?;
        let Some(row)=row else {return Ok(None)};
        let request=crate::execution_request::stored_row(&row)?.request;
        if request.payload.tool_name.as_str()!=REVISION_TOOL {return Ok(None);}
        let i=input(&request)?;let r=i.revision.as_ref().ok_or(StorageError::CorruptRecord)?;
        self.checked_query(session,&r.root_operation_id).await?.ok_or(StorageError::NotFound)?;
        let mut tx=self.store.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await?;
        // Check the whole current chain too. A damaged later version must be
        // reported; the returned original receipt itself is never replaced by head.
        head(&mut tx,&identity.user_id,&r.root_operation_id,false).await?;
        verify_result(&mut tx,&identity.user_id,&request).await?;
        let mut saved=saved_event(&mut tx,&identity.user_id,&request).await?;
        let operation=crate::repository::operation_row(&sqlx::query("SELECT * FROM operations WHERE operation_id=$1").bind(op.as_str()).fetch_one(&mut *tx).await?)?;
        let task=crate::repository::task_row(&sqlx::query("SELECT * FROM tasks WHERE task_id=$1 AND owner_id=$2").bind(operation.task_id.as_str()).bind(identity.user_id.as_str()).fetch_one(&mut *tx).await?)?;
        validate_checkpoint(&task,std::slice::from_ref(&operation))?;
        if operation.state!=OperationState::Succeeded || task.state!=TaskState::Completed {return Err(StorageError::CorruptRecord);}
        tx.commit().await?;
        self.checked_query(session,&r.root_operation_id).await?.ok_or(StorageError::NotFound)?;
        self.guard(session,&identity)?;
        if let Some(object)=saved.as_object_mut(){for name in ["session_digest","binding_version","confirmation_ref","owner_id","request_digest"] {object.remove(name);}}
        Ok(Some(BillingFeedbackView{operation,task_state:task.state,saved,replayed:true,domain_audit:BillingDomainAuditStatus::PublicHistoryOnlyV1}))
    }
    pub(super) async fn check_revision_replay(&self,session:&SessionRef,task:&TaskId,op:&OperationId,confirmation:&ConfirmationRef)->Result<(),StorageError>{
        let identity=self.store.identity(session)?;
        let view=self.query_revision(session,op).await?.ok_or(StorageError::NotFound)?;
        let mut tx=self.store.repository.pool.begin().await?;
        let row=sqlx::query("SELECT * FROM public_execution_requests WHERE owner_id=$1 AND operation_id=$2").bind(identity.user_id.as_str()).bind(op.as_str()).fetch_one(&mut *tx).await?;
        let request=crate::execution_request::stored_row(&row)?.request;
        let saved=saved_event(&mut tx,&identity.user_id,&request).await?;
        if view.operation.task_id!=*task || saved["confirmation_ref"]!=json!(confirmation) || saved["session_digest"]!=json!(hash(&("safeflow-confirmation-session:v1",session.as_str()))?) || saved["binding_version"]!=json!(identity.binding_version){return Err(StorageError::NotFound);}
        tx.commit().await?;
        self.guard(session,&identity)
    }
}
