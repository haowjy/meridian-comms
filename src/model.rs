//! The JSON shapes the core exchanges with sources and surfaces. They are the
//! contract in the design's adapter page: a module may later run out of process
//! or on another machine, and these structs are what crosses.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    #[serde(rename = "ref")]
    pub ref_: String,
    #[serde(default)]
    pub id: Option<String>,
    pub person: String,
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
