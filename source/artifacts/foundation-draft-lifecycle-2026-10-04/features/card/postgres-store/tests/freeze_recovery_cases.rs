mod freeze_support;
macro_rules! database_case {
    ($name:ident) => {
        #[tokio::test]
        #[ignore = "requires a dedicated loopback safeflow_card_store_test database"]
        async fn $name() {
            freeze_support::run_case(stringify!($name))
                .await
                .expect("freeze original recovery PostgreSQL acceptance");
        }
    };
}
database_case!(discarded_result_new_connection_recovers_original);
database_case!(released_original_is_history_not_current_state);
database_case!(history_is_owner_and_account_scoped);
database_case!(missing_original_lookup_is_read_only);
database_case!(database_error_is_not_missing_history);
database_case!(broken_join_is_not_missing_history);
database_case!(original_operation_reason_and_receipt_are_immutable);
