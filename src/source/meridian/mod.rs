//! The Meridian source. It uses only Meridian's public CLI: `spawn list
//! --global --json` here and on each saved machine for the running sessions,
//! `message --wake` to wake one, `message inbox` and `inbox read/take/offer`
//! for the machine's inbox, and `machine name` and `machine list` for which
//! machines there are.
//!
//! A ref is `<machine ID>/<project ID>/<spawn ID>`: the machine's ID, which no
//! rename changes. How Meridian addresses the session is looked up from it
//! (`address`): the bare spawn ID here, `<saved name>/<spawn ID>` on a saved
//! machine.

pub mod people;
pub mod router;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::future::join_all;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::{RefStart, Source};
use crate::config::{Config, MeridianConfig, is_id};
use crate::model::{
    Activity, ActivityState, CallerKind, Candidate, DecideRequest, Decision, FileRequest, Filed, Group, IdentifyRequest, IdentifyResult,
    InboxItem, ListResponse, Machine, NoticeKind, OfferResult, Offered, WakeRequest, WakeResult,
};

const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// How long `machine list`, and then each saved machine's listing, may take:
/// together under the core's 30 s for a listing across machines. Measured on
/// raspberry asking homebox: 4.9 s alone, 9.5 s beside five other listings;
/// `ssh` itself gives up connecting after 10.
const MACHINE_TIMEOUT: Duration = Duration::from_secs(12);
/// What this program calls itself in an inbox item's `via` and in a claim.
const PROGRAM: &str = "meridian-comms";

pub struct Meridian {
    /// Meridian's name for this machine, asked by `check` once: what people
    /// see, and what this machine's sessions answer to.
    machine: String,
    /// The names Meridian says this machine had before.
    was: Vec<String>,
    /// This machine's ID, which no rename changes: what every ref to a
    /// session here starts with, and what an inbox item's sender carries.
    id: String,
    owner: String,
    default: Option<String>,
    projects: BTreeMap<String, Project>,
    /// A listing every machine answered has been compared with the
    /// configured projects: each key is warned about once.
    keys_checked: AtomicBool,
}

/// A machine Meridian has saved: the ID that never changes, and the name
/// it is saved under, which is what `--machine` and `<machine>/<id>` take.
struct Saved {
    id: String,
    name: String,
}

struct Project {
    default: Option<String>,
}

#[derive(Deserialize)]
struct Rows {
    #[serde(default)]
    spawns: Vec<Row>,
}

#[derive(Deserialize)]
struct Row {
    spawn_id: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    work_id: Option<String>,
    #[serde(default)]
    idle: Option<Idle>,
    #[serde(default)]
    activity: Option<String>,
    /// Only in a `--global` listing.
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    project_root: Option<String>,
}

#[derive(Deserialize)]
struct Idle {
    since: Option<DateTime<Utc>>,
    warm_until: Option<DateTime<Utc>>,
    #[serde(default)]
    ttl_estimated: bool,
}

impl Meridian {
    pub fn new(cfg: &Config, m: &MeridianConfig) -> Result<Self> {
        // A filed message carries its sender's directory name and its channel's
        // name as fields, and Meridian refuses a field outside its safe set.
        let names = cfg
            .people
            .keys()
            .map(|name| (format!("people.{name}"), name))
            .chain(cfg.channels().map(|(id, ch)| (format!("channel {id}"), &ch.name)));
        for (at, name) in names {
            if !is_field(name) {
                bail!("{at}: the name {name:?} cannot go in Meridian's inbox, which takes {FIELD_RULE}");
            }
        }
        for (key, p) in &m.projects {
            let at = format!("sources.meridian.projects.{key}");
            if !is_project_id(key) {
                bail!(
                    "{at}: the key must be a Meridian project ID (what `meridian context` prints as `project_id` in the project), which takes {FIELD_RULE} in parts joined by `/`, none starting with `.`"
                );
            }
            if p.root.is_some() {
                tracing::warn!(
                    "{at}.root is no longer read: a project is its ID, and its agents may run on any machine the bot reaches. Remove it from {}",
                    crate::config::config_path().display()
                );
            }
        }
        let projects = m
            .projects
            .iter()
            .map(|(name, p)| (name.clone(), Project { default: p.default.clone() }))
            .collect();
        Ok(Self {
            machine: String::new(),
            was: vec![],
            id: String::new(),
            owner: cfg.owner.clone(),
            default: m.default.clone(),
            projects,
            keys_checked: AtomicBool::new(false),
        })
    }

    /// The machine ID, project and spawn ID of a Meridian ref. A project ID
    /// may itself hold a `/`, so the machine comes off the front and the spawn
    /// ID off the end.
    fn split_ref<'a>(&self, r: &'a str) -> Option<(&'a str, &'a str, &'a str)> {
        let (machine, rest) = r.split_once('/')?;
        let (project, id) = rest.rsplit_once('/')?;
        (is_id(machine) && is_id(id)).then_some((machine, project, id))
    }

    /// The lookup: how a Meridian command on this machine names a listed
    /// session. The spawn ID for one here; behind the name its machine is
    /// saved under, which the listing read with it, for one elsewhere. Two
    /// machines can each have a `p41`.
    pub(super) fn address(&self, c: &Candidate) -> Option<String> {
        let (machine, _, id) = self.split_ref(&c.ref_)?;
        if machine == self.id {
            return Some(id.to_string());
        }
        Some(format!("{}/{id}", c.machine.as_deref().filter(|m| is_id(m))?))
    }

    /// What a session is listed as: its ref, and the words that also name it,
    /// each of which `resolve` matches. The spawn ID, `<machine>/<spawn ID>`,
    /// and here, for one minor release, the name-based ref older builds
    /// stored and printed, under this machine's current name and each past one.
    fn ref_and_names(&self, machine: &Saved, project: &str, spawn: &str) -> (String, Vec<String>) {
        let mut names = vec![spawn.to_string(), format!("{}/{spawn}", machine.name)];
        if machine.id == self.id {
            names.extend(
                std::iter::once(&self.machine)
                    .chain(&self.was)
                    .map(|n| format!("{n}/{project}/{spawn}")),
            );
        }
        (format!("{}/{project}/{spawn}", machine.id), names)
    }

    /// The ref of the session that filed an item, from the item's `from`:
    /// `None` when no session did, or none a candidate could be.
    ///
    /// `from` names the sender's chat and machine; a candidate is named by its
    /// spawn ID, so the ref takes one more call, on the sender's machine. That
    /// machine is told by its ID, which no rename changes, or by its name on
    /// an item that has no ID: this one, a saved one (the ref then takes the
    /// saved name, as a candidate's does), or neither, and a machine nobody
    /// saved lists no candidates, so its chat ID is not looked up: here it
    /// would name some other session.
    /// A chat Meridian does not find is taken as gone, so not a candidate; any
    /// other failure, a machine that can't be reached among them, is an
    /// error, or the sender would be offered its own message.
    async fn sender_ref(&self, sender: &Value) -> Result<Option<String>> {
        let Some(chat) = field(&sender["id"]).filter(|id| sender["kind"] == "session" && is_id(id)) else {
            return Ok(None);
        };
        let (id, name) = (field(&sender["machine_id"]), field(&sender["machine"]));
        // An item from before the ID was written carries a name alone, and
        // Meridian's own rule reads it: a name this machine has or had is this
        // machine, before any saved machine that has since taken it.
        let own = |name: &String| *name == self.machine || self.was.contains(name);
        let here = match &id {
            Some(id) => *id == self.id,
            None => name.as_ref().is_none_or(own),
        };
        let machine = if here {
            None
        } else {
            let saved = self.saved_machines().await.context("cannot tell which machine sent it")?;
            let saved = saved.into_iter().find(|m| match &id {
                Some(id) => m.id == *id,
                None => Some(&m.name) == name.as_ref(),
            });
            match saved {
                Some(m) => Some(m),
                None => {
                    // The ID is read once, at start: this machine got a new
                    // one since (its machine.json was replaced), and until a
                    // restart no sender here can be told from the candidates.
                    if id.is_some() && name.as_ref() == Some(&self.machine) {
                        tracing::warn!(
                            "an inbox item's sender is on {} under a machine ID this daemon does not know: this machine's ID changed since the daemon started, so a session here can be offered its own message. Restart the daemon",
                            self.machine
                        );
                    }
                    return Ok(None);
                }
            }
        };
        let mut args = vec!["spawn", "show", &chat, "--json"];
        if let Some(m) = &machine {
            args.extend(["--machine", &m.name]);
        }
        let out = run(None, &args).await?;
        let shown: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        match field(&shown["spawn_id"]).filter(|id| is_id(id)) {
            // Composed as a candidate's ref is, so the router can leave the
            // sender out.
            Some(id) => {
                let machine = machine.map_or_else(|| self.id.clone(), |m| m.id);
                Ok(field(&sender["project"]).map(|project| format!("{machine}/{project}/{id}")))
            }
            None if not_found(&out) => Ok(None),
            None => bail!(
                "cannot tell which session sent it: `meridian spawn show {chat}` failed: {}{}",
                tail(&out.stdout),
                tail(&out.stderr)
            ),
        }
    }

    /// Every project's running sessions, primaries and spawns, on this
    /// machine or a saved one.
    async fn everywhere(&self, machine: Option<&str>) -> Result<Rows> {
        let mut args = vec!["spawn", "list", "--global", "--json"];
        if let Some(m) = machine {
            args.extend(["--machine", m]);
        }
        rows(run(None, &args).await?)
    }

    /// The machines Meridian has saved. A list that can't be read is an
    /// error, not an empty list: a caller that took it for "nothing saved"
    /// would look for a remote session here.
    async fn saved_machines(&self) -> Result<Vec<Saved>> {
        let out = tokio::time::timeout(MACHINE_TIMEOUT, run(None, &["machine", "list", "--json"]))
            .await
            .context("`meridian machine list` gave no answer")??;
        let listed: Value = serde_json::from_slice(&out.stdout)
            .ok()
            .filter(|_| out.status.success())
            .with_context(|| format!("`meridian machine list` failed: {}{}", tail(&out.stdout), tail(&out.stderr)))?;
        let machines = listed["machines"]
            .as_array()
            .context("`meridian machine list` printed no `machines`")?;
        Ok(machines
            .iter()
            .filter_map(|m| {
                Some(Saved {
                    id: field(&m["id"])?,
                    name: field(&m["name"]).filter(|n| is_id(n))?,
                })
            })
            .collect())
    }

    /// Every running primary on this machine and each saved one, with the
    /// groups their sessions run in and how each machine answered. Every
    /// machine is asked at once, and each saved one has its own limit: one that
    /// is down or slow contributes nothing, holds the listing no longer than
    /// that, and is named in `machines`.
    async fn everything(&self) -> Result<ListResponse> {
        let mut out = ListResponse::default();
        let elsewhere = async {
            // The sessions here are still worth listing without the others.
            let asked = Instant::now();
            let saved = self.saved_machines().await;
            let took = asked.elapsed().as_secs_f32();
            let saved = match saved {
                Ok(saved) => {
                    tracing::info!("machine list answered in {took:.1} s: {} saved", saved.len());
                    saved
                }
                Err(e) => {
                    tracing::warn!("no saved machine's sessions are listed: machine list failed after {took:.1} s: {e:#}");
                    return Err(());
                }
            };
            Ok(join_all(saved.into_iter().map(|m| async move {
                let asked = Instant::now();
                let rows = match tokio::time::timeout(MACHINE_TIMEOUT, self.everywhere(Some(&m.name))).await {
                    Ok(rows) => rows,
                    Err(_) => Err(anyhow::anyhow!("no answer in {} s", MACHINE_TIMEOUT.as_secs())),
                };
                let took = asked.elapsed().as_secs_f32();
                match &rows {
                    Ok(r) => tracing::info!("{} answered in {took:.1} s: {} session(s)", m.name, r.spawns.len()),
                    Err(e) => tracing::warn!("no sessions listed from {} after {took:.1} s: {e:#}", m.name),
                }
                (m, rows)
            }))
            .await)
        };
        let (here, there) = tokio::join!(self.everywhere(None), elsewhere);
        let this = Saved {
            id: self.id.clone(),
            name: self.machine.clone(),
        };
        out.machines.push(Machine {
            id: Some(this.id.clone()),
            name: Some(this.name.clone()),
            ok: true,
            why: None,
        });
        let mut listed = vec![(this, here?)];
        match there {
            Ok(there) => {
                for (m, rows) in there {
                    out.machines.push(Machine {
                        id: Some(m.id.clone()),
                        name: Some(m.name.clone()),
                        ok: rows.is_ok(),
                        why: rows.as_ref().err().map(|e| format!("{e:#}")),
                    });
                    listed.extend(rows.ok().map(|r| (m, r)));
                }
            }
            Err(()) => out.machines.push(Machine {
                id: None,
                name: None,
                ok: false,
                why: Some("saved machines unreadable".into()),
            }),
        }
        for (machine, rows) in listed {
            // Per project on this machine: its groups, and the folders its rows run in.
            #[derive(Default)]
            struct Seen {
                groups: BTreeMap<Option<String>, u32>,
                roots: Vec<(bool, String)>,
            }
            let mut projects: BTreeMap<String, Seen> = BTreeMap::new();
            for mut row in rows.spawns {
                let Some(project) = row.project_id.take() else { continue };
                let Seen { groups, roots } = projects.entry(project.clone()).or_default();
                let candidate = row.kind == "primary";
                // A primary that is starting or ending can't take a message
                // and is no other session either: it is not counted.
                if candidate && row.status != "running" {
                    continue;
                }
                *groups.entry(row.work_id.clone()).or_default() += u32::from(!candidate);
                roots.extend(row.project_root.clone().map(|r| (candidate, r)));
                if candidate {
                    out.candidates.push(self.candidate(&machine, &project, row));
                }
            }
            for (project, Seen { groups, roots }) in projects {
                let project_label = project_label(&roots);
                out.groups.extend(groups.into_iter().map(|(group, others)| Group {
                    machine: machine.id.clone(),
                    project: project.clone(),
                    project_label: project_label.clone(),
                    group,
                    others,
                }));
            }
        }
        self.check_keys(&out);
        Ok(out)
    }

    /// Warns once about each configured project no session was seen in, the
    /// first time every machine answered: nothing else checks a key.
    fn check_keys(&self, out: &ListResponse) {
        if out.machines.iter().any(|m| !m.ok) || self.keys_checked.swap(true, Ordering::Relaxed) {
            return;
        }
        for key in self.projects.keys().filter(|k| !out.groups.iter().any(|g| g.project == **k)) {
            tracing::warn!(
                "no agent seen in sources.meridian.projects.{key} on any machine: check the ID against `meridian context` in the project"
            );
        }
    }

    /// `machine` is the machine the listing was asked on, by its ID and the
    /// name it is saved under, which is what Meridian's commands take. What a
    /// row calls its own machine is not read.
    fn candidate(&self, machine: &Saved, project: &str, row: Row) -> Candidate {
        let here = machine.id == self.id;
        let configured = self.projects.get(project);
        let own = configured.and_then(|p| p.default.clone());
        let default = configured.is_some() && row.agent.is_some() && row.agent == own.or_else(|| self.default.clone());
        // `idle` is measured from the last turn the user's loop ended, so a row
        // can carry it while a turn runs: `activity` wins.
        let activity = match row.activity.as_deref() {
            Some("turn_active" | "starting" | "finalizing") => Some(Activity {
                state: ActivityState::Working,
                since: None,
                warm_until: None,
                note: None,
            }),
            _ => row.idle.map(|i| Activity {
                state: ActivityState::Idle,
                since: i.since,
                warm_until: i.warm_until,
                note: i.ttl_estimated.then(|| "cache expiry estimated".to_string()),
            }),
        };
        let (ref_, names) = self.ref_and_names(machine, project, &row.spawn_id);
        Candidate {
            ref_,
            id: Some(row.spawn_id),
            names,
            person: self.owner.clone(),
            machine: Some(machine.name.clone()),
            project: Some(project.to_string()),
            group: row.work_id,
            label: row.agent,
            model: row.model,
            default,
            // A configured project, on any machine the listing reached.
            wakeable: configured.is_some(),
            activity,
            // A path on another machine is no use to a command run here.
            root: row.project_root.filter(|_| here).map(PathBuf::from),
        }
    }
}

/// The name of the folder a project lives in. Meridian's rows give only the
/// folder each session runs in, which can be a worktree's: the folder most of
/// its primaries run in (people start those in the main checkout), else most
/// of its sessions. A worktree in `<name>.worktrees/` (worktrunk's layout) is
/// named for `<name>`; one elsewhere can't be told from a checkout.
fn project_label(roots: &[(bool, String)]) -> Option<String> {
    let last = |p: &Path| {
        let worktrees = p
            .ancestors()
            .filter_map(|a| a.file_name()?.to_str()?.strip_suffix(".worktrees"))
            .find(|n| !n.is_empty());
        worktrees
            .map(String::from)
            .or_else(|| p.file_name().map(|n| n.to_string_lossy().into_owned()))
    };
    let primaries = roots.iter().any(|(primary, _)| *primary);
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, r) in roots.iter().filter(|(primary, _)| *primary || !primaries) {
        *counts.entry(r).or_default() += 1;
    }
    let most = counts.into_iter().max_by_key(|(_, n)| *n)?.0;
    last(Path::new(most))
}

/// Meridian's form of a spawn ID.
fn is_spawn_id(s: &str) -> bool {
    s.strip_prefix('p').is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// A Meridian project ID that can also go in the inbox's `--project-id`:
/// Meridian's form (parts joined by `/`, none empty or starting with `.`), in
/// its safe set, and not an ad-hoc ID, which names a path on one machine.
fn is_project_id(s: &str) -> bool {
    is_field(s) && !s.starts_with("adhoc.") && s.split('/').all(|p| !p.is_empty() && !p.starts_with('.'))
}

/// `meridian [-C <root>] ...` with Meridian's session variables removed, so the
/// call is not framed as coming from whatever session started the daemon.
/// `MERIDIAN_HOME` stays: it says where Meridian keeps its state. A command
/// about the machine (the inbox, every project's sessions) has no root and
/// runs in the home directory, as Meridian runs a machine hook.
fn meridian_command(root: Option<&Path>) -> Command {
    let mut cmd = Command::new("meridian");
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("MERIDIAN_") && k != "MERIDIAN_HOME" {
            cmd.env_remove(k);
        }
    }
    match root {
        Some(root) => cmd.arg("-C").arg(root),
        None => cmd.current_dir(std::env::var_os("HOME").unwrap_or_else(|| "/".into())),
    };
    cmd.stdin(Stdio::null()).kill_on_drop(true);
    cmd
}

/// The oldest Meridian this daemon runs with: the first with the machine
/// inbox, machine names and IDs, a `kind` on every JSON error, and the
/// `inbox-router` profile in its wheel.
const FLOOR: [u32; 3] = [0, 28, 1];

/// Refuses a Meridian older than `FLOOR`, or one that can't be run, with
/// what to do about it; `or_else` ends the advice on a missing or old one.
async fn check_version(or_else: &str) -> Result<()> {
    let floor = FLOOR.map(|n| n.to_string()).join(".");
    let out = match tokio::time::timeout(CHECK_TIMEOUT, run(None, &["--version"])).await {
        Err(_) => bail!("`meridian --version` gave no answer in {} seconds", CHECK_TIMEOUT.as_secs()),
        Ok(Err(e))
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            bail!("`meridian` was not found on PATH: install Meridian {floor} or later (`uv tool install meridian-cli`){or_else}")
        }
        Ok(r) => r?,
    };
    let printed = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let version: Option<[u32; 3]> = printed
        .strip_prefix("meridian ")
        .and_then(|v| {
            // Each part's leading digits: `0.29.0-rc.1` is 0.29.0, `0.28.0+local` is 0.28.0.
            v.split('.')
                .take(3)
                .map(|p| p[..p.find(|c: char| !c.is_ascii_digit()).unwrap_or(p.len())].parse().ok())
                .collect::<Option<Vec<u32>>>()
        })
        .and_then(|v| v.try_into().ok());
    let Some(version) = version.filter(|_| out.status.success()) else {
        bail!(
            "`meridian --version` gave no version this build can read: {}{}",
            tail(&out.stdout),
            tail(&out.stderr)
        );
    };
    if version < FLOOR {
        bail!(
            "this is {printed}, and meridian-comms needs Meridian {floor} or later: upgrade it with `uv tool install --upgrade meridian-cli`{or_else}"
        );
    }
    Ok(())
}

/// Whether a failed `spawn show` says the chat does not exist. Meridian's
/// kind for that is `not_found`: at the top of the error here (on stderr), and
/// inside `error` through `--machine` (on stdout). An unknown machine is
/// `not_found` too, and such a machine lists no candidates. Every other kind,
/// `failed` among them, is a machine that could not be asked.
fn not_found(out: &std::process::Output) -> bool {
    [&out.stdout, &out.stderr]
        .into_iter()
        .flat_map(|stream| stream.split(|b| *b == b'\n'))
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .any(|e| e["kind"] == "not_found" || e["error"]["kind"] == "not_found")
}

fn rows(out: std::process::Output) -> Result<Rows> {
    if !out.status.success() {
        // Meridian's own words when it gave them: through `--machine`, why the
        // machine could not be asked.
        let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        match field(&v["error"]["text"]) {
            Some(text) => bail!("meridian spawn list failed: {}", tail(text.as_bytes())),
            None => bail!("meridian spawn list failed: {}{}", tail(&out.stdout), tail(&out.stderr)),
        }
    }
    serde_json::from_slice(&out.stdout).context("meridian spawn list: unexpected JSON")
}

async fn run(root: Option<&Path>, args: &[&str]) -> Result<std::process::Output> {
    meridian_command(root).args(args).output().await.context("cannot run `meridian`")
}

/// The end of what a command wrote, for an error: short, and on one line.
fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    let start = text.len().saturating_sub(300);
    let start = (start..=text.len()).find(|i| text.is_char_boundary(*i)).unwrap_or(text.len());
    text[start..].replace('\n', " ")
}

/// Meridian's rule for an inbox field (its `SAFE_FIELD`), with room left for
/// the message ID a channel's name is joined to.
const FIELD_RULE: &str = "at most 150 characters from A-Z a-z 0-9 . _ : / @ -";

fn is_field(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 150
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '@' | '-'))
}

/// What a `--json` inbox or message command printed: its object on success, else the
/// error's kind (one of Meridian's closed list). Anything else is an error.
fn answer(out: &std::process::Output) -> Result<std::result::Result<Value, String>> {
    let v: Value = serde_json::from_slice(&out.stdout)
        .ok()
        .filter(|v: &Value| v["ok"].is_boolean())
        .with_context(|| format!("meridian gave no answer this build understands: {}", tail(&out.stderr)))?;
    if v["ok"] == true {
        return Ok(Ok(v));
    }
    Ok(Err(v["error"]["kind"].as_str().unwrap_or("failed").to_string()))
}

fn field(v: &Value) -> Option<String> {
    v.as_str().map(String::from)
}

fn ago(sent: DateTime<Utc>) -> String {
    let secs = (Utc::now() - sent).num_seconds().max(0);
    match secs {
        0..=89 => "just now".into(),
        90..=5399 => format!("{} minutes ago", (secs + 30) / 60),
        _ => format!("{} hours ago", (secs + 1800) / 3600),
    }
}

fn line(req: &WakeRequest) -> String {
    let n = &req.notice;
    match n.kind {
        NoticeKind::Message => {
            let also = if n.also.is_empty() {
                String::new()
            } else {
                format!(", also sent to {}", n.also.join(", "))
            };
            format!(
                "Slack message {m} from {from}, {ago}{also}. Run `meridian-comms read {m}`.",
                m = n.message,
                from = n.from,
                ago = ago(n.sent),
            )
        }
        NoticeKind::Released => format!(
            "Slack thread {m} is now with {}; you no longer get its messages. `meridian-comms read {m}` shows it.",
            n.also.join(", "),
            m = n.message,
        ),
    }
}

#[async_trait]
impl Source for Meridian {
    fn name(&self) -> &'static str {
        "meridian"
    }

    async fn list(&self) -> Result<ListResponse> {
        self.everything().await
    }

    async fn wake(&self, req: &WakeRequest) -> Result<WakeResult> {
        let c = &req.to;
        let (machine, _, id) = self
            .split_ref(&c.ref_)
            .with_context(|| format!("{} is not a Meridian ref", c.ref_))?;
        let target = self
            .address(c)
            .with_context(|| format!("{} was listed with no machine name", c.ref_))?;
        // A message notice goes to a session the rule or a person chose, so it
        // passes `--wake`, cache warm or not. A release notice was chosen by
        // nobody: without `--wake` Meridian refuses a cold session (kind `cold`,
        // nothing sent) rather than pay a cold read to say there is nothing to do.
        let line = line(req);
        let mut args = vec!["message", &target, &line];
        if req.notice.kind == NoticeKind::Message {
            args.push("--wake");
        }
        args.push("--json");
        // A spawn ID issued before machine-wide IDs is found only in its own
        // project: here that is the folder the row runs in.
        let root = c.root.as_deref().filter(|_| machine == self.id);
        let out = run(root, &args).await?;
        let error = answer(&out)?.err().map(|kind| {
            let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
            (kind, field(&v["error"]["text"]).unwrap_or_default())
        });
        let on = c.machine.as_deref().unwrap_or(machine);
        // Only `maybe_delivered` may have reached the session; it is reported
        // as not woken, so the message still waits.
        let (woken, detail) = match error {
            None => (true, String::new()),
            Some((kind, _)) if kind == "not_found" && machine != self.id => (
                false,
                format!("{id} was not found on {on}: it may be a session from before machine-wide IDs; message it from {on}"),
            ),
            Some((kind, _)) if kind == "not_running" || kind == "not_found" => (false, format!("{id} is no longer running")),
            Some((kind, text)) if kind == "maybe_delivered" => (false, format!("may have reached {id}: {text}")),
            Some((kind, text)) => (false, format!("{kind}: {text}")),
        };
        Ok(WakeResult {
            woken,
            ref_: c.ref_.clone(),
            detail,
        })
    }

    fn identify(&self, req: &IdentifyRequest) -> IdentifyResult {
        let var = |k: &str| req.env.get(k).filter(|v| !v.is_empty());
        let in_session = [
            "MERIDIAN_SPAWN_ID",
            "MERIDIAN_CHAT_ID",
            "MERIDIAN_PROJECT_DIR",
            "MERIDIAN_SESSION_ROLE",
        ]
        .iter()
        .any(|k| var(k).is_some());
        if !in_session {
            return IdentifyResult::none();
        }
        let unlisted = IdentifyResult {
            caller: CallerKind::Unlisted,
            ..IdentifyResult::none()
        };
        let (Some(project), Some(id)) = (var("MERIDIAN_PROJECT_ID"), var("MERIDIAN_SPAWN_ID")) else {
            return unlisted;
        };
        if !self.projects.contains_key(project) {
            return unlisted;
        }
        IdentifyResult {
            caller: CallerKind::Session,
            source: Some(self.name().to_string()),
            session: Some(id.clone()),
            person: Some(self.owner.clone()),
            project: Some(project.clone()),
            may_post: var("MERIDIAN_SESSION_ROLE").is_some_and(|r| r == "primary"),
        }
    }

    fn session_ref(&self, project: &str, session: &str) -> Option<String> {
        (self.projects.contains_key(project) && is_id(session)).then(|| format!("{}/{project}/{session}", self.id))
    }

    fn show(&self, r: &str, machines: &[Machine]) -> Option<String> {
        let (machine, _, id) = self.split_ref(r)?;
        let name = if machine == self.id {
            Some(self.machine.as_str())
        } else {
            machines.iter().find(|m| m.id.as_deref() == Some(machine)).and_then(|m| m.name.as_deref())
        };
        Some(format!("{id} on {}", name.unwrap_or(machine)))
    }

    /// A bare spawn ID could be on any machine; anything else names its
    /// machine first, by ID or name. This machine always answers: a listing
    /// without it fails whole. With no saved machine known (`machine list`
    /// failed), every other machine is in doubt. A word of a message's text
    /// (`to: p41 please`) is none of these.
    fn could_be_unchecked(&self, word: &str, missing: &[Machine]) -> bool {
        if missing.is_empty() {
            return false;
        }
        let Some((first, _)) = word.split_once('/') else {
            return is_spawn_id(word);
        };
        if first == self.id || first == self.machine || self.was.iter().any(|w| w == first) {
            return false;
        }
        missing
            .iter()
            .any(|m| m.id.is_none() || m.id.as_deref() == Some(first) || m.name.as_deref() == Some(first))
    }

    async fn check(&mut self) -> Result<()> {
        check_version(", or remove [sources.meridian] from the config to run without it").await?;
        // This machine's ID starts every ref to a session here, and a daemon
        // that can't learn it must not guess: its refs would match nothing a
        // listing or another start wrote.
        let asked = "`meridian machine name --json`, whose ID every ref to a session here starts with,";
        let out = tokio::time::timeout(CHECK_TIMEOUT, run(None, &["machine", "name", "--json"]))
            .await
            .with_context(|| format!("{asked} gave no answer in {} seconds", CHECK_TIMEOUT.as_secs()))??;
        let told: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        let (name, id, was) = match (field(&told["name"]), field(&told["id"]), told["was"].as_array()) {
            (Some(name), Some(id), Some(was)) if out.status.success() && is_id(&name) => (name, id, was),
            _ => bail!("{asked} failed: {}{}", tail(&out.stdout), tail(&out.stderr)),
        };
        self.machine = name;
        self.id = id;
        self.was = was.iter().filter_map(field).filter(|n| is_id(n)).collect();
        Ok(())
    }

    fn machine(&self) -> Option<&str> {
        Some(self.machine.as_str()).filter(|m| !m.is_empty())
    }

    /// Builds before machine IDs started refs with this machine's name, and
    /// only ever stored this machine's refs.
    fn ref_start(&self) -> Option<RefStart> {
        let start = |name: &String| format!("{name}/");
        Some(RefStart {
            now: start(&self.id),
            earlier: std::iter::once(&self.machine).chain(&self.was).map(start).collect(),
        })
    }

    async fn file(&self, req: &FileRequest) -> Result<Option<Filed>> {
        let mut cmd = meridian_command(None);
        cmd.args(["message", "inbox", "-", "--json", "--via-program", PROGRAM]);
        cmd.args([
            "--reply-to",
            &req.reply_to,
            "--via-ref",
            &req.via_ref,
            "--via-sender",
            &req.via_sender,
        ]);
        if let Some(p) = &req.project {
            cmd.args(["--project-id", p]);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("cannot run `meridian`")?;
        let mut stdin = child.stdin.take().context("no stdin")?;
        // A Meridian that refuses the flags exits without reading: not an error here.
        let _ = stdin.write_all(req.text.as_bytes()).await;
        drop(stdin);
        let out = child.wait_with_output().await?;
        let v = answer(&out)?.map_err(|kind| anyhow::anyhow!("meridian message inbox: {kind}"))?;
        let item = field(&v["message"])
            .filter(|m| is_id(m))
            .context("meridian message inbox gave no message ID")?;
        let machine = field(&v["machine"]).unwrap_or_else(|| self.machine.to_string());
        Ok(Some(Filed {
            item,
            place: format!("{machine}'s inbox"),
        }))
    }

    async fn inbox_item(&self, item: &str) -> Result<Option<InboxItem>> {
        if !is_id(item) {
            bail!("{item:?} is not an inbox message ID");
        }
        let out = run(None, &["inbox", "read", item, "--json"]).await?;
        let v = match answer(&out)? {
            Ok(v) => v,
            Err(kind) if kind == "not_found" => return Ok(None),
            Err(kind) => bail!("meridian inbox read: {kind}"),
        };
        let from = self.sender_ref(&v["from"]).await?;
        Ok(Some(InboxItem {
            text: field(&v["text"]).unwrap_or_default(),
            from,
            via_program: field(&v["via"]["program"]),
            via_ref: field(&v["via"]["ref"]),
            via_sender: field(&v["via"]["sender"]),
            project: field(&v["project"]),
            waiting: v["taken"].is_null(),
        }))
    }

    async fn offer(&self, item: &str, to: &Candidate) -> Result<OfferResult> {
        // The spawn ID alone names the session, whether or not its project is configured.
        let target = self.address(to).with_context(|| format!("{} is not a Meridian ref", to.ref_))?;
        if !is_id(item) {
            bail!("{item:?} is not an inbox message ID");
        }
        // The item is on this machine, so the offer runs here whatever machine
        // the session is on: never through `--machine`.
        let out = run(None, &["inbox", "offer", item, &target, "--json"]).await?;
        Ok(match answer(&out)? {
            Ok(_) => OfferResult {
                offered: Offered::Yes,
                detail: String::new(),
            },
            // Only this kind means the notice may have arrived; every other
            // sent nothing: `refused`, `not_running`, `cold`, `taken`, and from
            // a machine that is down or too old for the inbox `unreachable`,
            // `usage` or `version`.
            Err(kind) => OfferResult {
                offered: if kind == "maybe_delivered" { Offered::Maybe } else { Offered::No },
                detail: kind,
            },
        })
    }

    async fn take(&self, item: &str, by: &str) -> Result<()> {
        if !is_id(item) {
            bail!("{item:?} is not an inbox message ID");
        }
        let out = run(
            None,
            &["inbox", "take", item, "--json", "--via-program", PROGRAM, "--via-sender", by],
        )
        .await?;
        match answer(&out)? {
            Ok(_) => Ok(()),
            Err(kind) if kind == "taken" => Ok(()),
            Err(kind) => bail!("meridian inbox take: {kind}"),
        }
    }

    async fn start_decision(&self, req: &DecideRequest) -> Result<Option<String>> {
        self.launch_router(req).await.map(Some)
    }

    async fn decision(&self, req: &DecideRequest, run: &str) -> Result<Decision> {
        self.router_decision(req, run).await
    }

    fn routed_item(&self, req: &IdentifyRequest) -> Option<String> {
        req.env.get("MERIDIAN_INBOX_MESSAGE_ID").filter(|v| !v.is_empty()).cloned()
    }
}
