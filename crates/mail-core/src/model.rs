//! Types shared with the UI. Serialized camelCase to match `app/src/api-contract.md`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Gmail,
    Outlook,
    Imap,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Gmail => "gmail",
            Provider::Outlook => "outlook",
            Provider::Imap => "imap",
        }
    }

    pub fn parse(s: &str) -> Provider {
        match s {
            "gmail" => Provider::Gmail,
            "outlook" => Provider::Outlook,
            _ => Provider::Imap,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub id: i64,
    pub email: String,
    pub display_name: String,
    pub provider: Provider,
    #[serde(skip)]
    pub imap_host: String,
    #[serde(skip)]
    pub imap_port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FolderRole {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Junk,
    Archive,
    All,
    Flagged,
    Important,
}

impl FolderRole {
    pub fn as_str(self) -> &'static str {
        match self {
            FolderRole::Inbox => "inbox",
            FolderRole::Sent => "sent",
            FolderRole::Drafts => "drafts",
            FolderRole::Trash => "trash",
            FolderRole::Junk => "junk",
            FolderRole::Archive => "archive",
            FolderRole::All => "all",
            FolderRole::Flagged => "flagged",
            FolderRole::Important => "important",
        }
    }

    pub fn parse(s: &str) -> Option<FolderRole> {
        Some(match s {
            "inbox" => FolderRole::Inbox,
            "sent" => FolderRole::Sent,
            "drafts" => FolderRole::Drafts,
            "trash" => FolderRole::Trash,
            "junk" => FolderRole::Junk,
            "archive" => FolderRole::Archive,
            "all" => FolderRole::All,
            "flagged" => FolderRole::Flagged,
            "important" => FolderRole::Important,
            _ => return None,
        })
    }

    pub fn friendly_name(self) -> &'static str {
        match self {
            FolderRole::Inbox => "Inbox",
            FolderRole::Sent => "Sent",
            FolderRole::Drafts => "Drafts",
            FolderRole::Trash => "Trash",
            FolderRole::Junk => "Junk",
            FolderRole::Archive => "Archive",
            FolderRole::All => "All Mail",
            FolderRole::Flagged => "Starred",
            FolderRole::Important => "Important",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    pub id: i64,
    pub account_id: i64,
    pub path: Vec<String>,
    pub display_name: String,
    pub role: Option<FolderRole>,
    pub selectable: bool,
    pub total: u32,
    pub unread: u32,
    pub synced_count: u32,
    /// Raw (modified UTF-7) mailbox name used in IMAP commands.
    #[serde(skip)]
    pub raw_name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageSummary {
    pub id: i64,
    pub folder_id: i64,
    pub subject: String,
    pub from_name: String,
    pub from_addr: String,
    pub date: i64,
    pub snippet: Option<String>,
    pub unread: bool,
    pub flagged: bool,
    pub has_attachments: bool,
    pub has_body: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePage {
    pub total: u32,
    pub messages: Vec<MessageSummary>,
}

/// Which messages of a conversation to show: those in the current folder always; with
/// `all_folders`, also any outside Trash/Spam; with `include_trash`, also those in Trash/Spam.
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadScope {
    pub all_folders: bool,
    pub include_trash: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConversationScope {
    Folder,
    All,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSummary {
    pub id: i64,
    pub folder_id: i64,
    pub subject: String,
    pub participants: Vec<String>,
    pub message_count: u32,
    pub unread: bool,
    pub flagged: bool,
    pub has_attachments: bool,
    pub date: i64,
    pub snippet: Option<String>,
    pub latest_message_id: i64,
}

/// A conversation as shown in the reader, plus how many of its messages the scope hid.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadView {
    pub messages: Vec<MessageSummary>,
    /// Hidden messages that are in other folders (outside Trash/Spam).
    pub hidden_other: u32,
    /// Hidden messages that are only in Trash/Spam.
    pub hidden_trash: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadPage {
    pub total: u32,
    pub threads: Vec<ThreadSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Addr {
    pub name: Option<String>,
    pub addr: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    /// Position in the message's attachment list: stored attachments first, then omitted ones.
    pub index: usize,
    pub name: String,
    pub mime: String,
    pub size: usize,
    /// Available locally (in the stored message or the attachment cache).
    pub downloaded: bool,
    /// False for executable/script types, which are only saved, never opened directly.
    pub openable: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageDetail {
    pub id: i64,
    pub subject: String,
    pub from: Addr,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    pub date: i64,
    pub html: Option<String>,
    pub text: Option<String>,
    pub attachments: Vec<Attachment>,
    pub has_remote_content: bool,
    /// The user chose "Load images" for this message before.
    pub remote_allowed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncPhase {
    Idle,
    Connecting,
    Folders,
    Headers,
    Bodies,
    Error,
    /// The server rejected the stored credentials; sync is paused until they're updated.
    Auth,
    /// The server refused a connection for a temporary reason (connection limit reached by
    /// other apps/devices, bandwidth); sync retries on its own.
    Limited,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    pub account_id: i64,
    pub phase: SyncPhase,
    pub folder_id: Option<i64>,
    pub done: u64,
    pub total: u64,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionResult {
    /// Pass to `undo_action`; None when nothing had to be sent.
    pub op_id: Option<i64>,
    /// True while the action can still be undone (moves wait a few seconds before sending).
    pub undoable: bool,
}

/// Events emitted by the core for the UI layer to forward.
#[derive(Debug, Clone)]
pub enum CoreEvent {
    FoldersChanged { account_id: i64 },
    MessagesChanged { folder_ids: Vec<i64> },
    MessageReady { id: i64 },
    /// A queued action was rejected by the server and has been rolled back locally.
    ActionFailed { message: String },
    Sync(SyncStatus),
}

/// Message flag bits stored in `messages.flags`.
pub mod flags {
    pub const SEEN: i64 = 1;
    pub const FLAGGED: i64 = 2;
    pub const ANSWERED: i64 = 4;
    pub const DRAFT: i64 = 8;
    pub const DELETED: i64 = 16;
}
