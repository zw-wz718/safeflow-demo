//! Explicit, separately confirmed delegation. Never a wire bool or card grant.
use super::*;
use crate::{PgUnifiedFictionalSource, UNIFIED_FICTIONAL_IMPLEMENTATION, UNIFIED_FICTIONAL_SOURCE};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

pub const BACKGROUND_POLICY: &str = "sf-isolated-demo-policy-20261027-v1";
#[derive(Clone)]
pub(super) struct BackgroundConfig {
    source: PgUnifiedFictionalSource,
    controller_hash: [u8; 32],
    generation: uuid::Uuid,
}
/// Opaque native deployment capability, never deserialized from customer input.
#[derive(Clone)]
pub struct PaymentBackgroundHost(BackgroundConfig);
impl PaymentBackgroundHost {
    pub fn for_isolated_native(
        source: PgUnifiedFictionalSource,
        controller: Zeroizing<String>,
    ) -> Result<Self, StorageError> {
        if !(32..=256).contains(&controller.len()) {
            return Err(StorageError::Invalid("independent controller secret"));
        }
        Ok(Self(BackgroundConfig {
            source,
            controller_hash: Sha256::digest(controller.as_bytes()).into(),
            generation: uuid::Uuid::new_v4(),
        }))
    }
}
/// Receipt selector only; this cannot create or extend authority.
#[derive(Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct PaymentBackgroundDisplay {
    pub contract_version: u16,
    pub consent_id: String,
    pub display: Value,
    pub display_digest: String,
}
/// Native one-time display; no Debug, Serialize or Clone.
pub struct PaymentBackgroundCode(Zeroizing<String>);
impl PaymentBackgroundCode {
    pub fn expose_for_native_display(&self) -> &str {
        &self.0
    }
}
#[derive(Clone)]
pub(in crate::permit) struct AcceptedConsent {
    id: String,
    account_epoch: i64,
    display: Value,
    auth_ref: AuthGrantRef,
}
fn session_digest(s: &SessionRef) -> Result<String, StorageError> {
    Ok(hash(&("safeflow-confirmation-session:v1", s.as_str()))?
        .as_str()
        .into())
}
fn code_hash(id: &str, code: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!("isolated-payment-code-v1:{id}:{code}").as_bytes())
    )
}
async fn now(tx: &mut Transaction<'_, Postgres>) -> Result<DateTime<Utc>, StorageError> {
    Ok(sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?)
}
async fn catalog(
    tx: &mut Transaction<'_, Postgres>,
    account: &ResourceId,
) -> Result<Value, StorageError> {
    Ok(sqlx::query_scalar("SELECT to_jsonb(a) FROM (SELECT owner_id,source_id,implementation_revision,ordinal,source_revision,catalog_digest FROM public_card_catalog_adoptions_v1 WHERE account_id=$1 ORDER BY ordinal DESC LIMIT 1) a").bind(account.as_str()).fetch_optional(&mut **tx).await?.ok_or(StorageError::DependencyUnavailable)?)
}
// The existing card authorization roots are shared account/session revocation
// roots, not card authentication. Background never consults a card challenge.
async fn fences(
    tx: &mut Transaction<'_, Postgres>,
    identity: &SessionBinding,
    s: &SessionRef,
    account: &ResourceId,
    initialize: bool,
) -> Result<(i64, i64), StorageError> {
    let d = session_digest(s)?;
    if initialize {
        sqlx::query("INSERT INTO card_safety_session_fences_v1(session_digest,owner_id,binding_version) VALUES($1,$2,$3) ON CONFLICT DO NOTHING").bind(&d).bind(identity.user_id.as_str()).bind(db_version(identity.binding_version)?).execute(&mut **tx).await?;
    }
    let r = sqlx::query(
        "SELECT * FROM card_safety_session_fences_v1 WHERE session_digest=$1 FOR SHARE",
    )
    .bind(&d)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(StorageError::Session)?;
    if r.try_get::<bool, _>("revoked")?
        || r.try_get::<String, _>("owner_id")? != identity.user_id.as_str()
        || r.try_get::<i64, _>("binding_version")? != db_version(identity.binding_version)?
    {
        return Err(StorageError::Session);
    }
    let se = r.try_get("epoch")?;
    if initialize {
        sqlx::query("INSERT INTO card_safety_account_fences_v1(owner_id,account_id) VALUES($1,$2) ON CONFLICT DO NOTHING").bind(identity.user_id.as_str()).bind(account.as_str()).execute(&mut **tx).await?;
    }
    let r = sqlx::query(
        "SELECT * FROM card_safety_account_fences_v1 WHERE owner_id=$1 AND account_id=$2 FOR SHARE",
    )
    .bind(identity.user_id.as_str())
    .bind(account.as_str())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(StorageError::NotFound)?;
    if r.try_get::<bool, _>("revoked")? {
        return Err(StorageError::AuthGrantRevoked);
    }
    Ok((se, r.try_get("epoch")?))
}
impl<'a> PaymentExecutionService<'a> {
    /// Native deployment assembly only. Uses exact already-adopted fictional DB.
    pub fn with_isolated_background(
        self,
        source: PgUnifiedFictionalSource,
        controller: Zeroizing<String>,
    ) -> Result<Self, StorageError> {
        self.with_background_host(PaymentBackgroundHost::for_isolated_native(
            source, controller,
        )?)
    }
    pub fn with_background_host(
        mut self,
        host: PaymentBackgroundHost,
    ) -> Result<Self, StorageError> {
        let supplied = host.0.source.repository_for_isolated_backend();
        let actual = &self.store.repository;
        let a = supplied.pool.connect_options();
        let b = actual.pool.connect_options();
        if supplied.namespace != actual.namespace
            || a.get_host() != b.get_host()
            || a.get_port() != b.get_port()
            || a.get_database() != b.get_database()
        {
            return Err(StorageError::TransactionScope);
        }
        self.profile.minimum_level = SecurityLevel::Red;
        self.background = Some(host.0);
        Ok(self)
    }
    fn background_config(&self) -> Result<&BackgroundConfig, StorageError> {
        self.background
            .as_ref()
            .ok_or(StorageError::DependencyUnavailable)
    }
    async fn consent_transaction(
        &self,
        s: &SessionRef,
        task: &TaskId,
        p: &OperationId,
        id: Option<&str>,
    ) -> Result<(Transaction<'static, Postgres>, SessionBinding, SavedPayment), StorageError> {
        let b = self.store.identity(s)?;
        let mut tx = self.store.repository.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await?;
        let read = sqlx::query("SELECT * FROM tasks WHERE owner_id=$1 AND task_id=$2")
            .bind(b.user_id.as_str())
            .bind(task.as_str())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StorageError::NotFound)?;
        let task_read = crate::repository::task_row(&read)?;
        let initial = saved_payment(&mut tx, &task_read, p, false).await?;
        let account = &initial.snapshot.transfer.payer_account_id;
        fences(&mut tx, &b, s, account, id.is_none()).await?;
        crate::daily_usage::protect_owner(&mut tx, &b.user_id).await?;
        let task = lock_task_for_mode(&mut tx, &b.user_id, task, true).await?;
        let saved = saved_payment(&mut tx, &task, p, true).await?;
        if saved.snapshot != initial.snapshot {
            return Err(StorageError::FactsConflict);
        }
        self.background_config()?
            .source
            .protect_card_scope(&mut tx, &b.user_id, account)
            .await?;
        if let Some(id) = id {
            self.read_consent(&mut tx, s, &b, &saved, id, false).await?;
        }
        Ok((tx, b, saved))
    }
    /// Read/display creates a proposal only. No confirmation, auth, grant or job.
    pub async fn display_background_delegation(
        &self,
        s: &SessionRef,
        task: &TaskId,
        p: &OperationId,
    ) -> Result<PaymentBackgroundDisplay, StorageError> {
        let b = self.store.identity(s)?;
        let guard = || {
            if self.store.identity(s)? == b {
                Ok(())
            } else {
                Err(StorageError::Session)
            }
        };
        let (tx,value)=crate::repository::guarded_poll(&guard,Box::pin(async{
            let(mut tx,b,saved)=self.consent_transaction(s,task,p,None).await?;
            let task_row=crate::repository::task_row(&sqlx::query("SELECT * FROM tasks WHERE task_id=$1").bind(task.as_str()).fetch_one(&mut *tx).await?)?;
            if replay(&mut tx,&task_row,&saved).await?.is_some(){return Err(StorageError::IllegalTransition);}
            let(se,ae)=fences(&mut tx,&b,s,&saved.snapshot.transfer.payer_account_id,false).await?;
            let display=json!({"schema":"payment-background-display-v1","policy":BACKGROUND_POLICY,"mode":"PROJECT_SIMULATION",
                "explicit_text":"如暂未完成，允许在本委托期内按这个原单继续办理","duration_minutes":30,"default_delegated":false,
                "owner_id":b.user_id,"task_id":task,"preparation_operation_id":p,"snapshot":saved.snapshot,
                "preparation_binding":saved.stored.record.binding,"confirmation_ref":saved.stored.record.confirmation_ref,
                "source_id":UNIFIED_FICTIONAL_SOURCE,"source_implementation":UNIFIED_FICTIONAL_IMPLEMENTATION,
                "catalog_adoption":catalog(&mut tx,&saved.snapshot.transfer.payer_account_id).await?});
            let d=hash(&display)?.as_str().to_owned();
            let existing=sqlx::query("SELECT consent_id::text AS id,display,display_digest FROM payment_background_consents_v1 WHERE preparation_operation_id=$1").bind(p.as_str()).fetch_optional(&mut *tx).await?;
            let cid=if let Some(row)=existing {
                if row.try_get::<Json<Value>,_>("display")?.0!=display || row.try_get::<String,_>("display_digest")?!=d{return Err(StorageError::ConfirmationBindingMismatch);}
                row.try_get("id")?
            } else {
                let id=uuid::Uuid::new_v4().to_string();
                sqlx::query("INSERT INTO payment_background_consents_v1(consent_id,preparation_operation_id,task_id,owner_id,account_id,session_digest,binding_version,session_epoch,account_epoch,display,display_digest) VALUES($1::uuid,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)").bind(&id).bind(p.as_str()).bind(task.as_str()).bind(b.user_id.as_str()).bind(saved.snapshot.transfer.payer_account_id.as_str()).bind(session_digest(s)?).bind(db_version(b.binding_version)?).bind(se).bind(ae).bind(Json(&display)).bind(&d).execute(&mut *tx).await?;id
            };
            Ok((tx,PaymentBackgroundDisplay{contract_version:1,consent_id:cid,display,display_digest:d}))
        })).await?;
        guard()?;
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)?;
        guard()?;
        Ok(value)
    }
    /// Separate explicit action, comparing the exact server-rendered version.
    pub async fn confirm_background_delegation(
        &self,
        s: &SessionRef,
        task: &TaskId,
        p: &OperationId,
        shown: &PaymentBackgroundDisplay,
    ) -> Result<(), StorageError> {
        let b = self.store.identity(s)?;
        let guard = || {
            if self.store.identity(s)? == b {
                Ok(())
            } else {
                Err(StorageError::Session)
            }
        };
        let tx=crate::repository::guarded_poll(&guard,Box::pin(async{
            if shown.contract_version!=1 || hash(&shown.display)?.as_str()!=shown.display_digest{return Err(StorageError::ConfirmationBindingMismatch);}
            let(mut tx,b,saved)=self.consent_transaction(s,task,p,Some(&shown.consent_id)).await?;
            let c=self.read_consent(&mut tx,s,&b,&saved,&shown.consent_id,false).await?;
            if c.try_get::<Json<Value>,_>("display")?.0!=shown.display || c.try_get::<String,_>("display_digest")?!=shown.display_digest {return Err(StorageError::ConfirmationBindingMismatch);}
            let t=now(&mut tx).await?;
            if c.try_get::<Option<DateTime<Utc>>,_>("confirmed_at")?.is_none(){sqlx::query("UPDATE payment_background_consents_v1 SET confirmed_at=$2,confirmed_until=$2+interval '5 minutes' WHERE consent_id=$1::uuid").bind(&shown.consent_id).bind(t).execute(&mut *tx).await?;}
            Ok(tx)
        })).await?;
        guard()?;
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)?;
        guard()
    }
    pub async fn background_challenge_for_native_controller(
        &self,
        secret: &str,
        s: &SessionRef,
        task: &TaskId,
        p: &OperationId,
        id: &str,
    ) -> Result<Option<PaymentBackgroundCode>, StorageError> {
        let config = self.background_config()?;
        let actual: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
        if !bool::from(actual.ct_eq(&config.controller_hash)) {
            return Err(StorageError::FormalAuthUnavailable);
        }
        let b = self.store.identity(s)?;
        let guard = || {
            if self.store.identity(s)? == b {
                Ok(())
            } else {
                Err(StorageError::Session)
            }
        };
        let(tx,code)=crate::repository::guarded_poll(&guard,Box::pin(async{
            let(mut tx,b,saved)=self.consent_transaction(s,task,p,Some(id)).await?;
            let c=self.read_consent(&mut tx,s,&b,&saved,id,true).await?;
            let code=if c.try_get::<Option<String>,_>("code_hash")?.is_some(){None}else{
                let n=loop{let mut v=[0;4];getrandom::getrandom(&mut v).map_err(|_|StorageError::FormalAuthUnavailable)?;let n=u32::from_le_bytes(v);if n<4_294_000_000{break n%1_000_000;}};
                let code=Zeroizing::new(format!("{n:06}"));let t=now(&mut tx).await?;
                sqlx::query("UPDATE payment_background_consents_v1 SET challenge_id=$2::uuid,host_generation=$3::uuid,code_hash=$4,challenge_at=$5,challenge_until=$5+interval '2 minutes' WHERE consent_id=$1::uuid").bind(id).bind(uuid::Uuid::new_v4().to_string()).bind(config.generation.to_string()).bind(code_hash(id,&code)).bind(t).execute(&mut *tx).await?;
                Some(PaymentBackgroundCode(code))
            };Ok::<_,StorageError>((tx,code))
        })).await?;
        guard()?;
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)?;
        guard()?;
        Ok(code)
    }
    pub async fn verify_background_code(
        &self,
        s: &SessionRef,
        task: &TaskId,
        p: &OperationId,
        id: &str,
        code: Zeroizing<String>,
    ) -> Result<bool, StorageError> {
        let b = self.store.identity(s)?;
        let guard = || {
            if self.store.identity(s)? == b {
                Ok(())
            } else {
                Err(StorageError::Session)
            }
        };
        let(tx,valid)=crate::repository::guarded_poll(&guard,Box::pin(async{
            let(mut tx,b,saved)=self.consent_transaction(s,task,p,Some(id)).await?;
            let c=self.read_consent(&mut tx,s,&b,&saved,id,true).await?;let t=now(&mut tx).await?;
            let host:Option<String>=c.try_get("host_text")?;
            if host.as_deref()!=Some(&self.background_config()?.generation.to_string()) || c.try_get::<i16,_>("failures")?>=5 || c.try_get::<Option<String>,_>("auth_ref")?.is_some() || c.try_get::<Option<DateTime<Utc>>,_>("challenge_until")?.is_none_or(|v|t>=v){return Err(StorageError::AuthChallengeInactive);}
            let expected:String=c.try_get("code_hash")?;
            let valid=code.len()==6 && code.bytes().all(|c|c.is_ascii_digit()) && bool::from(expected.as_bytes().ct_eq(code_hash(id,&code).as_bytes()));
            if valid{sqlx::query("UPDATE payment_background_consents_v1 SET auth_at=$2,auth_until=$2+interval '2 minutes',auth_ref=$3 WHERE consent_id=$1::uuid").bind(id).bind(t).bind(format!("isolated_payment_grant_{}",uuid::Uuid::new_v4())).execute(&mut *tx).await?;}
            else{sqlx::query("UPDATE payment_background_consents_v1 SET failures=failures+1 WHERE consent_id=$1::uuid").bind(id).execute(&mut *tx).await?;}
            Ok((tx,valid))
        })).await?;
        guard()?;
        tx.commit()
            .await
            .map_err(|_| StorageError::ReconcileRequired)?;
        guard()?;
        Ok(valid)
    }
    async fn read_consent(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        s: &SessionRef,
        b: &SessionBinding,
        saved: &SavedPayment,
        id: &str,
        confirmed: bool,
    ) -> Result<PgRow, StorageError> {
        uuid::Uuid::parse_str(id).map_err(|_| StorageError::Invalid("consent ID"))?;
        let c=sqlx::query("SELECT *,host_generation::text AS host_text FROM payment_background_consents_v1 WHERE consent_id=$1::uuid AND preparation_operation_id=$2 FOR UPDATE").bind(id).bind(saved.op.operation_id.as_str()).fetch_optional(&mut **tx).await?.ok_or(StorageError::NotFound)?;
        let (se, ae) = fences(tx, b, s, &saved.snapshot.transfer.payer_account_id, false).await?;
        let display: Value = c.try_get::<Json<Value>, _>("display")?.0;
        if c.try_get::<String, _>("owner_id")? != b.user_id.as_str()
            || c.try_get::<String, _>("session_digest")? != session_digest(s)?
            || c.try_get::<i64, _>("binding_version")? != db_version(b.binding_version)?
            || c.try_get::<i64, _>("session_epoch")? != se
            || c.try_get::<i64, _>("account_epoch")? != ae
            || display["snapshot"] != json!(saved.snapshot)
            || display["preparation_binding"] != json!(saved.stored.record.binding)
            || display["catalog_adoption"]
                != catalog(tx, &saved.snapshot.transfer.payer_account_id).await?
            || hash(&display)?.as_str() != c.try_get::<String, _>("display_digest")?
        {
            return Err(StorageError::ConfirmationBindingMismatch);
        }
        // Re-read DB clock after all waits; client/worker clocks are never authority.
        if confirmed {
            let t = now(tx).await?;
            if c.try_get::<Option<DateTime<Utc>>, _>("confirmed_until")?
                .is_none_or(|v| t >= v)
            {
                return Err(StorageError::ConfirmationExpired);
            }
        }
        Ok(c)
    }
    pub(super) async fn acceptance_consent(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        s: &SessionRef,
        b: &SessionBinding,
        saved: &SavedPayment,
        id: &str,
    ) -> Result<AcceptedConsent, StorageError> {
        let c = self.read_consent(tx, s, b, saved, id, true).await?;
        let t = now(tx).await?;
        if c.try_get::<Option<String>, _>("host_text")?.as_deref()
            != Some(&self.background_config()?.generation.to_string())
            || c.try_get::<Option<DateTime<Utc>>, _>("auth_until")?
                .is_none_or(|v| t >= v)
        {
            return Err(StorageError::FormalAuthUnavailable);
        }
        Ok(AcceptedConsent {
            id: id.into(),
            account_epoch: c.try_get("account_epoch")?,
            display: c.try_get::<Json<Value>, _>("display")?.0,
            auth_ref: AuthGrantRef::parse(c.try_get::<String, _>("auth_ref")?)
                .map_err(|_| StorageError::CorruptRecord)?,
        })
    }
    pub(super) async fn acceptance_fences(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        s: &SessionRef,
        b: &SessionBinding,
        task: &TaskId,
        p: &OperationId,
    ) -> Result<(), StorageError> {
        let row = sqlx::query("SELECT * FROM tasks WHERE task_id=$1 AND owner_id=$2")
            .bind(task.as_str())
            .bind(b.user_id.as_str())
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(StorageError::NotFound)?;
        let t = crate::repository::task_row(&row)?;
        let saved = saved_payment(tx, &t, p, false).await?;
        fences(tx, b, s, &saved.snapshot.transfer.payer_account_id, false).await?;
        Ok(())
    }
}
pub(super) fn authentication(c: &AcceptedConsent) -> VerifiedPermitAuthentication {
    VerifiedPermitAuthentication::IsolatedPayment {
        grant_ref: c.auth_ref.clone(),
    }
}
pub(in crate::permit) async fn guard_payment_consent(
    unit: &mut PermitTransaction,
) -> Result<(), StorageError> {
    let p = unit
        .payment
        .as_ref()
        .ok_or(StorageError::FormalAuthUnavailable)?;
    let Some(c) = p.background.as_ref() else {
        return Ok(());
    };
    let tx = unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
    let (se, ae) = fences(
        tx,
        &unit.identity,
        &unit.session,
        &p.snapshot.transfer.payer_account_id,
        false,
    )
    .await?;
    let row=sqlx::query("SELECT *,host_generation::text AS host_text FROM payment_background_consents_v1 WHERE consent_id=$1::uuid FOR UPDATE").bind(&c.id).fetch_one(&mut **tx).await?;
    let t = now(tx).await?;
    if ae != c.account_epoch
        || row.try_get::<i64, _>("session_epoch")? != se
        || row.try_get::<String, _>("session_digest")? != session_digest(&unit.session)?
        || row.try_get::<i64, _>("binding_version")? != db_version(unit.identity.binding_version)?
        || row.try_get::<String, _>("auth_ref")? != c.auth_ref.as_str()
        || row.try_get::<Json<Value>, _>("display")?.0 != c.display
        || row.try_get::<DateTime<Utc>, _>("auth_until")? <= t
        || row.try_get::<DateTime<Utc>, _>("confirmed_until")? <= t
    {
        return Err(StorageError::AuthExpired);
    }
    Ok(())
}
pub(super) async fn build_acceptance(
    unit: &mut PermitTransaction,
    preparation: &OperationId,
) -> Result<(), StorageError> {
    guard_payment_consent(unit).await?;
    let p = unit
        .payment
        .clone()
        .ok_or(StorageError::TransactionAborted)?;
    let Some(c) = p.background else {
        return Ok(());
    };
    let tx = unit.tx.as_mut().ok_or(StorageError::TransactionAborted)?;
    let history = evidence::inspect_processing(tx, &unit.task, preparation).await?;
    let source =
        sqlx::query("SELECT * FROM public_unified_fictional_source_v1 WHERE singleton=1 FOR SHARE")
            .fetch_one(&mut **tx)
            .await?;
    let evidence = json!({"schema":"payment-background-acceptance-v1","mode":"PROJECT_SIMULATION","history":history,
        "consent_id":c.id,"delegation_display":c.display,"account_epoch":c.account_epoch,
        "owner_id":unit.identity.user_id,"payer_account_id":p.snapshot.transfer.payer_account_id,
        "request_payload":p.request.payload,"request_digest":p.request.digest()?,"snapshot":p.snapshot,
        "source_id":source.try_get::<String,_>("source_id")?,"source_revision":source.try_get::<String,_>("source_revision")?,
        "source_implementation":source.try_get::<String,_>("implementation_revision")?,"source_sha256":source.try_get::<String,_>("artifact_sha256")?,
        "catalog_adoption":catalog(tx,&p.snapshot.transfer.payer_account_id).await?,
        "amount_cents":history["amount_cents"],"original_recipient_account_id":history["original_recipient_account_id"]});
    // The restricted API login cannot write grant/control/job tables directly.
    // The definer validates the consumed closed permit in this same transaction.
    let gid: String = sqlx::query_scalar("SELECT payment_background_accept_v1($1::uuid,$2)")
        .bind(&c.id)
        .bind(Json(&evidence))
        .fetch_one(&mut **tx)
        .await?;
    audit(tx,&unit.task,unit.transfer_result.as_ref(),PersistentAuditKind::SecurityDecisionRecorded,json!({"scope":"EXPLICIT_BACKGROUND_ACCEPTANCE_V1","grant_id":gid,"consent_id":c.id,"policy":BACKGROUND_POLICY,"completion_minutes":30})).await?;
    Ok(())
}
