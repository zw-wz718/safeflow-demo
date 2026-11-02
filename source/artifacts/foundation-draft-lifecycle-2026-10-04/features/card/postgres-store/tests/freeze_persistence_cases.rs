mod freeze_support;
macro_rules! database_case {
    ($name:ident) => {
        #[tokio::test]
        #[ignore = "requires a dedicated loopback safeflow_card_store_test database"]
        async fn $name() {
            freeze_support::run_case(stringify!($name))
                .await
                .expect("freeze persistence PostgreSQL acceptance");
        }
    };
}
database_case!(first_freeze_saves_complete_records);
database_case!(same_operation_replays_without_writes);
database_case!(same_event_new_operation_reuses_lock);
database_case!(distinct_events_and_scopes_keep_multiple_locks);
database_case!(self_new_reason_keeps_one_active_lock);
database_case!(three_sources_preserve_private_evidence);
database_case!(parameter_and_key_conflicts_write_nothing);
database_case!(foreign_or_missing_card_writes_nothing);
