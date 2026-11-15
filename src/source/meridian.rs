//! The Meridian source. It uses only Meridian's public CLI: `spawn list
//! --primary --json` for the running primaries and `spawn message` to wake one.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures_util::future::join_all;
use serde::Deserialize;
use tokio::process::Command;

use super::Source;
use crate::config::{Config, MeridianConfig};
use crate::model::{
    Activity, ActivityState, CallerKind, Candidate, IdentifyRequest, IdentifyResult, ListRequest, ListResponse, NoticeKind, WakeRequest,
    WakeResult,
};

pub struct Meridian {
    machine: String,
    owner: String,
    default: Option<String>,
    projects: BTreeMap<String, Project>,
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
        let mut projects = BTreeMap::new();
        for (name, p) in &m.projects {
            if name.contains('/') {
                bail!("sources.meridian.projects.{name}: a project name contains no '/'");
            }
            projects.insert(
                name.clone(),
                Project {
                    root: p.root.clone(),
                    default: p.default.clone(),
                },
            );
        }
        Ok(Self {
            machine: cfg.machine().to_string(),
            owner: cfg.owner.clone(),
            default: m.default.clone(),
            projects,
        })
    }

    fn ref_of(&self, project: &str, id: &str) -> String {
        format!("{}/{project}/{id}", self.machine)
    }

    /// `machine/project/spawn-id` of a ref this source composed.
    fn parse_ref<'a>(&self, r: &'a str) -> Option<(&'a str, &'a str)> {
        let rest = r.strip_prefix(self.machine.as_str())?.strip_prefix('/')?;
        let (project, id) = rest.split_once('/')?;
        self.projects.contains_key(project).then_some((project, id))
    }

    async fn primaries(&self, project: &str) -> Result<Rows> {
        let root = &self.projects[project].root;
        let out = run(root, &["spawn", "list", "--primary", "--json", "--limit", "200"]).await?;
        if !out.status.success() {
            bail!("meridian spawn list failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        serde_json::from_slice(&out.stdout).context("meridian spawn list: unexpected JSON")
    }

    fn candidate(&self, project: &str, row: Row) -> Candidate {
        let p = &self.projects[project];
        let default = row.agent.is_some() && row.agent == p.default.clone().or_else(|| self.default.clone());
        let activity = row.idle.map(|i| Activity {
            state: ActivityState::Idle,
            since: i.since,
            warm_until: i.warm_until,
            note: i.ttl_estimated.then(|| "cache expiry estimated".to_string()),
        });
        Candidate {
            ref_: self.ref_of(project, &row.spawn_id),
            id: Some(row.spawn_id),
            person: self.owner.clone(),
            project: Some(project.to_string()),
            group: row.work_id,
            label: row.agent,
            model: row.model,
            default,
            activity,
        }
    }
}

/// `meridian -C <root> ...` with Meridian's session variables removed, so the
/// call is not framed as coming from whatever session started the daemon.
/// `MERIDIAN_HOME` stays: it says where Meridian keeps its state.
fn meridian_command(root: &Path) -> Command {
    let mut cmd = Command::new("meridian");
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("MERIDIAN_") && k != "MERIDIAN_HOME" {
            cmd.env_remove(k);
        }
    }
    cmd.arg("-C").arg(root).stdin(Stdio::null()).kill_on_drop(true);
    cmd
}

async fn run(root: &Path, args: &[&str]) -> Result<std::process::Output> {
    meridian_command(root).args(args).output().await.context("cannot run `meridian`")
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
                    out.candidates.push(self.candidate(project, row));
                }
            }
        }
        Ok(out)
    }

    async fn wake(&self, req: &WakeRequest) -> Result<WakeResult> {
        let (project, id) = self
            .parse_ref(&req.ref_)
            .with_context(|| format!("{} is not a Meridian ref", req.ref_))?;
        let out = run(&self.projects[project].root, &["spawn", "message", id, "--message", &line(req)]).await?;
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
            ref_: Some(self.ref_of(project, id)),
            person: Some(self.owner.clone()),
            project: Some(project.clone()),
            may_post: var("MERIDIAN_SESSION_ROLE").is_some_and(|r| r == "primary"),
        }
    }
}
