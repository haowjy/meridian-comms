//! Surfaces: where people talk. A surface delivers messages and clicks, takes
//! posts, and keeps no state for the core. Text is the base form of everything
//! the core shows; buttons, menus and reactions are extras.

pub mod slack;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::model::{Interacted, Msg, PostRequest, Posted, SurfaceEvent, View, WhoAmI};

#[async_trait]
pub trait Surface: Send + Sync {
    /// Holds the connection open and reconnects until the process ends. Each
    /// event is acknowledged to the product before it is sent here. The
    /// surface never sends the bot's own posts, edits or deletions. An
    /// interaction is acknowledged only with its answer, so each goes on
    /// `interactions` with the channel the answer comes back on; one not
    /// answered in time gets the surface's own "try again".
    async fn run(&self, events: mpsc::Sender<SurfaceEvent>, interactions: mpsc::Sender<Interacted>);
    /// Posts as the bot, with the post's menu if the surface has menus. Never retried by the core.
    /// An error is a `Failure`: `MaybeSent` when the post may have gone out.
    async fn post(&self, req: PostRequest) -> Result<Posted>;
    /// Adds `note` under the bot's post `post`, keeping its text. Its menu and
    /// buttons go unless `keep_menu`: a choice was made with them, or (kept)
    /// the note only tells people something.
    async fn annotate(&self, conversation: &str, thread: &str, post: &str, note: &str, keep_menu: bool) -> Result<()>;
    /// Shows on message `id` that sessions were woken for it.
    async fn acknowledge(&self, conversation: &str, id: &str) -> Result<()>;
    /// Tells a paused thread that it is paused after `limit` messages, and how
    /// a person lets it go on.
    async fn notify_pause(&self, conversation: &str, thread: &str, limit: u32) -> Result<()>;
    /// How a post names person `user` (their ID on this surface) so the surface notifies them.
    fn mention(&self, user: &str) -> String;
    /// A permalink to message `id`, for a source that cannot run `read`.
    fn link(&self, conversation: &str, id: &str, thread: &str) -> String;
    /// The thread containing message `id`, oldest first; with `from`, only later messages.
    async fn thread(&self, conversation: &str, id: &str, from: Option<&str>) -> Result<Vec<Msg>>;
    /// A conversation's top-level messages after `from` (or the latest `limit`), oldest first.
    async fn recent(&self, conversation: &str, from: Option<&str>, limit: Option<usize>) -> Result<Vec<Msg>>;
    async fn whoami(&self, conversations: &[String]) -> Result<WhoAmI>;
    /// Replaces what person `viewer` (their ID on this surface) sees in their
    /// view of the agents with `view`.
    async fn show(&self, viewer: &str, view: &View) -> Result<()>;
}
