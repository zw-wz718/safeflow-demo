//! Native local persistence is the simulated channel action, not external delivery.
//! A joined scoped thread owns its own runtime AND fresh connections. It never
//! waits on the host's advisory lock or borrows its pool/transaction. No queue,
//! detached task, nested block_on on Tokio's worker, or serialized batch exists.
use super::*;
use safeflow_subscription_evidence::confirmation_mock::{OriginalReminderRequestMock,ReminderReadQueryMock};
use sha2::{Digest,Sha256};
use sqlx::Row;
#[cfg(test)]
#[path="persistent_tests.rs"]
mod persistent_tests;

fn hash(v:&Value)->Result<String,StorageError>{
    Ok(format!("{:x}",Sha256::digest(serde_json::to_vec(v).map_err(|_|StorageError::CorruptRecord)?)))
}
pub(super) async fn reserve_attempts(host:&SubscriptionReminderHost,scope:&crate::SubscriptionPublicScope,c:&mut PgConnection,messages:&[ReminderMessage])->Result<Vec<String>,StorageError>{
    let saved=host.service.load(c,host.identity.user_id.as_str()).await?;
    let history=saved.reminder_history_mock().map_err(|_|StorageError::CorruptRecord)?;
    let last=history.last().ok_or(StorageError::CorruptRecord)?;
    if last.items.len()!=messages.len(){return Err(StorageError::FactsConflict);}
    let request=saved.original_reminder_request_mock(last.source_position).map_err(|_|StorageError::CorruptRecord)?;
    let original=serde_json::to_value(&request).map_err(|_|StorageError::CorruptRecord)?;
    let reserved=&original["prefix"]["commands"].as_array().ok_or(StorageError::CorruptRecord)?.last().ok_or(StorageError::CorruptRecord)?["messages"];
    let binding=json!({"scope":scope.reminder_binding(),"database":host.database_scope,"accounts":host.config.accounts,"session_version":host.identity.binding_version});
    let mut ids=Vec::new();
    for (ordinal,message) in messages.iter().enumerate(){
        let message=serde_json::to_value(message).map_err(|_|StorageError::CorruptRecord)?;
        if reserved.get(ordinal)!=Some(&message){return Err(StorageError::FactsConflict);}
        let id=uuid::Uuid::new_v4().to_string();
        sqlx::query("INSERT INTO subscription_reminder_attempts_v1(attempt_id,owner_id,source_position,ordinal,original_request,message,binding) VALUES($1::uuid,$2,$3,$4,$5,$6,$7)")
            .bind(&id).bind(host.identity.user_id.as_str()).bind(last.source_position as i64).bind(ordinal as i32).bind(&original).bind(message).bind(&binding).execute(&mut *c).await?;
        ids.push(id);
    }
    Ok(ids)
}

pub(super) struct PersistentSink<'a>{host:&'a SubscriptionReminderHost,stop:&'a SubscriptionReminderStop,deadline:Instant}
impl<'a> PersistentSink<'a>{pub fn new(host:&'a SubscriptionReminderHost,stop:&'a SubscriptionReminderStop)->Self{Self{host,stop,deadline:Instant::now()+host.config.scan_timeout}}}
impl MockInAppSink for PersistentSink<'_>{
    fn simulate_in_app(&mut self,message:&ReminderMessage)->DeliveryOutcome{
        let deadline=self.deadline;
        // Scoped join is deliberate: cancellation cannot abandon a queued write.
        // Channel's own connect/SQL/global timeouts expire before returning.
        let result=std::thread::scope(|threads|threads.spawn(||{
            tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|_|StorageError::DependencyUnavailable)?
                .block_on(async{tokio::time::timeout(deadline.saturating_duration_since(Instant::now()),self.persist(message,deadline)).await.map_err(|_|StorageError::ReconcileRequired)?})
        }).join().unwrap_or(Err(StorageError::ReconcileRequired)));
        match result{Ok(())=>DeliveryOutcome::SimulatedSuccess,Err(_)=>DeliveryOutcome::Unknown}
    }
}
impl PersistentSink<'_>{
    async fn persist(&self,message:&ReminderMessage,deadline:Instant)->Result<(),StorageError>{
        self.host.check(self.stop,deadline)?;
        let scope=self.host.public_scope.as_ref().ok_or(StorageError::DependencyUnavailable)?;
        let opts=self.host.service.repository.pool.connect_options();
        let mut c=PgConnection::connect_with(&opts).await?;
        sqlx::query(&format!("SET search_path TO \"{}\"",self.host.service.repository.namespace)).execute(&mut c).await?;
        sqlx::query("SET statement_timeout='2000ms'").execute(&mut c).await?;
        sqlx::query("SET lock_timeout='1500ms'").execute(&mut c).await?;
        if database_scope(&mut c).await?!=self.host.database_scope{return Err(StorageError::TransactionScope);}
        let pid:i32=sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&mut c).await?;
        let mut tx=c.begin().await?;
        self.host.check(self.stop,deadline)?;
        scope.check_reminder_database(&self.host.session,&mut tx).await?;
        self.host.check(self.stop,deadline)?;
        let value=serde_json::to_value(message).map_err(|_|StorageError::CorruptRecord)?;
        let binding=json!({"scope":scope.reminder_binding(),"database":self.host.database_scope,"accounts":self.host.config.accounts,"session_version":self.host.identity.binding_version});
        // The exact current ReserveScan prefix and ordinal are the authoritative
        // association, not an owner/subscription/kind/cycle guessed identifier.
        let snapshot=self.host.service.load(&mut tx,self.host.identity.user_id.as_str()).await?;
        let history=snapshot.reminder_history_mock().map_err(|_|StorageError::CorruptRecord)?;
        let last=history.last().ok_or(StorageError::CorruptRecord)?;
        let original=snapshot.original_reminder_request_mock(last.source_position).map_err(|_|StorageError::CorruptRecord)?;
        let original_value=serde_json::to_value(&original).map_err(|_|StorageError::CorruptRecord)?;
        let row=sqlx::query("SELECT attempt_id::text,original_request FROM subscription_reminder_attempts_v1 WHERE owner_id=$1 AND source_position=$2 AND message=$3 AND binding=$4")
            .bind(self.host.identity.user_id.as_str()).bind(last.source_position as i64).bind(&value).bind(&binding).fetch_all(&mut *tx).await?;
        if row.len()!=1 || row[0].try_get::<Value,_>("original_request")?!=original_value{return Err(StorageError::FactsConflict);}
        snapshot.original_reminder_result_mock(&original).map_err(|_|StorageError::FactsConflict)?;
        let id:String=row[0].try_get("attempt_id")?;
        let digest=hash(&value)?;
        self.host.check(self.stop,deadline)?;
        sqlx::query("INSERT INTO subscription_simulated_notifications_v1(attempt_id,message_sha256,receipt) VALUES($1::uuid,$2,jsonb_build_object('attempt_ref',$1::text,'message_sha256',$2::text,'channel','IN_APP_PERSISTENT_SIMULATION','notification_sent',false,'result','PERSISTED_SIMULATION','persisted_at',clock_timestamp())) ON CONFLICT(attempt_id) DO NOTHING")
            .bind(&id).bind(&digest).execute(&mut *tx).await?;
        let receipt:Value=sqlx::query_scalar("SELECT receipt FROM subscription_simulated_notifications_v1 WHERE attempt_id=$1::uuid AND message_sha256=$2").bind(&id).bind(&digest).fetch_optional(&mut *tx).await?.ok_or(StorageError::FactsConflict)?;
        self.host.check(self.stop,deadline)?;
        let committed=tx.commit().await.is_ok();
        // Distinct backend observation; it grants no reconstructed send token.
        let mut reader=PgConnection::connect_with(&opts).await?;
        sqlx::query(&format!("SET search_path TO \"{}\"",self.host.service.repository.namespace)).execute(&mut reader).await?;
        let other:i32=sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&mut reader).await?;
        if pid==other || database_scope(&mut reader).await?!=self.host.database_scope{return Err(StorageError::TransactionScope);}
        let observed:Option<Value>=sqlx::query_scalar("SELECT receipt FROM subscription_simulated_notifications_v1 WHERE attempt_id=$1::uuid AND message_sha256=$2").bind(&id).bind(&digest).fetch_optional(&mut reader).await?;
        self.host.check(self.stop,deadline)?;
        if !committed || observed.as_ref()!=Some(&receipt){return Err(StorageError::ReconcileRequired);}
        Ok(())
    }
}

impl SubscriptionService{
    pub async fn persistent_reminder_host_for_backend(&self,session:&SessionRef,scope:&crate::SubscriptionPublicScope,bank:Arc<dyn BankGateway>,config:SubscriptionReminderConfig)->Result<SubscriptionReminderHost,StorageError>{
        let accounts:Vec<_>=config.accounts.iter().map(|a|a.as_str().to_owned()).collect();
        if accounts!=scope.reminder_accounts(){return Err(StorageError::TransactionScope);}
        let mut host=self.reminder_host_for_backend(session,scope.reminder_host(),bank,config).await?;
        let mut c=self.repository.pool.acquire().await?;
        scope.check_reminder_database(session,&mut c).await?;
        host.public_scope=Some(scope.clone());Ok(host)
    }
    /// Safe current-owner view. No raw message, source, journal or saved prefix.
    /// Does not reserve, resolve, retry or resurrect any member capability.
    pub async fn persistent_reminder_view_for_backend(&self,session:&SessionRef,scope:&crate::SubscriptionPublicScope,bank:&dyn BankGateway,attempt:Option<&str>)->Result<Value,StorageError>{
        let identity=self.identity(session)?;
        scope.authorize_reminder_read(session,bank).await?;
        let id=attempt.map(|a|uuid::Uuid::parse_str(a).map(|x|x.to_string()).map_err(|_|StorageError::NotFound)).transpose()?;
        let mut tx=self.repository.subscription_read_pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY").execute(&mut *tx).await?;
        let rows=sqlx::query("SELECT a.attempt_id::text,a.ordinal,a.original_request,a.message,n.receipt,n.message_sha256 FROM subscription_reminder_attempts_v1 a LEFT JOIN subscription_simulated_notifications_v1 n USING(attempt_id) WHERE a.owner_id=$1 AND ($2::text IS NULL OR a.attempt_id::text=$2) ORDER BY a.reserved_at,a.ordinal LIMIT 128")
            .bind(identity.user_id.as_str()).bind(&id).fetch_all(&mut *tx).await?;
        if id.is_some() && rows.is_empty(){return Err(StorageError::NotFound);}
        let saved=self.load(&mut tx,identity.user_id.as_str()).await?;
        let mut items=Vec::new();
        for row in rows{
            let original:OriginalReminderRequestMock=serde_json::from_value(row.try_get("original_request")?).map_err(|_|StorageError::CorruptRecord)?;
            let view=saved.reminder_read_view_mock(&ReminderReadQueryMock::OriginalReminder{request:original}).map_err(|_|StorageError::CorruptRecord)?;
            let mut view=serde_json::to_value(view).map_err(|_|StorageError::CorruptRecord)?;
            let ordinal=usize::try_from(row.try_get::<i32,_>("ordinal")?).map_err(|_|StorageError::CorruptRecord)?;
            let item=view["items"].get(ordinal).cloned().ok_or(StorageError::CorruptRecord)?;
            view["items"]=json!([item]);
            let receipt:Option<Value>=row.try_get("receipt")?;
            if let Some(ref receipt)=receipt{
                let expected=hash(&row.try_get::<Value,_>("message")?)?;
                if receipt["attempt_ref"]!=row.try_get::<String,_>("attempt_id")? || receipt["message_sha256"]!=expected || row.try_get::<Option<String>,_>("message_sha256")?.as_deref()!=Some(&expected){return Err(StorageError::CorruptRecord);}
            }
            items.push(json!({"attempt_ref":row.try_get::<String,_>("attempt_id")?,"original":view,"channel_state":if receipt.is_some(){"PERSISTED_SIMULATION"}else{"NOT_OBSERVED"},"persisted_at":receipt.as_ref().and_then(|r|r.get("persisted_at")),"notification_sent":false}));
        }
        self.check(session,&identity)?;tx.commit().await?;
        scope.authorize_reminder_read(session,bank).await?;
        Ok(json!({"mode":"PROJECT_SIMULATION","notification_sent":false,"items":items}))
    }
}
