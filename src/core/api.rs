//! The client's requests: `read`, `send`, `post` and `list`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use chrono::Utc;

use super::deliver::{Author, How, Incoming};
use super::online::{By, Reader};
use super::{Core, Listing};
use crate::config::Channel;
use crate::model::{CallerKind, FailKind, Failure, IdentifyResult, Msg, PostRequest, Posted};
use crate::proto::{ListOut, MessageOut, PersonOut, PostTo, ReadOut, SendOut, Target, WaitingOut};
use crate::state::{Pick, ThreadState};

/// A `send` resolved up to the post: nothing has gone out yet.
struct Prepared {
    conv: String,
    requested: Option<Msg>,
    to: Vec<String>,
    text: Option<String>,
    persons: Vec<String>,
    words: Vec<String>,
    listing: Listing,
    held: Option<tokio::sync::OwnedMutexGuard<()>>,
    st: ThreadState,
    post: Option<PostRequest>,
}

/// The caller after `identify`: a session, or the owner at a terminal.
pub struct Who {
    ref_: Option<String>,
    /// How people see `ref_`.
    shown: String,
    person: String,
    project: Option<String>,
    may_post: bool,
}

impl Who {
    fn label(&self) -> String {
        match &self.ref_ {
            Some(_) => format!("{}'s session `{}`", self.person, self.shown),
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
            CallerKind::Unlisted => bail!(Failure::new(
                FailKind::NotAllowed,
                "this session's project is not in the config, so it cannot use meridian-comms"
            )),
            CallerKind::Session => {
                let r = self.session_ref(id)?;
                Ok(Who {
                    shown: self.show(&Listing::default(), &r),
                    ref_: Some(r),
                    person: id.person.clone().unwrap_or_else(|| self.cfg.owner.clone()),
                    project: id.project.clone(),
                    may_post: id.may_post,
                })
            }
            CallerKind::None => Ok(Who {
                ref_: None,
                shown: String::new(),
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
            bail!(Failure::new(FailKind::NotFound, format!("no configured channel is named {name:?}")))
        };
        if !Self::can_see(who, ch) {
            bail!(another_project(name));
        }
        Ok((conv.clone(), ch))
    }

    fn message_out(&self, m: &Msg, authors: &BTreeMap<String, String>) -> MessageOut {
        let person = self.author(authors, m);
        let bot = person.is_none() && m.sender.id == self.bot;
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
                    messages: msgs
                        .iter()
                        .map(|m| {
                            // Each top-level message is its own thread, with its own file.
                            let authors = if m.sender.id == self.bot {
                                self.authors(&conv, &m.thread)
                            } else {
                                BTreeMap::new()
                            };
                            self.message_out(m, &authors)
                        })
                        .collect(),
                    omitted: 0,
                })
            }
            Target::Message(r) => {
                let (conv, id) = self.parse_message_ref(&r)?;
                let ch = self.cfg.channel(&conv).expect("parsed from the config");
                if !Self::can_see(who, ch) {
                    bail!(another_project(&ch.name));
                }
                let mut msgs = self
                    .surface
                    .thread(&conv, &id, None)
                    .await
                    .with_context(|| format!("cannot read message {r}"))?;
                let Some(root) = msgs.first().map(|m| m.thread.clone()) else {
                    bail!(not_found(&r))
                };
                let authors = self.authors(&conv, &root);
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
                    messages: msgs.iter().map(|m| self.message_out(m, &authors)).collect(),
                    omitted,
                })
            }
        }
    }

    /// Posts `text` (naming `mention`, a person as the surface notifies them)
    /// and wakes whom `to` names. Everything is resolved before the post, and
    /// any error after it is `MaybeSent`.
    pub async fn send(self: &Arc<Self>, who: &Who, target: Target, to: Vec<String>, text: Option<String>, mention: Option<String>) -> Result<SendOut> {
        let prep = self.prepare_send(who, target, to, text, mention).await?;
        let posted = match &prep.post {
            Some(req) => Some(self.surface.post(req.clone()).await?),
            None => None,
        };
        self.finish_send(who, prep, posted)
            .await
            .map_err(|e| Failure::new(FailKind::MaybeSent, format!("the post or wake went out, then: {e:#}")).into())
    }

    async fn prepare_send(
        self: &Arc<Self>,
        who: &Who,
        target: Target,
        to: Vec<String>,
        text: Option<String>,
        mention: Option<String>,
    ) -> Result<Prepared> {
        Self::may_post(who)?;
        if text.is_none() && to.is_empty() {
            bail!(Failure::new(
                FailKind::Invalid,
                "nothing to send: give --message (or stdin), or --to to wake someone about an existing message"
            ));
        }
        // Resolve where it goes.
        let (conv, ch, requested): (String, &Channel, Option<Msg>) = match &target {
            Target::Channel(name) => {
                let (conv, ch) = self.resolve_channel(who, name)?;
                if text.is_none() {
                    bail!(Failure::new(FailKind::Invalid, "opening a thread needs --message"));
                }
                if ch.project.is_none() && !ch.post {
                    bail!(Failure::new(
                        FailKind::NotAllowed,
                        format!("channel {name:?} has no project and does not allow sessions to open threads (post = true)")
                    ));
                }
                (conv, ch, None)
            }
            Target::Message(r) => {
                let (conv, id) = self.parse_message_ref(r)?;
                let ch = self.cfg.channel(&conv).expect("parsed from the config");
                if !Self::can_see(who, ch) {
                    bail!(another_project(&ch.name));
                }
                let msgs = self.surface.thread(&conv, &id, None).await?;
                let Some(msg) = msgs.into_iter().find(|m| m.id == id) else {
                    bail!(not_found(r))
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
                Some(p) => bail!(Failure::new(
                    FailKind::NotFound,
                    format!("{p:?} is not in the directory; `meridian-comms list` names the people")
                )),
                None => words.push(w.clone()),
            }
        }

        let mut held = None;
        let st = match &requested {
            Some(m) => {
                held = Some(self.lock(&conv, &m.thread).await);
                self.store
                    .load_thread(&conv, &m.thread)?
                    .unwrap_or_else(|| ThreadState::new(&conv, &m.thread))
            }
            None => ThreadState::new(&conv, ""),
        };
        if !to.is_empty() && st.paused {
            bail!(Failure::new(
                FailKind::NotAllowed,
                format!(
                    "this thread is paused after {} messages in a row that may be from agents; a person presses Continue on the thread",
                    super::events::LOOP_LIMIT
                )
            ));
        }

        let mut listing = Listing::default();
        if !to.is_empty() {
            let refs: Vec<String> = st.scope.values().chain(st.woken.values()).flatten().cloned().collect();
            let reader = Reader::Thread {
                project: project.as_deref(),
                refs: &refs,
            };
            listing = self.reach(&*self.fresh(By::Client).await, reader);
            // A word that names nothing is reported after one listing started
            // since: a session may just have started.
            if words.iter().any(|w| listing.resolve(w).is_none() && !self.unchecked(&listing, w)) {
                listing = self.reach(&*self.listed_after(Instant::now(), By::Client).await, reader);
            }
            for w in &words {
                if listing.resolve(w).is_none() {
                    bail!(Failure::new(
                        FailKind::NotFound,
                        format!(
                            "{} Use a ref or short ID from `meridian-comms list`, or @person.",
                            self.why_not(&listing, w)
                        )
                    ));
                }
            }
        }

        let post = text.as_ref().map(|body| {
            let named: Vec<&str> = mention.iter().chain(&to).map(String::as_str).collect();
            let to_part = if named.is_empty() {
                String::new()
            } else {
                format!(" to {}", named.join(", "))
            };
            PostRequest {
                conversation: conv.clone(),
                thread: requested.as_ref().map(|m| m.thread.clone()),
                text: format!("From {}{to_part}:\n{body}", who.label()),
                menu: None,
                plain: false,
                by: None,
            }
        });
        Ok(Prepared {
            conv,
            requested,
            to,
            text,
            persons,
            words,
            listing,
            held,
            st,
            post,
        })
    }

    /// The wakes and the thread file, after the post (if any) went out.
    async fn finish_send(&self, who: &Who, prep: Prepared, posted: Option<Posted>) -> Result<SendOut> {
        let Prepared {
            conv,
            requested,
            to,
            text,
            persons,
            words,
            listing,
            mut held,
            mut st,
            ..
        } = prep;
        let (message_id, from, sent, text_out) = match (posted, &text, &requested) {
            (Some(posted), Some(body), _) => {
                if requested.is_none() {
                    // The thread exists now: a reply can arrive while the wakes run. Take
                    // its lock and write its file before waking, so the reply waits its
                    // turn and finds the scope.
                    held = Some(self.lock(&conv, &posted.id).await);
                    st = self
                        .store
                        .load_thread(&conv, &posted.id)?
                        .unwrap_or_else(|| ThreadState::new(&conv, &posted.id));
                    self.store.save_thread(&st)?;
                }
                (posted.id, who.label(), Utc::now(), body.clone())
            }
            (None, None, Some(m)) => {
                let from = self
                    .author(&st.authors, m)
                    .map(|n| self.cfg.display(&n))
                    .unwrap_or_else(|| format!("{}-agents", self.cfg.owner));
                (m.id.clone(), from, m.time, m.text.clone())
            }
            _ => unreachable!("a post exactly when there is text, and a target message when there is none"),
        };
        let _held = held;
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

    fn may_post(who: &Who) -> Result<()> {
        if !who.may_post {
            bail!(Failure::new(
                FailKind::NotAllowed,
                "only a primary session, or the owner at a terminal, may post; a child spawn reads and reports to its parent"
            ));
        }
        Ok(())
    }

    /// Posts `text` for a person or in a thread, and wakes nobody. For a
    /// person it opens a thread naming them in the caller's channel.
    pub async fn post_to(self: &Arc<Self>, who: &Who, to: PostTo, text: String) -> Result<SendOut> {
        Self::may_post(who)?;
        if text.trim().is_empty() {
            bail!(Failure::new(FailKind::Invalid, "nothing to send: the message text is empty"));
        }
        let name = match to {
            PostTo::Thread(r) => return self.send(who, Target::Message(r), vec![], Some(text), None).await,
            PostTo::Person(name) => name,
        };
        let Some(person) = self.cfg.people.get(&name) else {
            bail!(Failure::new(
                FailKind::NotFound,
                format!("no person {name:?} in the directory; `meridian-comms list` names the people")
            ))
        };
        let mention = self.surface.mention(&person.slack);
        let channel = self.channel_for(who)?;
        self.send(who, Target::Channel(channel), vec![], Some(text), Some(mention)).await
    }

    /// Where a post for a person opens its thread: the caller's project's channel, else
    /// the one channel with no project and `post = true`. For the owner at a
    /// terminal, the one channel they may open threads in.
    fn channel_for(&self, who: &Who) -> Result<String> {
        let names = |keep: &dyn Fn(&Channel) -> bool| -> Vec<String> {
            self.cfg.channels().filter(|(_, c)| keep(c)).map(|(_, c)| c.name.clone()).collect()
        };
        let open = |c: &Channel| c.project.is_none() && c.post;
        let found = match &who.project {
            Some(p) => match names(&|c| c.project.as_ref() == Some(p)) {
                mine if mine.is_empty() => names(&open),
                mine => mine,
            },
            None => names(&|c| c.project.is_some() || c.post),
        };
        let config = crate::config::config_path();
        match found.as_slice() {
            [one] => Ok(one.clone()),
            [] => bail!(Failure::new(
                FailKind::NoChannel,
                match &who.project {
                    Some(p) => format!(
                        "no channel takes a new thread from project {p}: in {}, give one channel `project = \"{p}\"`, or one channel with no project `post = true`",
                        config.display()
                    ),
                    None => format!(
                        "no channel takes a new thread: in {}, give a channel a project or `post = true`",
                        config.display()
                    ),
                }
            )),
            many => bail!(Failure::new(
                FailKind::Ambiguous,
                format!(
                    "channels {} could each take a new thread: answer in an existing thread by its message ref instead, or change {} so that only one can",
                    many.join(", "),
                    config.display()
                )
            )),
        }
    }

    pub async fn list_people(self: &Arc<Self>, who: &Who, project: Option<String>) -> Result<ListOut> {
        let project = project.or_else(|| who.project.clone());
        if let (Some(mine), Some(asked)) = (&who.project, &project)
            && mine != asked
        {
            bail!("a session lists its own project ({mine}) only");
        }
        let listing = self.reach(&*self.fresh(By::Client).await, Reader::Project(project.as_deref()));
        let unreachable: Vec<String> = listing
            .machines
            .iter()
            .filter(|(_, m)| !m.ok)
            .map(|(_, m)| m.name.clone().unwrap_or_else(|| "the saved machines".into()))
            .chain((!listing.failed.is_empty()).then(|| "a source".into()))
            .collect();
        let mut people = vec![];
        for (name, p) in &self.cfg.people {
            let mut candidates: Vec<_> = listing.for_person(name).map(|(_, c)| c.clone()).collect();
            candidates.sort_by(|a, b| a.ref_.cmp(&b.ref_));
            people.push(PersonOut {
                name: name.clone(),
                note: p.note.clone(),
                candidates,
                unreachable: unreachable.clone(),
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

fn another_project(channel: &str) -> Failure {
    Failure::new(FailKind::NotAllowed, format!("channel {channel:?} belongs to another project"))
}

fn not_found(message: &str) -> Failure {
    Failure::new(FailKind::NotFound, format!("message {message} not found"))
}
