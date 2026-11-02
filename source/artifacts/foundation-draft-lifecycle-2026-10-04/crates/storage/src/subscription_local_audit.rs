//! Candidate-r1 public encoding only. Never an execution DTO or a receipt builder.
use crate::StorageError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub(crate) const KIND: &str = "SUBSCRIPTION_LOCAL_CHANGE_RECORDED";
pub(crate) const MAPPING: &str = "safeflow.subscription.local-result.v1";

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(crate) enum LocalAction {
    Registration,
    ReminderSettings,
    RecordCancellation,
    RevokeCancellation,
}
impl LocalAction {
    pub(crate) fn result(self) -> &'static str {
        match self {
            Self::Registration => "ACTIVE",
            Self::ReminderSettings => "SETTINGS_SAVED",
            Self::RecordCancellation => "RECORDED",
            Self::RevokeCancellation => "REVOKED",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalDetails {
    pub(crate) mapping_version: String,
    pub(crate) action: LocalAction,
    pub(crate) result: String,
    pub(crate) business_ref: String,
    pub(crate) business_occurred_at: String,
    pub(crate) business_time_source: String,
    pub(crate) fictional: bool,
    pub(crate) production_registered: bool,
    pub(crate) notification_sent: bool,
}
// Same expressly narrow calendar/time grammar as the reviewed 408 candidate:
// positive four-digit year, actual Gregorian date, explicit offset, no leap :60.
pub(crate) fn valid_business_time(s: &str) -> bool {
    let b = s.as_bytes();
    if !s.is_ascii()
        || b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
        || ![0..4, 5..7, 8..10, 11..13, 14..16, 17..19]
            .iter()
            .all(|r| b[r.clone()].iter().all(u8::is_ascii_digit))
        || &s[..4] == "0000"
        || &s[11..13] > "23"
        || &s[14..16] > "59"
        || &s[17..19] > "59"
    {
        return false;
    }
    let mut i = 19;
    if b[i] == b'.' {
        i += 1;
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    let zone = &b[i..];
    let valid_zone = matches!(zone, [b'Z'] | [b'z'])
        || (zone.len() == 6
            && matches!(zone[0], b'+' | b'-')
            && zone[3] == b':'
            && zone[1..3].iter().all(u8::is_ascii_digit)
            && zone[4..6].iter().all(u8::is_ascii_digit)
            && &zone[1..3] <= b"23".as_slice()
            && &zone[4..6] <= b"59".as_slice());
    valid_zone && chrono::DateTime::parse_from_rfc3339(s).is_ok()
}
pub(crate) fn checked_details(value: &Value) -> Result<LocalDetails, StorageError> {
    let d: LocalDetails =
        serde_json::from_value(value.clone()).map_err(|_| StorageError::CorruptRecord)?;
    let id = uuid::Uuid::parse_str(&d.business_ref).map_err(|_| StorageError::CorruptRecord)?;
    if serde_json::to_value(&d).map_err(|_| StorageError::CorruptRecord)? != *value
        || d.mapping_version != MAPPING
        || d.result != d.action.result()
        || id.is_nil()
        || id.to_string() != d.business_ref
        || !valid_business_time(&d.business_occurred_at)
        || d.business_time_source != "FICTIONAL_COMMAND_NOW"
        || !d.fictional
        || d.production_registered
        || d.notification_sent
    {
        return Err(StorageError::CorruptRecord);
    }
    Ok(d)
}
pub(crate) fn check_envelope(value: &Value) -> Result<(), StorageError> {
    if value["public_event"]["kind"] == KIND {
        let e = value["public_event"]
            .as_object()
            .ok_or(StorageError::CorruptRecord)?;
        let keys = [
            "sequence",
            "event_id",
            "task_id",
            "operation_id",
            "kind",
            "task_revision",
            "operation_revision",
            "details",
            "occurred_at",
        ];
        if value.as_object().map(|v| v.len()) != Some(3)
            || value["storage_version"] != 1
            || !value["owner_id"].is_string()
            || e.len() != 9
            || keys.iter().any(|k| !e.contains_key(*k))
            || !e["operation_id"].is_string()
            || !e["operation_revision"].is_u64()
        {
            return Err(StorageError::CorruptRecord);
        }
        checked_details(&e["details"])?;
    }
    Ok(())
}
