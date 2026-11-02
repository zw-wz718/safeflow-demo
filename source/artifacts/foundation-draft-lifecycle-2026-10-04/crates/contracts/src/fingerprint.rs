use crate::{OperationId, ResourceId, ToolName, UserId};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fmt;

pub const OPERATION_FINGERPRINT_VERSION: u16 = 1;
const DOMAIN_SEPARATOR: &str = "safeflow-operation-fingerprint:v1:";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FingerprintPayloadV1 {
    pub user_id: UserId,
    pub operation_id: OperationId,
    pub tool_name: ToolName,
    pub resource_id: ResourceId,
    pub normalized_arguments: Value,
    pub plan_version: u32,
    pub schema_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct OperationFingerprint(String);

impl OperationFingerprint {
    pub fn parse(value: impl Into<String>) -> Result<Self, FingerprintError> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(FingerprintError::InvalidDigest);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for OperationFingerprint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(D::Error::custom)
    }
}

impl fmt::Display for OperationFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FingerprintError {
    ZeroPlanVersion,
    ZeroSchemaVersion,
    FloatingPointNumber,
    Serialization(String),
    InvalidDigest,
}

impl fmt::Display for FingerprintError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroPlanVersion => formatter.write_str("plan_version must be greater than zero"),
            Self::ZeroSchemaVersion => {
                formatter.write_str("schema_version must be greater than zero")
            }
            Self::FloatingPointNumber => formatter.write_str(
                "normalized arguments must not contain floating-point numbers; use decimal strings",
            ),
            Self::Serialization(message) => write!(formatter, "serialization failed: {message}"),
            Self::InvalidDigest => {
                formatter.write_str("fingerprint must be 64 lowercase hexadecimal characters")
            }
        }
    }
}

impl std::error::Error for FingerprintError {}

pub fn compute_operation_fingerprint(
    payload: &FingerprintPayloadV1,
) -> Result<OperationFingerprint, FingerprintError> {
    if payload.plan_version == 0 {
        return Err(FingerprintError::ZeroPlanVersion);
    }
    if payload.schema_version == 0 {
        return Err(FingerprintError::ZeroSchemaVersion);
    }
    reject_floating_point_numbers(&payload.normalized_arguments)?;

    let value = serde_json::to_value(payload)
        .map_err(|error| FingerprintError::Serialization(error.to_string()))?;
    let mut canonical = String::new();
    write_canonical_json(&value, &mut canonical)?;

    let mut hasher = Sha256::new();
    hasher.update(DOMAIN_SEPARATOR.as_bytes());
    hasher.update(canonical.as_bytes());
    let bytes = hasher.finalize();
    let digest = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join("");
    OperationFingerprint::parse(digest)
}

fn reject_floating_point_numbers(value: &Value) -> Result<(), FingerprintError> {
    match value {
        Value::Number(number) if !(number.is_i64() || number.is_u64()) => {
            Err(FingerprintError::FloatingPointNumber)
        }
        Value::Array(values) => {
            for value in values {
                reject_floating_point_numbers(value)?;
            }
            Ok(())
        }
        Value::Object(values) => {
            for value in values.values() {
                reject_floating_point_numbers(value)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn write_canonical_json(value: &Value, output: &mut String) -> Result<(), FingerprintError> {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => output.push_str(&value.to_string()),
        Value::String(value) => output.push_str(
            &serde_json::to_string(value)
                .map_err(|error| FingerprintError::Serialization(error.to_string()))?,
        ),
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_canonical_json(value, output)?;
            }
            output.push(']');
        }
        Value::Object(values) => {
            output.push('{');
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(
                    &serde_json::to_string(key)
                        .map_err(|error| FingerprintError::Serialization(error.to_string()))?,
                );
                output.push(':');
                write_canonical_json(value, output)?;
            }
            output.push('}');
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn payload(arguments: Value) -> FingerprintPayloadV1 {
        FingerprintPayloadV1 {
            user_id: UserId::parse("user_001").unwrap(),
            operation_id: OperationId::parse("operation_001").unwrap(),
            tool_name: ToolName::parse("execute_transfer").unwrap(),
            resource_id: ResourceId::parse("draft_001").unwrap(),
            normalized_arguments: arguments,
            plan_version: 1,
            schema_version: 1,
        }
    }

    #[test]
    fn object_key_order_does_not_change_the_fingerprint() {
        let first = payload(json!({
            "amount": "200.00",
            "currency": "CNY",
            "payee_id": "payee_008"
        }));
        let second = payload(json!({
            "payee_id": "payee_008",
            "currency": "CNY",
            "amount": "200.00"
        }));
        assert_eq!(
            compute_operation_fingerprint(&first).unwrap(),
            compute_operation_fingerprint(&second).unwrap()
        );
    }

    #[test]
    fn changing_a_security_relevant_argument_changes_the_fingerprint() {
        let first = payload(json!({"amount": "200.00", "currency": "CNY"}));
        let second = payload(json!({"amount": "2000.00", "currency": "CNY"}));
        assert_ne!(
            compute_operation_fingerprint(&first).unwrap(),
            compute_operation_fingerprint(&second).unwrap()
        );
    }

    #[test]
    fn floating_point_money_is_rejected() {
        let payload = payload(json!({"amount": 200.0, "currency": "CNY"}));
        assert_eq!(
            compute_operation_fingerprint(&payload),
            Err(FingerprintError::FloatingPointNumber)
        );
    }

    #[test]
    fn frozen_v1_test_vector_is_stable() {
        let payload = payload(json!({
            "amount": "200.00",
            "currency": "CNY",
            "payee_id": "payee_008"
        }));
        assert_eq!(
            compute_operation_fingerprint(&payload).unwrap().as_str(),
            "de54ef29d7808fbe7711138d7bad82488d84cd0f40a058fdcb3f150b3ee3be12"
        );
    }
}
