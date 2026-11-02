//! Persistent corruption, actual lock waits and restricted SQL bypass attempts.
//! Corruption uses ONLY the owned test administrator, never a product bypass.
use super::*;

async fn damage(v:&Setup, q:&str) {
    sqlx::raw_sql(&format!("SET session_replication_role=replica; {q}; SET session_replication_role=origin"))
        .execute(&v.db.scoped).await.unwrap();
}
async fn begin(v:&Setup,pool:&sqlx::PgPool,gid:&str,epoch:i64)->Result<sqlx::Transaction<'static,sqlx::Postgres>,sqlx::Error> {
    let mut tx=pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED").execute(&mut *tx).await?;
    sqlx::query("SELECT set_config('search_path',$1,true)").bind(&v.db.namespace).execute(&mut *tx).await?;
    sqlx::query("SELECT payment_completion_begin_v1($1,$2)").bind(gid).bind(epoch).execute(&mut *tx).await?;
    Ok(tx)
}
async fn expired_lease(v:&Setup) {
    sqlx::query("UPDATE payment_completion_candidate_jobs SET lease_epoch=lease_epoch+1,leased_at=clock_timestamp()-interval '1 minute',lease_until=clock_timestamp()-interval '30 seconds'").execute(&v.db.scoped).await.unwrap();
}
async fn close(v:Setup,w:PaymentBackgroundActor,wp:sqlx::PgPool,p:PaymentBackgroundActor,pp:sqlx::PgPool) {
    w.close().await;wp.close().await;p.close().await;pp.close().await;v.server.stop();v.db.finish().await;
}

#[tokio::test]
#[ignore="owned PG: old confirmation expiry/logout must not revoke independent delegation"]
async fn background_final_logout_expired_confirmation_and_old_entry_denied() {
    let (v,gid)=accepted().await;
    let (p,pp,_)=actor(&v,PaymentBackgroundRole::Producer).await;
    let (w,wp,_)=enabled_worker(&v).await;
    // Exercise the actual old native resolver, not a nonexistent HTTP route.
    let service=v.server.state.payment_background_for_backend().unwrap();
    let s=session(&v.cookie);let task=task(&v.p);let prep=prep(&v.p);
    let before=snapshot(&v).await;
    let consumed_before:i64=sqlx::query_scalar("SELECT count(*) FROM execution_permits WHERE status='CONSUMED'").fetch_one(&v.db.scoped).await.unwrap();
    assert_eq!(service.resolve_simulated_success(&s,&task,&prep).await.err().unwrap(),safeflow_storage::StorageError::FormalAuthUnavailable);
    assert_eq!(before,snapshot(&v).await);
    v.server.state.0.sessions.revoke(&session(&v.cookie)).unwrap();
    sqlx::raw_sql("CREATE OR REPLACE FUNCTION payment_background_clock_v1() RETURNS TIMESTAMPTZ LANGUAGE sql VOLATILE AS $$ SELECT first_submitted_at+interval '6 minutes' FROM payment_completion_candidate_grants $$").execute(&v.db.scoped).await.unwrap();
    let expired:bool=sqlx::query_scalar("SELECT payment_background_clock_v1()>confirmed_until AND payment_background_clock_v1()>auth_until FROM payment_background_consents_v1").fetch_one(&v.db.scoped).await.unwrap();assert!(expired);
    p.record_explicit_decision(&gid,"after-logout-and-confirmation-expiry",PaymentSimulationDecision::SettleLocalSimulation).await.unwrap();
    let (r,old_result)=tokio::join!(w.complete_original(&gid),service.resolve_simulated_success(&s,&task,&prep));
    assert_eq!(r.unwrap()["state"],"COMPLETED");assert!(old_result.is_err());
    let count:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM transfer_ledger_v1),(SELECT count(*) FROM payment_completion_candidate_receipts),(SELECT count(*) FROM execution_permits WHERE status='CONSUMED')").fetch_one(&v.db.scoped).await.unwrap();
    assert_eq!(count.0,2);assert_eq!(count.1,1);assert_eq!(count.2,consumed_before);
    close(v,w,wp,p,pp).await;
}

#[tokio::test]
#[ignore="owned upgrade runner only: real parent35 schema migration via new binary"]
async fn background_final_upgrade_child() {
    assert_eq!(std::env::var("SAFEFLOW_FINAL_UPGRADE").unwrap(),"OWNED_CLUSTER_ONLY");
    let url=std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
    let ns=std::env::var("SAFEFLOW_FINAL_UPGRADE_NAMESPACE").unwrap();
    let repo=PgRepository::connect_existing(&url,&ns).await.unwrap();
    let result=repo.migrate().await;
    println!("FINAL_UPGRADE_REPLY:{}",json!({"migrated":result.is_ok(),"error":result.err().map(|e|format!("{e:?}"))}));
    repo.close().await;
}

#[tokio::test]
#[ignore="owned PG: persistent damage must reject without new business or terminal writes"]
async fn background_final_persisted_original_decision_and_scope_conflicts() {
    for (mode,q) in [
        ("original-owner","UPDATE public_execution_requests SET owner_id='user_002'"),
        ("original-task","UPDATE public_execution_requests SET task_id='task_different_original'"),
        ("original-business","UPDATE public_execution_requests SET business_operation_ref='business_different_original'"),
        ("original-key","UPDATE public_execution_requests SET idempotency_key='damaged-original-key'"),
        ("request-payload","UPDATE public_execution_requests SET fingerprint_payload=jsonb_set(fingerprint_payload,'{normalized_arguments,amount}',to_jsonb('999.00'::text))"),
        ("association","UPDATE payment_funds_execution_v1 SET association_digest=repeat('0',64)"),
        ("original-preparation","UPDATE payment_funds_execution_v1 SET preparation_operation_id='payment_different_original'"),
        ("snapshot","UPDATE payment_preparations SET snapshot_digest=repeat('0',64)"),
        ("consumed-permit","UPDATE execution_permits SET arguments_hash=repeat('0',64) WHERE execution_scope='TRANSFER'"),
        ("permit-confirmation","UPDATE execution_permits SET confirmation_ref='confirmation_different_original' WHERE execution_scope='TRANSFER'"),
        ("grant-digest","UPDATE payment_completion_candidate_grants SET evidence_digest=repeat('0',64)"),
        ("decision-hash","UPDATE payment_completion_candidate_decisions SET envelope_digest=repeat('0',64)"),
        ("decision-result","UPDATE payment_completion_candidate_decisions SET result_id='different-result'"),
        ("decision-outcome","UPDATE payment_completion_candidate_decisions SET outcome='DEFINITE_SIMULATED_DECLINE'"),
        ("decision-sequence","UPDATE payment_completion_candidate_decisions SET envelope=jsonb_set(envelope,'{decision_seq}','2'::jsonb),envelope_digest=encode(sha256(convert_to(jsonb_set(envelope,'{decision_seq}','2'::jsonb)::text,'UTF8')),'hex')"),
        ("decision-body","UPDATE payment_completion_candidate_decisions SET envelope=jsonb_set(envelope,'{mode}',to_jsonb('EXTERNAL_BANK_SUCCESS'::text)),envelope_digest=encode(sha256(convert_to(jsonb_set(envelope,'{mode}',to_jsonb('EXTERNAL_BANK_SUCCESS'::text))::text,'UTF8')),'hex')"),
        ("producer-epoch","UPDATE payment_completion_candidate_fences SET epoch=epoch+1 WHERE kind='PRODUCER'"),
        ("worker-scope","UPDATE payment_completion_candidate_fences SET scope='{}',epoch=epoch+1 WHERE kind='WORKER'"),
        ("catalog-adoption","UPDATE public_card_catalog_adoptions_v1 SET source_revision='different-adoption'"),
        ("recipient","UPDATE transfer_business_orders_v1 SET request=jsonb_set(request,'{amount}',to_jsonb('999.00'::text))"),
    ] {
        let (v,gid)=accepted().await;
        let (p,pp,_)=actor(&v,PaymentBackgroundRole::Producer).await;
        let (w,wp,_)=enabled_worker(&v).await;
        p.record_explicit_decision(&gid,"binding-original",PaymentSimulationDecision::SettleLocalSimulation).await.unwrap();
        damage(&v,q).await;let before=snapshot(&v).await;
        assert!(w.complete_original(&gid).await.is_err(),"accepted damaged {mode}");
        assert_pending(&v).await;assert_eq!(before,snapshot(&v).await,"{mode}");
        let contexts:i64=sqlx::query_scalar("SELECT count(*) FROM payment_completion_transactions_v1").fetch_one(&v.db.scoped).await.unwrap();assert_eq!(contexts,0);
        println!("persistent conflict rejected with original hold unchanged: {mode}");
        close(v,w,wp,p,pp).await;
    }
}

#[tokio::test]
#[ignore="owned PG: missing terminal evidence must reconcile, never synthesize or repeat settlement"]
async fn background_final_terminal_missing_side_never_repairs_or_reexecutes() {
    for (mode,q) in [
        ("receipt","DELETE FROM payment_completion_candidate_receipts"),
        ("ledger","DELETE FROM transfer_ledger_v1 WHERE direction='CREDIT'"),
        ("event","DELETE FROM audit_events WHERE details->>'event'='BACKGROUND_FINAL_RESULT'"),
        ("outbox","DELETE FROM public_audit_outbox WHERE envelope->'public_event'->'details'->>'event'='BACKGROUND_FINAL_RESULT'"),
        ("balance","UPDATE transfer_accounts_v1 SET booked_cents=booked_cents+1 WHERE account_id='account_001'"),
    ] {
        let (v,gid)=accepted().await;
        let (p,pp,_)=actor(&v,PaymentBackgroundRole::Producer).await;let (w,wp,_)=enabled_worker(&v).await;
        p.record_explicit_decision(&gid,"terminal-original",PaymentSimulationDecision::SettleLocalSimulation).await.unwrap();
        assert_eq!(w.complete_original(&gid).await.unwrap()["state"],"COMPLETED");
        damage(&v,q).await;let before=super::classification::persistent_snapshot(&v).await;
        assert_eq!(w.read_completed_original(&gid).await.unwrap_err(),safeflow_storage::StorageError::ReconcileRequired,"read {mode}");
        assert_eq!(w.complete_original(&gid).await.unwrap_err(),safeflow_storage::StorageError::ReconcileRequired,"complete {mode}");
        assert_eq!(before,super::classification::persistent_snapshot(&v).await);
        println!("terminal damage read/complete ReconcileRequired, all persisted tables unchanged: {mode}");close(v,w,wp,p,pp).await;
    }
}

#[tokio::test]
#[ignore="owned PG: restricted direct SQL cannot commit a half-chain, stale lease or swallowed error"]
async fn background_final_closed_sql_partial_cancel_and_stale_lease() {
    let (v,gid)=accepted().await;
    let (p,pp,_)=actor(&v,PaymentBackgroundRole::Producer).await;let (w,wp,_)=enabled_worker(&v).await;
    p.record_explicit_decision(&gid,"sql-original",PaymentSimulationDecision::SettleLocalSimulation).await.unwrap();
    let before=snapshot(&v).await;
    let claim=w.inspect_and_claim_original(&gid).await.unwrap();let lease=claim["lease"]["lease_epoch"].as_i64().unwrap();
    assert!(begin(&v,&wp,&gid,lease+1).await.is_err());
    let tx=begin(&v,&wp,&gid,lease).await.unwrap();assert!(tx.commit().await.is_err());assert_eq!(before,snapshot(&v).await);
    let mut tx=begin(&v,&wp,&gid,lease).await.unwrap();
    let e=sqlx::query("SELECT payment_completion_finish_v1($1,$2)").bind(&gid).bind(lease).execute(&mut *tx).await;assert!(e.is_err());
    // PostgreSQL may return ROLLBACK as an OK COMMIT response after a SQL error;
    // regardless of driver return, no half-chain is allowed to persist.
    let _=tx.commit().await;assert_eq!(before,snapshot(&v).await);
    let mut tx=begin(&v,&wp,&gid,lease).await.unwrap();
    let pid:i32=sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&mut *tx).await.unwrap();
    let sleep=sqlx::query("SELECT pg_sleep(30)").execute(&mut *tx);
    let (_,cancelled)=tokio::join!(async {
        let until=std::time::Instant::now()+Duration::from_secs(5);
        loop {let wait:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND wait_event='PgSleep')").bind(pid).fetch_one(&v.db.admin).await.unwrap();if wait{break}assert!(std::time::Instant::now()<until);tokio::task::yield_now().await;}
        let yes:bool=sqlx::query_scalar("SELECT pg_cancel_backend($1)").bind(pid).fetch_one(&v.db.admin).await.unwrap();assert!(yes);
    },sleep);assert!(cancelled.is_err());let _=tx.commit().await;
    assert_eq!(before,snapshot(&v).await);assert_pending(&v).await;
    expired_lease(&v).await;assert_eq!(w.complete_original(&gid).await.unwrap()["state"],"COMPLETED");
    close(v,w,wp,p,pp).await;
}

#[tokio::test]
#[ignore="owned PG: simultaneous independently authenticated workers and same-original replay"]
async fn background_final_two_workers_one_effect() {
    let (v,gid)=accepted().await;let (p,pp,_)=actor(&v,PaymentBackgroundRole::Producer).await;
    let (a,ap,_)=enabled_worker(&v).await;let (b,bp,_)=enabled_worker(&v).await;
    p.record_explicit_decision(&gid,"concurrent-original",PaymentSimulationDecision::SettleLocalSimulation).await.unwrap();
    let service=v.server.state.payment_background_for_backend().unwrap();
    let s=session(&v.cookie);let t=task(&v.p);let op=prep(&v.p);
    let (ra,rb,old)=tokio::join!(a.complete_original(&gid),b.complete_original(&gid),service.resolve_simulated_success(&s,&t,&op));
    assert_eq!(old.err().unwrap(),safeflow_storage::StorageError::FormalAuthUnavailable);
    assert!(ra.as_ref().is_ok_and(|v|v["state"]=="COMPLETED")||rb.as_ref().is_ok_and(|v|v["state"]=="COMPLETED"));
    let r=a.read_completed_original(&gid).await.unwrap().unwrap();assert_eq!(Some(r.clone()),b.read_completed_original(&gid).await.unwrap());
    let count:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM transfer_ledger_v1),(SELECT count(*) FROM payment_completion_candidate_receipts),(SELECT count(*) FROM audit_events WHERE details->>'event'='BACKGROUND_FINAL_RESULT')").fetch_one(&v.db.scoped).await.unwrap();assert_eq!(count,(2,1,1));
    let after=snapshot(&v).await;assert_eq!(r,b.complete_original(&gid).await.unwrap());assert_eq!(after,snapshot(&v).await);
    b.close().await;bp.close().await;close(v,a,ap,p,pp).await;
}

#[tokio::test]
#[ignore="owned PG: real final writes precede revoke/freeze; later writers cannot change terminal"]
async fn background_final_completion_first_then_revocation_or_freeze() {
    for mode in ["control","worker","producer","account","source","card"] {
        let (v,gid)=accepted().await;let (p,pp,pr)=actor(&v,PaymentBackgroundRole::Producer).await;let (w,wp,wr)=enabled_worker(&v).await;
        p.record_explicit_decision(&gid,"completion-first",PaymentSimulationDecision::SettleLocalSimulation).await.unwrap();
        let key:i64=527018553;
        sqlx::raw_sql(&format!("CREATE FUNCTION step08_wait_write() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$ BEGIN PERFORM pg_advisory_xact_lock({key}); RETURN NEW; END $$; CREATE TRIGGER step08_wait_write BEFORE UPDATE ON transfer_fund_orders_v1 FOR EACH ROW EXECUTE FUNCTION step08_wait_write()")).execute(&v.db.scoped).await.unwrap();
        let mut holder=v.db.scoped.begin().await.unwrap();let holder_pid:i32=sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&mut *holder).await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock($1)").bind(key).execute(&mut *holder).await.unwrap();
        let q=match mode {
            "control"=>"UPDATE payment_completion_candidate_controls SET revoked=true,control_epoch=control_epoch+1".into(),
            "worker"|"producer"=>format!("UPDATE payment_completion_candidate_fences SET epoch=epoch+1,enabled=false WHERE authority_id='{}'",if mode=="worker"{&wr}else{&pr}),
            "account"=>"UPDATE card_safety_account_fences_v1 SET epoch=epoch+1,revoked=true WHERE account_id='account_001'".into(),
            "source"=>"UPDATE public_unified_fictional_source_v1 SET active=false".into(),
            _=>"SELECT card_save_freeze_v1('completion_first_lock','demo_card_salary_v1','USER_SELF',NULL,NULL,true,'completion_first_freeze','user_001','account_001','completion_first_op','USER_REPORT_LOST',NULL,NULL,'completion_first_key','completion_first_reason','CREATED')".into(),
        };
        let observer=async {
            blocked(&v.db,holder_pid).await;
            let worker_pid:i32=sqlx::query_scalar("SELECT pid FROM pg_stat_activity WHERE usename=$1 AND $2=ANY(pg_blocking_pids(pid))").bind(&wr).bind(holder_pid).fetch_one(&v.db.admin).await.unwrap();
            let mut later=v.db.scoped.begin().await.unwrap();
            let mutation=sqlx::query(&q).execute(&mut *later);
            let (_,r)=tokio::join!(async {blocked(&v.db,worker_pid).await;holder.commit().await.unwrap();},mutation);r.unwrap();later.commit().await.unwrap();
        };
        let (completed,())=tokio::join!(w.complete_original(&gid),observer);
        assert_eq!(completed.unwrap()["state"],"COMPLETED");
        let result:(String,i64,i64)=sqlx::query_as("SELECT (SELECT o.state FROM operations o JOIN payment_completion_candidate_grants g ON g.execution_operation_id=o.operation_id),(SELECT count(*) FROM transfer_ledger_v1),(SELECT count(*) FROM payment_completion_candidate_receipts)").fetch_one(&v.db.scoped).await.unwrap();assert_eq!(result,("SUCCEEDED".into(),2,1));
        if mode=="card" {assert_eq!(w.read_completed_original(&gid).await.unwrap().unwrap()["state"],"COMPLETED");}
        println!("actual final write first, later revocation/freeze waited until durable terminal: {mode}");close(v,w,wp,p,pp).await;
    }
}

#[tokio::test]
#[ignore="owned PG: actual lock waits, current authority and deadline after waiting"]
async fn background_final_waits_then_rechecks_roots_card_and_time() {
    for mode in ["control","producer","source","card","deadline","lease"] {
        let (v,gid)=accepted().await;let (p,pp,pr)=actor(&v,PaymentBackgroundRole::Producer).await;let (w,wp,_)=enabled_worker(&v).await;
        p.record_explicit_decision(&gid,"wait-original",PaymentSimulationDecision::SettleLocalSimulation).await.unwrap();
        let claim=w.inspect_and_claim_original(&gid).await.unwrap();let lease=claim["lease"]["lease_epoch"].as_i64().unwrap();
        let before=snapshot(&v).await;
        // Wait after scheduler claim; exercise the final transaction itself.
        let mut tx=v.db.scoped.begin().await.unwrap();let pid:i32=sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&mut *tx).await.unwrap();
        let q=match mode {
            "control"=>"UPDATE payment_completion_candidate_controls SET revoked=true,control_epoch=control_epoch+1".to_owned(),
            "producer"=>format!("UPDATE payment_completion_candidate_fences SET enabled=false,epoch=epoch+1 WHERE kind='PRODUCER' AND authority_id='{pr}'"),
            "source"=>"UPDATE public_unified_fictional_source_v1 SET active=false".into(),
            // The actual card saver locks the payer account protection and
            // both the old claim and new completion use the same card facts.
            "card"=>"SELECT card_save_freeze_v1('wait_lock','demo_card_salary_v1','USER_SELF',NULL,NULL,true,'wait_freeze','user_001','account_001','wait_op','USER_REPORT_LOST',NULL,NULL,'wait_key','wait_reason','CREATED')".into(),
            _=>"SELECT account_id FROM transfer_accounts_v1 WHERE account_id='account_001' FOR UPDATE".into(),
        };
        sqlx::query(&q).execute(&mut *tx).await.unwrap();
        let future=begin(&v,&wp,&gid,lease);
        let (_,r)=tokio::join!(async {
            blocked(&v.db,pid).await;
            if mode=="deadline"||mode=="lease" {
                let value=if mode=="deadline" {"completion_until FROM payment_completion_candidate_grants"}else{"lease_until FROM payment_completion_candidate_jobs"};
                sqlx::raw_sql(&format!("CREATE OR REPLACE FUNCTION payment_background_clock_v1() RETURNS TIMESTAMPTZ LANGUAGE sql VOLATILE AS $$ SELECT {value} $$")).execute(&mut *tx).await.unwrap();
            }
            tx.commit().await.unwrap();
        },future);
        if mode=="card" {
            // Begin may admit; outgoing/final guard must reject the frozen
            // original. Even a direct partial caller cannot commit.
            if let Ok(mut tx)=r {let allowed:bool=sqlx::query_scalar("SELECT payment_completion_outgoing_v1($1)").bind(&gid).fetch_one(&mut *tx).await.unwrap();assert!(!allowed);assert!(tx.commit().await.is_err());}
        }else{assert!(r.is_err(),"{mode}");}
        assert_pending(&v).await;assert_eq!(before,snapshot(&v).await,"{mode}");
        println!("final completion real wait then rejection: {mode}");close(v,w,wp,p,pp).await;
    }
}
