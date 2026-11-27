//! Jev, the decision model the router asks: one HTTP request with a `state`
//! string and one `choice` question, answered with a probability per option.
//! It generates no text. This module knows nothing of sessions or sources.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(20);
/// The API's limit on a question's options.
pub const MAX_OPTIONS: usize = 255;
const QUESTION: &str = "session";

pub struct Jev {
    http: reqwest::Client,
    url: String,
    model: String,
    key: String,
}

pub struct Answer {
    pub choice: String,
    /// Per option, 0 to 1. Empty when the service sent none.
    pub probabilities: BTreeMap<String, f64>,
}

impl Answer {
    /// The probability of the chosen option: 0 when the service gave none.
    pub fn probability(&self) -> f64 {
        self.probabilities.get(&self.choice).copied().unwrap_or(0.0)
    }
}

impl Jev {
    pub fn new(url: String, model: String, key: String) -> Result<Jev> {
        Ok(Jev {
            http: reqwest::Client::builder().timeout(TIMEOUT).build()?,
            url,
            model,
            key,
        })
    }

    /// `criteria` maps each option to its description. Any answer but a 2xx
    /// with a choice in it is an error; the error never holds the key.
    pub async fn choose(&self, state: &str, instructions: &str, criteria: &BTreeMap<String, String>) -> Result<Answer> {
        let body = json!({
            "model": self.model,
            "state": state,
            "questions": {QUESTION: {"type": "choice", "instructions": instructions, "criteria": criteria}},
        });
        let resp = self
            .http
            .post(&self.url)
            .bearer_auth(&self.key)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("no answer from the model: {}", e.without_url()))?;
        let status = resp.status();
        let text = resp.text().await.context("the model's answer broke off")?;
        if !status.is_success() {
            tracing::warn!("the model answered {status}: {}", cut(&text));
            bail!("the model answered HTTP {}", status.as_u16());
        }
        let parsed = serde_json::from_str::<Value>(&text).ok().and_then(|v| {
            let a = &v["answers"][QUESTION];
            let choice = a["choice"].as_str()?.to_string();
            let mut probabilities: BTreeMap<String, f64> = a["probabilities"]
                .as_object()
                .map(|m| m.iter().filter_map(|(k, p)| Some((k.clone(), p.as_f64()?))).collect())
                .unwrap_or_default();
            if let (false, Some(c)) = (probabilities.contains_key(&choice), a["confidence"].as_f64()) {
                probabilities.insert(choice.clone(), c);
            }
            Some(Answer { choice, probabilities })
        });
        match parsed {
            Some(a) => Ok(a),
            None => {
                tracing::warn!("the model's answer has no choice in it: {}", cut(&text));
                bail!("the model's answer could not be read")
            }
        }
    }
}

/// A response body in a log line: its first 500 bytes.
fn cut(s: &str) -> &str {
    let end = (0..=s.len().min(500)).rev().find(|i| s.is_char_boundary(*i)).unwrap_or(0);
    &s[..end]
}
