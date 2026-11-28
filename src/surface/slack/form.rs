//! The `/agent` form: a modal with a session field Slack fills by asking the
//! bot as the person types (an `external_select`, `block_suggestion`), and a
//! message box. Slack waits 3 s for the acknowledgement of the command, each
//! suggestion and the submission, and the answer travels in that
//! acknowledgement, so these envelopes are acknowledged only once answered.
//! The modal carries its channels and its preset session in `private_metadata`:
//! the suggestion and the submission name no channel of their own, and a
//! click outside a channel (the Home tab) has none to give.

use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::Instant;

use super::{MAX_LABEL, Slack, option_groups, plain};
use crate::model::{Form, FormField, Interaction, Response, Sender};

const CALLBACK: &str = "agent-form";
const CHANNEL_BLOCK: &str = "channel";
const CHANNEL: &str = "channel";
const SESSION_BLOCK: &str = "session";
const SESSION: &str = "session";
const TEXT_BLOCK: &str = "text";
const TEXT: &str = "text";
/// The value of the one option that only says why there are none to choose.
const NOTE: &str = "-";
/// Slack's limit for a `plain_text_input`.
const MAX_TEXT: usize = 3000;
/// From the envelope's arrival to its acknowledgement, under Slack's 3 s.
pub(super) const ACK_WITHIN: Duration = Duration::from_millis(2500);

/// What the modal carries to its suggestions and its submission.
#[derive(Serialize, Deserialize)]
struct Meta {
    /// The channels it may open the thread in; with several, the person chooses.
    #[serde(rename = "c")]
    conversations: Vec<String>,
    /// The session it was opened with, if any.
    #[serde(rename = "s", default)]
    session: Option<String>,
}

pub(super) enum Kind {
    Command {
        trigger: String,
    },
    Suggest,
    /// `choice`: the form has a channel field, where errors about the channel go.
    Submit {
        choice: bool,
    },
}

/// The interaction a Socket Mode envelope carries, if it is one the form answers.
pub(super) fn parse(envelope: &Value) -> Option<(Kind, Interaction)> {
    let p = &envelope["payload"];
    match envelope["type"].as_str()? {
        "slash_commands" => Some((
            Kind::Command {
                trigger: p["trigger_id"].as_str()?.to_string(),
            },
            Interaction::Form {
                conversations: vec![p["channel_id"].as_str()?.to_string()],
                sender: person(p["user_id"].as_str()?),
                session: None,
            },
        )),
        "interactive" => {
            let view = &p["view"];
            let meta: Meta = serde_json::from_str(view["private_metadata"].as_str()?).ok()?;
            let sender = person(p["user"]["id"].as_str()?);
            match p["type"].as_str()? {
                "block_suggestion" if p["action_id"] == SESSION => Some((
                    Kind::Suggest,
                    Interaction::Suggest {
                        conversation: meta.conversations.first()?.clone(),
                        sender,
                        query: p["value"].as_str().unwrap_or_default().to_string(),
                    },
                )),
                "view_submission" if view["callback_id"] == CALLBACK => {
                    let values = &view["state"]["values"];
                    let picked = |block: &str, action: &str| values[block][action]["selected_option"]["value"].as_str().map(String::from);
                    let conversation = match meta.conversations.as_slice() {
                        [only] => Some(only.clone()),
                        many => picked(CHANNEL_BLOCK, CHANNEL).filter(|c| many.contains(c)),
                    };
                    let session = picked(SESSION_BLOCK, SESSION).filter(|v| v != NOTE).or(meta.session);
                    Some((
                        Kind::Submit {
                            choice: meta.conversations.len() > 1,
                        },
                        Interaction::Submit {
                            form: view["id"].as_str()?.to_string(),
                            conversation: conversation.unwrap_or_default(),
                            sender,
                            session,
                            text: values[TEXT_BLOCK][TEXT]["value"].as_str().unwrap_or_default().to_string(),
                        },
                    ))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn person(id: &str) -> Sender {
    Sender {
        id: id.to_string(),
        app: false,
    }
}

fn view(form: &Form) -> Value {
    let meta = Meta {
        conversations: form.conversations.iter().map(|c| c.id.clone()).collect(),
        session: form.session.as_ref().map(|o| o.value.clone()),
    };
    let mut select = json!({
        "type": "external_select", "action_id": SESSION, "min_query_length": 0,
        "placeholder": plain("Type to find a session", MAX_LABEL),
    });
    if let Some(o) = &form.session {
        select["initial_option"] = json!({"text": plain(&o.label, MAX_LABEL), "value": o.value});
    }
    let mut blocks = vec![json!({"type": "context", "elements": [plain(&form.note, 2000)]})];
    match form.conversations.as_slice() {
        [only] => blocks.push(json!({"type": "context", "elements": [plain(&format!("The thread opens in #{}.", only.name), 2000)]})),
        many => {
            let options: Vec<Value> = many
                .iter()
                .take(100)
                .map(|c| json!({"text": plain(&format!("#{}", c.name), MAX_LABEL), "value": c.id}))
                .collect();
            blocks.push(json!({"type": "input", "block_id": CHANNEL_BLOCK, "label": plain("Channel", 2000),
                "element": {"type": "static_select", "action_id": CHANNEL, "options": options,
                            "placeholder": plain("Where the thread opens", MAX_LABEL)}}));
        }
    }
    blocks.push(json!({"type": "input", "block_id": SESSION_BLOCK, "label": plain("Session", 2000), "element": select}));
    blocks.push(json!({"type": "input", "block_id": TEXT_BLOCK, "label": plain("Message", 2000),
        "element": {"type": "plain_text_input", "action_id": TEXT, "multiline": true, "max_length": MAX_TEXT}}));
    json!({
        "type": "modal", "callback_id": CALLBACK,
        "private_metadata": serde_json::to_string(&meta).expect("plain strings"),
        "title": plain("Message a session", 24),
        "submit": plain("Send", 24),
        "close": plain("Cancel", 24),
        "blocks": blocks,
    })
}

/// Opens the form with `trigger`, a command's or a click's. The form says
/// which channels it may open the thread in, and the session it starts on.
pub(super) async fn open(slack: &Slack, trigger: &str, form: &Form) -> Result<()> {
    let body = json!({"trigger_id": trigger, "view": view(form)});
    slack.web.json("views.open", &body).await?;
    Ok(())
}

/// The acknowledgement's payload for `kind` answered with `answer` (`None`:
/// the daemon did not answer in time). For a command, opens the modal first.
pub(super) async fn ack(slack: &Slack, kind: Kind, answer: Option<Response>, deadline: Instant) -> Option<Value> {
    let late = "meridian-comms took too long to answer: try again.";
    match (kind, answer) {
        (Kind::Command { trigger }, Some(Response::Form(form))) => {
            let why = match tokio::time::timeout_at(deadline, open(slack, &trigger, &form)).await {
                Ok(Ok(_)) => return None,
                Ok(Err(e)) => format!("{e:#}"),
                Err(_) => "Slack did not answer in time".into(),
            };
            tracing::warn!("cannot open the form: {why}");
            Some(json!({"text": format!("The form could not be opened ({why}). Try again.")}))
        }
        (Kind::Command { .. }, Some(Response::Private(t))) => Some(json!({"text": t})),
        (Kind::Command { .. }, Some(Response::Done)) => None,
        (Kind::Command { .. }, _) => Some(json!({"text": late})),
        (Kind::Suggest, Some(Response::Options { groups, note })) => Some(match note {
            Some(n) if groups.is_empty() => json!({"options": [{"text": plain(&n, MAX_LABEL), "value": NOTE}]}),
            _ => json!({"option_groups": option_groups(&groups)}),
        }),
        (Kind::Suggest, _) => Some(json!({"options": [{"text": plain(late, MAX_LABEL), "value": NOTE}]})),
        (Kind::Submit { .. }, Some(Response::Done)) => None,
        (Kind::Submit { choice }, answer) => {
            let (block, error) = match answer {
                Some(Response::Invalid { field, error }) => (
                    match field {
                        FormField::Conversation if choice => CHANNEL_BLOCK,
                        FormField::Conversation | FormField::Session => SESSION_BLOCK,
                        FormField::Text => TEXT_BLOCK,
                    },
                    error,
                ),
                Some(Response::Private(t)) => (SESSION_BLOCK, t),
                _ => (SESSION_BLOCK, late.to_string()),
            };
            Some(json!({"response_action": "errors", "errors": {block: error}}))
        }
    }
}
