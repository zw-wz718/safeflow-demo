use super::*;
use safeflow_bank_gateway::DecimalAmount;
use safeflow_contracts::ResourceId;
use safeflow_transfer_integration::funds_store::FundsStore;

#[tokio::test]
#[ignore = "requires isolated PostgreSQL 16; existing API simulation session only"]
async fn transfer_sql_backend_reader_reuses_actual_api_session_without_sample_owner_aliases() {
    let db = Db::new().await;
    let state = ApiState::for_project_simulation(
        db.repo.clone(),
        ApiConfig::local_simulation(Zeroizing::new(SECRET.into())),
    )
    .unwrap();
    let cookie = state.0.sessions.issue(SECRET, None).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::COOKIE,
        cookie.split(';').next().unwrap().parse().unwrap(),
    );
    let identity = state
        .identity(&headers)
        .unwrap_or_else(|_| panic!("existing simulation login failed"));
    assert_eq!(identity.binding.user_id.as_str(), "user_001");
    let read = state.transfer_sql_reader_for_backend();
    let account = ResourceId::parse("isolated-api-sql-account").unwrap();
    // Test-only trusted provisioning, not automatic account creation in ApiState.
    let mut tx = db.scoped.begin().await.unwrap();
    FundsStore::provision_account(
        &mut tx,
        &UserId::parse("sql-owner-success").unwrap(),
        &account,
        &DecimalAmount::parse("2.00").unwrap(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        read.funds(&identity.reference, &account).await.unwrap_err(),
        StorageError::NotFound
    );
    let own_account = ResourceId::parse("isolated-api-owned-account").unwrap();
    let mut tx = db.scoped.begin().await.unwrap();
    FundsStore::provision_account(
        &mut tx,
        &identity.binding.user_id,
        &own_account,
        &DecimalAmount::parse("3.00").unwrap(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        read.funds(&identity.reference, &own_account)
            .await
            .unwrap()
            .available
            .as_str(),
        "3.00"
    );
    state.0.sessions.revoke(&identity.reference).unwrap();
    assert_eq!(
        read.funds(&identity.reference, &own_account)
            .await
            .unwrap_err(),
        StorageError::Session
    );
    assert_eq!(state.0.audit.events().unwrap().len(), 0);
    db.finish().await;
}
