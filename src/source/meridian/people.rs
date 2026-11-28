//! `meridian-comms deliver`: the program Meridian runs for `meridian message @…`
//! when `[message] people` names it. Meridian passes the address as the last
//! argument and the text on stdin, and reads one line of JSON back; its error
//! kinds are a closed list where only `maybe_delivered` may have arrived.

use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::json;

use crate::client;
use crate::model::{FailKind, Failure};
use crate::proto::PostTo;

/// Meridian stops the program after 120 s; this leaves room to answer.
const ANSWER_WITHIN: Duration = Duration::from_secs(60);

/// What `setup` tells the person to add to Meridian's config, which this
/// program never writes.
pub const SETUP: &str = "For `meridian message @person \"...\"` to post through this bot, add to ~/.meridian/config.toml by hand:\n\n[message]\npeople = [\"meridian-comms\", \"deliver\"]";

/// Prints the one answer line, and exits 1 unless it is `ok`.
pub async fn deliver(address: String) {
    let answer = match post(&address).await {
        Ok(link) => json!({"ok": true, "delivery": "acknowledged", "note": link}),
        Err(e) => json!({"ok": false, "error": {"kind": kind(Failure::kind_of(&e)), "text": format!("{e:#}")}}),
    };
    println!("{answer}");
    if answer["ok"] != true {
        std::process::exit(1);
    }
}

/// `@<person>` or `@<channel>/<message id>`, as the wake notice names a message.
async fn post(address: &str) -> Result<Option<String>> {
    let to = match address.strip_prefix('@') {
        Some(r) if r.contains('/') => PostTo::Thread(r.to_string()),
        Some(p) if !p.is_empty() => PostTo::Person(p.to_string()),
        _ => bail!(Failure::new(
            FailKind::Invalid,
            format!("{address:?} is not an address: expected @<person> or @<channel>/<message id>")
        )),
    };
    let mut text = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
        .map_err(|e| Failure::new(FailKind::Invalid, format!("cannot read the message text on stdin: {e}")))?;
    let out = client::post(to, text.trim_end().to_string(), ANSWER_WITHIN).await?;
    Ok(out.link)
}

fn kind(k: FailKind) -> &'static str {
    match k {
        FailKind::Invalid => "usage",
        FailKind::NotFound => "not_found",
        FailKind::Ambiguous => "ambiguous",
        FailKind::NoChannel => "no_route",
        FailKind::NotAllowed => "refused",
        FailKind::TooLarge => "too_large",
        FailKind::Unreachable => "unreachable",
        FailKind::NotSent => "failed",
        FailKind::MaybeSent => "maybe_delivered",
    }
}
