//! The wake rule: who gets a message, and the notices that go out.

use chrono::{DateTime, Utc};
use futures_util::future::join_all;

use super::{Core, Listing, text};
use crate::model::{Candidate, Menu, MenuGroup, MenuOption, Notice, NoticeKind, WakeRequest, WakeResult};
use crate::state::{Pending, Pick, ThreadState};

/// The message being delivered, as notices describe it. `text` and `link` are
/// for a source that cannot run `read`.
pub struct Incoming {
    pub conversation: String,
    pub thread: String,
    pub id: String,
    pub from: String,
    pub sent: DateTime<Utc>,
    pub text: String,
    pub link: String,
}

pub type Plan = Vec<(usize, Candidate)>;

/// A session that speaks in the thread: it joins its person's scope.
pub struct Author<'a> {
    pub ref_: &'a str,
    pub person: &'a str,
}

/// What kind of delivery this is.
pub struct How<'a> {
    pub author: Option<Author<'a>>,
    /// The message may be from an agent: a wake counts toward the loop limit.
    pub agent_like: bool,
    /// A `send --to` with no text points at an existing message: the refs it
    /// wakes join the scope and nobody is released.
    pub pointer: bool,
}

#[derive(Default)]
pub struct Outcome {
    /// Refs as the wakes returned them.
    pub woken: Vec<String>,
    /// One line per candidate or ref that was not woken, with the reason.
    pub left_out: Vec<String>,
    /// Refs whose wake errored: the only ones worth retrying.
    pub failed: Vec<String>,
}

impl Core {
    /// The one way a message reaches sessions. Per person, `picks` say which
    /// refs were chosen (which replaces that person's scope) or that the rule
    /// runs. Each ref is woken once however many picks name it, the scope and
    /// `woken` record the ref each wake returned, and `pending` and `streak`
    /// move the same way for every caller. In a paused thread nothing is woken:
    /// the choice still takes the scope and the message waits in `pending` with
    /// its picks for Continue. The caller holds the thread's lock and saves `st`.
    pub(super) async fn deliver(
        &self,
        st: &mut ThreadState,
        picks: &[(String, Pick)],
        listing: &Listing,
        inc: &Incoming,
        how: How<'_>,
    ) -> Outcome {
        let How {
            author,
            agent_like,
            pointer,
        } = how;
        let paused = st.paused;
        let mut plan = Plan::new();
        let mut asked_by: Vec<(String, String)> = vec![]; // ref -> person
        let mut out = Outcome::default();
        for (person, pick) in picks {
            let (p, left) = match pick {
                Pick::Rule => self.plan_rule(st, listing, person, author.as_ref().map(|a| a.ref_)),
                Pick::Refs(refs) => self.plan_choice(listing, refs),
            };
            out.left_out.extend(left);
            for (i, c) in p {
                if !plan.iter().any(|(_, o)| o.ref_ == c.ref_) {
                    asked_by.push((c.ref_.clone(), person.clone()));
                    plan.push((i, c));
                }
            }
        }

        let results = if paused { vec![] } else { self.wake_all(&plan, inc).await };
        // The ref a wake returned can differ from the one asked (a source that
        // opens a new thread per wake).
        let mut returned: Vec<(String, String, String)> = vec![]; // person, asked, returned
        for ((c, r), (_, person)) in results.into_iter().zip(&asked_by) {
            if r.woken {
                out.woken.push(r.ref_.clone());
                returned.push((person.clone(), c.ref_.clone(), r.ref_));
            } else {
                out.failed.push(c.ref_.clone());
                out.left_out
                    .push(format!("{}: not woken ({})", text::label(&c), text::clean(&r.detail)));
            }
        }

        let mut prior_woken: Vec<(String, Vec<String>)> = vec![];
        for (person, pick) in picks {
            let mine = returned.iter().filter(|(p, ..)| p == person);
            let mut woken: Vec<String> = mine.clone().map(|(.., ret)| ret.clone()).collect();
            if let Pick::Refs(refs) = pick {
                if !pointer {
                    prior_woken.push((person.clone(), st.woken_of(person).to_vec()));
                }
                let mut scope: Vec<String> = vec![];
                for r in refs {
                    let r = returned
                        .iter()
                        .find(|(p, asked, _)| p == person && asked == r)
                        .map_or(r, |(.., ret)| ret);
                    if !scope.contains(r) {
                        scope.push(r.clone());
                    }
                }
                if pointer {
                    let have = st.scope.entry(person.clone()).or_default();
                    for r in scope {
                        if !have.contains(&r) {
                            have.push(r);
                        }
                    }
                    for r in st.woken_of(person) {
                        if !woken.contains(r) {
                            woken.push(r.clone());
                        }
                    }
                } else {
                    st.scope.insert(person.clone(), scope);
                }
            } else {
                for (_, asked, ret) in mine.clone().filter(|(_, a, r)| a != r) {
                    let scope = st.scope.entry(person.clone()).or_default();
                    match scope.iter().position(|s| s == asked) {
                        Some(pos) => scope[pos] = ret.clone(),
                        None => scope.push(ret.clone()),
                    }
                }
            }
            if !woken.is_empty() {
                st.woken.insert(person.clone(), woken);
            }
        }
        if let Some(a) = &author {
            let scope = st.scope.entry(a.person.to_string()).or_default();
            if !scope.iter().any(|r| r == a.ref_) {
                scope.push(a.ref_.to_string());
            }
        }
        if !paused {
            for (person, prior) in &prior_woken {
                self.release(st, inc, person, prior, listing).await;
            }
        }

        if picks.is_empty() {
            return out;
        }
        if out.woken.is_empty() {
            st.pending = Some(Pending {
                message: inc.id.clone(),
                picks: picks.iter().cloned().collect(),
            });
        } else {
            st.pending = None;
            st.explained = false;
            if agent_like {
                st.streak += 1;
                self.pause_if_needed(st).await;
            }
        }
        out
    }

    /// The rule: with a scope, the scope's candidates; without, the source's
    /// defaults. Cold candidates are left out.
    fn plan_rule(&self, st: &ThreadState, listing: &Listing, person: &str, author: Option<&str>) -> (Plan, Vec<String>) {
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
    fn plan_choice(&self, listing: &Listing, refs: &[String]) -> (Plan, Vec<String>) {
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
    async fn wake_all(&self, plan: &Plan, inc: &Incoming) -> Vec<(Candidate, WakeResult)> {
        let message = self.message_ref(&inc.conversation, &inc.id);
        join_all(plan.iter().map(|(i, c)| {
            let also = plan.iter().filter(|(_, o)| o.ref_ != c.ref_).map(|(_, o)| o.ref_.clone()).collect();
            let req = WakeRequest {
                ref_: c.ref_.clone(),
                notice: Notice {
                    kind: NoticeKind::Message,
                    message: message.clone(),
                    from: inc.from.clone(),
                    sent: inc.sent,
                    also,
                    link: Some(inc.link.clone()),
                    text: Some(inc.text.clone()),
                },
            };
            async move { (c.clone(), self.wake(*i, &req).await) }
        }))
        .await
    }

    /// After `scope[person]` changed to something non-empty: each ref woken
    /// earlier that is no longer in it hears once that the thread moved on.
    async fn release(&self, st: &mut ThreadState, inc: &Incoming, person: &str, prior: &[String], listing: &Listing) {
        let thread_message = self.message_ref(&inc.conversation, &inc.thread);
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
                    message: thread_message.clone(),
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

    /// The picker's menu: the listed sessions of `person`, one group per project
    /// (or per source group when the listing has no project), each option's value
    /// its ref. `None` when there is nothing to choose.
    pub(super) fn menu(&self, person: &str, listing: &Listing) -> Option<Menu> {
        let now = Utc::now();
        let mut groups: Vec<MenuGroup> = vec![];
        for (_, c) in listing.for_person(person) {
            let place = c.project.as_deref().or(c.group.as_deref());
            let label = match place {
                Some(p) => format!("{} · {}", self.cfg.display(person), text::clean(p)),
                None => self.cfg.display(person),
            };
            let option = MenuOption {
                value: c.ref_.clone(),
                label: text::option_label(c, now),
            };
            match groups.iter_mut().find(|g| g.label == label) {
                Some(g) => g.options.push(option),
                None => groups.push(MenuGroup {
                    label,
                    options: vec![option],
                }),
            }
        }
        (!groups.is_empty()).then(|| Menu {
            groups,
            button: "Wake".into(),
        })
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
    if listing.failed || listing.unreachable.iter().any(|u| u == r) {
        format!("`{r}`: not checkable (unreachable)")
    } else {
        format!("`{r}`: no longer running")
    }
}
