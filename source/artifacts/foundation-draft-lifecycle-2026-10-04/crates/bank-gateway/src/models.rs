use crate::BankError;
use safeflow_contracts::ResourceId;
use serde::{de::Error as _, Deserialize, Deserializer, Serialize};

pub const MAX_RESOURCE_ID_LENGTH: usize = 128;
pub const MAX_QUERY_LENGTH: usize = 64;

/// Canonical nonnegative decimal string; no arithmetic or floating point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct DecimalAmount(String);

impl DecimalAmount {
    pub fn parse(value: impl Into<String>) -> Result<Self, BankError> {
        let value = value.into();
        let (integer, fraction) = value.split_once('.').ok_or(BankError::ValidationError)?;
        if integer.is_empty()
            || (integer.len() > 1 && integer.starts_with('0'))
            || !integer.bytes().all(|byte| byte.is_ascii_digit())
            || fraction.len() != 2
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(BankError::ValidationError);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for DecimalAmount {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CnyCurrency {
    #[serde(rename = "CNY")]
    Cny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetAccountBalanceInput {
    pub account_id: ResourceId,
}

impl GetAccountBalanceInput {
    pub fn validate(&self) -> Result<(), BankError> {
        validate_resource_id(&self.account_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindPayeesInput {
    pub query: String,
}

impl FindPayeesInput {
    pub fn validate(&self) -> Result<(), BankError> {
        if self.query.trim() != self.query {
            return Err(BankError::ValidationError);
        }
        validate_text(&self.query, MAX_QUERY_LENGTH)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaskedAccountView {
    pub account_id: ResourceId,
    pub display_name: String,
    pub masked_account_no: String,
    pub currency: CnyCurrency,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetAccountBalanceOutput {
    pub account: MaskedAccountView,
    pub available_balance: DecimalAmount,
    /// Adapter-provided RFC 3339 timestamp, not the application's current time.
    pub as_of: String,
}

impl GetAccountBalanceOutput {
    pub fn validate_for(&self, account_id: &ResourceId) -> Result<(), BankError> {
        if self.account.account_id != *account_id
            || validate_resource_id(&self.account.account_id).is_err()
            || validate_text(&self.account.display_name, 128).is_err()
            || !is_masked_account(&self.account.masked_account_no)
            || validate_text(&self.as_of, 64).is_err()
            || chrono::DateTime::parse_from_rfc3339(&self.as_of).is_err()
        {
            return Err(BankError::InvalidResponse);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayeeView {
    pub payee_id: ResourceId,
    pub display_name: String,
    pub bank_name: String,
    pub masked_account_no: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindPayeesOutput {
    pub candidates: Vec<PayeeView>,
}

impl FindPayeesOutput {
    pub fn validate(&self) -> Result<(), BankError> {
        let mut ids = std::collections::HashSet::new();
        for payee in &self.candidates {
            if validate_resource_id(&payee.payee_id).is_err()
                || validate_text(&payee.display_name, 128).is_err()
                || validate_text(&payee.bank_name, 128).is_err()
                || !is_masked_account(&payee.masked_account_no)
                || !ids.insert(&payee.payee_id)
            {
                return Err(BankError::InvalidResponse);
            }
        }
        Ok(())
    }
}

fn validate_resource_id(id: &ResourceId) -> Result<(), BankError> {
    let value = id.as_str();
    validate_text(value, MAX_RESOURCE_ID_LENGTH)?;
    if value.chars().any(char::is_whitespace) {
        return Err(BankError::ValidationError);
    }
    Ok(())
}

fn validate_text(value: &str, max_length: usize) -> Result<(), BankError> {
    if value.trim().is_empty()
        || value.chars().count() > max_length
        || value.chars().any(char::is_control)
    {
        return Err(BankError::ValidationError);
    }
    Ok(())
}

fn is_masked_account(value: &str) -> bool {
    value
        .strip_prefix("**** ")
        .is_some_and(|tail| tail.len() == 4 && tail.bytes().all(|byte| byte.is_ascii_digit()))
}
