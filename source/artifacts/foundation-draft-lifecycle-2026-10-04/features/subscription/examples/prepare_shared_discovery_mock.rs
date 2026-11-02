//! Local fictional shared-store candidate -> saved confirmation draft only.
//! Does not initialize data, confirm registration, send messages or grant access.
#[cfg(not(feature = "postgres-mock-store"))]
fn main() {
    eprintln!("POSTGRES_MOCK_STORE_FEATURE_REQUIRED");
    std::process::exit(1);
}
#[cfg(feature = "postgres-mock-store")]
#[tokio::main]
async fn main() -> std::process::ExitCode {
    use safeflow_subscription_evidence::confirmation_mock::shared_query_error_view_mock;
    let (view, ok) = match run().await {
        Ok(v) => (v, true),
        Err(e) => (shared_query_error_view_mock(e), false),
    };
    println!("{view}");
    if ok {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}
#[cfg(feature = "postgres-mock-store")]
async fn run() -> Result<
    serde_json::Value,
    safeflow_subscription_evidence::confirmation_mock::ProposedLedgerWriteIssue,
> {
    use safeflow_subscription_evidence::{confirmation_mock::*, Sample};
    use sqlx::{Connection, Executor};
    use std::str::FromStr;
    type Issue = ProposedLedgerWriteIssue;
    let a: Vec<_> = std::env::args().skip(1).collect();
    if a.len() != 7
        || a[0] != "--run-fictional"
        || !a[1].starts_with("sub_write_check_")
        || a[2].trim().is_empty()
    {
        return Err(Issue::Conflict);
    }
    let root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&a[3]).map_err(|_| Issue::Conflict)?)
            .map_err(|_| Issue::Conflict)?;
    if root["fictional"] != true
        || root["production_registered"] != false
        || root["provenance"] != "HANNAH_OWN_FICTIONAL_INPUT_NOT_A_JONATHAN_CAPTURE"
    {
        return Err(Issue::Conflict);
    }
    let sample: Sample =
        serde_json::from_value(root["sample"].clone()).map_err(|_| Issue::Conflict)?;
    if sample.trusted_context_fixture.user_id != a[2] {
        return Err(Issue::Conflict);
    }
    let index = a[4].parse::<usize>().map_err(|_| Issue::Conflict)?;
    let options = sqlx::postgres::PgConnectOptions::from_str(
        &std::env::var("SAFEFLOW_SUBSCRIPTION_SHARED_QUERY_URL").map_err(|_| Issue::Unavailable)?,
    )
    .map_err(|_| Issue::Unavailable)?;
    if options.get_host() != "127.0.0.1"
        || options.get_database() != Some("safeflow_subscription_write_test")
    {
        return Err(Issue::VerificationDatabaseMismatch);
    }
    let store = ProposedLedgerStoreMock::new(&a[1])?;
    let mut writer = sqlx::PgConnection::connect_with(&options)
        .await
        .map_err(|_| Issue::Unavailable)?;
    let mut reader = sqlx::PgConnection::connect_with(&options)
        .await
        .map_err(|_| Issue::Unavailable)?;
    let mut read = reader.begin().await.map_err(|_| Issue::Unavailable)?;
    read.execute("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
        .map_err(|_| Issue::Unavailable)?;
    let selection = store
        .select_discovery_candidate_on_connection_mock(
            &mut read,
            &a[2],
            &sample.trusted_context_fixture,
            &sample.snapshot,
            index,
            &a[5],
        )
        .await?;
    read.rollback().await.map_err(|_| Issue::Unavailable)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| Issue::Unavailable)?
        .as_nanos();
    // Fictional operation only. Real authenticated operation context belongs to the host.
    let operation = MockOperationContext {
        user_id: a[2].clone(),
        session_ref: "fictional-shared-discovery-session".into(),
        task_id: "fictional-shared-discovery".into(),
        node_id: "prepare".into(),
        operation_id: format!("fictional-selection-{nonce}"),
        idempotency_key: format!("fictional-selection-{nonce}"),
        plan_version: 1,
    };
    let mut tx = writer.begin().await.map_err(|_| Issue::Unavailable)?;
    let mut work = store.load_work_on_connection_mock(&mut tx, &a[2]).await?;
    let provisional = work
        .prepare_registration_from_selection_mock(selection, operation, &a[5], &a[6])
        .await?;
    let binding = provisional.confirmation_binding;
    let pending = store.save_on_connection_mock(&mut tx, work).await?;
    tx.commit().await.map_err(|_| Issue::Unavailable)?;
    // A failed readback may follow a successful COMMIT: do not report saved success
    // or retry blindly. The host must query the original operation before retrying.
    let saved = pending
        .read_committed_on_connection_mock(&mut reader)
        .await?;
    let preview = saved.registration_preview_mock(&binding.draft_id, &binding.context, &a[5])?;
    Ok(
        serde_json::json!({"fictional":true,"saved":true,"registered":false,"state":"AWAITING_CONFIRMATION","preview":preview}),
    )
}
