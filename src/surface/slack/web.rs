//! A small Slack Web API client. Calls that hit the rate limit wait out
//! `Retry-After` and go again: a 429 means Slack did not process the call.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

pub struct Web {
    http: reqwest::Client,
    bot: String,
    app: String,
}

pub enum Token {
    Bot,
    App,
}

impl Web {
    pub fn new(bot: String, app: String) -> Result<Self> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?;
        Ok(Self { http, bot, app })
    }

    /// Form-encoded call: every method accepts it. `chat.postMessage` uses `json`.
    pub async fn form(&self, token: Token, method: &str, params: &[(&str, String)]) -> Result<Value> {
        self.send(token, method, |rb| rb.form(params)).await
    }

    pub async fn json(&self, method: &str, body: &Value) -> Result<Value> {
        self.send(Token::Bot, method, |rb| rb.json(body)).await
    }

    async fn send(&self, token: Token, method: &str, body: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder) -> Result<Value> {
        let token = match token {
            Token::Bot => &self.bot,
            Token::App => &self.app,
        };
        for attempt in 0..6 {
            let rb = self.http.post(format!("https://slack.com/api/{method}")).bearer_auth(token);
            let resp = body(rb).send().await.with_context(|| format!("slack {method}: request failed"))?;
            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(1);
                tracing::warn!("slack {method}: rate limited, waiting {wait}s (attempt {})", attempt + 1);
                tokio::time::sleep(Duration::from_secs(wait.clamp(1, 60))).await;
                continue;
            }
            if !resp.status().is_success() {
                bail!("slack {method}: HTTP {}", resp.status());
            }
            let v: Value = resp.json().await.with_context(|| format!("slack {method}: bad JSON"))?;
            if v["ok"].as_bool() != Some(true) {
                let err = v["error"].as_str().unwrap_or("unknown_error");
                match v["needed"].as_str() {
                    Some(needed) => bail!("slack {method}: {err} (needs scope {needed})"),
                    None => bail!("slack {method}: {err}"),
                }
            }
            return Ok(v);
        }
        bail!("slack {method}: still rate limited after 6 tries")
    }
}
