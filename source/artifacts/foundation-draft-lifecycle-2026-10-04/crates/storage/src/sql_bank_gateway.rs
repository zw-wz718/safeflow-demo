//! Overlay actual SQL balances on an independently trusted display/payee source.
//! Does not derive recipient routes, provision accounts, or replace identities.
use crate::PgRepository;
use safeflow_bank_gateway::{BankError, BankGateway, FindPayeesOutput, GetAccountBalanceOutput};
use safeflow_contracts::{ContractFuture, ResourceId, UserId};
use safeflow_transfer_integration::funds_store::{FundsError, FundsStore};
use std::sync::Arc;

#[derive(Clone)]
pub struct TransferSqlBankGateway {
    repository: PgRepository,
    catalog: Arc<dyn BankGateway>,
}
impl TransferSqlBankGateway {
    pub fn for_trusted_backend(repository: PgRepository, catalog: Arc<dyn BankGateway>) -> Self {
        Self {
            repository,
            catalog,
        }
    }
}
impl BankGateway for TransferSqlBankGateway {
    fn get_account_balance<'a>(
        &'a self,
        owner: &'a UserId,
        account: &'a ResourceId,
    ) -> ContractFuture<'a, Result<GetAccountBalanceOutput, BankError>> {
        Box::pin(async move {
            let mut view = self.catalog.get_account_balance(owner, account).await?;
            view.validate_for(account)?;
            let mut tx = self
                .repository
                .pool
                .begin()
                .await
                .map_err(|_| BankError::Unavailable)?;
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *tx)
                .await
                .map_err(|_| BankError::Unavailable)?;
            let funds = FundsStore::funds(&mut tx, owner, account)
                .await
                .map_err(|error| match error {
                    FundsError::NotFound => BankError::ResourceNotOwned,
                    FundsError::Database(_) => BankError::Unavailable,
                    _ => BankError::InvalidResponse,
                })?;
            let now: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
                .fetch_one(&mut *tx)
                .await
                .map_err(|_| BankError::Unavailable)?;
            view.available_balance = funds.available;
            view.as_of = now.to_rfc3339();
            view.validate_for(account)?;
            tx.commit().await.map_err(|_| BankError::Unavailable)?;
            Ok(view)
        })
    }
    fn find_payees<'a>(
        &'a self,
        owner: &'a UserId,
        query: &'a str,
    ) -> ContractFuture<'a, Result<FindPayeesOutput, BankError>> {
        self.catalog.find_payees(owner, query)
    }
}
