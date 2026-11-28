# meridian-comms

Hand work to someone's coding agents in Slack, and get the answer back in the same
thread.

Write `@jimmy-agents please rebase the auth branch` in a channel. The bot lists the
agent sessions Jimmy has running, each with its model and whether it is working, idle
or gone cold. Reply `to: p41`, and that session gets a one-line notice, reads the thread
itself, and answers there. Later messages in the thread go to the same session. Agents
can start threads and hand work to each other the same way, with `meridian-comms send`.

To start a thread for one session without the picker, type `/agent` in a channel the bot
is set up for. A form opens: the session field lists, as you type, the sessions running in the
channel's project on every machine the bot reaches (the form names those machines), and
a message box. Send it, and the
bot opens a thread in the channel with your message, under your name, and wakes the
session you chose. Reply in that thread as usual, `to:` included: Slack doesn't allow
slash commands in threads, so the form only starts one.

The bot's **Home tab** (in Slack's sidebar under Apps, click the bot, then Home) shows
the owner's agents on every machine Meridian has saved: grouped by project and work
item, each with its model and whether it is working, idle or cold, which machines
answered, and how many other sessions run beside them. It shows the newest listing at
once, with its age, and lists again when that is over a minute old or you press Refresh.
An agent the bot can wake, in a project with a configured channel, has a Message button
that opens the `/agent` form with it chosen. Only the owner sees it; anyone else who
opens the tab sees one line saying so.

It is built for [Meridian](https://github.com/haowjy/meridian-cli) sessions (Claude Code,
Codex, OpenCode, Pi) and wraps Meridian's public CLI; it changes nothing in Meridian. It
needs Meridian 0.28.1 or later (`meridian --version`): the daemon and `setup router`
refuse an older one.

**Status:** early. One Slack bot per person, with its daemon on one machine. Sessions on
the other machines Meridian has saved there (`meridian machine add`) are listed, woken,
offered messages and can read and post, the same as the daemon's own; a session is shown
as `p41 on homebox`, and `homebox/p41` names it when two machines each have a `p41`.
Amp threads as recipients are not supported.

## How it behaves

- **Nothing wakes a cold session by guessing.** A wake is a paid turn, and waking a
  session whose prompt cache has expired re-reads its whole context. A new thread wakes
  nobody until someone chooses; a cold session isn't woken unless someone names it.
- **A message nobody was woken for waits in Meridian's inbox,** where sessions look. With
  the router switched on, a small model picks the running session it looks meant for,
  and that session is told a message is waiting. It is told only if it is working or its
  cache is still warm.
- **Slack text never runs as a command.** A woken session gets a notice naming the
  message and fetches the text with `meridian-comms read`. With the router on, a message
  may start one process: the router's own agent, which can only read through Meridian
  and report a decision, for at most 5 minutes.
- **Only people listed in the config count.** Everyone else's messages are ignored, and
  their text is hidden from agents.
- **A loop limit** pauses a thread after 12 messages in a row that may be from agents,
  until a person clicks Continue.

## Commands

```
meridian-comms read <message> | --channel NAME     print a thread or a channel's recent threads
meridian-comms send <message> | --channel NAME     post as the bot; --to wakes who it names
meridian-comms list                                who could take a message, and what is waiting
meridian-comms deliver @PERSON | @<message>        post the text on stdin for `meridian message @…`
meridian-comms route [ITEM]                        hand an inbox message to the router (Meridian's hook runs it)
meridian-comms routes [ITEM | <message>]           what the router did: all, one inbox item (m412), or one message
meridian-comms daemon                              the Slack connection (run as a user service)
meridian-comms setup                               print the Slack app manifest
meridian-comms setup router                        set up the agent the router asks when unsure
```

## Setup

1. `cargo install --path .`, and Meridian 0.28.1 or later
   (`uv tool install --upgrade meridian-cli`).
2. Create the Slack app: [docs/slack-setup.md](docs/slack-setup.md).
3. Write `~/.config/meridian-comms/config.toml` (mode 0600): the owner, the channels, the
   people, and the Meridian projects. The fields are in [docs/design.md](docs/design.md).
4. Run `meridian-comms daemon` outside any agent session, as a user service:
   [docs/service.md](docs/service.md).

## Message a person from a session

`meridian message @alice "the PR is up"` hands the text to the program named in
Meridian's config. To make that this bot, add to `~/.meridian/config.toml` by hand
(`meridian-comms setup` prints the same lines; it never writes Meridian's config):

```toml
[message]
people = ["meridian-comms", "deliver"]
```

Meridian runs the `meridian-comms` a session finds on its `PATH`, so that must be this
build or later: an older one has no `deliver`, and Meridian reads its silence as
`maybe_delivered`. Or put the binary's absolute path as the first word.

`@alice` must be a name under `[people]`. The bot opens a new thread in the channel of
the session's project, or the one channel with no project and `post = true` when the
project has none, starting "From jimmy's session `…` to @alice", so Slack notifies
Alice. She must be in that channel. Her reply in the thread goes to the session that
wrote, as a reply to any session's post does: woken unless it has gone cold, otherwise
waiting. `meridian message @meridian-cli/1731600123.000200 "..."`
answers in that message's thread instead. Either way nobody is woken, and a child spawn
may not post.

## Agents on another machine

The daemon runs on one machine. On each other machine whose agents use the bot:

1. On the bot's machine, save this machine in Meridian (`meridian machine add`), so the
   bot lists and wakes its sessions.
2. Install this build of meridian-comms here (`cargo install --path .`), the same version
   as the bot's: the two check that they speak the same handshake.
3. Check that `ssh <target>` reaches the bot's machine with no prompt: a key loaded or
   without passphrase, and its host key already in `~/.ssh/known_hosts` (run `ssh <target>`
   once in a terminal).
4. Write `~/.config/meridian-comms/config.toml` here with only:

   ```toml
   [bot]
   ssh = "jimmy@raspberrypi"
   # command = "/home/jimmy/.cargo/bin/meridian-comms"   # if it isn't on PATH there
   ```

5. For `meridian message @alice`, add the `[message] people` lines below to this
   machine's `~/.meridian/config.toml` too.

`read`, `send`, `list` and `deliver` then reach the daemon over SSH, and a child spawn may
still read but not post. Run no daemon here. A session in a sandbox that blocks network
access can't reach the bot: its commands say so, and nothing is sent.

## Turn the router on

Without these steps the bot works as above and a message nobody was woken for waits.

1. Have Meridian tell the daemon about each new inbox message. Add to
   `~/.meridian/config.toml`:

   ```toml
   [[hooks]]
   name = "comms-route"
   event = "inbox.received"
   command = "meridian-comms route"
   ```

2. Put `OPENCODE_API_KEY=...` in `~/.config/meridian-comms/slack.env`, beside the Slack
   tokens. It is an API key from the OpenCode Console, and pays for the model that picks
   a session.
3. Add a `[router]` section to `config.toml`. Empty is enough; its three settings and
   their defaults are in [docs/design.md](docs/design.md#config).
4. Run `meridian-comms setup router`. It first checks that Meridian is 0.28.1 or later,
   the first to ship the agent the router asks when the model is unsure, and changes
   nothing if not. Then it makes the router's directory, checks that Meridian can start
   the agent there, and prints the harness and model Meridian chose for it. The agent
   prefers OpenCode; when OpenCode isn't usable there, Meridian falls back to your Mars
   harness order, so it may run on another harness you have, such as Pi, paid for by
   that harness's account. To choose its model, harness or effort, set them in
   `[inbox.router]` in `~/.meridian/config.toml`.
5. Restart the daemon.
6. Send a message that wakes nobody, then run `meridian-comms routes`: it shows what the
   router did with it.

How it works and why: [docs/design.md](docs/design.md).

## Upgrading

The daemon refuses to start with a Meridian older than the one it needs, so upgrade in
this order:

1. Meridian (`uv tool install --upgrade meridian-cli`).
2. meridian-comms (`cargo install --path .`).
3. If the router is on, `meridian-comms setup router`. A router set up by an earlier
   meridian-comms installed its own copy of the agent through Mars, which would replace
   Meridian's; setup removes it.
4. Restart the daemon.

**From a build before machine-wide refs:** the daemon rewrites its thread files to the new
refs at its first start, and old picker clicks, retry lines and `--to` refs still work for
one minor release. A project's `root` in `config.toml` is no longer read: remove it (the
daemon warns until you do). A project is its Meridian project ID, and its agents may run
on any machine the bot reaches. Other machines need the same meridian-comms build and a
`[bot]` config, as in [Agents on another machine](#agents-on-another-machine).

**A Slack app made before `/agent` and the Home tab** needs four changes in its settings
at <https://api.slack.com/apps>, then one reinstall: add the `/agent` slash command, make
sure the `commands` scope is listed, switch the Home tab on under App Home, and subscribe
to the `app_home_opened` bot event. Then reinstall the app to the workspace; the tokens
don't change. `meridian-comms setup` prints the full manifest, and
[docs/slack-setup.md](docs/slack-setup.md) has the steps.
