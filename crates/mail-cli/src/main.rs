//! `mailsync` — headless driver for the sync engine. Mainly for benchmarking strategies:
//!
//!     MAIL_PASSWORD=app-password cargo run --release -p mail-cli -- bench you@gmail.com

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use mail_core::model::{CoreEvent, SyncPhase};
use mail_core::sync::{STRATEGIES, SyncOptions, strategy_by_name};
use mail_core::{Core, NewAccount, Secrets};

mod action_test;
mod probe;

#[derive(Parser)]
#[command(name = "mailsync")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Sync an account from scratch into a fresh directory and report timings.
    Bench {
        email: String,
        #[arg(long)]
        host: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        /// Header strategy to use.
        #[arg(long, default_value = "chunked")]
        strategy: String,
        /// Headers per chunk.
        #[arg(long, default_value_t = 5000)]
        chunk: u32,
        /// Also download message bodies for this many seconds after headers finish.
        #[arg(long, default_value_t = 0)]
        bodies_secs: u64,
        /// Background body connections.
        #[arg(long, default_value_t = 3)]
        body_workers: usize,
        /// Keep the synced data here instead of a temporary directory.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// List available header strategies.
    Strategies,
    /// Compare the incremental expunge walk with a full UID comparison for one folder, against
    /// the real server (read-only: EXAMINE). Use a copy of the data directory.
    ReconcileCheck {
        #[arg(long)]
        data_dir: PathBuf,
        #[arg(long)]
        email: String,
        #[arg(long, default_value = "[Gmail]/All Mail")]
        folder: String,
        #[arg(long, default_value_t = 5000)]
        chunk: u32,
    },
    /// Download one omitted attachment straight from the server (read-only: EXAMINE), the way
    /// the app does, and write it to a file.
    FetchPart {
        #[arg(long)]
        email: String,
        #[arg(long, default_value = "imap.gmail.com")]
        host: String,
        #[arg(long, default_value = "[Gmail]/All Mail")]
        folder: String,
        #[arg(long)]
        uid: u32,
        /// Position among the message's omitted attachments.
        #[arg(long, default_value_t = 0)]
        ordinal: usize,
        #[arg(long)]
        out: PathBuf,
    },
    /// Exercise the message-action commands against a real Gmail account on a throwaway test
    /// message it APPENDs to the Inbox (and deletes for good at the end). Nothing else is touched.
    ActionTest {
        #[arg(long)]
        email: String,
        #[arg(long, default_value = "imap.gmail.com")]
        host: String,
    },
    /// Write a stored message's sanitized HTML (what the reader renders) to a file.
    Render {
        /// App data directory (holds mail.db and bodies.db).
        #[arg(long)]
        data_dir: PathBuf,
        /// Internal message id.
        #[arg(long)]
        id: i64,
        #[arg(long)]
        out: PathBuf,
    },
    /// Diagnose one message: raw IMAP responses for its structure and body-only fetch.
    /// Uses the password the app stored in the system keychain.
    Probe {
        email: String,
        #[arg(long, default_value = "[Gmail]/All Mail")]
        folder: String,
        #[arg(long)]
        uid: u32,
        #[arg(long, default_value = "imap.gmail.com")]
        host: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();

    match Cli::parse().cmd {
        Cmd::Probe { email, folder, uid, host } => probe::run(&email, &folder, uid, &host).await,
        Cmd::ReconcileCheck { data_dir, email, folder, chunk } => {
            use std::collections::HashSet;
            let store = mail_core::store::Store::open(&data_dir)?;
            let account = store.accounts()?.into_iter().find(|a| a.email == email).ok_or_else(|| anyhow!("no account {email}"))?;
            let f = store.folders(account.id)?.into_iter().find(|f| f.raw_name == folder).ok_or_else(|| anyhow!("no folder {folder}"))?;
            let local = store.local_uids(f.id)?;
            let password = keyring::Entry::new("dev.mail-client.imap", &email)?.get_password()?;
            let cfg = mail_core::imap::ServerConfig {
                host: account.imap_host.clone(),
                port: account.imap_port,
                user: email.clone(),
                credentials: mail_core::imap::Credentials::Password(password),
            };
            let mut conn = mail_core::imap::connect(&cfg).await?;
            let mb = conn.session.examine(&folder).await?;
            println!("{folder}: server {} messages, local {}", mb.exists, local.len());

            let t = Instant::now();
            let walked = mail_core::sync::chunked::reconcile_walk(
                &mut conn,
                &local,
                mb.exists,
                chunk,
                mail_core::sync::chunked::MAX_WALK_STEPS,
            )
            .await?;
            let fell_back = walked.is_none();
            let (mut gone, mut missing) = match walked {
                Some(d) => d,
                None => {
                    let server: HashSet<u32> = conn.uid_search_all().await?.into_iter().collect();
                    let mine: HashSet<u32> = local.iter().copied().collect();
                    (mine.difference(&server).copied().collect(), server.difference(&mine).copied().collect())
                }
            };
            let walk_time = t.elapsed();
            if fell_back {
                println!("walk didn't line up within {} steps; used a full comparison", mail_core::sync::chunked::MAX_WALK_STEPS);
            }
            gone.sort();
            missing.sort();

            let t = Instant::now();
            let server: HashSet<u32> = conn.uid_search_all().await?.into_iter().collect();
            let full_time = t.elapsed();
            let mine: HashSet<u32> = local.iter().copied().collect();
            let mut fgone: Vec<u32> = mine.difference(&server).copied().collect();
            let mut fmissing: Vec<u32> = server.difference(&mine).copied().collect();
            fgone.sort();
            fmissing.sort();
            println!("walk: {} gone, {} missing in {:?}", gone.len(), missing.len(), walk_time);
            println!("full: {} gone, {} missing in {:?}", fgone.len(), fmissing.len(), full_time);
            println!("agree: {}", gone == fgone && missing == fmissing);
            conn.logout().await;
            Ok(())
        }
        Cmd::FetchPart { email, host, folder, uid, ordinal, out } => {
            let password = keyring::Entry::new("dev.mail-client.imap", &email)?.get_password()?;
            let cfg = mail_core::imap::ServerConfig {
                host,
                port: 993,
                user: email,
                credentials: mail_core::imap::Credentials::Password(password),
            };
            let mut conn = mail_core::imap::connect(&cfg).await?;
            conn.session.examine(&folder).await?;
            let t = Instant::now();
            let data = conn.fetch_omitted_part(uid, ordinal, None, None).await?;
            println!("fetched {} bytes in {:?}", data.len(), t.elapsed());
            std::fs::write(&out, &data)?;
            conn.logout().await;
            Ok(())
        }
        Cmd::ActionTest { email, host } => action_test::run(&email, &host).await,
        Cmd::Render { data_dir, id, out } => {
            let store = mail_core::store::Store::open(&data_dir)?;
            let raw = store.body(id)?.ok_or_else(|| anyhow!("no body stored for message {id}"))?;
            let detail = mail_core::mime::render(id, &raw, 0).ok_or_else(|| anyhow!("could not parse message"))?;
            std::fs::write(&out, detail.html.unwrap_or_default())?;
            println!("wrote {}", out.display());
            Ok(())
        }
        Cmd::Strategies => {
            for s in STRATEGIES {
                println!("{s}");
            }
            Ok(())
        }
        Cmd::Bench { email, host, port, strategy, chunk, bodies_secs, body_workers, data_dir } => {
            let strat = strategy_by_name(&strategy, chunk).ok_or_else(|| anyhow!("unknown strategy {strategy}"))?;
            let password = match std::env::var("MAIL_PASSWORD") {
                Ok(p) => p,
                Err(_) => rpassword::prompt_password(format!("Password for {email}: "))?,
            };
            let dir = data_dir.unwrap_or_else(|| {
                std::env::temp_dir().join(format!("mailsync-bench-{}", std::process::id()))
            });
            let _ = std::fs::remove_dir_all(&dir);
            let options = SyncOptions { body_workers, download_bodies: bodies_secs > 0, ..Default::default() };
            let core = Core::with_strategy(&dir, Secrets::memory(), strat, options)?;
            let mut events = core.subscribe();

            let t0 = Instant::now();
            let account = core
                .add_account(NewAccount { email, password, display_name: None, imap_host: host, imap_port: port })
                .await?;
            println!("strategy={strategy} chunk={chunk} provider={:?} data={}", account.provider, dir.display());
            println!("{:>8.2}s  connected + authenticated", t0.elapsed().as_secs_f64());

            let mut saw_folders = false;
            let mut saw_inbox = false;
            let mut last_print = Instant::now();
            let headers_done = loop {
                let ev = tokio::time::timeout(Duration::from_secs(600), events.recv()).await??;
                match ev {
                    CoreEvent::FoldersChanged { .. } if !saw_folders => {
                        saw_folders = true;
                        let n = core.folders(account.id)?.len();
                        println!("{:>8.2}s  folder list ({n} folders)", t0.elapsed().as_secs_f64());
                    }
                    CoreEvent::MessagesChanged { .. } if !saw_inbox => {
                        saw_inbox = true;
                        println!("{:>8.2}s  first headers stored", t0.elapsed().as_secs_f64());
                    }
                    CoreEvent::Sync(s) => match s.phase {
                        SyncPhase::Headers if last_print.elapsed() > Duration::from_millis(500) => {
                            last_print = Instant::now();
                            let name = core
                                .folders(account.id)?
                                .into_iter()
                                .find(|f| Some(f.id) == s.folder_id)
                                .map(|f| f.raw_name)
                                .unwrap_or_default();
                            println!("{:>8.2}s  {name}: {}/{}", t0.elapsed().as_secs_f64(), s.done, s.total);
                        }
                        SyncPhase::Idle => break t0.elapsed(),
                        SyncPhase::Error => println!("error: {}", s.message.unwrap_or_default()),
                        _ => {}
                    },
                    _ => {}
                }
            };
            let total_msgs: u32 = core.folders(account.id)?.iter().map(|f| f.synced_count).sum();
            println!(
                "{:>8.2}s  all headers in sync ({total_msgs} folder entries, {:.0} entries/s)",
                headers_done.as_secs_f64(),
                total_msgs as f64 / headers_done.as_secs_f64()
            );

            if bodies_secs > 0 {
                let start = Instant::now();
                let (_, before) = core.store.count_missing_bodies(account.id)?;
                tokio::time::sleep(Duration::from_secs(bodies_secs)).await;
                let (total, after) = core.store.count_missing_bodies(account.id)?;
                let n = after - before;
                println!(
                    "bodies: {n} downloaded in {:.1}s ({:.1}/s), {after}/{total} local",
                    start.elapsed().as_secs_f64(),
                    n as f64 / start.elapsed().as_secs_f64()
                );
            }
            Ok(())
        }
    }
}
