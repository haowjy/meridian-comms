//! Surfaces: where people talk. A surface delivers messages and clicks, takes
//! posts, and keeps no state for the core. Text is the base form of everything
//! the core shows; buttons, menus and reactions are extras.

pub mod slack;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::model::{Msg, PostRequest, Posted, SurfaceEvent, WhoAmI};

#[async_trait]
pub trait Surface: Send + Sync {
    /// Holds the connection open and reconnects until the process ends. Each
    /// event is acknowledged to the product before it is sent here. The
    /// surface never sends the bot's own posts, edits or deletions.
    async fn run(&self, events: mpsc::Sender<SurfaceEvent>);
    /// Posts as the bot. Never retried by the core.
    async fn post(&self, req: PostRequest) -> Result<Posted>;
    async fn react(&self, conversation: &str, id: &str, reaction: &str) -> Result<()>;
    /// The thread containing message `id`, oldest first; with `from`, only later messages.
    async fn thread(&self, conversation: &str, id: &str, from: Option<&str>) -> Result<Vec<Msg>>;
    /// A conversation's top-level messages after `from` (or the latest `limit`), oldest first.
    async fn recent(&self, conversation: &str, from: Option<&str>, limit: Option<usize>) -> Result<Vec<Msg>>;
    async fn whoami(&self, conversations: &[String]) -> Result<WhoAmI>;
}
