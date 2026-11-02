//! Immutable read-only fixtures. No ledger, submission, or draft repository.

use safeflow_bank_gateway::{
    BankError, BankGateway, FindPayeesInput, FindPayeesOutput, GetAccountBalanceInput,
    GetAccountBalanceOutput, PayeeView,
};
use safeflow_contracts::{ContractFuture, ResourceId, UserId};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixtures {
    users: Vec<UserId>,
    accounts: Vec<OwnedAccount>,
    payees: Vec<OwnedPayee>,
    same_name_payees: Vec<OwnedPayee>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedAccount {
    owner: UserId,
    balance: GetAccountBalanceOutput,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedPayee {
    owner: UserId,
    payee: PayeeView,
}

pub struct MockBankGateway {
    fixtures: Fixtures,
}

impl Default for MockBankGateway {
    fn default() -> Self {
        Self::new()
    }
}

impl MockBankGateway {
    pub fn new() -> Self {
        Self {
            fixtures: serde_json::from_str(include_str!("fixtures.json"))
                .expect("checked-in bank fixtures satisfy the typed contract"),
        }
    }

    pub fn with_same_name_payees() -> Self {
        let mut bank = Self::new();
        bank.fixtures
            .payees
            .append(&mut bank.fixtures.same_name_payees);
        bank
    }

    fn require_user(&self, user: &UserId) -> Result<(), BankError> {
        if !self.fixtures.users.contains(user) {
            return Err(BankError::ResourceNotOwned);
        }
        Ok(())
    }
}

impl BankGateway for MockBankGateway {
    fn get_account_balance<'a>(
        &'a self,
        authenticated_user_id: &'a UserId,
        account_id: &'a ResourceId,
    ) -> ContractFuture<'a, Result<GetAccountBalanceOutput, BankError>> {
        Box::pin(async move {
            GetAccountBalanceInput {
                account_id: account_id.clone(),
            }
            .validate()?;
            self.require_user(authenticated_user_id)?;
            let account = self
                .fixtures
                .accounts
                .iter()
                .find(|account| {
                    account.owner == *authenticated_user_id
                        && account.balance.account.account_id == *account_id
                })
                .ok_or(BankError::ResourceNotOwned)?;
            account.balance.validate_for(account_id)?;
            Ok(account.balance.clone())
        })
    }

    fn find_payees<'a>(
        &'a self,
        authenticated_user_id: &'a UserId,
        query: &'a str,
    ) -> ContractFuture<'a, Result<FindPayeesOutput, BankError>> {
        Box::pin(async move {
            FindPayeesInput {
                query: query.to_owned(),
            }
            .validate()?;
            self.require_user(authenticated_user_id)?;
            let output = FindPayeesOutput {
                candidates: self
                    .fixtures
                    .payees
                    .iter()
                    .filter(|candidate| {
                        candidate.owner == *authenticated_user_id
                            && candidate.payee.display_name.contains(query)
                    })
                    .map(|candidate| candidate.payee.clone())
                    .collect(),
            };
            output.validate()?;
            Ok(output)
        })
    }
}
