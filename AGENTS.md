# meridian-comms

People on different agent tools hand work to each other's agents in a shared Slack
channel and get the results back in the same thread. Jimmy runs his agents through
Meridian; his cofounder works in Amp. One Rust binary holds the daemon (the Slack
connection, thread state, the wake rule) and the client a session runs (`read`,
`send`, `list`), talking over a Unix socket only the owner's user can open.

The design and its reasons are in [docs/design.md](docs/design.md). Read it before
changing how messages are routed. Slack app setup: [docs/slack-setup.md](docs/slack-setup.md).

## What must hold

- **Wakes cost money.** Each wake is a paid turn, and waking a session whose prompt
  cache expired re-reads its whole context. The daemon wakes only what someone chose,
  never a cold session on its own, and a message nobody was woken for waits. Never add
  a path that wakes by guessing.
- **Nothing from Slack starts a process or carries text into a command line.** A
  woken session gets a one-line notice and fetches the text itself with `read`.
- **The core knows no tool's vocabulary.** Meridian, Amp and Slack specifics stay in
  their source or surface module, behind the JSON shapes the spec gives. A new source
  is one module.
- **Crash-safe state.** Thread files are written whole (temp file, fsync, rename, fsync
  the directory), and wakes happen before the write, so a crash repeats a notice and
  never loses one.
- **Meridian is used only through its public CLI JSON.** Nothing is written under
  `~/.meridian/` or into `meridian.toml`. The daemon strips Meridian's session
  variables before calling it.
- **Tokens stay out.** Slack and Amp credentials come from
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
