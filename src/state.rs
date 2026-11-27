//! Thread files and the recovery cursor. Every write replaces the whole file:
//! temp file, fsync, rename, fsync of the directory. A file that fails to parse
//! is moved aside and treated as absent (see `load_thread`).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::source::RefStart;

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
    /// The bot has posted why nobody was woken, and nothing has woken anyone
    /// since: the next message that wakes nobody is not explained again.
    #[serde(default)]
    pub explained: bool,
    #[serde(default)]
    pub streak: u32,
    #[serde(default)]
    pub paused: bool,
    /// The thread's message that waits in a source's inbox. At most one.
    #[serde(default)]
    pub filed: Option<FiledItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FiledItem {
    pub source: String,
    pub item: String,
    /// Where it waits, as the bot's post says it.
    pub place: String,
    /// The message that was filed.
    pub message: String,
    /// The bot's post that names the item, once made: the router's note goes there.
    #[serde(default)]
    pub post: Option<String>,
    /// The router's one question about the item.
    #[serde(default)]
    pub question: Question,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Question {
    #[default]
    None,
    /// Posted in the thread: a person's next message there routes the item again.
    Asked,
    /// That second routing came to an end: later messages route nothing.
    Answered,
}

/// What to deliver to one person: the refs chosen, or whatever the rule picks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Pick {
    #[serde(rename = "rule")]
    Rule,
    #[serde(untagged)]
    Refs(Vec<String>),
}

/// A message nobody was woken for, with what was asked for it. Replaying
/// `picks` through delivery is how it is woken later.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "PendingFile")]
pub struct Pending {
    pub message: String,
    pub picks: BTreeMap<String, Pick>,
}

/// Builds before `picks` kept only the person, and always meant the rule.
#[derive(Deserialize)]
struct PendingFile {
    message: String,
    #[serde(default)]
    picks: BTreeMap<String, Pick>,
    #[serde(default)]
    person: Option<String>,
}

impl From<PendingFile> for Pending {
    fn from(f: PendingFile) -> Self {
        let mut picks = f.picks;
        if let Some(p) = f.person {
            picks.entry(p).or_insert(Pick::Rule);
        }
        Pending { message: f.message, picks }
    }
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

#[derive(Clone)]
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

    /// `None` is a thread with no file. A file that cannot be parsed is moved
    /// aside and counts as absent: the scope, the waiting message and `handled`
    /// are lost, a reply without the mention is ignored from then on, and with
    /// a default configured a redelivered first message wakes it again. The log
    /// says where the old file went. Any other read error (permissions, too
    /// many open files) is an error: the intact file must not be replaced.
    pub fn load_thread(&self, conversation: &str, thread: &str) -> Result<Option<ThreadState>> {
        let path = self.thread_path(conversation, thread);
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        match serde_json::from_slice(&bytes) {
            Ok(s) => Ok(Some(s)),
            Err(e) => {
                let aside = path.with_extension(format!("json.corrupt-{}", chrono::Utc::now().timestamp()));
                tracing::error!(
                    "{} is unreadable ({e}); moved to {}, treating the thread as new",
                    path.display(),
                    aside.display()
                );
                let _ = fs::rename(&path, &aside);
                Ok(None)
            }
        }
    }

    pub fn save_thread(&self, state: &ThreadState) -> Result<()> {
        write_atomic(
            &self.thread_path(&state.conversation, &state.thread),
            &serde_json::to_vec_pretty(state)?,
        )
    }

    /// Every thread file that parses: for `list`'s waiting messages, and to find
    /// the thread an inbox item was filed from.
    pub fn all_threads(&self) -> Vec<ThreadState> {
        let Ok(rd) = fs::read_dir(self.dir.join("threads")) else {
            return vec![];
        };
        rd.flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
            .filter_map(|e| serde_json::from_slice(&fs::read(e.path()).ok()?).ok())
            .collect()
    }

    /// Makes every stored ref of `source` start the way the source starts
    /// them now, and records that start. A source names its sessions anew (a
    /// machine is renamed) and a thread keeps the sessions it chose. Run
    /// before any message is handled. Gives the threads changed.
    ///
    /// The record is written last: a daemon killed part-way rewrites the rest
    /// at its next start, and a ref already rewritten no longer matches.
    pub fn settle_refs(&self, source: &str, start: &RefStart) -> Result<usize> {
        let path = self.dir.join("ref-starts.json");
        let mut recorded: BTreeMap<String, String> = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let old = recorded.get(source).unwrap_or(&start.unrecorded);
        let changed = match *old == start.now {
            true => 0,
            false => self.rename_refs(old, &start.now)?,
        };
        if recorded.get(source) != Some(&start.now) {
            recorded.insert(source.to_string(), start.now.clone());
            write_atomic(&path, &serde_json::to_vec_pretty(&recorded)?)?;
        }
        Ok(changed)
    }

    fn rename_refs(&self, old: &str, new: &str) -> Result<usize> {
        let mut changed = 0;
        for mut st in self.all_threads() {
            let before = serde_json::to_vec(&st)?;
            let picked = st
                .pending
                .iter_mut()
                .flat_map(|p| p.picks.values_mut())
                .filter_map(|pick| match pick {
                    Pick::Refs(refs) => Some(refs),
                    Pick::Rule => None,
                });
            for r in st.scope.values_mut().chain(st.woken.values_mut()).chain(picked).flatten() {
                if let Some(rest) = r.strip_prefix(old) {
                    *r = format!("{new}{rest}");
                }
            }
            if serde_json::to_vec(&st)? != before {
                self.save_thread(&st)?;
                changed += 1;
            }
        }
        Ok(changed)
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

/// Each write has its own temp name, so two writes of one file at once cannot
/// truncate each other's temp file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().context("state path has no parent")?;
    let tmp = dir.join(format!(
        "{}.{}.tmp",
        path.file_name().unwrap().to_string_lossy(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut f = fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    fs::rename(&tmp, path)?;
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}
