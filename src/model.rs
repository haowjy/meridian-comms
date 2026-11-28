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
    /// Whether `wake` from this daemon would reach it: the source decides, as
    /// it decides `default`.
    #[serde(default)]
    pub wakeable: bool,
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
    /// With `offerable`: each place something runs, candidates or not.
    #[serde(default)]
    pub groups: Vec<Group>,
    /// With `offerable`: every machine the source looked on, and whether it
    /// answered.
    #[serde(default)]
    pub machines: Vec<Machine>,
}

/// One `(machine, project, group)` with something running in it. A later
/// call that describes groups names them by these three.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub machine: String,
    pub project: String,
    /// What people call the project, when the source can tell.
    #[serde(default)]
    pub project_label: Option<String>,
    /// `None`: what runs in no group.
    #[serde(default)]
    pub group: Option<String>,
    /// How many things run there that are not candidates.
    #[serde(default)]
    pub others: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Machine {
    /// What no rename changes, when the source has one; `None` with `name`
    /// when the source could not tell which machines there are.
    #[serde(default)]
    pub id: Option<String>,
    /// What people call it.
    pub name: Option<String>,
    pub ok: bool,
    /// Why it did not answer, in the source's words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
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
    /// The source that claimed the caller, and that source's own ID for the
    /// session. The daemon makes the ref from them: the client names nothing.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
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
            source: None,
            session: None,
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

/// Something a person did that the surface must answer at once, with the
/// answer in hand, within seconds. Each comes with the channel its
/// `Response` goes back on.
#[derive(Debug, Clone)]
pub enum Interaction {
    /// A person asked, in `conversation`, for the form that opens a thread
    /// there for one session.
    Form { conversation: String, sender: Sender },
    /// A person clicked Message in the view, which is the owner's alone: the
    /// form, with the session chosen and the conversations it may use.
    ViewMessage { sender: Sender, to: FormFor },
    /// The form's session field asks what to offer for what was typed, for
    /// the form's first conversation.
    Suggest {
        conversation: String,
        sender: Sender,
        query: String,
    },
    /// The form was sent. `form` names the open form, the same each time it
    /// is sent again; `session` is the chosen ref.
    Submit {
        form: String,
        conversation: String,
        sender: Sender,
        session: Option<String>,
        text: String,
    },
}

pub type Interacted = (Interaction, tokio::sync::oneshot::Sender<Response>);

#[derive(Debug, Clone)]
pub enum Response {
    /// Nothing to show: the surface acknowledges and closes what is open.
    Done,
    /// A short answer only the person who asked sees. Never a post.
    Private(String),
    /// Open the form.
    Form(Form),
    /// The session field's options. With none, `note` says why, in the
    /// field's place.
    Options { groups: Vec<MenuGroup>, note: Option<String> },
    /// The form stays open, with `error` beside `field`.
    Invalid { field: FormField, error: String },
}

#[derive(Debug, Clone)]
pub struct Form {
    /// The option the session field starts on.
    pub session: Option<MenuOption>,
    /// Where sending opens the thread: one, or several to choose from.
    pub conversations: Vec<FormConversation>,
    /// One line above the fields: which machines the sessions are on.
    pub note: String,
}

#[derive(Debug, Clone)]
pub struct FormConversation {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormField {
    Conversation,
    Session,
    Text,
}

#[derive(Debug, Clone)]
pub enum SurfaceEvent {
    Message(Msg),
    Action(Action),
    /// A person opened their view of the agents (Slack's Home tab).
    ViewOpened(Sender),
    /// A person asked for their view to be listed again.
    ViewRefresh(Sender),
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
    /// The person (a display name) the bot posts this for: the surface shows
    /// it beside the text, and the text is theirs alone.
    pub by: Option<String>,
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

/// Why a post failed. Every kind but `MaybeSent` says nothing was posted,
/// so sending again is safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailKind {
    /// The request itself is wrong: a malformed ref, empty text.
    Invalid,
    NotFound,
    Ambiguous,
    /// No channel is configured to take it.
    NoChannel,
    NotAllowed,
    TooLarge,
    /// The client could not reach the daemon.
    Unreachable,
    NotSent,
    MaybeSent,
}

/// An error that carries its kind through `anyhow`. An error without one
/// is `NotSent`, so a step that runs after a post must say `MaybeSent`.
#[derive(Debug, thiserror::Error)]
#[error("{text}")]
pub struct Failure {
    pub kind: FailKind,
    pub text: String,
}

impl Failure {
    pub fn new(kind: FailKind, text: impl Into<String>) -> Self {
        Self { kind, text: text.into() }
    }

    pub fn kind_of(e: &anyhow::Error) -> FailKind {
        e.downcast_ref::<Failure>().map_or(FailKind::NotSent, |f| f.kind)
    }
}

/// What one person's view of the agents shows: a surface lays it out, fits
/// it to its own limits, and shows the text as it is, making no mention, link
/// or formatting from it.
#[derive(Debug, Clone)]
pub enum View {
    /// One line, for someone the view is not for.
    Note(String),
    Agents(Agents),
}

#[derive(Debug, Clone)]
pub struct Agents {
    /// The line under the title: when it was listed, and which machines answered.
    pub status: String,
    pub projects: Vec<ViewProject>,
    /// What the view says with no project to show.
    pub empty: String,
}

#[derive(Debug, Clone)]
pub struct ViewProject {
    pub label: String,
    pub groups: Vec<ViewGroup>,
}

#[derive(Debug, Clone)]
pub struct ViewGroup {
    pub label: String,
    pub agents: Vec<ViewAgent>,
    /// What else runs there, in a line: "5 other sessions running".
    pub others: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ViewAgent {
    pub line: String,
    /// What a Message button opens: the form, preset with this session and
    /// these conversations. `None`: no button.
    pub message: Option<FormFor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormFor {
    pub session: String,
    pub conversations: Vec<String>,
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
