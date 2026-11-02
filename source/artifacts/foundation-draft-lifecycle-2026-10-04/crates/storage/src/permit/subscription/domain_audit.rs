//! Explicit isolated native host. No HTTP/Tool/default activation, no member algorithms.
use super::*;
use crate::subscription_local_audit::{checked_details, LocalAction, LocalDetails, KIND, MAPPING};

/// Operator-only exact scope; cannot be deserialized or passed as a write flag.
pub struct SubscriptionA2CandidateScope {
    namespace: String,
    owner: UserId,
    revision: String,
}
impl SubscriptionA2CandidateScope {
    pub(super) fn for_team_source(source: &crate::TeamConsumptionSource, owner: UserId) -> Result<Self,StorageError> {
        let repo=source.repository_for_backend();
        if !repo.namespace.starts_with("sf_unified_") || source.owner()!=owner.as_str() {
            return Err(StorageError::TransactionScope);
        }
        Ok(Self{namespace:repo.namespace,owner,revision:crate::TEAM_CONSUMPTION_REVISION.into()})
    }
    pub fn for_isolated_backend(
        namespace: &str,
        owner: UserId,
        revision: &str,
    ) -> Result<Self, StorageError> {
        valid_id(namespace)?;
        valid_id(revision)?;
        if !(namespace.starts_with("sf_subscription_test_")
            || namespace.starts_with("sf_http_test_"))
        {
            return Err(StorageError::TransactionScope);
        }
        Ok(Self {
            namespace: namespace.into(),
            owner,
            revision: revision.into(),
        })
    }
}
/// Opaque native service. It does not expose its underlying default service.
pub struct SubscriptionA2Candidate<'a> {
    inner: SubscriptionWriteService<'a>,
    owner: UserId,
}
#[derive(Debug, Serialize)]
pub struct SubscriptionA2Result {
    pub original: Value,
    pub domain_audit: Value,
}

pub(super) async fn writer_lock(tx: &mut Transaction<'_, Postgres>) -> Result<(), StorageError> {
    sqlx::query("SELECT pg_advisory_xact_lock_shared(1680000029,hashtext(current_schema()))")
        .execute(&mut **tx)
        .await?;
    Ok(())
}
pub(super) async fn check_source(
    tx: &mut Transaction<'_, Postgres>,
    owner: &str,
    expected: Option<&String>,
) -> Result<Option<String>, StorageError> {
    let row:Option<(String,bool)>=sqlx::query_as("SELECT source_id::text,database_name=current_database() AND database_oid=(SELECT oid FROM pg_database WHERE datname=current_database()) AND namespace=current_schema() AND namespace_oid=current_schema()::regnamespace::oid AND server_port=current_setting('port')::integer AND member_codec='safeflow.subscription.fictional-journal.v1' AND mapping_version=$2 FROM public_subscription_a2_sources_v1 WHERE owner_id=$1")
        .bind(owner).bind(MAPPING).fetch_optional(&mut **tx).await?;
    match row {
        Some((id, true)) if expected.is_none_or(|e| *e == id) => Ok(Some(id)),
        None if expected.is_none() => Ok(None),
        _ => Err(StorageError::TransactionScope),
    }
}
impl PgRepository {
    /// Stop relevant writers before this explicit isolated cutover. SQL locks
    /// drain old writers; each exact pre-existing successful O is enumerated.
    pub async fn install_subscription_a2_candidate_for_isolated_backend(
        &self,
        scope: &SubscriptionA2CandidateScope,
    ) -> Result<String, StorageError> {
        if self.namespace != scope.namespace {
            return Err(StorageError::TransactionScope);
        }
        let mut tx = self.pool.begin().await?;
        let isolated:bool=sqlx::query_scalar("SELECT current_database() LIKE 'billing_test_%' OR current_database() LIKE 'subscription_test_%'").fetch_one(&mut *tx).await?;
        if !isolated {
            return Err(StorageError::TransactionScope);
        }
        sqlx::query("SELECT pg_advisory_xact_lock(1680000029,hashtext(current_schema()))")
            .execute(&mut *tx)
            .await?;
        sqlx::query("LOCK TABLE subscription_runtime_records,subscription_public_bindings_v1,subscription_preparations_v1 IN SHARE ROW EXCLUSIVE MODE").execute(&mut *tx).await?;
        if check_source(&mut tx, scope.owner.as_str(), None)
            .await?
            .is_some()
        {
            return Err(StorageError::IdConflict);
        }
        let operations:Vec<String>=sqlx::query_scalar("SELECT b.operation_id FROM subscription_public_bindings_v1 b JOIN operations o USING(operation_id) WHERE b.owner_id=$1 AND o.state='SUCCEEDED' ORDER BY b.operation_id").bind(scope.owner.as_str()).fetch_all(&mut *tx).await?;
        sqlx::query(
            "SELECT set_config('safeflow.subscription_a2_install',txid_current()::text,true)",
        )
        .execute(&mut *tx)
        .await?;
        let id = uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO public_subscription_a2_sources_v1(source_id,owner_id,source_revision,database_name,database_oid,namespace,namespace_oid,server_port,member_codec,mapping_version,legacy_count) SELECT $1::uuid,$2,$3,current_database(),oid,current_schema(),current_schema()::regnamespace::oid,current_setting('port')::integer,'safeflow.subscription.fictional-journal.v1',$4,$5 FROM pg_database WHERE datname=current_database()")
            .bind(&id).bind(scope.owner.as_str()).bind(&scope.revision).bind(MAPPING).bind(operations.len() as i64).execute(&mut *tx).await?;
        // No synthetic session is needed to check historical codec facts. This
        // service uses no session resolution during its connection-only load.
        let read = SubscriptionService::for_trusted_backend(self.clone(), Arc::new(NoSession));
        for op in operations {
            let operation = OperationId::parse(op).map_err(|_| StorageError::CorruptRecord)?;
            let f = checked_facts(&mut tx, &scope.owner, &operation, &read).await?;
            sqlx::query("INSERT INTO public_subscription_a2_legacy_v1(operation_id,business_ref,owner_id,source_id,facts_digest,facts) VALUES($1,$2,$3,$4::uuid,$5,$6)").bind(operation.as_str()).bind(&f.details.business_ref).bind(scope.owner.as_str()).bind(&id).bind(f.digest.as_str()).bind(&f.raw).execute(&mut *tx).await?;
        }
        sqlx::query("SET CONSTRAINTS ALL IMMEDIATE")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(id)
    }
}
// A rejecting resolver is used for codec-only historical installation reads;
// it cannot authorize a user, create a permit or run business.
struct NoSession;
impl SessionResolver for NoSession {
    fn resolve(&self, _: &SessionRef) -> Result<UserId, SecurityError> {
        Err(SecurityError {
            reason_code: ReasonCode::parse("CANDIDATE_NO_SESSION").unwrap(),
            message: "historical codec read only".into(),
        })
    }
}
impl<'a> SubscriptionWriteService<'a> {
    pub async fn into_isolated_a2_candidate(
        mut self,
        scope: &SubscriptionA2CandidateScope,
    ) -> Result<SubscriptionA2Candidate<'a>, StorageError> {
        let id = self
            .store
            .repository
            .install_subscription_a2_candidate_for_isolated_backend(scope)
            .await?;
        self.candidate_activation = Some(id);
        Ok(SubscriptionA2Candidate {
            inner: self,
            owner: scope.owner.clone(),
        })
    }
    /// Reopen the same installed generation after restart; never silently installs.
    pub async fn open_isolated_a2_candidate(
        mut self,
        scope: &SubscriptionA2CandidateScope,
    ) -> Result<SubscriptionA2Candidate<'a>, StorageError> {
        if self.store.repository.namespace != scope.namespace {
            return Err(StorageError::TransactionScope);
        }
        let mut tx = self.store.repository.pool.begin().await?;
        let id = check_source(&mut tx, scope.owner.as_str(), None)
            .await?
            .ok_or(StorageError::DependencyUnavailable)?;
        let revision:String=sqlx::query_scalar("SELECT source_revision FROM public_subscription_a2_sources_v1 WHERE source_id=$1::uuid").bind(&id).fetch_one(&mut *tx).await?;
        let db: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&mut *tx)
            .await?;
        if revision != scope.revision
            || !(db.starts_with("billing_test_") || db.starts_with("subscription_test_"))
        {
            return Err(StorageError::TransactionScope);
        }
        tx.commit().await?;
        self.candidate_activation = Some(id);
        Ok(SubscriptionA2Candidate {
            inner: self,
            owner: scope.owner.clone(),
        })
    }
}
impl SubscriptionA2Candidate<'_> {
    async fn check(&self, s: &SessionRef) -> Result<(), StorageError> {
        let identity = self.inner.read.identity(s)?;
        if identity.user_id != self.owner {
            return Err(StorageError::NotFound);
        }
        let mut tx = self.inner.store.repository.pool.begin().await?;
        check_source(
            &mut tx,
            self.owner.as_str(),
            self.inner.candidate_activation.as_ref(),
        )
        .await?;
        tx.commit().await?;
        self.inner.read.check(s, &identity)
    }
    pub async fn prepare_change(
        &self,
        s: &SessionRef,
        host: &SubscriptionHost,
        nonce: &str,
        r: SubscriptionChangeRequestMock,
    ) -> Result<SubscriptionPreparation, StorageError> {
        self.check(s).await?;
        self.inner
            .prepare_technical(s, host, nonce, Preparation::Change(r))
            .await
    }
    pub async fn capture_registration(
        &self,
        s: &SessionRef,
        h: &SubscriptionHost,
        r: &safeflow_billing_source_reader::query::Request,
        p: &impl safeflow_billing_source_reader::query::PagedSource,
        l: safeflow_billing_source_reader::query::Limits,
    ) -> Result<SubscriptionRegistrationCapture, StorageError> {
        self.check(s).await?;
        self.inner
            .capture_registration_for_trusted_backend(s, h, r, p, l)
            .await
    }
    pub async fn select_registration_from_capture(
        &self,
        s: &SessionRef,
        h: &SubscriptionHost,
        c: SubscriptionRegistrationCapture,
        i: usize,
    ) -> Result<SubscriptionRegistrationSelection, StorageError> {
        self.check(s).await?;
        self.inner
            .select_registration_from_capture_for_trusted_backend(s, h, c, i)
            .await
    }
    pub async fn prepare_registration(
        &self,
        s: &SessionRef,
        h: &SubscriptionHost,
        n: &str,
        c: &SubscriptionRegistrationSelection,
    ) -> Result<SubscriptionPreparation, StorageError> {
        self.check(s).await?;
        self.inner
            .prepare_technical(s, h, n, Preparation::Registration(c))
            .await
    }
    pub async fn recover_preparation(
        &self,
        s: &SessionRef,
        n: &str,
    ) -> Result<SubscriptionPreparation, StorageError> {
        self.check(s).await?;
        let identity = self.inner.read.identity(s)?;
        let op:String=sqlx::query_scalar("SELECT b.operation_id FROM subscription_preparations_v1 p JOIN subscription_public_bindings_v1 b USING(business_ref) WHERE p.nonce=$1 AND b.owner_id=$2").bind(n).bind(identity.user_id.as_str()).fetch_optional(&self.inner.store.repository.pool).await?.ok_or(StorageError::NotFound)?;
        let op = OperationId::parse(op).map_err(|_| StorageError::CorruptRecord)?;
        self.query(s, &op).await?;
        // Preserve the original terminal-task refusal. Reading the saved link
        // is not permission to turn a completed operation back into a draft.
        self.inner.recover_technical(s, n).await
    }
    pub async fn confirm(
        &self,
        s: &SessionRef,
        t: &TaskId,
        c: &ConfirmationRef,
    ) -> Result<ConfirmationRecord, StorageError> {
        self.check(s).await?;
        self.inner.confirm_technical(s, t, c).await
    }
    pub async fn execute(
        &self,
        s: &SessionRef,
        h: &SubscriptionHost,
        t: &TaskId,
        o: &OperationId,
        c: &ConfirmationRef,
    ) -> Result<SubscriptionA2Result, StorageError> {
        self.check(s).await?;
        self.inner.execute_technical(s, h, t, o, c).await?;
        self.query(s, o).await
    }
    pub async fn query(
        &self,
        s: &SessionRef,
        o: &OperationId,
    ) -> Result<SubscriptionA2Result, StorageError> {
        self.check(s).await?;
        let identity = self.inner.read.identity(s)?;
        let mut tx = self
            .inner
            .store
            .repository
            .subscription_read_pool
            .begin()
            .await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .execute(&mut *tx)
            .await?;
        let row=sqlx::query("SELECT original_request,public_parameters,member_binding FROM subscription_public_bindings_v1 WHERE owner_id=$1 AND operation_id=$2").bind(identity.user_id.as_str()).bind(o.as_str()).fetch_optional(&mut *tx).await?.ok_or(StorageError::NotFound)?;
        let input: Input = serde_json::from_value(row.try_get("public_parameters")?)
            .map_err(|_| StorageError::CorruptRecord)?;
        let member: Value = row.try_get("member_binding")?;
        let original = serde_json::from_value(row.try_get("original_request")?)
            .map_err(|_| StorageError::CorruptRecord)?;
        let snap = self
            .inner
            .read
            .load(&mut tx, identity.user_id.as_str())
            .await?;
        let status = match snap
            .original_result_mock(&original)
            .map_err(|_| StorageError::CorruptRecord)?
        {
            OriginalResultMock::NeedsReview => json!({"status":"UNEXECUTED_OR_NEEDS_REVIEW"}),
            OriginalResultMock::Recorded { .. } => {
                verify_link(&mut tx, &identity.user_id, o, &self.inner.read).await?
            }
        };
        let result = SubscriptionA2Result {
            original: json!(snap
                .original_view_mock(&original)
                .map_err(|_| StorageError::CorruptRecord)?),
            domain_audit: status,
        };
        self.inner.read.check(s, &identity)?;
        tx.commit().await?;
        self.inner
            .check_accounts(s, &identity, &input, &member)
            .await?;
        Ok(result)
    }
}

struct Facts {
    details: LocalDetails,
    raw: Value,
    digest: ArgumentsHash,
}
async fn checked_facts(
    tx: &mut Transaction<'_, Postgres>,
    owner: &UserId,
    operation: &OperationId,
    read: &SubscriptionService,
) -> Result<Facts, StorageError> {
    let raw:Value=sqlx::query_scalar("SELECT to_jsonb(b) FROM subscription_public_bindings_v1 b WHERE owner_id=$1 AND operation_id=$2").bind(owner.as_str()).bind(operation.as_str()).fetch_optional(&mut **tx).await?.ok_or(StorageError::NotFound)?;
    let input: Input = serde_json::from_value(raw["public_parameters"].clone())
        .map_err(|_| StorageError::CorruptRecord)?;
    let member = &raw["member_binding"];
    MemberDeadline::from_binding(&input, member)?;
    let original: OriginalOperationRequestMock =
        serde_json::from_value(raw["original_request"].clone())
            .map_err(|_| StorageError::CorruptRecord)?;
    let snap = read.load(tx, owner.as_str()).await?;
    verify_receipt(&snap, &original, member, &input)?; // actual historical typed getters, D1/D2
    let context: MockOperationContext = serde_json::from_value(member["context"].clone())
        .map_err(|_| StorageError::CorruptRecord)?;
    let records = snap
        .business_records_mock()
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| StorageError::CorruptRecord)?;
    // RawBusinessRecordMock is the existing validated codec's backend archive,
    // not a serialized private permission/selection or a client-provided receipt.
    let matched: Vec<_> = records
        .iter()
        .filter(|r| r["original_binding"] == *member)
        .collect();
    if matched.len() != 1 {
        return Err(StorageError::CorruptRecord);
    }
    let r = matched[0];
    let command = &r["original_command"];
    let receipt = &command["receipt"];
    let action = match input.action {
        Action::Registration => LocalAction::Registration,
        Action::ReminderSettings => LocalAction::ReminderSettings,
        Action::RecordCancellation => LocalAction::RecordCancellation,
        Action::RevokeCancellation => LocalAction::RevokeCancellation,
    };
    let kind = match action {
        LocalAction::Registration => "Register",
        LocalAction::ReminderSettings => "Settings",
        _ => "Intent",
    };
    if command["kind"] != kind
        || raw["original_request"]["command_kind"] != kind
        || raw["original_request"]["original_binding"] != *member
        || context.user_id != owner.as_str()
        || context.operation_id != operation.as_str()
        || context.idempotency_key != operation.as_str()
        || receipt["operation_id"] != operation.as_str()
        || input.business_ref != raw["business_ref"]
        || input.input_version != 2
        || json!(input) != raw["public_parameters"]
    {
        return Err(StorageError::CorruptRecord);
    }
    match action {
        LocalAction::Registration if receipt["state"] != "ACTIVE" => {
            return Err(StorageError::CorruptRecord)
        }
        LocalAction::RecordCancellation
            if member["kind"] != "RECORD"
                || receipt["kind"] != "RECORD"
                || receipt["local_intent_state"] != "RECORDED"
                || receipt["ledger_state_at_operation"] != "CANCELLING" =>
        {
            return Err(StorageError::CorruptRecord)
        }
        LocalAction::RevokeCancellation
            if member["kind"] != "REVOKE"
                || receipt["kind"] != "REVOKE"
                || receipt["local_intent_state"] != "REVOKED"
                || receipt["ledger_state_at_operation"] != "ACTIVE" =>
        {
            return Err(StorageError::CorruptRecord)
        }
        _ => {}
    }
    let details = LocalDetails {
        mapping_version: MAPPING.into(),
        action,
        result: action.result().into(),
        business_ref: input.business_ref.clone(),
        business_occurred_at: command["now"]
            .as_str()
            .ok_or(StorageError::CorruptRecord)?
            .into(),
        business_time_source: "FICTIONAL_COMMAND_NOW".into(),
        fictional: receipt["fictional"]
            .as_bool()
            .ok_or(StorageError::CorruptRecord)?,
        production_registered: receipt["production_registered"]
            .as_bool()
            .ok_or(StorageError::CorruptRecord)?,
        notification_sent: receipt["notification_sent"]
            .as_bool()
            .ok_or(StorageError::CorruptRecord)?,
    };
    checked_details(&json!(details))?;
    let request_row = sqlx::query("SELECT * FROM public_execution_requests WHERE operation_id=$1")
        .bind(operation.as_str())
        .fetch_one(&mut **tx)
        .await?;
    let stored = crate::execution_request::stored_row(&request_row)?;
    let request = &stored.request;
    request.validate()?;
    let b = &request.binding.snapshot;
    if request.business_operation_ref.as_str() != input.business_ref
        || request.payload.resource_id.as_str() != input.business_ref
        || request.payload.normalized_arguments != json!(input)
        || b.user_id != *owner
        || b.operation_id != *operation
        || b.tool_name.as_str() != TOOL
        || context.task_id != b.task_id.as_str()
        || context.node_id != b.node_id.as_str()
        || context.plan_version != b.plan_version
        || hash(member)? != input.member_digest
        || stored.first_submission.is_none()
        || raw["idempotency_key"] != context.idempotency_key
        || raw["session_digest"]
            != json!(hash(&(
                "safeflow-confirmation-session:v1",
                context.session_ref.as_str()
            ))?)
    {
        return Err(StorageError::CorruptRecord);
    }
    let permits =
        sqlx::query("SELECT * FROM execution_permits WHERE operation_id=$1 AND status='CONSUMED'")
            .bind(operation.as_str())
            .fetch_all(&mut **tx)
            .await?;
    if permits.len() != 1 {
        return Err(StorageError::CorruptRecord);
    }
    let p = permit_row(&permits[0])?;
    let confirmation = crate::confirmation_repository::read_historical_confirmation(
        tx,
        &b.task_id,
        &p.confirmation_ref,
    )
    .await?;
    if p.binding != request.binding
        || p.payload != request.payload
        || p.execution_scope != Some(VerifiedPermitScope::Common)
        || confirmation.record.binding != *b
        || confirmation.record.confirmed_at.is_none()
        || p.session_digest != confirmation.session_digest
        || p.session_version != confirmation.session_binding_version
        || raw["session_digest"] != p.session_digest
        || raw["binding_version"] != p.session_version
    {
        return Err(StorageError::CorruptRecord);
    }
    let task_row = sqlx::query("SELECT * FROM tasks WHERE task_id=$1 AND owner_id=$2")
        .bind(b.task_id.as_str())
        .bind(owner.as_str())
        .fetch_one(&mut **tx)
        .await?;
    let task = crate::repository::task_row(&task_row)?;
    let ops = operations(tx, &b.task_id).await?;
    let op = ops.first().ok_or(StorageError::CorruptRecord)?;
    validate_plan(&task.plan)?;
    validate_checkpoint(&task, &ops)?;
    if ops.len() != 1
        || task.plan.nodes.len() != 2
        || task.state != TaskState::Completed
        || task.revision != b.task_revision + 3
        || op.state != OperationState::Succeeded
        || op.operation_id != *operation
        || op.node_id != b.node_id
        || op.plan_version != b.plan_version
        || op.facts_version != b.facts_version
        || op.arguments_hash != b.arguments_hash
        || op.revision != b.operation_revision + 2
        || op.business_operation_ref.as_ref() != Some(&request.business_operation_ref)
    {
        return Err(StorageError::CorruptRecord);
    }
    let position = r["source_command_position"]
        .as_u64()
        .ok_or(StorageError::CorruptRecord)? as usize;
    let mut journal: Value =
        sqlx::query_scalar("SELECT journal FROM subscription_runtime_records WHERE owner_id=$1")
            .bind(owner.as_str())
            .fetch_one(&mut **tx)
            .await?;
    let commands = journal["commands"]
        .as_array_mut()
        .ok_or(StorageError::CorruptRecord)?;
    if commands.get(position) != Some(command) {
        return Err(StorageError::CorruptRecord);
    }
    commands.truncate(position + 1);
    let request_raw: Value = sqlx::query_scalar(
        "SELECT to_jsonb(q) FROM public_execution_requests q WHERE operation_id=$1",
    )
    .bind(operation.as_str())
    .fetch_one(&mut **tx)
    .await?;
    let db_task: Value = sqlx::query_scalar("SELECT to_jsonb(t) FROM tasks t WHERE task_id=$1")
        .bind(b.task_id.as_str())
        .fetch_one(&mut **tx)
        .await?;
    let db_operation: Value =
        sqlx::query_scalar("SELECT to_jsonb(o) FROM operations o WHERE operation_id=$1")
            .bind(operation.as_str())
            .fetch_one(&mut **tx)
            .await?;
    let db_permit: Value =
        sqlx::query_scalar("SELECT to_jsonb(p) FROM execution_permits p WHERE permit_id=$1")
            .bind(p.id.as_str())
            .fetch_one(&mut **tx)
            .await?;
    let db_confirmation: Value = sqlx::query_scalar(
        "SELECT to_jsonb(c) FROM server_confirmations c WHERE confirmation_ref=$1",
    )
    .bind(p.confirmation_ref.as_str())
    .fetch_one(&mut **tx)
    .await?;
    let raw = json!({"public_binding":raw,"original_command":command,"original_binding":member,"journal_prefix":journal,"public_request":request_raw,"task":task,"operation":op,"confirmation":confirmation.record,"permit_id":p.id,"db_task":db_task,"db_operation":db_operation,"db_permit":db_permit,"db_confirmation":db_confirmation});
    Ok(Facts {
        details,
        digest: hash(&raw)?,
        raw,
    })
}
pub(super) async fn append(
    tx: &mut Transaction<'_, Postgres>,
    owner: &UserId,
    operation: &OperationId,
    read: &SubscriptionService,
) -> Result<(), StorageError> {
    let source = check_source(tx, owner.as_str(), None)
        .await?
        .ok_or(StorageError::DependencyUnavailable)?;
    let f = checked_facts(tx, owner, operation, read).await?;
    let event = EventId::parse(format!("event_{}", uuid::Uuid::new_v4()))
        .map_err(|_| StorageError::CorruptRecord)?;
    sqlx::query("INSERT INTO audit_events(event_id,task_id,operation_id,kind,task_revision,operation_revision,details) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(event.as_str()).bind(f.raw["task"]["task_id"].as_str().ok_or(StorageError::CorruptRecord)?).bind(operation.as_str()).bind(KIND).bind(f.raw["task"]["revision"].as_i64().ok_or(StorageError::CorruptRecord)?).bind(f.raw["operation"]["revision"].as_i64().ok_or(StorageError::CorruptRecord)?).bind(json!(f.details)).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO public_subscription_a2_links_v1(operation_id,business_ref,owner_id,source_id,event_id,facts,facts_digest) VALUES($1,$2,$3,$4::uuid,$5,$6,$7)").bind(operation.as_str()).bind(&f.details.business_ref).bind(owner.as_str()).bind(&source).bind(event.as_str()).bind(&f.raw).bind(f.digest.as_str()).execute(&mut **tx).await?;
    verify_link(tx, owner, operation, read).await?;
    Ok(())
}
pub(super) async fn verify_link(
    tx: &mut Transaction<'_, Postgres>,
    owner: &UserId,
    operation: &OperationId,
    read: &SubscriptionService,
) -> Result<Value, StorageError> {
    let f = checked_facts(tx, owner, operation, read).await?;
    let source = check_source(tx, owner.as_str(), None).await?;
    let Some(source) = source else {
        return Ok(json!({"status":"NOT_ACTIVATED"}));
    };
    let legacy:Option<(String,String)>=sqlx::query_as("SELECT business_ref,facts_digest FROM public_subscription_a2_legacy_v1 WHERE operation_id=$1 AND source_id=$2::uuid AND owner_id=$3").bind(operation.as_str()).bind(&source).bind(owner.as_str()).fetch_optional(&mut **tx).await?;
    if let Some((b, d)) = legacy {
        if b != f.details.business_ref || d != f.digest.as_str() {
            return Err(StorageError::CorruptRecord);
        }
        sqlx::query("SELECT verify_subscription_a2_v1($1)")
            .bind(operation.as_str())
            .execute(&mut **tx)
            .await?;
        return Ok(json!({"status":"LEGACY_WITHOUT_DOMAIN_EVENT"}));
    }
    let row=sqlx::query("SELECT event_id,facts,facts_digest,source_id::text AS source FROM public_subscription_a2_links_v1 WHERE operation_id=$1 AND owner_id=$2 AND business_ref=$3").bind(operation.as_str()).bind(owner.as_str()).bind(&f.details.business_ref).fetch_optional(&mut **tx).await?.ok_or(StorageError::ReconcileRequired)?;
    if row.try_get::<Value, _>("facts")? != f.raw
        || row.try_get::<String, _>("facts_digest")? != f.digest.as_str()
        || row.try_get::<String, _>("source")? != source
    {
        return Err(StorageError::CorruptRecord);
    }
    sqlx::query("SELECT verify_subscription_a2_v1($1)")
        .bind(operation.as_str())
        .execute(&mut **tx)
        .await?;
    let event: String = row.try_get("event_id")?;
    let details: Value = sqlx::query_scalar("SELECT details FROM audit_events WHERE event_id=$1")
        .bind(&event)
        .fetch_one(&mut **tx)
        .await?;
    checked_details(&details)?;
    if details != json!(f.details) {
        return Err(StorageError::CorruptRecord);
    }
    Ok(json!({"status":"VERIFIED_CANDIDATE_V1","event_id":event,"details":details}))
}
pub(super) async fn verify_existing(
    tx: &mut Transaction<'_, Postgres>,
    owner: &UserId,
    operation: &OperationId,
    read: &SubscriptionService,
) -> Result<(), StorageError> {
    let original:Value=sqlx::query_scalar("SELECT original_request FROM subscription_public_bindings_v1 WHERE owner_id=$1 AND operation_id=$2").bind(owner.as_str()).bind(operation.as_str()).fetch_one(&mut **tx).await?;
    let request = serde_json::from_value(original).map_err(|_| StorageError::CorruptRecord)?;
    let snap = read.load(tx, owner.as_str()).await?;
    if matches!(
        snap.original_result_mock(&request)
            .map_err(|_| StorageError::CorruptRecord)?,
        OriginalResultMock::Recorded { .. }
    ) {
        verify_link(tx, owner, operation, read).await?;
    }
    Ok(())
}
