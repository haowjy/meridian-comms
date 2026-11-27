//! Sources: where agents run. A source answers `list` (who could take a
//! message) and `wake` (send one of them a notice); one whose sessions run on
//! this machine also answers `identify`. A source may also keep an inbox, where
//! its sessions look for messages nobody was woken for: `file`, `inbox_item`,
//! `offer`, `take` and the second step are optional, and their defaults say "not
//! supported". A source never starts a session, touches the surface, or keeps
//! state between calls.

pub mod meridian;

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::model::{
    DecideRequest, Decision, FileRequest, Filed, IdentifyRequest, IdentifyResult, InboxItem, ListRequest, ListResponse, OfferResult,
    WakeRequest, WakeResult,
};

/// What a source's refs start with.
pub struct RefStart {
    pub now: String,
    /// Every other start the source knows its refs may still carry.
    pub earlier: Vec<String>,
}

#[async_trait]
pub trait Source: Send + Sync {
    fn name(&self) -> &'static str;
    async fn list(&self, req: &ListRequest) -> Result<ListResponse>;
    async fn wake(&self, req: &WakeRequest) -> Result<WakeResult>;
    /// Run in the client, on the caller's environment: it starts no process.
    fn identify(&self, req: &IdentifyRequest) -> IdentifyResult;
    /// Run in the daemon: the ref of the session `identify` reported, or
    /// `None` when the two are not a session this source would list.
    fn session_ref(&self, project: &str, session: &str) -> Option<String>;

    /// Whether `word` is the ref of a session this source can list and never
    /// wakes, because it runs on another machine.
    async fn elsewhere(&self, _word: &str) -> bool {
        false
    }
    /// Run once when the daemon starts; an error stops it. For what the config
    /// says about this source that only the source's tool can confirm.
    async fn check(&mut self) -> Result<()> {
        Ok(())
    }
    /// After `check`: what this source's refs start with. The daemon records
    /// it, and when it changes rewrites the refs its threads hold, so a thread
    /// keeps the sessions it chose.
    fn ref_start(&self) -> Option<RefStart> {
        None
    }
    /// Puts a message where this source's sessions look. `None`: no inbox.
    async fn file(&self, _req: &FileRequest) -> Result<Option<Filed>> {
        Ok(None)
    }
    /// Reads a filed item. `None`: no such item (or no inbox).
    async fn inbox_item(&self, _item: &str) -> Result<Option<InboxItem>> {
        Ok(None)
    }
    /// Tells the candidate `ref_` that `item` is waiting and looks like its own.
    async fn offer(&self, _item: &str, _ref_: &str) -> Result<OfferResult> {
        bail!("{} has no inbox", self.name())
    }
    /// Claims `item` for the daemon because `by` (a directory name) chose a
    /// session for its thread. An item someone already took is no error.
    async fn take(&self, _item: &str, _by: &str) -> Result<()> {
        bail!("{} has no inbox", self.name())
    }
    /// Starts the second step of routing and names the run, for the log.
    /// `None`: this source has none.
    async fn start_decision(&self, _req: &DecideRequest) -> Result<Option<String>> {
        Ok(None)
    }
    /// Waits for the run `start_decision` named and gives what it decided. The
    /// run bounds itself; an error is a run that came to no decision.
    async fn decision(&self, _req: &DecideRequest, _run: &str) -> Result<Decision> {
        bail!("{} has no second step", self.name())
    }
    /// The item a `route` run from this environment was called for.
    fn routed_item(&self, _req: &IdentifyRequest) -> Option<String> {
        None
    }
}
