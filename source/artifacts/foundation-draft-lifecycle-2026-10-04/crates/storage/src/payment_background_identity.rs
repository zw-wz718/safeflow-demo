//! Native-only independent PostgreSQL logins. No customer session synthesis.
use crate::{
    PgRepository, PgUnifiedFictionalSource, StorageError, UNIFIED_FICTIONAL_IMPLEMENTATION,
    UNIFIED_FICTIONAL_SOURCE,
};
use serde_json::{json, Value};
use sqlx::{
    postgres::{PgConnectOptions, PgPoolOptions},
    PgPool, Row,
};
use zeroize::Zeroizing;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PaymentBackgroundRole {
    Producer,
    Worker,
}
impl PaymentBackgroundRole {
    fn name(self) -> &'static str {
        match self {
            Self::Producer => "PRODUCER",
            Self::Worker => "WORKER",
        }
    }
    fn function(self) -> &'static str {
        match self {
            Self::Producer => "payment_background_decide_v1(text,text,text)",
            Self::Worker => "payment_background_worker_v1(text)",
        }
    }
}
#[derive(Clone, Copy)]
pub enum PaymentSimulationDecision {
    SettleLocalSimulation,
    DefiniteSimulatedDecline,
}
impl PaymentSimulationDecision {
    fn name(self) -> &'static str {
        match self {
            Self::SettleLocalSimulation => "SETTLE_LOCAL_SIMULATION",
            Self::DefiniteSimulatedDecline => "DEFINITE_SIMULATED_DECLINE",
        }
    }
}
pub struct PaymentBackgroundDeployment {
    source: PgUnifiedFictionalSource,
}
fn identifier(s: &str) -> Result<String, StorageError> {
    if s.len() < 3
        || s.len() > 63
        || !s.starts_with("sf_")
        || !s
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(StorageError::Invalid("isolated DB role/schema identifier"));
    }
    Ok(format!("\"{s}\""))
}
impl PaymentBackgroundDeployment {
    pub fn for_isolated_backend(source: PgUnifiedFictionalSource) -> Self {
        Self { source }
    }
    /// Fresh restricted HTTP-service login. The native deployment admin is kept
    /// separate; background authorization/decision writers stay definer-only.
    pub async fn register_new_api_login(
        &self,
        role_name: &str,
        password: Zeroizing<String>,
    ) -> Result<(), StorageError> {
        let name = identifier(role_name)?;
        if !(40..=200).contains(&password.len())
            || !password.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return Err(StorageError::Invalid(
                "random alphanumeric DB credential required",
            ));
        }
        let repo = self.source.repository_for_isolated_backend();
        let schema = identifier(&repo.namespace)?;
        let mut tx = repo.pool.begin().await?;
        self.source.check(&mut tx).await?;
        sqlx::query("SET LOCAL log_statement='none'")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SET LOCAL log_min_error_statement='panic'")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SET LOCAL password_encryption='scram-sha-256'")
            .execute(&mut *tx)
            .await?;
        sqlx::query(&format!("CREATE ROLE {name} LOGIN NOINHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD '{}'",password.as_str())).execute(&mut *tx).await?;
        sqlx::query(
            "INSERT INTO payment_background_api_logins_v1(role_name,active) VALUES($1,true)",
        )
        .bind(role_name)
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!("GRANT USAGE ON SCHEMA {schema} TO {name}"))
            .execute(&mut *tx)
            .await?;
        // Explicit current assembly writers only. New migrations do not silently
        // gain service write access; billing/subscription/card final writers and
        // final transfer ledger are outside this Pending-only Step 06 service.
        const WRITERS: &[&str] = &[
            "tasks",
            "operations",
            "audit_events",
            "public_audit_outbox",
            "server_confirmations",
            "execution_permits",
            "security_decisions",
            "security_decision_time_fence",
            "mock_draft_execution_claims",
            "mock_draft_lifecycle_tasks",
            "mock_draft_lifecycle_plans",
            "mock_draft_lifecycle_actions",
            "mock_draft_lifecycle_displays",
            "mock_draft_lifecycle_claims",
            "payment_preparations",
            "payment_execution_links",
            "payment_business_observations",
            "payment_agent_bindings",
            "payment_funds_execution_v1",
            "public_execution_requests",
            "transfer_drafts_v1",
            "transfer_accounts_v1",
            "transfer_payee_routes_v1",
            "transfer_business_orders_v1",
            "transfer_fund_orders_v1",
            "card_account_versions_v1",
            "card_catalog_v1",
            "public_card_catalog_adoptions_v1",
            "card_safety_session_fences_v1",
            "card_safety_account_fences_v1",
            "payment_background_consents_v1",
        ];
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT tablename FROM pg_tables WHERE schemaname=$1 ORDER BY tablename",
        )
        .bind(&repo.namespace)
        .fetch_all(&mut *tx)
        .await?;
        for table in tables {
            let quoted = format!("\"{}\"", table.replace('"', "\"\""));
            if table == "payment_background_principals_v1"
                || table == "payment_background_api_logins_v1"
                || table == "payment_completion_candidate_fences"
                || table == "payment_auto_runs_v1"
                || table == "payment_auto_originals_v1"
            {
                continue;
            }
            let privileges = if WRITERS.contains(&table.as_str()) {
                "SELECT,INSERT,UPDATE"
            } else {
                "SELECT"
            };
            sqlx::query(&format!(
                "GRANT {privileges} ON TABLE {schema}.{quoted} TO {name}"
            ))
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(&format!(
            "GRANT USAGE,SELECT ON ALL SEQUENCES IN SCHEMA {schema} TO {name}"
        ))
        .execute(&mut *tx)
        .await?;
        // PostgreSQL row SHARE locks require UPDATE on at least one column.
        // Only monotonic withdrawal is allowed by the existing immutable trigger.
        sqlx::query(&format!(
            "GRANT UPDATE(active) ON {schema}.public_unified_fictional_source_v1 TO {name}"
        ))
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!("GRANT EXECUTE ON FUNCTION {schema}.payment_background_accept_v1(uuid,jsonb),{schema}.payment_background_revoke_v1(text,text,text,bigint),{schema}.payment_background_api_identity_v1() TO {name}")).execute(&mut *tx).await?;
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)?;
        Ok(())
    }
    /// Resource administrator only. A caller-selected string is never a principal.
    /// Fresh roles only, no membership, no owner or API grants. Password stays private.
    pub async fn register_new_login(
        &self,
        role_name: &str,
        role: PaymentBackgroundRole,
        password: Zeroizing<String>,
    ) -> Result<(), StorageError> {
        let name = identifier(role_name)?;
        if !(40..=200).contains(&password.len())
            || !password.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return Err(StorageError::Invalid(
                "random alphanumeric DB credential required",
            ));
        }
        let repo = self.source.repository_for_isolated_backend();
        let schema = identifier(&repo.namespace)?;
        let mut tx = repo.pool.begin().await?;
        self.source.check(&mut tx).await?;
        // Dedicated local resource admin. Avoid statement/error logging of credentials.
        sqlx::query("SET LOCAL log_statement='none'")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SET LOCAL log_min_error_statement='panic'")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SET LOCAL password_encryption='scram-sha-256'")
            .execute(&mut *tx)
            .await?;
        sqlx::query(&format!("CREATE ROLE {name} LOGIN NOINHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD '{}'",password.as_str())).execute(&mut *tx).await?;
        let scope = json!({"accounts":[{"owner_id":"user_001","account_id":"account_001"},{"owner_id":"user_002","account_id":"account_002"},{"owner_id":"demo_user_li_v1","account_id":"demo_account_li_v1"}]});
        sqlx::query("INSERT INTO payment_completion_candidate_fences(kind,authority_id,epoch,enabled,valid_until,scope,adoption_ref) VALUES($1,$2,1,true,clock_timestamp()+interval '1 day',$3,'sf-isolated-demo-policy-20261027-v1')").bind(role.name()).bind(role_name).bind(sqlx::types::Json(scope)).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO payment_background_principals_v1(role_name,kind,source_id,implementation,epoch,active,valid_until) VALUES($1,$2,$3,$4,1,true,clock_timestamp()+interval '1 day')").bind(role_name).bind(role.name()).bind(UNIFIED_FICTIONAL_SOURCE).bind(UNIFIED_FICTIONAL_IMPLEMENTATION).execute(&mut *tx).await?;
        sqlx::query(&format!("GRANT USAGE ON SCHEMA {schema} TO {name}"))
            .execute(&mut *tx)
            .await?;
        sqlx::query(&format!(
            "GRANT EXECUTE ON FUNCTION {schema}.{} TO {name}",
            role.function()
        ))
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!(
            "GRANT EXECUTE ON FUNCTION {schema}.payment_background_identity_v1() TO {name}"
        ))
        .execute(&mut *tx)
        .await?;
        // No table privileges, ownership, default broad role, or arbitrary-function grant.
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)?;
        Ok(())
    }
    pub async fn revoke_login(&self, role_name: &str) -> Result<(), StorageError> {
        identifier(role_name)?;
        let repo = self.source.repository_for_isolated_backend();
        let mut tx = repo.pool.begin().await?;
        let kind: String = sqlx::query_scalar(
            "SELECT kind FROM payment_background_principals_v1 WHERE role_name=$1",
        )
        .bind(role_name)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StorageError::NotFound)?;
        sqlx::query("SELECT authority_id FROM payment_completion_candidate_fences WHERE kind=$1 AND authority_id=$2 FOR UPDATE").bind(&kind).bind(role_name).fetch_one(&mut *tx).await?;
        sqlx::query("UPDATE payment_completion_candidate_fences SET enabled=false,epoch=epoch+1 WHERE kind=$1 AND authority_id=$2 AND enabled").bind(&kind).bind(role_name).execute(&mut *tx).await?;
        sqlx::query("UPDATE payment_background_principals_v1 SET active=false,epoch=epoch+1 WHERE role_name=$1 AND active").bind(role_name).execute(&mut *tx).await?;
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)?;
        Ok(())
    }
}
pub struct PaymentBackgroundActor {
    pool: PgPool,
    namespace: String,
    role: PaymentBackgroundRole,
}
mod completion;
mod automatic;
impl PaymentBackgroundActor {
    pub async fn authenticate(
        options: PgConnectOptions,
        namespace: &str,
        role: PaymentBackgroundRole,
    ) -> Result<Self, StorageError> {
        identifier(namespace)?;
        // SCRAM/OS identity is established by PostgreSQL, not an HTTP header.
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await?;
        let q=format!("SELECT p.kind,p.active,p.epoch,f.epoch AS fence_epoch,f.enabled FROM \"{namespace}\".payment_background_identity_v1() p JOIN LATERAL (SELECT p.epoch AS epoch,p.active AS enabled) f ON true");
        let row = sqlx::query(&q)
            .fetch_optional(&pool)
            .await?
            .ok_or(StorageError::FormalAuthUnavailable)?;
        if row.try_get::<String, _>("kind")? != role.name()
            || !row.try_get::<bool, _>("active")?
            || !row.try_get::<bool, _>("enabled")?
            || row.try_get::<i64, _>("epoch")? != row.try_get::<i64, _>("fence_epoch")?
        {
            pool.close().await;
            return Err(StorageError::FormalAuthUnavailable);
        }
        Ok(Self {
            pool,
            namespace: namespace.into(),
            role,
        })
    }
    pub async fn record_explicit_decision(
        &self,
        grant: &str,
        result: &str,
        decision: PaymentSimulationDecision,
    ) -> Result<Value, StorageError> {
        if self.role != PaymentBackgroundRole::Producer {
            return Err(StorageError::FormalAuthUnavailable);
        }
        Ok(sqlx::query_scalar(&format!(
            "SELECT \"{}\".payment_background_decide_v1($1,$2,$3)",
            self.namespace
        ))
        .bind(grant)
        .bind(result)
        .bind(decision.name())
        .fetch_one(&self.pool)
        .await?)
    }
    pub async fn inspect_and_claim_original(&self, grant: &str) -> Result<Value, StorageError> {
        if self.role != PaymentBackgroundRole::Worker {
            return Err(StorageError::FormalAuthUnavailable);
        }
        Ok(sqlx::query_scalar(&format!(
            "SELECT \"{}\".payment_background_worker_v1($1)",
            self.namespace
        ))
        .bind(grant)
        .fetch_one(&self.pool)
        .await?)
    }
    pub async fn close(&self) {
        self.pool.close().await;
    }
}
impl PgRepository {
    /// Fail startup if the service accidentally uses the deployment administrator.
    pub async fn verify_payment_background_api_login(&self) -> Result<(), StorageError> {
        let yes: bool = sqlx::query_scalar("SELECT payment_background_api_identity_v1()")
            .fetch_one(&self.pool)
            .await?;
        if !yes {
            return Err(StorageError::FormalAuthUnavailable);
        }
        Ok(())
    }
    /// Current owner history only; revocation changes control, never the funds order.
    pub async fn revoke_payment_background(
        &self,
        sessions: std::sync::Arc<dyn safeflow_runtime::SessionResolver>,
        session: &safeflow_contracts::SessionRef,
        execution: &safeflow_contracts::OperationId,
    ) -> Result<(), StorageError> {
        let b = sessions
            .resolve_binding(session)
            .map_err(|_| StorageError::Session)?;
        let guard = || {
            if sessions
                .resolve_binding(session)
                .map_err(|_| StorageError::Session)?
                == b
            {
                Ok(())
            } else {
                Err(StorageError::Session)
            }
        };
        let tx=crate::repository::guarded_poll(&guard,Box::pin(async{
            let mut tx=self.pool.begin().await?;
            let owner:String=sqlx::query_scalar("SELECT owner_id FROM payment_completion_candidate_grants WHERE execution_operation_id=$1").bind(execution.as_str()).fetch_optional(&mut *tx).await?.ok_or(StorageError::NotFound)?;
            if owner!=b.user_id.as_str(){return Err(StorageError::NotFound);}
            let d=crate::models::hash(&("safeflow-confirmation-session:v1",session.as_str()))?.as_str().to_owned();
            sqlx::query("SELECT payment_background_revoke_v1($1,$2,$3,$4)").bind(execution.as_str()).bind(&owner).bind(&d).bind(crate::models::db_version(b.binding_version)?).execute(&mut *tx).await?;
            Ok(tx)
        })).await?;
        guard()?;
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)?;
        guard()
    }
}
