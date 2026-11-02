use serde_json::Value;
use std::process::Command;
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../fixtures/billing/consumption_pages.v1.json"
);

#[test]
fn replay_cli_runs_all_scenarios_and_blocks_partial_evidence() {
    let output = Command::new(env!("CARGO_BIN_EXE_billing-consumption"))
        .args(["--fixture", FIXTURE])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["fictional"], true);
    assert_eq!(result["production_registered"], false);
    assert_eq!(result["cases"].as_array().unwrap().len(), 12);
    let cases = result["cases"].as_array().unwrap();
    let complete = cases
        .iter()
        .find(|case| case["name"] == "complete")
        .unwrap();
    assert_eq!(complete["read"]["state"], "COMPLETE");
    assert_eq!(
        complete["read"]["eligible_consumption"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let partial = cases
        .iter()
        .find(|case| case["name"] == "second-page-failed")
        .unwrap();
    assert_eq!(partial["read"]["state"], "DATA_INCOMPLETE");
    assert!(partial["read"]["eligible_consumption"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(partial["subscription_snapshot"].is_null());
    let foreign = cases
        .iter()
        .find(|case| case["name"] == "foreign-account-row")
        .unwrap();
    assert_eq!(foreign["read"]["state"], "RESOURCE_UNAVAILABLE");
    assert!(foreign["read"]["observed_transactions"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn selected_case_has_no_unrequested_scenarios() {
    let output = Command::new(env!("CARGO_BIN_EXE_billing-consumption"))
        .args(["--fixture", FIXTURE, "--case", "complete"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["cases"].as_array().unwrap().len(), 1);
    assert_eq!(
        result["cases"][0]["subscription_snapshot"]["managed_subscriptions_read"]["state"],
        "UNAVAILABLE"
    );
}

#[test]
fn invalid_case_and_identity_override_fail_without_success_json() {
    for args in [
        vec!["--fixture", FIXTURE, "--case", "missing-case"],
        vec!["--fixture", FIXTURE, "--user", "pretend-admin"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_billing-consumption"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn non_fictional_fixture_is_rejected() {
    let mut value: Value =
        serde_json::from_str(&std::fs::read_to_string(FIXTURE).unwrap()).unwrap();
    value["fictional"] = Value::Bool(false);
    let path = std::env::temp_dir().join(format!(
        "safeflow-invalid-fictional-{}.json",
        std::process::id()
    ));
    std::fs::write(&path, value.to_string()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_billing-consumption"))
        .arg("--fixture")
        .arg(&path)
        .output()
        .unwrap();
    std::fs::remove_file(path).unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}
