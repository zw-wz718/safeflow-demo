mod support;
macro_rules! database_case {
    ($name:ident) => {
        #[tokio::test]
        #[ignore = "requires a dedicated loopback safeflow_card_store_test database"]
        async fn $name() {
            support::run_case(stringify!($name))
                .await
                .expect("actual card store PG case");
        }
    };
}
database_case!(catalog_persists_and_is_idempotent);
database_case!(missing_catalog_differs_from_complete_empty);
database_case!(ownership_and_missing_account_rejected);
database_case!(invalid_catalog_has_no_partial_initialization);
database_case!(register_card_advances_version_and_rejects_conflicts);
database_case!(multiple_sources_and_released_history);
database_case!(lock_change_then_restore_advances_version);
database_case!(failed_save_rolls_back_catalog_and_version);
database_case!(account_guard_blocks_a_later_lock_insert);
database_case!(database_failure_is_not_empty);
database_case!(mapping_and_owner_rewrite_are_disabled);
database_case!(caller_transaction_and_isolation_required);
database_case!(all_cards_and_locks_without_truncation);
database_case!(active_lock_unique_constraints);
