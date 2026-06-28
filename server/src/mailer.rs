//! Notifications (email today; pluggable transport).
//!
//! A [`Notification`] transport sends a plain-text message. Two implementations:
//!   * [`SmtpNotification`] — a real `lettre` `AsyncSmtpTransport` (rustls TLS)
//!     built from an [`SmtpConfig`].
//!   * [`StdoutNotification`] — prints the full message (recipient, subject, body)
//!     to **stdout**. This is the dev/default transport used when no SMTP server
//!     is configured: it never sends anything, so the demo and the test-suite need
//!     no live mail server, and it lets a developer grab reset/invite **links**
//!     straight from the server logs.
//!
//! Sending failures must NEVER fail the originating request: callers should log
//! the error and move on (see the `notify_*` helpers on `AppState`).

use async_trait::async_trait;

use lettre::message::Mailbox;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::models::SmtpConfig;

/// Error type for notification sending.
#[derive(Debug)]
pub enum MailError {
    /// The SMTP config or addresses were invalid.
    Config(String),
    /// The transport failed while sending.
    Transport(String),
}

impl std::fmt::Display for MailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MailError::Config(e) => write!(f, "mail config error: {e}"),
            MailError::Transport(e) => write!(f, "mail transport error: {e}"),
        }
    }
}

impl std::error::Error for MailError {}

/// A notification transport: sends a single plain-text message.
#[async_trait]
pub trait Notification: Send + Sync {
    async fn send(&self, to: &str, subject: &str, body: &str) -> Result<(), MailError>;
}

/// Real SMTP transport built from an [`SmtpConfig`] using lettre + rustls.
pub struct SmtpNotification {
    cfg: SmtpConfig,
}

impl SmtpNotification {
    pub fn new(cfg: SmtpConfig) -> Self {
        Self { cfg }
    }

    fn build_transport(&self) -> Result<AsyncSmtpTransport<Tokio1Executor>, MailError> {
        // STARTTLS for plain hosts, implicit TLS when `tls` is set. Both use
        // rustls (the `tokio1-rustls-tls` feature).
        let builder = if self.cfg.tls {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&self.cfg.host)
                .map_err(|e| MailError::Config(e.to_string()))?
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&self.cfg.host)
                .map_err(|e| MailError::Config(e.to_string()))?
        };
        let builder = builder.port(self.cfg.port);
        let builder = if !self.cfg.username.is_empty() {
            builder.credentials(Credentials::new(
                self.cfg.username.clone(),
                self.cfg.password.clone(),
            ))
        } else {
            builder
        };
        Ok(builder.build())
    }
}

#[async_trait]
impl Notification for SmtpNotification {
    async fn send(&self, to: &str, subject: &str, body: &str) -> Result<(), MailError> {
        let from: Mailbox = self
            .cfg
            .from
            .parse()
            .map_err(|e: lettre::address::AddressError| MailError::Config(e.to_string()))?;
        let to: Mailbox = to
            .parse()
            .map_err(|e: lettre::address::AddressError| MailError::Config(e.to_string()))?;
        let email = Message::builder()
            .from(from)
            .to(to)
            .subject(subject)
            .body(body.to_string())
            .map_err(|e| MailError::Config(e.to_string()))?;
        let transport = self.build_transport()?;
        transport
            .send(email)
            .await
            .map_err(|e| MailError::Transport(e.to_string()))?;
        Ok(())
    }
}

/// Dev/default transport: prints the message (incl. any reset/invite link in the
/// body) to **stdout** instead of sending it. Used when SMTP is unconfigured
/// (the demo + tests). Always succeeds.
pub struct StdoutNotification;

#[async_trait]
impl Notification for StdoutNotification {
    async fn send(&self, to: &str, subject: &str, body: &str) -> Result<(), MailError> {
        println!(
            "\n┌─ 📧 notification (stdout transport — SMTP not configured) ─────────\n\
             │ To:      {to}\n\
             │ Subject: {subject}\n\
             ├───────────────────────────────────────────────────────────────────\n\
             {body}\n\
             └───────────────────────────────────────────────────────────────────\n"
        );
        tracing::info!(target: "photon::notify", to, subject, "stdout notification");
        Ok(())
    }
}
