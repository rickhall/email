//! Sending over SMTP submission: implicit TLS on port 465, STARTTLS on other ports. Each
//! account has its own server (`Account::smtp`); it signs in with the account's credentials.

use std::time::Duration;

use lettre::address::Envelope;
use lettre::transport::smtp::authentication::{Credentials as SmtpCredentials, Mechanism};
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

use crate::imap::Credentials;

#[derive(Debug, Clone)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub credentials: Credentials,
}

/// The submission server that goes with a well-known IMAP server, for accounts without an SMTP
/// server of their own.
pub fn default_for(imap_host: &str) -> Option<(&'static str, u16)> {
    Some(match imap_host.trim().to_ascii_lowercase().as_str() {
        "imap.gmail.com" => ("smtp.gmail.com", 465),
        "outlook.office365.com" | "imap-mail.outlook.com" => ("smtp.office365.com", 587),
        "imap.mail.me.com" => ("smtp.mail.me.com", 587),
        "imap.fastmail.com" => ("smtp.fastmail.com", 465),
        "imap.mail.yahoo.com" => ("smtp.mail.yahoo.com", 465),
        _ => return None,
    })
}

#[derive(Debug)]
pub enum SendError {
    /// The server refused it for good (bad address, rejected content, credentials): retrying
    /// the same message won't help.
    Rejected(String),
    /// Couldn't get it through this time (network, server busy): worth retrying.
    Failed(anyhow::Error),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::Rejected(m) => write!(f, "{m}"),
            SendError::Failed(e) => write!(f, "{e:#}"),
        }
    }
}

const TIMEOUT: Duration = Duration::from_secs(60);

/// Send an already-built message to `recipients` (the envelope: To, Cc and Bcc).
pub async fn send(cfg: &SmtpConfig, from: &str, recipients: &[String], raw: &[u8]) -> Result<(), SendError> {
    Mailer::new(cfg)?.send(from, recipients, raw).await
}

/// One account's SMTP sender. Keeps a signed-in connection for a while after use (pooled), and
/// can open one ahead of time (`warm`) so a message leaves the moment its undo window ends.
/// Cheap to clone (the pool is shared).
#[derive(Clone)]
pub struct Mailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
}

impl Mailer {
    pub fn new(cfg: &SmtpConfig) -> Result<Mailer, SendError> {
        crate::imap::ensure_crypto_provider();
        Ok(Mailer { transport: transport(cfg)? })
    }

    /// Connect and sign in now (the connection waits in the pool); errors show up at `send`.
    pub async fn warm(&self) {
        if let Err(e) = self.transport.test_connection().await {
            tracing::debug!("SMTP warm-up: {e}");
        }
    }

    pub async fn send(&self, from: &str, recipients: &[String], raw: &[u8]) -> Result<(), SendError> {
        let sender = from.trim().parse().map_err(|e| SendError::Rejected(format!("sender address {from}: {e}")))?;
        let mut to = Vec::with_capacity(recipients.len());
        for r in recipients {
            to.push(r.trim().parse().map_err(|e| SendError::Rejected(format!("recipient address {r}: {e}")))?);
        }
        let envelope = Envelope::new(Some(sender), to).map_err(|e| SendError::Rejected(e.to_string()))?;
        match self.transport.send_raw(&envelope, raw).await {
            Ok(_) => Ok(()),
            Err(e) if e.is_permanent() => Err(SendError::Rejected(e.to_string())),
            Err(e) => Err(SendError::Failed(e.into())),
        }
    }
}

fn transport(cfg: &SmtpConfig) -> Result<AsyncSmtpTransport<Tokio1Executor>, SendError> {
    let builder = if cfg.port == 465 {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.host)
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.host)
    }
    .map_err(|e| SendError::Failed(e.into()))?;
    let (credentials, mechanisms) = match &cfg.credentials {
        Credentials::Password(pw) => (SmtpCredentials::new(cfg.user.clone(), pw.clone()), vec![Mechanism::Plain, Mechanism::Login]),
        Credentials::OAuth2 { access_token } => {
            (SmtpCredentials::new(cfg.user.clone(), access_token.clone()), vec![Mechanism::Xoauth2])
        }
    };
    Ok(builder.port(cfg.port).credentials(credentials).authentication(mechanisms).timeout(Some(TIMEOUT)).build())
}
