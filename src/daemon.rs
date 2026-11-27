//! The daemon: the Slack connection, the core, and the Unix socket clients use.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::config::{self, Config, Tokens};
use crate::core::Core;
use crate::jev::Jev;
use crate::paths;
use crate::proto::{Reply, Request};
use crate::routes;
use crate::source::{Source, meridian, meridian::Meridian};
use crate::state::Store;
use crate::surface::{Surface, slack::Slack};

const MAX_REQUEST: u64 = 4 << 20;

pub fn sources(cfg: &Config) -> Result<Vec<Arc<dyn Source>>> {
    let mut out: Vec<Arc<dyn Source>> = vec![];
    if let Some(m) = &cfg.sources.meridian {
        out.push(Arc::new(Meridian::new(cfg, m)?));
    }
    Ok(out)
}

pub async fn run() -> Result<()> {
    let cfg = Config::load()?;
    config::check_private(&config::config_path())?;
    let tokens = Tokens::load()?;
    if cfg.surface.slack.is_none() {
        bail!("config has no [surface.slack] section: this build holds a bot only");
    }
    let sources = sources(&cfg)?;
    if sources.is_empty() {
        tracing::info!("no source is configured: threads are read and posted, nobody is woken, and every message waits");
    }
    for s in &sources {
        s.check().await?;
    }
    if cfg.sources.meridian.is_some() {
        meridian::router::sweep();
    }
    let jev = match (&cfg.router, tokens.router) {
        (Some(r), Some(key)) => Some(Jev::new(r.url.clone(), r.model.clone(), key)?),
        (Some(_), None) => {
            tracing::warn!(
                "[router] is configured but OPENCODE_API_KEY is neither in the environment nor in {}: nothing is routed",
                config::env_path().display()
            );
            None
        }
        (None, _) => None,
    };

    let slack = Arc::new(Slack::connect(tokens.bot, tokens.app).await?);
    let channels: Vec<String> = cfg.channels().map(|(id, _)| id.clone()).collect();
    let who = slack.whoami(&channels).await?;
    for (id, ch) in cfg.channels() {
        if who.external.get(id).copied().unwrap_or(false) && !ch.external {
            bail!(
                "channel {} ({id}) is shared with another organization; set external = true in its entry to allow that",
                ch.name
            );
        }
    }

    let store = Store::open(paths::state_dir())?;
    let listener = bind(&paths::socket_path())?;
    let surface: Arc<dyn Surface> = slack;
    let routes = routes::Log::open(&paths::state_dir())?;
    let core = Core::new(cfg, surface.clone(), sources, store, who.bot, jev, routes)?;

    let (tx, rx) = mpsc::channel(4096);
    tokio::spawn(async move { surface.run(tx).await });
    let server = core.clone();
    tokio::spawn(async move { serve(server, listener).await });
    tracing::info!("meridian-comms daemon up, socket {}", paths::socket_path().display());

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = core.run(rx) => {}
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    let _ = std::fs::remove_file(paths::socket_path());
    Ok(())
}

/// Binds the socket, replacing a stale one left by a dead daemon.
fn bind(path: &Path) -> Result<UnixListener> {
    let dir = path.parent().context("socket path has no parent")?;
    std::fs::create_dir_all(dir)?;
    if dir.ends_with("run") {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            bail!("a daemon is already listening on {}", path.display());
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path).with_context(|| format!("cannot bind {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

async fn serve(core: Arc<Core>, listener: UnixListener) {
    loop {
        match listener.accept().await {
            Ok((conn, _)) => {
                let core = core.clone();
                tokio::spawn(async move {
                    if let Err(e) = connection(core, conn).await {
                        tracing::warn!("client connection: {e:#}");
                    }
                });
            }
            Err(e) => {
                tracing::error!("socket accept: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn connection(core: Arc<Core>, conn: UnixStream) -> Result<()> {
    // The socket's mode already keeps other users out; this is the second check.
    let peer = conn.peer_cred()?;
    if peer.uid() != unsafe { libc::geteuid() } {
        bail!("refused a connection from uid {}", peer.uid());
    }
    let (rd, mut wr) = conn.into_split();
    let mut line = String::new();
    BufReader::new(rd.take(MAX_REQUEST)).read_line(&mut line).await?;
    let reply = match serde_json::from_str::<Request>(&line) {
        Err(e) => error(format!("bad request: {e}")),
        // The request runs to the end in its own task, whether or not the client
        // is still waiting: a `send` that has posted must finish its wakes and
        // write its thread file.
        Ok(req) => match tokio::spawn(async move { handle(&core, req).await }).await {
            Ok(Ok(v)) => Reply {
                error: None,
                result: Some(v),
            },
            Ok(Err(e)) => error(format!("{e:#}")),
            Err(e) => error(format!("the request failed inside the daemon: {e}")),
        },
    };
    wr.write_all(serde_json::to_string(&reply)?.as_bytes()).await?;
    wr.write_all(b"\n").await?;
    Ok(())
}

fn error(e: String) -> Reply<serde_json::Value> {
    Reply {
        error: Some(e),
        result: None,
    }
}

async fn handle(core: &Arc<Core>, req: Request) -> Result<serde_json::Value> {
    Ok(match req {
        Request::Read { identity, target, limit } => serde_json::to_value(core.read(&core.who(&identity)?, target, limit).await?)?,
        Request::Send {
            identity,
            target,
            to,
            text,
        } => serde_json::to_value(core.send(&core.who(&identity)?, target, to, text).await?)?,
        Request::List { identity, project } => serde_json::to_value(core.list_people(&core.who(&identity)?, project).await?)?,
        Request::Route { source, item } => serde_json::to_value(core.route(source, item)?)?,
    })
}
