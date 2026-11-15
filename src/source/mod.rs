//! Sources: where agents run. A source answers `list` (who could take a
//! message) and `wake` (send one of them a notice); one whose sessions run on
//! this machine also answers `identify`. A source never starts a session,
//! touches the surface, decides who gets a message, or keeps state between calls.

pub mod meridian;

use anyhow::Result;
use async_trait::async_trait;

use crate::model::{IdentifyRequest, IdentifyResult, ListRequest, ListResponse, WakeRequest, WakeResult};

#[async_trait]
pub trait Source: Send + Sync {
    fn name(&self) -> &'static str;
    async fn list(&self, req: &ListRequest) -> Result<ListResponse>;
    async fn wake(&self, req: &WakeRequest) -> Result<WakeResult>;
    fn identify(&self, req: &IdentifyRequest) -> IdentifyResult;
}
