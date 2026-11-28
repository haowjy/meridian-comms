//! Who is online: the sessions a `to:` can name, kept per project as the
//! newest listing, so a surface that must answer within seconds can answer
//! from it. A source's listing can be slow (seconds of CPU per project on a
//! small machine), so nothing here lists while a caller waits on it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use super::{Core, Listing};
use crate::model::ListRequest;

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

#[derive(Default)]
pub(super) struct Snapshots(Mutex<HashMap<Option<String>, Snapshot>>);

struct Snapshot {
    newest: watch::Sender<Option<(Instant, Arc<Listing>)>>,
    running: bool,
}

impl Core {
    /// The owner's sessions in `project` (with none, every configured
    /// project): what a `to:` in a channel of that project can name. Answers
    /// from the newest listing, and starts a new one when the newest is older
    /// than 15 s and none runs. With none, or only one older than 5 minutes,
    /// waits up to `wait` for the new one.
    pub async fn online(self: &Arc<Self>, project: Option<&str>, wait: Duration) -> Online {
        let key = project.map(String::from);
        let mut newest = {
            let mut all = self.snapshots.0.lock().unwrap();
            let snap = all.entry(key.clone()).or_insert_with(|| Snapshot {
                newest: watch::Sender::new(None),
                running: false,
            });
            let stale = snap.newest.borrow().as_ref().is_none_or(|(at, _)| at.elapsed() > FRESH);
            if stale && !snap.running {
                snap.running = true;
                let core = self.clone();
                tokio::spawn(async move {
                    let req = ListRequest {
                        person: Some(core.cfg.owner.clone()),
                        project: key.clone(),
                        refs: vec![],
                        offerable: false,
                    };
                    // In a task of its own, so `running` is cleared however it ends.
                    let lister = core.clone();
                    let listed = tokio::spawn(async move { lister.list(&req).await }).await;
                    let mut all = core.snapshots.0.lock().unwrap();
                    let snap = all.get_mut(&key).expect("an entry is never removed");
                    snap.running = false;
                    match listed {
                        Ok(l) => {
                            snap.newest.send_replace(Some((Instant::now(), Arc::new(l))));
                        }
                        Err(e) => tracing::error!("listing sessions failed: {e}"),
                    }
                });
            }
            snap.newest.subscribe()
        };
        let usable = |v: &Option<(Instant, Arc<Listing>)>| v.as_ref().is_some_and(|(at, _)| at.elapsed() <= STALE);
        let sessions = match tokio::time::timeout(wait, newest.wait_for(usable)).await {
            Ok(Ok(v)) => v.as_ref().map(|(_, l)| l.clone()),
            _ => None,
        };
        Online {
            machines: self.sources.iter().filter_map(|s| s.machine().map(String::from)).collect(),
            sessions,
        }
    }
}
