use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use std::fmt;

macro_rules! string_id {
    ($name:ident, $label:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, IdentifierError> {
                let value = value.into();
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    return Err(IdentifierError::Empty($label));
                }
                if trimmed != value {
                    return Err(IdentifierError::SurroundingWhitespace($label));
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(value).map_err(D::Error::custom)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentifierError {
    Empty(&'static str),
    SurroundingWhitespace(&'static str),
    InvalidFormat(&'static str),
}

impl fmt::Display for IdentifierError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty(label) => write!(formatter, "{label} must not be empty"),
            Self::SurroundingWhitespace(label) => {
                write!(formatter, "{label} must not contain surrounding whitespace")
            }
            Self::InvalidFormat(label) => write!(formatter, "{label} has an invalid format"),
        }
    }
}

impl std::error::Error for IdentifierError {}

string_id!(AttemptId, "attempt_id");
string_id!(AuthGrantRef, "auth_grant_ref");
string_id!(BusinessOperationRef, "business_operation_ref");
string_id!(CheckId, "check_id");
string_id!(ConfirmationRef, "confirmation_ref");
string_id!(DecisionId, "decision_id");
string_id!(EventId, "event_id");
string_id!(EvidenceRef, "evidence_ref");
string_id!(IdempotencyKey, "idempotency_key");
string_id!(NodeId, "node_id");
string_id!(OperationId, "operation_id");
string_id!(PermitId, "permit_id");
string_id!(PolicyVersion, "policy_version");
string_id!(ReasonCode, "reason_code");
string_id!(ResourceId, "resource_id");
string_id!(SessionRef, "session_ref");
string_id!(SourceName, "source");
string_id!(TaskId, "task_id");
string_id!(ToolResultRef, "tool_result_ref");
string_id!(UserId, "user_id");

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct ToolName(String);

impl ToolName {
    pub fn parse(value: impl Into<String>) -> Result<Self, IdentifierError> {
        let value = value.into();
        if value.is_empty() {
            return Err(IdentifierError::Empty("tool_name"));
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(IdentifierError::InvalidFormat("tool_name"));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ToolName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(D::Error::custom)
    }
}

impl fmt::Display for ToolName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_reject_empty_and_surrounding_whitespace() {
        assert!(OperationId::parse("").is_err());
        assert!(OperationId::parse(" op_1").is_err());
        assert_eq!(OperationId::parse("op_1").unwrap().as_str(), "op_1");
    }

    #[test]
    fn tool_names_have_a_narrow_wire_format() {
        assert!(ToolName::parse("execute_transfer").is_ok());
        assert!(ToolName::parse("ExecuteTransfer").is_err());
        assert!(ToolName::parse("execute-transfer").is_err());
    }
}
