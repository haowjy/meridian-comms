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
    /// Per option, 0 to 1. An option the service gave no probability counts
    /// as 0; its `confidence` is another number and is not read.
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
    /// with a choice in it is an error; the error never holds the key, and no
    /// log line holds the answer's body: a service may echo the request.
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
            tracing::warn!("the model answered {status}, with a body of {} bytes", text.len());
            bail!("the model answered HTTP {}", status.as_u16());
        }
        let parsed = serde_json::from_str::<Value>(&text).ok().and_then(|v| {
            let a = &v["answers"][QUESTION];
            let choice = a["choice"].as_str()?.to_string();
            let probabilities = a["probabilities"]
                .as_object()
                .map(|m| m.iter().filter_map(|(k, p)| Some((k.clone(), p.as_f64()?))).collect())
                .unwrap_or_default();
            Some(Answer { choice, probabilities })
        });
        match parsed {
            Some(a) => Ok(a),
            None => {
                tracing::warn!("the model's answer ({status}, {} bytes) has no choice in it", text.len());
                bail!("the model's answer could not be read")
            }
        }
    }
}
