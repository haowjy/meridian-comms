//! Sources: where agents run. A source answers `list` (who could take a
//! message) and `wake` (send one of them a notice), and checks the sessions
//! it claimed (`session`). Its claim (`claim`, below) runs in the client, on
//! whatever machine that is, with no config. A source may also keep an inbox, where
//! its sessions look for messages nobody was woken for: `file`, `inbox_item`,
//! `offer`, `take` and the second step are optional, and their defaults say "not
//! supported". A source never starts a session, touches the surface, or keeps
//! state between calls.

pub mod meridian;

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::model::{
    Candidate, DecideRequest, Decision, FileRequest, Filed, IdentifyRequest, IdentifyResult, InboxItem, ListResponse, Machine, OfferResult,
    Session, WakeRequest, WakeResult,
};

/// Who the client was run for: the first compiled-in source that claims the
/// environment, else the owner at a terminal. A source claims a process by
/// its own session's variables, whatever this machine's config holds, so a
/// session never falls through to the owner. `fresh`: ask again what the
/// claim may have cached.
pub async fn claim(req: &IdentifyRequest, fresh: bool) -> Result<IdentifyResult> {
    match meridian::claim(req, fresh).await? {
        Some(id) => Ok(id),
        None => Ok(IdentifyResult::none()),
    }
}

/// The inbox item a `route` run from this environment was called for.
pub fn routed_item(req: &IdentifyRequest) -> Option<(&'static str, String)> {
    meridian::routed_item(req).map(|item| ("meridian", item))
}

/// What a source's refs start with.
pub struct RefStart {
    pub now: String,
    /// Every other start the source knows its refs may still carry.
    pub earlier: Vec<String>,
}

#[async_trait]
pub trait Source: Send + Sync {
    fn name(&self) -> &'static str;
    /// Every session this source reaches: one answer that each caller of the
    /// core filters for itself.
    async fn list(&self) -> Result<ListResponse>;
    async fn wake(&self, req: &WakeRequest) -> Result<WakeResult>;
    /// The session this source's claim reported, checked: `None` when the
    /// claim is another source's. An error says why it can't be named, as a
    /// `Failure`: a session no `wake` could reach must never be stored.
    async fn session(&self, caller: &IdentifyResult) -> Result<Option<Session>>;

    /// What people call the machine this source runs on. Known after `check`.
    fn machine(&self) -> Option<&str> {
        None
    }
    /// How people are shown one of this source's refs, with `machines` from
    /// the listing it came from: `None` for a ref that isn't this source's.
    fn show(&self, _ref_: &str, _machines: &[Machine]) -> Option<String> {
        None
    }
    /// Whether `word` could name a session on one of `missing`, the machines
    /// of this source that did not answer a listing: then a listing can't say
    /// the word names nothing.
    fn could_be_unchecked(&self, _word: &str, _missing: &[Machine]) -> bool {
        false
    }
    /// Run once when the daemon starts; an error stops it. For what the config
    /// says about this source that only the source's tool can confirm.
    async fn check(&mut self) -> Result<()> {
        Ok(())
    }
    /// After `check`: what this source's refs start with, now and before.
    /// The daemon rewrites the refs its threads hold from each earlier start,
    /// so a thread keeps the sessions it chose.
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
    /// Tells the candidate `to` that `item` is waiting and looks like its own.
    async fn offer(&self, _item: &str, _to: &Candidate) -> Result<OfferResult> {
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
}
