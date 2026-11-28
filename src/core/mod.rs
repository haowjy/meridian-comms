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
mod view;

pub use text::candidate_line;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use futures_util::future::join_all;
use tokio::sync::{OwnedMutexGuard, mpsc};

use crate::config::Config;
use crate::jev::Jev;
use crate::model::{Candidate, FailKind, Failure, Group, ListResponse, Machine, Msg, SurfaceEvent, WakeRequest, WakeResult};
use crate::routes;
use crate::source::Source;
use crate::state::Store;
use crate::surface::Surface;
use cursor::Cursor;
use events::Job;

/// For one source's listing across machines: it asks for its list of
/// machines, then all of them at once.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// A wake on another machine is a Meridian call over SSH, which Meridian
/// gives up to 15 s to connect and 75 s to answer.
const WAKE_TIMEOUT: Duration = Duration::from_secs(100);

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
    /// Messages (conversation, id) waiting for a source to answer, each with
    /// one retry loop, and since when.
    awaiting: Mutex<BTreeMap<ThreadKey, std::time::Instant>>,
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
    snapshot: online::Snapshot,
    forms: form::Sent,
    publisher: view::Publisher,
}

/// What the sources answered, with the index of the source that listed each
/// candidate and machine.
#[derive(Default, Clone)]
pub struct Listing {
    pub candidates: Vec<(usize, Candidate)>,
    pub groups: Vec<Group>,
    pub machines: Vec<(usize, Machine)>,
    /// The sources that failed or timed out: whatever they reach is unchecked.
    pub failed: Vec<usize>,
}

impl Listing {
    fn merge(&mut self, src: usize, r: ListResponse) {
        for c in r.candidates {
            if self.find(&c.ref_).is_none() {
                self.candidates.push((src, c));
            }
        }
        self.groups.extend(r.groups);
        self.machines.extend(r.machines.into_iter().map(|m| (src, m)));
    }

    /// A listing in which every source failed.
    fn unanswered(sources: usize) -> Listing {
        Listing {
            failed: (0..sources).collect(),
            ..Listing::default()
        }
    }

    /// The same listing with only the candidates `keep` takes.
    fn keep(&self, keep: impl Fn(usize, &Candidate) -> bool) -> Listing {
        Listing {
            candidates: self.candidates.iter().filter(|(i, c)| keep(*i, c)).cloned().collect(),
            ..self.clone()
        }
    }

    fn find(&self, ref_: &str) -> Option<&(usize, Candidate)> {
        self.candidates.iter().find(|(_, c)| c.ref_ == ref_)
    }

    pub fn for_person<'a>(&'a self, person: &'a str) -> impl Iterator<Item = &'a (usize, Candidate)> {
        self.candidates.iter().filter(move |(_, c)| c.person == person)
    }

    /// The candidates `word` names by one of their `names`.
    fn named<'a>(&'a self, word: &'a str) -> impl Iterator<Item = &'a (usize, Candidate)> + 'a {
        self.candidates.iter().filter(move |(_, c)| c.names.iter().any(|n| n == word))
    }

    /// A `to:` or `--to` word: a ref, or a name exactly one candidate has.
    pub fn resolve<'a>(&'a self, word: &'a str) -> Option<&'a (usize, Candidate)> {
        if let Some(c) = self.find(word) {
            return Some(c);
        }
        let mut named = self.named(word);
        match (named.next(), named.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        }
    }

    /// The word to type for `c` here: its first name no other candidate
    /// has, else its ref.
    fn typable<'a>(&'a self, c: &'a Candidate) -> &'a str {
        c.names
            .iter()
            .find(|n| self.named(n).count() == 1)
            .map_or(c.ref_.as_str(), String::as_str)
    }

    /// The machines of source `src` that did not answer.
    fn missing(&self, src: usize) -> Vec<Machine> {
        self.machines
            .iter()
            .filter(|(i, m)| *i == src && !m.ok)
            .map(|(_, m)| m.clone())
            .collect()
    }
}

impl Core {
    /// Whether `word` could name a session the listing could not see: on a
    /// source that failed, or on a machine that did not answer.
    fn unchecked(&self, listing: &Listing, word: &str) -> bool {
        !listing.failed.is_empty()
            || self.sources.iter().enumerate().any(|(i, s)| {
                let missing = listing.missing(i);
                !missing.is_empty() && s.could_be_unchecked(word, &missing)
            })
    }

    /// How people see the ref `r`, cleaned for a post: in the words of the
    /// source it belongs to, with the machines of the listing it came from.
    fn show(&self, listing: &Listing, r: &str) -> String {
        let shown = self.sources.iter().enumerate().find_map(|(i, s)| {
            let machines: Vec<Machine> = listing.machines.iter().filter(|(j, _)| *j == i).map(|(_, m)| m.clone()).collect();
            s.show(r, &machines)
        });
        text::clean(&shown.unwrap_or_else(|| r.to_string()))
    }

    /// A candidate in a post: how it is shown, and its label.
    fn label(&self, listing: &Listing, c: &Candidate) -> String {
        let shown = self.show(listing, &c.ref_);
        match &c.label {
            Some(l) => format!("`{shown}` ({})", text::clean(l)),
            None => format!("`{shown}`"),
        }
    }

    /// Why `listing` resolved nothing for the `to:` or `--to` word `word`, in
    /// Slack's words: an ambiguous one lists what to type instead.
    fn why_not(&self, listing: &Listing, word: &str) -> String {
        let named: Vec<String> = listing
            .named(word)
            .map(|(_, c)| format!("`{}`", text::clean(listing.typable(c))))
            .collect();
        let w = text::clean(word);
        if named.len() > 1 {
            format!("`{w}` is ambiguous: it is {}. Use one of those.", named.join(", "))
        } else if self.unchecked(listing, word) {
            let silent: Vec<String> = listing
                .machines
                .iter()
                .filter(|(_, m)| !m.ok)
                .map(|(_, m)| {
                    m.name
                        .as_deref()
                        .map_or_else(|| "the saved machines".to_string(), |n| format!("`{}`", text::clean(n)))
                })
                .chain((!listing.failed.is_empty()).then(|| "a source".to_string()))
                .collect();
            format!(
                "{} didn't answer, and `{w}` isn't running on the machines that did.",
                silent.join(" and ")
            )
        } else {
            format!("`{w}` matches no running session.")
        }
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
            snapshot: online::Snapshot::default(),
            forms: form::Sent::default(),
            publisher: view::Publisher::default(),
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
    /// gives nothing and is named in `failed`. `online` starts it.
    async fn list(&self) -> Listing {
        let answers = join_all(self.sources.iter().enumerate().map(|(i, s)| async move {
            match tokio::time::timeout(LIST_TIMEOUT, s.list()).await {
                Ok(Ok(r)) => (i, Some(r)),
                Ok(Err(e)) => {
                    tracing::warn!("{} list failed: {e:#}", s.name());
                    (i, None)
                }
                Err(_) => {
                    tracing::warn!("{} list timed out after {} s", s.name(), LIST_TIMEOUT.as_secs());
                    (i, None)
                }
            }
        }))
        .await;
        let mut l = Listing::default();
        for (i, r) in answers {
            match r {
                Some(r) => l.merge(i, r),
                None => l.failed.push(i),
            }
        }
        l
    }

    async fn wake(&self, src: usize, req: &WakeRequest) -> WakeResult {
        let failed = |detail: String| WakeResult {
            woken: false,
            ref_: req.to.ref_.clone(),
            detail,
        };
        match tokio::time::timeout(WAKE_TIMEOUT, self.sources[src].wake(req)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => failed(format!("{e:#}")),
            Err(_) => failed(format!(
                "no answer in {} s: it may have reached the session",
                WAKE_TIMEOUT.as_secs()
            )),
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
