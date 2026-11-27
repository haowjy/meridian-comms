//! The Meridian source. It uses only Meridian's public CLI: `spawn list
//! --primary --json` for the running primaries, `spawn message` to wake one,
//! `message inbox` and `inbox read/take/offer` for the machine's inbox, and
//! `context` for a root's project ID.

pub mod router;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
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
use crate::config::{Config, MeridianConfig};
use crate::model::{
    Activity, ActivityState, CallerKind, Candidate, DecideRequest, Decision, FileRequest, Filed, IdentifyRequest, IdentifyResult,
    InboxItem, ListRequest, ListResponse, NoticeKind, OfferResult, Offered, WakeRequest, WakeResult,
};

const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// How long one saved machine may take to list its sessions. Measured: about
/// 2 s for one that answers; `ssh` itself gives up connecting after 10.
const MACHINE_TIMEOUT: Duration = Duration::from_secs(8);
/// What this program calls itself in an inbox item's `via` and in a claim.
const PROGRAM: &str = "meridian-comms";

pub struct Meridian {
    /// What every ref to a session here starts with, for the daemon's life:
    /// the config's `machine` until `check` asks Meridian for its own name.
    /// A rename in Meridian while the daemon runs changes nothing here.
    machine: String,
    /// The config's `machine`: what refs started with before the daemon kept
    /// a record of it.
    configured: String,
    owner: String,
    default: Option<String>,
    projects: BTreeMap<String, Project>,
    /// Whether this Meridian has the machine inbox, by its `inbox` command and
    /// not its version: builds before the release share a version. Learnt by
    /// `check` when the daemon starts; without it nothing is filed or routed.
    inbox: bool,
}

struct Project {
    root: PathBuf,
    default: Option<String>,
}

#[derive(Deserialize)]
struct Rows {
    #[serde(default)]
    spawns: Vec<Row>,
    #[serde(default)]
    truncated: bool,
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
        let projects = m
            .projects
            .iter()
            .map(|(name, p)| {
                let project = Project {
                    root: p.root.clone(),
                    default: p.default.clone(),
                };
                (name.clone(), project)
            })
            .collect();
        Ok(Self {
            machine: cfg.machine().to_string(),
            configured: cfg.machine().to_string(),
            owner: cfg.owner.clone(),
            default: m.default.clone(),
            projects,
            inbox: true,
        })
    }

    fn ref_of(&self, project: &str, id: &str) -> String {
        format!("{}/{project}/{id}", self.machine)
    }

    /// The machine, project and spawn ID of a Meridian ref. A project ID may
    /// itself hold a `/`, so the machine comes off the front and the spawn ID
    /// off the end.
    fn split_ref<'a>(&self, r: &'a str) -> Option<(&'a str, &'a str, &'a str)> {
        let (machine, rest) = r.split_once('/')?;
        let (project, id) = rest.rsplit_once('/')?;
        (is_id(machine) && is_id(id)).then_some((machine, project, id))
    }

    /// The project and spawn ID of a ref on this machine, in a configured
    /// project: the only sessions this source wakes.
    fn parse_ref<'a>(&self, r: &'a str) -> Option<(&'a str, &'a str)> {
        let (machine, project, id) = self.split_ref(r)?;
        (machine == self.machine && self.projects.contains_key(project)).then_some((project, id))
    }

    /// How a Meridian command on this machine names the session of a ref: the
    /// spawn ID, with its machine in front when that is another one.
    pub(super) fn target(&self, r: &str) -> Option<String> {
        let (machine, _, id) = self.split_ref(r)?;
        Some(if machine == self.machine {
            id.to_string()
        } else {
            format!("{machine}/{id}")
        })
    }

    /// One project's running primaries.
    async fn primaries(&self, project: &str) -> Result<Rows> {
        let root = &self.projects[project].root;
        rows(run(Some(root), &["spawn", "list", "--primary", "--json", "--limit", "200"]).await?)
    }

    /// Every project's running primaries, on this machine or a saved one.
    async fn everywhere(&self, machine: Option<&str>) -> Result<Rows> {
        let mut args = vec!["spawn", "list", "--global", "--primary", "--json"];
        if let Some(m) = machine {
            args.extend(["--machine", m]);
        }
        rows(run(None, &args).await?)
    }

    /// The machines Meridian has saved, by name. None on a Meridian without them.
    async fn saved_machines(&self) -> Vec<String> {
        let Ok(out) = run(None, &["machine", "list", "--json"]).await else {
            return vec![];
        };
        let listed: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        let names = listed["machines"].as_array().into_iter().flatten();
        names.filter_map(|m| field(&m["name"])).filter(|n| is_id(n)).collect()
    }

    /// `machine` is the name the listing was asked by: this machine's, or the
    /// one a machine is saved under, which is what Meridian's commands take.
    /// What a row calls its own machine is not read.
    fn candidate(&self, machine: &str, project: &str, row: Row) -> Candidate {
        let here = machine == self.machine;
        let configured = self.projects.get(project).filter(|_| here);
        let own = configured.and_then(|p| p.default.clone());
        let default = row.agent.is_some() && row.agent == own.or_else(|| self.default.clone());
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
        Candidate {
            ref_: format!("{machine}/{project}/{}", row.spawn_id),
            id: Some(row.spawn_id),
            person: self.owner.clone(),
            machine: Some(machine.to_string()),
            project: Some(project.to_string()),
            group: row.work_id,
            label: row.agent,
            model: row.model,
            default,
            activity,
            // A path on another machine is no use to a command run here.
            root: row
                .project_root
                .filter(|_| here)
                .map(PathBuf::from)
                .or_else(|| configured.map(|p| p.root.clone())),
        }
    }
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

fn rows(out: std::process::Output) -> Result<Rows> {
    if !out.status.success() {
        bail!("meridian spawn list failed: {}{}", tail(&out.stdout), tail(&out.stderr));
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

/// An inbox item or spawn ID, safe as a command's argument: never an option.
fn is_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) && !s.starts_with('-')
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

/// What a `--json` inbox command printed: its object on success, else the
/// error's kind (one of Meridian's closed list). A Meridian without the
/// command prints no such object, and that is an error.
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

    async fn list(&self, req: &ListRequest) -> Result<ListResponse> {
        let for_owner = req.person.as_deref() == Some(self.owner.as_str());
        if req.offerable {
            let mut out = ListResponse::default();
            if !for_owner {
                return Ok(out);
            }
            // Every machine is asked at once, and each saved one has its own
            // limit: one that is down or slow contributes none, and holds the
            // listing no longer than that.
            let elsewhere = async {
                let saved = self.saved_machines().await;
                join_all(saved.into_iter().map(|m| async move {
                    let asked = Instant::now();
                    let rows = match tokio::time::timeout(MACHINE_TIMEOUT, self.everywhere(Some(&m))).await {
                        Ok(rows) => rows,
                        Err(_) => Err(anyhow::anyhow!("no answer")),
                    };
                    let took = asked.elapsed().as_secs_f32();
                    match &rows {
                        Ok(r) => tracing::debug!("{m} listed {} session(s) in {took:.1} s", r.spawns.len()),
                        Err(e) => tracing::warn!("no sessions listed from {m} after {took:.1} s: {e:#}"),
                    }
                    Some((m, rows.ok()?))
                }))
                .await
            };
            let (here, there) = tokio::join!(self.everywhere(None), elsewhere);
            let mut listed = vec![(self.machine.to_string(), here?)];
            listed.extend(there.into_iter().flatten());
            for (machine, rows) in listed {
                for mut row in rows.spawns.into_iter().filter(|r| r.kind == "primary" && r.status == "running") {
                    if let Some(project) = row.project_id.take() {
                        out.candidates.push(self.candidate(&machine, &project, row));
                    }
                }
            }
            return Ok(out);
        }
        let own: BTreeSet<&str> = req
            .refs
            .iter()
            .map(String::as_str)
            .filter(|r| self.parse_ref(r).is_some())
            .collect();

        let mut projects: BTreeSet<&str> = own.iter().filter_map(|r| self.parse_ref(r)).map(|(p, _)| p).collect();
        if for_owner {
            match req.project.as_deref() {
                Some(p) => projects.extend(self.projects.get_key_value(p).map(|(k, _)| k.as_str())),
                None => projects.extend(self.projects.keys().map(String::as_str)),
            }
        }

        let answers = join_all(projects.iter().map(|p| async move { (*p, self.primaries(p).await) })).await;
        let mut out = ListResponse::default();
        for (project, rows) in answers {
            let rows = match rows {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("meridian list for {project}: {e:#}");
                    out.unreachable.extend(
                        own.iter()
                            .filter(|r| self.parse_ref(r).is_some_and(|(p, _)| p == project))
                            .map(|r| r.to_string()),
                    );
                    continue;
                }
            };
            if rows.truncated {
                tracing::warn!("meridian list for {project} was truncated at 200 primaries");
            }
            for row in rows.spawns.into_iter().filter(|r| r.kind == "primary" && r.status == "running") {
                let r = self.ref_of(project, &row.spawn_id);
                if for_owner || own.contains(r.as_str()) {
                    out.candidates.push(self.candidate(&self.machine, project, row));
                }
            }
        }
        Ok(out)
    }

    async fn wake(&self, req: &WakeRequest) -> Result<WakeResult> {
        let (project, id) = self
            .parse_ref(&req.ref_)
            .with_context(|| format!("{} is not a Meridian ref", req.ref_))?;
        let out = run(
            Some(&self.projects[project].root),
            &["spawn", "message", id, "--message", &line(req)],
        )
        .await?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        // Exit 0 is woken, with or without a receipt. "not running" means the
        // session stopped; retrying cannot help.
        let (woken, detail) = if out.status.success() {
            (true, stdout.trim().to_string())
        } else if stderr.contains("not running") || stdout.contains("not running") {
            (false, format!("{id} is no longer running"))
        } else {
            (false, format!("{}{}", stderr.trim(), stdout.trim()))
        };
        Ok(WakeResult {
            woken,
            ref_: req.ref_.clone(),
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
        let (Some(dir), Some(id)) = (var("MERIDIAN_PROJECT_DIR"), var("MERIDIAN_SPAWN_ID")) else {
            return unlisted;
        };
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| PathBuf::from(dir));
        let Some(project) = self
            .projects
            .iter()
            .find(|(_, p)| std::fs::canonicalize(&p.root).unwrap_or_else(|_| p.root.clone()) == dir)
            .map(|(name, _)| name)
        else {
            return unlisted;
        };
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
        (self.projects.contains_key(project) && is_id(session)).then(|| self.ref_of(project, session))
    }

    /// Each project's key must be its root's Meridian project ID: a channel's
    /// project, a candidate's project and a filed item's project are one value.
    /// A check that could not run stops the daemon too, and says what stopped it.
    async fn check(&mut self) -> Result<()> {
        let answers = join_all(self.projects.iter().map(|(key, p)| async move {
            let out = tokio::time::timeout(CHECK_TIMEOUT, run(Some(&p.root), &["context", "--json"])).await;
            (key, p, out)
        }))
        .await;
        for (key, p, out) in answers {
            let at = format!("sources.meridian.projects.{key}");
            let root = p.root.display();
            let unchecked = "so its project ID could not be checked against the key";
            // Meridian answers for a directory that is not there, with an ID made from the path.
            if !p.root.is_dir() {
                bail!("{at}: the root {root} is not a directory on this machine, {unchecked}");
            }
            let out = match out {
                Err(_) => bail!(
                    "{at}: `meridian -C {root} context --json` gave no answer in {} seconds, {unchecked}",
                    CHECK_TIMEOUT.as_secs()
                ),
                Ok(Err(e))
                    if e.downcast_ref::<std::io::Error>()
                        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
                {
                    bail!("{at}: `meridian` was not found on PATH, {unchecked}")
                }
                Ok(Err(e)) => bail!("{at}: {e:#}, {unchecked}"),
                Ok(Ok(out)) => out,
            };
            let v: Value = serde_json::from_slice(&out.stdout).with_context(|| {
                format!(
                    "{at}: `meridian -C {root} context --json` failed, {unchecked}: {}",
                    tail(&out.stderr)
                )
            })?;
            let id = v["project_id"].as_str().unwrap_or_default();
            if v["project_identity_source"] != "meridian.toml" || id.is_empty() {
                bail!(
                    "{at}: {root} has no Meridian project ID of its own (no `[project] id` in its meridian.toml). Run `meridian init` there, then use the ID `meridian -C {root} context` prints as this key and as `project` on its channels"
                );
            }
            if id != key {
                bail!(
                    "{at}: the key must be the Meridian project ID of {root}, which is {id:?}. Write [sources.meridian.projects.{id}], and project = {id:?} on the channels that say project = {key:?}"
                );
            }
        }
        // Meridian names its machines from 0.28. Where it does, its name is the
        // one every ref carries, and a daemon that can't learn it must not
        // guess: its refs would match nothing Meridian or another start wrote.
        let has = tokio::time::timeout(CHECK_TIMEOUT, run(None, &["machine", "--help"])).await;
        if matches!(has, Ok(Ok(out)) if out.status.success()) {
            let asked = "`meridian machine name`, which every session's ref starts with,";
            let out = tokio::time::timeout(CHECK_TIMEOUT, run(None, &["machine", "name"]))
                .await
                .with_context(|| format!("{asked} gave no answer in {} seconds", CHECK_TIMEOUT.as_secs()))??;
            let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !out.status.success() || !is_id(&name) {
                bail!("{asked} failed: {}{}", tail(&out.stdout), tail(&out.stderr));
            }
            self.machine = name;
        }
        // An older Meridian has no `inbox` command: say so once, here, instead
        // of failing a command for every message.
        let has = tokio::time::timeout(CHECK_TIMEOUT, run(None, &["inbox", "--help"])).await;
        if !matches!(has, Ok(Ok(out)) if out.status.success()) {
            self.inbox = false;
            tracing::warn!(
                "this Meridian has no `inbox` command (it came with 0.28), so it can't file or route: a Slack message nobody is woken for waits in `meridian-comms list`"
            );
        }
        Ok(())
    }

    fn ref_start(&self) -> Option<RefStart> {
        Some(RefStart {
            now: format!("{}/", self.machine),
            unrecorded: format!("{}/", self.configured),
        })
    }

    async fn file(&self, req: &FileRequest) -> Result<Option<Filed>> {
        if !self.inbox {
            return Ok(None);
        }
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
        if !self.inbox {
            return Ok(None);
        }
        if !is_id(item) {
            bail!("{item:?} is not an inbox message ID");
        }
        let out = run(None, &["inbox", "read", item, "--json"]).await?;
        let v = match answer(&out)? {
            Ok(v) => v,
            Err(kind) if kind == "not_found" => return Ok(None),
            Err(kind) => bail!("meridian inbox read: {kind}"),
        };
        // `from` names the sender's chat and machine; a candidate is named by
        // its spawn ID, so the sender's ref takes one more call, on its machine.
        // A chat Meridian does not find is taken as gone, so not a candidate;
        // any other failure, a machine that can't be reached among them, fails
        // the read, or the sender would be offered its own message. A local
        // `spawn show` prints its error as JSON on stderr, one through
        // `--machine` on stdout.
        let mut from = None;
        let sender = &v["from"];
        if let Some(chat) = field(&sender["id"]).filter(|id| sender["kind"] == "session" && is_id(id)) {
            let machine = field(&sender["machine"]).filter(|m| is_id(m) && *m != self.machine);
            let mut args = vec!["spawn", "show", &chat, "--json"];
            if let Some(m) = &machine {
                args.extend(["--machine", m]);
            }
            let out = run(None, &args).await?;
            let shown: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
            let gone = || {
                let errors = String::from_utf8_lossy(&out.stderr);
                shown["error"]["kind"] == "not_found"
                    || errors
                        .lines()
                        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                        .any(|e| e["error"].as_str().is_some_and(|e| e.contains("not found")))
            };
            match field(&shown["spawn_id"]).filter(|id| is_id(id)) {
                Some(id) => {
                    let machine = machine.as_deref().unwrap_or(&self.machine);
                    from = field(&sender["project"]).map(|project| format!("{machine}/{project}/{id}"));
                }
                None if gone() => {}
                None => bail!(
                    "cannot tell which session sent it: `meridian spawn show {chat}` failed: {}{}",
                    tail(&out.stdout),
                    tail(&out.stderr)
                ),
            }
        }
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

    async fn offer(&self, item: &str, ref_: &str) -> Result<OfferResult> {
        if !self.inbox {
            bail!("this Meridian has no inbox");
        }
        // The spawn ID alone names the session, whether or not its project is configured.
        let target = self.target(ref_).with_context(|| format!("{ref_} is not a Meridian ref"))?;
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
        if !self.inbox {
            bail!("this Meridian has no inbox");
        }
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
