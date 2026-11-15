//! Socket Mode: `apps.connections.open` gives a websocket URL; frames are JSON
//! envelopes, each acknowledged by `envelope_id`. Slack ends a connection with a
//! `disconnect` frame before it refreshes it. We reconnect at once, and the core
//! recovers what was missed on the new `hello`.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::{BUTTON, MENU, MENU_BLOCK, Slack, web::Token};
use crate::model::{Action, Sender, SurfaceEvent};

/// Slack pings about every 30 s; no frame for this long means a dead link.
const SILENCE: Duration = Duration::from_secs(120);

pub async fn run(slack: &Slack, tx: mpsc::Sender<SurfaceEvent>) {
    let mut backoff = Duration::from_secs(1);
    let mut first = true;
    loop {
        match session(slack, &tx, &mut first).await {
            Ok(true) => backoff = Duration::from_secs(1),
            Ok(false) => {}
            Err(e) => tracing::warn!("socket mode: {e:#}"),
        }
        if tx.is_closed() {
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// One connection. `Ok(true)` when it got as far as `hello`.
async fn session(slack: &Slack, tx: &mpsc::Sender<SurfaceEvent>, first: &mut bool) -> Result<bool> {
    let open = slack.web.form(Token::App, "apps.connections.open", &[]).await?;
    let url = open["url"].as_str().context("apps.connections.open gave no url")?;
    let (ws, _) = tokio_tungstenite::connect_async(url).await.context("websocket connect failed")?;
    let (mut sink, mut stream) = ws.split();
    let mut hello = false;
    loop {
        let frame = match tokio::time::timeout(SILENCE, stream.next()).await {
            Err(_) => bail!("no frame for {}s, reconnecting", SILENCE.as_secs()),
            Ok(None) => return Ok(hello),
            Ok(Some(f)) => f?,
        };
        let text = match frame {
            Message::Text(t) => t,
            Message::Ping(p) => {
                sink.send(Message::Pong(p)).await?;
                continue;
            }
            Message::Close(_) => return Ok(hello),
            _ => continue,
        };
        let v: Value = match serde_json::from_str(text.as_str()) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("socket mode: unparsable frame: {e}");
                continue;
            }
        };
        if let Some(id) = v["envelope_id"].as_str() {
            sink.send(Message::text(json!({ "envelope_id": id }).to_string())).await?;
        }
        match v["type"].as_str() {
            Some("hello") => {
                hello = true;
                // Slack splits events between the connections of one app, so a
                // second daemon on this bot would take some of them.
                let n = v["num_connections"].as_u64().unwrap_or(1);
                // Only the first connection is checked: Slack's own refresh opens
                // the new connection before closing the old one, so a reconnect
                // can see 2. The daemon already connected is never told about a
                // newcomer, so the newcomer is the one that must refuse.
                if std::mem::take(first) && n > 1 {
                    tracing::error!(
                        "slack reports {n} connections open for this bot: another meridian-comms daemon holds it. \
                         One bot has exactly one daemon; stop the other one, or give this machine its own bot."
                    );
                    let _ = sink.send(Message::Close(None)).await;
                    std::process::exit(2);
                }
                tracing::info!("socket mode connected ({n} connection)");
                if tx.send(SurfaceEvent::Connected).await.is_err() {
                    return Ok(hello);
                }
            }
            Some("disconnect") => {
                tracing::info!("slack asked us to reconnect ({})", v["reason"].as_str().unwrap_or("no reason"));
                return Ok(hello);
            }
            Some("events_api") => {
                if let Some(msg) = slack.event_message(&v["payload"]["event"])
                    && tx.send(SurfaceEvent::Message(msg)).await.is_err()
                {
                    return Ok(hello);
                }
            }
            Some("interactive") => {
                if let Some(a) = action(&v["payload"])
                    && tx.send(SurfaceEvent::Action(a)).await.is_err()
                {
                    return Ok(hello);
                }
            }
            _ => {}
        }
    }
}

fn action(p: &Value) -> Option<Action> {
    if p["type"] != "block_actions" {
        return None;
    }
    let a = p["actions"].get(0)?;
    let name = a["action_id"].as_str()?;
    // The menu's own selection event carries nothing the button doesn't: the
    // button click reads the selection from `state`.
    if name == MENU {
        return None;
    }
    let values = if name == BUTTON {
        let chosen = p["state"]["values"][MENU_BLOCK][MENU]["selected_options"].as_array();
        chosen
            .into_iter()
            .flatten()
            .filter_map(|o| o["value"].as_str().map(String::from))
            .collect()
    } else {
        a["value"].as_str().map(|v| vec![v.to_string()]).unwrap_or_default()
    };
    let post = p["message"]["ts"].as_str()?;
    Some(Action {
        conversation: p["channel"]["id"].as_str()?.to_string(),
        thread: p["message"]["thread_ts"].as_str().unwrap_or(post).to_string(),
        post: post.to_string(),
        sender: Sender {
            id: p["user"]["id"].as_str()?.to_string(),
            app: false,
        },
        name: name.to_string(),
        values,
    })
}
