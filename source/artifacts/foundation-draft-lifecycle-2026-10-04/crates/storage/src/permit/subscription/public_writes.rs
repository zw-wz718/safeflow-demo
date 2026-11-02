//! Explicit team-source public boundary. Client selections never carry authority.
use super::*;
use crate::{simulation_source::SimulationFence, TeamConsumptionSource};
use std::{collections::HashMap, sync::Mutex, time::Instant};

fn approved_service_display_name(merchant_id: Option<&str>, service_ref: Option<&str>) -> Option<&'static str> {
    match (merchant_id, service_ref) {
        (Some("demo_merchant_media_v1"), Some("demo_service_video_v1")) => Some("星河视频月度会员"),
        (Some("demo_merchant_media_v1"), Some("demo_service_music_v1")) => Some("悦听音乐月度会员"),
        _ => None,
    }
}

// Display-only metadata: exact internal identity and one owner-limited calendar
// entry are required. No source JSON, receipt, confirmation or business digest
// is changed, and no private identity is added to the public response.
fn display_metadata(calendar: &Value, merchant: Option<&str>, service: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> Value {
    let matched: Vec<_> = calendar["evidence"].as_array().into_iter().flatten()
        .filter(|e| merchant.is_some() && service.is_some() && e["merchant_id"].as_str()==merchant && e["service_ref"].as_str()==service).collect();
    if matched.len()!=1 { return json!({"version":1,"name":null,"amount":null,"currency":null,"cycle":null,"billing_day":null,"next_charge_at":null,"expires_at":null}); }
    let e=matched[0];
    let future=|key:&str| e[key].as_str().and_then(|v|chrono::DateTime::parse_from_rfc3339(v).ok())
        .filter(|v|*v>=now).map(|v|v.to_rfc3339());
    json!({"version":1,"name":approved_service_display_name(merchant,service).map(|n|json!(n)).unwrap_or(e["display_name"].clone()),
        "amount":e["amount"],"currency":e["currency"],"cycle":e["cycle"],"billing_day":e["billing_day"],
        "next_charge_at":future("next_charge_at"),"expires_at":future("expires_at")})
}

#[cfg(test)]
mod display_metadata_tests {
    use super::*;
    #[test]
    fn exact_identity_unique_calendar_and_time_boundaries() {
        let now=chrono::DateTime::parse_from_rfc3339("2026-10-29T12:00:00+08:00").unwrap().with_timezone(&chrono::Utc);
        let entry=json!({"merchant_id":"demo_merchant_media_v1","service_ref":"demo_service_video_v1","display_name":"演示视频服务","amount":"19.90","currency":"CNY","cycle":"MONTHLY","billing_day":5,"next_charge_at":"2026-11-05T00:00:00+08:00","expires_at":"2026-10-28T00:00:00+08:00"});
        let calendar=json!({"evidence":[entry.clone()]});
        let before=calendar.clone();
        let metadata=display_metadata(&calendar,Some("demo_merchant_media_v1"),Some("demo_service_video_v1"),now);
        assert_eq!(metadata["name"],"星河视频月度会员");
        assert_eq!(metadata["amount"],"19.90");
        assert_eq!(metadata["billing_day"],5);
        assert_eq!(metadata["next_charge_at"],"2026-11-05T00:00:00+08:00");
        assert!(metadata["expires_at"].is_null());
        assert_eq!(calendar,before,"display must not rewrite source");
        for (merchant,service) in [(None,None),(Some("other"),Some("demo_service_video_v1")),(Some("demo_merchant_media_v1"),None),(Some("demo_merchant_media_v1"),Some("demo_service_music_v1"))] {
            let unknown=display_metadata(&calendar,merchant,service,now);
            assert!(unknown["name"].is_null());assert!(unknown["next_charge_at"].is_null());assert!(unknown["amount"].is_null());
        }
        let duplicate=json!({"evidence":[entry.clone(),entry.clone()]});
        assert!(display_metadata(&duplicate,Some("demo_merchant_media_v1"),Some("demo_service_video_v1"),now)["name"].is_null());
        let mut invalid=entry.clone();invalid["next_charge_at"]=json!("tomorrow");invalid["expires_at"]=json!("2026-02-30");
        let bad=display_metadata(&json!({"evidence":[invalid]}),Some("demo_merchant_media_v1"),Some("demo_service_video_v1"),now);
        assert!(bad["next_charge_at"].is_null());assert!(bad["expires_at"].is_null());
        assert!(!metadata.to_string().contains("demo_service_"));
        assert_eq!(approved_service_display_name(Some("demo_merchant_media_v1"),Some("demo_service_music_v1")),Some("悦听音乐月度会员"));
    }
}

#[derive(Clone)]
pub struct SubscriptionPublicScope {
    pub(super) fence: SimulationFence,
    host: SubscriptionHost,
    source: TeamConsumptionSource,
    read: SubscriptionService,
    activation: String,
    issuer: uuid::Uuid,
    captures: Arc<Mutex<HashMap<String, (Instant, SubscriptionRegistrationCapture)>>>,
}
/// Only public selection handles and user choices; no identity, dates, journal,
/// Allowed flags, original results or source hashes can be supplied here.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag="kind", rename_all="SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum SubscriptionPublicRequest {
    Registration { capture_ref: String, candidate_index: usize },
    ReminderSettings { selection_ref: String, enabled: bool, days_before: u8 },
    RecordCancellation { selection_ref: String },
    RevokeCancellation { selection_ref: String },
}
pub struct SubscriptionPublicWrites<'a> {
    inner: SubscriptionWriteService<'a>,
}

impl SubscriptionPublicScope {
    pub(crate) fn reminder_accounts(&self) -> &[String] { self.fence.accounts() }
    pub(crate) fn reminder_host(&self) -> &SubscriptionHost { &self.host }
    pub(crate) fn reminder_binding(&self) -> Value {
        json!({"source":self.fence.provenance(),"calendar":self.source.calendar_binding_for_backend(),
            "activation":self.activation,"host_instance":self.host.instance,"host_generation":self.host.generation})
    }
    /// Internal database fence reused by the native reminder host. No public
    /// request, deserialization or client-supplied authority is added.
    pub(crate) async fn check_reminder_database(&self,s:&SessionRef,c:&mut sqlx::PgConnection)->Result<(),StorageError>{
        let identity=self.read.identity(s)?;
        if identity.user_id.as_str()!=self.source.owner(){return Err(StorageError::NotFound);}
        self.fence.lock(c).await?;
        self.source.check_calendar_on_connection(c).await?;
        let dedicated:bool=sqlx::query_scalar("SELECT session_user='sf_noon_reminder_20261101'").fetch_one(&mut *c).await?;
        if dedicated {sqlx::query("SELECT demo_reminder_authority47_v1()").execute(&mut *c).await?;}
        self.read.check_host(c,&self.host,identity.user_id.as_str()).await?;
        let activation:Option<String>=sqlx::query_scalar("SELECT source_id::text FROM public_subscription_a2_sources_v1 WHERE owner_id=$1 AND source_id=$2::uuid AND source_revision=$3 AND database_name=current_database() AND namespace=current_schema()")
            .bind(identity.user_id.as_str()).bind(&self.activation).bind(crate::TEAM_CONSUMPTION_REVISION).fetch_optional(&mut *c).await?;
        let accounts:Vec<String>=sqlx::query_scalar("SELECT account_id FROM transfer_accounts_v1 WHERE owner_id=$1 ORDER BY account_id")
            .bind(identity.user_id.as_str()).fetch_all(&mut *c).await?;
        if activation.as_deref()!=Some(&self.activation) || accounts!=self.fence.accounts(){return Err(StorageError::FactsConflict);}
        self.read.check(s,&identity)
    }
    pub(crate) async fn authorize_reminder_read(&self,s:&SessionRef,bank:&dyn safeflow_bank_gateway::BankGateway)->Result<(),StorageError>{
        let identity=self.read.identity(s)?;
        if identity.user_id.as_str()!=self.source.owner(){return Err(StorageError::NotFound);}
        self.read.check_simulation_accounts(s,bank,&identity,self.fence.accounts()).await?;
        let accounts:Vec<String>=sqlx::query_scalar("SELECT account_id FROM transfer_accounts_v1 WHERE owner_id=$1 ORDER BY account_id")
            .bind(identity.user_id.as_str()).fetch_all(&self.read.repository.pool).await?;
        let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public_unified_fictional_source_v1 WHERE singleton=1 AND active AND source_id=$1 AND source_revision=$2)")
            .bind(crate::UNIFIED_FICTIONAL_SOURCE).bind(crate::UNIFIED_FICTIONAL_REVISION).fetch_one(&self.read.repository.pool).await?;
        self.read.check(s,&identity)?;
        if !active || accounts!=self.fence.accounts(){return Err(StorageError::NotFound);}
        Ok(())
    }
}

impl SubscriptionService {
    /// Explicit native install, only in the fixed team's new isolated database.
    /// Initialization and host startup must already have genuinely occurred.
    pub async fn install_team_public_scope_for_backend(
        &self, s:&SessionRef, host:&SubscriptionHost, source:&TeamConsumptionSource,
        bank:&dyn safeflow_bank_gateway::BankGateway,
    )->Result<SubscriptionPublicScope,StorageError>{
        let owner=self.identity(s)?.user_id;
        let scope=SubscriptionA2CandidateScope::for_team_source(source,owner)?;
        let capture=self.capture_simulation_for_backend(s,bank,&source.persistent_for_backend(),&source.request()).await?;
        let mut tx=self.repository.pool.begin().await?;
        capture.check_same_database(&mut tx).await?;
        self.check_host(&mut tx,host,source.owner()).await?;
        self.load(&mut tx,source.owner()).await?;
        self.check(s,&self.identity(s)?)?;
        tx.commit().await?;
        capture.check_live()?;
        drop(capture);
        self.repository.install_subscription_a2_candidate_for_isolated_backend(&scope).await?;
        self.open_team_public_scope_for_backend(s,host,source,bank).await
    }
    /// Reopen the fixed original activation; never silently create a new one.
    pub async fn open_team_public_scope_for_backend(
        &self,s:&SessionRef,host:&SubscriptionHost,source:&TeamConsumptionSource,
        bank:&dyn safeflow_bank_gateway::BankGateway,
    )->Result<SubscriptionPublicScope,StorageError>{
        let identity=self.identity(s)?;
        if identity.user_id.as_str()!=source.owner() || source.repository_for_backend().namespace!=self.repository.namespace {
            return Err(StorageError::TransactionScope);
        }
        let capture=self.capture_simulation_for_backend(s,bank,&source.persistent_for_backend(),&source.request()).await?;
        let mut tx=self.repository.pool.begin().await?;
        capture.check_same_database(&mut tx).await?;
        self.check_host(&mut tx,host,identity.user_id.as_str()).await?;
        self.load(&mut tx,identity.user_id.as_str()).await?;
        let activation=domain_audit::check_source(&mut tx,identity.user_id.as_str(),None).await?.ok_or(StorageError::DependencyUnavailable)?;
        let revision:String=sqlx::query_scalar("SELECT source_revision FROM public_subscription_a2_sources_v1 WHERE source_id=$1::uuid").bind(&activation).fetch_one(&mut *tx).await?;
        if revision!=crate::TEAM_CONSUMPTION_REVISION {return Err(StorageError::TransactionScope);}
        self.check(s,&identity)?;
        tx.commit().await?;
        capture.check_live()?;
        Ok(SubscriptionPublicScope{fence:capture.fence.clone(),host:host.clone(),source:source.clone(),read:self.clone(),activation,
            issuer:uuid::Uuid::new_v4(),captures:Arc::new(Mutex::new(HashMap::new()))})
    }
}
impl<'a> SubscriptionWriteService<'a> {
    pub fn for_team_public_scope(mut self,scope:&SubscriptionPublicScope)->Result<SubscriptionPublicWrites<'a>,StorageError>{
        if self.read.repository.namespace!=scope.read.repository.namespace || !Arc::ptr_eq(&self.read.sessions,&scope.read.sessions) {
            return Err(StorageError::TransactionScope);
        }
        self.read=scope.read.clone();
        self.selection_issuer=scope.issuer;
        self.candidate_activation=Some(scope.activation.clone());
        self.public_scope=Some(scope.clone());
        Ok(SubscriptionPublicWrites{inner:self})
    }
}
pub(super) async fn check_live(
    service:&SubscriptionWriteService<'_>,scope:&SubscriptionPublicScope,s:&SessionRef,
    tx:&mut Transaction<'_,Postgres>,
)->Result<(),StorageError>{
    let identity=service.read.identity(s)?;
    if identity.user_id.as_str()!=scope.source.owner(){return Err(StorageError::NotFound);}
    scope.fence.lock(tx).await?;
    scope.source.check_calendar_on_connection(tx).await?;
    domain_audit::check_source(tx,identity.user_id.as_str(),Some(&scope.activation)).await?;
    service.read.check_host(tx,&scope.host,identity.user_id.as_str()).await?;
    service.read.check_simulation_accounts(s,service.bank.as_ref(),&identity,scope.fence.accounts()).await?;
    scope.fence.lock(tx).await?;
    service.read.check(s,&identity)
}
pub(super) fn build_evidence(
    service:&SubscriptionWriteService<'_>,identity:&SessionBinding,s:&SessionRef,nonce:&str,
    input:&Input,member:&Value,preparation:&Value,
)->Result<Option<Value>,StorageError>{
    let Some(scope)=&service.public_scope else{return Ok(None)};
    let context=service.public_request.as_ref().ok_or(StorageError::DependencyUnavailable)?;
    Ok(Some(json!({"version":1,"owner":identity.user_id,"namespace":service.read.repository.namespace,
        "session_digest":hash(&("safeflow-confirmation-session:v1",s.as_str()))?,"binding_version":identity.binding_version,
        "nonce":nonce,"business_ref":input.business_ref,"operation_id":member["context"]["operation_id"],
        "source":scope.fence.provenance(),"activation":scope.activation,
        "host_instance":scope.host.instance,"host_generation":scope.host.generation,
        "public_context":context,"original_preparation":preparation,"member_binding":member})))
}
pub(super) async fn save_evidence(tx:&mut Transaction<'_,Postgres>,op:&OperationId,input:&Input,body:&Value)->Result<(),StorageError>{
    let digest=input.public_evidence.as_ref().ok_or(StorageError::CorruptRecord)?;
    sqlx::query("INSERT INTO subscription_public_evidence_v1(operation_id,business_ref,owner_id,evidence_digest,body,body_sha256) VALUES($1,$2,$3,$4,$5,encode(sha256(convert_to(($5::jsonb)::text,'UTF8')),'hex'))")
        .bind(op.as_str()).bind(&input.business_ref).bind(body["owner"].as_str().ok_or(StorageError::CorruptRecord)?)
        .bind(digest.as_str()).bind(body).execute(&mut **tx).await?;
    Ok(())
}
pub(super) async fn check_prepared_version(tx:&mut Transaction<'_,Postgres>,owner:&UserId,input:&Input)->Result<(),StorageError>{
    let expected=i64::try_from(input.storage_version).map_err(|_|StorageError::CorruptRecord)?.checked_add(1).ok_or(StorageError::CorruptRecord)?;
    let current:i64=sqlx::query_scalar("SELECT storage_version FROM subscription_runtime_records WHERE owner_id=$1 FOR UPDATE").bind(owner.as_str()).fetch_one(&mut **tx).await?;
    if current!=expected{return Err(StorageError::FactsConflict);}
    Ok(())
}
pub(super) async fn verify_evidence(
    tx:&mut Transaction<'_,Postgres>,owner:&UserId,op:&OperationId,scope:Option<&SubscriptionPublicScope>,
)->Result<Value,StorageError>{
    let row=sqlx::query("SELECT e.body,e.evidence_digest,b.member_binding,b.public_parameters,b.business_ref,b.session_digest,b.binding_version FROM subscription_public_evidence_v1 e JOIN subscription_public_bindings_v1 b USING(operation_id) WHERE b.owner_id=$1 AND b.operation_id=$2")
        .bind(owner.as_str()).bind(op.as_str()).fetch_optional(&mut **tx).await?.ok_or(StorageError::ReconcileRequired)?;
    let body:Value=row.try_get("body")?;
    let digest:String=row.try_get("evidence_digest")?;
    let input:Input=serde_json::from_value(row.try_get("public_parameters")?).map_err(|_|StorageError::CorruptRecord)?;
    if hash(&body)?.as_str()!=digest || input.public_evidence.as_ref().map(ArgumentsHash::as_str)!=Some(digest.as_str())
        || body["owner"]!=owner.as_str() || body["operation_id"]!=op.as_str()
        || body["business_ref"]!=row.try_get::<String,_>("business_ref")?
        || body["member_binding"]!=row.try_get::<Value,_>("member_binding")?
        || body["session_digest"]!=row.try_get::<String,_>("session_digest")?
        || body["binding_version"]!=row.try_get::<i64,_>("binding_version")? {
        return Err(StorageError::ReconcileRequired);
    }
    if let Some(scope)=scope {
        if body["source"]!=scope.fence.provenance() || body["host_instance"]!=scope.host.instance
            || body["host_generation"]!=scope.host.generation || body["activation"]!=scope.activation {
            return Err(StorageError::FactsConflict);
        }
    }
    Ok(body)
}
impl SubscriptionPublicWrites<'_>{
    fn scope(&self)->Result<&SubscriptionPublicScope,StorageError>{
        self.inner.public_scope.as_ref().ok_or(StorageError::DependencyUnavailable)
    }
    async fn guard(&self,s:&SessionRef)->Result<(),StorageError>{
        let mut tx=self.inner.store.repository.pool.begin().await?;
        check_live(&self.inner,self.scope()?,s,&mut tx).await?;
        tx.commit().await?;
        Ok(())
    }
    /// History need not inherit a live business host or consumption generation.
    /// It still requires CURRENT account authority. This final RC statement uses
    /// the primary writer pool, never the earlier RR Journal snapshot.
    async fn read_authorized(&self,s:&SessionRef)->Result<(),StorageError>{
        let identity=self.inner.read.identity(s)?;
        let scope=self.scope()?;
        if identity.user_id.as_str()!=scope.source.owner(){return Err(StorageError::NotFound);}
        self.inner.read.check_simulation_accounts(s,self.inner.bank.as_ref(),&identity,scope.fence.accounts()).await?;
        let valid:bool=sqlx::query_scalar("/* subscription_public_current_read_authority_v1 */ SELECT coalesce((SELECT array_agg(account_id ORDER BY account_id)=$2::text[] FROM transfer_accounts_v1 WHERE owner_id=$1),false) AND EXISTS(SELECT 1 FROM public_unified_fictional_source_v1 WHERE singleton=1 AND active AND source_id=$3 AND source_revision=$4)")
            .bind(identity.user_id.as_str()).bind(scope.fence.accounts()).bind(crate::UNIFIED_FICTIONAL_SOURCE).bind(crate::UNIFIED_FICTIONAL_REVISION).fetch_one(&self.inner.store.repository.pool).await?;
        self.inner.read.check(s,&identity)?;
        if !valid{return Err(StorageError::NotFound);}
        Ok(())
    }
    pub async fn original(&self,s:&SessionRef,reference:&str)->Result<Value,StorageError>{
        let view=self.inner.read.original(s,reference).await?;
        self.read_authorized(s).await?;
        Ok(view)
    }
    pub async fn details(&self,s:&SessionRef,reference:&str)->Result<Value,StorageError>{
        let view=self.inner.read.details(s,reference).await?;
        self.read_authorized(s).await?;
        Ok(view)
    }
    pub async fn capture(&self,s:&SessionRef)->Result<Value,StorageError>{
        self.guard(s).await?;
        let scope=self.scope()?;
        let capture=self.inner.capture_team_registration_for_backend(s,&scope.host,&scope.source).await?;
        let preview=capture.preview().clone();
        let groups=preview.pointer("/recognition/groups").and_then(Value::as_array).ok_or(StorageError::CorruptRecord)?;
        if groups.len()!=capture.display_identities().len(){return Err(StorageError::CorruptRecord);}
        let calendar=scope.source.calendar_for_backend();
        let now=chrono::Utc::now();
        let display:Vec<_>=groups.iter().zip(capture.display_identities()).enumerate().map(|(index,(group,(merchant,service)))| {
            if group["selection_index"].as_u64()!=Some(index as u64){return Err(StorageError::CorruptRecord);}
            Ok(json!({"selection_index":index,"metadata":display_metadata(&calendar,Some(merchant),service.as_deref(),now)}))
        }).collect::<Result<_,StorageError>>()?;
        let reference=uuid::Uuid::new_v4().to_string();
        let mut cache=scope.captures.lock().map_err(|_|StorageError::DependencyUnavailable)?;
        cache.retain(|_,(expires,_)|*expires>Instant::now());
        if cache.len()>=256{return Err(StorageError::DependencyUnavailable);}
        cache.insert(reference.clone(),(Instant::now()+std::time::Duration::from_secs(20),capture));
        Ok(json!({"capture_ref":reference,"preview":preview,"display":{"version":1,"groups":display},"mode":"PROJECT_SIMULATION","business_executed":false}))
    }
    /// Safe adopted display names on the original owner list; handles and
    /// member ordering are retained, and no domain IDs are exposed.
    pub async fn list(&self,s:&SessionRef)->Result<Value,StorageError>{
        let identity=self.inner.read.identity(s)?;
        if identity.user_id.as_str()!=self.scope()?.source.owner(){return Err(StorageError::NotFound);}
        let mut view=self.inner.read.read(s).await?;
        let mut tx=self.inner.store.repository.subscription_read_pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await?;
        let snapshot=self.inner.read.load(&mut tx,identity.user_id.as_str()).await?;
        let entries=snapshot.ledger_mock().list_mock(identity.user_id.as_str()).map_err(|_|StorageError::CorruptRecord)?;
        let calendar=self.scope()?.source.calendar_for_backend();
        for item in view["items"].as_array_mut().ok_or(StorageError::CorruptRecord)? {
            let (id,version)=self.inner.read.target_for_public_write(s,item["selection_ref"].as_str().ok_or(StorageError::CorruptRecord)?)?;
            if version!=snapshot.version_mock(){return Err(StorageError::FactsConflict);}
            let entry=entries.iter().find(|e|e.subscription_id==id).ok_or(StorageError::NotFound)?;
            let matched:Vec<_>=calendar["evidence"].as_array().ok_or(StorageError::CorruptRecord)?.iter().filter(|e|e["merchant_id"].as_str()==entry.merchant_id.as_deref() && e["service_ref"].as_str()==entry.service_ref.as_deref()).collect();
            item["service_display_name"]=if matched.len()==1 {
                approved_service_display_name(entry.merchant_id.as_deref(),entry.service_ref.as_deref())
                    .map_or_else(||matched[0]["display_name"].clone(),|name|json!(name))
            } else {Value::Null};
            item["display"]=display_metadata(&calendar,entry.merchant_id.as_deref(),entry.service_ref.as_deref(),chrono::Utc::now());
        }
        self.inner.read.check(s,&identity)?;
        tx.commit().await?;
        self.inner.read.check(s,&identity)?;
        self.read_authorized(s).await?;
        view["writes_enabled"]=json!(true);
        view["scope"]=json!("SUBSCRIPTION_PUBLIC_SIMULATION");
        Ok(view)
    }
    pub async fn prepare(mut self,s:&SessionRef,nonce:&str,request:SubscriptionPublicRequest)->Result<SubscriptionPreparation,StorageError>{
        self.guard(s).await?;
        let incoming=json!(request);
        let identity=self.inner.read.identity(s)?;
        let existing:Option<String>=sqlx::query_scalar("SELECT b.operation_id FROM subscription_preparations_v1 p JOIN subscription_public_bindings_v1 b USING(business_ref) WHERE p.nonce=$1 AND b.owner_id=$2")
            .bind(nonce).bind(identity.user_id.as_str()).fetch_optional(&self.inner.store.repository.subscription_read_pool).await?;
        if let Some(op)=existing {
            let mut tx=self.inner.store.repository.subscription_read_pool.begin().await?;
            let body=verify_evidence(&mut tx,&identity.user_id,&OperationId::parse(op).map_err(|_|StorageError::CorruptRecord)?,Some(self.scope()?)).await?;
            if body["public_context"]["request"]!=incoming{return Err(StorageError::FactsConflict);}
            tx.commit().await?;
            let result=self.inner.recover_technical(s,nonce).await?;
            self.guard(s).await?;
            return Ok(result);
        }
        let scope=self.scope()?.clone();
        match request {
            SubscriptionPublicRequest::Registration{capture_ref,candidate_index}=>{
                let (expires,capture)=scope.captures.lock().map_err(|_|StorageError::DependencyUnavailable)?.remove(&capture_ref).ok_or(StorageError::FactsConflict)?;
                if expires<=Instant::now(){return Err(StorageError::ConfirmationExpired);}
                let preview=capture.preview().clone();
                let selection=self.inner.select_registration_from_capture_for_trusted_backend(s,&scope.host,capture,candidate_index).await?;
                self.inner.public_request=Some(json!({"request":incoming,"summary":{"action":"REGISTRATION","candidate_index":candidate_index,"candidate":preview["recognition"]["groups"][candidate_index],"message":"仅登记本人已核对的本地订阅，不代表商户操作或真实通知。"}}));
                self.inner.prepare_technical(s,&scope.host,nonce,Preparation::Registration(&selection)).await
            }
            change=>{
                let reference=match &change {
                    SubscriptionPublicRequest::ReminderSettings{selection_ref,..}
                    |SubscriptionPublicRequest::RecordCancellation{selection_ref}
                    |SubscriptionPublicRequest::RevokeCancellation{selection_ref}=>selection_ref,
                    _=>return Err(StorageError::FactsConflict),
                };
                let (target,version)=self.inner.read.target_for_public_write(s,reference)?;
                let capture=self.inner.read.capture_simulation_for_backend(s,self.inner.bank.as_ref(),&scope.source.persistent_for_backend(),&scope.source.request()).await?;
                let mut tx=self.inner.store.repository.pool.begin().await?;
                capture.check_same_database(&mut tx).await?;
                let snap=self.inner.read.load(&mut tx,identity.user_id.as_str()).await?;
                if snap.version_mock()!=version{return Err(StorageError::FactsConflict);}
                let entry=snap.ledger_mock().list_mock(identity.user_id.as_str()).map_err(|_|StorageError::CorruptRecord)?
                    .iter().find(|e|e.subscription_id==target).ok_or(StorageError::NotFound)?.clone();
                let mut context=json!({"request":incoming,"capture":capture.evidence_for_public(),"target":entry,"storage_version":version});
                let r=match change {
                    SubscriptionPublicRequest::RecordCancellation{..}=>{
                        context["summary"]=json!({"action":"RECORD_CANCELLATION","message":"仅记录本地取消意图，请到商户官方渠道处理；不表示商户退订。"});
                        SubscriptionChangeRequestMock::CreateCancellationIntent{subscription_id:target.clone()}
                    }
                    SubscriptionPublicRequest::RevokeCancellation{..}=>{
                        context["summary"]=json!({"action":"REVOKE_CANCELLATION","message":"仅撤回本地取消意图，不表示恢复商户服务。"});
                        SubscriptionChangeRequestMock::RevokeCancellationIntent{subscription_id:target.clone()}
                    }
                    SubscriptionPublicRequest::ReminderSettings{enabled,days_before,..}=>{
                        let database_now:chrono::DateTime<chrono::Utc>=sqlx::query_scalar("SELECT clock_timestamp()").fetch_one(&mut *tx).await?;
                        let calendar=scope.source.calendar_at(database_now);
                        let matches:Vec<_>=calendar["evidence"].as_array().ok_or(StorageError::DependencyUnavailable)?.iter()
                            .filter(|e|e["owner_user_id"]==identity.user_id.as_str() && e["merchant_id"].as_str()==entry.merchant_id.as_deref() && e["service_ref"].as_str()==entry.service_ref.as_deref()).collect();
                        if matches.len()!=1 || entry.service_ref.is_none(){return Err(StorageError::DependencyUnavailable);}
                        let original=matches[0];
                        let rows=&context["capture"]["transactions"];
                        let ids=original["transaction_ids"].as_array().ok_or(StorageError::CorruptRecord)?;
                        if ids.len()<3 || !ids.iter().all(|id|rows.as_array().is_some_and(|rs|rs.iter().any(|t|t["transaction_id"]==*id && t["account_id"]==original["account_id"] && t["merchant_id"]==original["merchant_id"] && t["service_ref"]==original["service_ref"]))) {
                            return Err(StorageError::FactsConflict);
                        }
                        let date=|key:&str|->Result<chrono::NaiveDate,StorageError>{
                            chrono::DateTime::parse_from_rfc3339(original[key].as_str().ok_or(StorageError::DependencyUnavailable)?)
                                .map(|v|v.with_timezone(&chrono::FixedOffset::east_opt(8*3600).unwrap()).date_naive()).map_err(|_|StorageError::CorruptRecord)
                        };
                        let schedule=Schedule{cycle:safeflow_subscription_evidence::recognition::Cycle::CalendarMonth,
                            anchor_date:date("service_period_start")?,period_index:1,expected_date:date("next_charge_at")?,enabled,days_before};
                        schedule.validate().map_err(|_|StorageError::Invalid("calendar schedule"))?;
                        let due=schedule.expected_date.checked_sub_days(chrono::Days::new(days_before.into())).ok_or(StorageError::CorruptRecord)?;
                        let current=snap.reminder_plan_mock(&target).map_err(|_|StorageError::CorruptRecord)?;
                        let original_sha=if original["calendar_revision"]==crate::TEAM_CALENDAR_NEXT_REVISION{crate::TEAM_CALENDAR_NEXT_SHA}else{crate::TEAM_CONSUMPTION_SHA};
                        context["calendar"]=json!({"original":original,"adoption":calendar["adoption"],"original_sha256":original_sha,"schedule":schedule,"prior_settings":current});
                        context["summary"]=json!({"action":"REMINDER_SETTINGS","service":original["display_name"],"schedule":schedule,"timezone":"Asia/Shanghai","reminder_date":due.to_string(),"expires_at":original["expires_at"],"exact_send_time_promised":false,"notification_sent":false});
                        SubscriptionChangeRequestMock::UpdateReminderSettings{subscription_id:target.clone(),schedule}
                    }
                    _=>return Err(StorageError::FactsConflict),
                };
                self.inner.read.check(s,&identity)?;
                tx.commit().await?;
                capture.check_live()?;
                self.inner.public_target=Some((target,version));
                self.inner.public_request=Some(context);
                self.inner.prepare_technical(s,&scope.host,nonce,Preparation::Change(r)).await
            }
        }
    }
    /// Separate explicit confirmation. Source SHARE and host locks survive the
    /// confirmation service await; the original full argument digest is used.
    pub async fn confirm(&self,s:&SessionRef,task:&TaskId,reference:&ConfirmationRef)->Result<(),StorageError>{
        let original=self.inner.confirmations.read(s,task,reference).await?;
        let identity=self.inner.read.identity(s)?;
        let mut tx=self.inner.store.repository.pool.begin().await?;
        lock_task(&mut tx,&identity.user_id,task).await?;
        domain_audit::writer_lock(&mut tx).await?;
        check_live(&self.inner,self.scope()?,s,&mut tx).await?;
        verify_evidence(&mut tx,&identity.user_id,&original.binding.operation_id,Some(self.scope()?)).await?;
        let (input,member,_)=load_binding(&mut tx,&identity,s,&original.binding.operation_id).await?;
        check_prepared_version(&mut tx,&identity.user_id,&input).await?;
        MemberDeadline::from_binding(&input,&member)?.check(chrono::Utc::now().timestamp_millis())?;
        self.inner.confirmations.confirm_on_connection(&mut tx,s,task,reference,&original.binding).await?;
        check_live(&self.inner,self.scope()?,s,&mut tx).await?;
        tx.commit().await?;
        self.inner.read.check(s,&identity)
    }
    pub async fn execute(&self,s:&SessionRef,task:&TaskId,op:&OperationId,reference:&ConfirmationRef)->Result<Value,StorageError>{
        // The original execute path itself reads terminal results without
        // reusing confirmation or touching member business.
        self.inner.execute_technical(s,&self.scope()?.host,task,op,reference).await?;
        self.query(s,op).await
    }
    /// Owner-filtered original selector recovery only; never restores confirmation.
    pub async fn query_nonce(&self,s:&SessionRef,n:&str)->Result<Value,StorageError>{
        let id=uuid::Uuid::parse_str(n).map_err(|_|StorageError::Invalid("nonce"))?;
        if id.to_string()!=n{return Err(StorageError::Invalid("nonce"));}
        let identity=self.inner.read.identity(s)?;
        let op:String=sqlx::query_scalar("SELECT b.operation_id FROM subscription_preparations_v1 p JOIN subscription_public_bindings_v1 b USING(business_ref) WHERE p.nonce=$1 AND b.owner_id=$2").bind(n).bind(identity.user_id.as_str()).fetch_optional(&self.inner.store.repository.subscription_read_pool).await?.ok_or(StorageError::NotFound)?;
        let op=OperationId::parse(op).map_err(|_|StorageError::CorruptRecord)?;
        Ok(json!({"operation_id":op,"result":self.query(s,&op).await?,"can_continue":false}))
    }
    pub async fn query(&self,s:&SessionRef,op:&OperationId)->Result<Value,StorageError>{
        let identity=self.inner.read.identity(s)?;
        if identity.user_id.as_str()!=self.scope()?.source.owner(){return Err(StorageError::NotFound);}
        let mut tx=self.inner.store.repository.subscription_read_pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await?;
        let body=verify_evidence(&mut tx,&identity.user_id,op,None).await?;
        let original_value=sqlx::query_scalar::<_,Value>("SELECT original_request FROM subscription_public_bindings_v1 WHERE owner_id=$1 AND operation_id=$2").bind(identity.user_id.as_str()).bind(op.as_str()).fetch_one(&mut *tx).await?;
        let original=serde_json::from_value(original_value.clone()).map_err(|_|StorageError::CorruptRecord)?;
        let snap=self.inner.read.load(&mut tx,identity.user_id.as_str()).await?;
        let audit=match snap.original_result_mock(&original).map_err(|_|StorageError::CorruptRecord)?{
            OriginalResultMock::NeedsReview=>{
                let succeeded:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM operations WHERE operation_id=$1 AND state='SUCCEEDED')").bind(op.as_str()).fetch_one(&mut *tx).await?;
                if succeeded{return Err(StorageError::ReconcileRequired);}
                json!({"status":"UNEXECUTED_OR_NEEDS_REVIEW"})
            },
            OriginalResultMock::Recorded{..}=>domain_audit::verify_link(&mut tx,&identity.user_id,op,&self.inner.read).await?,
        };
        let view=json!(snap.original_view_mock(&original).map_err(|_|StorageError::CorruptRecord)?);
        // Absence of a receipt alone is NOT a retry authority. A validated
        // member journal must positively classify this exact preparation as
        // non-executable, and the public transaction must never have submitted.
        // This releases only the browser's current selection, not the old task.
        let mut recovery=json!({"version":1,"operation_id":op,"may_start_new":false});
        if audit["status"]=="UNEXECUTED_OR_NEEDS_REVIEW" {
            let pending=snap.pending_confirmations_mock(&chrono::Utc::now().to_rfc3339()).map_err(|_|StorageError::CorruptRecord)?;
            let matches:Vec<_>=pending.items.iter().filter(|p|json!(p.original_context)==original_value["original_binding"]["context"]).collect();
            let submitted:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM public_execution_requests WHERE operation_id=$1) OR EXISTS(SELECT 1 FROM operations WHERE operation_id=$1 AND (business_operation_ref IS NOT NULL OR state IN ('SUBMITTED','SUCCEEDED','RESULT_UNKNOWN')))").bind(op.as_str()).fetch_one(&mut *tx).await?;
            let generation:i64=sqlx::query_scalar("SELECT generation FROM subscription_host_generations_v1 WHERE owner_id=$1").bind(identity.user_id.as_str()).fetch_one(&mut *tx).await?;
            let prior=body["host_generation"].as_i64().ok_or(StorageError::CorruptRecord)?;
            // Restart permanently invalidates that member binding; an elapsed
            // clock or a missing result does not establish this proof.
            if matches.len()==1 && !submitted && matches[0].state=="INVALIDATED" && prior>0 && generation>prior {
                recovery=json!({"version":1,"operation_id":op,"may_start_new":true,"state":"NON_EXECUTABLE_PREPARATION","reason":"RESTART_INVALIDATED","original_writer_generation":prior,"current_writer_generation":generation,"original_preserved":true});
            }
        }
        self.inner.read.check(s,&identity)?;
        tx.commit().await?;
        self.inner.read.check(s,&identity)?;
        self.read_authorized(s).await?;
        Ok(json!({"original":view,"domain_audit":audit,"confirmed_arrangement":body["public_context"]["summary"],"preparation_recovery":recovery}))
    }
    pub async fn preparation_view(&self,s:&SessionRef,p:&SubscriptionPreparation)->Result<Value,StorageError>{
        let identity=self.inner.read.identity(s)?;
        let mut tx=self.inner.store.repository.subscription_read_pool.begin().await?;
        let body=verify_evidence(&mut tx,&identity.user_id,&p.operation.operation_id,None).await?;
        tx.commit().await?;
        self.inner.read.check(s,&identity)?;
        self.read_authorized(s).await?;
        Ok(json!({"task_id":p.task.task_id,"operation_id":p.operation.operation_id,"confirmation_ref":p.confirmation.confirmation_ref,
            "expires_at":p.confirmation.expires_at,"summary":body["public_context"]["summary"],"requires_explicit_confirmation":true,"business_executed":false,"mode":"PROJECT_SIMULATION"}))
    }
}
