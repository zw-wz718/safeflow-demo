#[path = "../tests/support/mod.rs"]
mod support;

#[tokio::main]
async fn main() {
    let mut cases = Vec::new();
    for case in support::CASES {
        match support::run_case(case).await {
            Ok(value) => cases.push(value),
            Err(_) => {
                eprintln!("CARD_SQL_STORE_EVIDENCE_FAILED");
                std::process::exit(1);
            }
        }
    }
    let report = serde_json::json!({"scope":"ACTUAL_CARD_CATALOG_STORE_ISOLATED_POSTGRESQL",
        "fictional_source_data":true,"actual_store_methods_executed":true,
        "public_migrations_installed":false,"public_permission_or_runtime_connected":false,
        "freeze_operation_store_implemented":true,"freeze_operation_store_exercised":false,
        "offline_release_entry_enabled":false,
        "cases":cases});
    println!(
        "{}",
        serde_json::to_string_pretty(&report).expect("fixture JSON")
    );
}
