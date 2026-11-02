//! Trusted process lifecycle only; never a route, Tool, or business executor.
use axum::Router;
use safeflow_storage::{
    PublicAuditOutbox, PublicAuditRetryPolicy, PublicAuditSender, PublicAuditSenderConfig,
};
use std::{ffi::OsString, future::Future, time::Duration};
use tokio::{net::TcpListener, sync::watch, task::JoinHandle};
use zeroize::Zeroizing;

const ENABLED: &str = "SAFEFLOW_PUBLIC_AUDIT_ENABLED";
const SETTINGS: [&str; 9] = [
    "SAFEFLOW_PUBLIC_AUDIT_ENDPOINT",
    "SAFEFLOW_PUBLIC_AUDIT_TOKEN",
    "SAFEFLOW_PUBLIC_AUDIT_RECEIVER",
    "SAFEFLOW_PUBLIC_AUDIT_POLL_MS",
    "SAFEFLOW_PUBLIC_AUDIT_BATCH",
    "SAFEFLOW_PUBLIC_AUDIT_TIMEOUT_MS",
    "SAFEFLOW_PUBLIC_AUDIT_ATTEMPTS",
    "SAFEFLOW_PUBLIC_AUDIT_BACKOFF_MS",
    "SAFEFLOW_PUBLIC_AUDIT_STOP_GRACE_MS",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditWorkerFailure {
    Configuration,
    Http,
    Shutdown,
}
impl std::fmt::Display for AuditWorkerFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Configuration => "invalid public audit worker configuration",
            Self::Http => "local HTTP transport unavailable",
            Self::Shutdown => "public audit worker shutdown failed",
        })
    }
}
impl std::error::Error for AuditWorkerFailure {}

// Deliberately no Debug/Serialize; endpoint, token and receiver are not logs.
pub struct AuditWorkerConfig {
    sender: PublicAuditSenderConfig,
    poll: Duration,
    batch: u16,
    stop_grace: Duration,
}
impl AuditWorkerConfig {
    /// No network or Outbox reads. Missing switch disables only when ALL related
    /// settings are absent. Residual, partial and non-Unicode values fail closed.
    pub fn from_env() -> Result<Option<Self>, AuditWorkerFailure> {
        Self::from_values(|name| std::env::var_os(name))
    }

    fn from_values(
        read: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Option<Self>, AuditWorkerFailure> {
        use AuditWorkerFailure::Configuration as Invalid;
        let switch = read(ENABLED)
            .map(|v| v.into_string().map_err(|_| Invalid))
            .transpose()?;
        match switch.as_deref() {
            None | Some("0") if SETTINGS.iter().all(|name| read(name).is_none()) => {
                return Ok(None)
            }
            Some("1") => {}
            _ => return Err(Invalid),
        }
        let text = |name: &str| {
            read(name)
                .ok_or(Invalid)?
                .into_string()
                .map_err(|_| Invalid)
        };
        let number = |name: &str, min, max| {
            let value = text(name)?;
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Invalid);
            }
            let value = value.parse::<u64>().map_err(|_| Invalid)?;
            if !(min..=max).contains(&value) {
                return Err(Invalid);
            }
            Ok(value)
        };
        let poll = Duration::from_millis(number(SETTINGS[3], 100, 60000)?);
        let batch = number(SETTINGS[4], 1, 1000)? as u16;
        let timeout = Duration::from_millis(number(SETTINGS[5], 100, 30000)?);
        let attempts = number(SETTINGS[6], 1, 5)? as u8;
        let backoff = Duration::from_millis(number(SETTINGS[7], 100, 30000)?);
        let stop_grace = Duration::from_millis(number(SETTINGS[8], 100, 5000)?);
        let token = Zeroizing::new(text(SETTINGS[1])?);
        let retry = PublicAuditRetryPolicy::new(timeout, attempts, backoff).map_err(|_| Invalid)?;
        let sender =
            PublicAuditSenderConfig::new(&text(SETTINGS[0])?, token, &text(SETTINGS[2])?, retry)
                .map_err(|_| Invalid)?;
        Ok(Some(Self {
            sender,
            poll,
            batch,
            stop_grace,
        }))
    }
}

struct Worker {
    stop: watch::Sender<bool>,
    job: Option<JoinHandle<()>>,
    grace: Duration,
}
impl Worker {
    fn start(
        outbox: PublicAuditOutbox,
        config: AuditWorkerConfig,
    ) -> Result<Self, AuditWorkerFailure> {
        let sender = PublicAuditSender::for_trusted_backend(outbox, config.sender)
            .map_err(|_| AuditWorkerFailure::Configuration)?;
        let (stop, mut stopping) = watch::channel(false);
        let job = tokio::spawn(async move {
            loop {
                let result = tokio::select! {
                    biased;
                    _ = stopping.changed() => break,
                    result = sender.send_pending(config.batch) => result,
                };
                match result {
                    Ok(results) if !results.is_empty() => {
                        let accepted = results.iter().filter(|r| r.outcome.is_ok()).count();
                        let rejected = results.len() - accepted;
                        // Counts only: no IDs, receiver, payload, URL or errors.
                        eprintln!(
                            "public audit worker: acknowledged={accepted} unconfirmed={rejected}"
                        );
                    }
                    Err(_) => eprintln!("public audit worker: outbox unavailable; retry deferred"),
                    _ => {}
                }
                // Includes permanent protocol errors, DB failures and empty batches.
                // No second queue, new IDs, quarantine or immediate busy retry.
                tokio::select! {
                    biased;
                    _ = stopping.changed() => break,
                    _ = tokio::time::sleep(config.poll) => {}
                }
            }
        });
        Ok(Self {
            stop,
            job: Some(job),
            grace: config.stop_grace,
        })
    }

    async fn finish(&mut self) -> Result<(), AuditWorkerFailure> {
        let _ = self.stop.send(true);
        let Some(job) = self.job.as_mut() else {
            return Ok(());
        };
        match tokio::time::timeout(self.grace, &mut *job).await {
            Ok(result) => result.map_err(|_| AuditWorkerFailure::Shutdown)?,
            Err(_) => {
                job.abort();
                // Even cancellation join is bounded; Drop retains abort fallback.
                let joined = tokio::time::timeout(self.grace, &mut *job)
                    .await
                    .map_err(|_| AuditWorkerFailure::Shutdown)?;
                if joined.is_err_and(|e| !e.is_cancelled()) {
                    return Err(AuditWorkerFailure::Shutdown);
                }
            }
        }
        self.job.take();
        Ok(())
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(job) = self.job.take() {
            job.abort();
        }
    }
}

/// The listener and application must already be validated before entry. The
/// worker owns only an Outbox; repository closure belongs to the outer process.
/// Shutdown cancels in-flight send/ACK waits. Commit/remote-storage uncertainty
/// is resolved through the original event/digest on restart, never business retry.
pub async fn serve_with_public_audit(
    listener: TcpListener,
    app: Router,
    outbox: PublicAuditOutbox,
    config: Option<AuditWorkerConfig>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), AuditWorkerFailure> {
    let mut worker = config.map(|c| Worker::start(outbox, c)).transpose()?;
    let stop = worker.as_ref().map(|w| w.stop.clone());
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.await;
            // Signal BEFORE draining HTTP, so audit unavailability cannot delay it.
            if let Some(stop) = stop {
                let _ = stop.send(true);
            }
        })
        .await;
    if let Some(worker) = worker.as_mut() {
        worker.finish().await?;
    }
    served.map_err(|_| AuditWorkerFailure::Http)
}

#[cfg(test)]
#[path = "audit_worker_tests.rs"]
mod tests;
