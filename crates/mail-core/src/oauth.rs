//! Signing in with OAuth 2.0 (Microsoft: outlook.com and Microsoft 365).
//!
//! The authorization-code flow for native apps (RFC 8252): the system browser opens Microsoft's
//! sign-in page; Microsoft redirects to a one-time listener on 127.0.0.1, carrying a code the
//! app exchanges for tokens. PKCE (RFC 7636) stands in for a client secret, which a desktop app
//! can't keep. The refresh token is kept in the keychain (like a password); access tokens last
//! about an hour and are renewed shortly before they expire, so each connection gets a current
//! one (XOAUTH2 for IMAP and SMTP).

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The app's registration in Microsoft Entra (multitenant + personal accounts, public client,
/// redirect `http://localhost`). Not a secret: it only names the app on the consent screen.
pub const MICROSOFT_CLIENT_ID: &str = "63148c30-ac9b-4082-a4c8-9f9f0624564b";
const MICROSOFT_AUTHORIZE: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/authorize";
const MICROSOFT_TOKEN_HOST: &str = "login.microsoftonline.com";
const MICROSOFT_TOKEN_PATH: &str = "/common/oauth2/v2.0/token";
/// Mail over IMAP and SMTP, a refresh token, and the address (from the ID token).
const MICROSOFT_SCOPES: &str =
    "https://outlook.office.com/IMAP.AccessAsUser.All https://outlook.office.com/SMTP.Send offline_access openid email";

/// Microsoft Graph, for what IMAP can't keep on Exchange: tags as Outlook categories (on the
/// messages, and the account's list of them with their colours) and the app's own markers
/// (snooze times) in a property of its own. Consented together with the mail scopes at
/// sign-in; tokens for it come from the same refresh token.
pub const GRAPH_SCOPES: &str =
    "https://graph.microsoft.com/Mail.ReadWrite https://graph.microsoft.com/MailboxSettings.ReadWrite offline_access";

/// How long the browser sign-in may take before the app stops waiting.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Renew an access token this long before it expires.
const RENEW_EARLY: Duration = Duration::from_secs(5 * 60);

/// The server (or the user) refused: the refresh token is no good (revoked, expired, password
/// changed). Only a new sign-in helps.
#[derive(Debug)]
pub struct SignInNeeded(pub String);

impl std::fmt::Display for SignInNeeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Microsoft needs you to sign in again: {}", self.0)
    }
}

impl std::error::Error for SignInNeeded {}

#[derive(Debug, Clone)]
pub struct Tokens {
    pub access_token: String,
    /// Microsoft may send a new one with a renewal; keep it instead of the old.
    pub refresh_token: Option<String>,
    pub expires_in: u64,
    pub id_token: Option<String>,
}

// ------------------------------------------------------------- PKCE

fn random_url_safe(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    aws_lc_rs::rand::fill(&mut buf).expect("system randomness");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// A PKCE verifier and its S256 challenge.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn new() -> Pkce {
        let verifier = random_url_safe(48); // 64 characters
        let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, verifier.as_bytes());
        let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.as_ref());
        Pkce { verifier, challenge }
    }
}

impl Default for Pkce {
    fn default() -> Self {
        Pkce::new()
    }
}

// ------------------------------------------------------------- the browser sign-in

/// Microsoft's sign-in page for this attempt. `login_hint` fills in the address.
/// `graph`: also ask for Microsoft Graph (`GRAPH_SCOPES`), consented together with the mail
/// scopes so the one refresh token serves both.
pub fn microsoft_authorize_url(redirect: &str, state: &str, challenge: &str, login_hint: Option<&str>, graph: bool) -> String {
    let scopes = if graph { format!("{MICROSOFT_SCOPES} {GRAPH_SCOPES}") } else { MICROSOFT_SCOPES.to_string() };
    let mut q = form_urlencoded::Serializer::new(String::new());
    q.append_pair("client_id", MICROSOFT_CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect)
        .append_pair("response_mode", "query")
        .append_pair("scope", &scopes)
        .append_pair("state", state)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("prompt", "select_account");
    if let Some(hint) = login_hint {
        q.append_pair("login_hint", hint);
    }
    format!("{MICROSOFT_AUTHORIZE}?{}", q.finish())
}

/// What the browser comes back with.
pub struct Redirect {
    listener: TcpListener,
    pub uri: String,
    state: String,
}

impl Redirect {
    /// A one-time listener on a free loopback port (Microsoft accepts any port for
    /// `http://localhost`).
    pub async fn listen() -> Result<Redirect> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.context("opening a local port for the sign-in")?;
        let port = listener.local_addr()?.port();
        Ok(Redirect { listener, uri: format!("http://localhost:{port}"), state: random_url_safe(16) })
    }

    pub fn state(&self) -> &str {
        &self.state
    }

    /// Wait for the browser's redirect and return the authorization code. Requests that aren't
    /// the redirect (a favicon) are answered and ignored.
    pub async fn code(self) -> Result<String> {
        tokio::time::timeout(SIGN_IN_TIMEOUT, async {
            loop {
                let (mut stream, _) = self.listener.accept().await?;
                let Some(target) = read_request_target(&mut stream).await else { continue };
                let Some(query) = target.split_once('?').map(|(_, q)| q.to_string()) else {
                    respond(&mut stream, 404, "").await;
                    continue;
                };
                let params: std::collections::HashMap<String, String> =
                    form_urlencoded::parse(query.as_bytes()).into_owned().collect();
                if params.get("state").map(String::as_str) != Some(self.state.as_str()) {
                    respond(&mut stream, 400, &page("This sign-in link isn't the one Email is waiting for. Try again from Email.")).await;
                    continue;
                }
                if let Some(code) = params.get("code") {
                    respond(&mut stream, 200, &page("You're signed in. You can close this tab and go back to Email.")).await;
                    return Ok(code.clone());
                }
                let why = params.get("error_description").or_else(|| params.get("error")).cloned().unwrap_or_default();
                respond(&mut stream, 200, &page("The sign-in didn't complete. You can close this tab; Email shows what happened.")).await;
                return Err(SignInNeeded(why).into());
            }
        })
        .await
        .map_err(|_| anyhow!("Gave up waiting for the sign-in in the browser."))?
    }
}

/// The request line's target ("/?code=…") of an HTTP request.
async fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 64 * 1024 {
        let n = tokio::time::timeout(Duration::from_secs(10), stream.read(&mut chunk)).await.ok()?.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let line = String::from_utf8_lossy(&buf);
    let mut parts = line.lines().next()?.split(' ');
    (parts.next()? == "GET").then(|| parts.next().map(str::to_string))?
}

async fn respond(stream: &mut TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn page(text: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><title>Email</title>\
         <body style=\"font:16px system-ui,sans-serif;margin:15vh auto;max-width:32em;text-align:center\">\
         <p>{text}</p></body>"
    )
}

// ------------------------------------------------------------- tokens

/// Trade the browser's code for tokens.
pub async fn microsoft_exchange(code: &str, verifier: &str, redirect: &str) -> Result<Tokens> {
    token_request(&[
        ("client_id", MICROSOFT_CLIENT_ID),
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect),
        ("code_verifier", verifier),
        ("scope", MICROSOFT_SCOPES),
    ])
    .await
}

/// A new access token from a refresh token.
pub async fn microsoft_refresh(refresh_token: &str) -> Result<Tokens> {
    microsoft_refresh_for(refresh_token, MICROSOFT_SCOPES).await
}

/// A new access token for `scopes` (one resource: mail, or Graph) from a refresh token.
pub async fn microsoft_refresh_for(refresh_token: &str, scopes: &str) -> Result<Tokens> {
    token_request(&[
        ("client_id", MICROSOFT_CLIENT_ID),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("scope", scopes),
    ])
    .await
}

async fn token_request(form: &[(&str, &str)]) -> Result<Tokens> {
    let body = form_urlencoded::Serializer::new(String::new()).extend_pairs(form).finish();
    let (status, text) = post_form(MICROSOFT_TOKEN_HOST, MICROSOFT_TOKEN_PATH, &body).await?;
    let v: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("unexpected answer from Microsoft (HTTP {status})"))?;
    if status != 200 {
        let code = v["error"].as_str().unwrap_or("error");
        let why = v["error_description"].as_str().unwrap_or("").lines().next().unwrap_or("").to_string();
        // invalid_grant: the refresh token or code is no good; interaction_required: consent.
        if matches!(code, "invalid_grant" | "interaction_required" | "consent_required" | "unauthorized_client") {
            return Err(SignInNeeded(format!("{code}: {why}")).into());
        }
        bail!("Microsoft refused the sign-in ({code}): {why}");
    }
    Ok(Tokens {
        access_token: v["access_token"].as_str().ok_or_else(|| anyhow!("no access token in Microsoft's answer"))?.to_string(),
        refresh_token: v["refresh_token"].as_str().map(str::to_string),
        expires_in: v["expires_in"].as_u64().unwrap_or(3600),
        id_token: v["id_token"].as_str().map(str::to_string),
    })
}

/// POST a form over HTTPS (one request, then the connection closes): (status, body).
async fn post_form(host: &str, path: &str, body: &str) -> Result<(u16, String)> {
    https(host, "POST", path, &[("Content-Type", "application/x-www-form-urlencoded")], body).await
}

/// GET a Graph path (`/v1.0/...`) with `auth` ("Bearer …").
pub async fn graph_get(auth: &str, path: &str) -> Result<(u16, String)> {
    https("graph.microsoft.com", "GET", path, &[("Authorization", auth)], "").await
}

/// One HTTPS request (HTTP/1.1, a new connection each time); its status and body.
pub(crate) async fn https(host: &str, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Result<(u16, String)> {
    let tcp = tokio::time::timeout(Duration::from_secs(20), TcpStream::connect((host, 443)))
        .await
        .map_err(|_| anyhow!("connecting to {host} timed out"))?
        .with_context(|| format!("connecting to {host}"))?;
    let name = tokio_rustls::rustls::pki_types::ServerName::try_from(host.to_string())?;
    let mut tls = tokio_rustls::TlsConnector::from(crate::imap::tls_config()).connect(name, tcp).await?;
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\n");
    for (k, v) in headers {
        request.push_str(&format!("{k}: {v}\r\n"));
    }
    request.push_str(&format!(
        "Content-Length: {}\r\nAccept: application/json\r\nConnection: close\r\nUser-Agent: Email\r\n\r\n{body}",
        body.len()
    ));
    tls.write_all(request.as_bytes()).await?;
    let mut raw = Vec::new();
    // A server may close without TLS close_notify once it has sent everything.
    let _ = tokio::time::timeout(Duration::from_secs(60), tls.read_to_end(&mut raw)).await.map_err(|_| anyhow!("{host} didn't answer"))?;
    parse_response(&raw)
}

/// Status and body of an HTTP/1.1 response (plain or chunked).
fn parse_response(raw: &[u8]) -> Result<(u16, String)> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| anyhow!("incomplete answer"))?;
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let mut body = raw[split + 4..].to_vec();
    let status: u16 = head.split(' ').nth(1).and_then(|s| s.parse().ok()).ok_or_else(|| anyhow!("no HTTP status"))?;
    let chunked = head.lines().any(|l| {
        let l = l.to_ascii_lowercase();
        l.starts_with("transfer-encoding:") && l.contains("chunked")
    });
    if chunked {
        let mut out = Vec::new();
        let mut rest = &body[..];
        loop {
            let Some(eol) = rest.windows(2).position(|w| w == b"\r\n") else { break };
            let size = usize::from_str_radix(String::from_utf8_lossy(&rest[..eol]).split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| anyhow!("bad chunk"))?;
            rest = &rest[eol + 2..];
            if size == 0 || rest.len() < size {
                break;
            }
            out.extend_from_slice(&rest[..size]);
            rest = rest.get(size + 2..).unwrap_or(&[]);
        }
        body = out;
    }
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

/// The signed-in address from an ID token (its payload, not checked: it came straight from
/// Microsoft over TLS). `email`, else `preferred_username` (the sign-in name, an address for
/// mail accounts).
pub fn email_from_id_token(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v["email"]
        .as_str()
        .or_else(|| v["preferred_username"].as_str())
        .filter(|s| s.contains('@'))
        .map(|s| s.trim().to_ascii_lowercase())
}

// ------------------------------------------------------------- a signed-in account

/// Where an account's refresh token is kept, and how a rotated one is saved.
pub trait TokenStore: Send + Sync {
    fn refresh_token(&self, email: &str) -> Result<String>;
    fn save_refresh_token(&self, email: &str, token: &str) -> Result<()>;
}

/// Current access tokens for one Microsoft account, renewed when they're about to expire.
pub struct MicrosoftSession {
    email: String,
    /// What its tokens are for: mail (IMAP, SMTP) or Graph.
    scopes: &'static str,
    store: Arc<dyn TokenStore>,
    /// The access token and when it expires, by the wall clock: the monotonic clock stops
    /// while the computer sleeps, so after a night asleep a token would look fresh when it
    /// expired hours ago.
    current: tokio::sync::Mutex<Option<(String, SystemTime)>>,
    /// The last renewal failed for good: don't hammer Microsoft until a new sign-in.
    refused: Mutex<Option<String>>,
}

impl MicrosoftSession {
    pub fn new(email: &str, store: Arc<dyn TokenStore>) -> MicrosoftSession {
        Self::for_scopes(email, store, MICROSOFT_SCOPES)
    }

    /// Tokens for Microsoft Graph (`GRAPH_SCOPES`), from the account's same refresh token.
    pub fn graph(email: &str, store: Arc<dyn TokenStore>) -> MicrosoftSession {
        Self::for_scopes(email, store, GRAPH_SCOPES)
    }

    fn for_scopes(email: &str, store: Arc<dyn TokenStore>, scopes: &'static str) -> MicrosoftSession {
        MicrosoftSession { email: email.to_string(), scopes, store, current: tokio::sync::Mutex::new(None), refused: Mutex::new(None) }
    }

    /// A token good for at least a few more minutes.
    pub async fn access_token(&self) -> Result<String> {
        if let Some(why) = self.refused.lock().unwrap().clone() {
            return Err(SignInNeeded(why).into());
        }
        let mut current = self.current.lock().await;
        if let Some((token, until)) = current.as_ref() {
            if SystemTime::now() + RENEW_EARLY < *until {
                return Ok(token.clone());
            }
        }
        let refresh = self.store.refresh_token(&self.email)?;
        let tokens = match microsoft_refresh_for(&refresh, self.scopes).await {
            Ok(t) => t,
            Err(e) => {
                if let Some(s) = e.downcast_ref::<SignInNeeded>() {
                    *self.refused.lock().unwrap() = Some(s.0.clone());
                }
                return Err(e);
            }
        };
        if let Some(new) = tokens.refresh_token.as_deref().filter(|t| *t != refresh) {
            if let Err(e) = self.store.save_refresh_token(&self.email, new) {
                tracing::warn!("saving the renewed sign-in: {e:#}");
            }
        }
        let until = SystemTime::now() + Duration::from_secs(tokens.expires_in);
        *current = Some((tokens.access_token.clone(), until));
        Ok(tokens.access_token)
    }

    /// The server turned `token` down: get a new one next time (unless another connection
    /// already has).
    pub async fn forget(&self, token: &str) {
        let mut current = self.current.lock().await;
        if current.as_ref().is_some_and(|(t, _)| t == token) {
            *current = None;
        }
    }
}

/// Which SMTP server a Microsoft address sends through: personal accounts and Microsoft 365
/// use different ones.
pub fn microsoft_smtp(email: &str) -> (&'static str, u16) {
    let domain = email.rsplit('@').next().unwrap_or("").to_ascii_lowercase();
    if matches!(domain.as_str(), "outlook.com" | "hotmail.com" | "live.com" | "msn.com")
        || domain.starts_with("outlook.")
        || domain.starts_with("hotmail.")
        || domain.starts_with("live.")
    {
        ("smtp-mail.outlook.com", 587)
    } else {
        ("smtp.office365.com", 587)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_the_verifiers_sha256() {
        // RFC 7636 appendix B
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, verifier.as_bytes());
        assert_eq!(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest.as_ref()), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        let p = Pkce::new();
        assert_eq!(p.verifier.len(), 64);
        assert_ne!(p.verifier, Pkce::new().verifier);
    }

    #[test]
    fn authorize_url_carries_the_request() {
        let url = microsoft_authorize_url("http://localhost:4321", "st", "ch", Some("john@example.com"), false);
        assert!(url.starts_with(MICROSOFT_AUTHORIZE));
        for part in ["client_id=63148c30", "redirect_uri=http%3A%2F%2Flocalhost%3A4321", "code_challenge=ch", "code_challenge_method=S256", "state=st", "login_hint=john%40example.com", "offline_access"] {
            assert!(url.contains(part), "{part} in {url}");
        }
    }

    #[test]
    fn responses_plain_and_chunked() {
        let plain = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 2\r\n\r\n{}";
        assert_eq!(parse_response(plain).unwrap(), (400, "{}".into()));
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n3\r\n 1}\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked).unwrap(), (200, "{\"a\": 1}".into()));
    }

    #[test]
    fn address_from_the_id_token() {
        let b64 = |s: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
        let t = format!("{}.{}.sig", b64("{}"), b64(r#"{"preferred_username":"John.Doe@Example.com"}"#));
        assert_eq!(email_from_id_token(&t).as_deref(), Some("john.doe@example.com"));
        let t = format!("{}.{}.sig", b64("{}"), b64(r#"{"email":"j@example.org","preferred_username":"x"}"#));
        assert_eq!(email_from_id_token(&t).as_deref(), Some("j@example.org"));
    }

    #[test]
    fn smtp_by_kind_of_account() {
        assert_eq!(microsoft_smtp("john@outlook.com").0, "smtp-mail.outlook.com");
        assert_eq!(microsoft_smtp("john@hotmail.co.uk").0, "smtp-mail.outlook.com");
        assert_eq!(microsoft_smtp("john@example.com").0, "smtp.office365.com");
    }

    /// The browser's redirect reaches the listener, which hands back the code.
    #[tokio::test]
    async fn the_redirect_brings_the_code() {
        let r = Redirect::listen().await.unwrap();
        let port: u16 = r.uri.rsplit(':').next().unwrap().parse().unwrap();
        let state = r.state().to_string();
        let waiting = tokio::spawn(r.code());
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(format!("GET /?code=abc123&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes()).await.unwrap();
        let mut reply = String::new();
        s.read_to_string(&mut reply).await.unwrap();
        assert!(reply.starts_with("HTTP/1.1 200") && reply.contains("signed in"));
        assert_eq!(waiting.await.unwrap().unwrap(), "abc123");
    }

    /// Microsoft's token endpoint answers a bad code with an error the app recognises (needs
    /// the network: `cargo test -p mail-core -- --ignored microsoft_rejects`).
    #[tokio::test]
    #[ignore]
    async fn microsoft_rejects_a_made_up_code() {
        let e = microsoft_exchange("not-a-code", &Pkce::new().verifier, "http://localhost:1").await.unwrap_err();
        println!("{e:#}");
        assert!(e.is::<SignInNeeded>() || format!("{e:#}").contains("Microsoft refused"), "{e:#}");
    }
}
