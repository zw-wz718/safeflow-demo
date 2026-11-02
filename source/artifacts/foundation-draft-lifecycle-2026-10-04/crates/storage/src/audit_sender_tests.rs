use super::*;
use serde_json::json;

fn policy() -> PublicAuditRetryPolicy {
    PublicAuditRetryPolicy::new(Duration::from_secs(1), 2, Duration::from_millis(100)).unwrap()
}
fn config(
    endpoint: &str,
    token: &str,
    receiver: &str,
) -> Result<PublicAuditSenderConfig, AuditSendFailure> {
    PublicAuditSenderConfig::new(endpoint, Zeroizing::new(token.into()), receiver, policy())
}
// Protocol unit input only, not a captured database pending or member business sample.
fn item() -> PendingPublicAudit {
    let envelope =
        json!({"public_event":{"event_id":"unit-event","task_id":"unit-task","operation_id":null}});
    PendingPublicAudit {
        event_id: EventId::parse("unit-event").unwrap(),
        digest: crate::models::hash(&envelope).unwrap(),
        envelope,
        historical: false,
        enqueued_at: "2026-10-21T00:00:00Z".parse().unwrap(),
    }
}
fn ack(p: &PendingPublicAudit) -> Value {
    json!({"ack_version":ACK_VERSION,"accepted":true,"duplicate":false,"event_id":p.event_id,"audit_id":"unit-audit","task_id":p.envelope["public_event"]["task_id"],"operation_id":p.envelope["public_event"]["operation_id"],"digest_echo":p.digest,"recorded_at":"2026-10-21T00:00:00Z"})
}

#[test]
fn audit_sender_configuration_fails_closed_and_errors_are_redacted() {
    for url in [
        "",
        "http://example.com/api/audit/outbox/v1",
        "http://localhost/api/audit/outbox/v1",
        "https://example.com/",
        "https://secret@example.com/api/audit/outbox/v1",
        "https://example.com/api/audit/outbox/v1?token=secret",
        "https://example.com/api/audit/outbox/v1#secret",
    ] {
        assert_eq!(
            config(url, "test-secret", "test-receiver").err(),
            Some(AuditSendFailure::Configuration)
        );
    }
    for token in ["", " contains spaces", "secret\r\nInjected: x", "密钥"] {
        assert_eq!(
            config(
                "https://example.com/api/audit/outbox/v1",
                token,
                "test-receiver"
            )
            .err(),
            Some(AuditSendFailure::Configuration)
        );
    }
    for receiver in ["", " ", "bad\nreceiver"] {
        assert_eq!(
            config(
                "https://example.com/api/audit/outbox/v1",
                "test-secret",
                receiver
            )
            .err(),
            Some(AuditSendFailure::Configuration)
        );
    }
    for absent in [
        "SAFEFLOW_PUBLIC_AUDIT_TOKEN",
        "SAFEFLOW_PUBLIC_AUDIT_ENDPOINT",
        "SAFEFLOW_PUBLIC_AUDIT_RECEIVER",
    ] {
        let result = PublicAuditSenderConfig::from_values(
            |key| {
                if key == absent {
                    return None;
                }
                Some(
                    match key {
                        "SAFEFLOW_PUBLIC_AUDIT_TOKEN" => "test-secret",
                        "SAFEFLOW_PUBLIC_AUDIT_ENDPOINT" => {
                            "https://example.com/api/audit/outbox/v1"
                        }
                        _ => "test-receiver",
                    }
                    .into(),
                )
            },
            policy(),
        );
        assert_eq!(result.err(), Some(AuditSendFailure::Configuration));
    }
    for (timeout, attempts, backoff) in [
        (0, 1, 100),
        (31000, 1, 100),
        (100, 0, 100),
        (100, 6, 100),
        (100, 1, 0),
        (100, 1, 31000),
    ] {
        assert!(PublicAuditRetryPolicy::new(
            Duration::from_millis(timeout),
            attempts,
            Duration::from_millis(backoff)
        )
        .is_err());
    }
    assert!(config(
        "http://127.0.0.1:12345/api/audit/outbox/v1",
        "test-secret",
        "test-receiver"
    )
    .is_ok());
    assert!(config(
        "https://example.com/api/audit/outbox/v1",
        "test-secret",
        "test-receiver"
    )
    .is_ok());
    assert_eq!(
        format!("{:?}", AuditSendFailure::Configuration),
        "Configuration"
    );
}

#[test]
fn audit_sender_ack_requires_every_field_and_exact_nullable_operation() {
    let p = item();
    let good = ack(&p);
    assert_eq!(
        verify_ack(&p, &serde_json::to_vec(&good).unwrap()).unwrap(),
        "unit-audit"
    );
    for key in good.as_object().unwrap().keys() {
        let mut bad = good.clone();
        bad.as_object_mut().unwrap().remove(key);
        assert_eq!(
            verify_ack(&p, &serde_json::to_vec(&bad).unwrap()),
            Err(AuditSendFailure::AckProtocol),
            "missing {key}"
        );
    }
    for (key, value) in [
        ("ack_version", json!("other")),
        ("accepted", json!(false)),
        ("event_id", json!("other")),
        ("task_id", json!("other")),
        ("operation_id", json!("other")),
        ("digest_echo", json!("0".repeat(64))),
        ("audit_id", json!("")),
        ("audit_id", json!(" invalid ")),
        ("recorded_at", json!("invalid")),
        ("duplicate", json!("true")),
        ("unexpected", json!(true)),
    ] {
        let mut bad = good.clone();
        bad[key] = value;
        assert_eq!(
            verify_ack(&p, &serde_json::to_vec(&bad).unwrap()),
            Err(AuditSendFailure::AckProtocol),
            "bad {key}"
        );
    }
    assert_eq!(
        verify_ack(&p, b"{broken"),
        Err(AuditSendFailure::AckProtocol)
    );
    let raw = serde_json::to_string(&good).unwrap();
    let duplicate = raw.replacen('{', "{\"accepted\":true,", 1);
    assert_eq!(
        verify_ack(&p, duplicate.as_bytes()),
        Err(AuditSendFailure::AckProtocol)
    );
    let mut non_null = p.clone();
    non_null.envelope["public_event"]["operation_id"] = json!("actual-operation");
    assert!(verify_ack(&non_null, raw.as_bytes()).is_err());
    assert!(verify_ack(&non_null, &serde_json::to_vec(&ack(&non_null)).unwrap()).is_ok());
}

#[test]
fn audit_sender_rejects_non_object_acks_without_losing_duplicate_key_checks() {
    let p = item();
    let good = ack(&p);
    let positional = json!([
        good["ack_version"],
        good["accepted"],
        good["duplicate"],
        good["event_id"],
        good["audit_id"],
        good["task_id"],
        good["operation_id"],
        good["digest_echo"],
        good["recorded_at"]
    ]);
    for body in [
        positional,
        json!([]),
        json!(null),
        json!(true),
        json!(17),
        json!("object"),
    ] {
        let raw = format!(" \r\n\t{}", serde_json::to_string(&body).unwrap());
        assert_eq!(
            verify_ack(&p, raw.as_bytes()),
            Err(AuditSendFailure::AckProtocol)
        );
    }
    let raw = serde_json::to_string(&good).unwrap();
    let duplicate = raw.replacen('{', "{\"accepted\":true,", 1);
    assert_eq!(
        verify_ack(&p, duplicate.as_bytes()),
        Err(AuditSendFailure::AckProtocol)
    );
    assert_eq!(
        verify_ack(&p, format!(" \r\n\t{raw} \t\r\n").as_bytes()).unwrap(),
        "unit-audit"
    );
}

#[test]
fn audit_sender_serialization_preserves_original_and_rejects_incomplete_source() {
    let mut p = item();
    p.envelope["public_event"]["details"] = json!({"note":"审计原文🙂"});
    p.historical = true;
    p.digest = crate::models::hash(&p.envelope).unwrap();
    let encoded: Value = serde_json::from_slice(&encode_pending(&p).unwrap()).unwrap();
    assert_eq!(encoded["envelope"], p.envelope);
    assert_eq!(encoded["digest"], json!(p.digest));
    assert_eq!(encoded["event_id"], json!(p.event_id));
    assert_eq!(encoded["historical"], true);
    assert_eq!(encoded["enqueued_at"], json!(p.enqueued_at));
    for key in ["event_id", "task_id", "operation_id"] {
        let mut invalid = p.clone();
        invalid.envelope["public_event"]
            .as_object_mut()
            .unwrap()
            .remove(key);
        assert_eq!(
            encode_pending(&invalid),
            Err(AuditSendFailure::InvalidPending)
        );
    }
}
