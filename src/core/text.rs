//! The words the daemon posts and prints. Text is the base form of everything:
//! a surface without menus or buttons loses only those.

use chrono::{DateTime, Local, Utc};

use crate::model::{Activity, ActivityState, Candidate};

/// Text from a source (labels, models, groups) goes into posts: drop what a
/// surface could read as markup or a link.
pub fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if matches!(c, '<' | '>' | '&' | '`' | '\n' | '\r') { ' ' } else { c })
        .collect()
}

pub fn duration(d: chrono::Duration) -> String {
    let mins = d.num_minutes().max(0);
    if mins < 120 {
        format!("{mins} m")
    } else {
        format!("{} h", (mins + 30) / 60)
    }
}

pub fn activity(a: Option<&Activity>, now: DateTime<Utc>) -> String {
    let Some(a) = a else { return "activity unknown".into() };
    match a.state {
        ActivityState::Working => "working".into(),
        ActivityState::Waiting => "waiting on its user".into(),
        ActivityState::Idle => {
            let idle = a
                .since
                .map(|s| format!("idle {}", duration(now - s)))
                .unwrap_or_else(|| "idle".into());
            match a.warm_until {
                Some(w) if w > now => format!("{idle}, warm {} more", duration(w - now)),
                Some(_) => format!("cold, {idle}"),
                None => idle,
            }
        }
    }
}

/// Why a candidate was skipped, for a candidate the source listed.
pub fn cold_reason(c: &Candidate, now: DateTime<Utc>) -> String {
    let a = c.activity.as_ref();
    let since = a
        .and_then(|a| a.since)
        .map(|s| format!("idle {}, ", duration(now - s)))
        .unwrap_or_default();
    let cold_at = a
        .and_then(|a| a.warm_until)
        .map(|w| w.with_timezone(&Local).format("%H:%M").to_string())
        .unwrap_or_default();
    format!("cold ({since}cache cold since {cold_at})")
}

/// One line per candidate, for the picker and for `list`.
pub fn candidate_line(c: &Candidate, now: DateTime<Utc>) -> String {
    let mut parts = vec![format!("`{}`", c.ref_)];
    if let Some(id) = &c.id {
        parts.push(format!("({})", clean(id)));
    }
    let facts: Vec<String> = [c.label.as_deref(), c.model.as_deref(), c.group.as_deref()]
        .into_iter()
        .flatten()
        .map(clean)
        .chain([activity(c.activity.as_ref(), now)])
        .collect();
    format!("{} {}", parts.join(" "), facts.join(" · "))
}

/// An option in a menu: short ID, label, model and activity. A surface cuts it to fit.
pub fn option_label(c: &Candidate, now: DateTime<Utc>) -> String {
    let short = c.id.as_deref().unwrap_or(&c.ref_);
    let facts: Vec<String> = [c.label.as_deref(), c.model.as_deref()]
        .into_iter()
        .flatten()
        .map(clean)
        .chain([activity(c.activity.as_ref(), now)])
        .collect();
    format!("{} · {}", clean(short), facts.join(" · "))
}

pub fn label(c: &Candidate) -> String {
    match &c.label {
        Some(l) => format!("`{}` ({})", c.ref_, clean(l)),
        None => format!("`{}`", c.ref_),
    }
}
