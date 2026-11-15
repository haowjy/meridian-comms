//! Events from the surface: messages, clicks, and reconnects.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use futures_util::future::join_all;

use super::deliver::Incoming;
use super::{Core, ThreadKey};
use crate::config::AgentMark;
use crate::model::{Action, ListRequest, Msg, PostRequest, SurfaceEvent};
use crate::state::{Pick, ThreadState};

/// The loop limit: this many messages in a row that woke someone and may be
/// from agents pause the thread.
pub const LOOP_LIMIT: u32 = 12;
const RECOVERY_WINDOW: Duration = Duration::from_secs(7 * 24 * 3600);

pub enum Job {
    Msg(Msg),
    Action(Action),
}

impl Job {
    fn key(&self) -> ThreadKey {
        match self {
            Job::Msg(m) => (m.conversation.clone(), m.thread.clone()),
            Job::Action(a) => (a.conversation.clone(), a.thread.clone()),
        }
    }
}

/// A `to:` line opens the message: `to: p41 desk/x/p7 please rebase`. Words
/// are taken while each one names a listed session (by ref, or by a short ID
/// one session has) or is `all`; the rest is the message.
fn to_words(text: &str) -> Option<Vec<String>> {
    let head = text.get(..3)?;
    if !head.eq_ignore_ascii_case("to:") {
        return None;
    }
    Some(
        text[3..]
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|w| !w.is_empty())
            .take(12)
            .map(String::from)
            .collect(),
    )
}

fn ts_now() -> String {
    let n = Utc::now();
    format!("{}.{:06}", n.timestamp(), n.timestamp_subsec_micros())
}

impl Core {
    pub(super) async fn on_event(self: &Arc<Self>, ev: SurfaceEvent) {
        match ev {
            SurfaceEvent::Connected => self.clone().recover().await,
            SurfaceEvent::Message(m) => self.accept(m),
            SurfaceEvent::Action(a) if self.cfg.channel(&a.conversation).is_some() => self.enqueue(Job::Action(a)),
            SurfaceEvent::Action(_) => {}
        }
    }

    fn accept(self: &Arc<Self>, m: Msg) {
        if self.cfg.channel(&m.conversation).is_none() {
            return;
        }
        self.cursor.begin(&m.conversation, &m.id);
        // The bot's own posts are never handled, whatever the surface sends;
        // they still count as seen, so the cursor can pass them.
        if m.sender.id == self.bot {
            return self.finish(&m);
        }
        self.enqueue(Job::Msg(m));
    }

    fn enqueue(self: &Arc<Self>, mut job: Job) {
        let key = job.key();
        let mut queues = self.queues.lock().unwrap();
        if let Some(tx) = queues.get(&key) {
            match tx.send(job) {
                Ok(()) => return,
                Err(tokio::sync::mpsc::error::SendError(j)) => job = j, // its worker died
            }
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tx.send(job).ok();
        queues.insert(key.clone(), tx);
        drop(queues);
        let core = self.clone();
        tokio::spawn(async move {
            while let Some(job) = rx.recv().await {
                core.handle(job).await;
                // Under the same lock `enqueue` sends under, so no job is stranded.
                let mut queues = core.queues.lock().unwrap();
                if rx.is_empty() {
                    queues.remove(&key);
                    break;
                }
            }
        });
    }

    fn finish(&self, m: &Msg) {
        if let Err(e) = self.cursor.finish(&m.conversation, &m.id) {
            tracing::error!("cannot save cursor.json: {e:#}");
        }
    }

    async fn handle(&self, job: Job) {
        match job {
            Job::Msg(m) => match self.handle_message(&m).await {
                // An event that failed stays in flight: the cursor stops
                // before it, and the next recovery handles it again.
                Err(e) => tracing::error!("message {} in {}: {e:#}", m.id, m.conversation),
                Ok(()) => self.finish(&m),
            },
            Job::Action(a) => {
                if let Err(e) = self.handle_action(&a).await {
                    tracing::error!("action {} in {}: {e:#}", a.name, a.conversation);
                }
            }
        }
    }

    async fn handle_message(&self, m: &Msg) -> Result<()> {
        let _guard = self.lock(&m.conversation, &m.thread).await;
        let Some((person, pcfg)) = self.cfg.person_by_slack(&m.sender.id) else {
            return Ok(());
        };
        let mut st = match self.store.load_thread(&m.conversation, &m.thread)? {
            Some(s) => s,
            None if m.addressed => ThreadState::new(&m.conversation, &m.thread),
            None => return Ok(()),
        };
        if st.is_handled(&m.id) {
            return Ok(());
        }
        let project = self.cfg.channel(&m.conversation).and_then(|c| c.project.clone());
        let owner = self.cfg.owner.clone();
        let first = m.id == m.thread;
        let agent_like = m.sender.app || pcfg.agent != AgentMark::No;
        let inc = Incoming {
            conversation: m.conversation.clone(),
            thread: m.thread.clone(),
            id: m.id.clone(),
            from: self.cfg.display(person),
            sent: m.time,
            text: m.text.clone(),
            link: self.surface.link(&m.conversation, &m.id, &m.thread),
        };

        // One listing per event: Meridian's costs 1.4 s and 300 MB.
        let words = to_words(&m.text);
        let mut refs: Vec<String> = st.scope.values().chain(st.woken.values()).flatten().cloned().collect();
        refs.extend(words.iter().flatten().cloned());
        let listing = self
            .list(&ListRequest {
                person: Some(owner.clone()),
                project: project.clone(),
                refs,
            })
            .await;
        let prior_pending = st.pending.is_some();

        // A choice: replace the scope of each person whose refs are named.
        let mut chosen: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut all = false;
        if let Some(words) = &words {
            let mut used = 0;
            for w in words {
                if w.eq_ignore_ascii_case("all") {
                    all = true;
                } else if let Some((_, c)) = listing.resolve(w) {
                    let e = chosen.entry(c.person.clone()).or_default();
                    if !e.contains(&c.ref_) {
                        e.push(c.ref_.clone());
                    }
                } else {
                    break;
                }
                used += 1;
            }
            if used == 0 {
                let bad = words.first().map(String::as_str).unwrap_or("");
                // A source that did not answer must not turn a choice into "can't match".
                if listing.failed {
                    bail!("a source did not answer, so `to: {bad}` could not be checked");
                }
                st.mark_handled(&m.id);
                self.store.save_thread(&st)?;
                let msg = format!(
                    "I can't match it: {}\n{}",
                    listing.why_not(bad),
                    self.explain(&owner, project.as_deref(), &listing, &[], &[], true)
                );
                self.say(&m.conversation, &m.thread, msg).await;
                return Ok(());
            }
        }

        if all {
            st.scope.remove(&owner);
        }
        if !agent_like {
            st.streak = 0;
        }

        let picks: Vec<(String, Pick)> = if chosen.is_empty() {
            vec![(owner.clone(), Pick::Rule)]
        } else {
            chosen.into_iter().map(|(p, refs)| (p, Pick::Refs(refs))).collect()
        };
        let was_paused = st.paused;
        let out = self.deliver(&mut st, &picks, &listing, &inc, None, agent_like).await;

        if was_paused {
            // The message waits for Continue; the pause notice already said so.
        } else if out.woken.is_empty() {
            // The picker on a thread's first message; after that, once per run of
            // waiting messages, so a conversation nobody was woken for stays quiet.
            if first || !prior_pending {
                let text = self.explain(&owner, project.as_deref(), &listing, &[], &out.left_out, first);
                self.say(&m.conversation, &m.thread, text).await;
            }
        } else if first {
            let text = self.explain(&owner, project.as_deref(), &listing, &out.woken, &out.left_out, true);
            self.say(&m.conversation, &m.thread, text).await;
        } else if let Err(e) = self.surface.acknowledge(&m.conversation, &m.id).await {
            tracing::warn!("acknowledging {} failed: {e:#}", m.id);
        }
        st.mark_handled(&m.id);
        self.store.save_thread(&st)
    }

    pub(super) async fn pause_if_needed(&self, st: &mut ThreadState) {
        if st.streak >= LOOP_LIMIT && !st.paused {
            st.paused = true;
            if let Err(e) = self.surface.notify_pause(&st.conversation, &st.thread, LOOP_LIMIT).await {
                tracing::error!("cannot post the pause notice: {e:#}");
            }
        }
    }

    /// A post by the bot in a thread; a failure is logged, never fatal.
    pub(super) async fn say(&self, conversation: &str, thread: &str, text: String) {
        let req = PostRequest {
            conversation: conversation.into(),
            thread: Some(thread.into()),
            text,
        };
        if let Err(e) = self.surface.post(req).await {
            tracing::warn!("bot post failed: {e:#}");
        }
    }

    async fn handle_action(&self, a: &Action) -> Result<()> {
        if a.name != "continue" {
            return Ok(());
        }
        let _guard = self.lock(&a.conversation, &a.thread).await;
        // A person in the directory who is not an agent's account.
        match self.cfg.person_by_slack(&a.sender.id) {
            Some((_, p)) if p.agent != AgentMark::Always => {}
            _ => return Ok(()),
        }
        let Some(mut st) = self.store.load_thread(&a.conversation, &a.thread)? else {
            return Ok(());
        };
        let was_paused = st.paused;
        st.streak = 0;
        st.paused = false;
        if let (true, Some(p)) = (was_paused, st.pending.clone()) {
            let thread = self
                .surface
                .thread(&a.conversation, &p.message, None)
                .await
                .context("cannot read the waiting message")?;
            if let Some(msg) = thread.iter().find(|m| m.id == p.message) {
                let author = self
                    .cfg
                    .person_by_slack(&msg.sender.id)
                    .map(|(n, _)| self.cfg.display(n))
                    .unwrap_or_default();
                let inc = Incoming {
                    conversation: a.conversation.clone(),
                    thread: a.thread.clone(),
                    id: msg.id.clone(),
                    from: author,
                    sent: msg.time,
                    text: msg.text.clone(),
                    link: self.surface.link(&a.conversation, &msg.id, &a.thread),
                };
                let project = self.cfg.channel(&a.conversation).and_then(|c| c.project.clone());
                let persons: Vec<String> = p.picks.keys().cloned().collect();
                let mut refs: Vec<String> = st.scope.values().chain(st.woken.values()).flatten().cloned().collect();
                refs.extend(p.picks.values().flat_map(|k| match k {
                    Pick::Refs(r) => r.clone(),
                    Pick::Rule => vec![],
                }));
                let listing = self.list_for(&persons, project.clone(), refs).await;
                let picks: Vec<(String, Pick)> = p.picks.into_iter().collect();
                let out = self.deliver(&mut st, &picks, &listing, &inc, None, false).await;
                if out.woken.is_empty() {
                    let text = self.explain(&persons[0], project.as_deref(), &listing, &[], &out.left_out, false);
                    self.say(&a.conversation, &a.thread, text).await;
                } else if let Err(e) = self.surface.acknowledge(&a.conversation, &p.message).await {
                    tracing::warn!("acknowledging {} failed: {e:#}", p.message);
                }
            }
        }
        self.store.save_thread(&st)
    }

    /// On start and on every reconnect: handles the messages missed while the
    /// connection was down. Each channel recovers on its own; one that fails is
    /// logged once, frozen at its cursor and retried every 30 s, and the others
    /// go on.
    pub(super) async fn recover(self: Arc<Self>) {
        let channels: Vec<String> = self.cfg.channels().map(|(id, _)| id.clone()).collect();
        join_all(channels.into_iter().map(|conv| {
            let core = self.clone();
            async move { core.recover_channel(conv).await }
        }))
        .await;
    }

    async fn recover_channel(self: Arc<Self>, conv: String) {
        if !self.recovering.lock().unwrap().insert(conv.clone()) {
            return; // its retry loop already covers this reconnect
        }
        match self.try_recover(&conv).await {
            Ok(()) => {
                self.recovering.lock().unwrap().remove(&conv);
            }
            Err(e) => {
                // The cursor must not move past the gap while recovery fails.
                self.cursor.freeze(&conv, true);
                tracing::error!("recovery of {conv} failed, retrying every 30 s: {e:#}");
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        if self.try_recover(&conv).await.is_ok() {
                            break;
                        }
                    }
                    tracing::info!("recovery of {conv} succeeded");
                    self.cursor.freeze(&conv, false);
                    self.recovering.lock().unwrap().remove(&conv);
                });
            }
        }
    }

    /// One channel's top-level messages after its cursor, and the replies after
    /// the cursor of every thread whose newest reply is past it. One `history`
    /// call covers the last 7 days (or back to the cursor, if older). What
    /// `handled` already has is dropped.
    async fn try_recover(self: &Arc<Self>, conv: &str) -> Result<()> {
        self.cursor.init(conv, ts_now())?;
        let from = self.cursor.get(conv).context("no cursor")?;
        let window = Utc::now().timestamp() - RECOVERY_WINDOW.as_secs() as i64;
        let oldest = from.clone().min(format!("{window}.000000"));
        let top = self
            .surface
            .recent(conv, Some(&oldest), None)
            .await
            .with_context(|| format!("history of {conv}"))?;
        let threads: Vec<&str> = top
            .iter()
            .filter(|m| m.latest_reply.as_deref().is_some_and(|l| l > from.as_str()))
            .map(|m| m.thread.as_str())
            .collect();
        let mut missed: BTreeMap<String, Msg> = BTreeMap::new();
        for t in threads {
            for m in self
                .surface
                .thread(conv, t, Some(&from))
                .await
                .with_context(|| format!("replies of {conv}/{t}"))?
            {
                missed.insert(m.id.clone(), m);
            }
        }
        for m in top {
            missed.entry(m.id.clone()).or_insert(m);
        }
        let missed: Vec<Msg> = missed.into_values().filter(|m| m.id > from).collect();
        let count = missed.iter().filter(|m| m.sender.id != self.bot).count();
        if count > 0 {
            tracing::info!("recovering {count} message(s) in {conv}");
        }
        for m in missed {
            self.accept(m);
        }
        Ok(())
    }
}
