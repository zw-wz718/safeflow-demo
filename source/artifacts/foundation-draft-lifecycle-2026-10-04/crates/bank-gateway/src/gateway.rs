use crate::{FindPayeesOutput, GetAccountBalanceOutput};
use safeflow_contracts::{ContractFuture, ResourceId, UserId};
use std::fmt;

/// The user must come from a server-resolved session, never from tool arguments.
pub trait BankGateway: Send + Sync {
    fn get_account_balance<'a>(
        &'a self,
        authenticated_user_id: &'a UserId,
        account_id: &'a ResourceId,
    ) -> ContractFuture<'a, Result<GetAccountBalanceOutput, BankError>>;

    fn find_payees<'a>(
        &'a self,
        authenticated_user_id: &'a UserId,
        query: &'a str,
    ) -> ContractFuture<'a, Result<FindPayeesOutput, BankError>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BankError {
    ValidationError,
    ResourceNotOwned,
    Unavailable,
    InvalidResponse,
}

impl BankError {
    pub fn reason_code(self) -> &'static str {
        match self {
            Self::ValidationError => "VALIDATION_ERROR",
            Self::ResourceNotOwned => "RESOURCE_NOT_OWNED",
            Self::Unavailable => "BANK_READ_UNAVAILABLE",
            Self::InvalidResponse => "INVALID_BANK_RESPONSE",
        }
    }
}

impl fmt::Display for BankError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::ValidationError => "invalid bank query input",
            Self::ResourceNotOwned => "resource is not accessible to the authenticated user",
            Self::Unavailable => "bank query service is unavailable",
            Self::InvalidResponse => "bank query response violates the contract",
        };
        write!(formatter, "{}: {message}", self.reason_code())
    }
}

impl std::error::Error for BankError {}
