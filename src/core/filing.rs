//! A message the rule woke nobody for is filed in a source's inbox, where
//! sessions look, and taken out again when a person chooses a session for it.

use std::time::Duration;

use super::Core;
use crate::model::{FileRequest, Msg, Posted};
use crate::state::{FiledItem, ThreadState};

/// Every inbox request but an offer. The thread's lock is held across `file`
/// and `take`, so a source that hangs must not hold the thread for long.
pub(super) const INBOX_TIMEOUT: Duration = Duration::from_secs(20);

impl Core {
    /// Files `m`, which the rule woke nobody for, with the first source that
    /// has an inbox, unless the thread's earlier item still waits there. The
    /// thread file is saved at once: the item's route request is already on
    /// its way. A failure is logged and the message waits as `pending` alone.
    /// The caller holds the thread's lock.
    pub(super) async fn file(&self, st: &mut ThreadState, m: &Msg, sender: &str) {
        if let Some(f) = &st.filed {
            let waits = match self.sources.iter().find(|s| s.name() == f.source) {
                None => Ok(false),
                Some(s) => match tokio::time::timeout(INBOX_TIMEOUT, s.inbox_item(&f.item)).await {
                    Ok(Ok(i)) => Ok(i.is_some_and(|i| i.waiting)),
                    Ok(Err(e)) => Err(format!("{e:#}")),
                    Err(_) => Err("timed out".into()),
                },
            };
            match waits {
                Ok(true) => return,
                Ok(false) => st.filed = None,
                Err(e) => {
                    tracing::warn!("cannot tell whether {} still waits in {}: {e}", f.item, f.place);
                    return;
                }
            }
        }
        let req = FileRequest {
            text: m.text.clone(),
            project: self.cfg.channel(&m.conversation).and_then(|c| c.project.clone()),
            reply_to: format!("@{}", self.message_ref(&m.conversation, &m.thread)),
            via_ref: self.message_ref(&m.conversation, &m.id),
            via_sender: sender.to_string(),
        };
        let _gate = self.filing.read().await;
        for s in &self.sources {
            match tokio::time::timeout(INBOX_TIMEOUT, s.file(&req)).await {
                Ok(Ok(None)) => continue,
                Ok(Ok(Some(f))) => {
                    tracing::info!("{} is in {} as {}", req.via_ref, f.place, f.item);
                    st.filed = Some(FiledItem {
                        source: s.name().to_string(),
                        item: f.item,
                        place: f.place,
                        message: m.id.clone(),
                        post: None,
                    });
                    if let Err(e) = self.store.save_thread(st) {
                        tracing::error!("cannot save the thread of {}: {e:#}", req.via_ref);
                    }
                }
                Ok(Err(e)) => tracing::warn!("{} was not filed with {}: {e:#}", req.via_ref, s.name()),
                Err(_) => tracing::warn!("{} was not filed with {}: timed out", req.via_ref, s.name()),
            }
            return;
        }
    }

    /// A person chose a session for the thread: its filed item leaves the inbox.
    pub(super) async fn take_filed(&self, st: &mut ThreadState, by: &str) {
        let Some(f) = st.filed.take() else { return };
        let Some(s) = self.sources.iter().find(|s| s.name() == f.source) else {
            return;
        };
        match tokio::time::timeout(INBOX_TIMEOUT, s.take(&f.item, by)).await {
            Ok(Ok(())) => tracing::info!("{} taken from {}: {by} chose a session", f.item, f.place),
            Ok(Err(e)) => tracing::warn!("{} was not taken from {}: {e:#}", f.item, f.place),
            Err(_) => tracing::warn!("{} was not taken from {}: timed out", f.item, f.place),
        }
    }
}

/// The line that says where the thread's message waits, owed until a post of
/// the bot's has carried it.
pub(super) fn note(st: &ThreadState) -> Option<String> {
    let f = st.filed.as_ref().filter(|f| f.post.is_none())?;
    Some(format!("In {} as {}.", super::text::clean(&f.place), super::text::clean(&f.item)))
}

/// Records the post that carried `note`'s line.
pub(super) fn posted(st: &mut ThreadState, post: Option<Posted>) {
    if let (Some(f), Some(p)) = (st.filed.as_mut().filter(|f| f.post.is_none()), post) {
        f.post = Some(p.id);
    }
}
