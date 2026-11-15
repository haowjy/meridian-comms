//! Events from the surface: messages, clicks, and reconnects.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;

use super::deliver::Incoming;
use super::{Core, Listing, ThreadKey};
use crate::config::AgentMark;
use crate::model::{Action, ListRequest, Msg, PostRequest, SurfaceEvent};
use crate::state::{Pending, ThreadState};

/// The loop limit: this many messages in a row that woke someone and may be
/// from agents pause the thread.
pub const LOOP_LIMIT: u32 = 12;
const RECOVERY_THREAD_WINDOW: Duration = Duration::from_secs(7 * 24 * 3600);

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
        if let Some(pos) = self.cursor.finish(&m.conversation, &m.id)
            && let Err(e) = self.store.save_cursor(&pos)
        {
            tracing::error!("cannot save cursor.json: {e:#}");
        }
    }

    async fn handle(&self, job: Job) {
        match job {
            Job::Msg(m) => match self.handle_message(&m).await {
                // An event that failed stays in flight: the cursor stops
                // before it, and the next recovery handles it again.
                Err(e) => tracing::error!("message {} in {}: {e:#}", m.id, m.conversation),
                Ok(()) => {
                    if let Some(pos) = self.cursor.finish(&m.conversation, &m.id)
                        && let Err(e) = self.store.save_cursor(&pos)
                    {
                        tracing::error!("cannot save cursor.json: {e:#}");
                    }
                }
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
        let mut st = match self.store.load_thread(&m.conversation, &m.thread) {
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
            id: m.id.clone(),
            from: self.cfg.display(person),
            sent: m.time,
        };
        let thread_message = self.message_ref(&m.conversation, &m.thread);

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
                st.mark_handled(&m.id);
                self.store.save_thread(&st)?;
                let bad = words.first().map(String::as_str).unwrap_or("");
                let msg = format!(
                    "I can't match `{}` to a running session.\n{}",
                    super::text::clean(bad),
                    self.explain(&owner, project.as_deref(), &listing, &[], &[], true)
                );
                self.say(&m.conversation, &m.thread, msg).await;
                return Ok(());
            }
        }

        if all {
            st.scope.remove(&owner);
        }
        for (p, refs) in &chosen {
            let prior = st.woken_of(p).to_vec();
            st.scope.insert(p.clone(), refs.clone());
            self.release(&mut st, &thread_message, p, &prior, &listing).await;
        }

        if st.paused {
            st.pending = Some(Pending {
                message: m.id.clone(),
                person: owner,
            });
            st.mark_handled(&m.id);
            return self.store.save_thread(&st);
        }
        if !agent_like {
            st.streak = 0;
        }

        let mut woken = vec![];
        let mut left_out = vec![];
        if chosen.is_empty() {
            let (plan, left) = self.plan_rule(&st, &listing, &owner, None);
            let d = self.wake_all(plan, left, &inc).await;
            if !d.woken.is_empty() {
                st.woken.insert(owner.clone(), d.woken.clone());
            }
            woken.extend(d.woken);
            left_out.extend(d.left_out);
        } else {
            for (p, refs) in &chosen {
                let (plan, left) = self.plan_choice(&listing, refs);
                let d = self.wake_all(plan, left, &inc).await;
                st.woken.insert(p.clone(), d.woken.clone());
                woken.extend(d.woken);
                left_out.extend(d.left_out);
            }
        }

        self.after_delivery(
            &mut st,
            &inc,
            &owner,
            project.as_deref(),
            &listing,
            &woken,
            &left_out,
            first,
            prior_pending,
            agent_like,
        )
        .await;
        st.mark_handled(&m.id);
        self.store.save_thread(&st)
    }

    /// What follows a delivery: pending, the streak, and what the bot says.
    #[allow(clippy::too_many_arguments)]
    async fn after_delivery(
        &self,
        st: &mut ThreadState,
        inc: &Incoming,
        person: &str,
        project: Option<&str>,
        listing: &Listing,
        woken: &[String],
        left_out: &[String],
        first: bool,
        prior_pending: bool,
        agent_like: bool,
    ) {
        if woken.is_empty() {
            st.pending = Some(Pending {
                message: inc.id.clone(),
                person: person.to_string(),
            });
            // The picker on a thread's first message; after that, once per run of
            // waiting messages, so a conversation nobody was woken for stays quiet.
            if first || !prior_pending {
                let text = self.explain(person, project, listing, &[], left_out, first);
                self.say(&inc.conversation, &st.thread, text).await;
            }
            return;
        }
        st.pending = None;
        if first {
            let text = self.explain(person, project, listing, woken, left_out, true);
            self.say(&inc.conversation, &st.thread, text).await;
        } else if let Err(e) = self.surface.react(&inc.conversation, &inc.id, "eyes").await {
            tracing::warn!("reaction failed: {e:#}");
        }
        if agent_like {
            st.streak += 1;
            self.pause_if_needed(st).await;
        }
    }

    pub(super) async fn pause_if_needed(&self, st: &mut ThreadState) {
        if st.streak >= LOOP_LIMIT && !st.paused {
            st.paused = true;
            let req = PostRequest {
                conversation: st.conversation.clone(),
                thread: Some(st.thread.clone()),
                text: format!("Paused after {LOOP_LIMIT} messages in a row that may be from agents. Press Continue to go on."),
                button: Some(("continue".into(), "Continue".into())),
            };
            if let Err(e) = self.surface.post(req).await {
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
            button: None,
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
        let Some(mut st) = self.store.load_thread(&a.conversation, &a.thread) else {
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
                    id: msg.id.clone(),
                    from: author,
                    sent: msg.time,
                };
                let project = self.cfg.channel(&a.conversation).and_then(|c| c.project.clone());
                let refs = st.scope.values().chain(st.woken.values()).flatten().cloned().collect();
                let listing = self
                    .list(&ListRequest {
                        person: Some(p.person.clone()),
                        project: project.clone(),
                        refs,
                    })
                    .await;
                let (plan, left) = self.plan_rule(&st, &listing, &p.person, None);
                let d = self.wake_all(plan, left, &inc).await;
                if d.woken.is_empty() {
                    let text = self.explain(&p.person, project.as_deref(), &listing, &[], &d.left_out, false);
                    self.say(&a.conversation, &a.thread, text).await;
                } else {
                    st.woken.insert(p.person.clone(), d.woken);
                    st.pending = None;
                    if let Err(e) = self.surface.react(&a.conversation, &p.message, "eyes").await {
                        tracing::warn!("reaction failed: {e:#}");
                    }
                }
            }
        }
        self.store.save_thread(&st)
    }

    /// Handles the messages missed while the connection was down: a channel's
    /// history after its cursor, and the replies of each thread that is new or
    /// was active in the last 7 days. Whatever `handled` already has is dropped.
    pub(super) async fn recover(self: Arc<Self>) {
        if let Err(e) = self.try_recover().await {
            // The cursor must not move past the gap while recovery fails.
            self.cursor.freeze(true);
            tracing::error!("recovery failed, retrying every 30 s: {e:#}");
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    match self.try_recover().await {
                        Ok(()) => break,
                        Err(e) => tracing::error!("recovery failed again: {e:#}"),
                    }
                }
                self.cursor.freeze(false);
            });
        } else {
            self.cursor.freeze(false);
        }
    }

    async fn try_recover(self: &Arc<Self>) -> Result<()> {
        let recent = self.store.recent_threads(RECOVERY_THREAD_WINDOW);
        for (conv, _) in self.cfg.channels() {
            if let Some(pos) = self.cursor.init(conv, ts_now()) {
                self.store.save_cursor(&pos)?;
            }
            let from = self.cursor.get(conv).context("no cursor")?;
            let top = self
                .surface
                .recent(conv, Some(&from), None)
                .await
                .with_context(|| format!("history of {conv}"))?;
            let mut threads: BTreeSet<String> = recent.iter().filter(|(c, _)| c == conv).map(|(_, t)| t.clone()).collect();
            threads.extend(top.iter().filter(|m| m.reply_count.unwrap_or(0) > 0).map(|m| m.thread.clone()));
            let mut missed: BTreeMap<String, Msg> = top.into_iter().map(|m| (m.id.clone(), m)).collect();
            for t in threads {
                for m in self
                    .surface
                    .thread(conv, &t, Some(&from))
                    .await
                    .with_context(|| format!("replies of {conv}/{t}"))?
                {
                    missed.entry(m.id.clone()).or_insert(m);
                }
            }
            let count = missed.values().filter(|m| m.id > from && m.sender.id != self.bot).count();
            if count > 0 {
                tracing::info!("recovering {count} message(s) in {conv}");
            }
            for m in missed.into_values().filter(|m| m.id > from) {
                self.accept(m);
            }
        }
        Ok(())
    }
}
