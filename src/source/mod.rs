//! Sources: where agents run. A source answers `list` (who could take a
//! message) and `wake` (send one of them a notice); one whose sessions run on
//! this machine also answers `identify`. A source may also keep an inbox, where
//! its sessions look for messages nobody was woken for: `file`, `inbox_item`,
//! `offer`, `take` and `decide` are optional, and their defaults say "not
//! supported". A source never starts a session, touches the surface, or keeps
//! state between calls.

pub mod meridian;

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::model::{
    DecideRequest, Decided, FileRequest, Filed, IdentifyRequest, IdentifyResult, InboxItem, ListRequest, ListResponse, OfferResult,
    WakeRequest, WakeResult,
};

#[async_trait]
pub trait Source: Send + Sync {
    fn name(&self) -> &'static str;
    async fn list(&self, req: &ListRequest) -> Result<ListResponse>;
    async fn wake(&self, req: &WakeRequest) -> Result<WakeResult>;
    fn identify(&self, req: &IdentifyRequest) -> IdentifyResult;

    /// Run once when the daemon starts; an error stops it. For what the config
    /// says about this source that only the source's tool can confirm.
    async fn check(&self) -> Result<()> {
        Ok(())
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
    /// session for its thread.
    async fn take(&self, _item: &str, _by: &str) -> Result<()> {
        bail!("{} has no inbox", self.name())
    }
    /// The second step of routing. `None`: this source has none.
    async fn decide(&self, _req: &DecideRequest) -> Result<Option<Decided>> {
        Ok(None)
    }
    /// The item a `route` run from this environment was called for.
    fn routed_item(&self, _req: &IdentifyRequest) -> Option<String> {
        None
    }
}
