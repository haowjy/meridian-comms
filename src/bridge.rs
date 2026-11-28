//! A client on another machine reaches the daemon over SSH, through
//! `meridian-comms bridge` on the bot's machine, the way Meridian's
//! `machine run` is reached. The client runs `ssh <options> -- <target>
//! <command> bridge` and writes nothing yet. `bridge` connects to the
//! daemon's socket and prints the ready line, or an error line if it can't.
//! Only after the ready line does the client write its one request, on stdin:
//! nothing from the request is ever on a command line. Before the ready
//! line, every failure is "not sent"; after the first request byte, an answer
//! that doesn't arrive whole may have been acted on.

use std::process::Stdio;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::config::BotConfig;
use crate::model::{FailKind, Failure};
use crate::paths;
use crate::proto::MAX_REQUEST;

const READY: &str = "meridian-comms-ready ";
const ERROR: &str = "meridian-comms-error ";
/// The handshake's version: a client and a `bridge` of different builds that
/// would misread each other's request differ in it.
const VERSION: u64 = 1;
/// From starting `ssh` to the ready line: ssh gives up connecting after 10.
const CONNECT: Duration = Duration::from_secs(15);
/// What `bridge` waits for a request and its answer: the client's longest
/// wait and some.
const LIMIT: Duration = Duration::from_secs(330);

/// Meridian's options for a call nobody watches: no prompt for a passphrase,
/// a password or a host key, a closed connection noticed within a minute, and
/// ssh's errors (a refused key, a changed host key) still printed.
const SSH_OPTIONS: [&str; 17] = [
    "-T",
    "-o",
    "BatchMode=yes",
    "-o",
    "NumberOfPasswordPrompts=0",
    "-o",
    "StrictHostKeyChecking=yes",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ConnectionAttempts=1",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=4",
    "-o",
    "LogLevel=ERROR",
];

/// `meridian-comms bridge`: one request from stdin to the daemon's socket,
/// and its answer back. It ends when the answer is out, when its stdin ends
/// (the client's `ssh` is gone), and at its own limit, so a killed client
/// never leaves it holding a daemon connection.
pub async fn serve() -> ! {
    let mut out = tokio::io::stdout();
    let path = paths::socket_path();
    let conn = match UnixStream::connect(&path).await {
        Ok(c) => c,
        Err(e) => {
            let text = format!(
                "the meridian-comms daemon isn't reachable at {} ({e}): start it there with `meridian-comms daemon`",
                path.display()
            );
            let _ = out.write_all(format!("{ERROR}{}\n", json!({ "text": text })).as_bytes()).await;
            let _ = out.flush().await;
            std::process::exit(1);
        }
    };
    let _ = out.write_all(format!("{READY}{}\n", json!({ "v": VERSION })).as_bytes()).await;
    let _ = out.flush().await;
    let mut input = BufReader::new(tokio::io::stdin());
    let relayed = tokio::time::timeout(LIMIT, relay(&mut input, conn, &mut out)).await;
    let _ = out.flush().await;
    // Exits at once: a read of stdin may still hold a thread.
    std::process::exit(if matches!(relayed, Ok(Ok(true))) { 0 } else { 1 })
}

/// The request line, then the answer line or the end of stdin, whichever is
/// first. `true` when the answer went out.
async fn relay<R: AsyncRead + Unpin>(input: &mut BufReader<R>, conn: UnixStream, out: &mut tokio::io::Stdout) -> Result<bool> {
    let mut request = Vec::new();
    (&mut *input).take(MAX_REQUEST as u64 + 1).read_until(b'\n', &mut request).await?;
    if !request.ends_with(b"\n") {
        return Ok(false);
    }
    let (rd, mut wr) = conn.into_split();
    wr.write_all(&request).await?;
    let mut answer = Vec::new();
    let mut rest = [0u8; 1];
    let mut rd = BufReader::new(rd);
    tokio::select! {
        r = rd.read_until(b'\n', &mut answer) => {
            r?;
            out.write_all(&answer).await?;
            Ok(answer.ends_with(b"\n"))
        }
        // The client writes nothing more: this is its end.
        _ = input.read(&mut rest) => Ok(false),
    }
}

/// A connection to the daemon through `bridge` on the bot's machine, after
/// the ready line. Dropping it ends `ssh`.
pub struct Bridge {
    pub reader: BufReader<ChildStdout>,
    pub writer: ChildStdin,
    _ssh: Child,
}

/// Starts `ssh` to the bot's machine and waits for the ready line. Every
/// error is a `Failure` that says nothing was sent.
pub async fn open(bot: &BotConfig) -> Result<Bridge> {
    let target = &bot.ssh;
    let not_sent = |text: String| anyhow::Error::from(Failure::new(FailKind::Unreachable, format!("{text}; nothing was sent")));
    let mut ssh = Command::new("ssh")
        .args(SSH_OPTIONS)
        .args(["--", target, &bot.command, "bridge"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                not_sent("`ssh` is not installed or not on PATH; this machine reaches the bot through it".into())
            }
            std::io::ErrorKind::PermissionDenied => not_sent(format!("this session's sandbox blocks ssh to {target} ({e})")),
            _ => not_sent(format!("cannot run ssh ({e})")),
        })?;
    let mut reader = BufReader::new(ssh.stdout.take().expect("piped"));
    let writer = ssh.stdin.take().expect("piped");
    let mut stderr = ssh.stderr.take().expect("piped");
    let ready = tokio::time::timeout(CONNECT, async {
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).await? == 0 {
                return Ok::<_, std::io::Error>(None);
            }
            // A login's rc file may print before the command runs.
            if let Some(v) = line.strip_prefix(READY).or_else(|| line.strip_prefix(ERROR)) {
                return Ok(Some((
                    line.starts_with(READY),
                    serde_json::from_str::<Value>(v.trim()).unwrap_or_default(),
                )));
            }
        }
    })
    .await;
    let says = match ready {
        Err(_) => return Err(not_sent(format!("{target} gave no ready line within {} s", CONNECT.as_secs()))),
        Ok(Err(e)) => return Err(not_sent(format!("reading from ssh to {target} failed ({e})"))),
        Ok(Ok(says)) => says,
    };
    match says {
        Some((true, v)) if v["v"] == VERSION => Ok(Bridge { reader, writer, _ssh: ssh }),
        Some((true, v)) => Err(not_sent(format!(
            "meridian-comms on {target} is a different build (its handshake is version {}, this one's {VERSION}): install the same version on both machines",
            v["v"]
        ))),
        Some((false, v)) => Err(not_sent(format!(
            "on {target}: {}",
            v["text"].as_str().unwrap_or("bridge could not reach the daemon")
        ))),
        None => {
            let mut err = String::new();
            let _ = tokio::time::timeout(Duration::from_secs(2), stderr.read_to_string(&mut err)).await;
            let status = tokio::time::timeout(Duration::from_secs(5), ssh.wait()).await;
            let code = status.ok().and_then(|s| s.ok()).and_then(|s| s.code());
            Err(not_sent(match code {
                Some(255) => not_connected(&err, target),
                Some(127) => format!(
                    "`{}` was not found on {target}: install meridian-comms there, or set [bot] command to its path",
                    bot.command
                ),
                _ => format!(
                    "`{} bridge` on {target} ended before it was ready ({}): a meridian-comms older than `bridge` must be upgraded there",
                    bot.command,
                    last_line(&err).unwrap_or("no error printed")
                ),
            }))
        }
    }
}

/// Why an `ssh` that exited 255 reached no command, from what it printed, the
/// way Meridian tells them apart: a host key, a refused sign-in, a sandbox
/// that refused the connection, or anything else.
fn not_connected(stderr: &str, target: &str) -> String {
    let last = last_line(stderr).unwrap_or("ssh exited 255");
    let lower = stderr.to_lowercase();
    if lower.contains("remote host identification has changed") || lower.contains("host key verification failed") {
        format!(
            "{target}'s host key is not one this machine knows, and background calls accept only known keys: if it is new, run `ssh {target}` once in a terminal; if its key changed, fix ~/.ssh/known_hosts yourself ({last})"
        )
    } else if lower.contains("operation not permitted") {
        format!("this session's sandbox blocks ssh to {target} ({last})")
    } else if lower.contains("permission denied") {
        format!("sign-in to {target} failed: run `ssh {target}` once in a terminal, or load the key with `ssh-add` ({last})")
    } else {
        format!("{target} can't be reached over SSH ({last})")
    }
}

fn last_line(s: &str) -> Option<&str> {
    s.lines().map(str::trim).rfind(|l| !l.is_empty())
}
