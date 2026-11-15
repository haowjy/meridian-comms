//! The client's three requests: `read`, `send` and `list`.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use chrono::Utc;

use super::deliver::{Delivery, Incoming, Plan};
use super::{Core, Listing};
use crate::config::Channel;
use crate::model::{CallerKind, IdentifyResult, ListRequest, Msg, PostRequest};
use crate::proto::{ListOut, MessageOut, PersonOut, ReadOut, SendOut, Target, WaitingOut};
use crate::state::ThreadState;

/// The caller after `identify`: a session, or the owner at a terminal.
pub struct Who {
    ref_: Option<String>,
    person: String,
    project: Option<String>,
    may_post: bool,
}

impl Who {
    fn label(&self) -> String {
        match &self.ref_ {
            Some(r) => format!("{}'s session `{r}`", self.person),
            None => format!("{} at a terminal", self.person),
        }
    }
}

fn ts_secs(id: &str) -> chrono::DateTime<Utc> {
    id.split('.')
        .next()
        .and_then(|s| s.parse().ok())
        .and_then(|s| chrono::DateTime::from_timestamp(s, 0))
        .unwrap_or_default()
}

impl Core {
    pub fn who(&self, id: &IdentifyResult) -> Result<Who> {
        match id.caller {
            CallerKind::Unlisted => bail!("this session's project is not in the config, so it cannot use meridian-comms"),
            CallerKind::Session => Ok(Who {
                ref_: id.ref_.clone(),
                person: id.person.clone().unwrap_or_else(|| self.cfg.owner.clone()),
                project: id.project.clone(),
                may_post: id.may_post,
            }),
            CallerKind::None => Ok(Who {
                ref_: None,
                person: self.cfg.owner.clone(),
                project: None,
                may_post: true,
            }),
        }
    }

    /// A session reads its own project's channels and the channels with no project.
    fn can_see(who: &Who, ch: &Channel) -> bool {
        match (&who.project, &ch.project) {
            (Some(mine), Some(theirs)) => mine == theirs,
            _ => true,
        }
    }

    fn resolve_channel(&self, who: &Who, name: &str) -> Result<(String, &Channel)> {
        let Some((conv, ch)) = self.cfg.channel_by_name(name) else {
            bail!("no configured channel is named {name:?}")
        };
        if !Self::can_see(who, ch) {
            bail!("channel {name:?} belongs to another project");
        }
        Ok((conv.clone(), ch))
    }

    fn message_out(&self, m: &Msg) -> MessageOut {
        let person = self.cfg.person_by_slack(&m.sender.id).map(|(n, _)| n.clone());
        let bot = m.sender.id == self.bot;
        let outsider = person.is_none() && !bot;
        MessageOut {
            message: self.message_ref(&m.conversation, &m.id),
            from: match (&person, bot) {
                (Some(p), _) => self.cfg.display(p),
                (None, true) => format!("{}-agents (bot)", self.cfg.owner),
                (None, false) => "someone outside the directory".into(),
            },
            person,
            bot,
            outsider,
            text: (!outsider).then(|| m.text.clone()),
            time: m.time,
            reply_count: m.reply_count,
        }
    }

    pub async fn read(&self, who: &Who, target: Target, limit: Option<usize>) -> Result<ReadOut> {
        match target {
            Target::Channel(name) => {
                let (conv, ch) = self.resolve_channel(who, &name)?;
                let msgs = self.surface.recent(&conv, None, Some(limit.unwrap_or(20))).await?;
                Ok(ReadOut {
                    channel: ch.name.clone(),
                    thread: None,
                    requested: None,
                    messages: msgs.iter().map(|m| self.message_out(m)).collect(),
                    omitted: 0,
                })
            }
            Target::Message(r) => {
                let (conv, id) = self.parse_message_ref(&r)?;
                let ch = self.cfg.channel(&conv).expect("parsed from the config");
                if !Self::can_see(who, ch) {
                    bail!("channel {:?} belongs to another project", ch.name);
                }
                let mut msgs = self
                    .surface
                    .thread(&conv, &id, None)
                    .await
                    .with_context(|| format!("cannot read message {r}"))?;
                let Some(root) = msgs.first().map(|m| m.thread.clone()) else {
                    bail!("message {r} not found")
                };
                let keep = limit.unwrap_or(30).max(2);
                let omitted = msgs.len().saturating_sub(keep);
                if omitted > 0 {
                    // The first message and the latest ones.
                    msgs.drain(1..=omitted);
                }
                Ok(ReadOut {
                    channel: ch.name.clone(),
                    thread: Some(self.message_ref(&conv, &root)),
                    requested: Some(r),
                    messages: msgs.iter().map(|m| self.message_out(m)).collect(),
                    omitted,
                })
            }
        }
    }

    pub async fn send(&self, who: &Who, target: Target, to: Vec<String>, text: Option<String>) -> Result<SendOut> {
        if !who.may_post {
            bail!("only a primary session, or the owner at a terminal, may post; a child spawn reads and reports to its parent");
        }
        if text.is_none() && to.is_empty() {
            bail!("nothing to send: give --message (or stdin), or --to to wake someone about an existing message");
        }
        let owner = self.cfg.owner.clone();

        // Resolve where it goes.
        let (conv, ch, requested): (String, &Channel, Option<Msg>) = match &target {
            Target::Channel(name) => {
                let (conv, ch) = self.resolve_channel(who, name)?;
                if text.is_none() {
                    bail!("opening a thread needs --message");
                }
                if ch.project.is_none() && !ch.post {
                    bail!("channel {name:?} has no project and does not allow sessions to open threads (post = true)");
                }
                (conv, ch, None)
            }
            Target::Message(r) => {
                let (conv, id) = self.parse_message_ref(r)?;
                let ch = self.cfg.channel(&conv).expect("parsed from the config");
                if !Self::can_see(who, ch) {
                    bail!("channel {:?} belongs to another project", ch.name);
                }
                let msgs = self.surface.thread(&conv, &id, None).await?;
                let Some(msg) = msgs.into_iter().find(|m| m.id == id) else {
                    bail!("message {r} not found")
                };
                (conv, ch, Some(msg))
            }
        };
        let project = ch.project.clone();

        // Who `--to` names, before anything is posted.
        let mut persons = vec![];
        let mut words = vec![];
        for w in &to {
            match w.strip_prefix('@') {
                Some(p) if self.cfg.people.contains_key(p) => persons.push(p.to_string()),
                Some(p) => bail!("{p:?} is not in the directory; `meridian-comms list` names the people"),
                None => words.push(w.clone()),
            }
        }

        let _guard = match &requested {
            Some(m) => Some(self.lock(&conv, &m.thread).await),
            None => None,
        };
        let mut st = match &requested {
            Some(m) => self
                .store
                .load_thread(&conv, &m.thread)
                .unwrap_or_else(|| ThreadState::new(&conv, &m.thread)),
            None => ThreadState::new(&conv, ""),
        };
        if !to.is_empty() && st.paused {
            bail!(
                "this thread is paused after {} messages in a row that may be from agents; a person presses Continue in Slack",
                super::events::LOOP_LIMIT
            );
        }
        let prior: BTreeMap<String, Vec<String>> = st.woken.clone();

        let mut listing = Listing::default();
        if !to.is_empty() {
            let refs: Vec<String> = st
                .scope
                .values()
                .chain(st.woken.values())
                .flatten()
                .chain(&words)
                .cloned()
                .collect();
            listing = self
                .list(&ListRequest {
                    person: Some(owner.clone()),
                    project: project.clone(),
                    refs,
                })
                .await;
            for p in persons.iter().filter(|p| **p != owner) {
                let l = self
                    .list(&ListRequest {
                        person: Some(p.clone()),
                        project: project.clone(),
                        refs: vec![],
                    })
                    .await;
                for (i, c) in l.candidates {
                    if listing.find(&c.ref_).is_none() {
                        listing.candidates.push((i, c));
                    }
                }
                listing.unreachable.extend(l.unreachable);
            }
            for w in &words {
                if listing.resolve(w).is_none() {
                    bail!("{w:?} matches no running session: use a ref or short ID from `meridian-comms list`, or @person");
                }
            }
        }

        // Post.
        let mut link = None;
        let (message_id, from, sent) = match (&text, &requested) {
            (Some(body), _) => {
                let to_part = if to.is_empty() {
                    String::new()
                } else {
                    format!(" to {}", to.join(", "))
                };
                let req = PostRequest {
                    conversation: conv.clone(),
                    thread: requested.as_ref().map(|m| m.thread.clone()),
                    text: format!("From {}{to_part}:\n{body}", who.label()),
                    button: None,
                };
                let posted = self.surface.post(req).await?;
                link = Some(posted.link);
                if requested.is_none() {
                    st.thread = posted.id.clone();
                }
                (posted.id, who.label(), Utc::now())
            }
            (None, Some(m)) => {
                let from = self
                    .cfg
                    .person_by_slack(&m.sender.id)
                    .map(|(n, _)| self.cfg.display(n))
                    .unwrap_or_else(|| format!("{}-agents", self.cfg.owner));
                (m.id.clone(), from, m.time)
            }
            (None, None) => unreachable!("a channel target requires text"),
        };
        let thread_message = self.message_ref(&conv, &st.thread);
        let message = self.message_ref(&conv, &message_id);
        let inc = Incoming {
            conversation: conv.clone(),
            id: message_id.clone(),
            from,
            sent,
        };

        // A session that speaks in a thread joins its scope.
        let mut changed: Vec<String> = vec![];
        if let Some(r) = &who.ref_ {
            let scope = st.scope.entry(who.person.clone()).or_default();
            if !scope.contains(r) {
                scope.push(r.clone());
                changed.push(who.person.clone());
            }
        }

        // `--to`: the rule for each `@person`, a wake for each ref.
        let mut all_woken: Vec<String> = vec![];
        let mut not_woken: Vec<String> = vec![];
        let mut delivered: BTreeMap<String, Vec<String>> = BTreeMap::new();
        if !to.is_empty() {
            let mut plans: Vec<(String, Plan, Vec<String>)> = vec![];
            for p in &persons {
                let (plan, left) = self.plan_rule(&st, &listing, p, who.ref_.as_deref());
                plans.push((p.clone(), plan, left));
            }
            let mut chosen: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for w in &words {
                let (_, c) = listing.resolve(w).expect("checked before posting");
                chosen.entry(c.person.clone()).or_default().push(c.ref_.clone());
            }
            for (p, refs) in chosen {
                let (plan, left) = self.plan_choice(&listing, &refs);
                plans.push((p, plan, left));
            }
            for (p, plan, left) in plans {
                let Delivery { woken, left_out } = self.wake_all(plan, left, &inc).await;
                not_woken.extend(left_out);
                if !woken.is_empty() {
                    let scope = st.scope.entry(p.clone()).or_default();
                    for r in &woken {
                        if !scope.contains(r) {
                            scope.push(r.clone());
                        }
                    }
                    if !changed.contains(&p) {
                        changed.push(p.clone());
                    }
                    all_woken.extend(woken.iter().cloned());
                    delivered.entry(p).or_default().extend(woken);
                }
            }
        }

        if !to.is_empty() && all_woken.is_empty() && not_woken.is_empty() {
            not_woken.push("no session to wake: none is in this thread's scope or marked default. Choose one with --to <ref> (see `meridian-comms list`)".into());
        }

        // Release what the scope change left behind, then record who was woken.
        let needs_release = changed
            .iter()
            .any(|p| prior.get(p).is_some_and(|w| w.iter().any(|r| !st.scope_of(p).contains(r))));
        if needs_release && listing.candidates.is_empty() {
            let refs = prior.values().flatten().cloned().collect();
            listing = self
                .list(&ListRequest {
                    person: Some(owner.clone()),
                    project: project.clone(),
                    refs,
                })
                .await;
        }
        for p in &changed {
            let old = prior.get(p).cloned().unwrap_or_default();
            self.release(&mut st, &thread_message, p, &old, &listing).await;
        }
        for (p, woken) in delivered {
            st.woken.insert(p, woken);
        }

        let mut paused = false;
        if !to.is_empty() && !all_woken.is_empty() {
            st.pending = None;
            st.streak += 1;
            self.pause_if_needed(&mut st).await;
            paused = st.paused;
        }
        self.store.save_thread(&st)?;

        let failed = !to.is_empty() && (all_woken.is_empty() || !not_woken.is_empty());
        Ok(SendOut {
            retry: failed.then(|| format!("meridian-comms send {message} --to {}", to.join(" --to "))),
            message,
            link,
            posted: text.is_some(),
            woken: all_woken,
            not_woken,
            paused,
        })
    }

    pub async fn list_people(&self, who: &Who, project: Option<String>) -> Result<ListOut> {
        let project = project.or_else(|| who.project.clone());
        if let (Some(mine), Some(asked)) = (&who.project, &project)
            && mine != asked
        {
            bail!("a session lists its own project ({mine}) only");
        }
        let mut people = vec![];
        for (name, p) in &self.cfg.people {
            let l = self
                .list(&ListRequest {
                    person: Some(name.clone()),
                    project: project.clone(),
                    refs: vec![],
                })
                .await;
            let mut candidates: Vec<_> = l.candidates.into_iter().map(|(_, c)| c).filter(|c| c.person == *name).collect();
            candidates.sort_by(|a, b| a.ref_.cmp(&b.ref_));
            people.push(PersonOut {
                name: name.clone(),
                note: p.note.clone(),
                candidates,
                unreachable: l.unreachable,
            });
        }
        // Messages nobody was woken for, in this project's channels and the
        // channels with no project, for the caller's person.
        let mut waiting: Vec<WaitingOut> = self
            .store
            .all_threads()
            .into_iter()
            .filter_map(|st| {
                let p = st.pending?;
                let ch = self.cfg.channel(&st.conversation)?;
                let mine = project.as_ref().is_none_or(|pr| ch.project.as_ref().is_none_or(|cp| cp == pr));
                (mine && p.person == who.person).then(|| WaitingOut {
                    message: self.message_ref(&st.conversation, &p.message),
                    person: p.person,
                    time: ts_secs(&p.message),
                })
            })
            .collect();
        waiting.sort_by_key(|w| w.time);
        Ok(ListOut { project, people, waiting })
    }
}
