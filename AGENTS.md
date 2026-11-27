# meridian-comms

People hand work to each other's agents in a shared Slack channel and get the results
back in the same thread. The agents are Meridian sessions; Meridian is the only source
today. One Rust binary holds the daemon (the Slack
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
- **Slack text never reaches a command line, and never starts a session that does
  work.** A woken session gets a one-line notice and fetches the text itself with
  `read`. A filed message's text reaches a source on stdin. There is one exception to
  "starts nothing", and only one: the router's second step, a spawn the daemon starts
  for a filed message. It reads through Meridian and reports one decision line, is
  limited to 5 minutes by its own spawn, and runs one at a time. Add no second
  exception.
- **A filed message's text is a stranger's.** The router sends it to the model and the
  second step's agent and nowhere else: not into a post, a command line or the routing
  log. An offer goes only to a candidate the daemon listed. The agent's question for a
  thread is as untrusted as the message: it is posted `plain`, cut to 300 code points,
  and never logged.
- **The core knows no tool's vocabulary.** Meridian and Slack specifics stay in their
  source or surface module, behind the JSON shapes in `src/model.rs`, which
  [docs/design.md](docs/design.md) describes. A new source is one module.
- **Crash-safe state.** Thread files are written whole (temp file, fsync, rename, fsync
  the directory), and wakes happen before the write, so a crash repeats a notice and
  never loses one.
- **Meridian is used only through its public CLI JSON.** The code writes under
  `~/.meridian/` only by running Meridian's commands, and never writes a `meridian.toml`
  or Meridian's config. The person running the bot edits those by hand: the hook row in
  `~/.meridian/config.toml` is theirs to add. The daemon strips Meridian's session
  variables before calling it.
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

On the machine that runs Jimmy's daemon, other agent sessions run too. Use a private
tmux server (`tmux -L <name>`) for anything you start, and kill only processes you
started, by PID. One bot has one daemon: check for a running one before you start
yours.
