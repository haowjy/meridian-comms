//! The Meridian source. It uses only Meridian's public CLI: `spawn list
//! --json` for the running sessions, `message --wake` to wake one,
//! `message inbox` and `inbox read/take/offer` for the machine's inbox, and
//! `context` for a root's project ID.

pub mod people;
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
use crate::config::{Config, MeridianConfig, is_id};
use crate::model::{
    Activity, ActivityState, CallerKind, Candidate, DecideRequest, Decision, FileRequest, Filed, Group, IdentifyRequest, IdentifyResult,
    InboxItem, ListRequest, ListResponse, Machine, NoticeKind, OfferResult, Offered, WakeRequest, WakeResult,
};

const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// How long `machine list`, and then each saved machine's listing, may take:
/// together under the core's 30 s for a listing across machines. Measured on
/// a Raspberry Pi: about 5 s for one that answers, nearly 10 beside other
/// listings; `ssh` itself gives up connecting after 10.
const MACHINE_TIMEOUT: Duration = Duration::from_secs(12);
/// What this program calls itself in an inbox item's `via` and in a claim.
const PROGRAM: &str = "meridian-comms";

pub struct Meridian {
    /// Meridian's name for this machine, which every ref to a session here
    /// starts with: asked by `check` once, so a rename in Meridian while the
    /// daemon runs changes nothing here. Empty in the client, which composes
    /// no refs.
    machine: String,
    /// The names Meridian says this machine had before.
    was: Vec<String>,
    /// This machine's ID, which no rename changes: what an inbox item's
    /// sender carries.
    id: String,
    owner: String,
    default: Option<String>,
    projects: BTreeMap<String, Project>,
}

/// A machine Meridian has saved: the ID that never changes, and the name
/// it is saved under, which is what `--machine` and `<machine>/<id>` take.
struct Saved {
    id: String,
    name: String,
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
            machine: String::new(),
            was: vec![],
            id: String::new(),
            owner: cfg.owner.clone(),
            default: m.default.clone(),
            projects,
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
                Some(m) => Some(m.name),
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
            args.extend(["--machine", m]);
        }
        let out = run(None, &args).await?;
        let shown: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        match field(&shown["spawn_id"]).filter(|id| is_id(id)) {
            Some(id) => {
                let machine = machine.as_deref().unwrap_or(&self.machine);
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

    /// One project's running primaries.
    async fn primaries(&self, project: &str) -> Result<Rows> {
        let root = &self.projects[project].root;
        rows(run(Some(root), &["spawn", "list", "--primary", "--json", "--limit", "200"]).await?)
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
    async fn offerable(&self) -> Result<ListResponse> {
        let mut out = ListResponse::default();
        let elsewhere = async {
            // The sessions here are still worth listing without the others.
            let saved = match self.saved_machines().await {
                Ok(saved) => saved,
                Err(e) => {
                    tracing::warn!("no saved machine's sessions are listed: {e:#}");
                    return Err(());
                }
            };
            Ok(join_all(saved.into_iter().map(|Saved { id, name: m }| async move {
                let asked = Instant::now();
                let rows = match tokio::time::timeout(MACHINE_TIMEOUT, self.everywhere(Some(&m))).await {
                    Ok(rows) => rows,
                    Err(_) => Err(anyhow::anyhow!("no answer in {} s", MACHINE_TIMEOUT.as_secs())),
                };
                let took = asked.elapsed().as_secs_f32();
                match &rows {
                    Ok(r) => tracing::debug!("{m} listed {} session(s) in {took:.1} s", r.spawns.len()),
                    Err(e) => tracing::warn!("no sessions listed from {m} after {took:.1} s: {e:#}"),
                }
                (id, m, rows)
            }))
            .await)
        };
        let (here, there) = tokio::join!(self.everywhere(None), elsewhere);
        let mut listed = vec![(self.machine.to_string(), here?)];
        out.machines.push(Machine {
            id: Some(self.id.clone()),
            name: Some(self.machine.to_string()),
            ok: true,
            why: None,
        });
        match there {
            Ok(there) => {
                for (id, m, rows) in there {
                    out.machines.push(Machine {
                        id: Some(id),
                        name: Some(m.clone()),
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
                let candidate = row.kind == "primary" && row.status == "running";
                *groups.entry(row.work_id.clone()).or_default() += u32::from(!candidate);
                roots.extend(row.project_root.clone().map(|r| (candidate, r)));
                if candidate {
                    out.candidates.push(self.candidate(&machine, &project, row));
                }
            }
            for (project, Seen { groups, roots }) in projects {
                let project_label = self.project_label(&machine, &project, &roots);
                out.groups.extend(groups.into_iter().map(|(group, others)| Group {
                    machine: machine.clone(),
                    project: project.clone(),
                    project_label: project_label.clone(),
                    group,
                    others,
                }));
            }
        }
        Ok(out)
    }

    /// The last part of the folder a project lives in. Meridian's rows give
    /// only the folder each session runs in, which can be a worktree's, so the
    /// configured root wins; for another project, the folder most of its
    /// primaries run in (people start those in the main checkout), else most
    /// of its sessions.
    fn project_label(&self, machine: &str, project: &str, roots: &[(bool, String)]) -> Option<String> {
        let last = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned());
        if machine == self.machine
            && let Some(p) = self.projects.get(project)
        {
            return last(&p.root);
        }
        let primaries = roots.iter().any(|(primary, _)| *primary);
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for (_, r) in roots.iter().filter(|(primary, _)| *primary || !primaries) {
            *counts.entry(r).or_default() += 1;
        }
        let most = counts.into_iter().max_by_key(|(_, n)| *n)?.0;
        last(Path::new(most))
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
            // What `parse_ref` takes: this machine, a configured project.
            wakeable: configured.is_some(),
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

    async fn list(&self, req: &ListRequest) -> Result<ListResponse> {
        let for_owner = req.person.as_deref() == Some(self.owner.as_str());
        if req.offerable {
            return if for_owner {
                self.offerable().await
            } else {
                Ok(ListResponse::default())
            };
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
        // A message notice goes to a session the rule or a person chose, so it
        // passes `--wake`, cache warm or not. A release notice was chosen by
        // nobody: without `--wake` Meridian refuses a cold session (kind `cold`,
        // nothing sent) rather than pay a cold read to say there is nothing to do.
        let line = line(req);
        let mut args = vec!["message", id, &line];
        if req.notice.kind == NoticeKind::Message {
            args.push("--wake");
        }
        args.push("--json");
        let out = run(Some(&self.projects[project].root), &args).await?;
        let error = answer(&out)?.err().map(|kind| {
            let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
            (kind, field(&v["error"]["text"]).unwrap_or_default())
        });
        // Only `maybe_delivered` may have reached the session; it is reported
        // as not woken, so the message still waits.
        let (woken, detail) = match error {
            None => (true, String::new()),
            Some((kind, _)) if kind == "not_running" || kind == "not_found" => (false, format!("{id} is no longer running")),
            Some((kind, text)) if kind == "maybe_delivered" => (false, format!("may have reached {id}: {text}")),
            Some((kind, text)) => (false, format!("{kind}: {text}")),
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

    async fn elsewhere(&self, word: &str) -> bool {
        let Some((machine, _, _)) = self.split_ref(word) else {
            return false;
        };
        // A list that can't be read leaves the plain "matches no running session".
        machine != self.machine
            && self
                .saved_machines()
                .await
                .is_ok_and(|saved| saved.iter().any(|m| m.name == machine))
    }

    fn session_ref(&self, project: &str, session: &str) -> Option<String> {
        (self.projects.contains_key(project) && is_id(session)).then(|| self.ref_of(project, session))
    }

    /// Each project's key must be its root's Meridian project ID: a channel's
    /// project, a candidate's project and a filed item's project are one value.
    /// A check that could not run stops the daemon too, and says what stopped it.
    async fn check(&mut self) -> Result<()> {
        check_version(", or remove [sources.meridian] from the config to run without it").await?;
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
        // Meridian's name is the one every ref carries, and a daemon that
        // can't learn it must not guess: its refs would match nothing Meridian
        // or another start wrote.
        let asked = "`meridian machine name --json`, which every session's ref starts with,";
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

    fn ref_start(&self) -> Option<RefStart> {
        let start = |name: &String| format!("{name}/");
        Some(RefStart {
            now: start(&self.machine),
            earlier: self.was.iter().map(start).collect(),
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

    async fn offer(&self, item: &str, ref_: &str) -> Result<OfferResult> {
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
