use crate::ResourceId;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const PAYMENT_INTENT_TOOL: &str = "execute_transfer";
pub const PAYMENT_LOCATOR_PROJECTION: &str = "payment-draft-locator-v1";
pub const PAYMENT_INTENT_POLICY: &str = "payment-intent-boundary-v1";

/// Backend adoption v1. These fields locate a business draft, never authorize payment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaymentDraftLocator {
    pub draft_id: ResourceId,
    pub draft_version: u32,
}
impl PaymentDraftLocator {
    pub fn schema() -> Value {
        json!({"type":"object","additionalProperties":false,
            "required":["draft_id","draft_version"],"properties":{
                "draft_id":{"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9_-]+$"},
                "draft_version":{"type":"integer","minimum":1,"maximum":4294967295_u64}}})
    }
}
