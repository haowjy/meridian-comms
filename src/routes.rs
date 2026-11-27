//! The routing log: `routes.jsonl` in the state directory, one JSON line per
//! routing. A line is appended with one `write` on an `O_APPEND` file, so a
//! crash leaves at most a torn last line, which readers skip. It never holds
//! message text.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::state::write_atomic;

const MAX_BYTES: u64 = 5 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    /// A session was chosen; `done` says whether the offer went out.
    Offer,
    /// Routed, and nobody is to be offered it.
    None,
    /// The second step's question was posted in the item's thread.
    Ask,
    /// The router is not configured, or has no key.
    Off,
    /// Over the limit on routings a minute.
    Capped,
    /// The run broke before a decision: see `error`.
    Failed,
}

impl Decision {
    /// Routed to an end: the item is not routed again. The others leave it
    /// waiting, and a later `route` for it runs.
    pub fn is_final(self) -> bool {
        matches!(self, Decision::Offer | Decision::None | Decision::Ask)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    pub time: DateTime<Utc>,
    pub source: String,
    pub item: String,
    /// The thread message the item was filed from, when this daemon filed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The refs the model was asked to choose among.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<String>,
    /// The model's choice: a ref, `none` or `more`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub probabilities: BTreeMap<String, f64>,
    /// The source's ID for a second-step run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record: Option<String>,
    pub decision: Decision,
    pub done: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join("routes.jsonl")
}

/// Every line that parses, oldest first. No file is no routings.
pub fn read(path: &Path) -> Result<Vec<Route>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    Ok(bytes
        .split(|b| *b == b'\n')
        .filter_map(|l| serde_json::from_slice(l).ok())
        .collect())
}

pub struct Log {
    path: PathBuf,
}

impl Log {
    /// At daemon start: a file over 5 MB is replaced by its newest half.
    pub fn open(state_dir: &Path) -> Result<Log> {
        let path = path(state_dir);
        if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
            let bytes = fs::read(&path)?;
            let half = bytes.len() / 2;
            let start = bytes[half..].iter().position(|b| *b == b'\n').map_or(bytes.len(), |i| half + i + 1);
            write_atomic(&path, &bytes[start..])?;
        }
        Ok(Log { path })
    }

    pub fn read(&self) -> Result<Vec<Route>> {
        read(&self.path)
    }

    pub fn append(&self, route: &Route) -> Result<()> {
        let mut line = serde_json::to_vec(route)?;
        line.push(b'\n');
        let mut f = fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        f.write_all(&line)?;
        f.sync_all()?;
        Ok(())
    }
}
