//! Transfer-owned shared DEMO provisioning plan, not an identity authority.
//!
//! Existing user_001/account_001 and user_002/account_002 stay unchanged. The
//! foundation must explicitly adopt the receiving account and its trusted owner.
//! Preparing this plan does not log anyone in, write SQL or reset balances.
//! To apply an adopted plan, call existing FundsStore::provision_account/payee
//! using the foundation's single transaction/connection; it owns commit/rollback.
//! Do not UPDATE balances on startup. Existing provisioning checks opening facts
//! on replay without replacing current booked balances.

use safeflow_bank_gateway::{CnyCurrency, DecimalAmount, MaskedAccountView, PayeeView};
use safeflow_contracts::{ResourceId, UserId};
use serde::Serialize;

pub const SHARED_DEMO_RULE_VERSION: &str = "transfer-shared-demo-2026-10-17-v1";

/// Backend configuration, never a customer-supplied transfer argument. No
/// default identity/ID: fixture IDs from examples are NOT adopted public IDs.
pub struct DemoRecipient {
    pub owner_id: UserId,
    pub account_id: ResourceId,
}

#[derive(Debug, Serialize)]
pub struct DemoAccount {
    pub owner_id: UserId,
    pub account: MaskedAccountView,
    pub opening_balance: DecimalAmount,
}

#[derive(Debug, Serialize)]
pub struct DemoRoute {
    pub owner_id: UserId,
    pub payee_id: ResourceId,
    pub recipient_account_id: ResourceId,
}

/// Read-only plan with validated explicit routing; not a shared database seed.
#[derive(Debug, Serialize)]
pub struct SharedDemoPlan {
    rule_version: &'static str,
    accounts: Vec<DemoAccount>,
    payee: PayeeView,
    route: DemoRoute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemoConfigError {
    InvalidIdentifier,
    ExistingAccountCollision,
    PayerIsRecipient,
}
impl std::fmt::Display for DemoConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "shared demo configuration rejected: {self:?}")
    }
}
impl std::error::Error for DemoConfigError {}

impl SharedDemoPlan {
    pub fn prepare(recipient: DemoRecipient) -> Result<Self, DemoConfigError> {
        for id in [recipient.owner_id.as_str(), recipient.account_id.as_str()] {
            if id.len() > 128 || id.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(DemoConfigError::InvalidIdentifier);
            }
        }
        if recipient.owner_id.as_str() == "user_001" {
            return Err(DemoConfigError::PayerIsRecipient);
        }
        if matches!(recipient.account_id.as_str(), "account_001" | "account_002") {
            return Err(DemoConfigError::ExistingAccountCollision);
        }
        let account = |owner, id, label: &str, mask: &str, opening| DemoAccount {
            owner_id: UserId::parse(owner).expect("static owner"),
            account: MaskedAccountView {
                account_id: ResourceId::parse(id).expect("static account"),
                display_name: label.into(),
                masked_account_no: mask.into(),
                currency: CnyCurrency::Cny,
            },
            opening_balance: DecimalAmount::parse(opening).expect("static decimal"),
        };
        Ok(Self {
            rule_version: SHARED_DEMO_RULE_VERSION,
            accounts: vec![
                account("user_001", "account_001", "工资卡", "**** 6628", "1288.00"),
                account("user_002", "account_002", "备用卡", "**** 7788", "50.00"),
                DemoAccount {
                    owner_id: recipient.owner_id,
                    account: MaskedAccountView {
                        account_id: recipient.account_id.clone(),
                        display_name: "小李模拟收款账户".into(),
                        masked_account_no: "**** 1024".into(),
                        currency: CnyCurrency::Cny,
                    },
                    opening_balance: DecimalAmount::parse("0.00").expect("static decimal"),
                },
            ],
            payee: PayeeView {
                payee_id: ResourceId::parse("payee_008").expect("static payee"),
                display_name: "小李".into(),
                bank_name: "示例银行".into(),
                masked_account_no: "**** 1024".into(),
            },
            route: DemoRoute {
                owner_id: UserId::parse("user_001").expect("static owner"),
                payee_id: ResourceId::parse("payee_008").expect("static payee"),
                recipient_account_id: recipient.account_id,
            },
        })
    }

    pub fn accounts(&self) -> &[DemoAccount] {
        &self.accounts
    }
    pub fn route(&self) -> &DemoRoute {
        &self.route
    }
    pub fn payee(&self) -> &PayeeView {
        &self.payee
    }
}
