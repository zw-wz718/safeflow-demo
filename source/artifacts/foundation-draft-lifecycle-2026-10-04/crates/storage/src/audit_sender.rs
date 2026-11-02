//! Explicit trusted-backend adapter. Never registered as an HTTP route or LLM tool.
use crate::{models::valid_id, PendingPublicAudit, PublicAuditOutbox, StorageError};
use chrono::{DateTime, Utc};
use reqwest::{header, Client, Url};
use safeflow_contracts::EventId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use zeroize::Zeroizing;

const MAX_ACK_BYTES: usize = 16384;
const MAX_REQUEST_BYTES: usize = 1048576;
const ACK_VERSION: &str = "audit-receiver-candidate-v1";

/// All retry parameters are explicit trusted deployment choices, not business rules.
#[derive(Clone, Copy)]
pub struct PublicAuditRetryPolicy {
    timeout: Duration,
    attempts: u8,
    backoff: Duration,
}
impl PublicAuditRetryPolicy {
    pub fn new(
        timeout: Duration,
        attempts: u8,
        backoff: Duration,
    ) -> Result<Self, AuditSendFailure> {
        if !(Duration::from_millis(100)..=Duration::from_secs(30)).contains(&timeout)
            || !(1..=5).contains(&attempts)
            || !(Duration::from_millis(100)..=Duration::from_secs(30)).contains(&backoff)
        {
            return Err(AuditSendFailure::Configuration);
        }
        Ok(Self {
            timeout,
            attempts,
            backoff,
        })
    }
}

/// No Debug/Serialize: token, endpoint and receiver are never included in reports.
pub struct PublicAuditSenderConfig {
    endpoint: Url,
    token: Zeroizing<String>,
    receiver: String,
    retry: PublicAuditRetryPolicy,
}
impl PublicAuditSenderConfig {
    pub fn new(
        endpoint: &str,
        token: Zeroizing<String>,
        receiver: &str,
        retry: PublicAuditRetryPolicy,
    ) -> Result<Self, AuditSendFailure> {
        let invalid = AuditSendFailure::Configuration;
        let url = Url::parse(endpoint).map_err(|_| invalid)?;
        let local_http =
            url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "[::1]"));
        if endpoint.trim() != endpoint
            || endpoint.len() > 2048
            || (url.scheme() != "https" && !local_http)
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.path() != "/api/audit/outbox/v1"
            || token.is_empty()
            || token.len() > 4096
            || !token.bytes().all(|b| b.is_ascii_graphic())
            || valid_id(receiver).is_err()
        {
            return Err(invalid);
        }
        Ok(Self {
            endpoint: url,
            token,
            receiver: receiver.to_owned(),
            retry,
        })
    }

    /// Reads only trusted backend environment. Missing/non-Unicode values fail closed.
    /// Reading this configuration does not start a worker or make a request.
    pub fn from_env(retry: PublicAuditRetryPolicy) -> Result<Self, AuditSendFailure> {
        Self::from_values(|name| std::env::var(name).ok(), retry)
    }
    fn from_values(
        read: impl Fn(&str) -> Option<String>,
        retry: PublicAuditRetryPolicy,
    ) -> Result<Self, AuditSendFailure> {
        let token = Zeroizing::new(
            read("SAFEFLOW_PUBLIC_AUDIT_TOKEN").ok_or(AuditSendFailure::Configuration)?,
        );
        let endpoint =
            read("SAFEFLOW_PUBLIC_AUDIT_ENDPOINT").ok_or(AuditSendFailure::Configuration)?;
        let receiver =
            read("SAFEFLOW_PUBLIC_AUDIT_RECEIVER").ok_or(AuditSendFailure::Configuration)?;
        Self::new(&endpoint, token, &receiver, retry)
    }
}

/// Redacted classifications only: never retain response bodies, URLs or credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditSendFailure {
    Configuration,
    InvalidPending,
    Transport,
    Http(u16),
    AckProtocol,
    Acknowledge,
}

#[derive(Debug)]
pub struct PublicAuditSendResult {
    pub event_id: EventId,
    pub attempts: u8,
    /// Ok means the original local acknowledge call succeeded, not just HTTP success.
    pub outcome: Result<(), AuditSendFailure>,
}

pub struct PublicAuditSender {
    outbox: PublicAuditOutbox,
    config: PublicAuditSenderConfig,
    client: Client,
}
impl PublicAuditSender {
    pub fn for_trusted_backend(
        outbox: PublicAuditOutbox,
        config: PublicAuditSenderConfig,
    ) -> Result<Self, AuditSendFailure> {
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .http1_only()
            .pool_max_idle_per_host(0)
            .connect_timeout(config.retry.timeout)
            .timeout(config.retry.timeout)
            .build()
            .map_err(|_| AuditSendFailure::Configuration)?;
        Ok(Self {
            outbox,
            config,
            client,
        })
    }

    /// Explicit single batch, no startup/background activation. Only pending + ACK
    /// APIs are reachable here: no business execution, permit or funds callbacks.
    pub async fn send_pending(
        &self,
        limit: u16,
    ) -> Result<Vec<PublicAuditSendResult>, StorageError> {
        let mut results = Vec::new();
        for item in self.outbox.pending(limit).await? {
            let mut attempts = 0;
            let outcome = match encode_pending(&item) {
                Err(error) => Err(error),
                Ok(body) => loop {
                    attempts += 1;
                    match self.request(&item, body.clone()).await {
                        Ok(audit_id) => {
                            break self
                                .outbox
                                .acknowledge(
                                    &item.event_id,
                                    &item.digest,
                                    &self.config.receiver,
                                    &audit_id,
                                )
                                .await
                                .map_err(|_| AuditSendFailure::Acknowledge);
                        }
                        Err(error) => {
                            let transient = matches!(
                                error,
                                AuditSendFailure::Transport
                                    | AuditSendFailure::Http(502 | 503 | 504)
                            );
                            if !transient || attempts >= self.config.retry.attempts {
                                break Err(error);
                            }
                            let delay = self
                                .config
                                .retry
                                .backoff
                                .saturating_mul(1 << (attempts - 1))
                                .min(Duration::from_secs(30));
                            tokio::time::sleep(delay).await;
                        }
                    }
                },
            };
            results.push(PublicAuditSendResult {
                event_id: item.event_id,
                attempts,
                outcome,
            });
        }
        Ok(results)
    }

    async fn request(
        &self,
        pending: &PendingPublicAudit,
        body: Vec<u8>,
    ) -> Result<String, AuditSendFailure> {
        let auth_raw = Zeroizing::new(format!("Bearer {}", self.config.token.as_str()));
        let mut authorization = header::HeaderValue::from_str(&auth_raw)
            .map_err(|_| AuditSendFailure::Configuration)?;
        authorization.set_sensitive(true);
        let mut response = self
            .client
            .post(self.config.endpoint.clone())
            .header(header::AUTHORIZATION, authorization)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|_| AuditSendFailure::Transport)?;
        let status = response.status().as_u16();
        if !matches!(status, 200 | 201) {
            return Err(AuditSendFailure::Http(status));
        }
        let json_type = response
            .headers()
            .get_all(header::CONTENT_TYPE)
            .iter()
            .count()
            == 1
            && response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|s| {
                    s.split(';')
                        .next()
                        .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"))
                });
        if !json_type
            || response.headers().contains_key(header::CONTENT_ENCODING)
            || response
                .content_length()
                .is_some_and(|n| n > MAX_ACK_BYTES as u64)
        {
            return Err(AuditSendFailure::AckProtocol);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| AuditSendFailure::Transport)?
        {
            if chunk.len() > MAX_ACK_BYTES.saturating_sub(bytes.len()) {
                return Err(AuditSendFailure::AckProtocol);
            }
            bytes.extend_from_slice(&chunk);
        }
        verify_ack(pending, &bytes)
    }
}

#[derive(Serialize)]
struct WirePending<'a> {
    event_id: &'a EventId,
    envelope: &'a Value,
    digest: &'a safeflow_contracts::ArgumentsHash,
    historical: bool,
    enqueued_at: DateTime<Utc>,
}
fn encode_pending(p: &PendingPublicAudit) -> Result<Vec<u8>, AuditSendFailure> {
    let e = &p.envelope["public_event"];
    if e["event_id"].as_str() != Some(p.event_id.as_str())
        || e["task_id"].as_str().is_none_or(|id| valid_id(id).is_err())
        || !matches!(
            e.get("operation_id"),
            Some(Value::Null) | Some(Value::String(_))
        )
        || e["operation_id"]
            .as_str()
            .is_some_and(|id| valid_id(id).is_err())
    {
        return Err(AuditSendFailure::InvalidPending);
    }
    let body = serde_json::to_vec(&WirePending {
        event_id: &p.event_id,
        envelope: &p.envelope,
        digest: &p.digest,
        historical: p.historical,
        enqueued_at: p.enqueued_at,
    })
    .map_err(|_| AuditSendFailure::InvalidPending)?;
    if body.len() > MAX_REQUEST_BYTES {
        return Err(AuditSendFailure::InvalidPending);
    }
    Ok(body)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ack {
    ack_version: String,
    accepted: bool,
    duplicate: bool,
    event_id: String,
    audit_id: String,
    task_id: String,
    // Value makes explicit null distinct from an absent field, unlike Option.
    operation_id: Value,
    digest_echo: String,
    recorded_at: DateTime<Utc>,
}
fn verify_ack(p: &PendingPublicAudit, raw: &[u8]) -> Result<String, AuditSendFailure> {
    // Derived structs also accept positional sequences. Require the JSON object
    // token, then deserialize ORIGINAL bytes so duplicate keys remain detectable.
    if raw
        .iter()
        .find(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        != Some(&b'{')
    {
        return Err(AuditSendFailure::AckProtocol);
    }
    let ack: Ack = serde_json::from_slice(raw).map_err(|_| AuditSendFailure::AckProtocol)?;
    if ack.ack_version != ACK_VERSION
        || !ack.accepted
        || ack.event_id != p.event_id.as_str()
        || Some(ack.task_id.as_str()) != p.envelope["public_event"]["task_id"].as_str()
        || ack.operation_id != p.envelope["public_event"]["operation_id"]
        || ack.digest_echo != p.digest.as_str()
        || valid_id(&ack.audit_id).is_err()
    {
        return Err(AuditSendFailure::AckProtocol);
    }
    // Both duplicate values are valid; neither the flag nor timestamp is authority.
    let _ = (ack.duplicate, ack.recorded_at);
    Ok(ack.audit_id)
}

#[cfg(test)]
#[path = "audit_sender_tests.rs"]
mod tests;
