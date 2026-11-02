use super::*;
use safeflow_transfer_integration::StoredDraft;

struct Readback {
    store: Arc<MockDraftStore<SystemClock>>,
    fault: u8,
}
impl LifecycleDraftObserver for Readback {
    fn observe<'a>(
        &'a self,
        context: &'a ExecutionContext,
    ) -> ContractFuture<'a, Result<Option<LifecycleDraftObservation>, StorageError>> {
        Box::pin(async move { StoreObserver(self.store.clone()).observe(context).await })
    }
    fn stored<'a>(
        &'a self,
        context: &'a ExecutionContext,
        id: &'a ResourceId,
    ) -> ContractFuture<'a, Result<StoredDraft, StorageError>> {
        Box::pin(async move {
            if self.fault == 0 {
                return Err(StorageError::DependencyUnavailable);
            }
            let mut actual = self
                .store
                .read(context, id)
                .await
                .map_err(|_| StorageError::DependencyUnavailable)?;
            match self.fault {
                1 => actual.user_id = safeflow_contracts::UserId::parse("user_002").unwrap(),
                2 => actual.task_id = new_task_id(),
                3 => actual.plan_version += 1,
                4 => actual.operation_id = new_operation_id(),
                5 => actual.expires_at += 60,
                6 => actual.details.as_mut().unwrap().remark = "changed after receipt".into(),
                _ => (),
            }
            Ok(actual)
        })
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual Runtime draft readback"]
async fn sql_bridge_missing_or_mismatched_complete_readback_never_settles_display_fields() {
    for fault in 0..7 {
        let db = Db::new().await;
        let confirmed = db.confirmed().await;
        let mut service = db.service().with_transfer_sql_bridge_for_backend();
        service.observer = Arc::new(Readback {
            store: db.store.clone(),
            fault,
        });
        let result = service
            .execute(
                &session(),
                &confirmed.task.task_id,
                &confirmed.operation.operation_id,
                &confirmed.confirmation.as_ref().unwrap().confirmation_ref,
                &version(&confirmed),
            )
            .await;
        assert!(
            result.is_err(),
            "invalid full readback {fault} was accepted"
        );
        assert_eq!(
            db.calls(),
            1,
            "Runtime and actual memory action completed before readback rejection"
        );
        assert_eq!(db.claim_status().await, vec!["CLAIMED"]);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM transfer_drafts_v1")
            .fetch_one(&db.sql)
            .await
            .unwrap();
        assert_eq!(count, 0);
        let view = service
            .query(
                &session(),
                &confirmed.task.task_id,
                Some(&confirmed.operation.operation_id),
            )
            .await
            .unwrap();
        assert!(view.receipt.is_none() && view.unknown_operation.is_some());
        let repeat = service
            .execute(
                &session(),
                &confirmed.task.task_id,
                &confirmed.operation.operation_id,
                &confirmed.confirmation.as_ref().unwrap().confirmation_ref,
                &version(&confirmed),
            )
            .await
            .unwrap();
        assert!(repeat.receipt.is_none());
        assert_eq!(db.calls(), 1);
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual Runtime draft readback"]
async fn sql_bridge_opt_in_requires_full_source_legacy_constructor_does_not_backfill_history() {
    let db = Db::new().await;
    let confirmed = db.confirmed().await;
    // Existing observer supplies only display fields. Opt-in may not silently fall back.
    let bridge = db.service().with_transfer_sql_bridge_for_backend();
    assert_eq!(
        bridge
            .execute(
                &session(),
                &confirmed.task.task_id,
                &confirmed.operation.operation_id,
                &confirmed.confirmation.as_ref().unwrap().confirmation_ref,
                &version(&confirmed)
            )
            .await
            .err(),
        Some(StorageError::DependencyUnavailable)
    );
    assert_eq!(db.claim_status().await, vec!["CLAIMED"]);
    let old = db.confirmed().await;
    let legacy = db.service();
    let success = legacy
        .execute(
            &session(),
            &old.task.task_id,
            &old.operation.operation_id,
            &old.confirmation.as_ref().unwrap().confirmation_ref,
            &version(&old),
        )
        .await
        .unwrap();
    assert!(success.receipt.is_some());
    let replay = bridge
        .execute(
            &session(),
            &old.task.task_id,
            &old.operation.operation_id,
            &old.confirmation.as_ref().unwrap().confirmation_ref,
            &version(&old),
        )
        .await
        .unwrap();
    assert_eq!(replay.receipt, success.receipt);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM transfer_drafts_v1")
        .fetch_one(&db.sql)
        .await
        .unwrap();
    assert_eq!(
        rows, 0,
        "historical memory-only success must never be backfilled on replay"
    );
    assert_eq!(db.calls(), 2);
    db.finish().await;
}
