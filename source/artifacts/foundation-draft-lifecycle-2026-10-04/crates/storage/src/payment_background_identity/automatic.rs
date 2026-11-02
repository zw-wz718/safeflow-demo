//! Approved local simulation source only. Durable original/result identity is
//! enrolled by the original acceptance transaction, never reconstructed on 404.
use super::*;

impl PaymentBackgroundDeployment {
    /// Admin-only explicit activation. Restart with the SAME run UUID is a
    /// read/validation operation; a disabled cohort cannot be resurrected.
    pub async fn enable_automatic_simulation(
        &self, run: uuid::Uuid, producer: &str, worker: &str,
    ) -> Result<(), StorageError> {
        let producer_name = identifier(producer)?;
        let worker_name = identifier(worker)?;
        if producer == worker { return Err(StorageError::FormalAuthUnavailable); }
        let repo = self.source.repository_for_isolated_backend();
        let schema = identifier(&repo.namespace)?;
        let mut tx = repo.pool.begin().await?;
        self.source.check(&mut tx).await?;
        for (role, kind) in [(producer,"PRODUCER"),(worker,"WORKER")] {
            let yes: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM payment_background_principals_v1 p JOIN payment_completion_candidate_fences f ON f.kind=p.kind AND f.authority_id=p.role_name JOIN pg_roles a ON a.rolname=p.role_name WHERE p.role_name=$1 AND p.kind=$2 AND p.active AND f.enabled AND p.epoch=f.epoch AND clock_timestamp()<p.valid_until AND clock_timestamp()<f.valid_until AND NOT a.rolsuper AND NOT a.rolcreaterole AND NOT a.rolcreatedb AND NOT a.rolbypassrls AND NOT EXISTS(SELECT 1 FROM pg_auth_members x WHERE x.member=a.oid))")
                .bind(role).bind(kind).fetch_one(&mut *tx).await?;
            if !yes { return Err(StorageError::FormalAuthUnavailable); }
        }
        let writer: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM payment_completion_logins_v1 WHERE role_name=$1 AND writer_version='completion-final-v1')")
            .bind(worker).fetch_one(&mut *tx).await?;
        if !writer { return Err(StorageError::FormalAuthUnavailable); }
        sqlx::query("INSERT INTO payment_auto_runs_v1(run_id,producer_role,worker_role) VALUES($1::uuid,$2,$3) ON CONFLICT(run_id) DO NOTHING")
            .bind(run.to_string()).bind(producer).bind(worker).execute(&mut *tx).await?;
        let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM payment_auto_runs_v1 WHERE run_id=$1::uuid AND enabled AND producer_role=$2 AND worker_role=$3)")
            .bind(run.to_string()).bind(producer).bind(worker).fetch_one(&mut *tx).await?;
        if !valid { return Err(StorageError::FormalAuthUnavailable); }
        for name in [producer_name,worker_name] {
            sqlx::query(&format!("GRANT EXECUTE ON FUNCTION {schema}.payment_auto_scan_v1(uuid,text,integer) TO {name}"))
                .execute(&mut *tx).await?;
        }
        tx.commit().await.map_err(|_|StorageError::ReconcileRequired)
    }

    /// Hard stop: no reenabling this cohort; does not cancel customer orders,
    /// fabricate a failure, release holds, or reset any existing state.
    pub async fn disable_automatic_simulation(&self, run: uuid::Uuid) -> Result<(), StorageError> {
        let repo = self.source.repository_for_isolated_backend();
        let mut tx = repo.pool.begin().await?;
        sqlx::query("UPDATE payment_auto_runs_v1 SET enabled=false WHERE run_id=$1::uuid AND enabled")
            .bind(run.to_string()).execute(&mut *tx).await?;
        tx.commit().await.map_err(|_|StorageError::ReconcileRequired)
    }
}

impl PaymentBackgroundActor {
    /// A failed/unknown scan returns an error, never permission to create a new
    /// result. Keyset pagination prevents one blocked old original starving new ones.
    pub async fn automatic_simulation_tick(&self, run: uuid::Uuid, after: &str) -> Result<Value,StorageError> {
        let rows: Value = sqlx::query_scalar(&format!("SELECT \"{}\".payment_auto_scan_v1($1::uuid,$2,32)",self.namespace))
            .bind(run.to_string()).bind(after).fetch_one(&self.pool).await?;
        let rows = rows.as_array().ok_or(StorageError::CorruptRecord)?;
        let mut cursor = String::new();
        let mut completed = 0;
        let mut decisions = 0;
        let mut preserved = 0;
        for row in rows {
            let grant = row["grant_id"].as_str().ok_or(StorageError::CorruptRecord)?;
            let result = row["result_id"].as_str().ok_or(StorageError::CorruptRecord)?;
            cursor = grant.into();
            if self.role == PaymentBackgroundRole::Producer {
                match row["has_decision"].as_bool() {
                    // Existing original decision is left to the original worker;
                    // never invent a replacement, even if its result differs.
                    Some(true) => preserved += 1,
                    Some(false) => match self.record_explicit_decision(grant,result,PaymentSimulationDecision::SettleLocalSimulation).await {
                        Ok(_) => decisions += 1,
                        Err(_) => preserved += 1, // Same map/result on next tick; no success inference.
                    },
                    None => return Err(StorageError::CorruptRecord),
                }
            } else if row["has_decision"] == true {
                match self.complete_original(grant).await {
                    Ok(v) if v["state"] == "COMPLETED" => completed += 1,
                    _ => preserved += 1, // Original recovery on next tick, no re-execution.
                }
            } else { preserved += 1; }
        }
        Ok(json!({"mode":"PROJECT_SIMULATION","scanned":rows.len(),"decisions":decisions,"completed":completed,"preserved":preserved,"next_cursor":cursor,"business_resubmitted":false}))
    }
}
