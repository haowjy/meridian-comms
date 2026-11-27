//! What a client and the daemon say to each other: one JSON object each way,
//! one request per connection. The same objects can cross a network later.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::{Candidate, IdentifyResult};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Request {
    Read {
        identity: IdentifyResult,
        target: Target,
        limit: Option<usize>,
    },
    Send {
        identity: IdentifyResult,
        target: Target,
        to: Vec<String>,
        text: Option<String>,
    },
    List {
        identity: IdentifyResult,
        project: Option<String>,
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

#[derive(Debug, Serialize, Deserialize)]
pub struct Reply<T> {
    pub error: Option<String>,
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
