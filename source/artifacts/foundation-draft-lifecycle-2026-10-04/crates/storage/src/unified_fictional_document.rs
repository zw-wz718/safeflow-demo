//! Exact, explicitly adopted handoff wrapper; never an API input or permission.
use crate::StorageError;
use safeflow_bank_gateway::{DecimalAmount, MaskedAccountView, PayeeView};
use safeflow_contracts::{ResourceId, UserId};
use safeflow_transfer_integration::shared_demo::{DemoRecipient, SharedDemoPlan};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub const UNIFIED_FICTIONAL_COMMIT: &str = "7c03f03b832aa17c174b1bbdd4a6ad3282e01422";
pub const UNIFIED_FICTIONAL_SHA256: &str =
    "85b6fc1b3c708c617586afc3e8ea61a78e525d0811439f267c93620238e8c49d";
pub const UNIFIED_FICTIONAL_SOURCE: &str = "safeflow-team-simulation";
pub const UNIFIED_FICTIONAL_REVISION: &str = "safeflow-team-simulation-20261026-v1";
pub const UNIFIED_FICTIONAL_IMPLEMENTATION: &str = "foundation-unified-fictional-v1";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceAccount {
    pub owner_id: UserId,
    pub account: MaskedAccountView,
    pub opening_balance: DecimalAmount,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Route {
    pub owner_id: UserId,
    pub payee_id: ResourceId,
    pub recipient_account_id: ResourceId,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Plan {
    pub rule_version: String,
    pub accounts: Vec<SourceAccount>,
    pub payee: PayeeView,
    pub route: Route,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Scope {
    pub owner: UserId,
    pub accounts: Vec<ResourceId>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Card {
    pub card_id: ResourceId,
    pub last_four: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Catalog {
    pub owner: UserId,
    pub account: ResourceId,
    pub source_revision: String,
    pub cards: Vec<Card>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Document {
    artifact_kind: String,
    pub source_id: String,
    pub source_revision: String,
    maintainer: String,
    fictional: bool,
    project_route_adopted: bool,
    runtime_adopted: bool,
    permission_or_policy_grant: bool,
    scope: String,
    completeness_basis: String,
    initial_state: String,
    consumption_source: (),
    initial_subscription_ledger: (),
    legacy_card_mapping: (),
    pub complete_users_and_accounts_in_this_snapshot: Vec<Scope>,
    pub transfer_plan: Plan,
    pub card_catalogs: Vec<Catalog>,
}

/// Only the original fixed bytes can construct this backend adoption input.
/// Its existence does not create a session, grant or business permission.
pub struct UnifiedFictionalDocument {
    pub(crate) raw: Vec<u8>,
    pub(crate) data: Document,
}
impl UnifiedFictionalDocument {
    pub fn read_adopted_v1(raw: &[u8]) -> Result<Self, StorageError> {
        if raw.len() > 16 * 1024 || format!("{:x}", Sha256::digest(raw)) != UNIFIED_FICTIONAL_SHA256
        {
            return Err(StorageError::Invalid("unified source fixed bytes"));
        }
        let data = parse_and_validate(raw)?;
        Ok(Self {
            raw: raw.to_vec(),
            data,
        })
    }
    pub fn complete_accounts(&self, owner: &UserId) -> Result<Vec<ResourceId>, StorageError> {
        self.data
            .complete_users_and_accounts_in_this_snapshot
            .iter()
            .find(|s| s.owner == *owner)
            .map(|s| s.accounts.clone())
            .ok_or(StorageError::NotFound)
    }
    pub fn consumption_source(&self) -> Result<(), StorageError> {
        Err(StorageError::DependencyUnavailable)
    }
    pub fn initial_subscription_ledger(&self) -> Result<(), StorageError> {
        Err(StorageError::DependencyUnavailable)
    }
    pub(crate) fn plan(&self) -> Result<SharedDemoPlan, StorageError> {
        let recipient = self
            .data
            .transfer_plan
            .accounts
            .iter()
            .find(|a| a.account.account_id == self.data.transfer_plan.route.recipient_account_id)
            .ok_or(StorageError::CorruptRecord)?;
        SharedDemoPlan::prepare(DemoRecipient {
            owner_id: recipient.owner_id.clone(),
            account_id: recipient.account.account_id.clone(),
        })
        .map_err(|_| StorageError::CorruptRecord)
    }
}
fn parse_and_validate(raw: &[u8]) -> Result<Document, StorageError> {
    let d: Document = serde_json::from_slice(raw)
        .map_err(|_| StorageError::Invalid("unified source typed JSON"))?;
    // Unit fields intentionally require JSON null, not a made-up empty source.
    let _nulls = (
        d.consumption_source,
        d.initial_subscription_ledger,
        d.legacy_card_mapping,
    );
    if d.artifact_kind != "SOURCE_DOCUMENT_BUNDLE_NOT_API_OR_AUTHORIZATION"
        || d.source_id != UNIFIED_FICTIONAL_SOURCE || d.source_revision != UNIFIED_FICTIONAL_REVISION
        || d.maintainer != "逐风" || !d.fictional || !d.project_route_adopted
        || d.runtime_adopted || d.permission_or_policy_grant
        || d.scope != "New isolated demo only; never overwrite an existing database"
        || d.completeness_basis != "This newly authored fictional universe contains exactly the listed users, accounts and cards; no legacy or external records are included."
        || d.initial_state != "New empty isolated database only: opening balances from transfer_plan; no preexisting operations, holds or locks. Existing databases must be read and reconciled, never reset."
    { return Err(StorageError::Invalid("unified source provenance/scope")); }
    let mut ids = BTreeSet::new();
    let mut accounts = BTreeSet::new();
    for a in &d.transfer_plan.accounts {
        crate::models::valid_id(a.owner_id.as_str())?;
        crate::models::valid_id(a.account.account_id.as_str())?;
        if !ids.insert(a.account.account_id.as_str()) {
            return Err(StorageError::FactsConflict);
        }
        accounts.insert((a.owner_id.as_str(), a.account.account_id.as_str()));
        // Member's cents conversion is checked too; no float or truncation.
        opening_cents(&a.opening_balance)?;
    }
    let mut scopes = BTreeSet::new();
    let mut owners = BTreeSet::new();
    for s in &d.complete_users_and_accounts_in_this_snapshot {
        if !owners.insert(s.owner.as_str()) || s.accounts.is_empty() {
            return Err(StorageError::FactsConflict);
        }
        for a in &s.accounts {
            if !scopes.insert((s.owner.as_str(), a.as_str())) {
                return Err(StorageError::FactsConflict);
            }
        }
    }
    let mut catalogs = BTreeSet::new();
    let mut cards = BTreeSet::new();
    for c in &d.card_catalogs {
        let account = d
            .transfer_plan
            .accounts
            .iter()
            .find(|a| a.owner_id == c.owner && a.account.account_id == c.account)
            .ok_or(StorageError::FactsConflict)?;
        if c.source_revision != d.source_revision
            || c.cards.is_empty()
            || !catalogs.insert((c.owner.as_str(), c.account.as_str()))
        {
            return Err(StorageError::FactsConflict);
        }
        for card in &c.cards {
            crate::models::valid_id(card.card_id.as_str())?;
            if !cards.insert(card.card_id.as_str())
                || card.last_four.len() != 4
                || !card.last_four.bytes().all(|b| b.is_ascii_digit())
                || account.account.masked_account_no != format!("**** {}", card.last_four)
            {
                return Err(StorageError::FactsConflict);
            }
        }
    }
    if accounts.len() != 3 || accounts != scopes || accounts != catalogs || cards.len() != 3 {
        return Err(StorageError::FactsConflict);
    }
    let recipient = d
        .transfer_plan
        .accounts
        .iter()
        .find(|a| a.account.account_id == d.transfer_plan.route.recipient_account_id)
        .ok_or(StorageError::FactsConflict)?;
    let expected = SharedDemoPlan::prepare(DemoRecipient {
        owner_id: recipient.owner_id.clone(),
        account_id: recipient.account.account_id.clone(),
    })
    .map_err(|_| StorageError::FactsConflict)?;
    if serde_json::to_value(expected).map_err(|_| StorageError::CorruptRecord)?
        != serde_json::to_value(&d.transfer_plan).map_err(|_| StorageError::CorruptRecord)?
    {
        return Err(StorageError::FactsConflict);
    }
    Ok(d)
}
pub(crate) fn opening_cents(value: &DecimalAmount) -> Result<i64, StorageError> {
    value
        .as_str()
        .replace('.', "")
        .parse::<i64>()
        .map_err(|_| StorageError::Invalid("unified source amount range"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn raw() -> Vec<u8> {
        std::fs::read(std::env::var("SAFEFLOW_UNIFIED_SOURCE_FILE").expect("fixed source path"))
            .unwrap()
    }
    #[test]
    fn unified_document_original_and_null_dependencies() {
        let d = UnifiedFictionalDocument::read_adopted_v1(&raw()).unwrap();
        assert_eq!(
            d.complete_accounts(&UserId::parse("user_001").unwrap())
                .unwrap()[0]
                .as_str(),
            "account_001"
        );
        assert!(d
            .complete_accounts(&UserId::parse("user_demo").unwrap())
            .is_err());
        assert_eq!(
            d.consumption_source(),
            Err(StorageError::DependencyUnavailable)
        );
        assert_eq!(
            d.initial_subscription_ledger(),
            Err(StorageError::DependencyUnavailable)
        );
    }
    #[test]
    fn unified_document_rejects_semantic_and_type_mutations() {
        let original: serde_json::Value = serde_json::from_slice(&raw()).unwrap();
        for (pointer, replacement) in [
            ("/source_revision", serde_json::json!("wrong")),
            ("/permission_or_policy_grant", serde_json::json!(true)),
            (
                "/transfer_plan/accounts/0/opening_balance",
                serde_json::json!(1288.0),
            ),
            (
                "/transfer_plan/accounts/0/opening_balance",
                serde_json::json!("1288.001"),
            ),
            (
                "/transfer_plan/accounts/0/opening_balance",
                serde_json::json!("92233720368547758.08"),
            ),
            (
                "/transfer_plan/accounts/1/account/account_id",
                serde_json::json!("account_001"),
            ),
            (
                "/complete_users_and_accounts_in_this_snapshot/0/owner",
                serde_json::json!("user_002"),
            ),
            ("/card_catalogs/0/owner", serde_json::json!("user_002")),
            (
                "/card_catalogs/0/cards/0/last_four",
                serde_json::json!(6628),
            ),
            (
                "/card_catalogs/1/cards/0/card_id",
                serde_json::json!("demo_card_salary_v1"),
            ),
            ("/consumption_source", serde_json::json!([])),
            ("/initial_subscription_ledger", serde_json::json!({})),
        ] {
            let mut v = original.clone();
            *v.pointer_mut(pointer).unwrap() = replacement;
            let b = serde_json::to_vec(&v).unwrap();
            assert!(parse_and_validate(&b).is_err(), "{pointer}");
            assert!(UnifiedFictionalDocument::read_adopted_v1(&b).is_err());
        }
        let mut v = original;
        v["unexpected"] = serde_json::json!(true);
        assert!(parse_and_validate(&serde_json::to_vec(&v).unwrap()).is_err());
        let text = String::from_utf8(raw()).unwrap().replacen(
            "\"fictional\": true",
            "\"fictional\": true, \"fictional\": true",
            1,
        );
        assert!(parse_and_validate(text.as_bytes()).is_err());
    }
}
