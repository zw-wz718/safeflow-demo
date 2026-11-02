#![cfg(feature = "postgres-store")]

use safeflow_bank_gateway::{CnyCurrency, DecimalAmount, MaskedAccountView, PayeeView};
use safeflow_contracts::{IdempotencyKey, ToolResultStatus};
use safeflow_contracts::{OperationId, ResourceId, TaskId, UserId};
use safeflow_transfer_integration::mock_execution::{
    MockBehavior, MockOutgoingState, MockResolution, MockTransferRequest,
};
use safeflow_transfer_integration::{
    daily_usage::{calculate_daily_usage, DailyUsageError, DailyUsageInput},
    funds_store::{FundsError, FundsStore, LockedOutgoingSource},
    DraftDetails, DraftStatus, StoredDraft,
};
use sqlx::{Connection, PgConnection};
mod policy_support {
    include!("support/funds_policy_fixture.rs");
}

async fn policy_result(
    conn: &mut PgConnection,
    facts: safeflow_transfer_integration::funds_policy::FundsStoredFacts,
) -> Vec<safeflow_contracts::SecurityFinding> {
    use safeflow_contracts::DomainPolicy;
    use safeflow_transfer_integration::funds_policy::FundsBusinessPolicy;
    let (mut context, mut combined) = policy_support::fixture();
    context.authenticated_user_id = facts.owner_id.clone();
    context.task_id = facts.request.task_id.clone();
    context.normalized_arguments = serde_json::to_value(&facts.request).unwrap();
    context.idempotency_key = Some(facts.key.clone());
    let now: i64 =
        sqlx::query_scalar("SELECT floor(extract(epoch from clock_timestamp()))::bigint")
            .fetch_one(conn)
            .await
            .unwrap();
    combined.binding = context.clone();
    combined.checked_at = chrono::DateTime::from_timestamp(now, 0)
        .unwrap()
        .to_rfc3339();
    combined.stored = facts;
    // This test supplies explicit fixture card/empty daily facts. They are NOT
    // read from the public security or card tables, or a real session.
    let policy = FundsBusinessPolicy::new(
        context.tool_name.clone(),
        policy_support::Source(Ok(combined)),
    );
    policy.evaluate(&context).await.unwrap()
}

fn key(suffix: &str) -> IdempotencyKey {
    IdempotencyKey::parse(format!("funds-key-{suffix}")).unwrap()
}

// Test-only source boundary faults. Real card SQL is exercised separately by
// compatibility/card_sql_guard.rs against the fixed card-owner delivery.
enum BoundarySource {
    Allowed,
    MustNotRead,
    WaitUntil { deadline: u64, completed: bool },
    EndCallerTransaction,
}
impl LockedOutgoingSource for BoundarySource {
    fn read<'a>(
        &'a mut self,
        connection: &'a mut PgConnection,
        _owner: &'a UserId,
        _payer: &'a ResourceId,
    ) -> futures::future::BoxFuture<'a, Result<MockOutgoingState, FundsError>> {
        Box::pin(async move {
            match self {
                Self::Allowed => {}
                Self::MustNotRead => panic!("definite failure must not read card facts"),
                Self::WaitUntil {
                    deadline,
                    completed,
                } => {
                    // A deterministic deadline crossing, not concurrency evidence.
                    sqlx::query("SELECT pg_sleep_until(to_timestamp($1::double precision))")
                        .bind(*deadline as f64)
                        .execute(connection)
                        .await?;
                    *completed = true;
                }
                Self::EndCallerTransaction => {
                    // Deliberately violate the trusted-reader contract, only in
                    // this disposable test. No writes exist to be committed.
                    sqlx::Executor::execute(&mut *connection, "COMMIT; BEGIN").await?;
                    sqlx::query("SELECT txid_current()")
                        .execute(connection)
                        .await?;
                }
            }
            Ok(MockOutgoingState::Allowed)
        })
    }
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn checked_submit_rejects_autocommit_and_stale_snapshot_transactions() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "checked-transaction").await;
    for isolation in [None, Some("REPEATABLE READ"), Some("SERIALIZABLE")] {
        if let Some(isolation) = isolation {
            sqlx::Executor::execute(
                &mut conn,
                format!("BEGIN ISOLATION LEVEL {isolation}").as_str(),
            )
            .await
            .unwrap();
        }
        let result = FundsStore::submit_checked(
            &mut conn,
            &d.user_id,
            &key("checked-transaction"),
            &request(&d),
            MockBehavior::Immediate,
            &mut BoundarySource::Allowed,
        )
        .await;
        assert!(matches!(result, Err(FundsError::TransactionRequired)));
        if isolation.is_some() {
            sqlx::Executor::execute(&mut conn, "ROLLBACK")
                .await
                .unwrap();
        }
        assert!(FundsStore::query(&mut conn, &d.user_id, &d.operation_id)
            .await
            .unwrap()
            .is_none());
        assert_funds(&mut conn, &d, "1288.00", "0.00", "1288.00", 0).await;
    }
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn checked_submit_does_not_save_after_source_read_outlives_draft() {
    let mut conn = connection().await;
    let mut d = draft(&mut conn, "checked-expiry").await;
    d.expires_at -= 3595; // Five seconds in this explicitly short-lived fixture.
    let d = setup_draft(&mut conn, "checked-expiry", d).await;
    let mut source = BoundarySource::WaitUntil {
        deadline: d.expires_at,
        completed: false,
    };
    let mut tx = conn.begin().await.unwrap();
    let result = FundsStore::submit_checked(
        &mut tx,
        &d.user_id,
        &key("checked-expiry"),
        &request(&d),
        MockBehavior::Immediate,
        &mut source,
    )
    .await;
    assert!(matches!(result, Err(FundsError::DraftInactive)));
    assert!(matches!(
        source,
        BoundarySource::WaitUntil {
            completed: true,
            ..
        }
    ));
    tx.rollback().await.unwrap();
    assert!(FundsStore::query(&mut conn, &d.user_id, &d.operation_id)
        .await
        .unwrap()
        .is_none());
    let mut expired = d.clone();
    expired.status = DraftStatus::Expired; // Public read derives expiry from time.
    assert_eq!(
        FundsStore::draft(&mut conn, &d.user_id, &d.draft_id)
            .await
            .unwrap(),
        expired
    );
    assert_funds(&mut conn, &d, "1288.00", "0.00", "1288.00", 0).await;
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn checked_failure_requires_transaction_without_reading_card_facts() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "checked-failure-transaction").await;
    submit(
        &mut conn,
        &d,
        "checked-failure-transaction",
        MockBehavior::Pending,
        MockOutgoingState::Allowed,
    )
    .await;
    for isolation in [None, Some("REPEATABLE READ"), Some("SERIALIZABLE")] {
        if let Some(isolation) = isolation {
            sqlx::Executor::execute(
                &mut conn,
                format!("BEGIN ISOLATION LEVEL {isolation}").as_str(),
            )
            .await
            .unwrap();
        }
        let result = FundsStore::resolve_checked(
            &mut conn,
            &d.user_id,
            &d.operation_id,
            MockResolution::Fail,
            &mut BoundarySource::MustNotRead,
        )
        .await;
        assert!(matches!(result, Err(FundsError::TransactionRequired)));
        if isolation.is_some() {
            sqlx::Executor::execute(&mut conn, "ROLLBACK")
                .await
                .unwrap();
        }
        let original = FundsStore::query(&mut conn, &d.user_id, &d.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(original.latest_receipt.status, ToolResultStatus::Processing);
        assert_funds(&mut conn, &d, "1288.00", "200.00", "1088.00", 0).await;
    }
    let mut tx = conn.begin().await.unwrap();
    let failed = FundsStore::resolve_checked(
        &mut tx,
        &d.user_id,
        &d.operation_id,
        MockResolution::Fail,
        &mut BoundarySource::MustNotRead,
    )
    .await
    .unwrap();
    assert_eq!(failed.latest_receipt.status, ToolResultStatus::Failed);
    tx.commit().await.unwrap();
    assert_funds(&mut conn, &d, "1288.00", "0.00", "1288.00", 0).await;
    // Historical terminal replay remains read-only and needs no live card source.
    let replay = FundsStore::resolve_checked(
        &mut conn,
        &d.user_id,
        &d.operation_id,
        MockResolution::Fail,
        &mut BoundarySource::MustNotRead,
    )
    .await
    .unwrap();
    assert_eq!(failed, replay);
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn checked_submit_detects_source_ending_the_callers_transaction() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "checked-ended-transaction").await;
    sqlx::Executor::execute(&mut conn, "BEGIN").await.unwrap();
    let result = FundsStore::submit_checked(
        &mut conn,
        &d.user_id,
        &key("checked-ended-transaction"),
        &request(&d),
        MockBehavior::Immediate,
        &mut BoundarySource::EndCallerTransaction,
    )
    .await;
    assert!(matches!(result, Err(FundsError::TransactionRequired)));
    sqlx::Executor::execute(&mut conn, "ROLLBACK")
        .await
        .unwrap();
    assert!(FundsStore::query(&mut conn, &d.user_id, &d.operation_id)
        .await
        .unwrap()
        .is_none());
    assert_funds(&mut conn, &d, "1288.00", "0.00", "1288.00", 0).await;
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn transactions_reject_invalid_completion_times_without_returning_partial_rows() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "ledger-bad-time").await;
    submit(
        &mut conn,
        &d,
        "ledger-bad-time",
        MockBehavior::Immediate,
        MockOutgoingState::Allowed,
    )
    .await;
    let original = FundsStore::transactions(&mut conn, &d.user_id)
        .await
        .unwrap();
    assert_eq!(original.len(), 1);
    for invalid_time in [
        Some(""),
        Some("not-a-time"),
        Some("2026-10-18T12:00:00"),
        Some("2026-02-30T12:00:00Z"),
        None,
    ] {
        let mut tx = conn.begin().await.unwrap();
        // Fault injection shadows ONLY this connection's ledger, inside a
        // rollback-only transaction. Real tables/immutable triggers stay intact.
        sqlx::query("CREATE TEMP TABLE transfer_ledger_v1 ON COMMIT DROP AS SELECT * FROM public.transfer_ledger_v1")
            .execute(&mut *tx).await.unwrap();
        // Keep a valid row before the malformed row: callers must not receive
        // a plausible-looking partial success or a fabricated replacement time.
        sqlx::query("INSERT INTO transfer_ledger_v1 SELECT transaction_id+1000000000,business_owner_id,operation_id,account_id,direction,amount_cents,$2::text FROM public.transfer_ledger_v1 WHERE account_id=$1")
            .bind(d.details.as_ref().unwrap().account.account_id.as_str())
            .bind(invalid_time).execute(&mut *tx).await.unwrap();
        assert!(
            FundsStore::transactions(&mut tx, &d.user_id).await.is_err(),
            "invalid completion time returned as completed ledger: {invalid_time:?}"
        );
        tx.rollback().await.unwrap();
        assert_eq!(
            FundsStore::transactions(&mut conn, &d.user_id)
                .await
                .unwrap(),
            original
        );
    }
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn transactions_reject_nonpositive_or_missing_ledger_amounts() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "ledger-bad-amount").await;
    submit(
        &mut conn,
        &d,
        "ledger-bad-amount",
        MockBehavior::Immediate,
        MockOutgoingState::Allowed,
    )
    .await;
    let original = FundsStore::transactions(&mut conn, &d.user_id)
        .await
        .unwrap();
    assert_eq!(original.len(), 1);
    for invalid_amount in [Some(0_i64), Some(-1), None] {
        let mut tx = conn.begin().await.unwrap();
        sqlx::query("CREATE TEMP TABLE transfer_ledger_v1 ON COMMIT DROP AS SELECT * FROM public.transfer_ledger_v1")
            .execute(&mut *tx).await.unwrap();
        // These values violate the EXISTING ledger amount_cents > 0 / NOT NULL
        // invariant. Do not weaken real constraints to simulate a bad source.
        sqlx::query("UPDATE transfer_ledger_v1 SET amount_cents=$2 WHERE account_id=$1")
            .bind(d.details.as_ref().unwrap().account.account_id.as_str())
            .bind(invalid_amount)
            .execute(&mut *tx)
            .await
            .unwrap();
        assert!(
            FundsStore::transactions(&mut tx, &d.user_id).await.is_err(),
            "invalid ledger amount returned normally: {invalid_amount:?}"
        );
        tx.rollback().await.unwrap();
        assert_eq!(
            FundsStore::transactions(&mut conn, &d.user_id)
                .await
                .unwrap(),
            original
        );
    }
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn transactions_reject_malformed_operation_ids_and_directions() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "ledger-bad-fields").await;
    submit(
        &mut conn,
        &d,
        "ledger-bad-fields",
        MockBehavior::Immediate,
        MockOutgoingState::Allowed,
    )
    .await;
    let original = FundsStore::transactions(&mut conn, &d.user_id)
        .await
        .unwrap();
    for (statement, value) in [
        (
            "UPDATE transfer_ledger_v1 SET operation_id=$2 WHERE account_id=$1",
            Some(""),
        ),
        (
            "UPDATE transfer_ledger_v1 SET operation_id=$2 WHERE account_id=$1",
            Some(" operation-invalid"),
        ),
        (
            "UPDATE transfer_ledger_v1 SET operation_id=$2 WHERE account_id=$1",
            None,
        ),
        (
            "UPDATE transfer_ledger_v1 SET direction=$2 WHERE account_id=$1",
            Some("OUT"),
        ),
        (
            "UPDATE transfer_ledger_v1 SET direction=$2 WHERE account_id=$1",
            Some("credit"),
        ),
        (
            "UPDATE transfer_ledger_v1 SET direction=$2 WHERE account_id=$1",
            None,
        ),
    ] {
        let mut tx = conn.begin().await.unwrap();
        sqlx::query("CREATE TEMP TABLE transfer_ledger_v1 ON COMMIT DROP AS SELECT * FROM public.transfer_ledger_v1")
            .execute(&mut *tx).await.unwrap();
        sqlx::query(statement)
            .bind(d.details.as_ref().unwrap().account.account_id.as_str())
            .bind(value)
            .execute(&mut *tx)
            .await
            .unwrap();
        assert!(
            FundsStore::transactions(&mut tx, &d.user_id).await.is_err(),
            "malformed ledger field accepted: {statement}, {value:?}"
        );
        tx.rollback().await.unwrap();
        assert_eq!(
            FundsStore::transactions(&mut conn, &d.user_id)
                .await
                .unwrap(),
            original
        );
    }
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn transactions_database_failure_is_not_an_empty_or_partial_success() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "ledger-read-fault").await;
    submit(
        &mut conn,
        &d,
        "ledger-read-fault",
        MockBehavior::Immediate,
        MockOutgoingState::Allowed,
    )
    .await;
    let original = FundsStore::transactions(&mut conn, &d.user_id)
        .await
        .unwrap();
    assert_eq!(original.len(), 1);
    let empty_owner = owner("ledger-no-records");
    assert!(FundsStore::transactions(&mut conn, &empty_owner)
        .await
        .unwrap()
        .is_empty());
    let mut tx = conn.begin().await.unwrap();
    assert!(sqlx::query("SELECT 1/0").execute(&mut *tx).await.is_err());
    // PostgreSQL now rejects reads in this failed transaction. Even a genuinely
    // empty owner must get an error, not a fabricated "no transactions" result.
    for checked_owner in [&d.user_id, &empty_owner] {
        assert!(matches!(
            FundsStore::transactions(&mut tx, checked_owner).await,
            Err(FundsError::Database(_))
        ));
    }
    tx.rollback().await.unwrap();
    assert_eq!(
        FundsStore::transactions(&mut conn, &d.user_id)
            .await
            .unwrap(),
        original
    );
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn transactions_preserve_valid_rows_and_scope_faults_to_the_requested_owner() {
    use safeflow_transfer_integration::mock_execution::TransferDirection;
    let mut conn = connection().await;
    let d = setup(&mut conn, "ledger-good-owner").await;
    let other = setup(&mut conn, "ledger-other-owner").await;
    for (draft, suffix) in [(&d, "ledger-good-owner"), (&other, "ledger-other-owner")] {
        submit(
            &mut conn,
            draft,
            suffix,
            MockBehavior::Immediate,
            MockOutgoingState::Allowed,
        )
        .await;
    }
    let original = FundsStore::transactions(&mut conn, &d.user_id)
        .await
        .unwrap();
    assert_eq!(original.len(), 1);
    let row = &original[0];
    assert_eq!(row.operation_id, d.operation_id);
    assert_eq!(
        row.account_id,
        d.details.as_ref().unwrap().account.account_id
    );
    assert_eq!(row.direction, TransferDirection::Debit);
    assert_eq!(row.amount.as_str(), "200.00");
    assert_eq!(row.currency, CnyCurrency::Cny);
    assert_eq!(row.transaction_type, "TRANSFER");
    assert_eq!(row.status, "COMPLETED");
    assert!(row.card_id.is_none() && row.merchant_id.is_none());
    let recipient = owner("recipient-ledger-good-owner");
    let incoming = FundsStore::transactions(&mut conn, &recipient)
        .await
        .unwrap();
    assert_eq!(incoming.len(), 1);
    assert_eq!(incoming[0].direction, TransferDirection::Credit);
    assert_eq!(incoming[0].amount.as_str(), "200.00");
    assert_eq!(incoming[0].operation_id, d.operation_id);
    let mut tx = conn.begin().await.unwrap();
    sqlx::query("CREATE TEMP TABLE transfer_ledger_v1 ON COMMIT DROP AS SELECT * FROM public.transfer_ledger_v1")
        .execute(&mut *tx).await.unwrap();
    sqlx::query("UPDATE transfer_ledger_v1 SET completed_at='bad-other-time' WHERE account_id=$1")
        .bind(other.details.as_ref().unwrap().account.account_id.as_str())
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(FundsStore::transactions(&mut tx, &other.user_id)
        .await
        .is_err());
    assert_eq!(
        FundsStore::transactions(&mut tx, &d.user_id).await.unwrap(),
        original
    );
    assert_eq!(
        FundsStore::transactions(&mut tx, &recipient).await.unwrap(),
        incoming
    );
    // The existing RFC3339 invariant permits offsets and fractional seconds.
    // Read validation must preserve the string, not normalize or replace it.
    for valid_time in ["2026-10-18T08:00:00+08:00", "2026-10-18T00:00:00.123456Z"] {
        sqlx::query("UPDATE transfer_ledger_v1 SET completed_at=$2 WHERE account_id=$1")
            .bind(d.details.as_ref().unwrap().account.account_id.as_str())
            .bind(valid_time)
            .execute(&mut *tx)
            .await
            .unwrap();
        let mut expected = original.clone();
        expected[0].completed_at = valid_time.into();
        assert_eq!(
            FundsStore::transactions(&mut tx, &d.user_id).await.unwrap(),
            expected
        );
    }
    tx.rollback().await.unwrap();
    assert_eq!(
        FundsStore::transactions(&mut conn, &d.user_id)
            .await
            .unwrap(),
        original
    );
    assert_eq!(
        FundsStore::transactions(&mut conn, &recipient)
            .await
            .unwrap(),
        incoming
    );
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn accounting_check_preserves_original_and_observes_settled_balances_without_writing() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "accounting-success").await;
    let original = submit(
        &mut conn,
        &d,
        "accounting-success",
        MockBehavior::LostReplyAfterCommit,
        MockOutgoingState::Allowed,
    )
    .await;
    for _ in 0..2 {
        let checked = FundsStore::check_accounting(&mut conn, &d.user_id, &d.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(checked.record, original);
        assert_eq!(
            checked.record.first_receipt.status,
            ToolResultStatus::Processing
        );
        assert_eq!(
            checked.record.latest_receipt.status,
            ToolResultStatus::Succeeded
        );
        assert_eq!(checked.payer_funds.booked.as_str(), "1088.00");
        assert_eq!(checked.payer_funds.reserved.as_str(), "0.00");
        assert_eq!(checked.operation_reserved.as_str(), "0.00");
    }
    assert_funds(&mut conn, &d, "1088.00", "0.00", "1088.00", 1).await;
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn accounting_check_rejects_balance_ledger_drift_on_either_account() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "accounting-drift").await;
    submit(
        &mut conn,
        &d,
        "accounting-drift",
        MockBehavior::Immediate,
        MockOutgoingState::Allowed,
    )
    .await;
    for account in [
        d.details.as_ref().unwrap().account.account_id.clone(),
        resource("recipient-accounting-drift"),
    ] {
        let mut tx = conn.begin().await.unwrap();
        // Deliberate storage fault in this isolated transaction, not business logic.
        sqlx::query(
            "UPDATE transfer_accounts_v1 SET booked_cents=booked_cents+1 WHERE account_id=$1",
        )
        .bind(account.as_str())
        .execute(&mut *tx)
        .await
        .unwrap();
        assert!(
            matches!(
                FundsStore::check_accounting(&mut tx, &d.user_id, &d.operation_id).await,
                Err(FundsError::Conflict)
            ),
            "one-cent drift accepted for {account:?}"
        );
        tx.rollback().await.unwrap();
    }
    assert!(
        FundsStore::check_accounting(&mut conn, &d.user_id, &d.operation_id)
            .await
            .unwrap()
            .is_some()
    );
    assert_funds(&mut conn, &d, "1088.00", "0.00", "1088.00", 1).await;
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn accounting_check_keeps_other_holds_and_releases_only_definite_failure() {
    let mut conn = connection().await;
    let a = setup(&mut conn, "accounting-holds").await;
    let b = another_draft(&mut conn, &a, "accounting-holds", "300.00").await;
    submit(
        &mut conn,
        &a,
        "accounting-hold-a",
        MockBehavior::Pending,
        MockOutgoingState::Allowed,
    )
    .await;
    submit(
        &mut conn,
        &b,
        "accounting-hold-b",
        MockBehavior::Pending,
        MockOutgoingState::Allowed,
    )
    .await;
    for _ in 0..2 {
        let checked = FundsStore::check_accounting(&mut conn, &a.user_id, &a.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            checked.record.latest_receipt.status,
            ToolResultStatus::Processing
        );
        assert_eq!(checked.operation_reserved.as_str(), "200.00");
        assert_eq!(checked.payer_funds.reserved.as_str(), "500.00");
        assert_eq!(checked.payer_funds.available.as_str(), "788.00");
    }
    let mut tx = conn.begin().await.unwrap();
    sqlx::query("UPDATE transfer_fund_orders_v1 SET amount_cents=130000 WHERE owner_id=$1 AND operation_id=$2")
        .bind(b.user_id.as_str()).bind(b.operation_id.as_str()).execute(&mut *tx).await.unwrap();
    assert!(matches!(
        FundsStore::check_accounting(&mut tx, &a.user_id, &a.operation_id).await,
        Err(FundsError::Conflict)
    ));
    tx.rollback().await.unwrap();
    let failed = resolve(
        &mut conn,
        &a,
        MockResolution::Fail,
        MockOutgoingState::Allowed,
    )
    .await;
    let checked = FundsStore::check_accounting(&mut conn, &a.user_id, &a.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(checked.record, failed);
    assert_eq!(checked.operation_reserved.as_str(), "0.00");
    assert_eq!(checked.payer_funds.reserved.as_str(), "300.00");
    assert_funds(&mut conn, &a, "1288.00", "300.00", "988.00", 0).await;
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn accounting_check_accepts_other_legitimate_settlements_not_a_fixed_balance() {
    let mut conn = connection().await;
    let a = setup(&mut conn, "accounting-other").await;
    let original = submit(
        &mut conn,
        &a,
        "accounting-other-a",
        MockBehavior::Immediate,
        MockOutgoingState::Allowed,
    )
    .await;
    let b = another_draft(&mut conn, &a, "accounting-other", "300.00").await;
    submit(
        &mut conn,
        &b,
        "accounting-other-b",
        MockBehavior::Immediate,
        MockOutgoingState::Allowed,
    )
    .await;
    let checked = FundsStore::check_accounting(&mut conn, &a.user_id, &a.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(checked.record, original);
    assert_eq!(checked.payer_funds.booked.as_str(), "788.00");
    assert_funds(&mut conn, &a, "788.00", "0.00", "788.00", 2).await;
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn accounting_check_scopes_original_and_never_returns_recipient_funds() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "accounting-scope").await;
    assert!(
        FundsStore::check_accounting(&mut conn, &d.user_id, &d.operation_id)
            .await
            .unwrap()
            .is_none()
    );
    submit(
        &mut conn,
        &d,
        "accounting-scope",
        MockBehavior::Decline,
        MockOutgoingState::Allowed,
    )
    .await;
    assert!(
        FundsStore::check_accounting(&mut conn, &owner("someone-else"), &d.operation_id)
            .await
            .unwrap()
            .is_none()
    );
    let checked = FundsStore::check_accounting(&mut conn, &d.user_id, &d.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        checked.record.latest_receipt.status,
        ToolResultStatus::Failed
    );
    assert_eq!(checked.payer_funds.booked.as_str(), "1288.00");
    assert_eq!(checked.operation_reserved.as_str(), "0.00");
    let json = serde_json::to_value(checked).unwrap();
    assert_eq!(json.as_object().unwrap().len(), 3);
    let text = json.to_string();
    assert!(!text.contains("funds-recipient-accounting-scope"));
    assert!(!text.contains("recipient-accounting-scope"));
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn accounting_check_database_failure_is_not_failed_or_missing() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "accounting-read-error").await;
    let original = submit(
        &mut conn,
        &d,
        "accounting-read-error",
        MockBehavior::Pending,
        MockOutgoingState::Allowed,
    )
    .await;
    let mut tx = conn.begin().await.unwrap();
    sqlx::query("SET LOCAL search_path=pg_catalog")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(matches!(
        FundsStore::check_accounting(&mut tx, &d.user_id, &d.operation_id).await,
        Err(FundsError::Database(_))
    ));
    tx.rollback().await.unwrap();
    assert_eq!(
        FundsStore::query(&mut conn, &d.user_id, &d.operation_id)
            .await
            .unwrap()
            .unwrap(),
        original
    );
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn accounting_check_observes_borrowed_transaction_without_committing() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "accounting-borrowed").await;
    let mut tx = conn.begin().await.unwrap();
    FundsStore::submit(
        &mut tx,
        &d.user_id,
        &key("accounting-borrowed"),
        &request(&d),
        MockBehavior::Immediate,
        MockOutgoingState::Allowed,
    )
    .await
    .unwrap();
    let checked = FundsStore::check_accounting(&mut tx, &d.user_id, &d.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(checked.payer_funds.booked.as_str(), "1088.00");
    let mut observer = connection().await;
    assert!(
        FundsStore::check_accounting(&mut observer, &d.user_id, &d.operation_id)
            .await
            .unwrap()
            .is_none()
    );
    tx.rollback().await.unwrap();
    assert!(
        FundsStore::check_accounting(&mut conn, &d.user_id, &d.operation_id)
            .await
            .unwrap()
            .is_none()
    );
    assert_funds(&mut conn, &d, "1288.00", "0.00", "1288.00", 0).await;
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn policy_sql_facts_use_current_draft_balance_route_and_do_not_write() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "policy-facts").await;
    let r = request(&d);
    let mut tx = conn.begin().await.unwrap();
    let facts = FundsStore::policy_facts(&mut tx, &d.user_id, &key("policy-facts"), &r)
        .await
        .unwrap();
    assert_eq!(facts.draft.as_ref(), Some(&d));
    assert_eq!(facts.request, r);
    assert!(!facts.submitted);
    assert_eq!(facts.payer_owner_id.as_ref(), Some(&d.user_id));
    assert_eq!(facts.funds.as_ref().unwrap().available.as_str(), "1288.00");
    assert_eq!(
        facts.recipient_account_id,
        Some(resource("recipient-policy-facts"))
    );
    assert!(facts.original_request.is_none());
    assert!(facts.key_operation_id.is_none());
    assert!(policy_result(&mut tx, facts).await.is_empty());
    tx.commit().await.unwrap();
    assert_funds(&mut conn, &d, "1288.00", "0.00", "1288.00", 0).await;
    assert!(FundsStore::query(&mut conn, &d.user_id, &d.operation_id)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn policy_sql_sees_uncommitted_changes_and_rollback_without_guessing_route() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "policy-rollback").await;
    let r = request(&d);
    let mut tx = conn.begin().await.unwrap();
    // Fault fixture only, not production business SQL.
    sqlx::query("DELETE FROM transfer_payee_routes_v1 WHERE owner_id=$1")
        .bind(d.user_id.as_str())
        .execute(&mut *tx)
        .await
        .unwrap();
    let facts = FundsStore::policy_facts(&mut tx, &d.user_id, &key("policy-rollback"), &r)
        .await
        .unwrap();
    assert!(facts.recipient_account_id.is_none());
    assert_eq!(
        policy_result(&mut tx, facts).await[0].reason_code.as_str(),
        "PAYEE_ROUTE_UNAVAILABLE"
    );
    tx.rollback().await.unwrap();
    let facts = FundsStore::policy_facts(&mut conn, &d.user_id, &key("policy-rollback"), &r)
        .await
        .unwrap();
    assert!(policy_result(&mut conn, facts).await.is_empty());
    let facts = FundsStore::policy_facts(
        &mut conn,
        &owner("policy-other"),
        &key("policy-rollback"),
        &r,
    )
    .await
    .unwrap();
    assert!(
        facts.draft.is_none()
            && facts.payer_owner_id.is_none()
            && facts.funds.is_none()
            && facts.recipient_account_id.is_none()
    );
    assert!(!policy_result(&mut conn, facts).await.is_empty());
    assert_funds(&mut conn, &d, "1288.00", "0.00", "1288.00", 0).await;
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn policy_sql_existing_operation_and_key_conflicts_never_repeat_money() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "policy-original").await;
    let original = submit(
        &mut conn,
        &d,
        "policy-original",
        MockBehavior::Pending,
        MockOutgoingState::Allowed,
    )
    .await;
    let r = request(&d);
    for case in 0..3 {
        let mut current = r.clone();
        let current_key = if case == 1 {
            key("other-policy-key")
        } else {
            key("policy-original")
        };
        if case == 2 {
            current.remark = "same key, changed arguments".into();
        }
        let facts = FundsStore::policy_facts(&mut conn, &d.user_id, &current_key, &current)
            .await
            .unwrap();
        assert_eq!(facts.original_request.as_ref(), Some(&r));
        let result = policy_result(&mut conn, facts).await;
        assert_eq!(
            result[0].reason_code.as_str(),
            if case == 0 {
                "FUNDS_ORIGINAL_REQUIRES_QUERY"
            } else {
                "FUNDS_IDEMPOTENCY_CONFLICT"
            }
        );
    }
    assert_eq!(
        FundsStore::query(&mut conn, &d.user_id, &d.operation_id)
            .await
            .unwrap(),
        Some(original)
    );
    assert_funds(&mut conn, &d, "1288.00", "200.00", "1088.00", 0).await;
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn policy_sql_database_failure_never_returns_empty_or_allowed() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "policy-db-error").await;
    let mut tx = conn.begin().await.unwrap();
    sqlx::query("SET LOCAL search_path TO pg_catalog")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(matches!(
        FundsStore::policy_facts(&mut tx, &d.user_id, &key("policy-db-error"), &request(&d)).await,
        Err(FundsError::Database(_))
    ));
    tx.rollback().await.unwrap();
    assert_funds(&mut conn, &d, "1288.00", "0.00", "1288.00", 0).await;
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn daily_read_covers_own_accounts_pending_and_replay_without_guessing_time() {
    let mut conn = connection().await;
    let first = setup(&mut conn, "daily-multi").await;
    let mut second = first.clone();
    second.draft_id = resource("daily-second-draft");
    second.operation_id = OperationId::parse("daily-second-operation").unwrap();
    second.details.as_mut().unwrap().account.account_id = resource("daily-second-account");
    let mut tx = conn.begin().await.unwrap();
    FundsStore::provision_account(
        &mut tx,
        &first.user_id,
        &resource("daily-second-account"),
        &DecimalAmount::parse("1288.00").unwrap(),
    )
    .await
    .unwrap();
    FundsStore::save_draft(&mut tx, &first.user_id, None, &second)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let other = setup(&mut conn, "daily-other").await;
    submit(
        &mut conn,
        &first,
        "daily-first",
        MockBehavior::Immediate,
        MockOutgoingState::Allowed,
    )
    .await;
    submit(
        &mut conn,
        &first,
        "daily-first",
        MockBehavior::Immediate,
        MockOutgoingState::Unknown,
    )
    .await;
    submit(
        &mut conn,
        &second,
        "daily-second",
        MockBehavior::Pending,
        MockOutgoingState::Allowed,
    )
    .await;
    submit(
        &mut conn,
        &other,
        "daily-other",
        MockBehavior::Immediate,
        MockOutgoingState::Allowed,
    )
    .await;

    let mut facts = FundsStore::daily_usage_facts(&mut conn, &first.user_id)
        .await
        .unwrap();
    assert_eq!(facts.len(), 2);
    assert!(facts
        .iter()
        .all(|f| f.owner_id == first.user_id && f.submitted_at.is_none()));
    assert!(facts.iter().any(|f| f.operation_id == first.operation_id
        && f.payer_account_id == first.details.as_ref().unwrap().account.account_id
        && f.status == ToolResultStatus::Succeeded
        && f.amount.as_str() == "200.00"));
    assert!(facts.iter().any(|f| f.operation_id == second.operation_id
        && f.payer_account_id == resource("daily-second-account")
        && f.status == ToolResultStatus::Processing));
    assert_eq!(
        calculate_daily_usage(DailyUsageInput {
            owner_id: &first.user_id,
            as_of: "2026-10-17T12:00:00+08:00",
            complete: true,
            submitted: &facts,
        }),
        Err(DailyUsageError::InvalidTime)
    );
    // Synthetic trusted-public-time fixture, NOT a timestamp read from business SQL.
    for fact in &mut facts {
        fact.submitted_at = Some("2026-10-17T10:00:00+08:00".into());
    }
    let usage = calculate_daily_usage(DailyUsageInput {
        owner_id: &first.user_id,
        as_of: "2026-10-17T12:00:00+08:00",
        complete: true,
        submitted: &facts,
    })
    .unwrap();
    assert_eq!(
        (
            usage.successful_cents,
            usage.pending_today_cents,
            usage.used_cents
        ),
        (20000, 20000, 40000)
    );
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn daily_read_rejects_account_ownership_mismatch_instead_of_omitting_order() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "daily-ownership").await;
    submit(
        &mut conn,
        &d,
        "daily-ownership",
        MockBehavior::Pending,
        MockOutgoingState::Allowed,
    )
    .await;
    // Database-boundary fault fixture; observe only through the public reader.
    let mut tx = conn.begin().await.unwrap();
    sqlx::query("UPDATE transfer_accounts_v1 SET owner_id='daily-wrong-owner' WHERE account_id=$1")
        .bind(d.details.as_ref().unwrap().account.account_id.as_str())
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(matches!(
        FundsStore::daily_usage_facts(&mut tx, &d.user_id).await,
        Err(FundsError::Conflict)
    ));
    tx.rollback().await.unwrap();

    let mut tx = conn.begin().await.unwrap();
    sqlx::query("INSERT INTO transfer_business_orders_v1(owner_id,operation_id,original_key,request,first_receipt,latest_receipt) SELECT 'daily-wrong-owner',operation_id,original_key,request,first_receipt,latest_receipt FROM transfer_business_orders_v1 WHERE owner_id=$1")
        .bind(d.user_id.as_str()).execute(&mut *tx).await.unwrap();
    sqlx::query(
        "UPDATE transfer_fund_orders_v1 SET owner_id='daily-wrong-owner' WHERE owner_id=$1",
    )
    .bind(d.user_id.as_str())
    .execute(&mut *tx)
    .await
    .unwrap();
    assert!(matches!(
        FundsStore::daily_usage_facts(&mut tx, &d.user_id).await,
        Err(FundsError::Conflict)
    ));
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn daily_read_keeps_old_pending_until_latest_definite_resolution() {
    let mut conn = connection().await;
    for (suffix, outcome, status) in [
        (
            "daily-final-success",
            MockResolution::Succeed,
            ToolResultStatus::Succeeded,
        ),
        (
            "daily-final-failure",
            MockResolution::Fail,
            ToolResultStatus::Failed,
        ),
    ] {
        let d = setup(&mut conn, suffix).await;
        submit(
            &mut conn,
            &d,
            suffix,
            MockBehavior::Pending,
            MockOutgoingState::Allowed,
        )
        .await;
        let mut facts = FundsStore::daily_usage_facts(&mut conn, &d.user_id)
            .await
            .unwrap();
        assert_eq!(facts.len(), 1);
        // Trusted-time fixture explicitly stands in for the public adapter.
        facts[0].submitted_at = Some("2026-10-16T23:59:00+08:00".into());
        let usage = calculate_daily_usage(DailyUsageInput {
            owner_id: &d.user_id,
            as_of: "2026-10-17T01:00:00+08:00",
            complete: true,
            submitted: &facts,
        })
        .unwrap();
        assert_eq!(
            (usage.carried_pending_cents, usage.used_cents),
            (20000, 20000)
        );
        resolve(&mut conn, &d, outcome, MockOutgoingState::Allowed).await;
        conn.close().await.unwrap();
        conn = connection().await;
        let mut facts = FundsStore::daily_usage_facts(&mut conn, &d.user_id)
            .await
            .unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].status, status);
        facts[0].submitted_at = Some("2026-10-16T23:59:00+08:00".into());
        let usage = calculate_daily_usage(DailyUsageInput {
            owner_id: &d.user_id,
            as_of: "2026-10-17T01:00:00+08:00",
            complete: true,
            submitted: &facts,
        })
        .unwrap();
        assert_eq!((usage.carried_pending_cents, usage.used_cents), (0, 0));
        let original_day = calculate_daily_usage(DailyUsageInput {
            owner_id: &d.user_id,
            as_of: "2026-10-16T23:59:59+08:00",
            complete: true,
            submitted: &facts,
        })
        .unwrap();
        assert_eq!(
            original_day.successful_cents,
            if status == ToolResultStatus::Succeeded {
                20000
            } else {
                0
            }
        );
    }
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn daily_read_uses_actual_success_after_lost_reply_and_excludes_drafts_and_incoming() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "daily-lost").await;
    another_draft(&mut conn, &d, "daily-unsubmitted", "900.00").await;
    let reply = submit(
        &mut conn,
        &d,
        "daily-lost",
        MockBehavior::LostReplyAfterCommit,
        MockOutgoingState::Allowed,
    )
    .await;
    assert_eq!(reply.first_receipt.status, ToolResultStatus::Processing);
    let facts = FundsStore::daily_usage_facts(&mut conn, &d.user_id)
        .await
        .unwrap();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].status, ToolResultStatus::Succeeded);
    assert_eq!(facts[0].operation_id, d.operation_id);
    // Recipient has a real incoming ledger row, but no outgoing fund order.
    let recipient = owner("recipient-daily-lost");
    assert_eq!(
        FundsStore::transactions(&mut conn, &recipient)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(FundsStore::daily_usage_facts(&mut conn, &recipient)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn daily_read_rejects_corrupt_or_conflicting_original_instead_of_partial_facts() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "daily-corrupt").await;
    submit(
        &mut conn,
        &d,
        "daily-corrupt",
        MockBehavior::Pending,
        MockOutgoingState::Allowed,
    )
    .await;
    for sql in [
        "UPDATE transfer_fund_orders_v1 SET amount_cents=30000 WHERE owner_id=$1",
        "UPDATE transfer_fund_orders_v1 SET state='FAILED' WHERE owner_id=$1",
        "UPDATE transfer_business_orders_v1 SET request='{}'::jsonb WHERE owner_id=$1",
        "UPDATE transfer_business_orders_v1 SET latest_receipt=jsonb_set(latest_receipt,'{operation_id}','\"wrong-operation\"') WHERE owner_id=$1",
    ] {
        let mut tx = conn.begin().await.unwrap();
        sqlx::query(sql).bind(d.user_id.as_str()).execute(&mut *tx).await.unwrap();
        assert!(FundsStore::daily_usage_facts(&mut tx, &d.user_id).await.is_err());
        tx.rollback().await.unwrap();
    }
    assert_eq!(
        FundsStore::daily_usage_facts(&mut conn, &d.user_id)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn daily_read_database_failure_is_not_zero_usage() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "daily-read-failure").await;
    // A successful empty business read is distinguishable from an unavailable source.
    assert!(FundsStore::daily_usage_facts(&mut conn, &d.user_id)
        .await
        .unwrap()
        .is_empty());
    let mut tx = conn.begin().await.unwrap();
    sqlx::query("SET LOCAL search_path=pg_catalog")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(matches!(
        FundsStore::daily_usage_facts(&mut tx, &d.user_id).await,
        Err(FundsError::Database(_))
    ));
    tx.rollback().await.unwrap();
    assert!(FundsStore::daily_usage_facts(&mut conn, &d.user_id)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn daily_read_obeys_callers_uncommitted_connection_and_rollback() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "daily-borrowed").await;
    let mut tx = conn.begin().await.unwrap();
    FundsStore::submit(
        &mut tx,
        &d.user_id,
        &key("daily-borrowed"),
        &request(&d),
        MockBehavior::Pending,
        MockOutgoingState::Allowed,
    )
    .await
    .unwrap();
    assert_eq!(
        FundsStore::daily_usage_facts(&mut tx, &d.user_id)
            .await
            .unwrap()
            .len(),
        1
    );
    let mut observer = connection().await;
    assert!(FundsStore::daily_usage_facts(&mut observer, &d.user_id)
        .await
        .unwrap()
        .is_empty());
    tx.rollback().await.unwrap();
    assert!(FundsStore::daily_usage_facts(&mut conn, &d.user_id)
        .await
        .unwrap()
        .is_empty());
}

async fn submit(
    conn: &mut PgConnection,
    d: &StoredDraft,
    k: &str,
    behavior: MockBehavior,
    outgoing: MockOutgoingState,
) -> safeflow_transfer_integration::business_store::BusinessRecord {
    let mut tx = conn.begin().await.unwrap();
    let result = FundsStore::submit(
        &mut tx,
        &d.user_id,
        &key(k),
        &request(d),
        behavior,
        outgoing,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    result
}
async fn resolve(
    conn: &mut PgConnection,
    d: &StoredDraft,
    result: MockResolution,
    outgoing: MockOutgoingState,
) -> safeflow_transfer_integration::business_store::BusinessRecord {
    let mut tx = conn.begin().await.unwrap();
    let record = FundsStore::resolve(&mut tx, &d.user_id, &d.operation_id, result, outgoing)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    record
}
async fn assert_funds(
    conn: &mut PgConnection,
    d: &StoredDraft,
    booked: &str,
    reserved: &str,
    available: &str,
    rows: usize,
) {
    let funds = FundsStore::funds(
        conn,
        &d.user_id,
        &d.details.as_ref().unwrap().account.account_id,
    )
    .await
    .unwrap();
    assert_eq!(
        (
            funds.booked.as_str(),
            funds.reserved.as_str(),
            funds.available.as_str()
        ),
        (booked, reserved, available)
    );
    assert_eq!(
        FundsStore::transactions(conn, &d.user_id)
            .await
            .unwrap()
            .len(),
        rows
    );
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn pending_holds_survive_reconnect_and_release_only_on_definite_failure() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "pending-fail").await;
    let first = submit(
        &mut conn,
        &d,
        "pending-fail",
        MockBehavior::Pending,
        MockOutgoingState::Allowed,
    )
    .await;
    assert_eq!(first.latest_receipt.status, ToolResultStatus::Processing);
    assert_funds(&mut conn, &d, "1288.00", "200.00", "1088.00", 0).await;
    conn.close().await.unwrap();
    let mut conn = connection().await;
    let mut tx = conn.begin().await.unwrap();
    assert!(matches!(
        FundsStore::resolve(
            &mut tx,
            &d.user_id,
            &d.operation_id,
            MockResolution::Succeed,
            MockOutgoingState::Unknown
        )
        .await,
        Err(FundsError::OutgoingUnknown)
    ));
    tx.rollback().await.unwrap();
    assert_funds(&mut conn, &d, "1288.00", "200.00", "1088.00", 0).await;
    let final_record = resolve(
        &mut conn,
        &d,
        MockResolution::Fail,
        MockOutgoingState::Unknown,
    )
    .await;
    assert_eq!(final_record.first_receipt, first.first_receipt);
    assert_eq!(final_record.latest_receipt.status, ToolResultStatus::Failed);
    assert_funds(&mut conn, &d, "1288.00", "0.00", "1288.00", 0).await;
    resolve(
        &mut conn,
        &d,
        MockResolution::Fail,
        MockOutgoingState::Unknown,
    )
    .await;
    let mut tx = conn.begin().await.unwrap();
    assert!(FundsStore::resolve(
        &mut tx,
        &d.user_id,
        &d.operation_id,
        MockResolution::Succeed,
        MockOutgoingState::Allowed
    )
    .await
    .is_err());
    tx.rollback().await.unwrap();
}

async fn another_draft(
    conn: &mut PgConnection,
    base: &StoredDraft,
    suffix: &str,
    amount: &str,
) -> StoredDraft {
    let mut next = base.clone();
    next.draft_id = resource(&format!("extra-{suffix}"));
    next.operation_id = OperationId::parse(format!("extra-op-{suffix}")).unwrap();
    next.details.as_mut().unwrap().amount = DecimalAmount::parse(amount).unwrap();
    let mut tx = conn.begin().await.unwrap();
    FundsStore::save_draft(&mut tx, &next.user_id, None, &next)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    next
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn settling_one_hold_keeps_the_other_and_checks_final_outgoing_state() {
    let mut conn = connection().await;
    let a = setup(&mut conn, "two-holds").await;
    let b = another_draft(&mut conn, &a, "two-holds", "300.00").await;
    submit(
        &mut conn,
        &a,
        "hold-a",
        MockBehavior::Pending,
        MockOutgoingState::Allowed,
    )
    .await;
    submit(
        &mut conn,
        &b,
        "hold-b",
        MockBehavior::Pending,
        MockOutgoingState::Allowed,
    )
    .await;
    assert_funds(&mut conn, &a, "1288.00", "500.00", "788.00", 0).await;
    resolve(
        &mut conn,
        &a,
        MockResolution::Succeed,
        MockOutgoingState::Allowed,
    )
    .await;
    assert_funds(&mut conn, &a, "1088.00", "300.00", "788.00", 1).await;
    let b_result = resolve(
        &mut conn,
        &b,
        MockResolution::Succeed,
        MockOutgoingState::Blocked,
    )
    .await;
    assert_eq!(b_result.latest_receipt.status, ToolResultStatus::Failed);
    assert_eq!(
        b_result.latest_receipt.reason_code.as_deref(),
        Some("ACCOUNT_OUTGOING_BLOCKED")
    );
    assert_funds(&mut conn, &a, "1088.00", "0.00", "1088.00", 1).await;
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn lost_reply_is_queried_not_paid_twice_and_low_level_receipt_cannot_fake_settlement() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "lost-reply").await;
    let result = submit(
        &mut conn,
        &d,
        "lost-reply",
        MockBehavior::LostReplyAfterCommit,
        MockOutgoingState::Allowed,
    )
    .await;
    assert_eq!(result.first_receipt.status, ToolResultStatus::Processing);
    assert_eq!(result.latest_receipt.status, ToolResultStatus::Succeeded);
    assert_funds(&mut conn, &d, "1088.00", "0.00", "1088.00", 1).await;
    assert_eq!(
        FundsStore::query(&mut conn, &d.user_id, &d.operation_id)
            .await
            .unwrap()
            .unwrap(),
        result
    );
    let replay = submit(
        &mut conn,
        &d,
        "lost-reply",
        MockBehavior::Immediate,
        MockOutgoingState::Allowed,
    )
    .await;
    assert_eq!(replay, result);
    assert_funds(&mut conn, &d, "1088.00", "0.00", "1088.00", 1).await;
    // A receipt-only legacy record cannot be used to claim a funded operation.
    let other = setup(&mut conn, "receipt-only").await;
    let r = request(&other);
    let mut receipt = result.latest_receipt.clone();
    receipt.task_id = r.task_id.clone();
    receipt.operation_id = r.operation_id.clone();
    receipt.draft_id = r.draft_id.clone();
    receipt.payer_account_id = r.payer_account_id.clone();
    receipt.payee_id = r.payee_id.clone();
    let mut tx = conn.begin().await.unwrap();
    safeflow_transfer_integration::business_store::BusinessStore::record_submission(
        &mut tx,
        &other.user_id,
        &key("receipt-only"),
        &r,
        &receipt,
        &receipt,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(
        FundsStore::query(&mut conn, &other.user_id, &other.operation_id)
            .await
            .is_err()
    );
    assert_funds(&mut conn, &other, "1288.00", "0.00", "1288.00", 0).await;
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn wrong_key_parameters_version_or_owner_never_change_money() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "binding").await;
    let original = request(&d);
    for field in 0..7 {
        let mut r = original.clone();
        match field {
            0 => r.draft_version = 2,
            1 => r.amount = DecimalAmount::parse("300.00").unwrap(),
            2 => r.payer_account_id = resource("someone-else"),
            3 => r.payee_id = resource("other-payee"),
            4 => r.remark = "篡改备注".into(),
            5 => r.task_id = TaskId::parse("other-task").unwrap(),
            _ => r.operation_id = OperationId::parse("other-op").unwrap(),
        };
        let mut tx = conn.begin().await.unwrap();
        assert!(FundsStore::submit(
            &mut tx,
            &d.user_id,
            &key("binding"),
            &r,
            MockBehavior::Immediate,
            MockOutgoingState::Allowed
        )
        .await
        .is_err());
        tx.rollback().await.unwrap();
    }
    let mut tx = conn.begin().await.unwrap();
    assert!(FundsStore::submit(
        &mut tx,
        &owner("attacker"),
        &key("binding"),
        &original,
        MockBehavior::Immediate,
        MockOutgoingState::Allowed
    )
    .await
    .is_err());
    tx.rollback().await.unwrap();
    assert_funds(&mut conn, &d, "1288.00", "0.00", "1288.00", 0).await;
    submit(
        &mut conn,
        &d,
        "binding",
        MockBehavior::Immediate,
        MockOutgoingState::Allowed,
    )
    .await;
    for r in [original.clone(), {
        let mut r = original.clone();
        r.remark = "same key different parameters".into();
        r
    }] {
        let k = if r == original {
            key("different-key")
        } else {
            key("binding")
        };
        let mut tx = conn.begin().await.unwrap();
        assert!(FundsStore::submit(
            &mut tx,
            &d.user_id,
            &k,
            &r,
            MockBehavior::Immediate,
            MockOutgoingState::Allowed
        )
        .await
        .is_err());
        tx.rollback().await.unwrap();
    }
    assert_funds(&mut conn, &d, "1088.00", "0.00", "1088.00", 1).await;
    assert!(
        FundsStore::draft(&mut conn, &owner("attacker"), &d.draft_id)
            .await
            .is_err()
    );
    assert!(
        FundsStore::funds(&mut conn, &owner("attacker"), &original.payer_account_id)
            .await
            .is_err()
    );
    assert!(
        FundsStore::query(&mut conn, &owner("attacker"), &d.operation_id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(FundsStore::transactions(&mut conn, &owner("attacker"))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn caller_rollback_undoes_accounts_orders_ledger_and_draft_submission() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "rollback-all").await;
    let mut tx = conn.begin().await.unwrap();
    FundsStore::submit(
        &mut tx,
        &d.user_id,
        &key("rollback-all"),
        &request(&d),
        MockBehavior::Immediate,
        MockOutgoingState::Allowed,
    )
    .await
    .unwrap();
    // Models a later public record/audit/outbox failure in the SAME transaction.
    assert!(sqlx::query("SELECT 1/0").execute(&mut *tx).await.is_err());
    tx.rollback().await.unwrap();
    assert_funds(&mut conn, &d, "1288.00", "0.00", "1288.00", 0).await;
    assert!(FundsStore::query(&mut conn, &d.user_id, &d.operation_id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        FundsStore::draft(&mut conn, &d.user_id, &d.draft_id)
            .await
            .unwrap(),
        d
    );
    // Rollback did not burn the draft. A new caller transaction can now submit.
    submit(
        &mut conn,
        &d,
        "rollback-all",
        MockBehavior::Immediate,
        MockOutgoingState::Allowed,
    )
    .await;
    assert_funds(&mut conn, &d, "1088.00", "0.00", "1088.00", 1).await;
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn concurrent_same_key_and_competing_holds_never_double_spend() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "concurrent").await;
    let mut handles = Vec::new();
    for _ in 0..6 {
        let d = d.clone();
        handles.push(tokio::spawn(async move {
            let mut conn = connection().await;
            submit(
                &mut conn,
                &d,
                "concurrent",
                MockBehavior::Immediate,
                MockOutgoingState::Allowed,
            )
            .await
        }));
    }
    for h in handles {
        assert_eq!(
            h.await.unwrap().latest_receipt.status,
            ToolResultStatus::Succeeded
        );
    }
    assert_funds(&mut conn, &d, "1088.00", "0.00", "1088.00", 1).await;
    let a = another_draft(&mut conn, &d, "compete-a", "800.00").await;
    let b = another_draft(&mut conn, &d, "compete-b", "800.00").await;
    let mut handles = Vec::new();
    for (k, d) in [("compete-a", a), ("compete-b", b)] {
        handles.push(tokio::spawn(async move {
            let mut conn = connection().await;
            submit(
                &mut conn,
                &d,
                k,
                MockBehavior::Pending,
                MockOutgoingState::Allowed,
            )
            .await
        }));
    }
    let mut statuses = Vec::new();
    for h in handles {
        statuses.push(h.await.unwrap().latest_receipt.status);
    }
    assert_eq!(
        statuses
            .iter()
            .filter(|&&s| s == ToolResultStatus::Processing)
            .count(),
        1
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|&&s| s == ToolResultStatus::Failed)
            .count(),
        1
    );
    assert_funds(&mut conn, &d, "1088.00", "800.00", "288.00", 1).await;
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn cancelled_expired_submitted_and_extended_drafts_cannot_be_used() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "draft-lifecycle").await;
    let mut cancelled = d.clone();
    cancelled.status = DraftStatus::Cancelled;
    let mut tx = conn.begin().await.unwrap();
    FundsStore::save_draft(&mut tx, &d.user_id, Some(1), &cancelled)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    assert!(FundsStore::save_draft(&mut tx, &d.user_id, Some(1), &d)
        .await
        .is_err());
    tx.rollback().await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    assert!(FundsStore::submit(
        &mut tx,
        &d.user_id,
        &key("cancelled"),
        &request(&d),
        MockBehavior::Immediate,
        MockOutgoingState::Allowed
    )
    .await
    .is_err());
    tx.rollback().await.unwrap();
    let good = another_draft(&mut conn, &d, "active-draft", "200.00").await;
    // another_draft copied CANCELLED? base d stays original READY.
    let mut extended = good.clone();
    extended.plan_version = 2;
    extended.expires_at += 60;
    let mut tx = conn.begin().await.unwrap();
    assert!(
        FundsStore::save_draft(&mut tx, &good.user_id, Some(1), &extended)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    submit(
        &mut conn,
        &good,
        "submitted-draft",
        MockBehavior::Pending,
        MockOutgoingState::Allowed,
    )
    .await;
    let mut cancelled = good.clone();
    cancelled.status = DraftStatus::Cancelled;
    let mut tx = conn.begin().await.unwrap();
    assert!(matches!(
        FundsStore::save_draft(&mut tx, &good.user_id, Some(1), &cancelled).await,
        Err(FundsError::AlreadySubmitted)
    ));
    tx.rollback().await.unwrap();
    let expired = another_draft(&mut conn, &d, "expired-draft", "200.00").await;
    // Fixture-only database time setup; no real user controls the clock/expiry.
    sqlx::query("UPDATE transfer_drafts_v1 SET expires_at=1 WHERE owner_id=$1 AND draft_id=$2")
        .bind(d.user_id.as_str())
        .bind(expired.draft_id.as_str())
        .execute(&mut conn)
        .await
        .unwrap();
    let mut tx = conn.begin().await.unwrap();
    assert!(matches!(
        FundsStore::submit(
            &mut tx,
            &expired.user_id,
            &key("expired"),
            &request(&expired),
            MockBehavior::Immediate,
            MockOutgoingState::Allowed
        )
        .await,
        Err(FundsError::DraftInactive)
    ));
    tx.rollback().await.unwrap();
    assert_funds(&mut conn, &d, "1288.00", "200.00", "1088.00", 0).await;
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn blocked_unknown_and_declined_requests_do_not_create_completed_ledger() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "blocked").await;
    let mut tx = conn.begin().await.unwrap();
    assert!(matches!(
        FundsStore::submit(
            &mut tx,
            &d.user_id,
            &key("unknown"),
            &request(&d),
            MockBehavior::Immediate,
            MockOutgoingState::Unknown
        )
        .await,
        Err(FundsError::OutgoingUnknown)
    ));
    tx.rollback().await.unwrap();
    let blocked = submit(
        &mut conn,
        &d,
        "blocked",
        MockBehavior::Immediate,
        MockOutgoingState::Blocked,
    )
    .await;
    assert_eq!(
        blocked.latest_receipt.reason_code.as_deref(),
        Some("ACCOUNT_OUTGOING_BLOCKED")
    );
    let other = another_draft(&mut conn, &d, "declined", "200.00").await;
    assert_eq!(
        submit(
            &mut conn,
            &other,
            "declined",
            MockBehavior::Decline,
            MockOutgoingState::Allowed
        )
        .await
        .latest_receipt
        .status,
        ToolResultStatus::Failed
    );
    assert_funds(&mut conn, &d, "1288.00", "0.00", "1288.00", 0).await;
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn provisioning_replay_does_not_reset_balances_or_change_routing() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "provision").await;
    submit(
        &mut conn,
        &d,
        "provision",
        MockBehavior::Immediate,
        MockOutgoingState::Allowed,
    )
    .await;
    let r = request(&d);
    let mut tx = conn.begin().await.unwrap();
    FundsStore::provision_account(
        &mut tx,
        &d.user_id,
        &r.payer_account_id,
        &DecimalAmount::parse("1288.00").unwrap(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    assert!(FundsStore::provision_account(
        &mut tx,
        &owner("attacker"),
        &r.payer_account_id,
        &DecimalAmount::parse("1288.00").unwrap()
    )
    .await
    .is_err());
    tx.rollback().await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    assert!(
        FundsStore::provision_payee(&mut tx, &d.user_id, &r.payee_id, &r.payer_account_id)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    assert!(FundsStore::provision_account(
        &mut tx,
        &d.user_id,
        &resource("overflow-account"),
        &DecimalAmount::parse("92233720368547758.08").unwrap()
    )
    .await
    .is_err());
    tx.rollback().await.unwrap();
    assert_funds(&mut conn, &d, "1088.00", "0.00", "1088.00", 1).await;
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn second_ledger_failure_rolls_back_first_ledger_and_both_balances() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "ledger-fault").await;
    sqlx::query("CREATE FUNCTION transfer_test_credit_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.business_owner_id='funds-ledger-fault' AND NEW.direction='CREDIT' THEN RAISE EXCEPTION 'TEST_CREDIT_FAILURE'; END IF; RETURN NEW; END; $$").execute(&mut conn).await.unwrap();
    sqlx::query("CREATE TRIGGER transfer_test_credit_fault BEFORE INSERT ON transfer_ledger_v1 FOR EACH ROW EXECUTE FUNCTION transfer_test_credit_fault()").execute(&mut conn).await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    assert!(matches!(
        FundsStore::submit(
            &mut tx,
            &d.user_id,
            &key("ledger-fault"),
            &request(&d),
            MockBehavior::Immediate,
            MockOutgoingState::Allowed
        )
        .await,
        Err(FundsError::Database(_))
    ));
    tx.rollback().await.unwrap();
    assert_funds(&mut conn, &d, "1288.00", "0.00", "1288.00", 0).await;
    assert_eq!(
        FundsStore::funds(
            &mut conn,
            &owner("recipient-ledger-fault"),
            &resource("recipient-ledger-fault")
        )
        .await
        .unwrap()
        .booked
        .as_str(),
        "0.00"
    );
    assert!(FundsStore::query(&mut conn, &d.user_id, &d.operation_id)
        .await
        .unwrap()
        .is_none());
    sqlx::query("DROP TRIGGER transfer_test_credit_fault ON transfer_ledger_v1")
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("DROP FUNCTION transfer_test_credit_fault()")
        .execute(&mut conn)
        .await
        .unwrap();
    submit(
        &mut conn,
        &d,
        "ledger-fault",
        MockBehavior::Immediate,
        MockOutgoingState::Allowed,
    )
    .await;
    assert_funds(&mut conn, &d, "1088.00", "0.00", "1088.00", 1).await;
    // Ledger history cannot be silently rewritten, including by application SQL.
    let mut tx = conn.begin().await.unwrap();
    assert!(
        sqlx::query("UPDATE transfer_ledger_v1 SET amount_cents=1 WHERE business_owner_id=$1")
            .bind(d.user_id.as_str())
            .execute(&mut *tx)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn recipient_ledger_ids_do_not_reveal_sender_identity() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "private-ledger").await;
    submit(
        &mut conn,
        &d,
        "private-ledger",
        MockBehavior::Immediate,
        MockOutgoingState::Allowed,
    )
    .await;
    let rows = FundsStore::transactions(&mut conn, &owner("recipient-private-ledger"))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].transaction_id.contains(d.user_id.as_str()));
    assert!(rows[0].card_id.is_none() && rows[0].merchant_id.is_none());
    assert_eq!(
        rows[0].direction,
        safeflow_transfer_integration::mock_execution::TransferDirection::Credit
    );
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn opposite_transfers_lock_accounts_in_the_same_order_and_conserve_funds() {
    let mut conn = connection().await;
    let mut a = setup(&mut conn, "opposite-a").await;
    let mut b = setup(&mut conn, "opposite-b").await;
    let a_id = a.details.as_ref().unwrap().account.account_id.clone();
    let b_id = b.details.as_ref().unwrap().account.account_id.clone();
    for (d, target, suffix) in [(&mut a, b_id, "a"), (&mut b, a_id, "b")] {
        d.plan_version = 2;
        d.details.as_mut().unwrap().payee.payee_id = resource(&format!("opposite-route-{suffix}"));
        let mut tx = conn.begin().await.unwrap();
        FundsStore::provision_payee(
            &mut tx,
            &d.user_id,
            &d.details.as_ref().unwrap().payee.payee_id,
            &target,
        )
        .await
        .unwrap();
        FundsStore::save_draft(&mut tx, &d.user_id, Some(1), d)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    let mut handles = Vec::new();
    for (d, k) in [(a.clone(), "opposite-a"), (b.clone(), "opposite-b")] {
        handles.push(tokio::spawn(async move {
            let mut conn = connection().await;
            let mut tx = conn.begin().await.unwrap();
            sqlx::query("SET LOCAL lock_timeout='5s'")
                .execute(&mut *tx)
                .await
                .unwrap();
            let record = FundsStore::submit(
                &mut tx,
                &d.user_id,
                &key(k),
                &request(&d),
                MockBehavior::Immediate,
                MockOutgoingState::Allowed,
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
            record
        }));
    }
    for h in handles {
        assert_eq!(
            h.await.unwrap().latest_receipt.status,
            ToolResultStatus::Succeeded
        );
    }
    assert_funds(&mut conn, &a, "1288.00", "0.00", "1288.00", 2).await;
    assert_funds(&mut conn, &b, "1288.00", "0.00", "1288.00", 2).await;
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn recipient_overflow_is_an_error_and_does_not_leave_partial_payment() {
    let mut conn = connection().await;
    let mut d = setup(&mut conn, "credit-overflow").await;
    let target = resource("maximum-recipient");
    d.plan_version = 2;
    d.details.as_mut().unwrap().payee.payee_id = resource("maximum-payee");
    let mut tx = conn.begin().await.unwrap();
    FundsStore::provision_account(
        &mut tx,
        &owner("maximum-recipient"),
        &target,
        &DecimalAmount::parse("92233720368547758.07").unwrap(),
    )
    .await
    .unwrap();
    FundsStore::provision_payee(
        &mut tx,
        &d.user_id,
        &d.details.as_ref().unwrap().payee.payee_id,
        &target,
    )
    .await
    .unwrap();
    FundsStore::save_draft(&mut tx, &d.user_id, Some(1), &d)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    assert!(matches!(
        FundsStore::submit(
            &mut tx,
            &d.user_id,
            &key("credit-overflow"),
            &request(&d),
            MockBehavior::Immediate,
            MockOutgoingState::Allowed
        )
        .await,
        Err(FundsError::Invalid)
    ));
    tx.rollback().await.unwrap();
    assert_funds(&mut conn, &d, "1288.00", "0.00", "1288.00", 0).await;
    assert!(FundsStore::query(&mut conn, &d.user_id, &d.operation_id)
        .await
        .unwrap()
        .is_none());
}

fn request(draft: &StoredDraft) -> MockTransferRequest {
    let d = draft.details.as_ref().unwrap();
    MockTransferRequest {
        task_id: draft.task_id.clone(),
        operation_id: draft.operation_id.clone(),
        draft_id: draft.draft_id.clone(),
        draft_version: draft.plan_version,
        payer_account_id: d.account.account_id.clone(),
        payee_id: d.payee.payee_id.clone(),
        amount: d.amount.clone(),
        currency: d.currency,
        remark: d.remark.clone(),
    }
}

async fn setup(conn: &mut PgConnection, suffix: &str) -> StoredDraft {
    let draft = draft(conn, suffix).await;
    setup_draft(conn, suffix, draft).await
}

async fn setup_draft(conn: &mut PgConnection, suffix: &str, draft: StoredDraft) -> StoredDraft {
    let d = draft.details.as_ref().unwrap();
    let mut tx = conn.begin().await.unwrap();
    FundsStore::provision_account(
        &mut tx,
        &draft.user_id,
        &d.account.account_id,
        &DecimalAmount::parse("1288.00").unwrap(),
    )
    .await
    .unwrap();
    FundsStore::provision_account(
        &mut tx,
        &owner(&format!("recipient-{suffix}")),
        &resource(&format!("recipient-{suffix}")),
        &DecimalAmount::parse("0.00").unwrap(),
    )
    .await
    .unwrap();
    FundsStore::provision_payee(
        &mut tx,
        &draft.user_id,
        &d.payee.payee_id,
        &resource(&format!("recipient-{suffix}")),
    )
    .await
    .unwrap();
    FundsStore::save_draft(&mut tx, &draft.user_id, None, &draft)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    draft
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn settlement_is_persistent_balanced_and_replayed_once() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "settlement").await;
    let r = request(&d);
    let mut tx = conn.begin().await.unwrap();
    let result = FundsStore::submit(
        &mut tx,
        &d.user_id,
        &key("settlement"),
        &r,
        MockBehavior::Immediate,
        MockOutgoingState::Allowed,
    )
    .await
    .unwrap();
    assert_eq!(result.latest_receipt.status, ToolResultStatus::Succeeded);
    tx.commit().await.unwrap();
    conn.close().await.unwrap();
    let mut conn = connection().await;
    let funds = FundsStore::funds(&mut conn, &d.user_id, &r.payer_account_id)
        .await
        .unwrap();
    assert_eq!(funds.booked.as_str(), "1088.00");
    assert_eq!(funds.reserved.as_str(), "0.00");
    let recipient = owner("recipient-settlement");
    assert_eq!(
        FundsStore::funds(&mut conn, &recipient, &resource("recipient-settlement"))
            .await
            .unwrap()
            .booked
            .as_str(),
        "200.00"
    );
    assert_eq!(
        FundsStore::transactions(&mut conn, &d.user_id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        FundsStore::transactions(&mut conn, &recipient)
            .await
            .unwrap()
            .len(),
        1
    );
    let mut tx = conn.begin().await.unwrap();
    let replay = FundsStore::submit(
        &mut tx,
        &d.user_id,
        &key("settlement"),
        &r,
        MockBehavior::Immediate,
        MockOutgoingState::Unknown,
    )
    .await
    .unwrap();
    assert_eq!(result, replay);
    tx.commit().await.unwrap();
    assert_eq!(
        FundsStore::funds(&mut conn, &d.user_id, &r.payer_account_id)
            .await
            .unwrap()
            .booked
            .as_str(),
        "1088.00"
    );
}

async fn connection() -> PgConnection {
    let options: sqlx::postgres::PgConnectOptions = std::env::var("TRANSFER_STORE_TEST_URL")
        .expect("use isolated check_postgres.ps1")
        .parse()
        .unwrap();
    assert_eq!(options.get_host(), "127.0.0.1");
    assert!(options
        .get_database()
        .unwrap_or_default()
        .starts_with("transfer_isolated_"));
    let mut connection = PgConnection::connect_with(&options).await.unwrap();
    let actual: String = sqlx::query_scalar("SELECT token FROM transfer_isolated_test_guard")
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(actual, std::env::var("TRANSFER_STORE_TEST_GUARD").unwrap());
    connection
}

fn owner(suffix: &str) -> UserId {
    UserId::parse(format!("funds-{suffix}")).unwrap()
}
fn resource(value: &str) -> ResourceId {
    ResourceId::parse(value).unwrap()
}

async fn draft(connection: &mut PgConnection, suffix: &str) -> StoredDraft {
    let now: i64 =
        sqlx::query_scalar("SELECT floor(extract(epoch from clock_timestamp()))::bigint")
            .fetch_one(connection)
            .await
            .unwrap();
    StoredDraft {
        draft_id: resource(&format!("draft-{suffix}")),
        operation_id: OperationId::parse(format!("operation-{suffix}")).unwrap(),
        task_id: TaskId::parse(format!("task-{suffix}")).unwrap(),
        user_id: owner(suffix),
        plan_version: 1,
        status: DraftStatus::ReadyToConfirm,
        expires_at: now as u64 + 3600,
        details: Some(DraftDetails {
            account: MaskedAccountView {
                account_id: resource(&format!("payer-{suffix}")),
                display_name: "模拟付款账户".into(),
                masked_account_no: "**** 0001".into(),
                currency: CnyCurrency::Cny,
            },
            payee: PayeeView {
                payee_id: resource(&format!("payee-{suffix}")),
                display_name: "模拟收款人".into(),
                bank_name: "模拟银行".into(),
                masked_account_no: "**** 1024".into(),
            },
            amount: DecimalAmount::parse("200.00").unwrap(),
            currency: CnyCurrency::Cny,
            remark: "模拟转账".into(),
            available_balance: DecimalAmount::parse("1288.00").unwrap(),
            balance_as_of: chrono::DateTime::from_timestamp(now, 0)
                .unwrap()
                .to_rfc3339(),
        }),
    }
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn saved_draft_survives_reconnect_and_stale_update_is_rejected() {
    let mut conn = connection().await;
    let original = draft(&mut conn, "draft-save").await;
    let mut tx = conn.begin().await.unwrap();
    FundsStore::save_draft(&mut tx, &original.user_id, None, &original)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    conn.close().await.unwrap();
    let mut conn = connection().await;
    assert_eq!(
        FundsStore::draft(&mut conn, &original.user_id, &original.draft_id)
            .await
            .unwrap(),
        original
    );
    let mut next = original.clone();
    next.plan_version = 2;
    next.details.as_mut().unwrap().amount = DecimalAmount::parse("300.00").unwrap();
    let mut tx = conn.begin().await.unwrap();
    FundsStore::save_draft(&mut tx, &original.user_id, Some(1), &next)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    assert!(matches!(
        FundsStore::save_draft(&mut tx, &original.user_id, Some(1), &original).await,
        Err(FundsError::VersionConflict)
    ));
    tx.rollback().await.unwrap();
    assert_eq!(
        FundsStore::draft(&mut conn, &original.user_id, &original.draft_id)
            .await
            .unwrap(),
        next
    );
}

#[tokio::test]
#[ignore = "requires an explicitly isolated PostgreSQL cluster"]
async fn restore_input_reads_current_sql_version_and_rejects_submitted_or_wrong_owner() {
    let mut conn = connection().await;
    let d = setup(&mut conn, "restore-input").await;
    let mut tx = conn.begin().await.unwrap();
    let recovered =
        FundsStore::draft_for_restore(&mut tx, &d.user_id, &d.draft_id, 1, "模拟收款人")
            .await
            .unwrap();
    assert_eq!(recovered.draft, d);
    assert!(!recovered.submitted);
    assert_eq!(recovered.expected_version, 1);
    assert_eq!(recovered.payee_query, "模拟收款人");
    tx.rollback().await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    assert!(matches!(
        FundsStore::draft_for_restore(&mut tx, &d.user_id, &d.draft_id, 2, "模拟收款人").await,
        Err(FundsError::VersionConflict)
    ));
    tx.rollback().await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    assert!(matches!(
        FundsStore::draft_for_restore(&mut tx, &owner("other"), &d.draft_id, 1, "模拟收款人").await,
        Err(FundsError::NotFound)
    ));
    tx.rollback().await.unwrap();
    submit(
        &mut conn,
        &d,
        "restore-input",
        MockBehavior::Pending,
        MockOutgoingState::Allowed,
    )
    .await;
    let mut tx = conn.begin().await.unwrap();
    assert!(matches!(
        FundsStore::draft_for_restore(&mut tx, &d.user_id, &d.draft_id, 1, "模拟收款人").await,
        Err(FundsError::AlreadySubmitted)
    ));
    tx.rollback().await.unwrap();
    assert_funds(&mut conn, &d, "1288.00", "200.00", "1088.00", 0).await;
}
