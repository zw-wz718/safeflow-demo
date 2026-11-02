//! Native resource-administrator operations, never HTTP action DTOs.
use super::*;

pub struct CardFinalDeployment {
    source: PgUnifiedFictionalSource,
}
fn quoted(s: &str) -> Result<String, StorageError> {
    if !s.starts_with("sf_")
        || !(3..=63).contains(&s.len())
        || !s
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(StorageError::Invalid("isolated identifier"));
    }
    Ok(format!("\"{s}\""))
}
impl CardFinalDeployment {
    pub fn for_isolated_backend(source: PgUnifiedFictionalSource) -> Self {
        Self { source }
    }
    /// Caller supplies exact enumerated sets, including an explicit empty set.
    /// Lock all affected relations before snapshot; never infer from a timestamp.
    pub async fn activate_exact(
        &self,
        member_records: &[String],
        public_operations: &[OperationId],
    ) -> Result<(), StorageError> {
        let repo = self.source.repository_for_isolated_backend();
        let mut tx = repo.pool.begin().await?;
        sqlx::query("LOCK TABLE public_card_final_deployment_v1,public_card_legacy_members_v1,public_card_legacy_operations_v1,public_card_safety_requests_v1,card_locks_v1,card_freeze_operations_v1,card_freeze_reasons_v1,card_freeze_receipts_v1 IN ACCESS EXCLUSIVE MODE").execute(&mut *tx).await?;
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public_card_final_deployment_v1)")
                .fetch_one(&mut *tx)
                .await?;
        if exists {
            return Err(StorageError::Invalid("deployment already activated"));
        }
        self.source.check(&mut tx).await?;
        let actual: Vec<String> = sqlx::query_scalar(
            "SELECT record_id FROM card_freeze_operations_v1 ORDER BY record_id",
        )
        .fetch_all(&mut *tx)
        .await?;
        let ops: Vec<String> = sqlx::query_scalar(
            "SELECT operation_id FROM public_card_safety_requests_v1 ORDER BY operation_id",
        )
        .fetch_all(&mut *tx)
        .await?;
        let mut wanted = member_records.to_vec();
        wanted.sort();
        let mut wanted_ops: Vec<_> = public_operations
            .iter()
            .map(|v| v.as_str().to_owned())
            .collect();
        wanted_ops.sort();
        if actual != wanted || ops != wanted_ops {
            return Err(StorageError::Invalid("exact legacy sets required"));
        }
        sqlx::query("INSERT INTO public_card_legacy_members_v1(record_id,bundle,classification) SELECT m.record_id,jsonb_build_object('operation',to_jsonb(m),'reason',to_jsonb(h),'receipt',to_jsonb(r),'lock',to_jsonb(l)),'LEGACY_UNLINKED' FROM card_freeze_operations_v1 m JOIN card_freeze_reasons_v1 h ON h.operation_record_id=m.record_id JOIN card_freeze_receipts_v1 r ON r.operation_record_id=m.record_id JOIN card_locks_v1 l ON l.lock_id=m.lock_id").execute(&mut *tx).await?;
        let saved: i64 = sqlx::query_scalar("SELECT count(*) FROM public_card_legacy_members_v1")
            .fetch_one(&mut *tx)
            .await?;
        if saved as usize != actual.len() {
            return Err(StorageError::CorruptRecord);
        }
        sqlx::query("INSERT INTO public_card_legacy_operations_v1 SELECT operation_id,to_jsonb(q) FROM public_card_safety_requests_v1 q").execute(&mut *tx).await?;
        sqlx::query("INSERT INTO public_card_final_deployment_v1(singleton,deployment_id,source_id,implementation,schema_oid,database_oid,reader_version,active,epoch,object_digest) VALUES(1,$1::uuid,$2,$3,(SELECT oid FROM pg_namespace WHERE nspname=current_schema()),(SELECT oid FROM pg_database WHERE datname=current_database()),'card-final-v1',true,1,public_card_final_objects_v1())").bind(uuid::Uuid::new_v4().to_string()).bind(UNIFIED_FICTIONAL_SOURCE).bind(UNIFIED_FICTIONAL_IMPLEMENTATION).execute(&mut *tx).await?;
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)
    }
    /// Fresh SCRAM service login, no superuser/membership/schema/TEMP privileges.
    /// Existing Step 06 API, producer and worker receive no final card grants.
    pub async fn register_new_login(
        &self,
        role: &str,
        password: Zeroizing<String>,
    ) -> Result<(), StorageError> {
        let name = quoted(role)?;
        if !(40..=200).contains(&password.len())
            || !password.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return Err(StorageError::Invalid("random DB credential"));
        }
        let repo = self.source.repository_for_isolated_backend();
        let schema = quoted(&repo.namespace)?;
        let mut tx = repo.pool.begin().await?;
        deployment_guard(&mut tx, false).await?;
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
        let db: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&mut *tx)
            .await?;
        sqlx::query(&format!(
            "REVOKE TEMP ON DATABASE {} FROM PUBLIC",
            quoted(&db)?
        ))
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!("CREATE ROLE {name} LOGIN NOINHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD '{}'",password.as_str())).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO public_card_final_logins_v1 VALUES($1,'card-final-v1')")
            .bind(role)
            .execute(&mut *tx)
            .await?;
        sqlx::query(&format!("GRANT USAGE ON SCHEMA {schema} TO {name}"))
            .execute(&mut *tx)
            .await?;
        const MUTABLE: &[&str] = &[
            "tasks",
            "operations",
            "server_confirmations",
            "execution_permits",
            "security_decisions",
            "security_decision_time_fence",
            "public_execution_requests",
            "card_safety_session_fences_v1",
            "card_safety_account_fences_v1",
            "public_card_safety_requests_v1",
            "card_safety_challenges_v1",
            "public_card_attempts_v1",
        ];
        const APPEND: &[&str] = &[
            "audit_events",
            "public_audit_outbox",
            "card_locks_v1",
            "card_freeze_operations_v1",
            "card_freeze_reasons_v1",
            "card_freeze_receipts_v1",
            "public_card_results_v1",
            "public_card_audit_links_v1",
        ];
        // Existing cross-domain deferred integrity triggers run on shared task,
        // permit/audit/outbox writes. Their lookup roots need SELECT even when
        // this card caller never writes billing or subscription business data.
        const READ: &[&str] = &[
            "payment_preparations",
            "payment_execution_links",
            "payment_funds_execution_v1",
            "_sqlx_migrations",
            "mock_draft_lifecycle_tasks",
            "mock_draft_lifecycle_plans",
            "transfer_accounts_v1",
            "transfer_payee_routes_v1",
            "public_unified_fictional_source_v1",
            "public_recipient_profile_versions_v1",
            "card_catalog_v1",
            "card_account_versions_v1",
            "public_card_catalog_adoptions_v1",
            "public_card_final_deployment_v1",
            "public_card_final_logins_v1",
            "public_card_legacy_members_v1",
            "public_card_legacy_operations_v1",
            "billing_prepare_requests_v2",
            "billing_prepare_attempts_v2",
            "public_billing_audit_links_v1",
            "public_billing_feedback_results_v1",
            "subscription_public_bindings_v1",
            "public_subscription_a2_sources_v1",
        ];
        for (tables, privileges) in [
            (MUTABLE, "SELECT,INSERT,UPDATE"),
            (APPEND, "SELECT,INSERT"),
            (READ, "SELECT"),
        ] {
            for table in tables {
                sqlx::query(&format!(
                    "GRANT {privileges} ON {schema}.\"{table}\" TO {name}"
                ))
                .execute(&mut *tx)
                .await?;
            }
        }
        for (table, column) in [
            ("transfer_accounts_v1", "account_id"),
            ("card_account_versions_v1", "revision"),
            ("public_unified_fictional_source_v1", "singleton"),
            ("public_card_final_deployment_v1", "epoch"),
        ] {
            sqlx::query(&format!(
                "GRANT UPDATE({column}) ON {schema}.{table} TO {name}"
            ))
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(&format!(
            "GRANT USAGE,SELECT ON SEQUENCE {schema}.audit_events_sequence_seq TO {name}"
        ))
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!("GRANT EXECUTE ON FUNCTION {schema}.card_save_freeze_v1(TEXT,TEXT,TEXT,TEXT,TEXT,BOOLEAN,TEXT,TEXT,TEXT,TEXT,TEXT,TEXT,TEXT,TEXT,TEXT,TEXT),{schema}.public_card_final_assert_v1(TEXT),{schema}.public_card_final_objects_v1() TO {name}")).execute(&mut *tx).await?;
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)
    }
    /// Explicit native composition for the new isolated bank site only.
    /// The existing restricted payment HTTP principal gains card API rights,
    /// never producer/worker identity or direct final funds completion grants.
    pub async fn adopt_payment_api_login_for_bank_site(&self,role:&str)->Result<(),StorageError>{
        let name=quoted(role)?;let repo=self.source.repository_for_isolated_backend();
        let schema=quoted(&repo.namespace)?;let mut tx=repo.pool.begin().await?;
        deployment_guard(&mut tx,false).await?;self.source.check(&mut tx).await?;
        let db:String=sqlx::query_scalar("SELECT current_database()").fetch_one(&mut *tx).await?;
        let db= db.replace('"',"\"\"");
        sqlx::query(&format!("REVOKE TEMP ON DATABASE \"{db}\" FROM PUBLIC")).execute(&mut *tx).await?;
        let valid:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM payment_background_api_logins_v1 a JOIN pg_roles r ON r.rolname=a.role_name WHERE a.role_name=$1 AND a.active AND NOT r.rolsuper AND NOT r.rolcreatedb AND NOT r.rolcreaterole AND NOT r.rolbypassrls AND NOT r.rolinherit) AND NOT has_schema_privilege($1,current_schema(),'CREATE') AND NOT has_database_privilege($1,current_database(),'TEMP') AND NOT EXISTS(SELECT 1 FROM pg_auth_members WHERE member=(SELECT oid FROM pg_roles WHERE rolname=$1)) AND NOT EXISTS(SELECT 1 FROM payment_background_principals_v1 WHERE role_name=$1)")
            .bind(role).fetch_one(&mut *tx).await?;
        if !valid{return Err(StorageError::FormalAuthUnavailable);}
        sqlx::query("INSERT INTO public_card_final_logins_v1 VALUES($1,'card-final-v1')")
            .bind(role)
            .execute(&mut *tx)
            .await?;
        sqlx::query(&format!("GRANT USAGE ON SCHEMA {schema} TO {name}"))
            .execute(&mut *tx)
            .await?;
        const MUTABLE: &[&str] = &[
            "tasks",
            "operations",
            "server_confirmations",
            "execution_permits",
            "security_decisions",
            "security_decision_time_fence",
            "public_execution_requests",
            "card_safety_session_fences_v1",
            "card_safety_account_fences_v1",
            "public_card_safety_requests_v1",
            "card_safety_challenges_v1",
            "public_card_attempts_v1",
        ];
        const APPEND: &[&str] = &[
            "audit_events",
            "public_audit_outbox",
            "card_locks_v1",
            "card_freeze_operations_v1",
            "card_freeze_reasons_v1",
            "card_freeze_receipts_v1",
            "public_card_results_v1",
            "public_card_audit_links_v1",
        ];
        // Existing cross-domain deferred integrity triggers run on shared task,
        // permit/audit/outbox writes. Their lookup roots need SELECT even when
        // this card caller never writes billing or subscription business data.
        const READ: &[&str] = &[
            "payment_preparations",
            "payment_execution_links",
            "payment_funds_execution_v1",
            "_sqlx_migrations",
            "mock_draft_lifecycle_tasks",
            "mock_draft_lifecycle_plans",
            "transfer_accounts_v1",
            "transfer_payee_routes_v1",
            "public_unified_fictional_source_v1",
            "public_recipient_profile_versions_v1",
            "card_catalog_v1",
            "card_account_versions_v1",
            "public_card_catalog_adoptions_v1",
            "public_card_final_deployment_v1",
            "public_card_final_logins_v1",
            "public_card_legacy_members_v1",
            "public_card_legacy_operations_v1",
            "billing_prepare_requests_v2",
            "billing_prepare_attempts_v2",
            "public_billing_audit_links_v1",
            "public_billing_feedback_results_v1",
            "subscription_public_bindings_v1",
            "public_subscription_a2_sources_v1",
        ];
        for (tables, privileges) in [
            (MUTABLE, "SELECT,INSERT,UPDATE"),
            (APPEND, "SELECT,INSERT"),
            (READ, "SELECT"),
        ] {
            for table in tables {
                sqlx::query(&format!(
                    "GRANT {privileges} ON {schema}.\"{table}\" TO {name}"
                ))
                .execute(&mut *tx)
                .await?;
            }
        }
        for (table, column) in [
            ("transfer_accounts_v1", "account_id"),
            ("card_account_versions_v1", "revision"),
            ("public_unified_fictional_source_v1", "singleton"),
            ("public_card_final_deployment_v1", "epoch"),
        ] {
            sqlx::query(&format!(
                "GRANT UPDATE({column}) ON {schema}.{table} TO {name}"
            ))
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(&format!(
            "GRANT USAGE,SELECT ON SEQUENCE {schema}.audit_events_sequence_seq TO {name}"
        ))
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!("GRANT EXECUTE ON FUNCTION {schema}.card_save_freeze_v1(TEXT,TEXT,TEXT,TEXT,TEXT,BOOLEAN,TEXT,TEXT,TEXT,TEXT,TEXT,TEXT,TEXT,TEXT,TEXT,TEXT),{schema}.public_card_final_assert_v1(TEXT),{schema}.public_card_final_objects_v1() TO {name}")).execute(&mut *tx).await?;
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)
    }
    /// One way writer withdrawal. Old readers are never reenabled after new kind.
    pub async fn disable_writer(&self) -> Result<(), StorageError> {
        sqlx::query("UPDATE public_card_final_deployment_v1 SET active=false,epoch=epoch+1 WHERE singleton=1 AND active").execute(&self.source.repository_for_isolated_backend().pool).await?;
        Ok(())
    }
}
