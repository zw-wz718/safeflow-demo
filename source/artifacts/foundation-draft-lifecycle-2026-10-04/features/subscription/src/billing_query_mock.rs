//! Fictional CLI replay adapter for billing 1044e918; NOT authenticated input.
//! Complete is a mock source assertion, never proof about a real bank query.
use crate::{Account, MockContext, ReadKind, ReadState, Sample, Snapshot, Transaction, Window};
use serde_json::Value;
use std::collections::HashSet;

pub fn load_case(json: &str, name: &str, context: MockContext) -> Result<Sample, &'static str> {
    let root: Value = serde_json::from_str(json).map_err(|_| "QUERY_PARSE_FAILED")?;
    if root["mode"] != "OFFLINE_FICTIONAL_BILLING_QUERY"
        || root["fictional"] != true
        || root["production_registered"] != false
        || root["formal_c_txn"] != false
        || root["real_consumption_source_connected"] != false
    {
        return Err("NOT_A_FICTIONAL_QUERY");
    }
    let cases = root["cases"].as_array().ok_or("CASES_MISSING")?;
    let matches: Vec<_> = cases.iter().filter(|c| c["name"] == name).collect();
    if matches.len() != 1 {
        return Err("CASE_NOT_UNIQUE");
    }
    let case = matches[0];
    let read = &case["read"];
    if read["contract"] != "LOCAL_BILLING_QUERY_V1" || read["production_registered"] != false {
        return Err("QUERY_CONTRACT_MISMATCH");
    }
    if read["state"] != "COMPLETE" || read["is_complete"] != true {
        return Err("QUERY_NOT_COMPLETE");
    }
    if read.get("next_cursor") != Some(&Value::Null)
        || read.get("reason") != Some(&Value::Null)
        || read["snapshot_id"]
            .as_str()
            .is_none_or(|s| s.trim().is_empty())
        || read["pages_read"].as_u64().is_none_or(|n| n == 0)
    {
        return Err("QUERY_COMPLETION_INCONSISTENT");
    }
    let source = read["source_id"]
        .as_str()
        .filter(|s| s.starts_with("fictional-"))
        .ok_or("MOCK_SOURCE_REQUIRED")?;
    let accounts: Vec<String> = serde_json::from_value(case["request"]["account_ids"].clone())
        .map_err(|_| "ACCOUNT_SCOPE_INVALID")?;
    let requested: HashSet<_> = context.requested_account_ids.iter().collect();
    if context.user_id.trim().is_empty()
        || accounts.is_empty()
        || requested.len() != context.requested_account_ids.len()
        || accounts.iter().collect::<HashSet<_>>().len() != accounts.len()
        || accounts.iter().collect::<HashSet<_>>() != requested
        || accounts.iter().any(|a| a.trim().is_empty())
    {
        return Err("ACCOUNT_SCOPE_INVALID");
    }
    let window: Window =
        serde_json::from_value(case["request"]["window"].clone()).map_err(|_| "WINDOW_INVALID")?;
    // Replay ALL observed rows through our filter, not just a source's eligible list.
    let rows = read["observed_transactions"]
        .as_array()
        .ok_or("OBSERVED_ROWS_MISSING")?;
    let mut transactions = Vec::new();
    for row in rows {
        let mut row = row.clone();
        let object = row.as_object_mut().ok_or("TRANSACTION_INVALID")?;
        let service = object.remove("service_ref").unwrap_or(Value::Null);
        // Mapping must not erase contradictory origins or service references.
        // These are still mock assertions, not independently verified identities.
        if object
            .get("source_id")
            .is_some_and(|value| value.as_str() != Some(source))
            || object
                .get("mock_service_ref")
                .is_some_and(|value| value != &service)
        {
            return Err("TRANSACTION_REFERENCE_CONFLICT");
        }
        object.insert("source_id".into(), Value::String(source.into()));
        object.insert("mock_service_ref".into(), service);
        let tx: Transaction = serde_json::from_value(row).map_err(|_| "TRANSACTION_INVALID")?;
        if !requested.contains(&tx.account_id) {
            return Err("ACCOUNT_SCOPE_INVALID");
        }
        transactions.push(tx);
    }
    Ok(Sample {
        sample_id: name.into(),
        snapshot: Snapshot {
            window,
            accounts: accounts
                .into_iter()
                .map(|account_id| Account {
                    account_id,
                    user_id: context.user_id.clone(),
                })
                .collect(),
            transactions_read: ReadState {
                state: ReadKind::Complete,
                is_complete: true,
                next_cursor: None,
                reason: None,
            },
            // Never accept an empty/complete managed ledger from the query envelope.
            managed_subscriptions_read: ReadState {
                state: ReadKind::Unavailable,
                is_complete: false,
                next_cursor: None,
                reason: Some("MANAGED_LEDGER_NOT_PROVIDED".into()),
            },
            transactions,
        },
        trusted_context_fixture: context,
    })
}
