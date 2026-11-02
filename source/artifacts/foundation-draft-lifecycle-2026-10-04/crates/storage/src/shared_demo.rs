use crate::{PgRepository, StorageError};
use safeflow_transfer_integration::{funds_store::FundsStore, shared_demo::SharedDemoPlan};
use std::collections::BTreeSet;

impl PgRepository {
    /// Apply only an explicitly adopted server-side plan; never reset balances.
    /// No session, login identity or final funding permission is created.
    pub async fn initialize_shared_demo(&self, plan: &SharedDemoPlan) -> Result<(), StorageError> {
        let mut tx = self.pool.begin().await?;
        let owners: BTreeSet<_> = plan.accounts().iter().map(|a| &a.owner_id).collect();
        for owner in owners {
            crate::daily_usage::protect_owner(&mut tx, owner).await?;
        }
        let mut accounts: Vec<_> = plan.accounts().iter().collect();
        accounts.sort_by_key(|a| a.account.account_id.as_str());
        for account in accounts {
            FundsStore::provision_account(
                &mut tx,
                &account.owner_id,
                &account.account.account_id,
                &account.opening_balance,
            )
            .await
            .map_err(crate::transfer_sql::business_error)?;
        }
        let route = plan.route();
        FundsStore::provision_payee(
            &mut tx,
            &route.owner_id,
            &route.payee_id,
            &route.recipient_account_id,
        )
        .await
        .map_err(crate::transfer_sql::business_error)?;
        tx.commit().await?;
        Ok(())
    }
}
