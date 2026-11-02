//! Local read-only normalization of BILL-01 output, not authentication or a
//! complete consumption snapshot. Never upgrades source evidence availability.
use crate::amount_minor_units;
use chrono::DateTime;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashSet;

#[derive(Clone, Debug, Serialize, PartialEq, Eq, Hash)]
pub struct SourceKey {
    pub source_id: String,
    pub account_id: String,
    pub transaction_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BillRead {
    source_id: String,
    entries: Vec<BillEntry>,
    added: usize,
    unchanged: usize,
    excluded_out_of_scope: usize,
    subscription_evidence: Unavailable,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum Unavailable {
    Unavailable,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BillEntry {
    transaction: SourceTransaction,
    consumption: Assessment,
    card_note: Option<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum Assessment {
    TransferExcluded,
    UnassessedSourceType,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceTransaction {
    transaction_id: String,
    operation_id: String,
    account_id: String,
    #[serde(deserialize_with = "required_nullable")]
    card_id: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    merchant_id: Option<String>,
    transaction_type: String,
    direction: SourceDirection,
    amount: String,
    currency: String,
    status: String,
    completed_at: String,
    // Optional local input extension. BILL-01 currently provides no category.
    category_code: Option<String>,
}

fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum SourceDirection {
    Debit,
    Credit,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ConsumptionCheck {
    TransferExcluded,
    ConsumptionOutgoingNeedsSourceVerification,
    NotOutgoingConsumption,
    UnassessedSourceType,
}

#[derive(Debug, Serialize)]
pub struct NormalizedTransaction {
    pub key: SourceKey,
    pub operation_id: String,
    pub card_id: Option<String>,
    pub merchant_id: Option<String>,
    pub category_code: Option<String>,
    pub transaction_type: String,
    pub direction: &'static str,
    pub original_amount: String,
    pub canonical_amount: String,
    pub amount_minor_units: u64,
    pub currency: String,
    pub status: String,
    pub completed_at: String,
    pub consumption_check: ConsumptionCheck,
}

#[derive(Debug, Serialize)]
pub struct NormalizedRead {
    pub mode: &'static str,
    pub source_id: String,
    pub entries: Vec<NormalizedTransaction>,
    pub source_added: usize,
    pub source_unchanged: usize,
    pub excluded_out_of_scope: usize,
    pub subscription_evidence: &'static str,
    pub recognition_executed: bool,
    pub production_registered: bool,
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Validates the entire file before returning anything. Source/user authority,
/// requested accounts, window and completeness must be supplied by a future
/// approved boundary. This function cannot create a recognition Snapshot.
pub fn normalize_bill_read(json: &str) -> Result<NormalizedRead, &'static str> {
    let input: BillRead = serde_json::from_str(json).map_err(|_| "BILL_READ_PARSE_FAILED")?;
    if !valid_id(&input.source_id) {
        return Err("INVALID_SOURCE_ID");
    }
    let mut keys = HashSet::new();
    let mut entries = Vec::new();
    for entry in input.entries {
        let tx = entry.transaction;
        if [
            &tx.transaction_id,
            &tx.account_id,
            &tx.operation_id,
            &tx.transaction_type,
        ]
        .iter()
        .any(|s| !valid_id(s))
            || [&tx.card_id, &tx.merchant_id, &tx.category_code]
                .iter()
                .any(|s| s.as_ref().is_some_and(|v| !valid_id(v)))
        {
            return Err("INVALID_REFERENCE");
        }
        if tx.currency != "CNY" || tx.status != "COMPLETED" {
            return Err("UNSUPPORTED_CURRENCY_OR_STATUS");
        }
        if DateTime::parse_from_rfc3339(&tx.completed_at).is_err() {
            return Err("INVALID_COMPLETION_TIME");
        }
        let (whole, fraction) = tx.amount.split_once('.').ok_or("INVALID_AMOUNT")?;
        if whole.is_empty() || !whole.bytes().all(|c| c.is_ascii_digit()) {
            return Err("INVALID_AMOUNT");
        }
        let stripped = whole.trim_start_matches('0');
        let normalized = format!(
            "{}.{}",
            if stripped.is_empty() { "0" } else { stripped },
            fraction
        );
        let cents = amount_minor_units(&normalized).ok_or("INVALID_AMOUNT")?;
        let key = SourceKey {
            source_id: input.source_id.clone(),
            account_id: tx.account_id,
            transaction_id: tx.transaction_id,
        };
        if !keys.insert(key.clone()) {
            return Err("DUPLICATE_SOURCE_KEY");
        }
        if (tx.transaction_type == "TRANSFER")
            != (entry.consumption == Assessment::TransferExcluded)
        {
            return Err("CONTRADICTORY_CONSUMPTION_ASSESSMENT");
        }
        let outgoing = matches!(tx.direction, SourceDirection::Debit);
        let consumption_check = match tx.transaction_type.as_str() {
            "TRANSFER" => ConsumptionCheck::TransferExcluded,
            "CONSUMPTION" if outgoing => {
                ConsumptionCheck::ConsumptionOutgoingNeedsSourceVerification
            }
            "CONSUMPTION" => ConsumptionCheck::NotOutgoingConsumption,
            _ => ConsumptionCheck::UnassessedSourceType,
        };
        // Source display text is not authoritative data and cannot supply a card.
        let _ = entry.card_note;
        entries.push(NormalizedTransaction {
            key,
            operation_id: tx.operation_id,
            card_id: tx.card_id,
            merchant_id: tx.merchant_id,
            category_code: tx.category_code,
            transaction_type: tx.transaction_type,
            direction: if outgoing { "OUT" } else { "IN" },
            original_amount: tx.amount,
            canonical_amount: normalized,
            amount_minor_units: cents,
            currency: tx.currency,
            status: tx.status,
            completed_at: tx.completed_at,
            consumption_check,
        });
    }
    let _ = input.subscription_evidence;
    Ok(NormalizedRead {
        mode: "LOCAL_BILLING_NORMALIZATION_ONLY",
        source_id: input.source_id,
        entries,
        source_added: input.added,
        source_unchanged: input.unchanged,
        excluded_out_of_scope: input.excluded_out_of_scope,
        subscription_evidence: "UNAVAILABLE",
        recognition_executed: false,
        production_registered: false,
    })
}
