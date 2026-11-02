#[path = "../examples/support/subscription_handoff_mock.rs"]
mod handoff;
use handoff::{run_mock, Scenario};
use serde_json::{json, Value};

fn step<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["step"] == name)
        .unwrap()
}

#[test]
fn unchanged_source_needs_date_input_instead_of_an_invented_future_date() {
    let r = run_mock(Scenario::Complete).unwrap();
    assert_eq!(
        step(&r, "consumption_read")["result"]["window"]["as_of"],
        "2026-10-13T12:00:00+08:00"
    );
    assert_eq!(
        step(&r, "date_preview_requires_input")["result"]["error"],
        "DATE_NEEDS_INPUT"
    );
    let settings = &step(&r, "reminder_settings_confirmation")["result"];
    assert_eq!(settings["date_source"], "EXPLICIT_FICTIONAL_USER_FORM");
    assert_eq!(settings["receipt"]["expected_date"], "2026-10-17");
    assert_eq!(
        step(&r, "next_cycle_keeps_original_anchor")["result"]["schedule"]["expected_date"],
        "2026-11-15"
    );
}

#[test]
fn failed_source_and_unavailable_ledger_never_progress_to_registration() {
    for scenario in [Scenario::SourceIncomplete, Scenario::LedgerUnavailable] {
        let r = run_mock(scenario).unwrap();
        assert!(r["simulated_messages"].as_array().unwrap().is_empty());
        assert!(r["steps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["subscription_count"].is_null() || s["subscription_count"] == 0));
        assert!(!r["steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["step"] == "registration_confirmation"));
    }
    let r = run_mock(Scenario::LedgerUnavailable).unwrap();
    assert!(step(&r, "ledger_and_discovery")["subscription_count"].is_null());
}

#[test]
fn unknown_service_cannot_become_a_registered_subscription_or_reminder() {
    let r = run_mock(Scenario::ServiceUnknown).unwrap();
    assert_eq!(r["stopped_at"], "service_review");
    assert_eq!(r["simulated_messages"], json!([]));
    let discovery = step(&r, "ledger_and_discovery");
    assert_eq!(discovery["subscription_count"], 0);
    assert_eq!(
        discovery["result"]["groups"][0]["group"]["status"],
        "NEEDS_SERVICE_EVIDENCE"
    );
}

#[test]
fn registration_and_reminder_confirmation_are_independent_gates() {
    let rejected = run_mock(Scenario::RegistrationUnconfirmed).unwrap();
    assert_eq!(rejected["stopped_at"], "registration_confirmation");
    assert_eq!(
        step(&rejected, "registration_confirmation")["subscription_count"],
        0
    );
    assert_eq!(rejected["simulated_messages"], json!([]));
    let settings = run_mock(Scenario::SettingsUnconfirmed).unwrap();
    assert_eq!(settings["stopped_at"], "reminder_settings_confirmation");
    let stopped = step(&settings, "reminder_settings_confirmation");
    assert_eq!(stopped["subscription_count"], 1);
    assert_eq!(stopped["result"]["error"], "CONFIRMATION_REQUIRED");
    assert_eq!(stopped["result"]["scan"]["items"], json!([]));
    assert_eq!(settings["simulated_messages"], json!([]));
}

#[test]
fn full_chain_retries_only_clear_failure_and_deduplicates_both_reminder_kinds() {
    let r = run_mock(Scenario::Complete).unwrap();
    assert_eq!(r["stopped_at"], "complete");
    for (name, state, attempts) in [
        ("ordinary_first_attempt", "RETRY_NEXT_DAY", 1),
        ("ordinary_same_day_repeat", "RETRY_WAITING", 1),
        ("ordinary_next_day", "SIMULATED_SUCCESS", 2),
        ("ordinary_success_repeat", "ALREADY_SIMULATED", 2),
        ("late_cancel_check", "SIMULATED_SUCCESS", 3),
        ("late_cancel_check_repeat", "ALREADY_SIMULATED", 3),
    ] {
        let s = step(&r, name);
        assert_eq!(s["result"]["items"][0]["state"], state, "{name}");
        assert_eq!(s["simulated_delivery_attempts"], attempts, "{name}");
    }
    assert_eq!(
        step(&r, "late_cancel_check")["result"]["items"][0]["late"],
        true
    );
    let messages = r["simulated_messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0], messages[1]);
    assert_eq!(messages[2]["kind"], "CANCELLATION_CHECK");
    assert_eq!(messages[0]["cycle_ref"], messages[2]["cycle_ref"]);
    let last = step(&r, "registration_history_and_current_state");
    assert_eq!(last["subscription_count"], 1);
    assert_eq!(last["registration_event_count"], 1);
    assert_eq!(last["intent_event_count"], 2);
    assert_eq!(last["result"]["original_receipt"]["state"], "ACTIVE");
    assert_eq!(last["result"]["current_ledger"][0]["status"], "ACTIVE");
    assert_eq!(
        step(&r, "record_cancel_intent")["result"]["ledger_state_at_operation"],
        "CANCELLING"
    );
}

#[test]
fn unknown_outcome_is_never_automatically_retried_the_next_day() {
    let r = run_mock(Scenario::DeliveryUnknown).unwrap();
    assert_eq!(r["stopped_at"], "delivery_result_review");
    assert_eq!(r["simulated_messages"].as_array().unwrap().len(), 1);
    for name in [
        "ordinary_first_attempt",
        "ordinary_same_day_repeat",
        "ordinary_next_day",
    ] {
        assert_eq!(
            step(&r, name)["result"]["items"][0]["state"],
            "UNKNOWN_NEEDS_REVIEW"
        );
        assert_eq!(step(&r, name)["simulated_delivery_attempts"], 1);
    }
}

#[test]
fn every_replayed_case_explicitly_denies_production_and_merchant_actions() {
    let report = handoff::replay_mock().unwrap();
    assert_eq!(report["case_count"], 7);
    for case in report["cases"].as_array().unwrap() {
        assert_eq!(case["fictional"], true);
        for flag in [
            "production_registered",
            "notification_sent",
            "database_connected",
            "audit_integrated",
            "security_kernel_connected",
            "merchant_action_executed",
            "merchant_subscription_cancelled",
            "merchant_service_restored",
        ] {
            assert_eq!(case[flag], false, "{}: {flag}", case["name"]);
        }
    }
}
