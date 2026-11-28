//! Who is online: the newest listing of the owner's sessions, kept per
//! project for what a `to:` can name, and once across every machine the
//! sources reach, for the router and the Home view. A source's listing can be
//! slow (seconds of CPU per project on a small machine, seconds more per
//! machine), so each is kept, one runs at a time per scope, and every caller
//! is answered from the newest: the router and the view never run two
//! listings across machines at once.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use super::{Core, EVERYWHERE_TIMEOUT, Listing};
use crate::model::{Candidate, ListRequest};

/// A listing older than this is replaced by the next one asked for.
const FRESH: Duration = Duration::from_secs(15);
/// A listing older than this is not given at all: callers wait for the new
/// one as for the first.
const STALE: Duration = Duration::from_secs(300);

pub struct Online {
    /// The machines `sessions` covers, as their sources name them. Known
    /// before any listing is.
    pub machines: Vec<String>,
    /// The newest finished listing: every session there, cold ones too, each
    /// as its source describes it: activity, model, label and group. `None`
    /// while the first one runs, or while the newest is over 5 minutes old.
    pub sessions: Option<Arc<Listing>>,
}

/// What a listing covers.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) enum Scope {
    /// The configured project's sessions here; `None`: every configured project's.
    Project(Option<String>),
    /// Every session the sources reach, on every machine (`offerable`).
    Everywhere,
}

/// A finished listing, and when it started: what it shows was true then.
#[derive(Clone)]
pub(super) struct Listed {
    pub started: Instant,
    pub listing: Arc<Listing>,
}

/// The newest finished listing of a scope, and the one running.
#[derive(Clone, Default)]
pub(super) struct Shared {
    pub newest: Option<Listed>,
    /// When the running listing started.
    pub running: Option<Instant>,
    /// Someone needs a listing that starts after the running one did: the
    /// next starts as soon as it ends.
    again: bool,
}

/// What a caller needs of a scope's listing.
#[derive(Clone, Copy)]
pub(super) enum Need {
    /// Any listing that finished; a new one only when the newest is older
    /// than this (or there is none) and none runs. What answers at once uses
    /// this: the newest is given now, the new one later.
    YoungerThan(Duration),
    /// One that started at this instant or after: the running one if it did,
    /// else the next. What must see the sessions as they are now uses this.
    StartedAfter(Instant),
}

/// Who reads the listing across machines: each uses its own part of it.
#[derive(Clone, Copy)]
pub(super) enum Reader {
    /// The router, for an item in this source's inbox.
    Router(usize),
    /// The owner's view.
    View,
}

#[derive(Default)]
pub(super) struct Snapshots(Mutex<HashMap<Scope, watch::Sender<Shared>>>);

impl Core {
    /// The newest listing of `scope` as it changes, after starting (or
    /// queueing) the one `need` asks for, if the newest won't do.
    pub(super) fn listing(self: &Arc<Self>, scope: Scope, need: Need) -> watch::Receiver<Shared> {
        let mut all = self.snapshots.0.lock().unwrap();
        let snap = all.entry(scope.clone()).or_default();
        let (start, again) = {
            let s = snap.borrow();
            match (need, s.running) {
                (Need::YoungerThan(age), None) => (s.newest.as_ref().is_none_or(|l| l.started.elapsed() >= age), false),
                (Need::YoungerThan(_), Some(_)) => (false, false),
                (Need::StartedAfter(t), None) => (s.newest.as_ref().is_none_or(|l| l.started < t), false),
                (Need::StartedAfter(t), Some(running)) => (false, running < t),
            }
        };
        if start {
            self.start_listing(scope, snap);
        } else if again {
            snap.send_modify(|s| s.again = true);
        }
        snap.subscribe()
    }

    /// Runs one listing of `scope`, and the next one if one is owed when it ends.
    fn start_listing(self: &Arc<Self>, scope: Scope, snap: &watch::Sender<Shared>) {
        let started = Instant::now();
        snap.send_modify(|s| {
            s.running = Some(started);
            s.again = false;
        });
        let core = self.clone();
        tokio::spawn(async move {
            let req = ListRequest {
                person: Some(core.cfg.owner.clone()),
                project: match &scope {
                    Scope::Project(p) => p.clone(),
                    Scope::Everywhere => None,
                },
                refs: vec![],
                offerable: scope == Scope::Everywhere,
            };
            // In a task of its own, so `running` is cleared however it ends.
            let lister = core.clone();
            let listed = tokio::spawn(async move { lister.list(&req).await }).await;
            let all = core.snapshots.0.lock().unwrap();
            let snap = all.get(&scope).expect("an entry is never removed");
            let mut again = false;
            snap.send_modify(|s| {
                s.running = None;
                again = s.again;
                match listed {
                    Ok(l) => {
                        s.newest = Some(Listed {
                            started,
                            listing: Arc::new(l),
                        })
                    }
                    Err(e) => tracing::error!("listing sessions failed: {e}"),
                }
            });
            if again {
                core.start_listing(scope.clone(), snap);
            }
        });
    }

    /// The newest listing of `scope`, without starting one.
    pub(super) fn newest(&self, scope: &Scope) -> Option<Arc<Listing>> {
        let all = self.snapshots.0.lock().unwrap();
        all.get(scope)?.borrow().newest.as_ref().map(|l| l.listing.clone())
    }

    /// The owner's sessions in `project` (with none, every configured
    /// project): what a `to:` in a channel of that project can name. Answers
    /// from the newest listing, and starts a new one when the newest is older
    /// than 15 s and none runs. With none, or only one older than 5 minutes,
    /// waits up to `wait` for the new one.
    pub async fn online(self: &Arc<Self>, project: Option<&str>, wait: Duration) -> Online {
        let mut newest = self.listing(Scope::Project(project.map(String::from)), Need::YoungerThan(FRESH));
        let usable = |s: &Shared| s.newest.as_ref().is_some_and(|l| l.started.elapsed() <= STALE);
        let sessions = match tokio::time::timeout(wait, newest.wait_for(usable)).await {
            Ok(Ok(s)) => s.newest.as_ref().map(|l| l.listing.clone()),
            _ => None,
        };
        Online {
            machines: self.sources.iter().filter_map(|s| s.machine().map(String::from)).collect(),
            sessions,
        }
    }

    /// Every session the sources reach, on every machine, in a listing that
    /// started at `after` or later: the one running, or the next. `None` when
    /// none finished in time.
    pub(super) async fn everywhere(self: &Arc<Self>, after: Instant) -> Option<Arc<Listing>> {
        let mut newest = self.listing(Scope::Everywhere, Need::StartedAfter(after));
        let late_enough = |s: &Shared| s.newest.as_ref().is_some_and(|l| l.started >= after);
        // At most the one running and the next; this is for one that never ends.
        let waited = tokio::time::timeout(EVERYWHERE_TIMEOUT * 2 + FRESH, newest.wait_for(late_enough)).await;
        match waited {
            Ok(Ok(s)) => s.newest.as_ref().map(|l| l.listing.clone()),
            _ => None,
        }
    }

    /// The candidates in `listing` that `reader` uses: every caller's reach,
    /// written in one place.
    pub(super) fn reach<'a>(&'a self, listing: &'a Listing, reader: Reader) -> impl Iterator<Item = &'a Candidate> {
        listing
            .for_person(&self.cfg.owner)
            .filter(move |(i, _)| match reader {
                Reader::Router(src) => *i == src,
                Reader::View => true,
            })
            .map(|(_, c)| c)
    }

    /// The conversations a form may open a thread for `c` in: its project's.
    /// None for a candidate its source can't wake from here.
    pub(super) fn form_conversations(&self, c: &Candidate) -> Vec<String> {
        if !c.wakeable {
            return vec![];
        }
        self.cfg
            .channels()
            .filter(|(_, ch)| ch.project.is_some() && ch.project == c.project)
            .map(|(id, _)| id.clone())
            .collect()
    }
}
