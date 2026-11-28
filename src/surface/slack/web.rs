//! A small Slack Web API client. Calls that hit the rate limit wait out
//! `Retry-After` and go again: a 429 means Slack did not process the call.

use std::time::Duration;

use anyhow::Result;
use serde_json::Value;

pub struct Web {
    http: reqwest::Client,
    bot: String,
    app: String,
}

/// A failed call, and whether Slack may have carried it out anyway: the
/// request went out and no answer said it was refused.
#[derive(Debug, thiserror::Error)]
#[error("slack {method}: {text}")]
pub struct CallError {
    pub method: String,
    pub text: String,
    pub unanswered: bool,
}

pub enum Token {
    Bot,
    App,
}

impl Web {
    pub fn new(bot: String, app: String) -> Result<Self> {
        // A connect that stalls is then a connect error: Slack never got the request.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { http, bot, app })
    }

    /// Form-encoded call: every method accepts it. `chat.postMessage` uses `json`.
    pub async fn form(&self, token: Token, method: &str, params: &[(&str, String)]) -> Result<Value, CallError> {
        self.send(token, method, |rb| rb.form(params)).await
    }

    pub async fn json(&self, method: &str, body: &Value) -> Result<Value, CallError> {
        self.send(Token::Bot, method, |rb| rb.json(body)).await
    }

    async fn send(
        &self,
        token: Token,
        method: &str,
        body: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    ) -> Result<Value, CallError> {
        let fail = |unanswered: bool, text: String| CallError {
            method: method.to_string(),
            text,
            unanswered,
        };
        let token = match token {
            Token::Bot => &self.bot,
            Token::App => &self.app,
        };
        for attempt in 0..6 {
            let rb = self.http.post(format!("https://slack.com/api/{method}")).bearer_auth(token);
            // Only a request that never had a connection is known not to have
            // reached Slack; a timeout or a dropped connection may come after
            // Slack carried it out.
            let resp = body(rb)
                .send()
                .await
                .map_err(|e| fail(!(e.is_connect() || e.is_builder()), format!("request failed: {e}")))?;
            let status = resp.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
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
            if !status.is_success() {
                return Err(fail(status.is_server_error(), format!("HTTP {status}")));
            }
            let v: Value = resp.json().await.map_err(|e| fail(true, format!("bad JSON: {e}")))?;
            if v["ok"].as_bool() != Some(true) {
                let code = v["error"].as_str().unwrap_or("unknown_error");
                let mut err = code.to_string();
                // `invalid_blocks` and kin say what is wrong only here.
                if let Some(details) = v["response_metadata"]["messages"].as_array() {
                    let details: Vec<&str> = details.iter().filter_map(Value::as_str).collect();
                    err = format!("{err}: {}", details.join("; "));
                }
                if let Some(needed) = v["needed"].as_str() {
                    err = format!("{err} (needs scope {needed})");
                }
                // Slack documents that these two may follow a partial success.
                return Err(fail(matches!(code, "internal_error" | "fatal_error"), err));
            }
            return Ok(v);
        }
        Err(fail(false, "still rate limited after 6 tries".into()))
    }
}
