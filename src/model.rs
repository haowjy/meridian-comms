//! The JSON shapes the core exchanges with sources and surfaces. They are the
//! contract in the design's adapter page: a module may later run out of process
//! or on another machine, and these structs are what crosses.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    #[serde(rename = "ref")]
    pub ref_: String,
    #[serde(default)]
    pub id: Option<String>,
    pub person: String,
    /// The machine the session runs on, when the source knows it.
    #[serde(default)]
    pub machine: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub activity: Option<Activity>,
    /// The directory the session's project lives in on this machine, when
    /// the source knows it. For the source's own later calls about the
    /// session: it never leaves the daemon.
    #[serde(skip)]
    pub root: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivityState {
    Working,
    Idle,
    Waiting,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Activity {
    pub state: ActivityState,
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub warm_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub note: Option<String>,
}

impl Candidate {
    /// Idle with a cache deadline that has passed. Unknown activity is never cold.
    pub fn is_cold(&self, now: DateTime<Utc>) -> bool {
        matches!(&self.activity, Some(Activity { state: ActivityState::Idle, warm_until: Some(t), .. }) if *t < now)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListRequest {
    #[serde(default)]
    pub person: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub refs: Vec<String>,
    /// Every session an inbox item could be offered to: in any project,
    /// configured or not, on this machine and on each one the source can
    /// reach. What the router lists; `project` and `refs` are not read with
    /// it. A source need not be able to wake what it lists here.
    #[serde(default)]
    pub offerable: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListResponse {
    #[serde(default)]
    pub candidates: Vec<Candidate>,
    #[serde(default)]
    pub unreachable: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NoticeKind {
    Message,
    Released,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notice {
    pub kind: NoticeKind,
    pub message: String,
    pub from: String,
    pub sent: DateTime<Utc>,
    #[serde(default)]
    pub also: Vec<String>,
    #[serde(default)]
    pub link: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WakeRequest {
    #[serde(rename = "ref")]
    pub ref_: String,
    pub notice: Notice,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WakeResult {
    pub woken: bool,
    #[serde(rename = "ref")]
    pub ref_: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentifyRequest {
    pub env: BTreeMap<String, String>,
    pub cwd: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CallerKind {
    Session,
    Unlisted,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentifyResult {
    pub caller: CallerKind,
    #[serde(default, rename = "ref")]
    pub ref_: Option<String>,
    #[serde(default)]
    pub person: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub may_post: bool,
}

impl IdentifyResult {
    pub fn none() -> Self {
        Self {
            caller: CallerKind::None,
            ref_: None,
            person: None,
            project: None,
            may_post: false,
        }
    }
}

// ---- a source's inbox ----
//
// A source may keep a place where its sessions look for messages nobody was
// woken for. Every request here is optional: a source without one leaves the
// message waiting in the thread.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRequest {
    pub text: String,
    #[serde(default)]
    pub project: Option<String>,
    /// Where an answer goes: the thread, as an address.
    pub reply_to: String,
    /// The message ref.
    pub via_ref: String,
    /// The directory name of who wrote it.
    pub via_sender: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Filed {
    pub item: String,
    /// Where it waits, for people: "desk's inbox".
    pub place: String,
}

/// A filed message as the router reads it. `text`, `via_*` and `from` are the
/// sender's: they go to the model and nowhere else.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxItem {
    pub text: String,
    /// The ref of the session that filed it, if a session did and the source
    /// can still name it.
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub via_program: Option<String>,
    #[serde(default)]
    pub via_ref: Option<String>,
    #[serde(default)]
    pub via_sender: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    /// Nobody has taken it yet.
    pub waiting: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Offered {
    Yes,
    /// Nothing was sent: the item waits.
    No,
    /// The notice may have reached the session, and the source recorded no offer.
    Maybe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfferResult {
    pub offered: Offered,
    /// The source's own word for why not (`cold`, `taken`): never the item's text.
    pub detail: String,
}

/// The second step: the item, and the candidates the core kept after its own
/// filters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecideRequest {
    pub item: String,
    pub message: InboxItem,
    pub candidates: Vec<Candidate>,
    /// A question can be posted: the item was filed from a thread, and none
    /// was asked about it yet.
    pub may_ask: bool,
    /// What a person answered to the question asked before: untrusted, like
    /// the message's text.
    #[serde(default)]
    pub reply: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    /// A ref; the core offers only if it is one of the request's candidates.
    Offer(String),
    None,
    /// A question for whoever wrote the message, in the decider's own words:
    /// untrusted, since what it read may have steered it.
    Ask(String),
}

// ---- surface ----

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sender {
    pub id: String,
    pub app: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Msg {
    pub conversation: String,
    pub thread: String,
    pub id: String,
    pub sender: Sender,
    pub addressed: bool,
    pub text: String,
    pub time: DateTime<Utc>,
    /// Only set by `recent`: the replies under a top-level message.
    #[serde(default)]
    pub reply_count: Option<u32>,
    /// Only set by `recent`: the ID of the newest reply under a top-level message.
    #[serde(default)]
    pub latest_reply: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub conversation: String,
    pub thread: String,
    pub post: String,
    pub sender: Sender,
    pub name: String,
    #[serde(default)]
    pub values: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum SurfaceEvent {
    Message(Msg),
    Action(Action),
    /// The connection is up (first time or after a drop): the core recovers
    /// what was missed before it handles later events.
    Connected,
}

#[derive(Debug, Clone)]
pub struct PostRequest {
    pub conversation: String,
    pub thread: Option<String>,
    pub text: String,
    pub menu: Option<Menu>,
    /// The text holds words the daemon did not write: the surface shows it as
    /// it is, making no mention, link or formatting from it.
    pub plain: bool,
}

/// A menu of grouped options and one button that submits the selection, as
/// `choose`. A surface fits it to its own limits and drops what it can't show.
#[derive(Debug, Clone)]
pub struct Menu {
    pub groups: Vec<MenuGroup>,
    pub button: String,
}

#[derive(Debug, Clone)]
pub struct MenuGroup {
    pub label: String,
    pub options: Vec<MenuOption>,
}

#[derive(Debug, Clone)]
pub struct MenuOption {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct Posted {
    pub id: String,
}

#[derive(Debug, Clone)]
pub struct WhoAmI {
    pub bot: String,
    /// Per configured conversation: whether people outside the owner's
    /// organization are in it.
    pub external: BTreeMap<String, bool>,
}
