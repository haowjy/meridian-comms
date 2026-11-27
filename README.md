# meridian-comms

Hand work to someone's coding agents in Slack, and get the answer back in the same
thread.

Write `@jimmy-agents please rebase the auth branch` in a channel. The bot lists the
agent sessions Jimmy has running, each with its model and whether it is working, idle
or gone cold. Reply `to: p41`, and that session gets a one-line notice, reads the thread
itself, and answers there. Later messages in the thread go to the same session. Agents
can start threads and hand work to each other the same way, with `meridian-comms send`.

It is built for [Meridian](https://github.com/haowjy/meridian-cli) sessions (Claude Code,
Codex, OpenCode, Pi) and wraps Meridian's public CLI; it changes nothing in Meridian.

**Status:** early. One machine, one Slack bot per person. Amp threads as recipients and
several machines are next.

## How it behaves

- **Nothing wakes a cold session by guessing.** A wake is a paid turn, and waking a
  session whose prompt cache has expired re-reads its whole context. A new thread wakes
  nobody until someone chooses; a cold session isn't woken unless someone names it.
- **A message nobody was woken for waits in Meridian's inbox,** where sessions look. With
  the router switched on, a small model picks the running session it looks meant for,
  and that session is told a message is waiting. It is told only if it is working or its
  cache is still warm.
- **Nothing from Slack runs as a command.** A woken session gets a notice naming the
  message and fetches the text with `meridian-comms read`.
- **Only people listed in the config count.** Everyone else's messages are ignored, and
  their text is hidden from agents.
- **A loop limit** pauses a thread after 12 messages in a row that may be from agents,
  until a person clicks Continue.

## Commands

```
meridian-comms read <message> | --channel NAME     print a thread or a channel's recent threads
meridian-comms send <message> | --channel NAME     post as the bot; --to wakes who it names
meridian-comms list                                who could take a message, and what is waiting
meridian-comms route [ITEM]                        hand an inbox message to the router (run as a hook)
meridian-comms routes [ITEM | <message>]           what the router did with each message
meridian-comms daemon                              the Slack connection (run as a user service)
meridian-comms setup                               print the Slack app manifest
```

## Setup

1. `cargo install --path .`
2. Create the Slack app: [docs/slack-setup.md](docs/slack-setup.md).
3. Write `~/.config/meridian-comms/config.toml` (mode 0600): the owner, the channels, the
   people, and the Meridian projects. The fields are in [docs/design.md](docs/design.md).
4. Run `meridian-comms daemon` outside any agent session, as a user service:
   [docs/service.md](docs/service.md).

How it works and why: [docs/design.md](docs/design.md).
