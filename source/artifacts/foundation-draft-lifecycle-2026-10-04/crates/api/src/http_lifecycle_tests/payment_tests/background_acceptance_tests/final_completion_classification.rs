//! Repair1: actual error enums, whole persistent table graphs, real SCRAM roles.
use super::*;
use safeflow_storage::StorageError;

pub(super) async fn persistent_snapshot(v: &Setup) -> Value {
    let tables: Vec<String> = sqlx::query_scalar("SELECT c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1 AND c.relkind='r' ORDER BY c.relname")
        .bind(&v.db.namespace).fetch_all(&v.db.scoped).await.unwrap();
    let mut graph=serde_json::Map::new();
    for name in tables {
        let q=format!("SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text),'[]'::jsonb) FROM \"{}\".\"{}\" t",v.db.namespace.replace('"',"\"\""),name.replace('"',"\"\""));
        let rows:Value=sqlx::query_scalar(&q).fetch_one(&v.db.scoped).await.unwrap();graph.insert(name,rows);
    }
    Value::Object(graph)
}
async fn close(v:Setup,w:PaymentBackgroundActor,wp:sqlx::PgPool,p:PaymentBackgroundActor,pp:sqlx::PgPool) {
    w.close().await;wp.close().await;p.close().await;pp.close().await;v.server.stop();v.db.finish().await;
}
async fn definition(v:&Setup,name:&str,args:&str)->String {
    sqlx::query_scalar("SELECT pg_get_functiondef(to_regprocedure($1))")
        .bind(format!("{}.{}({})",v.db.namespace,name,args)).fetch_one(&v.db.scoped).await.unwrap()
}

#[tokio::test]
#[ignore="owned PG: real recover SQLSTATE/RAISE origin and unrelated database faults"]
async fn background_final_repair1_database_errors_are_not_reconcile() {
    let (v,gid)=accepted().await;let (p,pp,_)=actor(&v,PaymentBackgroundRole::Producer).await;
    let (w,wp,role)=enabled_worker(&v).await;
    p.record_explicit_decision(&gid,"repair1-db-negative",PaymentSimulationDecision::SettleLocalSimulation).await.unwrap();
    let completed=w.complete_original(&gid).await.unwrap();
    let original=definition(&v,"payment_completion_recover_v1","text").await;
    let before=persistent_snapshot(&v).await;
    for (mode,body) in [
        ("ordinary-p0001","RAISE EXCEPTION 'OWNED_UNRELATED_FAILURE';"),
        ("prefix-is-not-contract","RAISE EXCEPTION 'COMPLETION_RECONCILE_REQUIRED_EXTRA';"),
        ("permission-42501","RAISE EXCEPTION 'COMPLETION_RECONCILE_REQUIRED' USING ERRCODE='42501';"),
        ("cancel-57014","RAISE EXCEPTION 'COMPLETION_RECONCILE_REQUIRED' USING ERRCODE='57014';"),
        ("division-22012","PERFORM 1/0;"),
        ("wrong-origin","PERFORM repair1_unrelated_fault();"),
    ] {
        sqlx::raw_sql("CREATE OR REPLACE FUNCTION repair1_unrelated_fault() RETURNS VOID LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'COMPLETION_RECONCILE_REQUIRED'; END $$")
            .execute(&v.db.scoped).await.unwrap();
        sqlx::raw_sql(&format!("CREATE OR REPLACE FUNCTION payment_completion_recover_v1(gid TEXT) RETURNS JSONB LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path FROM CURRENT AS $$ BEGIN {body} RETURN NULL; END $$"))
            .execute(&v.db.scoped).await.unwrap();
        assert_eq!(w.read_completed_original(&gid).await.unwrap_err(),StorageError::Database,"read {mode}");
        assert_eq!(w.complete_original(&gid).await.unwrap_err(),StorageError::Database,"complete {mode}");
        assert_eq!(before,persistent_snapshot(&v).await,"{mode}");
        sqlx::raw_sql(&original).execute(&v.db.scoped).await.unwrap();
        assert_eq!(Some(completed.clone()),w.read_completed_original(&gid).await.unwrap());
        println!("repair1 unrelated SQL fault stays Database, graph unchanged: {mode}");
    }
    // Actual EXECUTE denial, not a forged 42501, follows the same public calls.
    sqlx::raw_sql(&format!("REVOKE EXECUTE ON FUNCTION payment_completion_recover_v1(TEXT) FROM \"{role}\""))
        .execute(&v.db.scoped).await.unwrap();
    assert_eq!(w.read_completed_original(&gid).await.unwrap_err(),StorageError::Database);
    assert_eq!(w.complete_original(&gid).await.unwrap_err(),StorageError::Database);
    assert_eq!(before,persistent_snapshot(&v).await);
    sqlx::raw_sql(&format!("GRANT EXECUTE ON FUNCTION payment_completion_recover_v1(TEXT) TO \"{role}\""))
        .execute(&v.db.scoped).await.unwrap();
    assert_eq!(completed,w.complete_original(&gid).await.unwrap());
    w.close().await;
    assert_eq!(w.read_completed_original(&gid).await.unwrap_err(),StorageError::Database);
    assert_eq!(w.complete_original(&gid).await.unwrap_err(),StorageError::Database);
    assert_eq!(before,persistent_snapshot(&v).await);
    println!("repair1 closed actual worker pool stays Database, graph unchanged");
    close(v,w,wp,p,pp).await;
}

#[tokio::test]
#[ignore="owned PG: final begin/finish/guard faults remain database faults and no terminal writes"]
async fn background_final_repair1_write_boundary_database_errors() {
    for (name,args,tail) in [
        ("payment_completion_begin_v1","gid TEXT, lease BIGINT","bigint"),
        ("payment_completion_finish_v1","gid TEXT, lease BIGINT","bigint"),
        ("payment_completion_final_guard_v1","gid TEXT, lease BIGINT","bigint"),
    ] {
        let (v,gid)=accepted().await;let (p,pp,_)=actor(&v,PaymentBackgroundRole::Producer).await;
        let (w,wp,_)=enabled_worker(&v).await;
        p.record_explicit_decision(&gid,"repair1-write-negative",PaymentSimulationDecision::SettleLocalSimulation).await.unwrap();
        let original=definition(&v,name,&format!("text,{tail}")).await;
        let before=snapshot(&v).await;
        let ret=if name=="payment_completion_final_guard_v1" {"VOID"}else{"JSONB"};
        sqlx::raw_sql(&format!("CREATE OR REPLACE FUNCTION {name}({args}) RETURNS {ret} LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$ BEGIN RAISE EXCEPTION 'OWNED_UNRELATED_WRITE_FAILURE'; END $$"))
            .execute(&v.db.scoped).await.unwrap();
        assert_eq!(w.complete_original(&gid).await.unwrap_err(),StorageError::Database,"{name}");
        assert_eq!(before,snapshot(&v).await);assert_pending(&v).await;
        sqlx::raw_sql(&original).execute(&v.db.scoped).await.unwrap();
        sqlx::query("UPDATE payment_completion_candidate_jobs SET lease_epoch=lease_epoch+1,leased_at=clock_timestamp()-interval '1 minute',lease_until=clock_timestamp()-interval '30 seconds'")
            .execute(&v.db.scoped).await.unwrap();
        assert_eq!(w.complete_original(&gid).await.unwrap()["state"],"COMPLETED");
        println!("repair1 write boundary preserves Database and rolls back original: {name}");
        close(v,w,wp,p,pp).await;
    }
}
