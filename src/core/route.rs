//! The router: one message waiting in a source's inbox is offered to the
//! running session a model picks from the ones the sources list. An offer is
//! a guess: it goes only to a session that is working or whose cache is warm,
//! through the source, which sends its own fixed notice. The item's text goes
//! to the model and nowhere else: not into a post, a command or the log.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};

use super::filing::INBOX_TIMEOUT;
use super::{Core, WAKE_TIMEOUT, text};
use crate::jev;
use crate::model::{Activity, ActivityState, Candidate, DecideRequest, Decision, InboxItem, ListRequest, Offered, PostRequest};
use crate::proto::RouteOut;
use crate::routes::{Outcome, Route};
use crate::state::ThreadState;

const ROUTES_PER_MINUTE: usize = 20;
/// Two of the model's options are `none` and `more`.
const MAX_KEPT: usize = jev::MAX_OPTIONS - 2;
const STATE_TEXT_BYTES: usize = 4096;
/// How long a second step waits for the one before it.
const SECOND_STEP_QUEUE: Duration = Duration::from_secs(15 * 60);
/// A source's second step bounds itself; this is for one that hangs.
const SECOND_STEP_GUARD: Duration = Duration::from_secs(10 * 60);
const INSTRUCTIONS: &str = "Which running session should be offered this message?";
const NONE: &str = "none";
const MORE: &str = "more";

type ItemKey = (String, String);

/// How the second step left a routing: a session to offer to, or its end.
enum Second<'a> {
    Offer(&'a Candidate),
    End(Outcome, String),
}

/// Which items are routed or being routed, and the starts of the last minute.
#[derive(Default)]
pub struct Routing {
    done: HashSet<ItemKey>,
    flying: HashSet<ItemKey>,
    recent: VecDeque<Instant>,
}

impl Routing {
    pub fn resume(log: &[Route]) -> Routing {
        Routing {
            done: log
                .iter()
                .filter(|r| r.outcome.is_final())
                .map(|r| (r.source.clone(), r.item.clone()))
                .collect(),
            ..Default::default()
        }
    }
}

/// Working, or idle with a cache deadline still ahead. Unknown activity is
/// not enough for a guess.
fn awake(c: &Candidate, now: DateTime<Utc>) -> bool {
    match &c.activity {
        Some(Activity {
            state: ActivityState::Working,
            ..
        }) => true,
        Some(Activity {
            state: ActivityState::Idle,
            warm_until: Some(t),
            ..
        }) => *t > now,
        _ => false,
    }
}

/// What the model is told about a session: where it runs, what it is, what it
/// works on, and whether it is busy. No goal, prompt or transcript.
fn describe(c: &Candidate, now: DateTime<Utc>) -> String {
    let named = |word: &str, v: &Option<String>| v.as_ref().map(|v| format!("{word} {v}"));
    [
        named("project", &c.project),
        named("agent", &c.label),
        named("work item", &c.group),
        Some(text::activity(c.activity.as_ref(), now)),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(", ")
}

/// What the model is told about the message: who sent it, through what, for
/// which project, and the first 4 KiB of its text.
fn state(item: &InboxItem) -> String {
    let mut out = String::new();
    if let Some(f) = &item.from {
        out.push_str(&format!("From: session {f}\n"));
    }
    let via: Vec<&str> = [&item.via_program, &item.via_sender, &item.via_ref]
        .into_iter()
        .filter_map(|v| v.as_deref())
        .collect();
    if !via.is_empty() {
        out.push_str(&format!("Via: {}\n", via.join(", ")));
    }
    if let Some(p) = &item.project {
        out.push_str(&format!("Project: {p}\n"));
    }
    let end = (0..=item.text.len().min(STATE_TEXT_BYTES))
        .rev()
        .find(|i| item.text.is_char_boundary(*i))
        .unwrap_or(0);
    out.push('\n');
    out.push_str(&item.text[..end]);
    out
}

impl Core {
    /// Accepts `item` for routing and answers at once; the routing runs in its
    /// own task. An item already routed, or being routed, is not routed again:
    /// the source may ask twice for one message.
    pub fn route(self: &Arc<Self>, source: Option<String>, item: String) -> Result<RouteOut> {
        if item.is_empty() || item.len() > 64 || !item.chars().all(|c| c.is_ascii_alphanumeric()) {
            bail!("{item:?} is not an inbox item ID");
        }
        let src = match &source {
            None if self.sources.is_empty() => bail!("no source is configured"),
            None => 0,
            Some(name) => self
                .sources
                .iter()
                .position(|s| s.name() == name)
                .with_context(|| format!("no source is named {name:?}"))?,
        };
        let key = (self.sources[src].name().to_string(), item);
        let out = |accepted, detail: &str| RouteOut {
            accepted,
            detail: format!("{}: {detail}", key.1),
        };
        {
            let mut r = self.routing.lock().unwrap();
            if r.done.contains(&key) {
                return Ok(out(false, "already routed; `meridian-comms routes` shows what was done"));
            }
            if !r.flying.insert(key.clone()) {
                return Ok(out(false, "is being routed"));
            }
        }
        let answer = out(true, "accepted; `meridian-comms routes` shows what was done");
        let core = self.clone();
        tokio::spawn(async move {
            let mut route = Route {
                time: Utc::now(),
                source: key.0.clone(),
                item: key.1.clone(),
                message: None,
                candidates: vec![],
                choice: None,
                probabilities: BTreeMap::new(),
                record: None,
                outcome: Outcome::Failed,
                done: "nothing: the run failed".into(),
                error: None,
            };
            match core.run_route(src, &mut route).await {
                Ok((outcome, done)) => (route.outcome, route.done) = (outcome, done),
                Err(e) => route.error = Some(format!("{e:#}")),
            }
            match &route.error {
                Some(e) => tracing::warn!("routing {} failed: {e}", route.item),
                None => tracing::info!("routed {}: {}", route.item, route.done),
            }
            if let Err(e) = core.routes.append(&route) {
                tracing::error!("cannot write the routing log: {e:#}");
            }
            let mut r = core.routing.lock().unwrap();
            r.flying.remove(&key);
            if route.outcome.is_final() {
                r.done.insert(key);
            }
        });
        Ok(answer)
    }

    /// One routing, start to end: the decision and what was done. `route`
    /// gathers what the log keeps as the steps learn it. An error is a failed
    /// run: nothing was offered and the item waits.
    async fn run_route(&self, src: usize, route: &mut Route) -> Result<(Outcome, String)> {
        let Some(jev) = &self.jev else {
            return Ok((
                Outcome::Off,
                "nothing: the router is off (no [router] section, or no OPENCODE_API_KEY)".into(),
            ));
        };
        {
            let mut r = self.routing.lock().unwrap();
            let now = Instant::now();
            while r.recent.front().is_some_and(|t| now - *t >= Duration::from_secs(60)) {
                r.recent.pop_front();
            }
            if r.recent.len() >= ROUTES_PER_MINUTE {
                return Ok((Outcome::Capped, format!("nothing: over {ROUTES_PER_MINUTE} routings a minute")));
            }
            r.recent.push_back(now);
        }
        let source = &self.sources[src];
        let item = tokio::time::timeout(INBOX_TIMEOUT, source.inbox_item(&route.item))
            .await
            .context("reading the item timed out")??
            .with_context(|| format!("{} has no such item", source.name()))?;

        // The thread is found through the daemon's own files, never the item's
        // `via`: anyone can file an item that claims to come from here.
        drop(self.filing.write().await);
        let thread = self
            .store
            .all_threads()
            .into_iter()
            .find(|t| t.filed.as_ref().is_some_and(|f| f.source == source.name() && f.item == route.item));
        route.message = thread
            .as_ref()
            .and_then(|t| Some(self.message_ref(&t.conversation, &t.filed.as_ref()?.message)));
        if !item.waiting {
            return Ok((Outcome::None, "nothing: it was already taken".into()));
        }

        let owner = &self.cfg.owner;
        let listing = self
            .list(&ListRequest {
                person: Some(owner.clone()),
                any_project: true,
                ..Default::default()
            })
            .await;
        if listing.failed {
            bail!("a source could not list the running sessions");
        }
        let now = Utc::now();
        // A guess never carries a message out of its project, to its own
        // sender, or to a session that would pay to re-read its context.
        let kept: Vec<Candidate> = listing
            .candidates
            .into_iter()
            .filter(|(i, c)| *i == src && c.person == *owner)
            .map(|(_, c)| c)
            .filter(|c| item.project.is_none() || c.project == item.project)
            .filter(|c| item.from.is_none() || c.id != item.from)
            .filter(|c| awake(c, now))
            .collect();
        if kept.is_empty() {
            return Ok((Outcome::None, "nothing: no working or warm session to offer it to".into()));
        }
        if kept.len() > MAX_KEPT {
            return Ok((Outcome::None, format!("nothing: more than {MAX_KEPT} sessions to choose among")));
        }
        route.candidates = kept.iter().map(|c| c.ref_.clone()).collect();

        let option = |i: usize| format!("s{}", i + 1);
        let mut criteria: BTreeMap<String, String> = kept.iter().enumerate().map(|(i, c)| (option(i), describe(c, now))).collect();
        criteria.insert(NONE.into(), "none of these sessions".into());
        criteria.insert(MORE.into(), "this needs more context or reasoning than the list gives".into());
        let answer = jev.choose(&state(&item), INSTRUCTIONS, &criteria).await?;
        // The log names sessions by ref; an option the daemon did not list is dropped.
        let named = |o: &str| match kept.iter().enumerate().find(|(i, _)| option(*i) == o) {
            Some((_, c)) => Some(c.ref_.clone()),
            None => matches!(o, NONE | MORE).then(|| o.to_string()),
        };
        route.choice = named(&answer.choice);
        route.probabilities = answer.probabilities.iter().filter_map(|(o, p)| Some((named(o)?, *p))).collect();

        let chosen = kept.iter().enumerate().find(|(i, _)| option(*i) == answer.choice).map(|(_, c)| c);
        let target = match chosen {
            Some(c) if answer.probability() >= self.cfg.router.as_ref().map_or(1.0, |r| r.floor) => c,
            None if answer.choice != MORE => return Ok((Outcome::None, "nothing: the model chose no session".into())),
            // `more`, or a session the model was not sure of.
            _ => match self
                .second_step(src, route, &item, &kept, thread.as_ref(), chosen.is_some())
                .await?
            {
                Second::Offer(c) => c,
                Second::End(outcome, done) => return Ok((outcome, done)),
            },
        };

        let offer = tokio::time::timeout(WAKE_TIMEOUT, source.offer(&route.item, &target.ref_))
            .await
            .context("the offer timed out")??;
        // Not offered, or perhaps offered: either way the item waits, and the
        // routing is not run again. Repeating an offer is for a person to do.
        let why = text::clean(&offer.detail);
        match offer.offered {
            Offered::Yes => {}
            Offered::No => return Ok((Outcome::NotOffered, format!("not offered to {}: {why}", target.ref_))),
            Offered::Maybe => return Ok((Outcome::MaybeOffered, format!("may have been offered to {}: {why}", target.ref_))),
        }
        if let Some(t) = thread
            && let Err(e) = self.note_offer(&t, source.name(), &route.item, target, now).await
        {
            tracing::warn!("cannot say in the thread who was offered {}: {e:#}", route.item);
        }
        Ok((Outcome::Offered, format!("offered to {}", target.ref_)))
    }

    /// Asks the source to decide what the model could not. No thread lock and
    /// no gate is held: a step takes minutes. One runs at a time, and one that
    /// waited too long for its turn is dropped.
    async fn second_step<'a>(
        &self,
        src: usize,
        route: &mut Route,
        item: &InboxItem,
        kept: &'a [Candidate],
        thread: Option<&ThreadState>,
        unsure: bool,
    ) -> Result<Second<'a>> {
        let source = &self.sources[src];
        let none = |why: &str| Ok(Second::End(Outcome::None, format!("nothing: {why}")));
        let req = DecideRequest {
            item: route.item.clone(),
            message: item.clone(),
            candidates: kept.to_vec(),
            may_ask: thread.is_some(),
        };
        let one = tokio::time::timeout(SECOND_STEP_QUEUE, self.second.acquire())
            .await
            .context("dropped after 15 minutes waiting for another second step to finish")??;
        // The run's ID goes in the log before its end is known.
        let decided = tokio::time::timeout(SECOND_STEP_GUARD, async {
            let Some(run) = source.start_decision(&req).await? else {
                return Ok(None);
            };
            let run = route.record.insert(run);
            source.decision(&req, run).await.map(Some)
        })
        .await
        .unwrap_or_else(|_| bail!("the second step hung"));
        drop(one);
        let Some(decided) = decided? else {
            let why = if unsure { "was not sure enough" } else { "asked for more" };
            return none(&format!("the model {why}, and {} has no second step", source.name()));
        };
        match decided {
            Decision::Offer(r) => match kept.iter().find(|c| c.ref_ == r) {
                Some(c) => Ok(Second::Offer(c)),
                None => none("the second step named a session that was not listed"),
            },
            Decision::None => none("the second step chose no session"),
            Decision::Ask(q) => {
                let Some(t) = thread else {
                    return none("the second step asked a question, and the item has no thread to ask in");
                };
                self.ask(t, source.name(), &route.item, &q).await?;
                Ok(Second::End(Outcome::Asked, "asked the second step's question in the thread".into()))
            }
        }
    }

    /// Posts the second step's question in the thread its item was filed from.
    /// The question is the agent's words after reading a stranger's message:
    /// it is cut down, posted as plain text under the daemon's own label, and
    /// never logged.
    async fn ask(&self, t: &ThreadState, source: &str, item: &str, question: &str) -> Result<()> {
        let question = text::question(question);
        if question.is_empty() {
            bail!("the second step's question was empty once cleaned");
        }
        let _guard = self.lock(&t.conversation, &t.thread).await;
        let still = self.store.load_thread(&t.conversation, &t.thread)?;
        if !still.is_some_and(|st| st.filed.is_some_and(|f| f.source == source && f.item == item)) {
            bail!("the item left its thread before the question was posted");
        }
        let req = PostRequest {
            conversation: t.conversation.clone(),
            thread: Some(t.thread.clone()),
            text: format!(
                "The router asks about {}, so it can find a session for it: {question}",
                text::clean(item)
            ),
            menu: None,
            plain: true,
        };
        self.surface.post(req).await.context("the question could not be posted")?;
        Ok(())
    }

    /// Says in the thread an item was filed from who was offered it: on the
    /// bot's post that names the item, or in a post of its own.
    async fn note_offer(&self, t: &ThreadState, source: &str, item: &str, c: &Candidate, now: DateTime<Utc>) -> Result<()> {
        let _guard = self.lock(&t.conversation, &t.thread).await;
        let Some(mut st) = self.store.load_thread(&t.conversation, &t.thread)? else {
            return Ok(());
        };
        let Some(f) = st.filed.as_mut().filter(|f| f.source == source && f.item == item) else {
            return Ok(()); // someone chose a session while the model was asked
        };
        // A second step may have run since the listing: the activity is that old.
        let note = format!(
            "{} offered to `{}` ({} when listed)",
            text::clean(item),
            c.ref_,
            text::activity(c.activity.as_ref(), now)
        );
        match &f.post {
            Some(post) => self.surface.annotate(&t.conversation, &t.thread, post, &note, true).await,
            None => {
                let post = self.say(&t.conversation, &t.thread, note).await.context("the post failed")?;
                f.post = Some(post.id);
                self.store.save_thread(&st)
            }
        }
    }
}
