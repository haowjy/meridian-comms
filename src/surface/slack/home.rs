//! The Home tab: the view of the agents, published per person with
//! `views.publish`. Its events are Slack's `app_home_opened` for the Home tab
//! (the Messages tab sends the same event, and is not the view) and the clicks
//! on its buttons, which carry no channel or message. Refresh is an event;
//! Message must open the form within Slack's 3 s, so it goes the form's way.

use anyhow::Result;
use serde_json::{Value, json};

use super::form::Kind;
use super::{MAX_LABEL, SECTION_TEXT, Slack, plain};
use crate::model::{Agents, FormFor, Interaction, Sender, SurfaceEvent, View};

const REFRESH: &str = "home-refresh";
const MESSAGE: &str = "home-message";
/// Slack's limits: blocks in a view, a header's text, a context's text, a button's value.
const MAX_BLOCKS: usize = 100;
const HEADER_TEXT: usize = 150;
const CONTEXT_TEXT: usize = 2000;
const MAX_BUTTON_VALUE: usize = 2000;

fn person(id: &str) -> Sender {
    Sender {
        id: id.to_string(),
        app: false,
    }
}

/// An `events_api` event that opened the Home tab.
pub(super) fn opened(event: &Value) -> Option<SurfaceEvent> {
    (event["type"] == "app_home_opened" && event["tab"] == "home").then_some(())?;
    Some(SurfaceEvent::ViewOpened(person(event["user"].as_str()?)))
}

/// The first action of a click on the Home tab, and who clicked.
fn home_click(p: &Value) -> Option<(&Value, &str)> {
    if p["type"] != "block_actions" || p["view"]["type"] != "home" {
        return None;
    }
    Some((p["actions"].get(0)?, p["user"]["id"].as_str()?))
}

/// An `interactive` payload that is the Refresh button.
pub(super) fn refresh(p: &Value) -> Option<SurfaceEvent> {
    let (action, user) = home_click(p)?;
    (action["action_id"] == REFRESH).then(|| SurfaceEvent::ViewRefresh(person(user)))
}

/// An envelope that is a Message button: the form, opened with its trigger.
pub(super) fn message(envelope: &Value) -> Option<(Kind, Interaction)> {
    if envelope["type"] != "interactive" {
        return None;
    }
    let p = &envelope["payload"];
    let (action, user) = home_click(p)?;
    if action["action_id"] != MESSAGE {
        return None;
    }
    let to: FormFor = serde_json::from_str(action["value"].as_str()?).ok()?;
    Some((
        Kind::Click {
            trigger: p["trigger_id"].as_str()?.to_string(),
        },
        Interaction::Form {
            conversations: to.conversations,
            sender: person(user),
            session: Some(to.session),
            owner_only: true,
        },
    ))
}

fn text(s: &str) -> Value {
    json!({"type": "section", "text": plain(s, SECTION_TEXT)})
}

fn context(s: &str) -> Value {
    json!({"type": "context", "elements": [plain(s, CONTEXT_TEXT)]})
}

fn button(label: &str, action: &str, value: Option<String>) -> Value {
    let mut b = json!({"type": "button", "action_id": action, "text": plain(label, MAX_LABEL)});
    if let Some(v) = value {
        b["value"] = json!(v);
    }
    b
}

/// The view as blocks, at most Slack's 100: what doesn't fit is cut at the
/// end and counted as groups not shown. Every text is `plain_text`, so nothing
/// from a source becomes a mention, a link or formatting.
pub(super) fn blocks(view: &View) -> Vec<Value> {
    let a: &Agents = match view {
        View::Note(t) => return vec![text(t)],
        View::Agents(a) => a,
    };
    let mut out = vec![
        json!({"type": "section", "text": plain("Your agents", SECTION_TEXT),
               "accessory": button("Refresh", REFRESH, None)}),
        context(&a.status),
    ];
    if a.projects.is_empty() {
        out.push(text(&a.empty));
        return out;
    }
    // Each group's blocks, its project's header with the first of them.
    let mut groups: Vec<Vec<Value>> = vec![];
    for p in &a.projects {
        for (i, g) in p.groups.iter().enumerate() {
            let mut blocks = vec![];
            if i == 0 {
                blocks.push(json!({"type": "header", "text": plain(&p.label, HEADER_TEXT)}));
            }
            blocks.push(text(&format!("▸ {}", g.label)));
            for agent in &g.agents {
                let mut b = text(&agent.line);
                let value = agent.message.as_ref().map(|m| serde_json::to_string(m).expect("plain strings"));
                if let Some(v) = value.filter(|v| v.len() <= MAX_BUTTON_VALUE) {
                    b["accessory"] = button("Message", MESSAGE, Some(v));
                }
                blocks.push(b);
            }
            if let Some(o) = &g.others {
                blocks.push(context(o));
            }
            groups.push(blocks);
        }
    }
    let total: usize = groups.iter().map(Vec::len).sum();
    if out.len() + total <= MAX_BLOCKS {
        out.extend(groups.into_iter().flatten());
        return out;
    }
    // One block is kept for the line that says what was cut.
    let room = MAX_BLOCKS - out.len() - 1;
    let mut shown = 0;
    let mut whole = 0;
    for g in &groups {
        let fits = g.len().min(room - shown);
        out.extend(g[..fits].iter().cloned());
        shown += fits;
        if fits < g.len() {
            break;
        }
        whole += 1;
    }
    let cut = groups.len() - whole;
    out.push(context(&format!(
        "{cut} more group{} not shown in full: Slack shows at most {MAX_BLOCKS} blocks.",
        if cut == 1 { "" } else { "s" }
    )));
    out
}

pub(super) async fn show(slack: &Slack, viewer: &str, view: &View) -> Result<()> {
    let body = json!({"user_id": viewer, "view": {"type": "home", "blocks": blocks(view)}});
    slack.web.json("views.publish", &body).await?;
    Ok(())
}

/// A modal that says `t`, for a click that has no channel to answer in.
pub(super) async fn notice(slack: &Slack, trigger: &str, t: &str) -> Result<()> {
    let view = json!({
        "type": "modal", "title": plain("meridian-comms", 24), "close": plain("OK", 24),
        "blocks": [text(t)],
    });
    slack.web.json("views.open", &json!({"trigger_id": trigger, "view": view})).await?;
    Ok(())
}
