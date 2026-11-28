//! The core: thread state, the wake rule, the loop limit, recovery and the
//! client's requests. It knows no tool's vocabulary: sources and the surface
//! come in behind traits.

mod api;
mod cursor;
mod deliver;
mod events;
mod filing;
mod form;
mod online;
mod route;
mod text;

pub use text::candidate_line;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use futures_util::future::join_all;
use tokio::sync::{OwnedMutexGuard, mpsc};

use crate::config::Config;
use crate::jev::Jev;
use crate::model::{Candidate, FailKind, Failure, ListRequest, Msg, SurfaceEvent, WakeRequest, WakeResult};
use crate::routes;
use crate::source::Source;
use crate::state::Store;
use crate::surface::Surface;
use cursor::Cursor;
use events::Job;

const LIST_TIMEOUT: Duration = Duration::from_secs(15);
const WAKE_TIMEOUT: Duration = Duration::from_secs(60);

type ThreadKey = (String, String);

pub struct Core {
    cfg: Config,
    surface: Arc<dyn Surface>,
    sources: Vec<Arc<dyn Source>>,
    store: Store,
    /// The bot's own sender ID: its posts are never handled.
    bot: String,
    locks: Mutex<HashMap<ThreadKey, Arc<tokio::sync::Mutex<()>>>>,
    /// One worker per thread with events waiting, so a thread's events are
    /// handled in the order they arrived while other threads go on.
    queues: Mutex<HashMap<ThreadKey, mpsc::UnboundedSender<Job>>>,
    cursor: Cursor,
    /// Channels whose recovery is failing and being retried.
    recovering: Mutex<BTreeSet<String>>,
    /// Messages (conversation, id) waiting for a source to answer, each with one retry loop.
    awaiting: Mutex<BTreeSet<ThreadKey>>,
    /// The router's model. `None`: routing is off.
    jev: Option<Jev>,
    routes: routes::Log,
    routing: Mutex<route::Routing>,
    /// Held shared from before a message is filed until its thread file names
    /// the item. Filing starts the route request for the item, which can
    /// arrive before the source has answered with the item's ID; the router
    /// takes this exclusively once before it looks for the item's thread.
    filing: tokio::sync::RwLock<()>,
    /// One second step at a time: each starts an agent.
    second: tokio::sync::Semaphore,
    snapshots: online::Snapshots,
    forms: form::Sent,
}

/// What the sources answered, with the index of the source that listed each candidate.
#[derive(Default)]
pub struct Listing {
    pub candidates: Vec<(usize, Candidate)>,
    pub unreachable: Vec<String>,
    /// A source failed or timed out: what is missing may only be unchecked.
    pub failed: bool,
}

impl Listing {
    fn merge(&mut self, src: usize, r: crate::model::ListResponse) {
        for c in r.candidates {
            if !self.candidates.iter().any(|(_, o)| o.ref_ == c.ref_) {
                self.candidates.push((src, c));
            }
        }
        self.unreachable.extend(r.unreachable);
    }

    pub fn absorb(&mut self, other: Listing) {
        for (i, c) in other.candidates {
            if self.find(&c.ref_).is_none() {
                self.candidates.push((i, c));
            }
        }
        self.unreachable.extend(other.unreachable);
        self.failed |= other.failed;
    }

    fn find(&self, ref_: &str) -> Option<&(usize, Candidate)> {
        self.candidates.iter().find(|(_, c)| c.ref_ == ref_)
    }

    pub fn for_person<'a>(&'a self, person: &'a str) -> impl Iterator<Item = &'a (usize, Candidate)> {
        self.candidates.iter().filter(move |(_, c)| c.person == person)
    }

    /// A `to:` or `--to` word: a ref, or a short ID that exactly one candidate has.
    pub fn resolve(&self, word: &str) -> Option<&(usize, Candidate)> {
        if let Some(c) = self.find(word) {
            return Some(c);
        }
        let mut by_id = self.candidates.iter().filter(|(_, c)| c.id.as_deref() == Some(word));
        match (by_id.next(), by_id.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        }
    }

    /// Why `resolve` found nothing for `word`, as far as the listing can tell.
    fn why_not(&self, word: &str) -> String {
        let ids: Vec<&str> = self
            .candidates
            .iter()
            .filter(|(_, c)| c.id.as_deref() == Some(word))
            .map(|(_, c)| c.ref_.as_str())
            .collect();
        if ids.len() > 1 {
            format!("`{}` is ambiguous: it is {}. Use a full ref.", text::clean(word), ids.join(", "))
        } else if self.failed {
            format!("`{}` could not be checked: a source did not answer.", text::clean(word))
        } else {
            format!("`{}` matches no running session.", text::clean(word))
        }
    }
}

impl Core {
    /// Why `listing` resolved nothing for the `to:` or `--to` word `word`.
    async fn why_not(&self, listing: &Listing, word: &str) -> String {
        for s in &self.sources {
            if s.elsewhere(word).await {
                return format!(
                    "`{}` is on another machine: sessions there can be offered a message but not woken from Slack.",
                    text::clean(word)
                );
            }
        }
        listing.why_not(word)
    }

    pub fn new(
        cfg: Config,
        surface: Arc<dyn Surface>,
        sources: Vec<Arc<dyn Source>>,
        store: Store,
        bot: String,
        jev: Option<Jev>,
        routes: routes::Log,
    ) -> Result<Arc<Core>> {
        let cursor = Cursor::new(store.clone());
        let routing = Mutex::new(route::Routing::resume(&routes.read()?));
        Ok(Arc::new(Core {
            cfg,
            surface,
            sources,
            store,
            bot,
            locks: Mutex::default(),
            queues: Mutex::default(),
            cursor,
            recovering: Mutex::default(),
            awaiting: Mutex::default(),
            jev,
            routes,
            routing,
            filing: tokio::sync::RwLock::default(),
            second: tokio::sync::Semaphore::new(1),
            snapshots: online::Snapshots::default(),
            forms: form::Sent::default(),
        }))
    }

    /// Runs until the surface's event channel closes.
    pub async fn run(self: &Arc<Self>, mut events: mpsc::Receiver<SurfaceEvent>) {
        while let Some(ev) = events.recv().await {
            self.on_event(ev).await;
        }
    }

    pub async fn lock(&self, conversation: &str, thread: &str) -> OwnedMutexGuard<()> {
        let m = self
            .locks
            .lock()
            .unwrap()
            .entry((conversation.to_string(), thread.to_string()))
            .or_default()
            .clone();
        m.lock_owned().await
    }

    /// Asks every source, side by side. A source that fails or times out
    /// returns nothing and sets `failed`.
    async fn list(&self, req: &ListRequest) -> Listing {
        let answers = join_all(self.sources.iter().enumerate().map(|(i, s)| async move {
            match tokio::time::timeout(LIST_TIMEOUT, s.list(req)).await {
                Ok(Ok(r)) => (i, Some(r)),
                Ok(Err(e)) => {
                    tracing::warn!("{} list failed: {e:#}", s.name());
                    (i, None)
                }
                Err(_) => {
                    tracing::warn!("{} list timed out", s.name());
                    (i, None)
                }
            }
        }))
        .await;
        let mut l = Listing::default();
        for (i, r) in answers {
            match r {
                Some(r) => l.merge(i, r),
                None => l.failed = true,
            }
        }
        l
    }

    /// The owner's listing, plus each of `persons`' own sessions.
    async fn list_for(&self, persons: &[String], project: Option<String>, refs: Vec<String>) -> Listing {
        let owner = self.cfg.owner.clone();
        let mut listing = self
            .list(&ListRequest {
                person: Some(owner.clone()),
                project: project.clone(),
                refs,
                offerable: false,
            })
            .await;
        for p in persons.iter().filter(|p| **p != owner) {
            listing.absorb(
                self.list(&ListRequest {
                    person: Some(p.clone()),
                    project: project.clone(),
                    refs: vec![],
                    offerable: false,
                })
                .await,
            );
        }
        listing
    }

    async fn wake(&self, src: usize, req: &WakeRequest) -> WakeResult {
        let failed = |detail: String| WakeResult {
            woken: false,
            ref_: req.ref_.clone(),
            detail,
        };
        match tokio::time::timeout(WAKE_TIMEOUT, self.sources[src].wake(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => failed(format!("{e:#}")),
            Err(_) => failed("timed out".into()),
        }
    }

    /// The directory name of whoever wrote `m`, with `authors` from its
    /// thread's file: a post the bot made for a person is that person's.
    fn author(&self, authors: &BTreeMap<String, String>, m: &Msg) -> Option<String> {
        if m.sender.id == self.bot {
            return authors.get(&m.id).cloned();
        }
        self.cfg.person_by_slack(&m.sender.id).map(|(n, _)| n.clone())
    }

    /// The `authors` of a thread's file; none without a readable file.
    fn authors(&self, conversation: &str, thread: &str) -> BTreeMap<String, String> {
        match self.store.load_thread(conversation, thread) {
            Ok(st) => st.map(|st| st.authors).unwrap_or_default(),
            Err(e) => {
                tracing::warn!("cannot read the thread file of {conversation}/{thread}: {e:#}");
                BTreeMap::new()
            }
        }
    }

    /// `channel-name/ts`: how a message is named to sessions.
    fn message_ref(&self, conversation: &str, id: &str) -> String {
        let name = self.cfg.channel(conversation).map(|c| c.name.as_str()).unwrap_or(conversation);
        format!("{name}/{id}")
    }

    fn parse_message_ref(&self, r: &str) -> Result<(String, String)> {
        let Some((name, id)) = r.split_once('/') else {
            bail!(Failure::new(
                FailKind::Invalid,
                format!("{r:?} is not a message ref: expected <channel>/<id>")
            ))
        };
        let Some((conv, _)) = self.cfg.channel_by_name(name) else {
            bail!(Failure::new(FailKind::NotFound, format!("no configured channel is named {name:?}")))
        };
        Ok((conv.clone(), id.to_string()))
    }
}
