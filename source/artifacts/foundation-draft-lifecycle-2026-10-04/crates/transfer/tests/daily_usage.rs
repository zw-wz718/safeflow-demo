use safeflow_bank_gateway::DecimalAmount;
use safeflow_contracts::{OperationId, ResourceId, ToolResultStatus, UserId};
use safeflow_transfer_integration::daily_usage::{
    calculate_daily_usage, DailyUsageError, DailyUsageInput, SubmittedTransferFact,
};

#[test]
fn a_complete_empty_snapshot_is_zero_but_invalid_snapshot_time_is_not() {
    let owner = UserId::parse("user_001").unwrap();
    let calculate = |as_of| {
        calculate_daily_usage(DailyUsageInput {
            owner_id: &owner,
            as_of,
            complete: true,
            submitted: &[],
        })
    };
    assert_eq!(
        calculate("2026-10-17T12:00:00+08:00").unwrap().used_cents,
        0
    );
    assert_eq!(calculate("2026-10-17"), Err(DailyUsageError::InvalidTime));
}

#[test]
fn amount_and_sum_overflow_are_errors_not_wrapped_or_zero_usage() {
    let maximum = fact(
        "op-1",
        "salary",
        "184467440737095516.15",
        "2026-10-17T00:00:00+08:00",
    );
    let another = fact("op-2", "savings", "0.01", "2026-10-17T00:00:00+08:00");
    let mut oversized = maximum.clone();
    oversized.amount = DecimalAmount::parse("184467440737095516.16").unwrap();
    for facts in [vec![maximum, another], vec![oversized]] {
        assert_eq!(
            calculate_daily_usage(DailyUsageInput {
                owner_id: &UserId::parse("user_001").unwrap(),
                as_of: "2026-10-17T12:00:00+08:00",
                complete: true,
                submitted: &facts,
            }),
            Err(DailyUsageError::AmountOverflow)
        );
    }
}

#[test]
fn missing_or_malformed_submission_time_is_not_replaced_by_now() {
    for missing in [None, Some("not-a-date"), Some("2026-10-17T10:00:00")] {
        let mut record = fact("op", "salary", "200.00", "2026-10-17T00:00:00+08:00");
        record.submitted_at = missing.map(str::to_owned);
        assert_eq!(
            calculate_daily_usage(DailyUsageInput {
                owner_id: &record.owner_id,
                as_of: "2026-10-17T12:00:00+08:00",
                complete: true,
                submitted: &[record.clone()],
            }),
            Err(DailyUsageError::InvalidTime)
        );
    }
}

#[test]
fn contradictory_versions_of_one_operation_cannot_be_ordered_by_array_position() {
    let original = fact("op", "salary", "200.00", "2026-10-17T00:00:00+08:00");
    for changed in 0..4 {
        let mut other = original.clone();
        match changed {
            0 => other.amount = DecimalAmount::parse("300.00").unwrap(),
            1 => other.payer_account_id = ResourceId::parse("savings").unwrap(),
            2 => other.submitted_at = Some("2026-10-17T01:00:00+08:00".into()),
            _ => other.status = ToolResultStatus::Processing,
        }
        assert_eq!(
            calculate_daily_usage(DailyUsageInput {
                owner_id: &original.owner_id,
                as_of: "2026-10-17T12:00:00+08:00",
                complete: true,
                submitted: &[original.clone(), other],
            }),
            Err(DailyUsageError::ConflictingOperation)
        );
    }
}

#[test]
fn beijing_midnight_is_half_open_regardless_of_timestamp_offset() {
    let facts = [
        fact("old", "salary", "200.00", "2026-10-17T15:59:59Z"),
        fact("new", "salary", "50.00", "2026-10-17T16:00:00Z"),
    ];
    let usage = calculate_daily_usage(DailyUsageInput {
        owner_id: &UserId::parse("user_001").unwrap(),
        as_of: "2026-10-18T01:00:00+08:00",
        complete: true,
        submitted: &facts,
    })
    .unwrap();
    assert_eq!(usage.statistical_day, "2026-10-18");
    assert_eq!(usage.used_cents, 5_000);
}

fn fact(operation: &str, account: &str, amount: &str, time: &str) -> SubmittedTransferFact {
    SubmittedTransferFact {
        owner_id: UserId::parse("user_001").unwrap(),
        operation_id: OperationId::parse(operation).unwrap(),
        payer_account_id: ResourceId::parse(account).unwrap(),
        amount: DecimalAmount::parse(amount).unwrap(),
        submitted_at: Some(time.into()),
        status: ToolResultStatus::Succeeded,
    }
}

#[test]
fn missing_pages_or_unavailable_source_never_turns_into_zero_usage() {
    let result = calculate_daily_usage(DailyUsageInput {
        owner_id: &UserId::parse("user_001").unwrap(),
        as_of: "2026-10-17T12:00:00+08:00",
        complete: false,
        submitted: &[],
    });
    assert_eq!(result, Err(DailyUsageError::IncompleteSnapshot));
}

#[test]
fn same_persons_payment_accounts_are_summed_and_original_operation_replay_counts_once() {
    let first = fact("op-1", "salary", "200.00", "2026-10-17T00:00:00+08:00");
    let second = fact("op-2", "savings", "50.00", "2026-10-17T10:00:00+08:00");
    let facts = [first.clone(), second, first];
    let usage = calculate_daily_usage(DailyUsageInput {
        owner_id: &UserId::parse("user_001").unwrap(),
        as_of: "2026-10-17T12:00:00+08:00",
        complete: true,
        submitted: &facts,
    })
    .unwrap();
    assert_eq!(usage.statistical_day, "2026-10-17");
    assert_eq!(usage.successful_cents, 25_000);
    assert_eq!(usage.used_cents, 25_000);
}

#[test]
fn prior_day_unknown_occupies_today_then_releases_after_original_order_is_resolved() {
    let mut older = fact("old", "salary", "200.00", "2026-10-16T23:58:00+08:00");
    older.status = ToolResultStatus::Processing;
    let mut current = fact("new", "savings", "50.00", "2026-10-17T00:01:00+08:00");
    current.status = ToolResultStatus::Processing;
    let owner = UserId::parse("user_001").unwrap();
    let calculate = |facts: &[SubmittedTransferFact]| {
        calculate_daily_usage(DailyUsageInput {
            owner_id: &owner,
            as_of: "2026-10-17T12:00:00+08:00",
            complete: true,
            submitted: facts,
        })
        .unwrap()
    };
    let pending = calculate(&[older.clone(), current.clone()]);
    assert_eq!(pending.carried_pending_cents, 20_000);
    assert_eq!(pending.pending_today_cents, 5_000);
    assert_eq!(pending.used_cents, 25_000);
    // Replace the latest facts, never append contradictory receipt histories.
    older.status = ToolResultStatus::Succeeded;
    current.status = ToolResultStatus::Failed;
    let resolved = calculate(&[older.clone(), current.clone()]);
    assert_eq!(resolved.used_cents, 0);
    assert_eq!(resolved.carried_pending_cents, 0);
    current.status = ToolResultStatus::Succeeded;
    assert_eq!(calculate(&[older, current]).used_cents, 5_000);
}

#[test]
fn malformed_or_foreign_facts_are_unusable_even_when_failed_or_from_an_older_day() {
    let valid = fact("op", "salary", "200.00", "2026-10-16T23:00:00+08:00");
    let mut foreign = valid.clone();
    foreign.owner_id = UserId::parse("user_002").unwrap();
    let mut zero = valid.clone();
    zero.amount = DecimalAmount::parse("0.00").unwrap();
    let mut invalid_id = valid.clone();
    invalid_id.payer_account_id = ResourceId::parse("not an account").unwrap();
    let mut future = valid.clone();
    future.submitted_at = Some("2026-10-17T13:00:00+08:00".into());
    for mut invalid in [foreign, zero, invalid_id, future] {
        invalid.status = ToolResultStatus::Failed;
        assert!(
            calculate_daily_usage(DailyUsageInput {
                owner_id: &UserId::parse("user_001").unwrap(),
                as_of: "2026-10-17T12:00:00+08:00",
                complete: true,
                submitted: &[invalid.clone()],
            })
            .is_err(),
            "unexpectedly usable: {invalid:?}"
        );
    }
}
