//! Existing public-mechanism fixture reused; no new provider/authority/business JSON.
//! SQL candidate rows below test persistence mechanics, NOT adopted grants.
use super::*;
use safeflow_storage::{PaymentAcceptedHistoryReader, PaymentCompletionCandidateQueue};

fn reader(db: &Db, server: &Server) -> PaymentAcceptedHistoryReader {
    PaymentAcceptedHistoryReader::for_trusted_backend(
        db.repo.clone(),
        server.state.0.sessions.clone(),
    )
}
async fn read(
    db: &Db,
    server: &Server,
    cookie: &str,
    p: &Value,
) -> Result<safeflow_storage::PaymentAcceptedHistoryReport, StorageError> {
    reader(db, server)
        .query(
            &session(cookie),
            &serde_json::from_value(p["task_id"].clone()).unwrap(),
            &serde_json::from_value(p["operation"]["operation_id"].clone()).unwrap(),
        )
        .await
}
async fn seed(db: &Db, report: &safeflow_storage::PaymentAcceptedHistoryReport, id: &str) {
    let e = &report.evidence;
    sqlx::query("INSERT INTO payment_completion_candidate_grants(grant_id,owner_id,task_id,preparation_operation_id,execution_operation_id,business_operation_ref,original_key,grant_epoch,first_submitted_at,completion_until,evidence,evidence_digest,policy_ref) SELECT $1,owner_id,task_id,$2,operation_id,business_operation_ref,idempotency_key,1,submitted_at,clock_timestamp()+interval '1 hour',$3,$4,'UNADOPTED_STORAGE_MECHANISM_ONLY' FROM public_execution_requests WHERE operation_id=$5")
        .bind(id).bind(e["preparation_operation_id"].as_str().unwrap()).bind(sqlx::types::Json(e))
        .bind(report.evidence_digest.as_str()).bind(e["execution_operation_id"].as_str().unwrap()).execute(&db.scoped).await.unwrap();
    sqlx::query("INSERT INTO payment_completion_candidate_controls SELECT execution_operation_id,grant_id,grant_epoch,1,false,false FROM payment_completion_candidate_grants WHERE grant_id=$1")
        .bind(id).execute(&db.scoped).await.unwrap();
    sqlx::query("INSERT INTO payment_completion_candidate_jobs(grant_id) VALUES($1)")
        .bind(id)
        .execute(&db.scoped)
        .await
        .unwrap();
}
async fn decision(db: &Db, id: &str, grant: &str, result: &str) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO payment_completion_candidate_decisions(decision_id,grant_id,source_id,source_epoch,result_id,decision_seq,outcome,envelope,envelope_digest,fact_at) VALUES($1,$2,'UNADOPTED_STORAGE_TEST',1,$3,1,'SETTLE_LOCAL_SIMULATION','{}',repeat('a',64),clock_timestamp())")
        .bind(id).bind(grant).bind(result).execute(&db.scoped).await.map(|_|())
}
async fn candidate_state(db: &Db) -> Vec<Value> {
    let mut all = checked_state(db).await;
    for table in [
        "payment_completion_candidate_fences",
        "payment_completion_candidate_grants",
        "payment_completion_candidate_controls",
        "payment_completion_candidate_decisions",
        "payment_completion_candidate_jobs",
        "payment_completion_candidate_receipts",
    ] {
        all.push(sqlx::query_scalar(&format!("SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text),'[]') FROM {table} t")).fetch_one(&db.scoped).await.unwrap());
    }
    all
}

async fn history_wait(db: &Db, holder: i32, reader: Option<i32>) -> Result<Option<Value>, sqlx::Error> {
    // Scope the first discovery to the private schema's actual waiting locks;
    // every subsequent sample is bound to that same reader PID. Do NOT filter
    // SQL text: any second-holder wait after invalidation is a regression.
    sqlx::query_scalar("SELECT jsonb_build_object('pid',a.pid,'backend_start',a.backend_start,'application_name',a.application_name,'observed_at',clock_timestamp(),'query',a.query,'state',a.state,'wait_event_type',a.wait_event_type,'wait_event',a.wait_event,'blockers',pg_blocking_pids(a.pid),'namespace',$2::text) FROM pg_stat_activity a WHERE $1=ANY(pg_blocking_pids(a.pid)) AND a.datname=current_database() AND ($3::integer IS NULL OR a.pid=$3) AND EXISTS(SELECT 1 FROM pg_locks l JOIN pg_class c ON c.oid=l.relation JOIN pg_namespace n ON n.oid=c.relnamespace WHERE l.pid=a.pid AND NOT l.granted AND n.nspname=$2)")
        .bind(holder).bind(&db.namespace).bind(reader).fetch_optional(&db.admin).await
}

fn history_observation(root: &std::path::Path, namespace: &str, mode: &str,
    sequence: &std::sync::atomic::AtomicU64, stage: &str, details: Value) {
    use std::io::Write;
    let index = sequence.fetch_add(1, Ordering::SeqCst);
    let path = root.join(format!("history-observe-{mode}-{namespace}-{index:05}.json"));
    let value = json!({"case":mode,"namespace":namespace,"sequence":index,"stage":stage,
        "observed_unix_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis(),
        "details":details});
    // Each raw observation is durable before assertions, including errors and
    // deadline decisions. Never overwrite an earlier successful or failed run.
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path).unwrap();
    file.write_all(&serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    file.sync_all().unwrap();
}

fn history_second_wait_ready(mode: &str, wait: &Value, holder: i32) -> bool {
    // This is called only after a real second-holder wait for the bound PID.
    // Invalid cases must keep ANY such wait, including a mixed snapshot.
    mode != "valid" || (wait["query"].as_str().is_some_and(|q|q.contains("EXISTS(SELECT 1 FROM _sqlx_migrations"))
        && wait["wait_event_type"] == "Lock"
        && wait["blockers"].as_array().is_some_and(|pids|pids.contains(&json!(holder))))
}

#[test]
fn history_observation_mixed_snapshot_keeps_invalid_advancement() {
    // The independent single-statement experiment's actual mixed and next
    // stable values. No business fixture, source, or expected result invented.
    let mixed = json!({"pid":4324,"query":"SELECT * FROM tasks WHERE task_id='task' AND owner_id='owner'",
        "state":"active","blockers":[17452],"wait_event":"relation","wait_event_type":"Lock"});
    assert!(!history_second_wait_ready("valid", &mixed, 17452));
    for mode in ["revoke", "rotate", "expire"] {
        assert!(history_second_wait_ready(mode, &mixed, 17452));
    }
    let mut stable = mixed.clone();
    stable["query"] = json!("SELECT EXISTS(SELECT 1 FROM _sqlx_migrations WHERE version=13 AND success)");
    assert!(history_second_wait_ready("valid", &stable, 17452));
    assert!(!history_second_wait_ready("valid", &stable, 35544));
}

#[tokio::test]
#[ignore = "requires private PG; two observed SQL barriers, real session invalidation while suspended"]
async fn completion_candidate_history_stops_after_suspended_session_invalidation() {
    use safeflow_runtime::SessionResolver;
    for mode in ["revoke", "rotate", "expire", "valid"] {
        let db = Db::new().await;
        let output = std::env::var_os("SAFEFLOW_SIMULATION_EVIDENCE").expect("explicit evidence root");
        let output = std::path::Path::new(&output);
        let sequence = std::sync::atomic::AtomicU64::new(0);
        let record = |stage: &str, details: Value| history_observation(output, &db.namespace, mode, &sequence, stage, details);
        record("setup", json!({"phase":"before business fixture"}));
        let payment = server(&db).await;
        let payer = payment.login().await;
        let (p, _, binding) = setup(&db, &payment, &payer).await;
        let svc = service(&db, &payment, Some(binding), MockBehavior::Pending, SecurityLevel::Yellow);
        execute(&svc, &payer, &p).await.unwrap();
        let expected = read(&db, &payment, &payer, &p).await.unwrap();
        let current = Server::new(&db, "user_001", Duration::from_secs(60),
            if mode == "expire" { Duration::from_secs(1) } else { Duration::from_secs(60) }).await;
        let cookie = current.login().await;
        let reference = session(&cookie);
        let original_binding = current.state.0.sessions.resolve_binding(&reference).unwrap();
        let before = candidate_state(&db).await;

        // First barrier is the reader's first evidence SELECT; the second is
        // the immediately following lifecycle query. Both locks are private,
        // uncommitted test scaffolding, not product or business modifications.
        let mut first = db.scoped.begin().await.unwrap();
        sqlx::query("LOCK TABLE tasks IN ACCESS EXCLUSIVE MODE").execute(&mut *first).await.unwrap();
        let first_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&mut *first).await.unwrap();
        let mut second = db.scoped.begin().await.unwrap();
        sqlx::query("LOCK TABLE _sqlx_migrations IN ACCESS EXCLUSIVE MODE").execute(&mut *second).await.unwrap();
        let second_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&mut *second).await.unwrap();
        let completed = AtomicBool::new(false);
        let task = serde_json::from_value(p["task_id"].clone()).unwrap();
        let prep = serde_json::from_value(p["operation"]["operation_id"].clone()).unwrap();
        let history = reader(&db, &current);
        let query = async {
            let result = history.query(&reference, &task, &prep).await;
            record("reader-returned", json!({"result":match &result { Ok(_) => "REPORT".into(), Err(e) => format!("{e:?}") },
                "report_published":result.is_ok(),"session_valid":current.state.0.sessions.resolve_binding(&reference).is_ok()}));
            completed.store(true, Ordering::SeqCst);
            result
        };
        let observe = async {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            record("locks-held", json!({"first_blocker_pid":first_pid,"second_blocker_pid":second_pid,
                "session_valid":current.state.0.sessions.resolve_binding(&reference).is_ok()}));
            let first_wait = loop {
                let sample = history_wait(&db, first_pid, None).await;
                let expired = std::time::Instant::now() >= deadline;
                let ended = completed.load(Ordering::SeqCst);
                record("first-sample", json!({"sample":sample.as_ref().ok(),"error":sample.as_ref().err().map(|e|e.to_string()),
                    "first_blocker_pid":first_pid,"second_blocker_pid":second_pid,"reader_completed":ended,
                    "deadline_reason":if expired {Some("first barrier not reached")} else {None}}));
                assert!(!expired, "first barrier not reached");
                if let Some(wait) = sample.unwrap() {
                    if wait["query"].as_str().is_some_and(|q|q.contains("SELECT * FROM tasks"))
                        && wait["wait_event_type"] == "Lock"
                        && wait["blockers"].as_array().unwrap().contains(&json!(first_pid)) { break wait; }
                }
                assert!(!ended, "reader ended before first barrier");
                tokio::task::yield_now().await;
            };
            assert!(first_wait["query"].as_str().unwrap().contains("SELECT * FROM tasks"));
            let reader_pid = i32::try_from(first_wait["pid"].as_i64().unwrap()).unwrap();
            let mut replacement_version = None;
            match mode {
                "revoke" => current.state.0.sessions.revoke(&reference).unwrap(),
                "rotate" => {
                    let next = current.state.0.sessions.issue(SECRET, Some(&reference)).unwrap();
                    let next_binding = current.state.0.sessions.resolve_binding(&session(&next)).unwrap();
                    assert_eq!(original_binding.user_id, next_binding.user_id);
                    assert!(next_binding.binding_version > original_binding.binding_version);
                    replacement_version = Some(next_binding.binding_version);
                }
                "expire" => {
                    while current.state.0.sessions.resolve_binding(&reference).is_ok() {
                        let expired = std::time::Instant::now() >= deadline;
                        record("expiry-wait",json!({"reader_pid":reader_pid,"session_valid":true,
                            "deadline_reason":if expired {Some("session did not expire")} else {None}}));
                        assert!(!expired, "session did not expire");
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
                "valid" => assert_eq!(original_binding, current.state.0.sessions.resolve_binding(&reference).unwrap()),
                _ => unreachable!(),
            }
            record("session-action", json!({"reader_pid":reader_pid,"first_blocker_pid":first_pid,"second_blocker_pid":second_pid,
                "session_valid":current.state.0.sessions.resolve_binding(&reference).is_ok(),"replacement_binding_version":replacement_version}));
            if mode != "valid" { assert!(current.state.0.sessions.resolve_binding(&reference).is_err()); }
            // Release only after BOTH the actual SQL wait and invalidation.
            let released = first.commit().await;
            record("first-released", json!({"reader_pid":reader_pid,"error":released.as_ref().err().map(|e|e.to_string())}));
            released.unwrap();
            let next_wait = loop {
                let sample = history_wait(&db, second_pid, Some(reader_pid)).await;
                let expired = std::time::Instant::now() >= deadline;
                let ended = completed.load(Ordering::SeqCst);
                record("second-sample", json!({"sample":sample.as_ref().ok(),"error":sample.as_ref().err().map(|e|e.to_string()),
                    "reader_pid":reader_pid,"first_blocker_pid":first_pid,"second_blocker_pid":second_pid,
                    "session_valid":current.state.0.sessions.resolve_binding(&reference).is_ok(),"reader_completed":ended,
                    "deadline_reason":if expired {Some("neither returned nor reached stable second barrier")} else {None}}));
                assert!(!expired, "neither returned nor reached stable second barrier");
                if let Some(wait) = sample.unwrap() {
                    assert_eq!(wait["pid"], first_wait["pid"]);
                    // For invalid sessions even stale SQL text cannot hide a
                    // real second-blocker wait. Keep it and fail below.
                    // Only the VALID control may retain the lock and resample.
                    if history_second_wait_ready(mode, &wait, second_pid) { break Some(wait); }
                }
                if ended { break None; }
                tokio::task::yield_now().await;
            };
            if let Some(wait) = &next_wait {
                assert_eq!(wait["pid"], first_wait["pid"]);
                if mode == "valid" { assert!(wait["query"].as_str().unwrap().contains("EXISTS(SELECT 1 FROM _sqlx_migrations")); }
            }
            let released = second.commit().await;
            record("second-released", json!({"reader_pid":reader_pid,"next_wait":next_wait,"error":released.as_ref().err().map(|e|e.to_string())}));
            released.unwrap();
            json!({"case":mode,"namespace":db.namespace,"first_blocker_pid":first_pid,
                "second_blocker_pid":second_pid,"first_wait":first_wait,"next_wait":next_wait,
                "original_binding_version":original_binding.binding_version,"replacement_binding_version":replacement_version,
                "invalidated_after_observed_wait":mode!="valid"})
        };
        let joined = tokio::time::timeout(Duration::from_secs(20), async { tokio::join!(query, observe) }).await;
        record("joined",json!({"deadline_reason":if joined.is_err() {Some("outer 20s timeout")} else {None},
            "session_valid":current.state.0.sessions.resolve_binding(&reference).is_ok()}));
        let (result, mut evidence) = joined.unwrap();
        // The next legitimate read drains any cancelled read-only transaction
        // and proves recovery. All application facts, holds and audit stay exact.
        let recovered = read(&db, &payment, &payer, &p).await;
        let after = candidate_state(&db).await;
        let no_writes = before == after;
        record("final-result",json!({"result":match &result { Ok(_) => "REPORT".into(), Err(e) => format!("{e:?}") },
            "recovery_error":recovered.as_ref().err().map(|e|format!("{e:?}")),"recovered_report_equal":recovered.as_ref().ok()==Some(&expected),
            "business_state_unchanged":no_writes,"next_wait":evidence["next_wait"],"before":before,"after":after}));
        assert_eq!(expected, recovered.unwrap());
        evidence["business_state_unchanged"] = json!(no_writes);
        evidence["report_published"] = json!(result.is_ok());
        evidence["result"] = json!(match &result { Ok(_) => "REPORT".into(), Err(e) => format!("{e:?}") });
        println!("history suspended session evidence: {evidence}");
        let path = output.join(format!("history-wait-{mode}-{}.json",db.namespace));
        use std::io::Write;
        std::fs::OpenOptions::new().write(true).create_new(true).open(path).unwrap()
            .write_all(&serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
        let advanced = !evidence["next_wait"].is_null();
        current.stop();
        payment.stop();
        db.finish().await;
        assert!(no_writes);
        if mode == "valid" { assert_eq!(result.unwrap(), expected); assert!(advanced); }
        else {
            assert_eq!(result.unwrap_err(), StorageError::Session);
            assert!(!advanced, "invalid session resumed subsequent evidence SQL: {mode}");
        }
    }
}
#[tokio::test]
#[ignore = "requires private PG; historical read and scheduler mechanism only"]
async fn completion_candidate_history_and_lease_are_not_payment_authority() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (p, provider, binding) = setup(&db, &server, &cookie).await;
    assert!(read(&db, &server, &cookie, &p).await.is_err()); // no original, no inferred unpaid
    let svc = service(
        &db,
        &server,
        Some(binding),
        MockBehavior::Pending,
        SecurityLevel::Yellow,
    );
    execute(&svc, &cookie, &p).await.unwrap();
    let before = candidate_state(&db).await;
    let report = read(&db, &server, &cookie, &p).await.unwrap();
    assert_eq!(report, read(&db, &server, &cookie, &p).await.unwrap());
    assert_eq!(before, candidate_state(&db).await);
    assert_eq!(
        report.evidence["original_recipient_account_id"],
        "composition_recipient"
    );
    assert_eq!(
        report.evidence["accounting"]["operation_reserved"],
        "200.00"
    );
    let calls = provider.calls.load(Ordering::SeqCst);
    provider.fail.store(true, Ordering::SeqCst); // reader must not require a live card source
    let fresh = server.login().await;
    assert_eq!(report, read(&db, &server, &fresh, &p).await.unwrap());
    assert_eq!(calls, provider.calls.load(Ordering::SeqCst));
    server.state.0.sessions.revoke(&session(&fresh)).unwrap();
    assert_eq!(
        read(&db, &server, &fresh, &p).await.unwrap_err(),
        StorageError::Session
    );
    seed(&db, &report, "candidate_original").await;
    let queue = PaymentCompletionCandidateQueue::for_trusted_backend(db.repo.clone());
    assert!(queue
        .claim("missing", "tag", Duration::from_secs(1))
        .await
        .unwrap()
        .is_none());
    assert!(queue
        .claim("candidate_original", "tag", Duration::ZERO)
        .await
        .is_err());
    assert!(queue
        .claim("candidate_original", "tag", Duration::from_secs(301))
        .await
        .is_err());
    let (a, b) = tokio::join!(
        queue.claim("candidate_original", "label_a", Duration::from_secs(5)),
        queue.claim("candidate_original", "label_b", Duration::from_secs(5))
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_ne!(a.is_some(), b.is_some());
    let old = a.or(b).unwrap();
    assert_eq!(old.epoch(), 1);
    assert!(queue.is_current(&old).await.unwrap());
    assert!(queue.clone().is_current(&old).await.unwrap());
    // Identical DB/schema/row/epoch/label, but a separately constructed queue
    // is deliberately a different scope. Clones are the supported sharing path.
    let other = PaymentCompletionCandidateQueue::for_trusted_backend(db.repo.clone());
    assert_eq!(
        other.is_current(&old).await.err(),
        Some(StorageError::TransactionScope)
    );
    assert_eq!(
        other.renew(&old, Duration::from_secs(1)).await.err(),
        Some(StorageError::TransactionScope)
    );
    let foreign_url = std::env::var("SAFEFLOW_COMPLETION_OTHER_DATABASE_URL")
        .expect("explicit second private database");
    let foreign_repo = safeflow_storage::PgRepository::connect(&foreign_url, &db.namespace)
        .await
        .unwrap();
    foreign_repo.migrate().await.unwrap();
    let foreign = PaymentCompletionCandidateQueue::for_trusted_backend(foreign_repo.clone());
    assert_eq!(
        foreign.is_current(&old).await.err(),
        Some(StorageError::TransactionScope)
    );
    assert_eq!(
        foreign.renew(&old, Duration::from_secs(1)).await.err(),
        Some(StorageError::TransactionScope)
    );
    foreign_repo.close().await;
    let new = queue
        .renew(&old, Duration::from_secs(5))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(new.epoch(), 2);
    assert!(!queue.is_current(&old).await.unwrap());
    assert!(queue
        .renew(&old, Duration::from_secs(5))
        .await
        .unwrap()
        .is_none());
    sqlx::query("UPDATE payment_completion_candidate_controls SET revoked=true,control_epoch=control_epoch+1").execute(&db.scoped).await.unwrap();
    assert!(!queue.is_current(&new).await.unwrap());
    assert!(queue
        .renew(&new, Duration::from_secs(5))
        .await
        .unwrap()
        .is_none());
    assert_processing_hold(&db, &p).await;
    assert_eq!(
        before,
        checked_state(&db)
            .await
            .into_iter()
            .chain((0..6).map(|_| json!([])))
            .collect::<Vec<_>>()
    );
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires private PG; existing accepted fixture, SQL persistence constraints only"]
async fn completion_candidate_constraints_cross_original_aba_and_rollback() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (p, _, binding) = setup(&db, &server, &cookie).await;
    let svc = service(
        &db,
        &server,
        Some(binding),
        MockBehavior::Pending,
        SecurityLevel::Yellow,
    );
    execute(&svc, &cookie, &p).await.unwrap();
    let report = read(&db, &server, &cookie, &p).await.unwrap();
    seed(&db, &report, "g_one").await;
    let (p2, _, binding2) = setup(&db, &server, &cookie).await;
    let svc2 = service(
        &db,
        &server,
        Some(binding2),
        MockBehavior::Pending,
        SecurityLevel::Yellow,
    );
    execute(&svc2, &cookie, &p2).await.unwrap();
    seed(
        &db,
        &read(&db, &server, &cookie, &p2).await.unwrap(),
        "g_two",
    )
    .await;
    let state = candidate_state(&db).await;
    // A complete real association cannot be combined with another owner/key/P.
    for column in [
        "owner_id",
        "task_id",
        "preparation_operation_id",
        "execution_operation_id",
        "business_operation_ref",
        "original_key",
    ] {
        let sql=format!("INSERT INTO payment_completion_candidate_grants SELECT 'wrong_{column}',{}, {}, {}, {}, {}, {},2,first_submitted_at,completion_until,evidence,evidence_digest,policy_ref FROM payment_completion_candidate_grants WHERE grant_id='g_one'",
            if column=="owner_id" {"'composition_recipient_owner'"}else{"owner_id"},if column=="task_id" {"(SELECT task_id FROM payment_completion_candidate_grants WHERE grant_id='g_two')"}else{"task_id"},
            if column=="preparation_operation_id" {"(SELECT preparation_operation_id FROM payment_completion_candidate_grants WHERE grant_id='g_two')"}else{"preparation_operation_id"},if column=="execution_operation_id" {"(SELECT execution_operation_id FROM payment_completion_candidate_grants WHERE grant_id='g_two')"}else{"execution_operation_id"},
            if column=="business_operation_ref" {"(SELECT business_operation_ref FROM payment_completion_candidate_grants WHERE grant_id='g_two')"}else{"business_operation_ref"},if column=="original_key" {"(SELECT original_key FROM payment_completion_candidate_grants WHERE grant_id='g_two')"}else{"original_key"});
        assert!(
            sqlx::query(&sql).execute(&db.scoped).await.is_err(),
            "{column}"
        );
    }
    assert_eq!(state, candidate_state(&db).await);
    assert!(
        sqlx::query("UPDATE payment_completion_candidate_grants SET evidence='{}'")
            .execute(&db.scoped)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM payment_completion_candidate_grants")
            .execute(&db.scoped)
            .await
            .is_err()
    );
    decision(&db, "d_one", "g_one", "result_one").await.unwrap();
    assert!(decision(&db, "d_two", "g_two", "result_one").await.is_err());
    decision(&db, "d_two", "g_two", "result_two").await.unwrap();
    assert!(decision(&db, "d_other", "g_one", "result_other")
        .await
        .is_err());
    assert!(decision(&db, "d_one", "g_one", "different").await.is_err());
    assert!(
        sqlx::query("UPDATE payment_completion_candidate_decisions SET envelope='{}'")
            .execute(&db.scoped)
            .await
            .is_err()
    );
    assert!(sqlx::query(
        "UPDATE payment_completion_candidate_controls SET control_epoch=1,revoked=true"
    )
    .execute(&db.scoped)
    .await
    .is_err());
    sqlx::query("UPDATE payment_completion_candidate_controls SET control_epoch=2,revoked=true")
        .execute(&db.scoped)
        .await
        .unwrap();
    assert!(sqlx::query(
        "UPDATE payment_completion_candidate_controls SET control_epoch=3,revoked=false"
    )
    .execute(&db.scoped)
    .await
    .is_err());
    sqlx::query("INSERT INTO payment_completion_candidate_fences VALUES('WORKER','UNADOPTED_STORAGE_ONLY',1,true,clock_timestamp()+interval '1 hour','{}','NOT_AN_ADOPTION')").execute(&db.scoped).await.unwrap();
    let mut fence_read = db.scoped.begin().await.unwrap();
    sqlx::query("SELECT epoch FROM payment_completion_candidate_fences FOR SHARE")
        .execute(&mut *fence_read)
        .await
        .unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *fence_read)
        .await
        .unwrap();
    let wait = async {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)) AND query LIKE '%payment_completion_candidate_fences%')")
                .bind(holder).fetch_one(&db.admin).await.unwrap();
            if blocked {
                println!("actual candidate fence SHARE/rotation wait confirmed");
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            tokio::task::yield_now().await;
        }
        fence_read.commit().await.unwrap();
    };
    let (rotated, ()) = tokio::join!(
        sqlx::query("UPDATE payment_completion_candidate_fences SET epoch=2,enabled=false")
            .execute(&db.scoped),
        wait
    );
    rotated.unwrap();
    assert!(
        sqlx::query("UPDATE payment_completion_candidate_fences SET epoch=1,enabled=true")
            .execute(&db.scoped)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM payment_completion_candidate_fences")
            .execute(&db.scoped)
            .await
            .is_err()
    );
    // Receipt cross-grant/result FK cannot consume a result under another original.
    assert!(sqlx::query("INSERT INTO payment_completion_candidate_receipts SELECT execution_operation_id,grant_id,grant_epoch,'d_one',4,'{}',repeat('a',64),clock_timestamp() FROM payment_completion_candidate_grants WHERE grant_id='g_two'").execute(&db.scoped).await.is_err());
    sqlx::query("INSERT INTO payment_completion_candidate_receipts SELECT execution_operation_id,grant_id,grant_epoch,'d_one',4,'{}',repeat('a',64),clock_timestamp() FROM payment_completion_candidate_grants WHERE grant_id='g_one'").execute(&db.scoped).await.unwrap();
    assert!(
        sqlx::query("DELETE FROM payment_completion_candidate_receipts")
            .execute(&db.scoped)
            .await
            .is_err()
    );
    let before = candidate_state(&db).await;
    let mut tx = db.scoped.begin().await.unwrap();
    sqlx::query("UPDATE payment_completion_candidate_fences SET epoch=3")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE payment_completion_candidate_controls SET control_epoch=0")
            .execute(&mut *tx)
            .await
            .is_err()
    );
    // PostgreSQL aborted transaction cannot commit the preceding successful write.
    tx.commit().await.unwrap();
    assert_eq!(before, candidate_state(&db).await);
    let (pending,held,ledger):(i64,i64,i64)=sqlx::query_as("SELECT (SELECT count(*) FROM transfer_fund_orders_v1 WHERE state='PROCESSING'),(SELECT sum(amount_cents)::bigint FROM transfer_fund_orders_v1 WHERE state='PROCESSING'),(SELECT count(*) FROM transfer_ledger_v1)")
        .fetch_one(&db.scoped).await.unwrap();
    assert_eq!((pending, held, ledger), (2, 40000, 0));
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires private PG; read-only original corruption counterexamples"]
async fn completion_candidate_history_rejects_incomplete_and_tampered_evidence() {
    // A fresh private schema for EACH committed corruption. No restored or
    // reused damaged database, no speculative repair of business facts.
    for sql in [
        "UPDATE payment_funds_execution_v1 SET association_digest=repeat('a',64)",
        "UPDATE payment_preparations SET snapshot=jsonb_set(snapshot,'{transfer,remark}','\"changed\"')",
        "UPDATE public_execution_requests SET idempotency_key='changed'",
        "UPDATE execution_permits SET session_binding_version=session_binding_version+1",
        "UPDATE execution_permits SET status='REVOKED',consumed_at_ms=NULL",
        "UPDATE execution_permits SET consumed_at_ms=issued_at_ms-1",
        "UPDATE operations SET revision=4 WHERE state='PROCESSING'",
        "DELETE FROM transfer_business_orders_v1",
        "DELETE FROM transfer_fund_orders_v1",
        "UPDATE transfer_fund_orders_v1 SET recipient_account_id='account_001'",
        "UPDATE transfer_fund_orders_v1 SET recipient_account_id='missing_recipient'",
        "DELETE FROM audit_events WHERE kind='PERMIT_CONSUMED'",
    ] {
        let db=Db::new().await; let server=server(&db).await; let cookie=server.login().await;
        let (p,_,binding)=setup(&db,&server,&cookie).await;
        let svc=service(&db,&server,Some(binding),MockBehavior::Pending,SecurityLevel::Yellow);
        execute(&svc,&cookie,&p).await.unwrap();
        read(&db,&server,&cookie,&p).await.unwrap();
        let table=sql.split_whitespace().nth(if sql.starts_with("DELETE"){2}else{1}).unwrap();
        let mut conn=db.scoped.acquire().await.unwrap();
        sqlx::query(&format!("ALTER TABLE {table} DISABLE TRIGGER ALL")).execute(&mut *conn).await.unwrap();
        // CHECK constraints still protect impossible timestamps. Such attempted
        // corruption is rejected by DB; otherwise the history reader must reject.
        let changed=sqlx::query(sql).execute(&mut *conn).await;
        if changed.is_ok(){assert!(read(&db,&server,&cookie,&p).await.is_err(),"accepted corruption: {sql}");}
        else { assert!(sql.contains("consumed_at_ms=issued_at_ms-1") || sql.contains("recipient_account_id='account_001'"),"unexpected failed injection {sql}: {changed:?}"); }
        drop(conn);
        println!("history corruption rejected: {sql}");
        server.stop(); db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires private PG; actual row-lock wait and post-wait DB deadline"]
async fn completion_candidate_lease_wait_expiry_reclaim_and_control_stop() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (p, _, binding) = setup(&db, &server, &cookie).await;
    let svc = service(
        &db,
        &server,
        Some(binding),
        MockBehavior::Pending,
        SecurityLevel::Yellow,
    );
    execute(&svc, &cookie, &p).await.unwrap();
    seed(
        &db,
        &read(&db, &server, &cookie, &p).await.unwrap(),
        "lock_job",
    )
    .await;
    let q = PaymentCompletionCandidateQueue::for_trusted_backend(db.repo.clone());
    let old = q
        .claim("lock_job", "label_old", Duration::from_millis(300))
        .await
        .unwrap()
        .unwrap();
    let mut blocker = db.scoped.begin().await.unwrap();
    sqlx::query("SELECT grant_id FROM payment_completion_candidate_jobs FOR UPDATE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    let observe = async {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)) AND query LIKE '%payment_completion_candidate_jobs%')")
                .bind(pid).fetch_one(&db.scoped).await.unwrap();
            if blocked {
                println!("actual job lock wait confirmed via pg_blocking_pids");
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(350)).await;
        blocker.commit().await.unwrap();
    };
    let (renew, ()) = tokio::join!(q.renew(&old, Duration::from_secs(1)), observe);
    assert!(renew.unwrap().is_none());
    let fresh = q
        .claim("lock_job", "label_new", Duration::from_secs(1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fresh.epoch(), 2);
    assert!(!q.is_current(&old).await.unwrap());
    assert!(q
        .renew(&old, Duration::from_secs(1))
        .await
        .unwrap()
        .is_none());
    sqlx::query("UPDATE payment_completion_candidate_controls SET paused=true,control_epoch=control_epoch+1").execute(&db.scoped).await.unwrap();
    assert!(!q.is_current(&fresh).await.unwrap());
    assert!(q
        .renew(&fresh, Duration::from_secs(1))
        .await
        .unwrap()
        .is_none());
    assert_processing_hold(&db, &p).await;
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires private PG; seed retained solely for separate-process recovery probes"]
async fn completion_candidate_recovery_seed() {
    let Some(output) = std::env::var_os("SAFEFLOW_COMPLETION_EVIDENCE") else {
        panic!("explicit recovery evidence root required");
    };
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (p, _, binding) = setup(&db, &server, &cookie).await;
    let svc = service(
        &db,
        &server,
        Some(binding),
        MockBehavior::Pending,
        SecurityLevel::Yellow,
    );
    execute(&svc, &cookie, &p).await.unwrap();
    let report = read(&db, &server, &cookie, &p).await.unwrap();
    seed(&db, &report, "recovery_job").await;
    decision(&db, "recovery_decision", "recovery_job", "recovery_result")
        .await
        .unwrap();
    let data = json!({"namespace":db.namespace,"report":report,"funds_state":checked_state(&db).await,
        "notice":"candidate SQL storage seed only, NOT an adopted policy/identity/decision"});
    let file = std::path::Path::new(&output).join(format!("recovery-seed-{}.json", db.namespace));
    use std::io::Write;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(file)
        .unwrap()
        .write_all(&serde_json::to_vec_pretty(&data).unwrap())
        .unwrap();
    server.stop();
    db.repo.close().await;
    db.scoped.close().await;
    db.admin.close().await;
    // Deliberately retain only this private schema and data for process/PG restart.
}

#[tokio::test]
#[ignore = "requires private PG; actual expiry passage, current session and terminal history"]
async fn completion_candidate_historical_expiry_is_not_current_authority() {
    let db = Db::new().await;
    // Reuse the existing server/Runtime/Policy assembly with an explicitly short
    // test confirmation lifetime. No clock source or authority is invented.
    let inner = Server::with_draft(
        &db,
        "user_001",
        Duration::from_secs(60),
        Duration::from_secs(600),
        Some(
            DraftConfirmationConfig::new(
                Duration::from_secs(2),
                Duration::from_secs(300),
                600,
                "completion-history-expiry-test",
            )
            .unwrap()
            .with_lifecycle_v2()
            .unwrap(),
        ),
    )
    .await;
    let runtime = safeflow_runtime::RuntimeBootstrap::build(
        safeflow_runtime::ToolRegistry::new(),
        safeflow_runtime::DomainPolicyRegistry::new(),
        safeflow_runtime::ToolSecurityCatalog::new(),
        inner.state.0.sessions.clone(),
        Arc::new(safeflow_runtime::InMemoryAuditRecorder::new()),
    )
    .unwrap()
    .with_final_payment_policy_for_backend(
        safeflow_storage::payment_final_registration_for_backend(payment_profile(
            SecurityLevel::Yellow,
        ))
        .unwrap(),
    )
    .unwrap();
    let server = PaymentServer {
        inner,
        final_runtime: runtime,
    };
    let cookie = server.login().await;
    let (p, provider, binding) = setup(&db, &server, &cookie).await;
    let svc = service(
        &db,
        &server,
        Some(binding),
        MockBehavior::Pending,
        SecurityLevel::Yellow,
    );
    execute(&svc, &cookie, &p).await.unwrap();
    let report = read(&db, &server, &cookie, &p).await.unwrap();
    let calls = provider.calls.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let expired: bool = sqlx::query_scalar(
        "SELECT clock_timestamp()>=expires_at FROM server_confirmations WHERE confirmation_ref=$1",
    )
    .bind(p["confirmation_ref"].as_str().unwrap())
    .fetch_one(&db.scoped)
    .await
    .unwrap();
    assert!(expired);
    assert_eq!(report, read(&db, &server, &cookie, &p).await.unwrap());
    let task = serde_json::from_value(p["task_id"].clone()).unwrap();
    let prep = serde_json::from_value(p["operation"]["operation_id"].clone()).unwrap();
    assert_eq!(calls, provider.calls.load(Ordering::SeqCst));
    assert_eq!(
        svc.resolve_simulated_success(&session(&cookie), &task, &prep)
            .await
            .err(),
        Some(StorageError::ConfirmationExpired)
    );
    server.state.0.sessions.revoke(&session(&cookie)).unwrap();
    let fresh = server.login().await;
    assert_eq!(report, read(&db, &server, &fresh, &p).await.unwrap());
    let foreign = Server::new(
        &db,
        "foreign_owner",
        Duration::from_secs(60),
        Duration::from_secs(60),
    )
    .await;
    let foreign_cookie = foreign.login().await;
    assert_eq!(
        read(&db, &foreign, &foreign_cookie, &p).await.err(),
        Some(StorageError::NotFound)
    );
    foreign.stop();
    assert_processing_hold(&db, &p).await;
    server.stop();
    db.finish().await;

    let db = Db::new().await;
    let server = super::server(&db).await;
    let cookie = server.login().await;
    let (p, _, binding) = setup(&db, &server, &cookie).await;
    let svc = service(
        &db,
        &server,
        Some(binding),
        MockBehavior::Immediate,
        SecurityLevel::Yellow,
    );
    execute(&svc, &cookie, &p).await.unwrap();
    let before = candidate_state(&db).await;
    assert_eq!(
        read(&db, &server, &cookie, &p).await.err(),
        Some(StorageError::ReconcileRequired)
    );
    let original = safeflow_storage::PaymentOriginalReader::for_trusted_backend(
        db.repo.clone(),
        server.state.0.sessions.clone(),
    )
    .query(
        &session(&cookie),
        &serde_json::from_value(p["task_id"].clone()).unwrap(),
        &serde_json::from_value(p["operation"]["operation_id"].clone()).unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(
        original.state,
        safeflow_storage::PaymentOriginalState::Succeeded
    );
    assert_eq!(before, candidate_state(&db).await);
    server.stop();
    db.finish().await;
}
