mod freeze_support;
macro_rules! database_case {
    ($name:ident) => {
        #[tokio::test]
        #[ignore = "requires a dedicated loopback safeflow_card_store_test database"]
        async fn $name() {
            freeze_support::run_case(stringify!($name))
                .await
                .expect("freeze rollback and protection PostgreSQL acceptance");
        }
    };
}
database_case!(failure_after_lock_insert_rolls_back_all);
database_case!(failure_after_reason_insert_rolls_back_all);
database_case!(caller_rollback_removes_successful_save);
database_case!(incomplete_operation_cannot_commit);
database_case!(freeze_first_account_reader_waits);
database_case!(account_reader_first_freeze_waits);
database_case!(concurrent_same_operation_keeps_one_result);
database_case!(concurrent_same_event_keeps_one_lock);
