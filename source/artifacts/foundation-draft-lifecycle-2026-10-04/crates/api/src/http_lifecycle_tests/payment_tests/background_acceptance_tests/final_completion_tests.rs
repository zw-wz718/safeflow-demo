//! Actual SCRAM worker completion against an owned PostgreSQL, original fixture.
use super::*;
#[path="final_completion_boundaries.rs"]
mod boundaries;
#[path="final_completion_classification.rs"]
mod classification;

async fn accepted() -> (Setup, String) {
    let v=setup().await;
    let display=authorize(&v).await;
    let r=ok(command_bg(&v,execution(&v,&display)).await);
    assert_eq!(r["operation"]["state"],"PROCESSING");
    let gid=grant(&v).await;
    (v,gid)
}
async fn enabled_worker(v:&Setup)->(PaymentBackgroundActor,sqlx::PgPool,String) {
    let (a,p,n)=actor(v,PaymentBackgroundRole::Worker).await;
    PaymentBackgroundDeployment::for_isolated_backend(v.source.clone()).enable_final_completion_for_login(&n).await.unwrap();
    (a,p,n)
}
async fn assert_pending(v:&Setup) {
    let r:(String,String,String,i64,i64)=sqlx::query_as("SELECT o.state,f.state,b.latest_receipt->>'status',(SELECT count(*) FROM transfer_ledger_v1),(SELECT count(*) FROM payment_completion_candidate_receipts) FROM operations o JOIN payment_completion_candidate_grants g ON g.execution_operation_id=o.operation_id JOIN transfer_fund_orders_v1 f ON f.owner_id=g.owner_id AND f.operation_id=g.business_operation_ref JOIN transfer_business_orders_v1 b ON b.owner_id=f.owner_id AND b.operation_id=f.operation_id")
        .fetch_one(&v.db.scoped).await.unwrap();
    assert_eq!(r,("PROCESSING".into(),"PROCESSING".into(),"PROCESSING".into(),0,0));
}

#[tokio::test]
#[ignore="owned PG16 SCRAM and original unified fictional source"]
async fn background_final_actual_worker_success_and_idempotent_history() {
    let (v,gid)=accepted().await;
    let (producer,pp,_)=actor(&v,PaymentBackgroundRole::Producer).await;
    let (worker,wp,role)=enabled_worker(&v).await;
    let before=snapshot(&v).await;
    assert_eq!(worker.complete_original(&gid).await.unwrap()["state"],"WAITING_TRUSTED_DECISION");
    assert_eq!(before,snapshot(&v).await);
    producer.record_explicit_decision(&gid,"result-original",PaymentSimulationDecision::SettleLocalSimulation).await.unwrap();
    // Writer-on grants must not permit ANY unscoped fund or private read.
    let count:i64=sqlx::query_scalar(&format!("SELECT count(*) FROM \"{}\".transfer_accounts_v1",v.db.namespace)).fetch_one(&wp).await.unwrap();
    assert_eq!(count,0);
    let n=sqlx::query(&format!("UPDATE \"{}\".transfer_accounts_v1 SET booked_cents=0",v.db.namespace)).execute(&wp).await.unwrap().rows_affected();
    assert_eq!(n,0);
    let r=worker.complete_original(&gid).await.unwrap();
    assert_eq!(r["state"],"COMPLETED");
    assert_eq!(r["receipt"]["evidence"]["public_operation"]["state"],"SUCCEEDED");
    assert_eq!(r["receipt"]["evidence"]["business"]["first_receipt"]["status"],"PROCESSING");
    assert_eq!(r["receipt"]["evidence"]["ledger"].as_array().unwrap().len(),2);
    let counts:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM payment_completion_candidate_receipts),(SELECT count(*) FROM transfer_ledger_v1),(SELECT count(*) FROM audit_events WHERE details->>'event'='BACKGROUND_FINAL_RESULT')").fetch_one(&v.db.scoped).await.unwrap();
    assert_eq!(counts,(1,2,1));
    let after=snapshot(&v).await;
    assert_eq!(r,worker.complete_original(&gid).await.unwrap());
    assert_eq!(r,worker.read_completed_original(&gid).await.unwrap().unwrap());
    assert_eq!(after,snapshot(&v).await);
    // History survives delegation expiry; it never reopens write authority.
    sqlx::raw_sql("CREATE OR REPLACE FUNCTION payment_background_clock_v1() RETURNS TIMESTAMPTZ LANGUAGE sql VOLATILE AS $$ SELECT completion_until+interval '1 second' FROM payment_completion_candidate_grants $$").execute(&v.db.scoped).await.unwrap();
    assert_eq!(r,worker.read_completed_original(&gid).await.unwrap().unwrap());
    PaymentBackgroundDeployment::for_isolated_backend(v.source.clone()).revoke_login(&role).await.unwrap();
    assert!(worker.read_completed_original(&gid).await.is_err());
    worker.close().await;wp.close().await;producer.close().await;pp.close().await;
    v.server.stop();v.db.finish().await;
}

#[tokio::test]
#[ignore="owned PG16 SCRAM and original unified fictional source"]
async fn background_final_definite_decline_preserves_balances() {
    let (v,gid)=accepted().await;
    let (producer,pp,_)=actor(&v,PaymentBackgroundRole::Producer).await;
    let (worker,wp,_)=enabled_worker(&v).await;
    let before=snapshot(&v).await;
    producer.record_explicit_decision(&gid,"decline-original",PaymentSimulationDecision::DefiniteSimulatedDecline).await.unwrap();
    let r=worker.complete_original(&gid).await.unwrap();
    assert_eq!(r["receipt"]["evidence"]["public_operation"]["state"],"FAILED");
    assert_eq!(r["receipt"]["evidence"]["business"]["latest_receipt"]["reason_code"],"MOCK_BANK_DECLINED");
    assert_eq!(r["receipt"]["evidence"]["ledger"],json!([]));
    assert_eq!(before["accounts"],snapshot(&v).await["accounts"]);
    assert_eq!(worker.complete_original(&gid).await.unwrap(),r);
    worker.close().await;wp.close().await;producer.close().await;pp.close().await;
    v.server.stop();v.db.finish().await;
}

#[tokio::test]
#[ignore="owned PG16 SCRAM and original unified fictional source"]
async fn background_final_unknown_expiry_revocation_and_source_keep_hold() {
    for mode in ["unknown","equal","expired","revoke","source","account","producer"] {
        let (v,gid)=accepted().await;
        let (producer,pp,pr)=actor(&v,PaymentBackgroundRole::Producer).await;
        let (worker,wp,_)=enabled_worker(&v).await;
        if mode!="unknown" {producer.record_explicit_decision(&gid,"result",PaymentSimulationDecision::SettleLocalSimulation).await.unwrap();}
        let before=snapshot(&v).await;
        match mode {
            "equal"|"expired"=>{
                let offset=if mode=="expired" {"+interval '1 microsecond'"}else{""};
                sqlx::raw_sql(&format!("CREATE OR REPLACE FUNCTION payment_background_clock_v1() RETURNS TIMESTAMPTZ LANGUAGE sql VOLATILE AS $$ SELECT completion_until {offset} FROM payment_completion_candidate_grants $$")).execute(&v.db.scoped).await.unwrap();
            },
            "revoke"=>{sqlx::query("UPDATE payment_completion_candidate_controls SET revoked=true,control_epoch=control_epoch+1").execute(&v.db.scoped).await.unwrap();},
            "source"=>v.source.deactivate_for_isolated_backend().await.unwrap(),
            "account"=>{sqlx::query("UPDATE card_safety_account_fences_v1 SET epoch=epoch+1,revoked=true WHERE account_id='account_001'").execute(&v.db.scoped).await.unwrap();},
            "producer"=>PaymentBackgroundDeployment::for_isolated_backend(v.source.clone()).revoke_login(&pr).await.unwrap(),
            _=>{},
        }
        let r=worker.complete_original(&gid).await;
        if mode=="unknown" {assert_eq!(r.unwrap()["state"],"WAITING_TRUSTED_DECISION");}else{assert!(r.is_err(),"{mode}: {r:?}");}
        assert_pending(&v).await;assert_eq!(before,snapshot(&v).await,"{mode}");
        worker.close().await;wp.close().await;producer.close().await;pp.close().await;
        v.server.stop();v.db.finish().await;
    }
}

#[tokio::test]
#[ignore="owned PG16: actual member/public writes and deferred COMMIT failure"]
async fn background_final_faults_roll_back_every_write() {
    for stage in ["transfer_accounts_v1","transfer_ledger_v1","transfer_fund_orders_v1","transfer_business_orders_v1","operations","audit_events","public_audit_outbox","payment_completion_candidate_receipts","deferred"] {
        let (v,gid)=accepted().await;
        let (producer,pp,_)=actor(&v,PaymentBackgroundRole::Producer).await;
        let (worker,wp,_)=enabled_worker(&v).await;
        producer.record_explicit_decision(&gid,"fault-original",PaymentSimulationDecision::SettleLocalSimulation).await.unwrap();
        let before=snapshot(&v).await;
        let before_audit:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM audit_events),(SELECT count(*) FROM public_audit_outbox)").fetch_one(&v.db.scoped).await.unwrap();
        let table=if stage=="deferred" {"payment_completion_transactions_v1"}else{stage};
        sqlx::raw_sql(&format!("CREATE FUNCTION step08_owned_failure() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$ BEGIN RAISE EXCEPTION 'step08-owned-failure-{stage}'; END $$")).execute(&v.db.scoped).await.unwrap();
        let trigger=if stage=="deferred" {format!("CREATE CONSTRAINT TRIGGER step08_fault AFTER INSERT ON {table} DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION step08_owned_failure()")}
            else{format!("CREATE TRIGGER step08_fault BEFORE INSERT OR UPDATE ON {table} FOR EACH ROW EXECUTE FUNCTION step08_owned_failure()")};
        sqlx::raw_sql(&trigger).execute(&v.db.scoped).await.unwrap();
        assert!(worker.complete_original(&gid).await.is_err(),"{stage}");
        assert_pending(&v).await;assert_eq!(before,snapshot(&v).await,"{stage}");
        let audit:(i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM audit_events),(SELECT count(*) FROM public_audit_outbox)").fetch_one(&v.db.scoped).await.unwrap();
        assert_eq!(before_audit,audit,"{stage}");
        let contexts:i64=sqlx::query_scalar("SELECT count(*) FROM payment_completion_transactions_v1").fetch_one(&v.db.scoped).await.unwrap();assert_eq!(contexts,0);
        sqlx::raw_sql(&format!("DROP TRIGGER step08_fault ON {table}; DROP FUNCTION step08_owned_failure()")).execute(&v.db.scoped).await.unwrap();
        // Test administrator advances ONLY the scheduler epoch to an expired
        // lease, never the immutable grant, source, decision or business input.
        sqlx::query("UPDATE payment_completion_candidate_jobs SET lease_epoch=lease_epoch+1,leased_at=clock_timestamp()-interval '1 minute',lease_until=clock_timestamp()-interval '30 seconds'").execute(&v.db.scoped).await.unwrap();
        let r=worker.complete_original(&gid).await.unwrap();assert_eq!(r["state"],"COMPLETED","{stage}");
        println!("actual write failure, whole rollback and same-original new-lease recovery: {stage}");
        worker.close().await;wp.close().await;producer.close().await;pp.close().await;
        v.server.stop();v.db.finish().await;
    }
}

#[tokio::test]
#[ignore="owned PG16: actual card save backend, other original hold and terminal balance"]
async fn background_final_card_restrictions_and_other_original_hold() {
    for mode in ["blocked-success","frozen-decline","other-hold-success","other-hold-decline"] {
        let (mut v,gid)=accepted().await;
        let first=v.p.clone();
        let mut second_gid=None;
        if mode.starts_with("other-") {
            let d=draft(&v.server,&v.cookie).await;
            let p=prepared(&v.server,&v.cookie,&d).await;
            v.p=decided(&v.server,&v.cookie,&p,"CONFIRM").await;
            let display=authorize(&v).await;ok(command_bg(&v,execution(&v,&display)).await);
            second_gid=Some(grant(&v).await);v.p=first;
        }
        let (producer,pp,_)=actor(&v,PaymentBackgroundRole::Producer).await;
        let (worker,wp,_)=enabled_worker(&v).await;
        let decline=mode.ends_with("decline");
        producer.record_explicit_decision(&gid,"card-original",if decline {PaymentSimulationDecision::DefiniteSimulatedDecline}else{PaymentSimulationDecision::SettleLocalSimulation}).await.unwrap();
        if mode.starts_with("blocked-")||mode.starts_with("frozen-") {
            sqlx::query("SELECT card_save_freeze_v1('step08_lock','demo_card_salary_v1','USER_SELF',NULL,NULL,true,'step08_freeze_record','user_001','account_001','step08_freeze_operation','USER_REPORT_LOST',NULL,NULL,'step08_freeze_key','step08_reason','CREATED')").execute(&v.db.scoped).await.unwrap();
        }
        let before=snapshot(&v).await;
        let other_before:Option<Value>=if let Some(ref id)=second_gid {Some(sqlx::query_scalar("SELECT to_jsonb(f) FROM transfer_fund_orders_v1 f JOIN payment_completion_candidate_grants g ON g.owner_id=f.owner_id AND g.business_operation_ref=f.operation_id WHERE g.grant_id=$1").bind(id).fetch_one(&v.db.scoped).await.unwrap())}else{None};
        let r=worker.complete_original(&gid).await;
        if mode=="blocked-success" {assert!(r.is_err());assert_eq!(before,snapshot(&v).await);assert_pending(&v).await;}
        else {
            let r=r.unwrap();assert_eq!(r["receipt"]["evidence"]["public_operation"]["state"],if decline{"FAILED"}else{"SUCCEEDED"});
            if decline{assert_eq!(before["accounts"],snapshot(&v).await["accounts"]);}
            if let Some(ref id)=second_gid {
                let after:Value=sqlx::query_scalar("SELECT to_jsonb(f) FROM transfer_fund_orders_v1 f JOIN payment_completion_candidate_grants g ON g.owner_id=f.owner_id AND g.business_operation_ref=f.operation_id WHERE g.grant_id=$1").bind(id).fetch_one(&v.db.scoped).await.unwrap();
                assert_eq!(Some(after),other_before);assert!(worker.read_completed_original(id).await.unwrap().is_none());
            }
        }
        println!("actual card/hold branch verified: {mode}");
        worker.close().await;wp.close().await;producer.close().await;pp.close().await;
        v.server.stop();v.db.finish().await;
    }
}
