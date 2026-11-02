//! Replay two independent fictional inputs; never persists registrations.
use safeflow_subscription_evidence::{
    billing_query_mock::load_case,
    managed_ledger_mock::{load_ledger, review_mock},
    MockContext,
};
use serde_json::{json, Value};
fn run() -> Result<Value, &'static str> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 2 {
        return Err("USAGE: review_ledger_mock BILLING_CAPTURE LEDGER_CASES");
    }
    let billing = std::fs::read_to_string(&args[0]).map_err(|_| "BILLING_READ_FAILED")?;
    let ledger = std::fs::read_to_string(&args[1]).map_err(|_| "LEDGER_READ_FAILED")?;
    let fixtures: Value = serde_json::from_str(&ledger).map_err(|_| "LEDGER_CASES_INVALID")?;
    if fixtures["schema_version"] != 1
        || fixtures["fictional"] != true
        || fixtures["production_registered"] != false
    {
        return Err("LEDGER_CASES_INVALID");
    }
    let cases = fixtures["cases"]
        .as_array()
        .filter(|c| !c.is_empty())
        .ok_or("LEDGER_CASES_INVALID")?;
    let mut names = std::collections::HashSet::new();
    let mut reports = Vec::new();
    for c in cases {
        let name = c["case_id"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or("CASE_ID_INVALID")?;
        if !names.insert(name) {
            return Err("CASE_ID_INVALID");
        }
        let source = c["consumption_case"]
            .as_str()
            .ok_or("CONSUMPTION_CASE_MISSING")?;
        let sample = load_case(
            &billing,
            source,
            MockContext {
                user_id: "user_demo".into(),
                requested_account_ids: vec!["account_001".into()],
            },
        )?;
        let ledger = load_ledger(&c["ledger"].to_string())?;
        reports.push(json!({"case_id":name,"report":review_mock(&sample.trusted_context_fixture,&sample.snapshot,&ledger)}));
    }
    Ok(json!({"fictional":true,"production_registered":false,"reports":reports}))
}
fn main() -> std::process::ExitCode {
    match run() {
        Ok(output) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&output).expect("serializable")
            );
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::from(2)
        }
    }
}
