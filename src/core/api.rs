//! The client's three requests: `read`, `send` and `list`.

use anyhow::{Context, Result, bail};
use chrono::Utc;

use super::deliver::{Author, How, Incoming};
use super::{Core, Listing};
use crate::config::Channel;
use crate::model::{CallerKind, IdentifyResult, ListRequest, Msg, PostRequest};
use crate::proto::{ListOut, MessageOut, PersonOut, ReadOut, SendOut, Target, WaitingOut};
use crate::state::{Pick, ThreadState};

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
                ref_: Some(self.session_ref(id)?),
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

    fn session_ref(&self, id: &IdentifyResult) -> Result<String> {
        let source = self.sources.iter().find(|s| Some(s.name()) == id.source.as_deref());
        let named = id.project.as_deref().zip(id.session.as_deref());
        source
            .zip(named)
            .and_then(|(s, (project, session))| s.session_ref(project, session))
            .context("the daemon cannot name this session: its project or source is not in the daemon's config, or the client and the daemon are different builds (restart the daemon)")
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

        let mut _held = None;
        let mut st = match &requested {
            Some(m) => {
                _held = Some(self.lock(&conv, &m.thread).await);
                self.store
                    .load_thread(&conv, &m.thread)?
                    .unwrap_or_else(|| ThreadState::new(&conv, &m.thread))
            }
            None => ThreadState::new(&conv, ""),
        };
        if !to.is_empty() && st.paused {
            bail!(
                "this thread is paused after {} messages in a row that may be from agents; a person presses Continue on the thread",
                super::events::LOOP_LIMIT
            );
        }

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
            listing = self.list_for(&persons, project.clone(), refs).await;
            for w in &words {
                if listing.resolve(w).is_none() {
                    bail!(
                        "{} Use a ref or short ID from `meridian-comms list`, or @person.",
                        self.why_not(&listing, w).await
                    );
                }
            }
        }

        // Post.
        let (message_id, from, sent, text_out) = match (&text, &requested) {
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
                    menu: None,
                    plain: false,
                };
                let posted = self.surface.post(req).await?;
                if requested.is_none() {
                    // The thread exists now: a reply can arrive while the wakes run. Take
                    // its lock and write its file before waking, so the reply waits its
                    // turn and finds the scope.
                    _held = Some(self.lock(&conv, &posted.id).await);
                    st = self
                        .store
                        .load_thread(&conv, &posted.id)?
                        .unwrap_or_else(|| ThreadState::new(&conv, &posted.id));
                    self.store.save_thread(&st)?;
                }
                (posted.id, who.label(), Utc::now(), body.clone())
            }
            (None, Some(m)) => {
                let from = self
                    .cfg
                    .person_by_slack(&m.sender.id)
                    .map(|(n, _)| self.cfg.display(n))
                    .unwrap_or_else(|| format!("{}-agents", self.cfg.owner));
                (m.id.clone(), from, m.time, m.text.clone())
            }
            (None, None) => unreachable!("a channel target requires text"),
        };
        let message = self.message_ref(&conv, &message_id);
        let inc = Incoming {
            conversation: conv.clone(),
            thread: st.thread.clone(),
            link: self.surface.link(&conv, &message_id, &st.thread),
            id: message_id,
            from,
            sent,
            text: text_out,
        };

        // `@person` runs the rule for that person; a ref or short ID is a choice
        // for the person who owns it. A choice for a person takes the place of
        // the rule for that person.
        let mut picks: Vec<(String, Pick)> = vec![];
        for w in &words {
            let (_, c) = listing.resolve(w).expect("checked before posting");
            match picks.iter_mut().find(|(p, _)| *p == c.person) {
                Some((_, Pick::Refs(refs))) => refs.push(c.ref_.clone()),
                _ => picks.push((c.person.clone(), Pick::Refs(vec![c.ref_.clone()]))),
            }
        }
        for p in &persons {
            if !picks.iter().any(|(q, _)| q == p) {
                picks.push((p.clone(), Pick::Rule));
            }
        }
        let author = who.ref_.as_deref().map(|r| Author {
            ref_: r,
            person: &who.person,
        });
        let was_paused = st.paused;
        let how = How {
            author,
            agent_like: true,
            pointer: text.is_none(),
            chooser: &who.person,
        };
        let out = self.deliver(&mut st, &picks, &listing, &inc, how).await;
        self.store.save_thread(&st)?;

        let mut not_woken = out.left_out;
        if !to.is_empty() && out.woken.is_empty() && not_woken.is_empty() {
            not_woken.push("no session to wake: none is in this thread's scope or marked default. Choose one with --to <ref> (see `meridian-comms list`)".into());
        }
        Ok(SendOut {
            retry: (!out.failed.is_empty()).then(|| format!("meridian-comms send {message} --to {}", out.failed.join(" --to "))),
            message,
            link: text.is_some().then_some(inc.link),
            posted: text.is_some(),
            woken: out.woken,
            not_woken,
            paused: st.paused && !was_paused,
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
                    offerable: false,
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
        // Messages nobody was woken for and no source's inbox holds, in this
        // project's channels and the channels with no project, for the caller's
        // person. A filed message is found where its source's sessions look.
        let mut waiting: Vec<WaitingOut> = self
            .store
            .all_threads()
            .into_iter()
            .filter(|st| st.filed.is_none())
            .filter_map(|st| {
                let p = st.pending?;
                let person = p.picks.contains_key(&who.person).then(|| who.person.clone())?;
                let ch = self.cfg.channel(&st.conversation)?;
                let mine = project.as_ref().is_none_or(|pr| ch.project.as_ref().is_none_or(|cp| cp == pr));
                mine.then(|| WaitingOut {
                    message: self.message_ref(&st.conversation, &p.message),
                    person,
                    time: ts_secs(&p.message),
                })
            })
            .collect();
        waiting.sort_by_key(|w| w.time);
        Ok(ListOut { project, people, waiting })
    }
}
