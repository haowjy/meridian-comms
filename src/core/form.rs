//! The form: a person opens it in a channel, chooses one session and writes a
//! message, and sending it opens a thread there with the message, delivered as
//! that person's choice of the session. Every answer here is due within the
//! surface's few seconds, so it reads only the newest listing (`online`);
//! what may take longer (the wake) runs after the answer.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Utc;
use tokio::sync::{mpsc, watch};

use super::deliver::{How, Incoming};
use super::online::{Online, Scope};
use super::{Core, Listing, text, view};
use crate::config::AgentMark;
use crate::model::{
    FailKind, Failure, Form, FormConversation, FormField, Interacted, Interaction, MenuGroup, MenuOption, PostRequest, Posted, Response,
    Sender,
};
use crate::state::{Pick, ThreadState};

/// How long an answer may take inside the daemon; the surface's own limit is
/// a little longer.
const ANSWER: Duration = Duration::from_millis(2200);
/// How long a submission waits for a listing, leaving the rest of `ANSWER`
/// for the post.
const SUBMIT_WAIT: Duration = Duration::from_millis(1200);
/// How long a sent form is remembered: a surface sends a form again only
/// while it is still open.
const SENT_KEEP: Duration = Duration::from_secs(3600);

type PostOutcome = watch::Receiver<Option<Result<(), String>>>;

/// Forms already sent, by form ID, with how each one's post went. A form sent
/// again, because the answer to the first send was lost or late, is answered
/// from here and never posted or woken twice.
#[derive(Default)]
pub(super) struct Sent(Mutex<HashMap<String, (Instant, PostOutcome)>>);

/// Who may use the form, where.
struct User<'a> {
    person: &'a String,
    agent: AgentMark,
    project: Option<String>,
}

/// What a checked submission delivers.
struct Accepted {
    conversation: String,
    person: String,
    agent: AgentMark,
    project: Option<String>,
    /// The chosen session's person and ref.
    chosen: (String, String),
    body: String,
    /// The listing the choice was checked against, which the delivery uses.
    listing: Arc<Listing>,
}

impl Core {
    /// Answers each interaction in its own task, so none waits behind another
    /// or behind the event stream's recovery.
    pub async fn serve_interactions(self: Arc<Self>, mut rx: mpsc::Receiver<Interacted>) {
        while let Some((i, reply)) = rx.recv().await {
            let core = self.clone();
            tokio::spawn(async move {
                let _ = reply.send(core.interact(i).await);
            });
        }
    }

    async fn interact(self: &Arc<Self>, i: Interaction) -> Response {
        let deadline = Instant::now() + ANSWER;
        match i {
            Interaction::Form { conversation, sender } => self.form(&[conversation], &sender, None).await,
            Interaction::ViewMessage { sender, .. } if !self.is_owner(&sender) => Response::Private(view::NOT_YOURS.into()),
            Interaction::ViewMessage { sender, to } => self.form(&to.conversations, &sender, Some(to.session)).await,
            Interaction::Suggest {
                conversation,
                sender,
                query,
            } => match self.form_user(&conversation, &sender) {
                Err(why) => Response::Options {
                    groups: vec![],
                    note: Some(why),
                },
                Ok(u) => {
                    let online = self.online(u.project.as_deref(), ANSWER - Duration::from_millis(200)).await;
                    self.options(&online, &query)
                }
            },
            Interaction::Submit {
                form,
                conversation,
                sender,
                session,
                text,
            } => match self.submit(&form, &conversation, &sender, session, text, deadline).await {
                Ok(()) => Response::Done,
                Err((field, error)) => Response::Invalid { field, error },
            },
        }
    }

    /// The form for one of `conversations`: those that are configured and
    /// share the first one's project. The person's errors go to them alone.
    async fn form(self: &Arc<Self>, conversations: &[String], sender: &Sender, session: Option<String>) -> Response {
        let Some(first) = conversations.iter().find(|c| self.cfg.channel(c).is_some()) else {
            return Response::Private("This channel isn't set up for meridian-comms: use the form in one of its channels.".into());
        };
        let u = match self.form_user(first, sender) {
            Ok(u) => u,
            Err(why) => return Response::Private(why),
        };
        let conversations = conversations
            .iter()
            .filter_map(|c| {
                let ch = self.cfg.channel(c).filter(|ch| ch.project == u.project)?;
                Some(FormConversation {
                    id: c.clone(),
                    name: ch.name.clone(),
                })
            })
            .collect();
        let online = self.online(u.project.as_deref(), Duration::ZERO).await;
        // The view's listing knows a session it offered before this one may.
        let everywhere = self.newest(&Scope::Everywhere);
        let chosen = session.map(|r| {
            let found = [online.sessions.as_ref(), everywhere.as_ref()]
                .into_iter()
                .flatten()
                .find_map(|l| l.resolve(&r).map(|(_, c)| c.clone()));
            MenuOption {
                label: found.map_or_else(|| text::clean(&r), |c| text::option_label(&c, Utc::now())),
                value: r,
            }
        });
        Response::Form(Form {
            session: chosen,
            conversations,
            note: where_(&online),
        })
    }

    /// A form is for a configured channel and a person in the directory who
    /// is not an agent's account; the error says so to whoever asked.
    fn form_user(&self, conversation: &str, sender: &Sender) -> Result<User<'_>, String> {
        let Some(ch) = self.cfg.channel(conversation) else {
            return Err("This channel isn't set up for meridian-comms: use the form in one of its channels.".into());
        };
        match self.cfg.person_by_slack(&sender.id) {
            Some((person, p)) if p.agent != AgentMark::Always => Ok(User {
                person,
                agent: p.agent,
                project: ch.project.clone(),
            }),
            _ => Err("Only people in meridian-comms' directory can hand a message to a session.".into()),
        }
    }

    /// The session field's options for `query`: the picker's menu, keeping
    /// the options whose label or ref holds it.
    fn options(&self, online: &Online, query: &str) -> Response {
        let on = machines(online);
        let Some(listing) = &online.sessions else {
            return Response::Options {
                groups: vec![],
                note: Some(format!("Still listing the sessions on {on}: type to look again")),
            };
        };
        let q = query.trim().to_lowercase();
        let groups: Vec<MenuGroup> = self
            .menu(&self.cfg.owner, listing)
            .map(|m| m.groups)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|mut g| {
                g.options
                    .retain(|o| o.label.to_lowercase().contains(&q) || o.value.to_lowercase().contains(&q));
                (!g.options.is_empty()).then_some(g)
            })
            .collect();
        let note = groups.is_empty().then(|| {
            if listing.failed {
                format!("The sessions on {on} could not all be listed: try again")
            } else if listing.for_person(&self.cfg.owner).next().is_none() {
                format!("No sessions are running on {on}")
            } else {
                format!("No session on {on} matches that")
            }
        });
        Response::Options { groups, note }
    }

    /// Checks a first send against the newest listing, then posts the message
    /// and answers with how the post went; the wake runs after the answer. A
    /// form sent again gets the first send's answer.
    async fn submit(
        self: &Arc<Self>,
        form: &str,
        conversation: &str,
        sender: &Sender,
        session: Option<String>,
        body: String,
        deadline: Instant,
    ) -> Result<(), (FormField, String)> {
        let sent = self.forms.0.lock().unwrap().get(form).map(|(_, o)| o.clone());
        let outcome = match sent {
            Some(o) => o,
            None => {
                let accepted = self.check(conversation, sender, session, body).await?;
                let mut forms = self.forms.0.lock().unwrap();
                forms.retain(|_, (at, _)| at.elapsed() < SENT_KEEP);
                match forms.get(form) {
                    // Sent again while this send was being checked.
                    Some((_, o)) => o.clone(),
                    None => {
                        let (tx, rx) = watch::channel(None);
                        forms.insert(form.to_string(), (Instant::now(), rx.clone()));
                        let (core, form) = (self.clone(), form.to_string());
                        tokio::spawn(async move { core.open_thread(&form, accepted, tx).await });
                        rx
                    }
                }
            }
        };
        let mut outcome = outcome;
        let bad = |e: String| (FormField::Session, e);
        let waited = tokio::time::timeout_at(deadline.into(), outcome.wait_for(Option::is_some))
            .await
            .map(|r| r.map(|v| v.clone()));
        match waited {
            Ok(Ok(Some(Err(e)))) => Err(bad(e)),
            Ok(Ok(_)) => Ok(()),
            // The post's task ended (a panic) before it said how the post went.
            // Forgotten, so the form doesn't wait on an answer that never comes.
            Ok(Err(_)) => {
                self.forms
                    .0
                    .lock()
                    .unwrap()
                    .retain(|f, (_, o)| f != form || !o.same_channel(&outcome));
                Err(bad("The message could not be posted. Send again.".into()))
            }
            Err(_) => Err(bad("Still posting the message: wait a moment, then send again.".into())),
        }
    }

    /// A send that may be posted: a channel and person that may use the form,
    /// a message, and a session the newest listing has.
    async fn check(
        self: &Arc<Self>,
        conversation: &str,
        sender: &Sender,
        session: Option<String>,
        body: String,
    ) -> Result<Accepted, (FormField, String)> {
        let bad = |e: String| (FormField::Session, e);
        if conversation.is_empty() {
            return Err((FormField::Conversation, "Choose a channel.".into()));
        }
        let u = self.form_user(conversation, sender).map_err(|e| (FormField::Conversation, e))?;
        let body = body.trim().to_string();
        if body.is_empty() {
            return Err((FormField::Text, "Write the message.".into()));
        }
        let Some(r) = session else {
            return Err(bad("Choose a session.".into()));
        };
        let online = self.online(u.project.as_deref(), SUBMIT_WAIT).await;
        let on = machines(&online);
        let Some(listing) = online.sessions else {
            return Err(bad(format!("Still listing the sessions on {on}: send again in a moment.")));
        };
        let Some((_, c)) = listing.resolve(&r) else {
            return Err(bad(if listing.failed {
                format!("`{}` could not be checked: a source did not answer. Send again.", text::clean(&r))
            } else {
                format!("`{}` is no longer running on {on}: choose another.", text::clean(&r))
            }));
        };
        Ok(Accepted {
            conversation: conversation.to_string(),
            person: u.person.clone(),
            agent: u.agent,
            project: u.project.clone(),
            chosen: (c.person.clone(), c.ref_.clone()),
            body,
            listing: listing.clone(),
        })
    }

    /// Posts the message for its person as a new thread, says on `posted` how
    /// that went, and delivers it as the person's choice of the session, the
    /// way a `to:` line opening a thread is. A post known not to have gone out
    /// forgets the form, so sending it again posts again; one that may have
    /// gone out keeps it, and the error says where to look.
    async fn open_thread(&self, form: &str, a: Accepted, posted: watch::Sender<Option<Result<(), String>>>) {
        let Accepted {
            conversation,
            person,
            agent,
            project,
            chosen: (for_person, r),
            body,
            listing,
        } = a;
        let req = PostRequest {
            conversation: conversation.clone(),
            thread: None,
            text: body.clone(),
            menu: None,
            plain: true,
            by: Some(self.cfg.display(&person)),
        };
        let id = match self.surface.post(req).await {
            Ok(Posted { id }) => {
                posted.send_replace(Some(Ok(())));
                id
            }
            Err(e) => {
                tracing::error!("form message in {conversation}: cannot post: {e:#}");
                let why = text::clean(&format!("{e:#}"));
                // A post that may have gone out stays remembered: sending the form
                // again must not post it twice.
                let error = if Failure::kind_of(&e) == FailKind::MaybeSent {
                    format!(
                        "Slack may have posted the message without saying so ({why}), and nobody was woken. Look in the channel: if it is there, reply `to: {}` in its thread; if not, cancel and use the form again.",
                        text::clean(&r)
                    )
                } else {
                    self.forms.0.lock().unwrap().remove(form);
                    format!("The message could not be posted ({why}). Send again.")
                };
                posted.send_replace(Some(Err(error)));
                return;
            }
        };
        if let Err(e) = self
            .deliver_form(&conversation, &id, &person, agent, project, (for_person, r), body, &listing)
            .await
        {
            tracing::error!("form message {conversation}/{id}: {e:#}");
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn deliver_form(
        &self,
        conversation: &str,
        id: &str,
        person: &str,
        agent: AgentMark,
        project: Option<String>,
        (for_person, r): (String, String),
        body: String,
        listing: &Listing,
    ) -> Result<()> {
        // As `send` does: the thread's lock and file before the wake, so a reply
        // that arrives meanwhile waits its turn, and `read` names the author.
        let _guard = self.lock(conversation, id).await;
        let mut st = self
            .store
            .load_thread(conversation, id)?
            .unwrap_or_else(|| ThreadState::new(conversation, id));
        st.authors.insert(id.to_string(), person.to_string());
        self.store.save_thread(&st)?;

        let owner = self.cfg.owner.clone();
        let inc = Incoming {
            conversation: conversation.into(),
            thread: id.into(),
            link: self.surface.link(conversation, id, id),
            id: id.into(),
            from: self.cfg.display(person),
            sent: Utc::now(),
            text: body,
        };
        tracing::info!("{person} chose `{r}` with the form in {conversation}/{id}");
        let picks = [(for_person, Pick::Refs(vec![r]))];
        let how = How {
            author: None,
            agent_like: agent != AgentMark::No,
            pointer: false,
            chooser: person,
        };
        // Against the listing the choice was checked with: listing again would
        // leave the post unwoken for seconds. A session that ended since fails
        // its wake and is named as left out.
        let out = self.deliver(&mut st, &picks, listing, &inc, how).await;
        if out.woken.is_empty() {
            st.explained = true;
            let text = self.explain(&owner, project.as_deref(), listing, &[], None, &out.left_out, true);
            let menu = self.menu(&owner, listing);
            self.say_with(conversation, id, text, menu).await;
        } else if let Err(e) = self.surface.acknowledge(conversation, id).await {
            tracing::warn!("acknowledging {id} failed: {e:#}");
        }
        self.store.save_thread(&st)
    }
}

/// "this-machine", or "a, b": the machines a listing covers.
fn machines(online: &Online) -> String {
    match online.machines.as_slice() {
        [] => "no machine (no source is configured)".into(),
        m => m.iter().map(|m| text::clean(m)).collect::<Vec<_>>().join(", "),
    }
}

/// The form's line saying where its sessions are.
fn where_(online: &Online) -> String {
    let on = machines(online);
    match &online.sessions {
        Some(l) if !l.failed && l.candidates.is_empty() => format!("No sessions are running on {on}."),
        _ => format!("Sessions on {on}. Sending opens a thread and wakes the one you choose."),
    }
}
