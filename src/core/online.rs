//! Who is online: one listing of every session the sources reach, on every
//! machine and in every project, kept as the newest snapshot. A listing is
//! slow (seconds of CPU on a small machine, seconds more per machine), so one
//! runs at a time and every caller is answered from the newest one that is
//! fresh enough for it: a refresh is one fan-out, whoever asked. Which
//! candidates each caller uses is written once, in `reach`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use super::{Core, LIST_TIMEOUT, Listing};
use crate::model::Candidate;

/// What waits for a listing takes one no older than this.
pub(super) const FRESH: Duration = Duration::from_secs(15);
/// A listing older than this is not given at all: callers wait for the new
/// one as for the first.
const STALE: Duration = Duration::from_secs(300);

pub struct Online {
    /// The machines `sessions` covers, as their sources name them: the
    /// listing's, or before the first one, the sources' own.
    pub machines: Vec<String>,
    /// The newest finished listing, kept to the caller's part. `None` while
    /// the first one runs, or while the newest is over 5 minutes old.
    pub sessions: Option<Arc<Listing>>,
}

/// A finished listing, and when it started: what it shows was true then.
#[derive(Clone)]
pub(super) struct Listed {
    pub started: Instant,
    pub listing: Arc<Listing>,
}

/// The newest finished listing, and the one running.
#[derive(Clone, Default)]
pub(super) struct Shared {
    pub newest: Option<Listed>,
    /// When the running listing started.
    pub running: Option<Instant>,
    /// Who asked for the running listing: the first started it, the rest joined.
    by: Vec<By>,
    /// Some: someone needs a listing that starts after the running one did,
    /// and the next starts as soon as it ends, for these.
    next: Option<Vec<By>>,
}

/// Who asked for a listing, for the log.
#[derive(Clone, Copy)]
pub(super) enum By {
    ViewOpen,
    ViewRefresh,
    Router,
    Thread,
    Client,
    Form,
}

impl By {
    fn word(self) -> &'static str {
        match self {
            By::ViewOpen => "view open",
            By::ViewRefresh => "view refresh",
            By::Router => "router",
            By::Thread => "thread",
            By::Client => "client",
            By::Form => "form",
        }
    }
}

/// What a caller needs of the listing.
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

/// Who reads the listing: each uses its own part of it.
#[derive(Clone, Copy)]
pub(super) enum Reader<'a> {
    /// A thread's message, click, Continue and `send --to`, and the picker:
    /// what can be woken in the channel's project (any, for a channel with
    /// none), or is already among the thread's refs.
    Thread { project: Option<&'a str>, refs: &'a [String] },
    /// The form and `list`: what can be woken in the project.
    Project(Option<&'a str>),
    /// The owner's view: every one of the owner's sessions.
    View,
    /// The router, for an item in this source's inbox: every one of the
    /// owner's sessions there, configured project or not.
    Router(usize),
}

#[derive(Default)]
pub(super) struct Snapshot(Mutex<watch::Sender<Shared>>);

impl Core {
    /// The listing as it changes, after starting (or queueing) the one `need`
    /// asks for, if the newest won't do. `by`: who asks, for the log.
    pub(super) fn listing(self: &Arc<Self>, need: Need, by: By) -> watch::Receiver<Shared> {
        enum Act {
            Start,
            Join,
            Next,
            Newest,
        }
        let snap = self.snapshot.0.lock().unwrap();
        let act = {
            let s = snap.borrow();
            match (need, s.running) {
                (Need::YoungerThan(age), None) if s.newest.as_ref().is_none_or(|l| l.started.elapsed() >= age) => Act::Start,
                (Need::StartedAfter(t), None) if s.newest.as_ref().is_none_or(|l| l.started < t) => Act::Start,
                (_, None) => Act::Newest,
                (Need::StartedAfter(t), Some(running)) if running < t => Act::Next,
                (_, Some(_)) => Act::Join,
            }
        };
        // Who joined is only for the log: subscribers aren't told.
        match act {
            Act::Start => self.start_listing(&snap, vec![by]),
            Act::Join => {
                snap.send_if_modified(|s| {
                    s.by.push(by);
                    false
                });
            }
            Act::Next => {
                snap.send_if_modified(|s| {
                    s.next.get_or_insert_default().push(by);
                    false
                });
            }
            Act::Newest => {}
        }
        snap.subscribe()
    }

    /// Runs one listing for `by`, and the next one if one is owed when it ends.
    fn start_listing(self: &Arc<Self>, snap: &watch::Sender<Shared>, by: Vec<By>) {
        let started = Instant::now();
        snap.send_modify(|s| {
            s.running = Some(started);
            s.by = by;
        });
        let core = self.clone();
        tokio::spawn(async move {
            // In a task of its own, so `running` is cleared however it ends.
            let lister = core.clone();
            let listed = tokio::spawn(async move { lister.list().await }).await;
            let snap = core.snapshot.0.lock().unwrap();
            let mut next = None;
            snap.send_modify(|s| {
                s.running = None;
                next = std::mem::take(&mut s.next);
                let words: Vec<&str> = s.by.iter().map(|b| b.word()).collect();
                let (first, joined) = words.split_first().map_or(("nobody", &[][..]), |(f, j)| (*f, j));
                let joined = if joined.is_empty() {
                    "nobody".to_string()
                } else {
                    joined.join(", ")
                };
                let ended = match &listed {
                    Ok(l) if !l.failed.is_empty() => "a source failed".to_string(),
                    Ok(l) => format!("{} candidates", l.candidates.len()),
                    Err(_) => "the listing panicked".to_string(),
                };
                tracing::info!(
                    "listing: started for {first}, joined by {joined}, took {:.1} s, {ended}",
                    started.elapsed().as_secs_f32()
                );
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
            if let Some(next) = next {
                core.start_listing(&snap, next);
            }
        });
    }

    /// The newest listing, without starting one, unless it is over 5 minutes
    /// old: then none, as for `online`. A caller trusts what it says about the
    /// machines, so a machine removed since counts for at most that long.
    pub(super) fn newest(&self) -> Option<Arc<Listing>> {
        let snap = self.snapshot.0.lock().unwrap();
        let s = snap.borrow();
        s.newest
            .as_ref()
            .filter(|l| l.started.elapsed() <= STALE)
            .map(|l| l.listing.clone())
    }

    /// For what must answer within seconds: the newest listing, kept to
    /// `reader`'s part, starting a new one when the newest is older than 15 s
    /// and none runs. With none, or only one older than 5 minutes, waits up
    /// to `wait` for the new one.
    pub(super) async fn online(self: &Arc<Self>, reader: Reader<'_>, wait: Duration) -> Online {
        let mut newest = self.listing(Need::YoungerThan(FRESH), By::Form);
        let usable = |s: &Shared| s.newest.as_ref().is_some_and(|l| l.started.elapsed() <= STALE);
        let sessions = match tokio::time::timeout(wait, newest.wait_for(usable)).await {
            Ok(Ok(s)) => s.newest.as_ref().map(|l| Arc::new(self.reach(&l.listing, reader))),
            _ => None,
        };
        let listed: Vec<String> = sessions
            .iter()
            .flat_map(|l| l.machines.iter().filter_map(|(_, m)| m.name.clone()))
            .collect();
        Online {
            machines: match listed.is_empty() {
                true => self.sources.iter().filter_map(|s| s.machine().map(String::from)).collect(),
                false => listed,
            },
            sessions,
        }
    }

    /// For everything else: a listing that started at `after` or later, the
    /// one running or the next. Each source's own limit bounds it; this is
    /// for one that never ends, which leaves every source unanswered.
    pub(super) async fn listed_after(self: &Arc<Self>, after: Instant, by: By) -> Arc<Listing> {
        let mut newest = self.listing(Need::StartedAfter(after), by);
        let late_enough = |s: &Shared| s.newest.as_ref().is_some_and(|l| l.started >= after);
        // At most the one running and the next.
        let waited = tokio::time::timeout(LIST_TIMEOUT * 2 + FRESH, newest.wait_for(late_enough)).await;
        match waited {
            Ok(Ok(s)) => s.newest.as_ref().map(|l| l.listing.clone()),
            _ => None,
        }
        .unwrap_or_else(|| Arc::new(Listing::unanswered(self.sources.len())))
    }

    /// A listing no older than 15 s: what a handled message, a click, a
    /// client's request and the router read. A retry comes 30 s after its
    /// attempt, so it never reuses the listing that attempt failed on.
    pub(super) async fn fresh(self: &Arc<Self>, by: By) -> Arc<Listing> {
        let now = Instant::now();
        self.listed_after(now.checked_sub(FRESH).unwrap_or(now), by).await
    }

    /// The candidates in `listing` that `reader` uses: every caller's reach,
    /// written in one place. `default` is only ever set on a wakeable
    /// candidate, so the rule's defaults are wakeable too.
    pub(super) fn reach(&self, listing: &Listing, reader: Reader<'_>) -> Listing {
        let owner = self.cfg.owner.as_str();
        listing.keep(|src, c| match reader {
            Reader::Thread { project, refs } => c.wakeable && (in_project(c, project) || refs.contains(&c.ref_)),
            Reader::Project(project) => c.wakeable && in_project(c, project),
            Reader::View => c.person == owner,
            Reader::Router(s) => src == s && c.person == owner,
        })
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

/// A channel's project: `None` takes any.
fn in_project(c: &Candidate, project: Option<&str>) -> bool {
    project.is_none_or(|p| c.project.as_deref() == Some(p))
}
