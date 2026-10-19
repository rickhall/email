# Mail

A fast, local-first desktop email client: a Rust core (IMAP sync, SQLite cache, MIME) behind a Tauri v2 + SolidJS UI.

```
crates/mail-core   sync engine, storage, MIME parsing/sanitizing — no UI dependencies
crates/mail-cli    `mailsync`: headless benchmark driver for sync strategies
app/               SolidJS UI (app/src) + Tauri shell (app/src-tauri)
```

## Run

```sh
cd app
npm install
npm run tauri dev        # desktop app
npm run dev              # UI only, in a browser, against an in-memory mock backend
```

Gmail currently signs in with an [app password](https://myaccount.google.com/apppasswords) (requires 2-Step Verification). Google Workspace and Microsoft 365 custom domains are detected from the domain's MX records. Passwords are kept in the system keychain (Secret Service on Linux, Keychain on macOS); if the server rejects one, the account's sync pauses and the sidebar asks for a new one.

The browser mock takes URL flags: `?empty` (onboarding), `?auth` (rejected password), `?error` (sync error), `?many` (six accounts); they can be combined.

Data lives in `~/.local/share/dev.mailclient.desktop/` on Linux (`~/Library/Application Support/dev.mailclient.desktop/` on macOS): `mail.db` (accounts, folders, headers, threads, search index) and `bodies.db` (raw messages). Window size is in `~/.config/dev.mailclient.desktop/window.json`.

## Features

- **Conversations:** grouped by Message-ID / In-Reply-To / References (no subject matching), across folders. By default a conversation shows only the current folder's messages; the View menu in the message list switches the app-wide default to *All mail* and/or *Include Trash*, and each conversation offers a temporary "Show N more messages in other folders / in Trash".
- **Quick search** (`/` or Ctrl+F): SQLite FTS5 over sender, recipients and subject, prefix-matched as you type. Syntax: `"phrase"`, `from:`, `to:`, `subject:`, `-exclude`. Scope is the whole account or the current folder; Sender / Recipients / Subject chips limit plain words.
- **Reader:** sanitized HTML in a sandboxed iframe; remote images blocked until "Load images", which is remembered per message. Opening a message marks it read. Quoted replies get a bar per level (HTML `blockquote` and plain-text `>` lines alike). Trailing quoted history collapses behind "Show quoted text" only when it's safe: exactly one quote, nothing written after it, text before it, and no Outlook-style history in the message itself — inline and bottom-posted replies never collapse (`app/src/lib/quotes.ts`; `node app/scripts/quotes-selfcheck.mjs` checks the plain-text rules).
- **Sync status:** one fixed summary row at the bottom of the sidebar, with per-account details in a popover. Nothing in the folder tree or headers moves because of background activity.
- **Window:** size (and position, where the platform allows) is restored at launch, in logical units so GNOME fractional scaling works.

## Benchmark sync

```sh
MAIL_PASSWORD='xxxx xxxx xxxx xxxx' cargo run --release -p mail-cli --bin mailsync -- bench you@gmail.com
#   --chunk 5000          headers per chunk
#   --bodies-secs 30      also measure body download rate
#   --strategy chunked    see `mailsync strategies`
```

`mailsync render --data-dir <app data dir> --id N --out file.html` writes a stored message's sanitized HTML (what the reader renders), for diagnosing display issues.

To diagnose a single message that won't download, `mailsync probe you@example.com --uid N [--folder NAME]` prints the server's raw response sizes, timings and BODYSTRUCTURE (never bodies or credentials), using the password stored in the keychain.

## How sync works

- **Folders first.** `LIST` on connect gives the full folder tree in one round trip; a separate connection fills in counts with `STATUS`.
- **Headers in chunks, newest to oldest** (default 5000), addressed by sequence number. The cursor (lowest UID stored) is persisted, so a restart resumes. The focused folder always goes first, and switching folders takes effect after the current chunk.
- **Gmail:** Inbox, `[Gmail]/All Mail`, Trash and Spam are synced directly. Every other label's contents come from `X-GM-LABELS`, and messages are de-duplicated by `X-GM-MSGID`, so each message is stored and downloaded once however many labels it has.
- **Push:** IMAP IDLE on the Inbox and on the folder on screen. A push re-syncs that folder immediately, and bodies of new mail are fetched right away. Every folder is also polled every 60 s.
- **Incremental:** new mail above the highest UID; flag/label changes via CONDSTORE `CHANGEDSINCE`; reconciliation by count mismatch, then `UID SEARCH ALL` (removes expunged messages and fetches missed ones).
- **Bodies (attachments omitted for now):** `BODYSTRUCTURE` decides what to fetch. Messages without attachments come down whole. Others fetch only text parts and inline images under 2 MB, and are rebuilt into a valid MIME message tagged `X-Mail-Partial`. Omitted attachments are recorded (`messages.omitted`) and still listed in the reader. Stored in `bodies.db`, newest first, preferring the folder on screen, over 3 background connections. A message you open is fetched immediately on a dedicated, pre-connected connection.
- **Robustness:** a batch with no progress for 60 s is abandoned and its connection replaced (Gmail stalls connections during bulk downloads). A message that fails is retried alone and fetched whole without `BODYSTRUCTURE` (some malformed structures make the IMAP parser wait forever); after 3 failures it's skipped for the session.
- **Resumable:** the header cursor and every body are committed as they arrive, so quitting mid-sync loses at most one chunk.
- **Strategies are pluggable:** implement `mail_core::sync::HeaderSync` and register it in `strategy_by_name` to compare approaches with `mailsync bench`.

## HTML email

Bodies are sanitized in Rust (ammonia; scripts, handlers and forms removed; tables, inline styles and `<style>` kept). `cid:` images are inlined. The reader renders the result in a sandboxed iframe with no script execution and a CSP that blocks remote content until you click "Load images". Links open in the system browser.
