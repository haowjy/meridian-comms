//! What a client and the daemon say to each other: one JSON object each way,
//! one request per connection. The same objects can cross a network later.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::{Candidate, FailKind, IdentifyResult};

/// The longest request line the daemon reads.
pub const MAX_REQUEST: usize = 4 << 20;

/// `caller` was `identity` while the client composed its own ref. The name
/// changed with the shape, so a daemon of either build refuses the other's
/// request instead of reading a session as the owner at a terminal. Rename
/// it again whenever a build must not read the other's request.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Request {
    Read {
        caller: IdentifyResult,
        target: Target,
        limit: Option<usize>,
    },
    Send {
        caller: IdentifyResult,
        target: Target,
        to: Vec<String>,
        text: Option<String>,
    },
    List {
        caller: IdentifyResult,
        project: Option<String>,
    },
    /// Post `text` for a person or in a thread, waking nobody.
    Post {
        caller: IdentifyResult,
        to: PostTo,
        text: String,
    },
    /// Route one item waiting in a source's inbox. Answered when accepted, not
    /// when routed. No source named: the first one.
    Route {
        source: Option<String>,
        item: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    Message(String),
    Channel(String),
}

/// Who a `Post` is for.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PostTo {
    /// A name in the directory: a new thread in the caller's channel names them.
    Person(String),
    /// A message ref: the post goes in that message's thread.
    Thread(String),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Reply<T> {
    pub error: Option<String>,
    /// The error's kind; a daemon older than `post` sends none.
    #[serde(default)]
    pub kind: Option<FailKind>,
    pub result: Option<T>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MessageOut {
    pub message: String,
    pub from: String,
    /// A name from the directory; null for the bot and for people outside it.
    pub person: Option<String>,
    pub bot: bool,
    /// Not in the directory: shown without its text.
    pub outsider: bool,
    pub text: Option<String>,
    pub time: DateTime<Utc>,
    pub reply_count: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReadOut {
    pub channel: String,
    /// The thread's first message, when reading a thread.
    pub thread: Option<String>,
    /// The message the caller asked for, when reading a thread.
    pub requested: Option<String>,
    pub messages: Vec<MessageOut>,
    /// How many earlier messages were left out by the limit.
    pub omitted: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SendOut {
    /// The new post, or the message the wake was about.
    pub message: String,
    pub link: Option<String>,
    pub posted: bool,
    pub woken: Vec<String>,
    /// One line per recipient that was not woken, with the reason.
    pub not_woken: Vec<String>,
    /// A command that retries the wake, when one failed.
    pub retry: Option<String>,
    pub paused: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RouteOut {
    /// False when the item was routed before or is being routed now.
    pub accepted: bool,
    pub detail: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PersonOut {
    pub name: String,
    pub note: Option<String>,
    pub candidates: Vec<Candidate>,
    pub unreachable: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WaitingOut {
    pub message: String,
    pub person: String,
    pub time: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ListOut {
    pub project: Option<String>,
    pub people: Vec<PersonOut>,
    pub waiting: Vec<WaitingOut>,
}
