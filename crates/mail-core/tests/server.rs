//! The sync engine against real servers: Dovecot (IMAP) and Mailpit (SMTP), started by
//! `test-server/test-server.sh up`. Without its environment (MAIL_TEST_IMAP…) these tests skip.
//!
//!   eval "$(test-server/test-server.sh up)" && cargo test -p mail-core --test server
//!
//! Every test signs in as its own new user (Dovecot takes any name with the test password), so
//! each starts from an empty mailbox. A second connection plays "another mail client": it puts
//! mail in, changes it, and checks what reached the server.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::TryStreamExt;
use mail_core::actions::Action;
use mail_core::imap::{self, Connection, Credentials, ServerConfig};
use mail_core::model::*;
use mail_core::sync::SyncOptions;
use mail_core::{Core, NewAccount, Secrets};

const PASSWORD: &str = "secret";

struct Servers {
    imap: (String, u16),
    smtp: (String, u16),
    mailpit: String,
}

fn host_port(v: &str) -> (String, u16) {
    let (h, p) = v.rsplit_once(':').expect("host:port");
    (h.to_string(), p.parse().expect("port"))
}

/// The test servers, or None (the test is skipped) when they aren't set up. The core's log goes
/// to the test output with RUST_LOG (e.g. RUST_LOG=mail_core=debug … -- --nocapture).
fn servers() -> Option<Servers> {
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).with_test_writer().try_init();
    let Ok(imap) = std::env::var("MAIL_TEST_IMAP") else {
        eprintln!("skipped: no test servers (eval \"$(test-server/test-server.sh up)\")");
        return None;
    };
    Some(Servers {
        imap: host_port(&imap),
        smtp: host_port(&std::env::var("MAIL_TEST_SMTP").expect("MAIL_TEST_SMTP")),
        mailpit: std::env::var("MAIL_TEST_MAILPIT").expect("MAIL_TEST_MAILPIT"),
    })
}

/// A user nobody has used yet: an empty mailbox.
fn new_user(name: &str) -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    format!("{name}-{}-{nanos}@example.test", std::process::id())
}

/// "Another mail client": a plain connection to the user's mailbox.
async fn other_client(s: &Servers, user: &str) -> Connection {
    imap::connect(&ServerConfig {
        host: s.imap.0.clone(),
        port: s.imap.1,
        user: user.into(),
        credentials: Credentials::Password(PASSWORD.into()),
    })
    .await
    .expect("connecting to the test IMAP server")
}

fn raw_message(from: &str, to: &str, subject: &str, n: u32) -> String {
    // Older numbers are older mail: a day apart, so the list order is known.
    let date = chrono_like_date(n);
    format!(
        "From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nDate: {date}\r\nMessage-ID: <{n}.{}@example.test>\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\nBody of message {n}.\r\n",
        subject.replace(' ', "-")
    )
}

/// An RFC 5322 date `n` days after 1 Jan 2026 (no date crate needed for this).
fn chrono_like_date(n: u32) -> String {
    let days = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (mut d, mut m) = (n % 365, 0);
    while d >= days[m] {
        d -= days[m];
        m += 1;
    }
    format!("{} {} 2026 10:00:00 +0000", d + 1, months[m])
}

/// Store a message as if it had arrived on its own date (INTERNALDATE, which orders the list).
async fn append(c: &mut Connection, folder: &str, raw: &str, flags: Option<&str>) {
    let date = raw.lines().find_map(|l| l.strip_prefix("Date: ")).expect("Date header");
    let mut p = date.split(' ');
    let (d, m, y, t, z) = (p.next().unwrap(), p.next().unwrap(), p.next().unwrap(), p.next().unwrap(), p.next().unwrap());
    // quoted: async-imap passes it through as is
    let internal = format!("\"{d:0>2}-{m}-{y} {t} {z}\"");
    c.session.append(folder, flags, Some(&internal), raw.as_bytes()).await.expect("APPEND");
}

/// UIDs in `folder` whose subject is exactly `subject` (as the server has them). Fetched, not
/// SEARCHed: Dovecot's full-text index doesn't match every word (a lone "3").
async fn on_server(c: &mut Connection, folder: &str, subject: &str) -> Vec<u32> {
    let mb = c.session.select(folder).await.expect("SELECT");
    if mb.exists == 0 {
        return vec![];
    }
    let fetched: Vec<_> =
        c.session.uid_fetch("1:*", "BODY.PEEK[HEADER.FIELDS (SUBJECT)]").await.expect("FETCH").try_collect().await.expect("FETCH");
    let mut v: Vec<u32> = fetched
        .iter()
        .filter(|f| {
            let h = String::from_utf8_lossy(f.header().unwrap_or_default());
            h.trim().strip_prefix("Subject: ").is_some_and(|s| s.trim() == subject)
        })
        .filter_map(|f| f.uid)
        .collect();
    v.sort_unstable();
    v
}

async fn flags_on_server(c: &mut Connection, folder: &str, uid: u32) -> Vec<String> {
    c.session.select(folder).await.expect("SELECT");
    let fetched: Vec<_> = c.session.uid_fetch(uid.to_string(), "FLAGS").await.expect("FETCH").try_collect().await.expect("FETCH");
    fetched.first().map(|f| f.flags().map(|x| format!("{x:?}")).collect()).unwrap_or_default()
}

/// The app: a Core on a fresh data folder, with the account added and syncing.
struct App {
    core: Arc<Core>,
    account: Account,
    dir: PathBuf,
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn app(s: &Servers, user: &str) -> App {
    let dir = std::env::temp_dir().join(format!("mail-server-test-{}", user.split('@').next().unwrap()));
    let _ = std::fs::remove_dir_all(&dir);
    // Small header chunks, so a modest mailbox already takes several; one body worker; the
    // periodic poll far off, so what arrives must come by IDLE.
    let core = Core::with_strategy(
        &dir,
        Secrets::memory(),
        Arc::new(mail_core::sync::chunked::ChunkedNewestFirst::new(40)),
        SyncOptions { body_workers: 1, download_bodies: true, refresh_interval: Duration::from_secs(600) },
    )
    .expect("opening the store");
    let account = core
        .add_account(NewAccount {
            email: user.into(),
            password: PASSWORD.into(),
            display_name: None,
            imap_host: Some(s.imap.0.clone()),
            imap_port: Some(s.imap.1),
        })
        .await
        .expect("adding the account");
    App { core: Arc::new(core), account, dir }
}

impl App {
    fn folder(&self, role: FolderRole) -> Folder {
        self.core
            .folders(self.account.id)
            .unwrap()
            .into_iter()
            .find(|f| f.role == Some(role))
            .unwrap_or_else(|| panic!("no {role:?} folder"))
    }
    fn messages(&self, role: FolderRole) -> Vec<MessageSummary> {
        let f = self.folder(role);
        self.core.list_messages(f.id, 0, 1000).unwrap().messages
    }
    fn subjects(&self, role: FolderRole) -> Vec<String> {
        self.messages(role).into_iter().map(|m| m.subject).collect()
    }
}

/// Wait up to `secs` for `ok`, polling; panics with `what` if it never happens.
async fn until(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !ok() {
        assert!(Instant::now() < end, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Wait until the folders are listed (the first thing a new account's sync does).
async fn folders_listed(a: &App) {
    until("the folder list", 20, || a.core.folders(a.account.id).map(|f| f.iter().any(|f| f.role == Some(FolderRole::Inbox))).unwrap_or(false)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn first_sync_takes_everything_in_chunks() {
    let Some(s) = servers() else { return };
    let user = new_user("first");
    let mut other = other_client(&s, &user).await;
    for n in 1..=130 {
        append(&mut other, "INBOX", &raw_message("Jane Doe <jane@example.test>", &user, &format!("Hello {n}"), n), None).await;
    }
    for n in 1..=3 {
        append(&mut other, "Sent", &raw_message(&user, "jane@example.test", &format!("Reply {n}"), n), Some("(\\Seen)")).await;
    }

    let a = app(&s, &user).await;
    folders_listed(&a).await;
    for role in [FolderRole::Inbox, FolderRole::Sent, FolderRole::Drafts, FolderRole::Trash, FolderRole::Junk] {
        a.folder(role); // each special-use folder got its role
    }
    until("130 messages in the Inbox", 60, || a.messages(FolderRole::Inbox).len() == 130).await;
    until("3 in Sent", 30, || a.messages(FolderRole::Sent).len() == 3).await;
    // Newest first, all unread.
    let inbox = a.messages(FolderRole::Inbox);
    assert_eq!(inbox[0].subject, "Hello 130");
    assert_eq!(inbox[129].subject, "Hello 1");
    assert!(inbox.iter().all(|m| m.unread));
    // Every body comes down.
    until("all bodies downloaded", 60, || {
        let (total, have) = a.core.store.count_missing_bodies(a.account.id).unwrap();
        total == 133 && have == 133
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn changes_made_elsewhere_arrive() {
    let Some(s) = servers() else { return };
    let user = new_user("elsewhere");
    let mut other = other_client(&s, &user).await;
    for n in 1..=5 {
        append(&mut other, "INBOX", &raw_message("Jane Doe <jane@example.test>", &user, &format!("Note {n}"), n), None).await;
    }
    let a = app(&s, &user).await;
    folders_listed(&a).await;
    until("the first 5", 30, || a.messages(FolderRole::Inbox).len() == 5).await;

    // New mail: pushed by IDLE (the periodic poll is minutes away).
    append(&mut other, "INBOX", &raw_message("Jane Doe <jane@example.test>", &user, "Note 6", 6), None).await;
    until("new mail by IDLE", 20, || a.subjects(FolderRole::Inbox).first().map(String::as_str) == Some("Note 6")).await;

    // Read elsewhere.
    let uid = on_server(&mut other, "INBOX", "Note 3").await[0];
    other.session.uid_store(uid.to_string(), "+FLAGS (\\Seen)").await.unwrap().try_collect::<Vec<_>>().await.unwrap();
    until("Note 3 read", 20, || a.messages(FolderRole::Inbox).iter().any(|m| m.subject == "Note 3" && !m.unread)).await;

    // Deleted elsewhere.
    let uid = on_server(&mut other, "INBOX", "Note 2").await[0];
    other.session.uid_store(uid.to_string(), "+FLAGS (\\Deleted)").await.unwrap().try_collect::<Vec<_>>().await.unwrap();
    other.session.uid_expunge(uid.to_string()).await.unwrap().try_collect::<Vec<_>>().await.unwrap();
    until("Note 2 gone", 20, || !a.subjects(FolderRole::Inbox).iter().any(|s| s == "Note 2")).await;
    assert_eq!(a.messages(FolderRole::Inbox).len(), 5);
    // The badge shows what the list shows: 4 unread (Note 3 read, Note 2 gone).
    until("the badge to match the list", 20, || {
        let shown = a.messages(FolderRole::Inbox).iter().filter(|m| m.unread).count() as u32;
        let inbox = a.folder(FolderRole::Inbox);
        shown == 4 && inbox.unread == shown && inbox.total == 5
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn actions_reach_the_server_and_undo_stops_them() {
    let Some(s) = servers() else { return };
    let user = new_user("actions");
    let mut other = other_client(&s, &user).await;
    for subject in ["Delete me", "Star me", "Keep me"] {
        append(&mut other, "INBOX", &raw_message("Jane Doe <jane@example.test>", &user, subject, 1), None).await;
    }
    let a = app(&s, &user).await;
    folders_listed(&a).await;
    until("3 messages", 30, || a.messages(FolderRole::Inbox).len() == 3).await;
    let inbox = a.folder(FolderRole::Inbox);
    let id = |subject: &str| a.messages(FolderRole::Inbox).into_iter().find(|m| m.subject == subject).unwrap().id;

    // Delete: gone from the Inbox at once here, in Trash on the server after the undo window.
    a.core.apply_action(Action::Trash, &[id("Delete me")], inbox.id).unwrap();
    assert!(!a.subjects(FolderRole::Inbox).iter().any(|s| s == "Delete me"));
    // Star: a flag change, sent right away.
    a.core.apply_action(Action::Flag, &[id("Star me")], inbox.id).unwrap();
    // Delete, then undo within the window: nothing may reach the server.
    let undone = a.core.apply_action(Action::Trash, &[id("Keep me")], inbox.id).unwrap();
    a.core.undo_action(undone.op_id.expect("an undoable op")).unwrap();
    assert!(a.subjects(FolderRole::Inbox).iter().any(|s| s == "Keep me"));

    let end = Instant::now() + Duration::from_secs(20);
    loop {
        let moved = on_server(&mut other, "INBOX", "Delete me").await.is_empty() && on_server(&mut other, "Trash", "Delete me").await.len() == 1;
        if moved {
            break;
        }
        assert!(Instant::now() < end, "Delete me never reached Trash on the server");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let star = on_server(&mut other, "INBOX", "Star me").await[0];
    assert!(flags_on_server(&mut other, "INBOX", star).await.iter().any(|f| f.contains("Flagged")), "\\Flagged on the server");
    assert_eq!(on_server(&mut other, "INBOX", "Keep me").await.len(), 1, "the undone delete stayed undone");
    // And the app agrees with the server.
    until("Delete me in Trash here", 20, || a.subjects(FolderRole::Trash).iter().any(|s| s == "Delete me")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn acting_on_a_message_just_moved_reaches_the_server() {
    // Out of Trash into the Inbox, then (before the move has even been sent) deleted again or
    // starred there: the copy in the Inbox has no UID yet, so those find it by Message-ID.
    let Some(s) = servers() else { return };
    let user = new_user("chain");
    let mut other = other_client(&s, &user).await;
    for (n, subject) in [(1, "Bounce me"), (2, "Star me")] {
        append(&mut other, "Trash", &raw_message("Jane Doe <jane@example.test>", &user, subject, n), None).await;
    }
    let a = app(&s, &user).await;
    folders_listed(&a).await;
    until("2 messages in Trash", 30, || a.messages(FolderRole::Trash).len() == 2).await;
    let (inbox, trash) = (a.folder(FolderRole::Inbox), a.folder(FolderRole::Trash));
    let ids: Vec<i64> = a.messages(FolderRole::Trash).iter().map(|m| m.id).collect();
    a.core.apply_action(Action::Move { target_folder_id: inbox.id }, &ids, trash.id).unwrap();
    let id = |subject: &str| a.messages(FolderRole::Inbox).into_iter().find(|m| m.subject == subject).unwrap().id;
    a.core.apply_action(Action::Trash, &[id("Bounce me")], inbox.id).unwrap();
    a.core.apply_action(Action::Flag, &[id("Star me")], inbox.id).unwrap();

    let end = Instant::now() + Duration::from_secs(30);
    loop {
        // The star is queued after the moves: done once it's there too.
        let star = on_server(&mut other, "INBOX", "Star me").await;
        let starred = match star.as_slice() {
            [uid] => flags_on_server(&mut other, "INBOX", *uid).await.iter().any(|f| f.contains("Flagged")),
            _ => false,
        };
        let done = starred
            && on_server(&mut other, "Trash", "Bounce me").await.len() == 1
            && on_server(&mut other, "INBOX", "Bounce me").await.is_empty();
        if done {
            break;
        }
        assert!(Instant::now() < end, "the moves and the star never settled on the server");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    // One copy of each here too, where the server has it.
    until("settled here", 20, || {
        a.subjects(FolderRole::Trash) == ["Bounce me"] && a.subjects(FolderRole::Inbox) == ["Star me"]
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn snoozed_mail_waits_in_snoozed_and_comes_back_unread() {
    let Some(s) = servers() else { return };
    let user = new_user("snooze");
    let mut other = other_client(&s, &user).await;
    for (n, subject) in [(1, "Later"), (2, "Now")] {
        append(&mut other, "INBOX", &raw_message("Jane Doe <jane@example.test>", &user, subject, n), Some("(\\Seen)")).await;
    }
    let a = app(&s, &user).await;
    folders_listed(&a).await;
    until("2 messages", 30, || a.messages(FolderRole::Inbox).len() == 2).await;
    until("keyword support known", 30, || {
        a.core.accounts().unwrap().iter().any(|x| x.id == a.account.id && x.keywords == Some(true))
    })
    .await;
    let snoozed = a.core.ensure_snooze_folder(a.account.id).await.expect("a Snoozed folder");
    assert_eq!(a.core.ensure_snooze_folder(a.account.id).await.unwrap(), snoozed, "made once");
    let inbox = a.folder(FolderRole::Inbox);
    let id = |subject: &str| a.messages(FolderRole::Inbox).into_iter().find(|m| m.subject == subject).unwrap().id;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    // An hour from now: stays away. A minute ago: back as soon as it's there.
    a.core.apply_action(Action::Snooze { until: now + 3600 }, &[id("Later")], inbox.id).unwrap();
    a.core.apply_action(Action::Snooze { until: now - 60 }, &[id("Now")], inbox.id).unwrap();
    assert!(a.messages(FolderRole::Inbox).is_empty());

    let end = Instant::now() + Duration::from_secs(90);
    loop {
        let later = on_server(&mut other, "Snoozed", "Later").await;
        let back = on_server(&mut other, "INBOX", "Now").await;
        if later.len() == 1 && back.len() == 1 && on_server(&mut other, "Snoozed", "Now").await.is_empty() {
            let flags = flags_on_server(&mut other, "Snoozed", later[0]).await;
            assert!(flags.iter().any(|f| f.contains("$snooze-")), "the time is on the server: {flags:?}");
            let flags = flags_on_server(&mut other, "INBOX", back[0]).await;
            assert!(!flags.iter().any(|f| f.contains("Seen")), "back unread: {flags:?}");
            break;
        }
        assert!(Instant::now() < end, "snoozing never settled on the server");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    until("settled here", 30, || a.subjects(FolderRole::Inbox) == ["Now"]).await;
    let page = a.core.list_messages(snoozed, 0, 10).unwrap();
    assert_eq!(page.messages.iter().map(|m| m.subject.as_str()).collect::<Vec<_>>(), ["Later"]);
    assert!(page.messages[0].snoozed_until.is_some_and(|t| t > now));
}

/// Mailpit's messages, by subject (its HTTP API, without an HTTP client crate).
async fn mailpit_subjects(base: &str) -> Vec<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let hostport = base.trim_start_matches("http://");
    let mut tcp = tokio::net::TcpStream::connect(hostport).await.expect("Mailpit");
    tcp.write_all(format!("GET /api/v1/messages HTTP/1.0\r\nHost: {hostport}\r\n\r\n").as_bytes()).await.unwrap();
    let mut body = String::new();
    tcp.read_to_string(&mut body).await.unwrap();
    let json = body.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    let v: serde_json::Value = serde_json::from_str(json).expect("Mailpit JSON");
    v["messages"].as_array().into_iter().flatten().filter_map(|m| m["Subject"].as_str().map(str::to_string)).collect()
}

// TODO: hangs before anything is logged (not even the core's debug lines), with nothing reaching
// Mailpit; to be investigated. Run it alone with `-- --ignored`.
#[ignore = "hangs; under investigation"]
#[tokio::test(flavor = "multi_thread")]
async fn sent_mail_goes_out_and_into_sent_once() {
    let Some(s) = servers() else { return };
    let user = new_user("send");
    let mut other = other_client(&s, &user).await;
    let a = app(&s, &user).await;
    folders_listed(&a).await;
    a.core.set_smtp_server(a.account.id, Some((&s.smtp.0, s.smtp.1))).unwrap();
    let subject = format!("Greetings {}", user.split('@').next().unwrap());

    a.core
        .send_message(SendRequest {
            account_id: a.account.id,
            to: vec![Addr { name: Some("Jane Doe".into()), addr: "jane@example.test".into() }],
            cc: vec![],
            bcc: vec![],
            subject: subject.clone(),
            html: "<p>Hello Jane</p>".into(),
            attachments: vec![],
            replying_to: None,
            forwarding: None,
            forward_attachments: vec![],
            draft: None,
            draft_id: None,
            listed: true,
        })
        .await
        .expect("queueing");
    // In Sent here at once, marked as sending.
    let sent = a.messages(FolderRole::Sent);
    assert!(sent.iter().any(|m| m.subject == subject && m.sending), "local copy, sending: {sent:?}");

    // Out through SMTP after the undo window (10 s)…
    let end = Instant::now() + Duration::from_secs(40);
    while !mailpit_subjects(&s.mailpit).await.contains(&subject) {
        assert!(Instant::now() < end, "never reached the SMTP server");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // …filed in Sent on the server (Dovecot doesn't do that itself)…
    let end = Instant::now() + Duration::from_secs(20);
    while on_server(&mut other, "Sent", &subject).await.is_empty() {
        assert!(Instant::now() < end, "no copy in Sent on the server");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    // …and here that server copy is the one message in Sent: no duplicate, no longer sending.
    until("the server's copy takes over", 20, || {
        let sent: Vec<_> = a.messages(FolderRole::Sent).into_iter().filter(|m| m.subject == subject).collect();
        sent.len() == 1 && !sent[0].sending
    })
    .await;
    assert_eq!(on_server(&mut other, "Sent", &subject).await.len(), 1);
}
