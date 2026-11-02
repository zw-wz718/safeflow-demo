use safeflow_bank_gateway::{
    DecimalAmount, FindPayeesInput, GetAccountBalanceInput, GetAccountBalanceOutput,
};
use safeflow_contracts::ResourceId;
use serde_json::json;

#[test]
fn amounts_are_canonical_strings_and_reject_numbers() {
    for value in ["0.00", "200.00", "1288.00", "9999999999999999999999.99"] {
        let amount = DecimalAmount::parse(value).unwrap();
        assert_eq!(serde_json::to_value(&amount).unwrap(), json!(value));
        assert_eq!(
            serde_json::from_value::<DecimalAmount>(json!(value)).unwrap(),
            amount
        );
    }
    for value in [json!(200), json!(200.0), json!(null), json!(true)] {
        assert!(serde_json::from_value::<DecimalAmount>(value).is_err());
    }
    for value in [
        "", "200", "200.0", "200.000", "01.00", "-1.00", "+1.00", "1e2", " 1.00", "1.00 ", "１.00",
        ".00",
    ] {
        assert!(DecimalAmount::parse(value).is_err(), "accepted {value:?}");
    }
}

#[test]
fn query_inputs_reject_injected_identity_and_control_fields() {
    for field in [
        "user_id",
        "authenticated_user_id",
        "session_ref",
        "approved",
        "verified",
        "confirmation_ref",
        "auth_grant_ref",
        "permit",
        "amount",
    ] {
        let mut balance = json!({"account_id": "account_001"});
        balance[field] = json!("injected");
        assert!(serde_json::from_value::<GetAccountBalanceInput>(balance).is_err());
        let mut payees = json!({"query": "小李"});
        payees[field] = json!("injected");
        assert!(serde_json::from_value::<FindPayeesInput>(payees).is_err());
    }
}

#[test]
fn resource_and_query_limits_are_checked_without_silent_normalization() {
    for value in [
        "a b".to_owned(),
        "a\0b".to_owned(),
        "a\nb".to_owned(),
        "x".repeat(129),
    ] {
        let input = GetAccountBalanceInput {
            account_id: ResourceId::parse(value).unwrap(),
        };
        assert!(input.validate().is_err());
    }
    assert!(GetAccountBalanceInput {
        account_id: ResourceId::parse("x".repeat(128)).unwrap()
    }
    .validate()
    .is_ok());
    for query in [
        "".to_owned(),
        " ".to_owned(),
        " 小李".to_owned(),
        "小李 ".to_owned(),
        "小\n李".to_owned(),
        "小\0李".to_owned(),
        "李".repeat(65),
    ] {
        assert!(FindPayeesInput { query }.validate().is_err());
    }
    assert!(FindPayeesInput {
        query: "李".repeat(64)
    }
    .validate()
    .is_ok());
}

#[test]
fn output_deserialization_rejects_float_balances_and_other_currencies() {
    let output = json!({
        "account": {"account_id": "account_001", "display_name": "工资卡", "masked_account_no": "**** 6628", "currency": "CNY"},
        "available_balance": "1288.00",
        "as_of": "2026-10-01T14:30:00+08:00"
    });
    assert!(serde_json::from_value::<GetAccountBalanceOutput>(output.clone()).is_ok());
    let mut numeric = output.clone();
    numeric["available_balance"] = json!(1288.0);
    assert!(serde_json::from_value::<GetAccountBalanceOutput>(numeric).is_err());
    let mut other_currency = output;
    other_currency["account"]["currency"] = json!("USD");
    assert!(serde_json::from_value::<GetAccountBalanceOutput>(other_currency).is_err());
}
