use safeflow_bank_gateway::{BankError, GetAccountBalanceOutput};
use safeflow_contracts::ResourceId;
use serde_json::json;

fn balance_with_timestamp(as_of: &str) -> GetAccountBalanceOutput {
    serde_json::from_value(json!({
        "account": {
            "account_id": "account_001",
            "display_name": "Account",
            "masked_account_no": "**** 6628",
            "currency": "CNY"
        },
        "available_balance": "1288.00",
        "as_of": as_of
    }))
    .unwrap()
}

#[test]
fn bank_response_requires_a_real_rfc3339_timestamp_with_timezone() {
    let account_id = ResourceId::parse("account_001").unwrap();
    for invalid in [
        "not-a-date",
        "2026-10-03T12:00:00",
        "2026-02-30T12:00:00+08:00",
        "2026-13-01T12:00:00Z",
        "2026-10-03T25:00:00Z",
        "2026-10-03T12:00:00+25:00",
    ] {
        assert_eq!(
            balance_with_timestamp(invalid).validate_for(&account_id),
            Err(BankError::InvalidResponse),
            "accepted invalid timestamp: {invalid}"
        );
    }
}

#[test]
fn bank_response_accepts_rfc3339_offsets_and_fractional_seconds() {
    let account_id = ResourceId::parse("account_001").unwrap();
    for valid in [
        "2026-10-03T12:00:00Z",
        "2026-10-03T20:00:00+08:00",
        "2024-02-29T12:00:00.123-05:30",
    ] {
        assert!(balance_with_timestamp(valid)
            .validate_for(&account_id)
            .is_ok());
    }
}
