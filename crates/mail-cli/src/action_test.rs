//! Real-server check of the action executor (`Connection::run_op`) on Gmail, using one
//! throwaway message: archive (unlabel), back to Inbox, star, trash, delete forever.

use std::time::Instant;

use anyhow::{Result, anyhow, bail};
use mail_core::actions::ServerOp;
use mail_core::imap::{self, Connection, Credentials, ServerConfig};
use mail_core::model::FolderRole;

async fn find_uid(conn: &mut Connection, folder: &str, msgid: &str) -> Result<Option<u32>> {
    conn.select(folder).await?;
    let uids = conn.session.uid_search(format!("HEADER Message-ID \"{msgid}\"")).await?;
    Ok(uids.into_iter().max())
}

async fn labels_flags(conn: &mut Connection, folder: &str, uid: u32) -> Result<(Vec<String>, i64)> {
    conn.ensure_selected(folder).await?;
    let r = conn.fetch_flags(&uid.to_string(), None).await?;
    let (_, flags, labels) = r.into_iter().find(|x| x.0 == uid).ok_or_else(|| anyhow!("uid {uid} not found"))?;
    Ok((labels.unwrap_or_default(), flags))
}

fn check(ok: bool, what: &str, t: Instant) -> Result<()> {
    println!("{} {what} ({:?})", if ok { "ok  " } else { "FAIL" }, t.elapsed());
    if !ok {
        bail!("{what} failed");
    }
    Ok(())
}

pub async fn run(email: &str, host: &str) -> Result<()> {
    let password = keyring::Entry::new("dev.mail-client.imap", email)?.get_password()?;
    let cfg = ServerConfig { host: host.into(), port: 993, user: email.into(), credentials: Credentials::Password(password) };
    let mut conn = imap::connect(&cfg).await?;
    if !conn.caps.gmail {
        bail!("this test is for Gmail accounts");
    }
    let folders = conn.list_folders().await?;
    let role = |r| {
        folders
            .iter()
            .filter(|f| f.role == Some(r))
            .find(|f| f.raw_name.starts_with("[Gmail]/") || f.raw_name.starts_with("[Google Mail]/"))
            .map(|f| f.raw_name.clone())
            .ok_or_else(|| anyhow!("no {r:?} folder"))
    };
    let (all, trash) = (role(FolderRole::All)?, role(FolderRole::Trash)?);
    println!("caps: MOVE={} UIDPLUS={}", conn.caps.move_ext, conn.caps.uidplus);

    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis();
    let msgid = format!("email-app-action-test-{ts}@localhost");
    let raw = format!(
        "From: Email app test <{email}>\r\nTo: {email}\r\nSubject: [Email app test] safe to delete\r\nMessage-ID: <{msgid}>\r\nDate: {}\r\n\r\nTemporary message created by `mailsync action-test`. It deletes itself.\r\n",
        chrono::Utc::now().to_rfc2822()
    );
    conn.session.append("INBOX", Some("(\\Seen)"), None, raw.as_bytes()).await?;
    let result = steps(&mut conn, &all, &trash, &msgid).await;
    if result.is_err() {
        // Best effort: don't leave the test message behind.
        for folder in [&all, &trash, &"INBOX".to_string()] {
            if let Ok(Some(uid)) = find_uid(&mut conn, folder, &msgid).await {
                let op = if folder == &trash {
                    ServerOp::Expunge { folder: folder.clone(), uid }
                } else {
                    ServerOp::Move { folder: folder.clone(), uid, target: trash.clone() }
                };
                let _ = conn.run_op(&op).await;
            }
        }
        if let Ok(Some(uid)) = find_uid(&mut conn, &trash, &msgid).await {
            let _ = conn.run_op(&ServerOp::Expunge { folder: trash.clone(), uid }).await;
        }
        println!("cleaned up the test message");
    }
    conn.logout().await;
    result
}

async fn steps(conn: &mut Connection, all: &str, trash: &str, msgid: &str) -> Result<()> {
    let uid = find_uid(conn, all, msgid).await?.ok_or_else(|| anyhow!("test message not in All Mail"))?;
    let (labels, _) = labels_flags(conn, all, uid).await?;
    check(labels.iter().any(|l| l == "\\Inbox"), "appended test message is in the Inbox", Instant::now())?;

    let t = Instant::now();
    conn.run_op(&ServerOp::Labels { folder: all.into(), uid, add: vec![], remove: vec!["\\Inbox".into()] }).await?;
    let (labels, _) = labels_flags(conn, all, uid).await?;
    check(!labels.iter().any(|l| l == "\\Inbox"), "archive: \\Inbox label removed, still in All Mail", t)?;

    let t = Instant::now();
    conn.run_op(&ServerOp::Labels { folder: all.into(), uid, add: vec!["\\Inbox".into()], remove: vec![] }).await?;
    let (labels, _) = labels_flags(conn, all, uid).await?;
    check(labels.iter().any(|l| l == "\\Inbox"), "move back: \\Inbox label added", t)?;

    let t = Instant::now();
    conn.run_op(&ServerOp::Flags { folder: all.into(), uid, flag: "\\Flagged".into(), on: true }).await?;
    let (_, flags) = labels_flags(conn, all, uid).await?;
    check(flags & mail_core::model::flags::FLAGGED != 0, "star: \\Flagged set", t)?;

    let t = Instant::now();
    conn.run_op(&ServerOp::Move { folder: all.into(), uid, target: trash.into() }).await?;
    let in_trash = find_uid(conn, trash, msgid).await?;
    let in_all = find_uid(conn, all, msgid).await?;
    check(in_trash.is_some() && in_all.is_none(), "trash: moved to Trash, gone from All Mail", t)?;

    // Archive from Trash: back to All Mail, unfiled.
    let t = Instant::now();
    conn.run_op(&ServerOp::Move { folder: trash.into(), uid: in_trash.unwrap(), target: all.into() }).await?;
    let back = find_uid(conn, all, msgid).await?;
    let still_in_trash = find_uid(conn, trash, msgid).await?.is_some();
    let inbox_label = match back {
        Some(u) => labels_flags(conn, all, u).await?.0.iter().any(|l| l == "\\Inbox"),
        None => false,
    };
    check(back.is_some() && !still_in_trash && !inbox_label, "archive from Trash: back in All Mail, unfiled", t)?;

    conn.run_op(&ServerOp::Move { folder: all.into(), uid: back.unwrap(), target: trash.into() }).await?;
    let in_trash = find_uid(conn, trash, msgid).await?;

    let t = Instant::now();
    conn.run_op(&ServerOp::Expunge { folder: trash.into(), uid: in_trash.unwrap() }).await?;
    let gone = find_uid(conn, trash, msgid).await?.is_none();
    check(gone, "delete forever: removed from Trash", t)?;
    Ok(())
}
