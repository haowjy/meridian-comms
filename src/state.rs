//! Thread files and the recovery cursor. Every write replaces the whole file:
//! temp file, fsync, rename, fsync of the directory. A file that fails to parse
//! is moved aside and treated as absent (see `load_thread`).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const HANDLED_KEEP: usize = 50;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreadState {
    pub conversation: String,
    pub thread: String,
    #[serde(default)]
    pub handled: Vec<String>,
    #[serde(default)]
    pub scope: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub woken: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub pending: Option<Pending>,
    #[serde(default)]
    pub streak: u32,
    #[serde(default)]
    pub paused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub message: String,
    pub person: String,
}

impl ThreadState {
    pub fn new(conversation: &str, thread: &str) -> Self {
        Self {
            conversation: conversation.into(),
            thread: thread.into(),
            ..Default::default()
        }
    }

    /// A message in `handled`, or older than a full list, is a redelivery.
    pub fn is_handled(&self, id: &str) -> bool {
        self.handled.iter().any(|h| h == id)
            || (self.handled.len() >= HANDLED_KEEP && self.handled.iter().min().is_some_and(|min| id < min.as_str()))
    }

    pub fn mark_handled(&mut self, id: &str) {
        if !self.handled.iter().any(|h| h == id) {
            self.handled.push(id.to_string());
            self.handled.sort();
            let excess = self.handled.len().saturating_sub(HANDLED_KEEP);
            self.handled.drain(..excess);
        }
    }

    pub fn scope_of(&self, person: &str) -> &[String] {
        self.scope.get(person).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn woken_of(&self, person: &str) -> &[String] {
        self.woken.get(person).map(Vec::as_slice).unwrap_or(&[])
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CursorFile {
    #[serde(default)]
    channels: BTreeMap<String, String>,
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn open(dir: PathBuf) -> Result<Store> {
        let threads = dir.join("threads");
        fs::create_dir_all(&threads).with_context(|| format!("cannot create {}", threads.display()))?;
        for d in [&dir, &threads] {
            for e in fs::read_dir(d)?.flatten() {
                if e.file_name().to_string_lossy().ends_with(".tmp") {
                    let _ = fs::remove_file(e.path());
                }
            }
        }
        Ok(Store { dir })
    }

    fn thread_path(&self, conversation: &str, thread: &str) -> PathBuf {
        self.dir.join("threads").join(format!("{conversation}-{thread}.json"))
    }

    /// A file that cannot be parsed is moved aside and the thread is treated as
    /// new. That cannot wake anyone: with no scope a message goes to the
    /// defaults, and the Meridian source marks none. What is lost is the
    /// scope, the waiting message and `handled`; a reply without the mention is
    /// ignored from then on, and the log says where the old file went.
    pub fn load_thread(&self, conversation: &str, thread: &str) -> Option<ThreadState> {
        let path = self.thread_path(conversation, thread);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::error!("cannot read {}: {e}; treating the thread as new", path.display());
                return None;
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(s) => Some(s),
            Err(e) => {
                let aside = path.with_extension(format!("json.corrupt-{}", chrono::Utc::now().timestamp()));
                tracing::error!(
                    "{} is unreadable ({e}); moved to {}, treating the thread as new",
                    path.display(),
                    aside.display()
                );
                let _ = fs::rename(&path, &aside);
                None
            }
        }
    }

    pub fn save_thread(&self, state: &ThreadState) -> Result<()> {
        write_atomic(
            &self.thread_path(&state.conversation, &state.thread),
            &serde_json::to_vec_pretty(state)?,
        )
    }

    /// Threads whose file changed within `window` of now.
    pub fn recent_threads(&self, window: Duration) -> Vec<(String, String)> {
        let cutoff = SystemTime::now() - window;
        let Ok(rd) = fs::read_dir(self.dir.join("threads")) else {
            return vec![];
        };
        rd.flatten()
            .filter(|e| e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t >= cutoff))
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let (conv, thread) = name.strip_suffix(".json")?.split_once('-')?;
                Some((conv.to_string(), thread.to_string()))
            })
            .collect()
    }

    /// Every thread file that parses, for `list`'s waiting messages.
    pub fn all_threads(&self) -> Vec<ThreadState> {
        let Ok(rd) = fs::read_dir(self.dir.join("threads")) else {
            return vec![];
        };
        rd.flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
            .filter_map(|e| serde_json::from_slice(&fs::read(e.path()).ok()?).ok())
            .collect()
    }

    pub fn load_cursor(&self) -> BTreeMap<String, String> {
        let path = self.dir.join("cursor.json");
        match fs::read(&path) {
            Ok(b) => serde_json::from_slice::<CursorFile>(&b).map(|c| c.channels).unwrap_or_else(|e| {
                tracing::error!("{} is unreadable ({e}); recovery restarts from now", path.display());
                BTreeMap::new()
            }),
            Err(_) => BTreeMap::new(),
        }
    }

    pub fn save_cursor(&self, channels: &BTreeMap<String, String>) -> Result<()> {
        write_atomic(
            &self.dir.join("cursor.json"),
            &serde_json::to_vec_pretty(&CursorFile {
                channels: channels.clone(),
            })?,
        )
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("state path has no parent")?;
    let tmp = dir.join(format!("{}.tmp", path.file_name().unwrap().to_string_lossy()));
    let mut f = fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}
