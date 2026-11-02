//! Closed background completion over the ORIGINAL member settlement method.
//! No SessionBinding, permit renewal, callback or caller-selected result exists.
use super::*;
use crate::permit::payment::evidence::{inspect_history, inspect_processing};
use safeflow_contracts::{OperationId, ResourceId, UserId};
use safeflow_transfer_integration::{
    funds_store::{FundsError, FundsStore, LockedOutgoingSource},
    mock_execution::{MockOutgoingState, MockResolution},
};
use sqlx::{Postgres, Transaction};

// This adapter is local to calls into migration 36's closed completion SQL.
// P0001 alone (or a COMPLETION_* prefix) is NOT a reconciliation contract.
// Match the exact persisted-integrity diagnostic AND its originating RAISE
// frame, as reported by the trusted PG16 backend in the validated namespace.
// No global sqlx conversion, SQL definition, role grant or migration changes.
fn completion_sql_error(namespace: &str, error: sqlx::Error) -> StorageError {
    let known = error.as_database_error()
        .and_then(|db| db.try_downcast_ref::<sqlx::postgres::PgDatabaseError>())
        .is_some_and(|db| {
            if db.code() != "P0001" || db.routine() != Some("exec_stmt_raise") {
                return false;
            }
            let functions: &[&str] = match db.message() {
                "COMPLETION_RECONCILE_REQUIRED" => &[
                    "payment_completion_begin_v1(text,bigint)",
                    "payment_completion_bundle_v1(text)",
                    "payment_completion_recover_v1(text)",
                ],
                "COMPLETION_GRANT_CORRUPT" | "COMPLETION_DECISION_CORRUPT"
                | "COMPLETION_DECISION_ORIGINAL_MISMATCH" =>
                    &["payment_completion_authority_v1(text,boolean)"],
                "COMPLETION_OWNER_CONFLICT" | "COMPLETION_PROCESSING_HAS_LEDGER" =>
                    &["payment_completion_begin_v1(text,bigint)"],
                "COMPLETION_ACCOUNTING_CONFLICT" | "COMPLETION_RECEIPT_CONTEXT"
                | "COMPLETION_RECEIPT_INCOMPLETE" => &["payment_completion_bundle_v1(text)"],
                "COMPLETION_OTHER_HOLDS_CHANGED" | "COMPLETION_BALANCE_DELTA" =>
                    &["payment_completion_final_guard_v1(text,bigint)"],
                "COMPLETION_PUBLIC_CAS" => &["payment_completion_finish_v1(text,bigint)"],
                "COMPLETION_ACCEPTED_BINDING_CONFLICT" | "COMPLETION_ACCEPTED_AUTH_CONFLICT"
                | "COMPLETION_ACCEPTED_TRAIL_INCOMPLETE" => &["payment_completion_accepted_v1(text)"],
                _ => return false,
            };
            // PG emits an unqualified signature when visible in the pinned
            // definer path; only this exact schema qualification is also valid.
            let Some(frame) = db.r#where().and_then(|v| v.lines().next())
                .and_then(|v| v.strip_prefix("PL/pgSQL function ")) else { return false; };
            let Some((signature, location)) = frame.split_once(" line ") else { return false; };
            let Some(line) = location.strip_suffix(" at RAISE") else { return false; };
            if line.parse::<u32>().ok().filter(|n| *n > 0).is_none() { return false; }
            functions.iter().any(|f| signature == *f || signature == format!("{namespace}.{f}"))
        });
    if known { StorageError::ReconcileRequired } else { StorageError::from(error) }
}

fn completion_funds_error(namespace: &str, error: FundsError) -> StorageError {
    match error {
        FundsError::Database(error)
        | FundsError::Receipt(safeflow_transfer_integration::business_store::StoreError::Database(error)) =>
            completion_sql_error(namespace, error),
        other => crate::transfer_sql::business_error(other),
    }
}

// The transaction is deliberately private. Dropping it, any failed await or an
// uncertain COMMIT exposes no writable connection and cannot synthesize FAILED.
struct CompletionTransaction {
    tx: Transaction<'static, Postgres>,
    grant: String,
    lease_epoch: i64,
}
struct CompletionOutgoingSource {
    grant: String,
    owner: UserId,
    payer: ResourceId,
}
impl LockedOutgoingSource for CompletionOutgoingSource {
    fn read<'a>(
        &'a mut self,
        connection: &'a mut sqlx::PgConnection,
        owner: &'a UserId,
        payer: &'a ResourceId,
    ) -> safeflow_contracts::ContractFuture<'a, Result<MockOutgoingState, FundsError>> {
        Box::pin(async move {
            if owner != &self.owner || payer != &self.payer {
                return Err(FundsError::OutgoingUnknown);
            }
            let allowed: bool = sqlx::query_scalar(
                "SELECT payment_completion_outgoing_v1($1)",
            ).bind(&self.grant).fetch_one(connection).await?;
            // NEVER return Blocked: the member correctly maps that to FAILED,
            // whereas this public background contract requires preserving hold.
            if !allowed { return Err(FundsError::OutgoingUnknown); }
            Ok(MockOutgoingState::Allowed)
        })
    }
}

impl PaymentBackgroundDeployment {
    /// Separate writer-on maintenance action, AFTER migration/readers and only
    /// for an already registered local worker. Existing pending-only logins do
    /// not silently gain completion privileges during migration.
    pub async fn enable_final_completion_for_login(&self, role_name: &str) -> Result<(), StorageError> {
        let name = identifier(role_name)?;
        let repo = self.source.repository_for_isolated_backend();
        let schema = identifier(&repo.namespace)?;
        let mut tx = repo.pool.begin().await?;
        self.source.check(&mut tx).await?;
        let yes: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM payment_background_principals_v1 p JOIN pg_roles r ON r.rolname=p.role_name WHERE p.role_name=$1 AND p.kind='WORKER' AND p.active AND NOT r.rolsuper AND NOT r.rolcreatedb AND NOT r.rolcreaterole AND NOT r.rolbypassrls AND NOT EXISTS(SELECT 1 FROM pg_auth_members m WHERE m.member=r.oid))")
            .bind(role_name).fetch_one(&mut *tx).await?;
        if !yes { return Err(StorageError::FormalAuthUnavailable); }
        // TEMP is a database-wide PUBLIC privilege in vanilla PostgreSQL. In a
        // dedicated NEW isolated database revoke it before enabling this writer.
        let db: String = sqlx::query_scalar("SELECT current_database()").fetch_one(&mut *tx).await?;
        sqlx::query(&format!("REVOKE TEMPORARY ON DATABASE \"{}\" FROM PUBLIC", db.replace('"', "\"\""))).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO payment_completion_logins_v1(role_name,writer_version) VALUES($1,'completion-final-v1') ON CONFLICT DO NOTHING").bind(role_name).execute(&mut *tx).await?;
        for table in ["tasks", "operations", "public_execution_requests", "payment_preparations", "payment_funds_execution_v1", "server_confirmations", "execution_permits", "mock_draft_lifecycle_plans", "transfer_drafts_v1", "transfer_business_orders_v1", "transfer_fund_orders_v1", "transfer_accounts_v1", "transfer_ledger_v1", "audit_events"] {
            sqlx::query(&format!("GRANT SELECT ON {schema}.{table} TO {name}")).execute(&mut *tx).await?;
        }
        // Only the member's existing writes; all rows are RLS-scoped, mutation
        // checked and deferred until the complete bundle exists. No DELETE,
        // business INSERT, operation UPDATE, audit/receipt or control write.
        for statement in [
            format!("GRANT UPDATE(booked_cents) ON {schema}.transfer_accounts_v1 TO {name}"),
            format!("GRANT UPDATE(state) ON {schema}.transfer_fund_orders_v1 TO {name}"),
            format!("GRANT UPDATE(latest_receipt,latest_recorded_at) ON {schema}.transfer_business_orders_v1 TO {name}"),
            format!("GRANT INSERT(business_owner_id,operation_id,account_id,direction,amount_cents,completed_at) ON {schema}.transfer_ledger_v1 TO {name}"),
        ] { sqlx::query(&statement).execute(&mut *tx).await?; }
        sqlx::query(&format!("GRANT EXECUTE ON FUNCTION {schema}.payment_completion_begin_v1(text,bigint),{schema}.payment_completion_finish_v1(text,bigint),{schema}.payment_completion_recover_v1(text),{schema}.payment_completion_outgoing_v1(text),{schema}.payment_completion_final_guard_v1(text,bigint) TO {name}"))
            .execute(&mut *tx).await?;
        tx.commit().await.map_err(|_| StorageError::ReconcileRequired)
    }
}

impl PaymentBackgroundActor {
    /// Read original complete evidence on a fresh primary RC statement. This
    /// does not grant completion authority or require an unexpired delegation.
    pub async fn read_completed_original(&self, grant: &str) -> Result<Option<Value>, StorageError> {
        if self.role != PaymentBackgroundRole::Worker { return Err(StorageError::FormalAuthUnavailable); }
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED READ ONLY").execute(&mut *tx).await?;
        let result: Option<Value> = sqlx::query_scalar(&format!("/* completion_final_current_read_authority_v1 */ SELECT \"{}\".payment_completion_recover_v1($1)",self.namespace))
            .bind(grant).fetch_one(&mut *tx).await.map_err(|e| completion_sql_error(&self.namespace, e))?;
        tx.commit().await.map_err(|e| completion_sql_error(&self.namespace, e))?;
        Ok(result)
    }

    /// Native scheduler only. The only input is an original grant identifier;
    /// result/mode/owner/recipient/amount come from immutable verified storage.
    pub async fn complete_original(&self, grant: &str) -> Result<Value, StorageError> {
        if self.role != PaymentBackgroundRole::Worker { return Err(StorageError::FormalAuthUnavailable); }
        if let Some(v) = self.read_completed_original(grant).await? { return Ok(v); }
        let claim = self.inspect_and_claim_original(grant).await?;
        if claim["state"] != "READY_FOR_STEP08" { return Ok(claim); }
        let lease_epoch = claim["lease"]["lease_epoch"].as_i64().ok_or(StorageError::ReconcileRequired)?;
        let mut unit = CompletionTransaction { tx: self.pool.begin().await?, grant: grant.into(), lease_epoch };
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED").execute(&mut *unit.tx).await?;
        sqlx::query("SELECT set_config('search_path',$1,true)").bind(format!("\"{}\",pg_catalog,pg_temp",self.namespace)).execute(&mut *unit.tx).await?;
        let admitted: Value = sqlx::query_scalar("SELECT payment_completion_begin_v1($1,$2)")
            .bind(&unit.grant).bind(unit.lease_epoch).fetch_one(&mut *unit.tx).await.map_err(|e| completion_sql_error(&self.namespace, e))?;
        if admitted["state"] == "COMPLETED" {
            unit.tx.rollback().await?;
            return self.read_completed_original(grant).await?.ok_or(StorageError::ReconcileRequired);
        }
        let original = &admitted["original"];
        let task_id = original["task_id"].as_str().ok_or(StorageError::ReconcileRequired)?;
        let row=sqlx::query("SELECT * FROM tasks WHERE task_id=$1").bind(task_id).fetch_one(&mut *unit.tx).await?;
        let task = crate::repository::task_row(&row)?;
        let preparation = OperationId::parse(original["evidence"]["history"]["preparation_operation_id"].as_str().ok_or(StorageError::ReconcileRequired)?).map_err(|_|StorageError::CorruptRecord)?;
        let before = inspect_processing(&mut unit.tx, &task, &preparation).await?;
        let mut accepted = original["evidence"]["history"].clone();
        let mut current = before.clone();
        // Payer available balance may have changed from OTHER accepted orders;
        // inspect_processing rechecks CURRENT full accounting/own hold itself.
        accepted.as_object_mut().ok_or(StorageError::CorruptRecord)?.remove("accounting");
        current.as_object_mut().ok_or(StorageError::CorruptRecord)?.remove("accounting");
        if current != accepted { return Err(StorageError::ReconcileRequired); }
        let stored = sqlx::query("SELECT * FROM public_execution_requests WHERE operation_id=$1")
            .bind(original["execution_operation_id"].as_str().ok_or(StorageError::CorruptRecord)?).fetch_one(&mut *unit.tx).await?;
        let request = crate::execution_request::stored_row(&stored)?.request;
        let snapshot = crate::permit::payment::validate_request(&request)?;
        let resolution = match admitted["decision"]["outcome"].as_str() {
            Some("SETTLE_LOCAL_SIMULATION") => MockResolution::Succeed,
            Some("DEFINITE_SIMULATED_DECLINE") => MockResolution::Fail,
            _ => return Err(StorageError::ReconcileRequired),
        };
        let mut source = CompletionOutgoingSource { grant: grant.into(), owner: task.owner_id.clone(), payer: snapshot.transfer.payer_account_id.clone() };
        let record = FundsStore::resolve_checked(&mut unit.tx, &task.owner_id, &snapshot.transfer.operation_id, resolution, &mut source)
            .await.map_err(|e| completion_funds_error(&self.namespace, e))?;
        if record.request != snapshot.transfer { return Err(StorageError::ReconcileRequired); }
        // Finish is a closed DB composer. It makes the public CAS, event/Outbox
        // and immutable receipt and asserts baseline deltas. The deferred guard
        // repeats final authority after every possible member/SQL wait.
        let result: Value = sqlx::query_scalar("SELECT payment_completion_finish_v1($1,$2)")
            .bind(&unit.grant).bind(unit.lease_epoch).fetch_one(&mut *unit.tx).await.map_err(|e| completion_sql_error(&self.namespace, e))?;
        inspect_history(&mut unit.tx, &task, &preparation, false).await?;
        // No query may be issued after this final check except COMMIT. It takes
        // no new locks and never checks a stale producer/worker/lease snapshot.
        sqlx::query("SELECT payment_completion_final_guard_v1($1,$2)")
            .bind(&unit.grant).bind(unit.lease_epoch).execute(&mut *unit.tx).await.map_err(|e| completion_sql_error(&self.namespace, e))?;
        match unit.tx.commit().await {
            Ok(()) => Ok(result),
            Err(_) => self.read_completed_original(grant).await?
                .ok_or(StorageError::ReconcileRequired),
        }
    }
}
