//! The view: every agent the owner has running, on every machine the sources
//! reach, grouped by project and group, from the listing the router shares
//! (`online`). Only the owner sees it: statuses and names are the owner's
//! agents' own words. Nothing polls: opening it shows the newest listing and
//! starts a new one when that is over a minute old, Refresh always starts
//! one (or joins the one running), and the owner's view is published again
//! whenever the listing changes, by one task, so the newest always wins.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{Local, Utc};
use tokio::sync::watch;

use super::online::{By, Listed, Need, Reader, Scope, Shared};
use super::{Core, Listing, text};
use crate::model::{Agents, FormFor, Sender, View, ViewAgent, ViewGroup, ViewProject};

/// Opening the view lists again when the newest listing is older than this.
/// A listing across machines is several processes on a small machine, and the
/// view is opened often.
const REFRESH_AFTER: Duration = Duration::from_secs(60);
pub(super) const NOT_YOURS: &str = "This shows the owner's agents.";

/// The owner's publisher, told to publish once more by a send.
#[derive(Default)]
pub(super) struct Publisher(Mutex<Option<watch::Sender<()>>>);

impl Core {
    pub(super) fn is_owner(&self, who: &Sender) -> bool {
        self.cfg.people.get(&self.cfg.owner).is_some_and(|p| p.slack == who.id)
    }

    /// `refresh`: list again whatever the newest listing's age.
    pub(super) fn view(self: &Arc<Self>, viewer: Sender, refresh: bool) {
        if !self.is_owner(&viewer) {
            let surface = self.surface.clone();
            tokio::spawn(async move {
                if let Err(e) = surface.show(&viewer.id, &View::Note(NOT_YOURS.into())).await {
                    tracing::warn!("cannot show {} the view: {e:#}", viewer.id);
                }
            });
            return;
        }
        let listing = match refresh {
            true => self.listing(Scope::Everywhere, Need::YoungerThan(Duration::ZERO), Some(By::ViewRefresh)),
            false => self.listing(Scope::Everywhere, Need::YoungerThan(REFRESH_AFTER), Some(By::ViewOpen)),
        };
        let mut publisher = self.publisher.0.lock().unwrap();
        if let Some(again) = publisher.as_ref() {
            again.send_replace(());
            return;
        }
        let (again, mut asked) = watch::channel(());
        *publisher = Some(again);
        let core = self.clone();
        tokio::spawn(async move {
            let mut listing = listing;
            loop {
                let shared = listing.borrow_and_update().clone();
                asked.borrow_and_update();
                let view = View::Agents(core.agents(&shared));
                if let Err(e) = core.surface.show(&viewer.id, &view).await {
                    tracing::warn!("cannot show the owner the view: {e:#}");
                }
                // Whatever changed while it was published is published next.
                let ended = tokio::select! {
                    r = listing.changed() => r.is_err(),
                    r = asked.changed() => r.is_err(),
                };
                if ended {
                    return;
                }
            }
        });
    }

    /// What the owner's view shows for `shared`.
    fn agents(&self, shared: &Shared) -> Agents {
        let refreshing = if shared.running.is_some() { " · refreshing" } else { "" };
        let Some(Listed { started, listing }) = &shared.newest else {
            return Agents {
                status: format!("Listing your agents on every machine{refreshing}…"),
                projects: vec![],
                empty: "Nothing listed yet: this fills in when the listing is done.".into(),
            };
        };
        let age = chrono::Duration::from_std(started.elapsed()).unwrap_or_default();
        let when = (Local::now() - age).format("%H:%M");
        let ago = if age.num_seconds() < 60 {
            "just now".to_string()
        } else {
            format!("{} ago", text::duration(age))
        };
        let mut status = vec![format!("as of {when}, {ago}")];
        status.extend(listing.machines.iter().map(|m| {
            let why = m.why.as_deref().unwrap_or("did not answer");
            match (&m.name, m.ok) {
                (Some(name), true) => format!("{name} ✓"),
                (Some(name), false) => format!("{name} ✗ {why}"),
                (None, _) => why.to_string(),
            }
        }));
        if listing.failed {
            status.push("a source did not answer".into());
        }
        Agents {
            status: format!("{}{refreshing}", status.join(" · ")),
            projects: self.projects(listing),
            empty: "No agents are running.".into(),
        }
    }

    /// Candidates and groups by project, then group, across machines: a
    /// project and a group are named the same on every machine.
    fn projects(&self, listing: &Listing) -> Vec<ViewProject> {
        #[derive(Default)]
        struct Place {
            agents: Vec<ViewAgent>,
            others: u32,
        }
        let now = Utc::now();
        let mut labels: BTreeMap<Option<&str>, &str> = BTreeMap::new();
        let mut places: BTreeMap<Option<&str>, BTreeMap<Option<&str>, Place>> = BTreeMap::new();
        // The sources list their own machine first: its label wins.
        for g in &listing.groups {
            if let Some(l) = &g.project_label {
                labels.entry(Some(&g.project)).or_insert(l);
            }
            let place = places.entry(Some(&g.project)).or_default().entry(g.group.as_deref()).or_default();
            place.others += g.others;
        }
        for c in self.reach(listing, Reader::View) {
            let conversations = self.form_conversations(c);
            let message = (!conversations.is_empty()).then(|| FormFor {
                session: c.ref_.clone(),
                conversations,
            });
            let place = places
                .entry(c.project.as_deref())
                .or_default()
                .entry(c.group.as_deref())
                .or_default();
            place.agents.push(ViewAgent {
                line: text::option_label(c, now),
                message,
            });
        }
        let named: BTreeMap<Option<&str>, &str> = places
            .keys()
            .map(|p| (*p, labels.get(p).copied().or(*p).unwrap_or("No project")))
            .collect();
        // Two projects whose folders share a name: each says its ID too.
        let mut uses: BTreeMap<&str, usize> = BTreeMap::new();
        for l in named.values() {
            *uses.entry(l).or_default() += 1;
        }
        let mut projects: Vec<ViewProject> = places
            .into_iter()
            .map(|(project, groups)| {
                let label = match (named[&project], project) {
                    (l, Some(id)) if uses[l] > 1 && l != id => format!("{l} ({id})"),
                    (l, _) => l.to_string(),
                };
                // What runs in no group goes last.
                let (named, none): (Vec<_>, Vec<_>) = groups.into_iter().partition(|(g, _)| g.is_some());
                let groups = named
                    .into_iter()
                    .chain(none)
                    .map(|(group, p)| ViewGroup {
                        label: group.unwrap_or("No group").to_string(),
                        agents: p.agents,
                        others: match p.others {
                            0 => None,
                            1 => Some("1 other session running".into()),
                            n => Some(format!("{n} other sessions running")),
                        },
                    })
                    .collect();
                ViewProject { label, groups }
            })
            .collect();
        projects.sort_by(|a, b| a.label.cmp(&b.label));
        projects
    }
}
