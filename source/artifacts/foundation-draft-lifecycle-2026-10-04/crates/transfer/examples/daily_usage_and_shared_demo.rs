//! Offline backend facts/configuration example. No SQL, permit, HTTP or debit.
use safeflow_bank_gateway::DecimalAmount;
use safeflow_contracts::{OperationId, ResourceId, ToolResultStatus, UserId};
use safeflow_transfer_integration::{
    daily_usage::{calculate_daily_usage, DailyUsageInput, SubmittedTransferFact},
    shared_demo::{DemoRecipient, SharedDemoPlan},
};
use serde_json::{json, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let owner = UserId::parse("user_001")?;
    let plan = SharedDemoPlan::prepare(DemoRecipient {
        // LOCAL FIXTURE ONLY. Echo must explicitly adopt a trusted mapping;
        // these are not new shared identities or adopted public account IDs.
        owner_id: UserId::parse("fixture-li-owner")?,
        account_id: ResourceId::parse("fixture-li-account")?,
    })?;
    let older = SubmittedTransferFact {
        owner_id: owner.clone(),
        operation_id: OperationId::parse("fixture-old-operation")?,
        payer_account_id: ResourceId::parse("account_001")?,
        amount: DecimalAmount::parse("200.00")?,
        submitted_at: Some("2026-10-16T23:58:00+08:00".into()),
        status: ToolResultStatus::Processing,
    };
    let current = SubmittedTransferFact {
        owner_id: owner.clone(),
        operation_id: OperationId::parse("fixture-current-operation")?,
        // Synthetic second account for calculator demonstration only.
        // Not an additional adopted/provisioned common payment account.
        payer_account_id: ResourceId::parse("fixture-second-payment-account")?,
        amount: DecimalAmount::parse("50.00")?,
        submitted_at: Some("2026-10-17T10:00:00+08:00".into()),
        status: ToolResultStatus::Succeeded,
    };
    let mut scenarios: Vec<Value> = Vec::new();
    for status in [
        ToolResultStatus::Processing,
        ToolResultStatus::Succeeded,
        ToolResultStatus::Failed,
    ] {
        let mut latest = older.clone();
        latest.status = status;
        let facts = vec![latest, current.clone(), current.clone()];
        let usage = calculate_daily_usage(DailyUsageInput {
            owner_id: &owner,
            as_of: "2026-10-17T12:00:00+08:00",
            complete: true,
            submitted: &facts,
        })?;
        scenarios.push(json!({"older_order_latest_status": status, "trusted_fixture_input": facts, "calculated_usage": usage}));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "scope": "OFFLINE_BACKEND_BUSINESS_FACTS_ONLY",
            "funds_executed": false,
            "shared_identity_or_sql_initialized": false,
            "recipient_ids": "LOCAL_FIXTURE_ONLY_NOT_ADOPTED_BY_FOUNDATION",
            "money_unit": "CNY_CENTS_FOR_CALCULATED_USAGE; CNY_DECIMAL_FOR_AMOUNTS_AND_OPENING",
            "demo_provisioning_plan": plan,
            "daily_scenarios": scenarios,
        }))?
    );
    Ok(())
}
