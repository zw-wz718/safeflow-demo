use super::*;
use crate::confirmation::{timestamp, ConfirmationSession};
use crate::confirmation_repository::{check_active_at, read_stored_confirmation};
use crate::models::{db_version, valid_id};
use crate::repository::{audit, lock_task_for_mode, operations, update_task};
use safeflow_contracts::{
    AgentPlan, ConfirmationRef, PlanningResult, SecurityGate, SecurityLevel, SecurityOutcome,
    SecurityPhase, TaskEvent, ToolName, Understanding, UnderstandingDecision,
};

fn ticket() -> String {
    format!("lifecycle_{}", uuid::Uuid::new_v4().simple())
}
fn node(version: u32, decision: bool) -> NodeId {
    NodeId::parse(format!(
        "{}_{}",
        if decision { "decision" } else { "action" },
        version
    ))
    .expect("server node")
}
fn tail(
    version: u32,
    action: MockDraftAction,
    parameters: Value,
    dependencies: Vec<NodeId>,
) -> Vec<PlanNode> {
    let mut nodes = vec![PlanNode::Tool {
        id: node(version, false),
        tool: ToolName::parse(action.tool_name()).expect("existing tool"),
        arguments: parameters,
        depends_on: dependencies,
    }];
    if action != MockDraftAction::Cancel {
        nodes.push(PlanNode::UserDecision {
            id: node(version, true),
            depends_on: vec![node(version, false)],
        });
    }
    nodes
}
fn proposal(plan: AgentPlan) -> PlanningResult {
    PlanningResult::for_plan(
        Understanding {
            intent: "MOCK_DRAFT_LIFECYCLE_ONLY".into(),
            entities: json!({}),
            missing_fields: vec![],
            context_refs: vec![],
            decision: UnderstandingDecision::Pass,
        },
        plan,
    )
}

impl LifecycleService<'_> {
    pub async fn create(
        &self,
        session: &SessionRef,
        id: &TaskId,
        parameters: Value,
        facts_version: u64,
    ) -> Result<LifecycleView, StorageError> {
        let mut tx=self.repository.pool.begin().await?;
        self.insert_new_intent(&mut tx,session,id,parameters,facts_version).await?;
        tx.commit().await?;
        self.query(session,id,None).await
    }

    pub(super) async fn insert_new_intent(
        &self, tx:&mut Transaction<'_,Postgres>,session:&SessionRef,id:&TaskId,
        parameters:Value,facts_version:u64,
    )->Result<(),StorageError>{
        valid_id(id.as_str())?;
        db_version(facts_version)?;
        let binding = self.identity(session)?;
        let plan = AgentPlan::new(
            id.clone(),
            1,
            tail(1, MockDraftAction::Create, parameters.clone(), vec![]),
        )
        .map_err(|_| StorageError::PlanConflict)?;
        let raw =
            serde_json::to_vec(&proposal(plan.clone())).map_err(|_| StorageError::CorruptRecord)?;
        let review = self
            .intake
            .accept_json_for_persistence(session, id, 1, &raw)
            .await
            .map_err(intake_error)?;
        self.intake
            .check_persistence_review(&review)
            .map_err(intake_error)?;
        let task = TaskRecord {
            task_id: id.clone(),
            owner_id: binding.user_id.clone(),
            state: TaskState::Running,
            revision: 1,
            facts_version,
            plan,
            checkpoint: RecoveryPoint::AtNode {
                node_id: node(1, false),
            },
            completed_nodes: vec![],
        };
        let operation = new_action(&task, MockDraftAction::Create, &parameters)?;
        let selection = ticket();
        let mut request =
            json!({"action":MockDraftAction::Create.action(),"parameters":parameters});
        if let Some(source) = self.planning_source {
            request["planning_source"] = json!(source);
        }
        self.retain_payee_query(&mut request, MockDraftAction::Create, &parameters);
        let history = PlanHistory {
            plan: task.plan.clone(),
            plan_digest: hash(&task.plan)?,
            registration_hash: review.registration_hash(),
            completed_prefix: vec![],
            previous_digest: None,
        };
        self.repository.retain_recipient_profile(tx, &binding.user_id, &mut request, &parameters["payee_id"]).await?;
        if request.get("draft_restore_query").is_none(){
            if let Some(p)=PgRepository::recipient_from_request(tx,&binding.user_id,&request,&parameters["payee_id"]).await?{
                request["draft_restore_query"]=json!({"payee_id":p.payee_id,"query":p.display_name});
            }
        }
        self.guard(session, &binding)?;
        let inserted = sqlx::query("INSERT INTO tasks(task_id,owner_id,contract_version,state,revision,plan_version,facts_version,plan,creation_digest,checkpoint,completed_nodes) VALUES($1,$2,1,'RUNNING',1,1,$3,$4,$5,$6,'[]') ON CONFLICT(task_id) DO NOTHING")
            .bind(id.as_str()).bind(binding.user_id.as_str()).bind(db_version(facts_version)?).bind(Json(&task.plan))
            .bind(hash(&(task.plan.clone(), facts_version))?.as_str()).bind(Json(&task.checkpoint)).execute(&mut **tx).await?;
        if inserted.rows_affected() != 1 {
            return Err(StorageError::IdConflict);
        }
        sqlx::query("INSERT INTO mock_draft_lifecycle_tasks(task_id,lifecycle_version,scope,initial_task,initial_digest,registration_hash,active_operation,selection_ticket) VALUES($1,2,'MOCK_DRAFT_LIFECYCLE_ONLY',$2,$3,$4,$5,$6)")
            .bind(id.as_str()).bind(Json(&task)).bind(hash(&task)?.as_str()).bind(history.registration_hash.as_str()).bind(operation.operation_id.as_str()).bind(&selection).execute(&mut **tx).await?;
        insert_plan_action(
            tx,
            &task,
            &operation,
            &history,
            MockDraftAction::Create,
            &parameters,
            &ticket(),
            &request,
            None,
        )
        .await?;
        audit(tx, &task, None, PersistentAuditKind::TaskCreated, json!({"lifecycle_origin":{"initial_digest":hash(&task)?,"registration_hash":history.registration_hash,"initial_task":task}})).await?;
        checked_history(tx, &task, &[operation]).await?;
        self.intake
            .check_persistence_review(&review)
            .map_err(intake_error)?;
        self.guard(session, &binding)?;
        Ok(())
    }

    pub async fn select(
        &self,
        session: &SessionRef,
        id: &TaskId,
        input: LifecycleSelection,
    ) -> Result<LifecycleView, StorageError> {
        valid_id(&input.selection_ticket)?;
        let identity = self.identity(session)?;
        let mut request = json!({"selection_ticket":input.selection_ticket,"task_revision":input.task_revision,"plan_version":input.plan_version,"facts_version":input.facts_version,"action":input.action.action(),"parameters":input.parameters});
        if let Some(source) = self.planning_source {
            request["planning_source"] = json!(source);
        }
        self.retain_payee_query(&mut request, input.action, &input.parameters);
        let mut tx = self.repository.pool.begin().await?;
        let old = lock_task_for_mode(&mut tx, &identity.user_id, id, true).await?;
        let mut ops = operations(&mut tx, id).await?;
        let histories = checked_history(&mut tx, &old, &ops).await?;
        checked_claims(&mut tx, &old, &ops).await?;
        if let Some(existing) = sqlx::query("SELECT operation_id,request FROM mock_draft_lifecycle_actions WHERE task_id=$1 AND request_ticket=$2")
            .bind(id.as_str()).bind(&input.selection_ticket).fetch_optional(&mut *tx).await? {
            let mut original = existing.try_get::<Json<Value>,_>("request")?.0;
            // Adoption later must not change an idempotent old selection's snapshot.
            original.as_object_mut().ok_or(StorageError::CorruptRecord)?.remove("recipient_profile_revision");
            if original != request { return Err(StorageError::DecisionConflict); }
            let operation: OperationId = crate::repository::decode(&existing, "operation_id")?;
            self.guard(session, &identity)?;
            tx.commit().await?;
            let mut view=self.query(session, id, Some(&operation)).await?;
            view.replayed=true;
            return Ok(view);
        }
        reject_unknown(&mut tx, id).await?;
        if old.state.is_terminal() {
            return Err(StorageError::Terminal);
        }
        if old.revision != input.task_revision {
            return Err(StorageError::RevisionConflict);
        }
        if old.plan.plan_version != input.plan_version {
            return Err(StorageError::StalePlan);
        }
        if old.facts_version != input.facts_version {
            return Err(StorageError::FactsConflict);
        }
        let row = sqlx::query("SELECT * FROM mock_draft_lifecycle_tasks WHERE task_id=$1")
            .bind(id.as_str())
            .fetch_one(&mut *tx)
            .await?;
        if row.try_get::<String, _>("selection_ticket")? != input.selection_ticket {
            return Err(StorageError::DecisionConflict);
        }
        let active: OperationId = crate::repository::decode(&row, "active_operation")?;
        let active_op = ops
            .iter()
            .find(|op| op.operation_id == active)
            .ok_or(StorageError::CorruptRecord)?
            .clone();
        let observed = self
            .observe_for_action(&mut tx, session, &identity, &old, &active_op, input.action)
            .await?;
        if input.action == MockDraftAction::Create {
            let original = sqlx::query("SELECT parameters FROM mock_draft_lifecycle_actions WHERE task_id=$1 AND plan_version=1").bind(id.as_str()).fetch_one(&mut *tx).await?;
            if original.try_get::<Json<Value>, _>("parameters")?.0 != input.parameters {
                return Err(StorageError::PlanConflict);
            }
        } else {
            let draft = observed
                .as_ref()
                .ok_or(StorageError::DependencyUnavailable)?;
            if input.parameters["draft_id"] != json!(draft.draft_id)
                || input.parameters["expected_version"] != json!(draft.draft_version)
            {
                return Err(StorageError::ConfirmationBindingMismatch);
            }
        }
        if input.action != MockDraftAction::Cancel {
            let payee = if input.action == MockDraftAction::Create {&input.parameters["payee_id"]}
                else {input.parameters["changes"].get("payee_id").unwrap_or(&observed.as_ref().ok_or(StorageError::DependencyUnavailable)?.parameters["payee_id"])};
            self.repository.retain_recipient_profile(&mut tx, &identity.user_id, &mut request, payee).await?;
        }
        let next_version = old
            .plan
            .plan_version
            .checked_add(1)
            .ok_or(StorageError::PlanConflict)?;
        let mut task = old.clone();
        if old.state == TaskState::WaitingUser {
            let RecoveryPoint::AtNode { node_id } = &old.checkpoint else {
                return Err(StorageError::CorruptRecord);
            };
            task.completed_nodes.push(node_id.clone());
            task.state = old
                .state
                .transition(TaskEvent::UserResponded)
                .map_err(|_| StorageError::IllegalTransition)?;
        } else if old.state == TaskState::Running && active_op.state == OperationState::Draft {
            let claimed: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM mock_draft_lifecycle_claims WHERE operation_id=$1)",
            )
            .bind(active.as_str())
            .fetch_one(&mut *tx)
            .await?;
            if claimed {
                return Err(StorageError::RuntimeAlreadyClaimed);
            }
        } else {
            return Err(StorageError::RuntimeWaitingNodeMismatch);
        }
        let mut nodes: Vec<_> = old
            .plan
            .nodes
            .iter()
            .filter(|n| task.completed_nodes.contains(n.id()))
            .cloned()
            .collect();
        let dependencies = nodes
            .last()
            .map(|n| vec![n.id().clone()])
            .unwrap_or_default();
        nodes.extend(tail(
            next_version,
            input.action,
            input.parameters.clone(),
            dependencies,
        ));
        task.plan = AgentPlan::new(id.clone(), next_version, nodes)
            .map_err(|_| StorageError::PlanConflict)?;
        validate_plan(&task.plan)?;
        if serde_json::to_vec(&task.plan)
            .map_err(|_| StorageError::CorruptRecord)?
            .len()
            > 65536
        {
            return Err(StorageError::PlanConflict);
        }
        let raw = serde_json::to_vec(&proposal(task.plan.clone()))
            .map_err(|_| StorageError::CorruptRecord)?;
        let review = self
            .intake
            .accept_json_for_persistence(session, id, next_version, &raw)
            .await
            .map_err(intake_error)?;
        self.intake
            .check_persistence_review(&review)
            .map_err(intake_error)?;
        if histories
            .last()
            .ok_or(StorageError::CorruptRecord)?
            .registration_hash
            != review.registration_hash()
        {
            return Err(StorageError::DecisionAssemblyChanged);
        }
        task.revision = task
            .revision
            .checked_add(1)
            .ok_or(StorageError::RevisionConflict)?;
        task.checkpoint = RecoveryPoint::AtNode {
            node_id: node(next_version, false),
        };
        if active_op.state == OperationState::Draft {
            let op = ops
                .iter_mut()
                .find(|op| op.operation_id == active)
                .ok_or(StorageError::CorruptRecord)?;
            op.state = op
                .state
                .transition(safeflow_contracts::OperationEvent::Cancel)
                .map_err(|_| StorageError::IllegalTransition)?;
            op.revision = op
                .revision
                .checked_add(1)
                .ok_or(StorageError::RevisionConflict)?;
            sqlx::query("UPDATE operations SET state='CANCELLED',revision=$1 WHERE operation_id=$2 AND revision=$3").bind(db_version(op.revision)?).bind(active.as_str()).bind(db_version(active_op.revision)?).execute(&mut *tx).await?;
            sqlx::query(
                "UPDATE mock_draft_lifecycle_actions SET archived=true WHERE operation_id=$1",
            )
            .bind(active.as_str())
            .execute(&mut *tx)
            .await?;
            audit(
                &mut tx,
                &task,
                Some(op),
                PersistentAuditKind::OperationTransitioned,
                json!({"event":"CANCEL","lifecycle_archived":true}),
            )
            .await?;
        }
        let operation = new_action(&task, input.action, &input.parameters)?;
        let history = PlanHistory {
            plan: task.plan.clone(),
            plan_digest: hash(&task.plan)?,
            registration_hash: review.registration_hash(),
            completed_prefix: task.completed_nodes.clone(),
            previous_digest: Some(
                histories
                    .last()
                    .ok_or(StorageError::CorruptRecord)?
                    .plan_digest
                    .clone(),
            ),
        };
        insert_plan_action(
            &mut tx,
            &task,
            &operation,
            &history,
            input.action,
            &input.parameters,
            &input.selection_ticket,
            &request,
            observed.as_ref(),
        )
        .await?;
        sqlx::query("UPDATE mock_draft_lifecycle_tasks SET active_operation=$1,selection_ticket=$2 WHERE task_id=$3").bind(operation.operation_id.as_str()).bind(ticket()).bind(id.as_str()).execute(&mut *tx).await?;
        update_task(&mut tx, &task, old.revision).await?;
        ops.push(operation);
        checked_history(&mut tx, &task, &ops).await?;
        self.intake
            .check_persistence_review(&review)
            .map_err(intake_error)?;
        self.guard(session, &identity)?;
        tx.commit().await?;
        self.query(session, id, None).await
    }

    pub async fn query(
        &self,
        session: &SessionRef,
        id: &TaskId,
        selected: Option<&OperationId>,
    ) -> Result<LifecycleView, StorageError> {
        let identity = self.identity(session)?;
        let mut tx = self.repository.pool.begin().await?;
        let task = lock_task_for_mode(&mut tx, &identity.user_id, id, true).await?;
        let ops = operations(&mut tx, id).await?;
        checked_history(&mut tx, &task, &ops).await?;
        let claims = checked_claims(&mut tx, &task, &ops).await?;
        let unknown_operation = claims
            .iter()
            .find(|claim| claim.receipt.is_none())
            .map(|claim| claim.operation.clone());
        let row = sqlx::query("SELECT active_operation,selection_ticket FROM mock_draft_lifecycle_tasks WHERE task_id=$1").bind(id.as_str()).fetch_one(&mut *tx).await?;
        let active: OperationId = crate::repository::decode(&row, "active_operation")?;
        let selected = selected.unwrap_or(&active);
        let operation = ops
            .iter()
            .find(|op| &op.operation_id == selected)
            .ok_or(StorageError::NotFound)?
            .clone();
        let action_row = sqlx::query(
            "SELECT * FROM mock_draft_lifecycle_actions WHERE operation_id=$1 AND task_id=$2",
        )
        .bind(selected.as_str())
        .bind(id.as_str())
        .fetch_one(&mut *tx)
        .await?;
        let intent_proof=super::intent::proof(&mut tx,&task,&ops).await?;
        let intent_note=super::intent::note(&mut tx,&identity.user_id,id).await?;
        let mut view = LifecycleView {
            planning_source: action_row
                .try_get::<Json<Value>, _>("request")?
                .0
                .get("planning_source")
                .map(|source| {
                    serde_json::from_value(source.clone()).map_err(|_| StorageError::CorruptRecord)
                })
                .transpose()?,
            replayed: claims.iter().any(|claim| &claim.operation == selected),
            task,
            operations: ops,
            operation,
            active_operation_id: active.clone(),
            action: action_for(&action_row.try_get::<String, _>("action")?)?,
            parameters: action_row.try_get::<Json<Value>, _>("parameters")?.0,
            observed_draft: action_row
                .try_get::<Option<Json<LifecycleDraftObservation>>, _>("observed_draft")?
                .map(|v| v.0),
            selection_ticket: row.try_get("selection_ticket")?,
            is_current_action: selected == &active,
            unknown_operation,
            receipt: claims
                .iter()
                .find(|claim| &claim.operation == selected)
                .and_then(|claim| claim.receipt.clone()),
            confirmation: None,
            pre_tool: None,
            pre_auth: None,
            display_ticket: None,
            confirmation_recipient: None,
            recipient_reconfirmation_required: false,
            intent_termination: intent_note,
            intent_expires_at: intent_proof.and_then(|v|serde_json::from_value(v["expires_at"].clone()).ok()),
        };
        let request = action_row.try_get::<Json<Value>, _>("request")?.0;
        if request.get("recipient_profile_revision").is_some() {
            let payee = if view.action == MockDraftAction::Create {&view.parameters["payee_id"]}
                else {view.parameters["changes"].get("payee_id").unwrap_or(&view.observed_draft.as_ref().ok_or(StorageError::CorruptRecord)?.parameters["payee_id"])};
            view.confirmation_recipient = PgRepository::recipient_from_request(&mut tx, &identity.user_id, &request, payee).await?;
        }
        if request.get("recipient_profile_revision").is_none()
            && view.action != MockDraftAction::Cancel && view.receipt.is_none()
            && view.is_current_action && view.operation.state == OperationState::Draft {
            let payee = if view.action == MockDraftAction::Create {&view.parameters["payee_id"]}
                else {view.parameters["changes"].get("payee_id").unwrap_or(&view.observed_draft.as_ref().ok_or(StorageError::CorruptRecord)?.parameters["payee_id"])};
            let payee = ResourceId::parse(payee.as_str().ok_or(StorageError::CorruptRecord)?).map_err(|_|StorageError::CorruptRecord)?;
            view.recipient_reconfirmation_required = PgRepository::recipient_profile_on(&mut tx, &identity.user_id, &payee).await?.is_some();
        }
        if view.intent_termination.is_none() && view.is_current_action
            && !view.recipient_reconfirmation_required
            && view.unknown_operation.is_none()
            && view.task.state == TaskState::Running
            && view.operation.state == OperationState::Draft
        {
            let display = self.display(&mut tx, session, &identity, &view).await;
            match display {
                Ok(Some((confirmation, a, b, ticket))) => {
                    view.confirmation = Some(confirmation);
                    view.pre_tool = Some(a);
                    view.pre_auth = Some(b);
                    view.display_ticket = Some(ticket);
                }
                Ok(None)
                | Err(
                    StorageError::DecisionExpired
                    | StorageError::DecisionInactive
                    | StorageError::DecisionAssemblyChanged
                    | StorageError::ConfirmationExpired
                    | StorageError::ConfirmationRevoked
                    | StorageError::NotFound,
                ) => {}
                Err(error) => return Err(error),
            }
        }
        self.guard(session, &identity)?;
        tx.commit().await?;
        Ok(view)
    }

    pub async fn observe_current(
        &self,
        session: &SessionRef,
        id: &TaskId,
        operation: &OperationId,
    ) -> Result<LifecycleDraftObservation, StorageError> {
        let identity = self.identity(session)?;
        let view = self.query(session, id, Some(operation)).await?;
        let draft = self
            .observer
            .observe(&Self::context(&view.task, &view.operation))
            .await?;
        self.guard(session, &identity)?;
        if let Some(draft) = draft {
            return Ok(draft);
        }
        if !self.transfer_sql_bridge {
            return Err(StorageError::DependencyUnavailable);
        }
        let mut tx = self.repository.pool.begin().await?;
        let task = lock_task_for_mode(&mut tx, &identity.user_id, id, true).await?;
        let ops = operations(&mut tx, id).await?;
        checked_history(&mut tx, &task, &ops).await?;
        checked_claims(&mut tx, &task, &ops).await?;
        reject_unknown(&mut tx, id).await?;
        let op = ops
            .iter()
            .find(|op| &op.operation_id == operation)
            .ok_or(StorageError::NotFound)?;
        let draft = self
            .observe_for_action(
                &mut tx,
                session,
                &identity,
                &task,
                op,
                MockDraftAction::Modify,
            )
            .await?
            .ok_or(StorageError::DependencyUnavailable)?;
        self.guard(session, &identity)?;
        tx.commit().await?;
        Ok(draft)
    }

    pub(super) async fn observe_for_action(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        session: &SessionRef,
        identity: &SessionBinding,
        task: &TaskRecord,
        op: &OperationRecord,
        action: MockDraftAction,
    ) -> Result<Option<LifecycleDraftObservation>, StorageError> {
        let actual = self.observer.observe(&Self::context(task, op)).await?;
        self.guard(session, identity)?;
        let create_claim: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mock_draft_lifecycle_claims WHERE task_id=$1 AND action='CREATE_MOCK_DRAFT')").bind(task.task_id.as_str()).fetch_one(&mut **tx).await?;
        if action == MockDraftAction::Create {
            if create_claim || actual.is_some() {
                return Err(StorageError::RuntimeAlreadyClaimed);
            }
            return Ok(None);
        }
        let last: Option<String> = sqlx::query_scalar(
            "SELECT last_success FROM mock_draft_lifecycle_tasks WHERE task_id=$1",
        )
        .bind(task.task_id.as_str())
        .fetch_one(&mut **tx)
        .await?;
        let receipt: Json<LifecycleReceipt> = sqlx::query_scalar("SELECT receipt FROM mock_draft_lifecycle_claims WHERE operation_id=$1 AND task_id=$2 AND status='SUCCEEDED'")
            .bind(last.ok_or(StorageError::DependencyUnavailable)?).bind(task.task_id.as_str()).fetch_optional(&mut **tx).await?.ok_or(StorageError::DependencyUnavailable)?;
        let draft = match actual {
            Some(draft) => draft,
            None if self.transfer_sql_bridge => {
                self.restore_sql_draft(tx, session, identity, task, op, &receipt.0)
                    .await?
            }
            None => return Err(StorageError::DependencyUnavailable),
        };
        if draft.draft_id != receipt.0.draft_id
            || draft.business_operation_id != receipt.0.business_operation_id
            || draft.draft_version != receipt.0.draft_version
            || draft.status != "READY_TO_CONFIRM"
            || draft.expires_at <= timestamp(self.clock.as_ref())?
            || draft.parameters != result_parameters(&receipt.0.result.data)?
            || json!(draft.expires_at) != receipt.0.result.data["expires_at"]
        {
            return Err(StorageError::DependencyUnavailable);
        }
        if self.transfer_sql_bridge {
            self.check_sql_draft_before(tx, &Self::context(task, op), &draft)
                .await?;
            self.guard(session, identity)?;
        }
        Ok(Some(draft))
    }

    pub async fn prepare(
        &self,
        session: &SessionRef,
        id: &TaskId,
        operation: &OperationId,
    ) -> Result<LifecycleView, StorageError> {
        let identity = self.identity(session)?;
        let view = self.query(session, id, Some(operation)).await?;
        ensure_active(&view)?;
        let mut tx = self.repository.pool.begin().await?;
        let task = lock_task_for_mode(&mut tx, &identity.user_id, id, true).await?;
        let ops = operations(&mut tx, id).await?;
        reject_unknown(&mut tx, id).await?;
        let history = checked_history(&mut tx, &task, &ops).await?;
        if self
            .intake
            .registration_hash_for_backend()
            .map_err(intake_error)?
            != history
                .last()
                .ok_or(StorageError::CorruptRecord)?
                .registration_hash
        {
            return Err(StorageError::DecisionAssemblyChanged);
        }
        let binding = binding_for_mode(&mut tx, &task, &ops, operation, true).await?;
        if task != view.task {
            return Err(StorageError::RevisionConflict);
        }
        let actual = self
            .observe_for_action(
                &mut tx,
                session,
                &identity,
                &task,
                &view.operation,
                view.action,
            )
            .await?;
        if actual != view.observed_draft {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let session_digest = hash(&("safeflow-confirmation-session:v1", session.as_str()))?;
        let anchor = binding
            .lifecycle_anchor
            .as_ref()
            .ok_or(StorageError::CorruptRecord)?;
        sqlx::query("INSERT INTO mock_draft_lifecycle_displays(task_id,operation_id,session_digest,binding_version,lifecycle_anchor,ticket,pre_tool_request,pre_auth_request) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(operation_id,session_digest,binding_version) DO NOTHING")
            .bind(id.as_str()).bind(operation.as_str()).bind(session_digest.as_str()).bind(db_version(identity.binding_version)?).bind(anchor.as_str()).bind(ticket()).bind(DecisionRequestId::new_server_id().as_str()).bind(DecisionRequestId::new_server_id().as_str()).execute(&mut *tx).await?;
        let journal = sqlx::query("SELECT * FROM mock_draft_lifecycle_displays WHERE operation_id=$1 AND session_digest=$2 AND binding_version=$3")
            .bind(operation.as_str()).bind(session_digest.as_str()).bind(db_version(identity.binding_version)?).fetch_one(&mut *tx).await?;
        if journal.try_get::<String, _>("lifecycle_anchor")? != anchor.as_str() {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let a_id = DecisionRequestId::parse(journal.try_get::<String, _>("pre_tool_request")?)?;
        let b_id = DecisionRequestId::parse(journal.try_get::<String, _>("pre_auth_request")?)?;
        let display_ticket: String = journal.try_get("ticket")?;
        self.guard(session, &identity)?;
        tx.commit().await?;
        let a = self
            .decisions
            .evaluate_and_save(session, id, operation, &a_id, SecurityPhase::PreTool)
            .await?;
        let b = self
            .decisions
            .evaluate_and_save(session, id, operation, &b_id, SecurityPhase::PreAuth)
            .await?;
        check_preflight(&a, SecurityPhase::PreTool)?;
        check_preflight(&b, SecurityPhase::PreAuth)?;
        let c = self.confirmations.prepare(session, id, operation).await?;
        if a.binding != binding || b.binding != binding || c.binding != binding {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let display = display_value(&view, &a, &b, &c)?;
        let mut tx = self.repository.pool.begin().await?;
        let task = lock_task_for_mode(&mut tx, &identity.user_id, id, true).await?;
        let ops = operations(&mut tx, id).await?;
        reject_unknown(&mut tx, id).await?;
        if binding_for_mode(&mut tx, &task, &ops, operation, true).await? != binding {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        self.decisions
            .read_display_sources(
                &mut tx,
                session,
                &task,
                &ops,
                &[a.stored_decision_id.clone(), b.stored_decision_id.clone()],
            )
            .await?;
        if self
            .observe_for_action(
                &mut tx,
                session,
                &identity,
                &task,
                &view.operation,
                view.action,
            )
            .await?
            != view.observed_draft
        {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let updated = sqlx::query("UPDATE mock_draft_lifecycle_displays SET pre_tool=$1,pre_auth=$2,confirmation_ref=$3,display_digest=$4,display=$5 WHERE ticket=$6 AND (display IS NULL OR display=$5)")
            .bind(a.stored_decision_id.as_str()).bind(b.stored_decision_id.as_str()).bind(c.confirmation_ref.as_str()).bind(hash(&display)?.as_str()).bind(Json(&display)).bind(&display_ticket).execute(&mut *tx).await?;
        if updated.rows_affected() != 1 {
            return Err(StorageError::DecisionConflict);
        }
        self.guard(session, &identity)?;
        check_active_at(&c, timestamp(self.clock.as_ref())?)?;
        tx.commit().await?;
        self.query(session, id, Some(operation)).await
    }

    pub async fn act(
        &self,
        session: &SessionRef,
        id: &TaskId,
        operation: &OperationId,
        reference: &ConfirmationRef,
        display_ticket: &str,
        action: PlanningConfirmationAction,
    ) -> Result<LifecycleView, StorageError> {
        let view = self.query(session, id, Some(operation)).await?;
        ensure_active(&view)?;
        let confirmation = view
            .confirmation
            .as_ref()
            .ok_or(StorageError::ConfirmationRequired)?;
        if &confirmation.confirmation_ref != reference
            || view.display_ticket.as_deref() != Some(display_ticket)
        {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        let a = view
            .pre_tool
            .as_ref()
            .ok_or(StorageError::DecisionSourcesUnavailable)?;
        let b = view
            .pre_auth
            .as_ref()
            .ok_or(StorageError::DecisionSourcesUnavailable)?;
        self.decisions
            .verify(session, id, &a.stored_decision_id, &confirmation.binding)
            .await?;
        self.decisions
            .verify(session, id, &b.stored_decision_id, &confirmation.binding)
            .await?;
        match action {
            PlanningConfirmationAction::Confirm => {
                self.confirmations
                    .record_explicit_confirmation(session, id, reference, &confirmation.binding)
                    .await?;
            }
            PlanningConfirmationAction::Reject => {
                self.confirmations
                    .reject_prepared(session, id, reference, &confirmation.binding)
                    .await?;
            }
        }
        self.query(session, id, Some(operation)).await
    }

    async fn display(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        session: &SessionRef,
        identity: &SessionBinding,
        view: &LifecycleView,
    ) -> Result<
        Option<(
            ConfirmationRecord,
            StoredSecurityDecision,
            StoredSecurityDecision,
            String,
        )>,
        StorageError,
    > {
        let row = sqlx::query("SELECT * FROM mock_draft_lifecycle_displays WHERE operation_id=$1 AND session_digest=$2 AND binding_version=$3")
            .bind(view.operation.operation_id.as_str()).bind(hash(&("safeflow-confirmation-session:v1",session.as_str()))?.as_str()).bind(db_version(identity.binding_version)?).fetch_optional(&mut **tx).await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let Some(display) = row.try_get::<Option<Json<Value>>, _>("display")? else {
            return Ok(None);
        };
        let a_id = StoredDecisionId::parse(row.try_get::<String, _>("pre_tool")?)?;
        let b_id = StoredDecisionId::parse(row.try_get::<String, _>("pre_auth")?)?;
        let records = self
            .decisions
            .read_display_sources(tx, session, &view.task, &view.operations, &[a_id, b_id])
            .await?;
        let [a, b] = records.as_slice() else {
            return Err(StorageError::CorruptRecord);
        };
        check_preflight(a, SecurityPhase::PreTool)?;
        check_preflight(b, SecurityPhase::PreAuth)?;
        let c_id: ConfirmationRef = crate::repository::decode(&row, "confirmation_ref")?;
        let c = read_stored_confirmation(tx, &view.task.task_id, &c_id).await?;
        c.check_session(&ConfirmationSession {
            binding: identity.clone(),
            digest: hash(&("safeflow-confirmation-session:v1", session.as_str()))?,
        })?;
        check_active_at(&c.record, timestamp(self.clock.as_ref())?)?;
        let binding = binding_for_mode(
            tx,
            &view.task,
            &view.operations,
            &view.operation.operation_id,
            true,
        )
        .await?;
        if c.record.binding != binding
            || a.binding != binding
            || b.binding != binding
            || binding.lifecycle_anchor.as_ref().map(|v| v.as_str())
                != Some(row.try_get::<String, _>("lifecycle_anchor")?.as_str())
            || display.0 != display_value(view, a, b, &c.record)?
            || hash(&display.0)?.as_str() != row.try_get::<String, _>("display_digest")?
        {
            return Err(StorageError::CorruptRecord);
        }
        Ok(Some((
            c.record,
            a.clone(),
            b.clone(),
            row.try_get("ticket")?,
        )))
    }
}

fn intake_error(error: safeflow_runtime::PlanningIntakeError) -> StorageError {
    match error {
        safeflow_runtime::PlanningIntakeError::Session => StorageError::Session,
        _ => StorageError::RuntimeIntakeRequired,
    }
}
fn new_action(
    task: &TaskRecord,
    action: MockDraftAction,
    parameters: &Value,
) -> Result<OperationRecord, StorageError> {
    let operation = OperationRecord {
        operation_id: new_operation_id(),
        task_id: task.task_id.clone(),
        node_id: node(task.plan.plan_version, false),
        state: OperationState::Draft,
        revision: 1,
        plan_version: task.plan.plan_version,
        facts_version: task.facts_version,
        arguments_hash: hash(parameters)?,
        business_operation_ref: None,
    };
    if task
        .plan
        .nodes
        .iter()
        .find(|n| n.id() == &operation.node_id)
        .and_then(|n| {
            if let PlanNode::Tool { tool, .. } = n {
                Some(tool.as_str())
            } else {
                None
            }
        })
        != Some(action.tool_name())
    {
        return Err(StorageError::CorruptRecord);
    }
    Ok(operation)
}
#[allow(clippy::too_many_arguments)]
async fn insert_plan_action(
    tx: &mut Transaction<'_, Postgres>,
    task: &TaskRecord,
    op: &OperationRecord,
    history: &PlanHistory,
    action: MockDraftAction,
    parameters: &Value,
    request_ticket: &str,
    request: &Value,
    observed: Option<&LifecycleDraftObservation>,
) -> Result<(), StorageError> {
    sqlx::query("INSERT INTO mock_draft_lifecycle_plans(task_id,plan_version,plan,plan_digest,registration_hash,completed_prefix,previous_version,previous_digest) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(task.task_id.as_str()).bind(task.plan.plan_version as i64).bind(Json(&task.plan)).bind(history.plan_digest.as_str()).bind(history.registration_hash.as_str()).bind(Json(&history.completed_prefix)).bind(if task.plan.plan_version==1 {None} else {Some((task.plan.plan_version-1) as i64)}).bind(history.previous_digest.as_ref().map(|v|v.as_str())).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO operations(operation_id,task_id,node_id,state,revision,plan_version,facts_version,arguments_hash) VALUES($1,$2,$3,'DRAFT',1,$4,$5,$6)")
        .bind(op.operation_id.as_str()).bind(op.task_id.as_str()).bind(op.node_id.as_str()).bind(op.plan_version as i64).bind(db_version(op.facts_version)?).bind(op.arguments_hash.as_str()).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO mock_draft_lifecycle_actions(operation_id,task_id,node_id,plan_version,action,parameters,request_ticket,request,request_digest,observed_draft) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
        .bind(op.operation_id.as_str()).bind(task.task_id.as_str()).bind(op.node_id.as_str()).bind(op.plan_version as i64).bind(action.action()).bind(Json(parameters)).bind(request_ticket).bind(Json(request)).bind(hash(request)?.as_str()).bind(observed.map(Json)).execute(&mut **tx).await?;
    audit(tx,task,Some(op),PersistentAuditKind::OperationCreated,json!({"lifecycle_action":action_origin(op,action,parameters,request_ticket,request,observed)?})).await?;
    audit(
        tx,
        task,
        Some(op),
        PersistentAuditKind::PlanReplaced,
        json!({"lifecycle_plan":history}),
    )
    .await?;
    Ok(())
}

pub(super) fn action_origin(
    op: &OperationRecord,
    action: MockDraftAction,
    parameters: &Value,
    request_ticket: &str,
    request: &Value,
    observed: Option<&LifecycleDraftObservation>,
) -> Result<Value, StorageError> {
    Ok(
        json!({"operation_id":op.operation_id,"task_id":op.task_id,"node_id":op.node_id,"plan_version":op.plan_version,"action":action.action(),"parameters":parameters,"request_ticket":request_ticket,"request":request,"request_digest":hash(request)?,"observed_draft":observed}),
    )
}
pub(super) async fn reject_unknown(
    tx: &mut Transaction<'_, Postgres>,
    id: &TaskId,
) -> Result<(), StorageError> {
    super::execution::reject_closed_nonfunding_draft(tx,id).await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM mock_draft_lifecycle_claims WHERE task_id=$1 AND status='CLAIMED')").bind(id.as_str()).fetch_one(&mut **tx).await?;
    if exists {
        return Err(StorageError::RuntimeAlreadyClaimed);
    }
    Ok(())
}
pub(super) fn ensure_active(view: &LifecycleView) -> Result<(), StorageError> {
    if let Some(note) = &view.intent_termination {
        // query verified the complete task-level note before producing this view.
        return Err(super::intent::ended_error(note));
    }
    if view.recipient_reconfirmation_required && view.unknown_operation.is_none() {
        return Err(StorageError::FactsConflict);
    }
    if view.unknown_operation.is_some() {
        return Err(StorageError::RuntimeAlreadyClaimed);
    }
    if !view.is_current_action
        || view.task.state != TaskState::Running
        || view.operation.state != OperationState::Draft
    {
        return Err(StorageError::Terminal);
    }
    Ok(())
}
pub(super) fn check_preflight(
    record: &StoredSecurityDecision,
    phase: SecurityPhase,
) -> Result<(), StorageError> {
    if record.source_mode != SOURCE_MODE
        || record.phase != phase
        || record.evidence_kind != "PREFLIGHT_ONLY"
        || record.decision.outcome != SecurityOutcome::ReadyToConfirm
        || record.decision.required_gates != [SecurityGate::ExplicitConfirmation]
        || record.decision.minimum_level != SecurityLevel::Green
    {
        return Err(StorageError::DecisionSourcesUnavailable);
    }
    Ok(())
}
pub(super) fn result_parameters(data: &Value) -> Result<Value, StorageError> {
    let mut parameters = serde_json::Map::new();
    for key in [
        "payer_account_id",
        "payee_id",
        "amount",
        "currency",
        "remark",
    ] {
        parameters.insert(
            key.into(),
            data.get(key).ok_or(StorageError::CorruptRecord)?.clone(),
        );
    }
    Ok(Value::Object(parameters))
}
fn display_value(
    view: &LifecycleView,
    a: &StoredSecurityDecision,
    b: &StoredSecurityDecision,
    c: &ConfirmationRecord,
) -> Result<Value, StorageError> {
    let mut display = json!({"action":view.action.action(),"parameters":view.parameters,"observed_draft":view.observed_draft,"binding":a.binding,"pre_tool":a.stored_decision_id,"pre_auth":b.stored_decision_id,"confirmation_ref":c.confirmation_ref,"prepared_at":c.prepared_at,"expires_at":c.expires_at});
    if let Some(recipient) = &view.confirmation_recipient {
        display["confirmation_recipient"] = value(recipient)?;
    }
    Ok(display)
}
