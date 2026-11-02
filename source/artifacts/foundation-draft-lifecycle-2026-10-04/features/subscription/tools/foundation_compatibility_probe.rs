//! Compile-only probe. No permit creation, database execution or authorization.
use safeflow_storage::PermitTransaction;
use safeflow_subscription_evidence::confirmation_mock::{
    PendingRuntimeWriteMock, SubscriptionReadSnapshotMock, SubscriptionRuntimeMock,
};

pub async fn borrowed_write(
    transaction: &mut PermitTransaction,
    runtime: &SubscriptionRuntimeMock,
) -> Result<PendingRuntimeWriteMock<()>, String> {
    let connection = transaction.connection().map_err(|e| format!("{e:?}"))?;
    runtime
        .stage_on_connection_mock(connection, |_| Box::pin(async { Ok(()) }))
        .await
        .map_err(|e| format!("{e:?}"))
}

pub async fn borrowed_read(
    transaction: &mut PermitTransaction,
    trusted_owner: &str,
) -> Result<SubscriptionReadSnapshotMock, String> {
    let connection = transaction.connection().map_err(|e| format!("{e:?}"))?;
    SubscriptionReadSnapshotMock::load_on_connection_mock(connection, trusted_owner)
        .await
        .map_err(|e| format!("{e:?}"))
}
