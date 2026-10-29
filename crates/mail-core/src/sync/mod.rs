//! Sync engine.
//!
//! Each account runs several independent tasks, each on its own IMAP connection:
//!
//! * **headers**  – walks folders in priority order, calling a pluggable [`HeaderSync`]
//!   strategy one step (one chunk) at a time, so a folder switch preempts within a chunk.
//!   Also runs the STATUS sweep for folder counts between steps.
//! * **interactive** – downloads bodies the user explicitly opened, immediately.
//! * **bodies ×N** – background download of every remaining body, newest first, preferring
//!   the folder on screen. Connected only while there is something to download.
//! * **idle ×2**  – IMAP IDLE on the Inbox and on the folder on screen (the latter connected
//!   only while that folder isn't the Inbox). A push is handled on the IDLE connection itself,
//!   including that folder's counts; bodies of new mail go to the interactive connection.
//!
//! Steady state is 3 connections per account (4 while a non-Inbox folder is on screen);
//! the download workers add up to N more only while bodies are pending.
//!
//! Header strategies live in submodules; `chunked::ChunkedNewestFirst` is the default.

pub mod chunked;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use futures::future::BoxFuture;
use tokio::sync::{Notify, broadcast, mpsc, watch};
use tokio::task::JoinHandle;

use crate::imap::{self, Connection, ServerConfig};
use crate::mime;
use crate::partial;
use crate::model::*;
use crate::store::Store;

/// Outcome of one strategy step on one folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// More work remains in this folder (call again).
    More,
    /// Folder is fully in sync for this pass.
    Done,
}

/// Everything a strategy needs while syncing a folder.
pub struct FolderCtx<'a> {
    pub store: &'a Store,
    pub account: &'a Account,
    pub folder: &'a Folder,
    /// Gmail label (as returned by X-GM-LABELS) -> folder id.
    pub label_folders: &'a HashMap<String, i64>,
    /// True on the first step for this folder in the current pass: check for new mail,
    /// flag changes and deletions, not just backfill.
    pub first_in_pass: bool,
    /// `Store::local_epoch` when the step started (before checking for pending actions): the
    /// step's writes fail with `SyncSuperseded` if the user changed something meanwhile.
    pub epoch: u64,
    /// Report progress and changed folders back to the engine.
    pub report: &'a (dyn Fn(Progress) + Sync),
}

#[derive(Debug, Clone)]
pub enum Progress {
    Changed(Vec<i64>),
    /// New mail arrived in `folder_id` (incremental, not backfill): download these now.
    NewMail { folder_id: i64, uids: Vec<u32> },
    Headers { folder_id: i64, done: u64, total: u64 },
}

/// A header sync algorithm. Implement this to try an alternative and compare it with
/// `mailsync bench --strategy <name>`.
pub trait HeaderSync: Send + Sync {
    fn name(&self) -> &'static str;

    /// Order in which folders should be synced. `focus` is the folder on screen.
    fn plan(&self, account: &Account, folders: &[Folder], focus: Option<i64>, gmail: bool) -> Vec<i64> {
        default_plan(account, folders, focus, gmail)
    }

    /// Do one bounded unit of work on `ctx.folder`.
    fn step<'a>(&'a self, conn: &'a mut Connection, ctx: &'a FolderCtx<'a>) -> BoxFuture<'a, Result<Step>>;
}

/// Focused folder first, then by role. For Gmail only folders that are not reachable through
/// `[Gmail]/All Mail` labels are synced directly (Inbox first so it fills instantly).
pub fn default_plan(_account: &Account, folders: &[Folder], focus: Option<i64>, gmail: bool) -> Vec<i64> {
    fn rank(role: Option<FolderRole>) -> u8 {
        match role {
            Some(FolderRole::Inbox) => 0,
            Some(FolderRole::All) => 1,
            Some(FolderRole::Sent) => 2,
            Some(FolderRole::Drafts) => 3,
            Some(FolderRole::Archive) => 4,
            None => 5,
            Some(FolderRole::Flagged) | Some(FolderRole::Important) => 6,
            Some(FolderRole::Junk) => 7,
            Some(FolderRole::Trash) => 8,
        }
    }
    let mut list: Vec<&Folder> = folders
        .iter()
        .filter(|f| f.selectable)
        .filter(|f| {
            !gmail
                || Some(f.id) == focus
                || matches!(f.role, Some(FolderRole::Inbox | FolderRole::All | FolderRole::Trash | FolderRole::Junk))
        })
        .collect();
    list.sort_by_key(|f| (Some(f.id) != focus, rank(f.role), f.raw_name.clone()));
    list.into_iter().map(|f| f.id).collect()
}

/// Map Gmail labels to folders: system labels by role, user labels by mailbox name.
/// All Mail, Trash and Spam are real mailboxes, not labels, so they are excluded.
pub fn gmail_label_map(folders: &[Folder]) -> HashMap<String, i64> {
    let mut map = HashMap::new();
    for f in folders {
        if !f.selectable || matches!(f.role, Some(FolderRole::All | FolderRole::Trash | FolderRole::Junk)) {
            continue;
        }
        map.insert(f.raw_name.clone(), f.id);
        let sys = match f.role {
            Some(FolderRole::Inbox) => "\\Inbox",
            Some(FolderRole::Sent) => "\\Sent",
            Some(FolderRole::Drafts) => "\\Draft",
            Some(FolderRole::Important) => "\\Important",
            Some(FolderRole::Flagged) => "\\Starred",
            _ => continue,
        };
        map.insert(sys.to_string(), f.id);
    }
    map
}

/// Tunables, mainly for benchmarking.
#[derive(Debug, Clone)]
pub struct SyncOptions {
    pub body_workers: usize,
    pub download_bodies: bool,
    pub refresh_interval: Duration,
}

impl Default for SyncOptions {
    fn default() -> Self {
        SyncOptions { body_workers: 3, download_bodies: true, refresh_interval: Duration::from_secs(60) }
    }
}

struct Shared {
    store: Arc<Store>,
    account: Account,
    server: ServerConfig,
    strategy: Arc<dyn HeaderSync>,
    events: broadcast::Sender<CoreEvent>,
    focus: watch::Receiver<Option<i64>>,
    /// What the header task is doing; combined with body progress by `status_task`.
    header: Mutex<SyncStatus>,
    /// Last status published to the UI.
    status: Mutex<SyncStatus>,
    gmail: Mutex<bool>,
    /// Credentials were rejected: every task parks until the account is restarted with new
    /// ones, rather than retrying (repeated bad logins can get an account locked).
    auth_failed: std::sync::atomic::AtomicBool,
    /// Bodies currently being downloaded by some worker.
    inflight: Mutex<HashSet<i64>>,
    /// Background download failures per message this session; after
    /// `MAX_BODY_FAILURES` a message is skipped so it can't block the queue.
    body_failures: Mutex<HashMap<i64, u32>>,
    /// Signalled when new headers arrive (wakes idle body workers).
    new_headers: Notify,
    /// Folders the server told us changed (IDLE); the headers task re-syncs them first.
    dirty: Mutex<HashSet<i64>>,
    /// Wakes the headers task (dirty folders, count sweep requests).
    refresh: Notify,
    /// Wakes the interactive task: an action was queued or cancelled.
    outbox: Notify,
    /// A STATUS sweep of all folder counts is due (run by the headers task between steps).
    counts_due: std::sync::atomic::AtomicBool,
    /// Folders whose contents changed since their counts were last fetched; recounted
    /// individually between steps (cheap, unlike a full sweep over every folder).
    counts_dirty: Mutex<HashSet<i64>>,
    urgent: mpsc::UnboundedSender<Job>,
    options: SyncOptions,
}

impl Shared {
    fn emit(&self, e: CoreEvent) {
        let _ = self.events.send(e);
    }

    fn set_status(&self, phase: SyncPhase, folder_id: Option<i64>, done: u64, total: u64, message: Option<String>) {
        // Once credentials are rejected, nothing but that is reported until the restart.
        if phase != SyncPhase::Auth && self.auth_failed.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        *self.header.lock().unwrap() =
            SyncStatus { account_id: self.account.id, phase, folder_id, done, total, message };
    }

    async fn connect(&self) -> Result<Connection> {
        use std::sync::atomic::Ordering;
        if self.auth_failed.load(Ordering::SeqCst) {
            // Park: AccountSync is replaced once the user supplies a new password.
            std::future::pending::<()>().await;
        }
        match imap::connect(&self.server).await {
            Ok(conn) => {
                *self.gmail.lock().unwrap() = conn.caps.gmail;
                Ok(conn)
            }
            Err(e) => {
                if imap::is_auth_error(&e) && !self.auth_failed.swap(true, Ordering::SeqCst) {
                    tracing::warn!(account = self.account.email, "credentials rejected; pausing sync: {e:#}");
                    self.set_status(SyncPhase::Auth, None, 0, 0, Some(format!("{e:#}")));
                }
                Err(e)
            }
        }
    }
}

/// Work for the interactive connection.
enum Job {
    Body(i64),
    Seen { id: i64, seen: bool },
    Part(PartRequest),
}

/// Download one omitted attachment (a single MIME part) of a stored message.
pub struct PartRequest {
    pub message_id: i64,
    /// Position among the message's omitted attachments.
    pub ordinal: usize,
    pub section: Option<String>,
    pub encoding: Option<String>,
    pub reply: tokio::sync::oneshot::Sender<Result<Vec<u8>>>,
}

/// Handle to a running account sync.
pub struct AccountSync {
    shared: Arc<Shared>,
    focus_tx: watch::Sender<Option<i64>>,
    urgent_tx: mpsc::UnboundedSender<Job>,
    tasks: Vec<JoinHandle<()>>,
}

impl AccountSync {
    pub fn start(
        store: Arc<Store>,
        account: Account,
        server: ServerConfig,
        strategy: Arc<dyn HeaderSync>,
        events: broadcast::Sender<CoreEvent>,
        options: SyncOptions,
    ) -> AccountSync {
        let (focus_tx, focus) = watch::channel(None);
        let (urgent_tx, urgent_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            store,
            header: Mutex::new(initial_status(account.id)),
            status: Mutex::new(initial_status(account.id)),
            account,
            server,
            strategy,
            events,
            focus,
            gmail: Mutex::new(false),
            auth_failed: std::sync::atomic::AtomicBool::new(false),
            inflight: Mutex::new(HashSet::new()),
            body_failures: Mutex::new(HashMap::new()),
            new_headers: Notify::new(),
            dirty: Mutex::new(HashSet::new()),
            refresh: Notify::new(),
            outbox: Notify::new(),
            counts_due: std::sync::atomic::AtomicBool::new(true),
            counts_dirty: Mutex::new(HashSet::new()),
            urgent: urgent_tx.clone(),
            options,
        });

        let mut tasks = vec![
            tokio::spawn(supervise("headers", shared.clone(), |s| Box::pin(header_task(s)))),
            tokio::spawn(interactive_task(shared.clone(), urgent_rx)),
            tokio::spawn(status_task(shared.clone())),
            tokio::spawn(supervise("idle-inbox", shared.clone(), |s| Box::pin(idle_task(s, false)))),
            tokio::spawn(supervise("idle-focus", shared.clone(), |s| Box::pin(idle_task(s, true)))),
        ];
        if shared.options.download_bodies {
            for i in 0..shared.options.body_workers {
                tasks.push(tokio::spawn(supervise(
                    if i == 0 { "bodies" } else { "bodies+" },
                    shared.clone(),
                    |s| Box::pin(body_task(s)),
                )));
            }
        }
        AccountSync { shared, focus_tx, urgent_tx, tasks }
    }

    pub fn focus(&self, folder_id: i64) {
        let _ = self.focus_tx.send(Some(folder_id));
    }

    /// Fetch this message's body ahead of everything else.
    pub fn request_body(&self, message_id: i64) {
        let _ = self.urgent_tx.send(Job::Body(message_id));
    }

    /// A user action was queued: send it when due.
    pub fn wake_outbox(&self) {
        self.shared.outbox.notify_one();
    }

    /// Fetch an omitted attachment on the interactive connection.
    pub fn fetch_part(&self, req: PartRequest) {
        if let Err(mpsc::error::SendError(Job::Part(req))) = self.urgent_tx.send(Job::Part(req)) {
            let _ = req.reply.send(Err(anyhow::anyhow!("account is not syncing")));
        }
    }

    /// Push a \\Seen change to the server (the local store is updated by the caller).
    pub fn set_seen(&self, message_id: i64, seen: bool) {
        let _ = self.urgent_tx.send(Job::Seen { id: message_id, seen });
    }

    pub fn status(&self) -> SyncStatus {
        self.shared.status.lock().unwrap().clone()
    }

    pub fn stop(&self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Drop for AccountSync {
    fn drop(&mut self) {
        self.stop();
    }
}

fn initial_status(account_id: i64) -> SyncStatus {
    SyncStatus { account_id, phase: SyncPhase::Connecting, folder_id: None, done: 0, total: 0, message: None }
}

/// Publish one coherent status per account: header work while there is any, otherwise body
/// download progress. Emitted on change, at most once a second, so the UI can't get stuck on
/// a stale intermediate state.
async fn status_task(shared: Arc<Shared>) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tick.tick().await;
        let mut s = shared.header.lock().unwrap().clone();
        if s.phase == SyncPhase::Idle && shared.options.download_bodies {
            if let Ok((total, have)) = shared.store.count_missing_bodies(shared.account.id) {
                // Messages skipped this session (repeated failures) aren't "pending".
                let skipped = shared.body_failures.lock().unwrap().values().filter(|&&n| n >= MAX_BODY_FAILURES).count() as u64;
                let total = total.saturating_sub(skipped);
                if have < total {
                    s = SyncStatus { phase: SyncPhase::Bodies, folder_id: None, done: have, total, ..s };
                }
            }
        }
        let changed = {
            let mut cur = shared.status.lock().unwrap();
            let changed = (cur.phase, cur.folder_id, cur.done, cur.total, &cur.message)
                != (s.phase, s.folder_id, s.done, s.total, &s.message);
            *cur = s.clone();
            changed
        };
        if changed {
            shared.emit(CoreEvent::Sync(s));
        }
    }
}

/// A connection that moves no data for this long during a sync step is considered stalled:
/// it's dropped and reconnected. Measured as inactivity, not total time, so a big chunk on a
/// slow link is never cut off while it's still arriving.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Run a connection-owning task forever, reconnecting with exponential backoff.
async fn supervise<F>(name: &'static str, shared: Arc<Shared>, f: F)
where
    F: Fn(Arc<Shared>) -> BoxFuture<'static, Result<()>>,
{
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = std::time::Instant::now();
        match f(shared.clone()).await {
            Ok(()) => return,
            Err(e) => {
                tracing::warn!(account = shared.account.email, task = name, "sync error: {e:#}");
                let limited = imap::is_limit_error(&e);
                if name == "headers" && !imap::is_auth_error(&e) {
                    let phase = if limited { SyncPhase::Limited } else { SyncPhase::Error };
                    shared.set_status(phase, None, 0, 0, Some(format!("{e:#}")));
                }
                if limited {
                    // Connections are scarce: wait longer, with jitter so several of our
                    // instances (or tasks) don't all retry at the same moment.
                    backoff = backoff.max(Duration::from_secs(15));
                    let jitter = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.subsec_millis() as u64 * 10)
                        .unwrap_or(0);
                    tokio::time::sleep(Duration::from_millis(jitter)).await;
                }
            }
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_secs(1);
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(120));
    }
}

async fn header_task(shared: Arc<Shared>) -> Result<()> {
    shared.set_status(SyncPhase::Connecting, None, 0, 0, None);
    let mut conn = shared.connect().await?;
    let mut focus = shared.focus.clone();
    let account = &shared.account;
    let mut first_step_done = false;
    let mut first_pass = true;
    let mut last_full_sweep: Option<tokio::time::Instant> = None;

    loop {
        // Folder structure first, every pass, so new/renamed folders appear promptly. Only the
        // first pass after connecting is reported: routine re-checks aren't news.
        if first_pass {
            shared.set_status(SyncPhase::Folders, None, 0, 0, None);
        }
        let listed = conn.list_folders().await?;
        shared.store.sync_folder_list(account.id, &listed)?;
        shared.emit(CoreEvent::FoldersChanged { account_id: account.id });

        let mut done: HashSet<i64> = HashSet::new();
        let mut started: HashSet<i64> = HashSet::new();
        let refresh = tokio::time::Instant::now() + shared.options.refresh_interval;
        // Full count sweep (one STATUS per folder; ~15 s for 150 folders) at most every
        // FULL_SWEEP_INTERVAL. In between, pushes keep the Inbox and focused folder current and
        // folders that sync saw change are recounted individually.
        if last_full_sweep.is_none_or(|t| t.elapsed() >= FULL_SWEEP_INTERVAL) {
            shared.counts_due.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        first_pass = false;

        loop {
            let epoch = shared.store.local_epoch();
            // A pending user action (archive, move, ...) hasn't reached the server yet: syncing
            // now would put the message back. Wait for the outbox to drain (normally ≤ 5 s).
            if shared.store.has_pending_ops(account.id)? {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                    _ = shared.refresh.notified() => {}
                }
                continue;
            }
            // Counts: after the first step (so the focused folder's new mail isn't delayed
            // at startup), or whenever there's nothing else to do.
            if first_step_done {
                if shared.counts_due.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    sweep_counts(&shared, &mut conn, None).await?;
                    last_full_sweep = Some(tokio::time::Instant::now());
                } else {
                    let dirty: HashSet<i64> = std::mem::take(&mut *shared.counts_dirty.lock().unwrap());
                    if !dirty.is_empty() {
                        sweep_counts(&shared, &mut conn, Some(&dirty)).await?;
                    }
                }
            }
            let folders = shared.store.folders(account.id)?;
            let label_folders = gmail_label_map(&folders);
            let current_focus = *focus.borrow_and_update();
            let mut plan = shared.strategy.plan(account, &folders, current_focus, conn.caps.gmail);
            // Server-pushed changes jump the queue and get a fresh incremental check.
            let dirty: Vec<i64> = shared.dirty.lock().unwrap().drain().collect();
            for id in dirty.iter().rev() {
                done.remove(id);
                started.remove(id);
                plan.retain(|p| p != id);
                plan.insert(0, *id);
            }
            let Some(&next) = plan.iter().find(|id| !done.contains(id)) else {
                if shared.counts_due.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    sweep_counts(&shared, &mut conn, None).await?;
                    last_full_sweep = Some(tokio::time::Instant::now());
                    continue;
                }
                let dirty: HashSet<i64> = std::mem::take(&mut *shared.counts_dirty.lock().unwrap());
                if !dirty.is_empty() {
                    sweep_counts(&shared, &mut conn, Some(&dirty)).await?;
                    continue;
                }
                // Everything is in sync: wait for the user to switch folders or the refresh timer.
                shared.set_status(SyncPhase::Idle, None, 0, 0, None);
                tokio::select! {
                    r = focus.changed() => {
                        r?;
                        if let Some(f) = *focus.borrow() {
                            done.remove(&f);
                            started.remove(&f);
                        }
                        continue;
                    }
                    _ = shared.refresh.notified() => continue,
                    _ = tokio::time::sleep_until(refresh) => break,
                }
            };
            let folder = folders.iter().find(|f| f.id == next).expect("planned folder exists");

            let report = |p: Progress| match p {
                Progress::Changed(ids) => {
                    shared.new_headers.notify_waiters();
                    shared.counts_dirty.lock().unwrap().extend(ids.iter().copied());
                    shared.emit(CoreEvent::MessagesChanged { folder_ids: ids });
                }
                Progress::Headers { folder_id, done, total } => {
                    shared.set_status(SyncPhase::Headers, Some(folder_id), done, total, None);
                }
                Progress::NewMail { folder_id, uids } => {
                    if let Ok(ids) = shared.store.message_ids_for_uids(folder_id, &uids) {
                        for id in ids {
                            let _ = shared.urgent.send(Job::Body(id));
                        }
                    }
                }
            };
            let ctx = FolderCtx {
                store: &shared.store,
                account,
                folder,
                label_folders: &label_folders,
                first_in_pass: started.insert(next),
                epoch,
                report: &report,
            };
            let activity = conn.activity.clone();
            let what = format!("syncing {}", folder.raw_name);
            let step = imap::stall_timeout(&activity, STALL_TIMEOUT, &what, shared.strategy.step(&mut conn, &ctx)).await?;
            first_step_done = true;
            match step {
                Ok(Step::Done) => {
                    done.insert(next);
                }
                Ok(Step::More) => {}
                Err(e) if e.is::<crate::store::SyncSuperseded>() => {
                    // The user acted while this ran: redo the folder once that's on the server.
                    started.remove(&next);
                    shared.dirty.lock().unwrap().insert(next);
                }
                Err(e) => {
                    // A broken folder shouldn't stall the rest; skip it this pass. Connection
                    // errors will surface again on the next folder and trigger a reconnect.
                    tracing::warn!(folder = folder.raw_name, "folder sync failed: {e:#}");
                    done.insert(next);
                    if conn.session.noop().await.is_err() {
                        return Err(e);
                    }
                }
            }
        }
    }
}

/// How often every folder's counts are refreshed with STATUS (changed folders sooner).
const FULL_SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// STATUS sweep: total/unread for every selectable folder (or just `only`), on the caller's
/// connection.
async fn sweep_counts(shared: &Shared, conn: &mut Connection, only: Option<&HashSet<i64>>) -> Result<()> {
    let folders = shared.store.folders(shared.account.id)?;
    let wanted = folders.iter().filter(|f| f.selectable && only.is_none_or(|o| o.contains(&f.id)));
    for (i, f) in wanted.enumerate() {
        match conn.status(&f.raw_name).await {
            Ok((total, unread)) => shared.store.set_folder_counts(f.id, total, unread)?,
            Err(e) => tracing::debug!(folder = f.raw_name, "STATUS failed: {e:#}"),
        }
        if i % 10 == 9 {
            shared.emit(CoreEvent::FoldersChanged { account_id: shared.account.id });
        }
    }
    shared.emit(CoreEvent::FoldersChanged { account_id: shared.account.id });
    Ok(())
}

/// IMAP IDLE on one folder: the Inbox (`follow_focus == false`) or the folder on screen.
/// A push is handled right here (see `sync_pushed_folder`). IDLE is restarted every 25
/// minutes, under the 29-minute limit servers enforce. The focus watcher holds a connection
/// only while the folder on screen isn't the Inbox (which has its own watcher).
async fn idle_task(shared: Arc<Shared>, follow_focus: bool) -> Result<()> {
    let mut focus = shared.focus.clone();
    let mut conn: Option<Connection> = None;
    loop {
        let inbox = shared
            .store
            .folders(shared.account.id)?
            .into_iter()
            .find(|f| f.role == Some(FolderRole::Inbox));
        let current = *focus.borrow_and_update();
        let target = if follow_focus {
            current.filter(|f| Some(*f) != inbox.as_ref().map(|i| i.id))
        } else {
            inbox.as_ref().map(|i| i.id)
        };
        let Some(folder_id) = target else {
            if let Some(c) = conn.take() {
                c.logout().await;
            }
            tokio::select! {
                r = focus.changed() => { r?; }
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            }
            continue;
        };
        let Some((_, raw)) = shared.store.folder(folder_id)? else { continue };

        let mut c = match conn.take() {
            Some(c) => c,
            None => shared.connect().await?,
        };
        if !c.caps.idle {
            tracing::info!("server has no IDLE; relying on polling");
            c.logout().await;
            std::future::pending::<()>().await;
            unreachable!();
        }
        c.select(&raw).await?;
        let imap::Connection { session, caps, activity, .. } = c;
        let mut handle = session.idle();
        handle.init().await?;
        let (wait, stop) = handle.wait_with_timeout(Duration::from_secs(25 * 60));
        let pushed = tokio::select! {
            r = wait => matches!(r?, async_imap::extensions::idle::IdleResponse::NewData(_)),
            r = focus.changed(), if follow_focus => { r?; false }
        };
        drop(stop);
        let session = handle.done().await?;
        let mut c = imap::Connection { session, caps, selected: Some(raw.clone()), activity };

        if pushed {
            let t = std::time::Instant::now();
            // Handle the push right here (new mail, expunges, flag changes, counts) rather than
            // waiting for the headers task, which may be busy with a long step elsewhere.
            sync_pushed_folder(&shared, &mut c, folder_id).await?;
            tracing::info!(folder = raw, ms = t.elapsed().as_millis() as u64, "IDLE push handled");
        }
        conn = Some(c);
    }
}

/// One incremental step (new mail, flag changes, expunges) on a folder the server just told
/// us about, on the caller's connection. Bodies of new mail are queued at top priority.
async fn sync_pushed_folder(shared: &Shared, conn: &mut Connection, folder_id: i64) -> Result<()> {
    let epoch = shared.store.local_epoch();
    // Leave it to the headers task, which waits for pending actions to be sent first.
    if shared.store.has_pending_ops(shared.account.id)? {
        shared.dirty.lock().unwrap().insert(folder_id);
        shared.refresh.notify_one();
        return Ok(());
    }
    let folders = shared.store.folders(shared.account.id)?;
    let Some(folder) = folders.iter().find(|f| f.id == folder_id) else { return Ok(()) };
    let label_folders = gmail_label_map(&folders);
    let report = |p: Progress| match p {
        Progress::Changed(ids) => {
            shared.new_headers.notify_waiters();
            // Other folders touched (e.g. Gmail labels) are recounted by the headers task.
            shared.counts_dirty.lock().unwrap().extend(ids.iter().copied().filter(|&id| id != folder_id));
            shared.emit(CoreEvent::MessagesChanged { folder_ids: ids });
        }
        Progress::NewMail { folder_id, uids } => {
            if let Ok(ids) = shared.store.message_ids_for_uids(folder_id, &uids) {
                for id in ids {
                    let _ = shared.urgent.send(Job::Body(id));
                }
            }
        }
        Progress::Headers { .. } => {}
    };
    let ctx = FolderCtx {
        store: &shared.store,
        account: &shared.account,
        folder,
        label_folders: &label_folders,
        first_in_pass: true,
        epoch,
        report: &report,
    };
    let activity = conn.activity.clone();
    let what = format!("handling a push for {}", folder.raw_name);
    match imap::stall_timeout(&activity, STALL_TIMEOUT, &what, shared.strategy.step(conn, &ctx)).await? {
        Err(e) if e.is::<crate::store::SyncSuperseded>() => {
            // The user acted meanwhile: hand the folder to the headers task (see above).
            shared.dirty.lock().unwrap().insert(folder_id);
            shared.refresh.notify_one();
            return Ok(());
        }
        r => {
            r?;
        }
    }
    // This folder's counts, now, on this connection (re-SELECT for EXISTS, then count UNSEEN);
    // the other folders catch up in the headers task's next sweep.
    let mb = conn.select(&folder.raw_name).await?;
    let unread = conn.session.search("UNSEEN").await?.len() as u32;
    shared.store.set_folder_counts(folder_id, mb.exists, unread)?;
    shared.emit(CoreEvent::FoldersChanged { account_id: shared.account.id });
    if !shared.counts_dirty.lock().unwrap().is_empty() {
        shared.refresh.notify_one();
    }
    Ok(())
}

fn preferred_body_folder(shared: &Shared) -> Option<i64> {
    // For Gmail every message is in All Mail, so download from there to stay on one SELECT.
    if !*shared.gmail.lock().unwrap() {
        return None;
    }
    shared
        .store
        .folders(shared.account.id)
        .ok()?
        .into_iter()
        .find(|f| f.role == Some(FolderRole::All))
        .map(|f| f.id)
}

/// Download bodies, fetching each from `preferred` when the message is there.
/// `whole` skips BODYSTRUCTURE and fetches complete messages (the fallback for messages whose
/// structure the IMAP parser can't handle).
async fn download(
    shared: &Shared,
    conn: &mut Connection,
    ids: &[i64],
    preferred: Option<i64>,
    announce: bool,
    whole: bool,
) -> Result<HashSet<i64>> {
    // Group by the folder we can fetch each message from.
    let mut by_folder: HashMap<String, Vec<(u32, i64)>> = HashMap::new();
    for &id in ids {
        if let Some(loc) = shared.store.body_location(id, preferred)? {
            by_folder.entry(loc.raw_name).or_default().push((loc.uid, id));
        }
    }
    let mut changed = HashSet::new();
    let mut stored = HashSet::new();
    for (raw_name, items) in by_folder {
        conn.ensure_selected(&raw_name).await?;
        let uids: Vec<u32> = items.iter().map(|(u, _)| *u).collect();
        let by_uid: HashMap<u32, i64> = items.into_iter().collect();

        // Body-only: messages without attachments come down whole in one FETCH; the rest
        // fetch just their text parts (grouped by identical part lists) and are rebuilt.
        // Some malformed BODYSTRUCTUREs make the parser wait forever for data that never comes,
        // so this step gets a short stall timeout; the caller retries such messages with `whole`.
        let plans: HashMap<u32, Option<partial::Plan>> = if whole {
            HashMap::new()
        } else {
            let activity = conn.activity.clone();
            imap::stall_timeout(&activity, Duration::from_secs(20), "reading BODYSTRUCTURE", conn.fetch_plans(&uids))
                .await
                .map_err(|_| anyhow::anyhow!("BODYSTRUCTURE response unreadable (stalled)"))??
                .into_iter()
                .collect()
        };
        let whole: Vec<u32> = uids.iter().copied().filter(|u| !matches!(plans.get(u), Some(Some(_)))).collect();
        let mut groups: HashMap<String, Vec<u32>> = HashMap::new();
        for (uid, plan) in &plans {
            if let Some(plan) = plan {
                groups.entry(plan.fetch_items()).or_default().push(*uid);
            }
        }

        let mut downloaded: Vec<(u32, Vec<u8>, Option<String>)> = conn
            .fetch_bodies(&whole)
            .await?
            .into_iter()
            .map(|(uid, raw)| (uid, raw, None))
            .collect();
        for (items, group) in groups {
            for f in conn.fetch_items(&group, &items).await? {
                let Some(uid) = f.uid else { continue };
                let Some(Some(plan)) = plans.get(&uid) else { continue };
                match partial::rebuild(plan, &imap::FetchSections(&f), uid) {
                    Some(raw) => downloaded.push((uid, raw, Some(serde_json::to_string(&plan.omitted)?))),
                    None => tracing::debug!(uid, "could not rebuild partial message"),
                }
            }
        }

        for (uid, raw, omitted) in downloaded {
            let Some(&id) = by_uid.get(&uid) else { continue };
            let (snippet, has_att) = mime::body_summary(&raw);
            shared.store.store_body(id, &raw, snippet.as_deref(), has_att || omitted.is_some(), omitted.as_deref())?;
            stored.insert(id);
            changed.extend(shared.store.folders_of_message(id)?);
            if announce {
                shared.emit(CoreEvent::MessageReady { id });
            }
        }
    }
    if !changed.is_empty() {
        shared.emit(CoreEvent::MessagesChanged { folder_ids: changed.into_iter().collect() });
    }
    Ok(stored)
}

/// Dedicated connection for work the user is waiting on (opened messages, flag changes).
/// Connected up front and kept alive, never busy with background downloads, and it fetches
/// from the folder on screen (already selected, small) rather than Gmail's huge All Mail.
async fn interactive_task(shared: Arc<Shared>, mut rx: mpsc::UnboundedReceiver<Job>) {
    const KEEPALIVE: Duration = Duration::from_secs(4 * 60);
    let mut conn: Option<Connection> = None;
    loop {
        if conn.is_none() {
            match shared.connect().await {
                Ok(c) => conn = Some(c),
                Err(e) => tracing::warn!("interactive connect failed: {e:#}"),
            }
        }
        // Pre-select the folder on screen so the first click doesn't pay for SELECT.
        let focus = *shared.focus.borrow();
        if let (Some(c), Some(focus)) = (conn.as_mut(), focus) {
            if let Ok(Some((_, raw))) = shared.store.folder(focus) {
                if c.ensure_selected(&raw).await.is_err() {
                    conn = None;
                }
            }
        }

        // Queued user actions go first: they're what the user is waiting on.
        if let Some(c) = conn.as_mut() {
            if let Err(e) = run_outbox(&shared, c).await {
                tracing::debug!("outbox: {e:#}");
                conn = None;
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        }
        let next_due = shared.store.next_op_due(shared.account.id).ok().flatten();
        let until_due = next_due.map(|t| Duration::from_millis((t - now_ms()).max(0) as u64)).unwrap_or(KEEPALIVE);

        let first = tokio::select! {
            job = rx.recv() => match job {
                Some(j) => j,
                None => return,
            },
            _ = shared.outbox.notified() => continue,
            _ = tokio::time::sleep(until_due.min(KEEPALIVE)), if next_due.is_some() => continue,
            _ = tokio::time::sleep(KEEPALIVE) => {
                if let Some(c) = conn.as_mut() {
                    if c.session.noop().await.is_err() {
                        conn = None;
                    }
                }
                continue;
            }
        };
        let mut jobs = vec![first];
        while let Ok(j) = rx.try_recv() {
            jobs.push(j);
        }
        let mut bodies = Vec::new();
        let mut seen = Vec::new();
        let mut parts = Vec::new();
        for j in jobs {
            match j {
                // Bodies may have landed via a background worker in the meantime.
                Job::Body(id) if shared.store.body(id).ok().flatten().is_some() => {
                    shared.emit(CoreEvent::MessageReady { id });
                }
                Job::Body(id) => bodies.push(id),
                Job::Seen { id, seen: s } => seen.push((id, s)),
                Job::Part(p) => parts.push(p),
            }
        }
        let focus = *shared.focus.borrow();
        let pending_messages = !(bodies.is_empty() && seen.is_empty());
        for attempt in 0..(if pending_messages { 2 } else { 0 }) {
            let started = std::time::Instant::now();
            if conn.is_none() {
                match shared.connect().await {
                    Ok(c) => conn = Some(c),
                    Err(e) => {
                        tracing::warn!("interactive connect failed: {e:#}");
                        break;
                    }
                }
            }
            let c = conn.as_mut().unwrap();
            let activity = c.activity.clone();
            let result = imap::stall_timeout(&activity, Duration::from_secs(30), "fetching an opened message", async {
                // Retry with a whole-message fetch if the body-only path fails.
                if download(&shared, c, &bodies, focus, true, false).await.is_err() {
                    download(&shared, c, &bodies, focus, true, true).await?;
                }
                store_seen(&shared, c, &seen, focus).await
            })
            .await
            .and_then(|r| r);
            match result {
                Ok(()) => {
                    if !bodies.is_empty() {
                        tracing::info!(n = bodies.len(), ms = started.elapsed().as_millis() as u64, "opened message fetched");
                    }
                    break;
                }
                Err(e) => {
                    // Likely a connection the server dropped; reconnect once.
                    tracing::debug!(attempt, "interactive job failed: {e:#}");
                    conn = None;
                }
            }
        }
        for part in parts {
            let mut result = Err(anyhow::anyhow!("couldn't connect to the server"));
            for attempt in 0..2 {
                if conn.is_none() {
                    match shared.connect().await {
                        Ok(c) => conn = Some(c),
                        Err(e) => {
                            result = Err(e);
                            break;
                        }
                    }
                }
                let c = conn.as_mut().unwrap();
                let activity = c.activity.clone();
                result = imap::stall_timeout(&activity, Duration::from_secs(60), "downloading an attachment", fetch_part(&shared, c, &part, focus))
                    .await
                    .and_then(|r| r);
                match &result {
                    Ok(_) => break,
                    Err(e) => {
                        tracing::debug!(attempt, "attachment fetch failed: {e:#}");
                        conn = None;
                    }
                }
            }
            let _ = part.reply.send(result);
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Send every due queued action. Progress is saved after each command, so a dropped
/// connection resumes where it stopped. A server rejection (NO/BAD) rolls the action back
/// locally and tells the user; connection trouble retries with backoff.
async fn run_outbox(shared: &Shared, conn: &mut Connection) -> Result<()> {
    use crate::actions::ServerOp;
    while let Some(op) = shared.store.claim_due_op(shared.account.id, now_ms())? {
        let mut ops: Vec<ServerOp> = serde_json::from_str(&op.ops).unwrap_or_default();
        let folders: Vec<i64> = serde_json::from_str(&op.folders).unwrap_or_default();
        let mut failure: Option<anyhow::Error> = None;
        while !ops.is_empty() {
            let activity = conn.activity.clone();
            match imap::stall_timeout(&activity, STALL_TIMEOUT, "sending an action", conn.run_op(&ops[0])).await.and_then(|r| r) {
                Ok(()) => {
                    ops.remove(0);
                    shared.store.update_op(op.id, &serde_json::to_string(&ops)?)?;
                }
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
        let snapshot: crate::store::Snapshot = serde_json::from_str(&op.snapshot).unwrap_or_default();
        match failure {
            None => {
                shared.store.finish_op(op.id)?;
                let ids: Vec<i64> = snapshot.flags.iter().map(|(m, _)| *m).collect();
                // Messages deleted for good are in no folder now: drop them and their bodies.
                shared.store.purge_orphans(&ids)?;
                // Re-sync what changed so UIDs of moved copies and counts catch up.
                shared.dirty.lock().unwrap().extend(folders.iter().copied());
                shared.counts_dirty.lock().unwrap().extend(folders.iter().copied());
                shared.refresh.notify_one();
            }
            Some(e) => {
                let rejected = e.chain().any(|c| {
                    matches!(c.downcast_ref::<async_imap::error::Error>(), Some(async_imap::error::Error::No(_) | async_imap::error::Error::Bad(_)))
                        || c.is::<imap::OpRejected>()
                });
                if rejected || op.attempts >= 5 {
                    tracing::warn!(op = op.id, "action failed, rolling back: {e:#}");
                    shared.store.finish_op(op.id)?;
                    let mut changed = shared.store.restore(&snapshot)?;
                    changed.extend(snapshot.entries.iter().map(|e| e.folder_id));
                    changed.sort_unstable();
                    changed.dedup();
                    shared.emit(CoreEvent::MessagesChanged { folder_ids: changed });
                    shared.emit(CoreEvent::FoldersChanged { account_id: shared.account.id });
                    shared.emit(CoreEvent::ActionFailed {
                        message: format!("The server didn't accept the change, so it was undone: {e:#}"),
                    });
                    shared.dirty.lock().unwrap().extend(folders.iter().copied());
                    shared.refresh.notify_one();
                    if !rejected {
                        return Err(e);
                    }
                } else {
                    let backoff = 2_000i64 << op.attempts.min(5);
                    shared.store.retry_op(op.id, now_ms() + backoff)?;
                    return Err(e);
                }
            }
        }
    }
    Ok(())
}

/// Fetch one omitted attachment: its section is recorded when the body was downloaded, or
/// recovered from BODYSTRUCTURE for entries stored before that.
async fn fetch_part(shared: &Shared, conn: &mut Connection, req: &PartRequest, preferred: Option<i64>) -> Result<Vec<u8>> {
    let loc = shared
        .store
        .body_location(req.message_id, preferred)?
        .ok_or_else(|| anyhow::anyhow!("This message is no longer on the server."))?;
    conn.ensure_selected(&loc.raw_name).await?;
    conn.fetch_omitted_part(loc.uid, req.ordinal, req.section.as_deref(), req.encoding.as_deref()).await
}

async fn store_seen(
    shared: &Shared,
    conn: &mut Connection,
    changes: &[(i64, bool)],
    preferred: Option<i64>,
) -> Result<()> {
    for &(id, seen) in changes {
        // Every folder copy shares flags on Gmail; elsewhere update the copy we know about.
        let Some(loc) = shared.store.body_location(id, preferred)? else { continue };
        conn.ensure_selected(&loc.raw_name).await?;
        let op = if seen { "+FLAGS.SILENT (\\Seen)" } else { "-FLAGS.SILENT (\\Seen)" };
        let responses: Vec<_> =
            futures::TryStreamExt::try_collect(conn.session.uid_store(loc.uid.to_string(), op).await?).await?;
        drop(responses);
    }
    Ok(())
}

const BODY_BATCH: usize = 40;
const BODY_BATCH_BYTES: u64 = 8 * 1024 * 1024;
/// No data for this long means the connection stalled (Gmail throttles by going quiet
/// mid-response), so drop it and reconnect. Inactivity, not total time: a large batch on a
/// slow link keeps going as long as it's arriving.
const BODY_STALL: Duration = Duration::from_secs(60);
const MAX_BODY_FAILURES: u32 = 3;

async fn body_task(shared: Arc<Shared>) -> Result<()> {
    // Connected only while there is work; closed as soon as the queue is empty.
    let mut conn: Option<Connection> = None;
    loop {
        let focus = *shared.focus.borrow();
        let preferred = preferred_body_folder(&shared);
        let batch: Vec<i64> = {
            let skipped = shared.body_failures.lock().unwrap().values().filter(|&&n| n >= MAX_BODY_FAILURES).count();
            let candidates = shared.store.messages_missing_body(shared.account.id, focus, 400 + skipped as u32)?;
            let failures = shared.body_failures.lock().unwrap();
            let mut inflight = shared.inflight.lock().unwrap();
            let mut batch = Vec::new();
            let mut bytes = 0u64;
            let mut folder: Option<i64> = None;
            for id in candidates {
                if inflight.contains(&id) || failures.get(&id).is_some_and(|&n| n >= MAX_BODY_FAILURES) {
                    continue;
                }
                let Some(loc) = shared.store.body_location(id, preferred)? else { continue };
                // Keep a batch within one folder so it is a single UID FETCH.
                if *folder.get_or_insert(loc.folder_id) != loc.folder_id {
                    continue;
                }
                // Attachments are skipped, so a big message mostly costs its text part.
                let cost = (loc.size as u64).min(512 * 1024);
                if !batch.is_empty() && bytes + cost > BODY_BATCH_BYTES {
                    break;
                }
                bytes += cost;
                batch.push(id);
                // After a failure, go one message at a time to isolate the culprit.
                if batch.len() >= BODY_BATCH || failures.get(&id).is_some() {
                    break;
                }
            }
            inflight.extend(&batch);
            batch
        };

        if batch.is_empty() {
            if let Some(c) = conn.take() {
                c.logout().await;
            }
            tokio::select! {
                _ = shared.new_headers.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
            continue;
        }
        if conn.is_none() {
            match shared.connect().await {
                Ok(c) => conn = Some(c),
                Err(e) => {
                    shared.inflight.lock().unwrap().retain(|id| !batch.contains(id));
                    return Err(e);
                }
            }
        }
        let c = conn.as_mut().unwrap();

        // A message that already failed once is fetched whole (no BODYSTRUCTURE).
        let whole = {
            let failures = shared.body_failures.lock().unwrap();
            batch.iter().any(|id| failures.contains_key(id))
        };
        let started = std::time::Instant::now();
        let activity = c.activity.clone();
        let result = imap::stall_timeout(&activity, BODY_STALL, "downloading bodies", download(&shared, c, &batch, preferred, false, whole))
            .await
            .and_then(|r| r);
        {
            let mut inflight = shared.inflight.lock().unwrap();
            for id in &batch {
                inflight.remove(id);
            }
        }
        // Count a failure against every message that didn't make it.
        let stored = result.as_ref().map(|s| s.clone()).unwrap_or_default();
        let failed: Vec<i64> = batch.iter().copied().filter(|id| !stored.contains(id)).collect();
        if !failed.is_empty() {
            let mut failures = shared.body_failures.lock().unwrap();
            for id in &failed {
                let n = failures.entry(*id).or_insert(0);
                *n += 1;
                if *n == MAX_BODY_FAILURES {
                    tracing::warn!(id, "body download failed {MAX_BODY_FAILURES} times; skipping it this session");
                }
            }
        }
        match result {
            Ok(_) => {
                if !failed.is_empty() {
                    tracing::warn!(?failed, "server returned no usable body for some messages");
                }
                tracing::debug!(n = batch.len(), ms = started.elapsed().as_millis() as u64, "bodies downloaded");
            }
            Err(e) => {
                tracing::warn!(n = batch.len(), first = batch[0], "body batch failed: {e:#}");
                return Err(e);
            }
        }
    }
}

/// Look up a header strategy by name (for the benchmark CLI and settings).
pub fn strategy_by_name(name: &str, chunk: u32) -> Option<Arc<dyn HeaderSync>> {
    match name {
        "chunked" => Some(Arc::new(chunked::ChunkedNewestFirst::new(chunk))),
        _ => None,
    }
}

pub const STRATEGIES: &[&str] = &["chunked"];
