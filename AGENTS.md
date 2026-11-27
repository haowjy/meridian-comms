# meridian-comms

People on different agent tools hand work to each other's agents in a shared Slack
channel and get the results back in the same thread. Jimmy runs his agents through
Meridian; his cofounder works in Amp. One Rust binary holds the daemon (the Slack
connection, thread state, the wake rule, the router) and the client a session runs
(`read`, `send`, `list`), talking over a Unix socket only the owner's user can open.

The design and its reasons are in [docs/design.md](docs/design.md). Read it before
changing how messages are routed. Slack app setup: [docs/slack-setup.md](docs/slack-setup.md).

## What must hold

- **Wakes cost money.** Each wake is a paid turn, and waking a session whose prompt
  cache expired re-reads its whole context. The rule wakes only what someone chose, and
  a message nobody was woken for waits in a source's inbox. Nothing wakes a cold session
  by guessing: the router's guess is an offer, made only to a session that is working or
  whose cache is warm.
- **Nothing from Slack starts a session or carries text into a command line.** A
  woken session gets a one-line notice and fetches the text itself with `read`. A filed
  message's text reaches a source on stdin.
- **A filed message's text is a stranger's.** The router sends it to the model and
  nowhere else: not into a post, a command line or the routing log. An offer goes only
  to a candidate the daemon listed.
- **The core knows no tool's vocabulary.** Meridian, Amp and Slack specifics stay in
  their source or surface module, behind the JSON shapes the spec gives. A new source
  is one module.
- **Crash-safe state.** Thread files are written whole (temp file, fsync, rename, fsync
  the directory), and wakes happen before the write, so a crash repeats a notice and
  never loses one.
- **Meridian is used only through its public CLI JSON.** Anything under `~/.meridian/`
  is written only through Meridian's commands, and nothing is written into
  `meridian.toml`. The daemon strips Meridian's session variables before calling it.
- **Tokens stay out.** Slack credentials and the router's model key come from
  `~/.config/meridian-comms/slack.env` (mode 0600) or the environment, never from the
  repo, a log line or a chat. This repo is public.

## Engineering

Choose the simpler system, not the smaller change; fix the cause, not the symptom.
Comments only for what the code can't show and a wrong edit would break silently.
Keep no test the user didn't ask for; verify on the running daemon against real Slack.

## Commands

```bash
cargo build
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo test
```

## This machine

Other agent sessions run here. Use a private tmux server (`tmux -L <name>`) for
anything you start, kill only processes you started, by PID, and never start an Amp
runner as Jimmy's own OS user.
