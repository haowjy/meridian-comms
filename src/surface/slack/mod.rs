//! Slack, hand-rolled: Socket Mode for events, the Web API for everything else.
//! Every post uses the bot token; the app has no user token.

mod socket;
mod web;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::Surface;
use crate::model::{Msg, PostRequest, Posted, Sender, SurfaceEvent, WhoAmI};
use web::{Token, Web};

pub struct Slack {
    web: Web,
    bot_user: String,
    bot_id: String,
    /// The workspace URL, for permalinks.
    url: String,
}

impl Slack {
    pub async fn connect(bot_token: String, app_token: String) -> Result<Slack> {
        let web = Web::new(bot_token, app_token)?;
        let me = web.form(Token::Bot, "auth.test", &[]).await.context("the bot token was refused")?;
        Ok(Slack {
            bot_user: me["user_id"].as_str().context("auth.test gave no user_id")?.to_string(),
            bot_id: me["bot_id"].as_str().unwrap_or_default().to_string(),
            url: me["url"].as_str().unwrap_or("https://slack.com/").to_string(),
            web,
        })
    }

    fn is_own(&self, v: &Value) -> bool {
        v["user"] == self.bot_user.as_str() || (!self.bot_id.is_empty() && v["bot_id"] == self.bot_id.as_str())
    }

    /// A message from the event stream: never the bot's own, never an edit,
    /// deletion or membership notice.
    fn event_message(&self, event: &Value) -> Option<Msg> {
        if event["type"] != "message" || self.is_own(event) {
            return None;
        }
        self.parse(event["channel"].as_str()?, event)
    }

    fn parse(&self, conversation: &str, v: &Value) -> Option<Msg> {
        // Only these subtypes are people's or apps' messages.
        if !matches!(
            v["subtype"].as_str(),
            None | Some("thread_broadcast" | "file_share" | "bot_message")
        ) {
            return None;
        }
        let id = v["ts"].as_str()?.to_string();
        let sender_id = if self.is_own(v) {
            &self.bot_user
        } else {
            v["user"].as_str().or(v["bot_id"].as_str())?
        };
        let app = !v["bot_id"].is_null() || !v["app_id"].is_null();
        let raw = v["text"].as_str().unwrap_or_default();
        let mention = format!("<@{}", self.bot_user);
        let trimmed = raw.trim_start();
        let (addressed, text) = match trimmed.strip_prefix(&mention) {
            Some(rest) if rest.starts_with('>') || rest.starts_with('|') => {
                (true, rest.split_once('>').map(|(_, t)| t).unwrap_or_default())
            }
            _ => (false, raw),
        };
        Some(Msg {
            conversation: conversation.to_string(),
            thread: v["thread_ts"].as_str().unwrap_or(&id).to_string(),
            time: ts_time(&id),
            sender: Sender {
                id: sender_id.to_string(),
                app,
            },
            addressed,
            text: text.trim().replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&"),
            reply_count: v["reply_count"].as_u64().map(|n| n as u32),
            id,
        })
    }

    fn link(&self, conversation: &str, ts: &str, thread: Option<&str>) -> String {
        let base = format!("{}archives/{conversation}/p{}", self.url, ts.replace('.', ""));
        match thread {
            Some(t) if t != ts => format!("{base}?thread_ts={t}&cid={conversation}"),
            _ => base,
        }
    }

    /// Reads pages of a messages method (`conversations.replies` / `.history`) until done.
    async fn pages(&self, method: &str, params: Vec<(&str, String)>, max: Option<usize>) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        for _ in 0..50 {
            let mut p = params.clone();
            if !cursor.is_empty() {
                p.push(("cursor", cursor.clone()));
            }
            let v = self.web.form(Token::Bot, method, &p).await?;
            out.extend(v["messages"].as_array().cloned().unwrap_or_default());
            cursor = v["response_metadata"]["next_cursor"].as_str().unwrap_or_default().to_string();
            if cursor.is_empty() || max.is_some_and(|m| out.len() >= m) {
                break;
            }
        }
        Ok(out)
    }
}

fn ts_time(ts: &str) -> DateTime<Utc> {
    let (s, us) = ts.split_once('.').unwrap_or((ts, "0"));
    let micros: u32 = us.parse().unwrap_or(0);
    DateTime::from_timestamp(s.parse().unwrap_or(0), micros.saturating_mul(1000)).unwrap_or_default()
}

#[async_trait]
impl Surface for Slack {
    async fn run(&self, events: mpsc::Sender<SurfaceEvent>) {
        socket::run(self, events).await
    }

    async fn post(&self, req: PostRequest) -> Result<Posted> {
        // `<!channel>` and its kin would notify everyone; text from a session
        // never gets to do that. Real user mentions (`<@U…>`) pass.
        let text = req.text.replace("<!", "&lt;!");
        let mut body = json!({
            "channel": req.conversation, "text": text, "unfurl_links": false, "unfurl_media": false,
        });
        if let Some(t) = &req.thread {
            body["thread_ts"] = json!(t);
        }
        if let Some((name, label)) = &req.button {
            body["blocks"] = json!([
                {"type": "section", "text": {"type": "mrkdwn", "text": text}},
                {"type": "actions", "elements": [
                    {"type": "button", "action_id": name, "value": name, "text": {"type": "plain_text", "text": label}}
                ]}
            ]);
        }
        let v = self.web.json("chat.postMessage", &body).await?;
        let id = v["ts"].as_str().context("chat.postMessage gave no ts")?.to_string();
        Ok(Posted {
            link: self.link(&req.conversation, &id, req.thread.as_deref()),
            id,
        })
    }

    async fn react(&self, conversation: &str, id: &str, reaction: &str) -> Result<()> {
        let p = [
            ("channel", conversation.to_string()),
            ("timestamp", id.to_string()),
            ("name", reaction.to_string()),
        ];
        match self.web.form(Token::Bot, "reactions.add", &p).await {
            Err(e) if e.to_string().contains("already_reacted") => Ok(()),
            r => r.map(|_| ()),
        }
    }

    async fn thread(&self, conversation: &str, id: &str, from: Option<&str>) -> Result<Vec<Msg>> {
        let fetch = |ts: String| async move {
            let mut p = vec![("channel", conversation.to_string()), ("ts", ts), ("limit", "200".into())];
            if let Some(f) = from {
                p.push(("oldest", f.to_string()));
            }
            self.pages("conversations.replies", p, None).await
        };
        let mut raw = fetch(id.to_string()).await?;
        // Asked with a reply's ID, Slack returns only that reply: ask again with the root's.
        if let Some(root) = raw.first().and_then(|m| m["thread_ts"].as_str()).filter(|root| *root != id) {
            raw = fetch(root.to_string()).await?;
        }
        Ok(raw.iter().filter_map(|m| self.parse(conversation, m)).collect())
    }

    async fn recent(&self, conversation: &str, from: Option<&str>, limit: Option<usize>) -> Result<Vec<Msg>> {
        let mut p = vec![
            ("channel", conversation.to_string()),
            ("limit", limit.unwrap_or(200).min(200).to_string()),
        ];
        if let Some(f) = from {
            p.push(("oldest", f.to_string()));
        }
        let mut raw = self.pages("conversations.history", p, limit).await?;
        raw.reverse(); // Slack returns newest first
        let mut msgs: Vec<Msg> = raw.iter().filter_map(|m| self.parse(conversation, m)).collect();
        if let Some(l) = limit {
            msgs.drain(..msgs.len().saturating_sub(l));
        }
        Ok(msgs)
    }

    async fn whoami(&self, conversations: &[String]) -> Result<WhoAmI> {
        let mut external = std::collections::BTreeMap::new();
        for c in conversations {
            let v = self.web.form(Token::Bot, "conversations.info", &[("channel", c.clone())]).await?;
            if v["channel"]["is_member"] == false {
                bail!("the bot is not in channel {c}: run /invite in it");
            }
            external.insert(c.clone(), v["channel"]["is_ext_shared"].as_bool().unwrap_or(false));
        }
        Ok(WhoAmI {
            bot: self.bot_user.clone(),
            external,
        })
    }
}
