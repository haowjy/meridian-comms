//! The wake rule: who gets a message, and the notices that go out.

use chrono::{DateTime, Utc};
use futures_util::future::join_all;

use super::{Core, Listing, text};
use crate::model::{Candidate, Notice, NoticeKind, WakeRequest};
use crate::state::ThreadState;

/// The message being delivered, as notices describe it.
pub struct Incoming {
    pub conversation: String,
    pub id: String,
    pub from: String,
    pub sent: DateTime<Utc>,
}

pub type Plan = Vec<(usize, Candidate)>;

#[derive(Default)]
pub struct Delivery {
    pub woken: Vec<String>,
    /// One line per candidate or ref that was not woken, with the reason.
    pub left_out: Vec<String>,
}

impl Core {
    /// The rule: with a scope, the scope's candidates; without, the source's
    /// defaults. Cold candidates are left out.
    pub(super) fn plan_rule(&self, st: &ThreadState, listing: &Listing, person: &str, author: Option<&str>) -> (Plan, Vec<String>) {
        let now = Utc::now();
        let scope = st.scope_of(person);
        let mut picked: Vec<&(usize, Candidate)> = vec![];
        let mut left = vec![];
        if scope.is_empty() {
            picked.extend(
                listing
                    .for_person(person)
                    .filter(|(_, c)| c.default && Some(c.ref_.as_str()) != author),
            );
        } else {
            for r in scope.iter().filter(|r| Some(r.as_str()) != author) {
                match listing.find(r) {
                    Some(found) => picked.push(found),
                    None => left.push(missing(listing, r)),
                }
            }
        }
        let mut plan = Plan::new();
        for (i, c) in picked {
            if c.is_cold(now) {
                left.push(format!("{}: {}", text::label(c), text::cold_reason(c, now)));
            } else {
                plan.push((*i, c.clone()));
            }
        }
        (plan, left)
    }

    /// A choice wakes what it names, cold or not.
    pub(super) fn plan_choice(&self, listing: &Listing, refs: &[String]) -> (Plan, Vec<String>) {
        let mut plan = Plan::new();
        let mut left = vec![];
        for r in refs {
            match listing.find(r) {
                Some((i, c)) => plan.push((*i, c.clone())),
                None => left.push(missing(listing, r)),
            }
        }
        (plan, left)
    }

    /// Wakes every planned candidate at once, each notice naming the others.
    pub(super) async fn wake_all(&self, plan: Plan, left: Vec<String>, inc: &Incoming) -> Delivery {
        let message = self.message_ref(&inc.conversation, &inc.id);
        let results = join_all(plan.iter().map(|(i, c)| {
            let also = plan.iter().filter(|(_, o)| o.ref_ != c.ref_).map(|(_, o)| o.ref_.clone()).collect();
            let req = WakeRequest {
                ref_: c.ref_.clone(),
                notice: Notice {
                    kind: NoticeKind::Message,
                    message: message.clone(),
                    from: inc.from.clone(),
                    sent: inc.sent,
                    also,
                    link: None,
                    text: None,
                },
            };
            async move { (c, self.wake(*i, &req).await) }
        }))
        .await;
        let mut d = Delivery {
            woken: vec![],
            left_out: left,
        };
        for (c, r) in results {
            if r.woken {
                d.woken.push(r.ref_);
            } else {
                d.left_out
                    .push(format!("{}: not woken ({})", text::label(c), text::clean(&r.detail)));
            }
        }
        d
    }

    /// After `scope[person]` changed to something non-empty: each ref woken
    /// earlier that is no longer in it hears once that the thread moved on.
    pub(super) async fn release(&self, st: &mut ThreadState, thread_message: &str, person: &str, prior: &[String], listing: &Listing) {
        let scope = st.scope_of(person).to_vec();
        if scope.is_empty() {
            return;
        }
        let released: Vec<&String> = prior.iter().filter(|r| !scope.contains(r)).collect();
        join_all(released.iter().filter_map(|r| listing.find(r)).map(|(i, c)| {
            let req = WakeRequest {
                ref_: c.ref_.clone(),
                notice: Notice {
                    kind: NoticeKind::Released,
                    message: thread_message.to_string(),
                    from: "meridian-comms".into(),
                    sent: Utc::now(),
                    also: scope.clone(),
                    link: None,
                    text: None,
                },
            };
            async move { self.wake(*i, &req).await }
        }))
        .await;
        if let Some(w) = st.woken.get_mut(person) {
            w.retain(|r| scope.contains(r));
        }
    }

    /// The bot's post when nobody was woken (the picker, on a thread's first
    /// message): who was woken, who was left out and why, and how to choose.
    pub(super) fn explain(
        &self,
        person: &str,
        project: Option<&str>,
        listing: &Listing,
        woken: &[String],
        left_out: &[String],
        running: bool,
    ) -> String {
        let now = Utc::now();
        let mut out = String::new();
        if woken.is_empty() {
            out.push_str("Nobody was woken for this message.\n");
        } else {
            out.push_str(&format!(
                "Woken for {person}: {}\n",
                woken.iter().map(|r| format!("`{r}`")).collect::<Vec<_>>().join(", ")
            ));
        }
        for l in left_out {
            out.push_str(&format!("• left out, {l}\n"));
        }
        if running {
            let ours: Vec<_> = listing.for_person(person).collect();
            let place = project.map(|p| format!(" in {}", text::clean(p))).unwrap_or_default();
            if ours.is_empty() {
                out.push_str(&format!("No running sessions for {person}{place}.\n"));
            } else {
                out.push_str(&format!("Running sessions for {person}{place}:\n"));
                for (_, c) in ours {
                    out.push_str(&format!("• {}\n", text::candidate_line(c, now)));
                }
            }
        }
        out.push_str(
            "Reply `to: <ref>` to wake a session (`to: p41` works when the short ID is unique), or `to: all` to go back to the defaults.",
        );
        out
    }
}

fn missing(listing: &Listing, r: &str) -> String {
    if listing.unreachable.iter().any(|u| u == r) {
        format!("`{r}`: not checkable (unreachable)")
    } else {
        format!("`{r}`: no longer running")
    }
}
