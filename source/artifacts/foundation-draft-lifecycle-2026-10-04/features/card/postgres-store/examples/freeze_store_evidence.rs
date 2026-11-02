#[path = "../tests/freeze_support/mod.rs"]
mod freeze_support;

#[tokio::main]
async fn main() {
    let mut cases = Vec::new();
    for case in freeze_support::SAVE_CASES
        .iter()
        .chain(freeze_support::RECOVERY_CASES.iter())
        .chain(freeze_support::TRANSACTION_CASES.iter())
    {
        match freeze_support::run_case(case).await {
            Ok(result) => cases.push(result),
            Err(_) => {
                eprintln!("CARD_FREEZE_SQL_EVIDENCE_FAILED");
                std::process::exit(1);
            }
        }
    }
    let report = serde_json::json!({
        "scope":"ACTUAL_CARD_FREEZE_STORE_ISOLATED_POSTGRESQL",
        "fictional_source_data":true,"actual_store_methods_executed":true,
        "groups":{"persistence":8,"recovery":7,"transactions":8},
        "public_migrations_installed":false,"public_permission_or_runtime_connected":false,
        "funds_store_or_real_payment_executed":false,"process_or_database_restart_tested":false,
        "network_response_loss_tested":false,"offline_release_entry_enabled":false,
        "cases":cases
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("freeze fixture JSON")
    );
}
