//! Default header strategy: fetch headers in fixed-size chunks, newest to oldest.
//!
//! Chunks are addressed by *sequence number* (dense, so each chunk is exactly N messages)
//! and the backfill cursor is persisted as the lowest UID stored, so a restart resumes where
//! it left off. Per pass, the first step on a folder also picks up new mail above the highest
//! stored UID, flag/label changes (CONDSTORE `CHANGEDSINCE` when supported) and expunges.

use anyhow::Result;
use futures::future::BoxFuture;

use super::{FolderCtx, HeaderSync, Progress, Step};
use crate::imap::Connection;

pub struct ChunkedNewestFirst {
    pub chunk: u32,
    /// When each folder last had a full UID comparison (this session).
    verified: std::sync::Mutex<std::collections::HashMap<i64, std::time::Instant>>,
}

/// How often a folder gets a full UID comparison, which also catches errors that cancel out
/// in counts (a stale message plus a missing one) that `reconcile_walk` can't see.
const FULL_VERIFY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

impl ChunkedNewestFirst {
    pub fn new(chunk: u32) -> Self {
        ChunkedNewestFirst { chunk, verified: Default::default() }
    }

    fn full_verify_due(&self, folder_id: i64) -> bool {
        self.verified
            .lock()
            .unwrap()
            .get(&folder_id)
            .is_none_or(|t| t.elapsed() >= FULL_VERIFY_INTERVAL)
    }
}

impl Default for ChunkedNewestFirst {
    fn default() -> Self {
        ChunkedNewestFirst::new(5000)
    }
}

impl HeaderSync for ChunkedNewestFirst {
    fn name(&self) -> &'static str {
        "chunked"
    }

    fn step<'a>(&'a self, conn: &'a mut Connection, ctx: &'a FolderCtx<'a>) -> BoxFuture<'a, Result<Step>> {
        Box::pin(self.run(conn, ctx))
    }
}

impl ChunkedNewestFirst {
    async fn run(&self, conn: &mut Connection, ctx: &FolderCtx<'_>) -> Result<Step> {
        let store = ctx.store;
        let folder = ctx.folder;
        let mut state = store.folder_sync_state(folder.id)?;

        let mb = conn.select(&folder.raw_name).await?;
        let uidvalidity = mb.uid_validity.unwrap_or(0);
        if state.uidvalidity != 0 && state.uidvalidity != uidvalidity {
            tracing::info!(folder = folder.raw_name, "UIDVALIDITY changed; resyncing folder");
            store.reset_folder(folder.id)?;
            state = Default::default();
        }
        state.uidvalidity = uidvalidity;
        let exists = mb.exists;

        if exists == 0 {
            let stale = store.local_uids(folder.id)?;
            if !stale.is_empty() {
                store.remove_uids(folder.id, &stale)?;
                (ctx.report)(Progress::Changed(vec![folder.id]));
            }
            state.complete = true;
            state.highest_modseq = mb.highest_modseq.unwrap_or(0);
            store.set_folder_sync_state(folder.id, &state)?;
            return Ok(Step::Done);
        }

        if ctx.first_in_pass && state.top_uid > 0 {
            // 1. New mail above what we have.
            let uid_next = mb.uid_next.unwrap_or(u32::MAX);
            if uid_next > state.top_uid + 1 {
                let mut recs = conn.fetch_headers_uid(&format!("{}:*", state.top_uid + 1)).await?;
                // `N:*` always returns the last message even when its UID < N.
                recs.retain(|r| r.uid > state.top_uid);
                if !recs.is_empty() {
                    state.top_uid = recs.iter().map(|r| r.uid).max().unwrap_or(state.top_uid);
                    let changed = store.upsert_headers(ctx.account.id, folder.id, &recs, ctx.label_folders)?;
                    (ctx.report)(Progress::Changed(changed));
                    (ctx.report)(Progress::NewMail { folder_id: folder.id, uids: recs.iter().map(|r| r.uid).collect() });
                }
            }

            // 2. Flag (and Gmail label) changes on messages we already have.
            let server_modseq = mb.highest_modseq.unwrap_or(0);
            let changes = if conn.caps.condstore && state.highest_modseq > 0 {
                if server_modseq > state.highest_modseq {
                    conn.fetch_flags(&format!("1:{}", state.top_uid), Some(state.highest_modseq)).await?
                } else {
                    vec![]
                }
            } else {
                conn.fetch_flags(&format!("1:{}", state.top_uid), None).await?
            };
            if !changes.is_empty() {
                let changed = store.update_flags(folder.id, &changes, ctx.label_folders)?;
                (ctx.report)(Progress::Changed(changed));
            }
            if server_modseq > 0 {
                state.highest_modseq = server_modseq;
            }

            // 3. Reconcile: removes expunged messages and fetches any the backfill missed. A full
            //    UID comparison once a day per folder; in between, only when counts disagree,
            //    and then by walking newest-first until our view and the server's line up
            //    (see `reconcile_walk`).
            let full = state.complete && self.full_verify_due(folder.id);
            if full || (state.complete && store.local_uid_count(folder.id)? != exists) {
                let local = store.local_uids(folder.id)?;
                let (gone, missing) = if full {
                    let server: std::collections::HashSet<u32> = conn.uid_search_all().await?.into_iter().collect();
                    let mine: std::collections::HashSet<u32> = local.iter().copied().collect();
                    self.verified.lock().unwrap().insert(folder.id, std::time::Instant::now());
                    (mine.difference(&server).copied().collect(), server.difference(&mine).copied().collect())
                } else {
                    match reconcile_walk(conn, &local, exists, self.chunk, MAX_WALK_STEPS).await? {
                        Some(diff) => diff,
                        None => {
                            let server: std::collections::HashSet<u32> =
                                conn.uid_search_all().await?.into_iter().collect();
                            let mine: std::collections::HashSet<u32> = local.iter().copied().collect();
                            (mine.difference(&server).copied().collect(), server.difference(&mine).copied().collect())
                        }
                    }
                };
                let missing: Vec<u32> = missing;
                if !gone.is_empty() {
                    store.remove_uids(folder.id, &gone)?;
                    (ctx.report)(Progress::Changed(vec![folder.id]));
                }
                for part in missing.chunks(self.chunk as usize) {
                    let recs = conn.fetch_headers_uid(&crate::imap::uid_set(part)).await?;
                    if !recs.is_empty() {
                        let changed = store.upsert_headers(ctx.account.id, folder.id, &recs, ctx.label_folders)?;
                        (ctx.report)(Progress::Changed(changed));
                    }
                }
            }
        } else if state.highest_modseq == 0 {
            state.highest_modseq = mb.highest_modseq.unwrap_or(0);
        }

        // 4. Backfill one chunk, newest to oldest.
        if !state.complete {
            // Sequence number just above the next chunk.
            let upper = if state.low_uid == 0 {
                exists + 1
            } else {
                match conn.seq_of_uid(state.low_uid).await? {
                    Some(seq) => seq,
                    // Our cursor message was expunged: count what's below it instead.
                    None => {
                        let below = conn
                            .session
                            .uid_search(format!("UID 1:{}", state.low_uid.saturating_sub(1).max(1)))
                            .await?;
                        below.iter().filter(|&&u| u < state.low_uid).count() as u32 + 1
                    }
                }
            };
            if upper <= 1 {
                state.complete = true;
            } else {
                let start = upper.saturating_sub(self.chunk).max(1);
                let recs = conn.fetch_headers_seq(&format!("{start}:{}", upper - 1)).await?;
                if let Some(min) = recs.iter().map(|r| r.uid).min() {
                    state.low_uid = if state.low_uid == 0 { min } else { state.low_uid.min(min) };
                    state.top_uid = state.top_uid.max(recs.iter().map(|r| r.uid).max().unwrap_or(0));
                    let changed = store.upsert_headers(ctx.account.id, folder.id, &recs, ctx.label_folders)?;
                    (ctx.report)(Progress::Changed(changed));
                }
                if start == 1 {
                    state.complete = true;
                }
            }
            (ctx.report)(Progress::Headers {
                folder_id: folder.id,
                done: store.local_uid_count(folder.id)? as u64,
                total: exists as u64,
            });
        }

        store.set_folder_sync_state(folder.id, &state)?;
        Ok(if state.complete { Step::Done } else { Step::More })
    }
}

/// Something that can list (sequence number, UID) pairs for a range of the selected folder.
pub trait SeqUids {
    fn seq_uids<'a>(&'a mut self, lo: u32, hi: u32) -> BoxFuture<'a, Result<Vec<(u32, u32)>>>;
}

impl SeqUids for Connection {
    fn seq_uids<'a>(&'a mut self, lo: u32, hi: u32) -> BoxFuture<'a, Result<Vec<(u32, u32)>>> {
        Box::pin(self.fetch_seq_uids(lo, hi))
    }
}

/// Find expunged (`gone`) and missing UIDs without downloading the folder's whole UID list
/// (after the emailer prototype's "short circuit"). UIDs only grow, so a message's sequence
/// number is its rank among the folder's UIDs: if, at the bottom of a chunk, the server's
/// sequence number for a UID equals the number of UIDs we hold up to it, nothing below differs
/// and the walk stops. Changes are usually recent, so this is typically one small FETCH
/// instead of ~800 KB of UIDs for a 100k-message folder. Each step is twice the previous one,
/// and the walk covers the newest `chunk * (2^max_steps - 1)` messages. If it hasn't lined up
/// by then the change is deep in the folder, where one full UID comparison is cheaper (Gmail
/// adds ~300 ms per SEARCH), so it returns `None`. Gmail has no QRESYNC, which would report
/// expunges directly.
pub async fn reconcile_walk(
    src: &mut impl SeqUids,
    local: &[u32],
    exists: u32,
    chunk: u32,
    max_steps: u32,
) -> Result<Option<(Vec<u32>, Vec<u32>)>> {
    let mut have: std::collections::BTreeSet<u32> = local.iter().copied().collect();
    let mut gone = Vec::new();
    let mut missing = Vec::new();
    let mut hi = exists;
    // Exclusive UID ceiling of the region still to compare (None: everything above).
    let mut ceil: Option<u32> = None;
    let mut step = chunk.max(1);
    let mut steps = 0;
    while hi >= 1 {
        if steps == max_steps {
            return Ok(None);
        }
        steps += 1;
        let lo = hi.saturating_sub(step - 1).max(1);
        step = step.saturating_mul(2);
        let server = src.seq_uids(lo, hi).await?;
        let Some(&(lo_seq, floor)) = server.first() else { break };
        let theirs: std::collections::HashSet<u32> = server.iter().map(|&(_, u)| u).collect();
        let region: Vec<u32> = match ceil {
            Some(c) => have.range(floor..c).copied().collect(),
            None => have.range(floor..).copied().collect(),
        };
        for u in region {
            if !theirs.contains(&u) {
                have.remove(&u);
                gone.push(u);
            }
        }
        for &(_, u) in &server {
            if have.insert(u) {
                missing.push(u);
            }
        }
        if lo_seq <= 1 {
            // Bottom of the folder: anything we hold below its first message is gone.
            let below: Vec<u32> = have.range(..floor).copied().collect();
            for u in below {
                have.remove(&u);
                gone.push(u);
            }
            break;
        }
        // Rank of `floor` in our view (it's present now) vs the server's sequence number.
        if have.range(..=floor).count() as u32 == lo_seq {
            break;
        }
        ceil = Some(floor);
        hi = lo_seq - 1;
    }
    Ok(Some((gone, missing)))
}

/// Steps the expunge walk may take before falling back to a full UID comparison.
pub const MAX_WALK_STEPS: u32 = 2;

#[cfg(test)]
mod tests {
    use super::*;

    /// A folder as the server sees it: ascending UIDs, sequence number = position + 1.
    struct Fake {
        uids: Vec<u32>,
        fetched: u32,
    }

    impl SeqUids for Fake {
        fn seq_uids<'a>(&'a mut self, lo: u32, hi: u32) -> BoxFuture<'a, Result<Vec<(u32, u32)>>> {
            let out: Vec<(u32, u32)> = (lo..=hi.min(self.uids.len() as u32))
                .map(|s| (s, self.uids[s as usize - 1]))
                .collect();
            self.fetched += out.len() as u32;
            Box::pin(async move { Ok(out) })
        }
    }

    fn walk(server: Vec<u32>, local: Vec<u32>, chunk: u32) -> (Vec<u32>, Vec<u32>, u32) {
        walk_steps(server, local, chunk, u32::MAX).expect("walk finished")
    }

    fn walk_steps(server: Vec<u32>, local: Vec<u32>, chunk: u32, max: u32) -> Option<(Vec<u32>, Vec<u32>, u32)> {
        let exists = server.len() as u32;
        let mut f = Fake { uids: server, fetched: 0 };
        let (mut g, mut m) =
            futures::executor::block_on(reconcile_walk(&mut f, &local, exists, chunk, max)).unwrap()?;
        g.sort();
        m.sort();
        Some((g, m, f.fetched))
    }

    #[test]
    fn gives_up_when_the_change_is_deep() {
        let local: Vec<u32> = (1..=10_000).collect();
        let server: Vec<u32> = local.iter().copied().filter(|&u| u != 2_000).collect();
        // Two steps of 1k + 2k don't reach rank 2,000: fall back to a full comparison.
        assert!(walk_steps(server.clone(), local.clone(), 1_000, 2).is_none());
        assert_eq!(walk_steps(server, local, 1_000, 4).map(|r| r.0), Some(vec![2_000]));
    }

    #[test]
    fn recent_expunge_costs_one_chunk() {
        let local: Vec<u32> = (1..=10_000).collect();
        let server: Vec<u32> = local.iter().copied().filter(|&u| u != 9_990).collect();
        let (gone, missing, fetched) = walk(server, local, 100);
        assert_eq!((gone, missing), (vec![9_990], vec![]));
        assert_eq!(fetched, 100);
    }

    #[test]
    fn old_expunges_walk_down_and_stop_when_aligned() {
        let local: Vec<u32> = (1..=10_000).collect();
        let server: Vec<u32> = local.iter().copied().filter(|&u| u != 5_000 && u != 5_001).collect();
        let (gone, missing, fetched) = walk(server, local, 1_000);
        assert_eq!((gone, missing), (vec![5_000, 5_001], vec![]));
        // Steps of 1k, 2k, 4k reach rank 5,000 and stop; the bottom half is never fetched.
        assert!(fetched <= 7_000, "should stop once aligned, fetched {fetched}");
    }

    #[test]
    fn finds_new_and_gone_down_to_the_bottom() {
        let local: Vec<u32> = vec![1, 2, 3, 5, 6, 8, 9];
        let server: Vec<u32> = vec![2, 3, 5, 6, 9, 11];
        let (gone, missing, _) = walk(server, local, 3);
        assert_eq!(gone, vec![1, 8]);
        assert_eq!(missing, vec![11]);
    }

    /// Known limit (shared with counting in general): a stale UID and a missing one below the
    /// alignment point cancel out. The daily full comparison exists for this.
    #[test]
    fn compensating_errors_below_alignment_are_invisible() {
        let local: Vec<u32> = vec![1, 2, 3, 5, 6, 8, 9];
        let server: Vec<u32> = vec![2, 3, 4, 5, 6, 9, 11];
        let (gone, missing, _) = walk(server, local, 3);
        assert_eq!((gone, missing), (vec![8], vec![11]));
    }

    #[test]
    fn everything_gone_but_one() {
        let (gone, missing, _) = walk(vec![50], (1..=60).collect(), 10);
        assert_eq!(gone.len(), 59);
        assert!(missing.is_empty());
    }
}
