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
}

impl Default for ChunkedNewestFirst {
    fn default() -> Self {
        ChunkedNewestFirst { chunk: 5000 }
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

            // 3. Reconcile: cheap count check, full UID comparison only when counts disagree.
            //    Removes expunged messages and fetches any the backfill missed.
            if state.complete && store.local_uid_count(folder.id)? != exists {
                let server: std::collections::HashSet<u32> = conn.uid_search_all().await?.into_iter().collect();
                let local: std::collections::HashSet<u32> = store.local_uids(folder.id)?.into_iter().collect();
                let gone: Vec<u32> = local.difference(&server).copied().collect();
                if !gone.is_empty() {
                    store.remove_uids(folder.id, &gone)?;
                    (ctx.report)(Progress::Changed(vec![folder.id]));
                }
                let missing: Vec<u32> = server.difference(&local).copied().collect();
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
