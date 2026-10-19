//! Raw IMAP probe for one message, bypassing the IMAP library's parser so we can see exactly
//! what the server sends. Prints response sizes, timing and the BODYSTRUCTURE, never bodies.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, pki_types::ServerName};

type Stream = BufReader<tokio_rustls::client::TlsStream<TcpStream>>;

/// Read one complete response for `tag`, including literals. Returns (text with literals
/// summarized, total bytes).
async fn read_tagged(s: &mut Stream, tag: &str, show_literals: bool) -> Result<(String, usize)> {
    let mut out = String::new();
    let mut total = 0usize;
    loop {
        let mut line = Vec::new();
        let n = tokio::time::timeout(Duration::from_secs(30), s.read_until(b'\n', &mut line))
            .await
            .map_err(|_| anyhow!("no data for 30s (after {total} bytes)"))??;
        if n == 0 {
            return Err(anyhow!("connection closed after {total} bytes"));
        }
        total += n;
        let text = String::from_utf8_lossy(&line).to_string();
        // Literal: line ends with {N}\r\n, followed by N raw bytes.
        if let Some(len) = text.trim_end().strip_suffix('}').and_then(|t| t.rsplit_once('{')).and_then(|(_, n)| n.trim_start_matches('~').parse::<usize>().ok().or_else(|| n.trim_end_matches('+').parse().ok())) {
            out.push_str(&text);
            let mut buf = vec![0u8; len];
            tokio::time::timeout(Duration::from_secs(60), s.read_exact(&mut buf))
                .await
                .map_err(|_| anyhow!("literal of {len} bytes stalled"))??;
            total += len;
            if show_literals {
                out.push_str(&String::from_utf8_lossy(&buf));
            } else {
                out.push_str(&format!("<{len} bytes>"));
            }
            continue;
        }
        out.push_str(&text);
        if text.starts_with(&format!("{tag} ")) {
            return Ok((out, total));
        }
    }
}

async fn cmd(s: &mut Stream, tag: &str, command: &str, show_literals: bool) -> Result<String> {
    let t = Instant::now();
    s.get_mut().write_all(format!("{tag} {command}\r\n").as_bytes()).await?;
    let (resp, bytes) = read_tagged(s, tag, show_literals).await?;
    // Print only the command verb(s): arguments can contain credentials (LOGIN).
    let verb = if command.starts_with("LOGIN") { "LOGIN" } else { command.split_whitespace().take(3).collect::<Vec<_>>().join(" ").leak() };
    println!("--- {tag} {verb} ({bytes} bytes, {:?})", t.elapsed());
    Ok(resp)
}

pub async fn run(email: &str, folder: &str, uid: u32, host: &str) -> Result<()> {
    let password = keyring::Entry::new("dev.mail-client.imap", email)?.get_password()?;
    let mut roots = RootCertStore::empty();
    for c in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(c);
    }
    let cfg = Arc::new(ClientConfig::builder().with_root_certificates(roots).with_no_client_auth());
    let tcp = TcpStream::connect((host, 993)).await?;
    let tls = TlsConnector::from(cfg).connect(ServerName::try_from(host.to_string())?, tcp).await?;
    let mut s = BufReader::new(tls);
    let mut greeting = String::new();
    s.read_line(&mut greeting).await?;

    let login = format!("LOGIN \"{}\" \"{}\"", email, password.replace('\\', "\\\\").replace('"', "\\\""));
    let r = cmd(&mut s, "a1", &login, false).await?;
    if !r.lines().last().unwrap_or("").contains("OK") {
        return Err(anyhow!("login failed"));
    }
    cmd(&mut s, "a2", &format!("EXAMINE \"{folder}\""), false).await?;

    let r = cmd(&mut s, "a3", &format!("UID FETCH {uid} (UID RFC822.SIZE BODYSTRUCTURE)"), true).await?;
    println!("{r}");

    // What our body-only download asks for, part by part, with timing.
    let r = cmd(&mut s, "a4", &format!("UID FETCH {uid} (UID BODY.PEEK[HEADER])"), false).await?;
    println!("{}", r.lines().next().unwrap_or(""));
    let r = cmd(&mut s, "a5", &format!("UID FETCH {uid} (UID BODY.PEEK[])"), false).await?;
    println!("{}", r.lines().next().unwrap_or(""));
    cmd(&mut s, "a6", "LOGOUT", false).await.ok();
    Ok(())
}
