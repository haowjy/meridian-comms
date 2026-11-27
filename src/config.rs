use std::collections::{BTreeMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::paths;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub owner: String,
    #[serde(default)]
    machine: Option<String>,
    #[serde(default)]
    pub surface: Surfaces,
    #[serde(default)]
    pub people: BTreeMap<String, Person>,
    #[serde(default)]
    pub sources: Sources,
    /// Present: messages filed in a source's inbox are routed.
    #[serde(default)]
    pub router: Option<RouterConfig>,
}

#[derive(Debug, Deserialize)]
pub struct RouterConfig {
    #[serde(default = "RouterConfig::default_url")]
    pub url: String,
    #[serde(default = "RouterConfig::default_model")]
    pub model: String,
    /// The probability the model must give a session before it is offered a message.
    #[serde(default = "RouterConfig::default_floor")]
    pub floor: f64,
}

impl RouterConfig {
    fn default_url() -> String {
        "https://opencode.ai/inference/systemone/v1/systemone".into()
    }
    fn default_model() -> String {
        "jev-1.13-free".into()
    }
    fn default_floor() -> f64 {
        0.6
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct Surfaces {
    pub slack: Option<SlackConfig>,
}

#[derive(Debug, Deserialize)]
pub struct SlackConfig {
    #[serde(default)]
    pub channels: BTreeMap<String, Channel>,
}

#[derive(Debug, Deserialize)]
pub struct Channel {
    pub name: String,
    #[serde(default)]
    pub project: Option<String>,
    /// Sessions may open threads here (needed when the channel has no project).
    #[serde(default)]
    pub post: bool,
    /// The channel is shared with another organization, and that is intended.
    #[serde(default)]
    pub external: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMark {
    #[default]
    No,
    Maybe,
    Always,
}

#[derive(Debug, Deserialize)]
pub struct Person {
    pub slack: String,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub agent: AgentMark,
}

#[derive(Debug, Default, Deserialize)]
pub struct Sources {
    pub meridian: Option<MeridianConfig>,
    // Later work: parsed so a config written for it still loads.
    #[allow(dead_code)]
    amp: Option<toml::Value>,
    #[allow(dead_code)]
    remote: Option<toml::Value>,
}

#[derive(Debug, Deserialize)]
pub struct MeridianConfig {
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub projects: BTreeMap<String, MeridianProject>,
}

#[derive(Debug, Deserialize)]
pub struct MeridianProject {
    pub root: PathBuf,
    #[serde(default)]
    pub default: Option<String>,
}

pub fn config_path() -> PathBuf {
    paths::config_dir().join("config.toml")
}

pub fn env_path() -> PathBuf {
    paths::config_dir().join("slack.env")
}

impl Config {
    pub fn load() -> Result<Config> {
        let path = config_path();
        let text = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "cannot read {} (create it from the config example in the design docs)",
                path.display()
            )
        })?;
        let mut cfg: Config = toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
        cfg.finish()?;
        Ok(cfg)
    }

    fn finish(&mut self) -> Result<()> {
        if self.machine.is_none() {
            let host = gethostname::gethostname().to_string_lossy().into_owned();
            self.machine = Some(host.split('.').next().unwrap_or("local").to_string());
        }
        let machine = self.machine();
        let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
        if machine.is_empty() || machine.len() > 64 || machine.starts_with('-') || !machine.chars().all(plain) {
            bail!("machine name {machine:?} must be 1 to 64 characters from A-Z a-z 0-9 . _ - and not start with '-'");
        }
        if !self.people.contains_key(&self.owner) {
            bail!("owner {:?} has no [people.{}] entry", self.owner, self.owner);
        }
        let mut ids = HashSet::new();
        for (name, p) in &self.people {
            if !ids.insert(&p.slack) {
                bail!("people.{name}: Slack ID {} is used by two people", p.slack);
            }
        }
        let mut names = HashSet::new();
        for (id, ch) in self.channels() {
            if ch.name.contains('/') || ch.name.is_empty() {
                bail!("channel {id}: name {:?} must be non-empty and contain no '/'", ch.name);
            }
            if !names.insert(&ch.name) {
                bail!("two channels are named {:?}", ch.name);
            }
        }
        if let Some(r) = &self.router
            && !(0.0..=1.0).contains(&r.floor)
        {
            bail!("router.floor is {}: it is a probability, from 0 to 1", r.floor);
        }
        Ok(())
    }

    pub fn machine(&self) -> &str {
        self.machine.as_deref().unwrap_or("local")
    }

    pub fn channels(&self) -> impl Iterator<Item = (&String, &Channel)> {
        self.surface.slack.iter().flat_map(|s| s.channels.iter())
    }

    pub fn channel(&self, id: &str) -> Option<&Channel> {
        self.surface.slack.as_ref()?.channels.get(id)
    }

    pub fn channel_by_name(&self, name: &str) -> Option<(&String, &Channel)> {
        self.channels().find(|(_, c)| c.name == name)
    }

    pub fn person_by_slack(&self, id: &str) -> Option<(&String, &Person)> {
        self.people.iter().find(|(_, p)| p.slack == id)
    }

    /// "Alice (cofounder, works in Amp)": how a notice names a person.
    pub fn display(&self, person: &str) -> String {
        let mut name = person.to_string();
        if let Some(c) = name.get_mut(0..1) {
            c.make_ascii_uppercase();
        }
        match self.people.get(person).and_then(|p| p.note.as_deref()) {
            Some(note) => format!("{name} ({note})"),
            None => name,
        }
    }
}

/// Refuse a file the group or others can read.
pub fn check_private(path: &Path) -> Result<()> {
    let mode = std::fs::metadata(path)
        .with_context(|| format!("cannot stat {}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        bail!(
            "{} is readable by group or others (mode {:o}); run: chmod 600 {}",
            path.display(),
            mode & 0o777,
            path.display()
        );
    }
    Ok(())
}

pub struct Tokens {
    pub bot: String,
    pub app: String,
    /// The router's model key. Without it nothing is routed; the daemon still runs.
    pub router: Option<String>,
}

impl Tokens {
    /// From the environment, then `slack.env`. The file is refused when others can read it.
    pub fn load() -> Result<Tokens> {
        let path = env_path();
        let mut file = BTreeMap::new();
        if path.exists() {
            check_private(&path)?;
            for line in std::fs::read_to_string(&path)?.lines() {
                let line = line.trim();
                let line = line.strip_prefix("export ").unwrap_or(line);
                if line.starts_with('#') {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    file.insert(k.trim().to_string(), v.trim().trim_matches(['"', '\'']).to_string());
                }
            }
        }
        let get = |key: &str| -> Option<String> {
            std::env::var(key)
                .ok()
                .filter(|v| !v.is_empty())
                .or_else(|| file.get(key).cloned().filter(|v| !v.is_empty()))
        };
        let need = |key: &str| get(key).with_context(|| format!("{key} is neither in the environment nor in {}", path.display()));
        Ok(Tokens {
            bot: need("SLACK_BOT_TOKEN")?,
            app: need("SLACK_APP_TOKEN")?,
            router: get("OPENCODE_API_KEY"),
        })
    }
}
