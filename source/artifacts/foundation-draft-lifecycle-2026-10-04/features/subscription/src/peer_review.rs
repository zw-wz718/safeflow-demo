//! Diagnostic review of a transfer-owner mock. Never converts it into C-TXN,
//! authenticates its caller, infers card ownership, or generates subscriptions.
use crate::amount_minor_units;
use chrono::DateTime;
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct TransferFixtureReview {
    pub mode: &'static str,
    pub recognition_executed: bool,
    pub production_registration: bool,
    pub direct_subscription_input_compatible: bool,
    pub amount_minor_units: u64,
    pub receipt_states: [&'static str; 4],
    pub completed_ledger_type: &'static str,
    pub completed_ledger_direction: &'static str,
    pub card_reference_present: bool,
    pub merchant_reference_present: bool,
    pub unresolved: Vec<&'static str>,
    pub cautions: [&'static str; 4],
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str().filter(|s| !s.trim().is_empty())
}

/// Checks the pinned mock proposal's internal consistency, not bank authenticity.
/// A changed proposal must be reviewed rather than silently adapted.
pub fn inspect_transfer_fixture(json: &str) -> Result<TransferFixtureReview, &'static str> {
    let source: Value = serde_json::from_str(json).map_err(|_| "PEER_FIXTURE_PARSE_FAILED")?;
    let invalid = "PEER_FIXTURE_SHAPE_OR_FACTS_CHANGED";
    let request = source.get("request").ok_or(invalid)?;
    let ledger = source.get("success_debit_transaction").ok_or(invalid)?;
    let cents = text(request, "amount")
        .and_then(amount_minor_units)
        .ok_or(invalid)?;
    if source.get("proposal_version").and_then(Value::as_u64) != Some(1)
        || text(&source, "authenticated_user_id").is_none()
        || source
            .get("clock_unix_seconds")
            .and_then(Value::as_i64)
            .is_none()
        || text(request, "currency") != Some("CNY")
        || request
            .get("draft_version")
            .and_then(Value::as_u64)
            .is_none_or(|v| v == 0)
    {
        return Err(invalid);
    }
    for key in [
        "task_id",
        "operation_id",
        "draft_id",
        "payer_account_id",
        "payee_id",
    ] {
        if text(request, key).is_none() {
            return Err(invalid);
        }
    }
    for (name, status, reason) in [
        ("success", "SUCCEEDED", None),
        ("processing", "PROCESSING", Some("AWAITING_BANK_RESULT")),
        ("failure", "FAILED", Some("BANK_DECLINED")),
        ("lost_reply", "PROCESSING", Some("OUTCOME_UNKNOWN")),
    ] {
        let receipt = source.get(name).ok_or(invalid)?;
        for key in [
            "task_id",
            "operation_id",
            "draft_id",
            "draft_version",
            "payer_account_id",
            "payee_id",
            "amount",
            "currency",
        ] {
            if receipt.get(key) != request.get(key) {
                return Err(invalid);
            }
        }
        if receipt.get("proposal_version").and_then(Value::as_u64) != Some(1)
            || text(receipt, "status") != Some(status)
            || receipt.get("reason_code")
                != Some(&reason.map_or(Value::Null, |s| Value::String(s.into())))
        {
            return Err(invalid);
        }
        if name == "success" {
            if text(receipt, "completed_at")
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .is_none()
                || receipt.get("completed_at") != ledger.get("completed_at")
            {
                return Err(invalid);
            }
        } else if receipt.get("completed_at") != Some(&Value::Null) {
            return Err(invalid);
        }
    }
    if text(ledger, "transaction_id").is_none()
        || ledger.get("operation_id") != request.get("operation_id")
        || ledger.get("account_id") != request.get("payer_account_id")
        || ledger.get("amount") != request.get("amount")
        || ledger.get("currency") != request.get("currency")
        || text(ledger, "transaction_type") != Some("TRANSFER")
        || text(ledger, "direction") != Some("DEBIT")
        || text(ledger, "status") != Some("COMPLETED")
        || ledger.get("card_id") != Some(&Value::Null)
        || ledger.get("merchant_id") != Some(&Value::Null)
    {
        return Err(invalid);
    }
    let mut unresolved = vec![
        "DEBIT_CREDIT_TO_OUT_IN_MAPPING_REQUIRES_AGREEMENT",
        "RECEIPT_STATUS_IS_NOT_LEDGER_STATUS",
        "NO_QUERY_COMPLETENESS_OR_OWNERSHIP_PROOF",
        "NO_SUBSCRIPTION_SERVICE_REFERENCE",
    ];
    for (key, issue) in [
        ("category_code", "CATEGORY_CODE_MISSING_FROM_SOURCE"),
        ("original_tx_id", "ORIGINAL_TX_ID_MISSING_FROM_SOURCE"),
    ] {
        if ledger.get(key).is_none() {
            unresolved.push(issue);
        } else {
            // New fields are a source change, not automatically approved defaults.
            return Err(invalid);
        }
    }
    Ok(TransferFixtureReview {
        mode: "OFFLINE_PEER_FIXTURE_REVIEW_ONLY",
        recognition_executed: false,
        production_registration: false,
        direct_subscription_input_compatible: false,
        amount_minor_units: cents,
        receipt_states: ["SUCCEEDED", "PROCESSING", "FAILED", "PROCESSING"],
        completed_ledger_type: "TRANSFER",
        completed_ledger_direction: "DEBIT",
        card_reference_present: false,
        merchant_reference_present: false,
        unresolved,
        cautions: [
            "TRANSFER_IS_NOT_SUBSCRIPTION_CONSUMPTION",
            "NULL_CARD_DOES_NOT_AUTHORIZE_CARD_LOOKUP_OR_FREEZE",
            "UNKNOWN_RECEIPT_DOES_NOT_PROVE_NO_COMPLETED_LEDGER",
            "FIXTURE_USER_ID_IS_NOT_AUTHENTICATION",
        ],
    })
}
