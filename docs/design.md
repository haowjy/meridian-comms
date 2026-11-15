# Design

`meridian-comms` lets people on different agent tools hand work to each other's agents
in a shared Slack channel and get the answer in the same thread. Jimmy runs sessions
through Meridian; his cofounder works in Amp. This page says what the code does and
why. Slack setup is in [slack-setup.md](slack-setup.md).

## Shape

```
Slack ── surface ──┐                        ┌── source: meridian  (this machine's primaries)
                   ├── core: thread state ──┤
session ─ client ──┘        + wake rule     └── source: amp, remote (later)
```

| Word | Meaning |
|---|---|
| surface | Where people talk (Slack). Delivers messages and clicks, takes posts. `src/surface/` |
| source | Where agents run. Answers `list` (who could take a message) and `wake`. `src/source/` |
| candidate | One thing a source can wake: a Meridian primary. Has a ref, a person, a label, a model, a `default` flag its source sets, and when known an activity. |
| ref | A candidate's name, unique across sources: `desk/meridian-cli/p41`. The core stores and compares refs and never takes one apart. |
| scope | Per thread and person: the refs that person's messages go to. Empty means the defaults. |
| notice | The one line a woken session gets. It names the message; the session fetches the text with `read`. |
| message ref | `meridian-cli/1731600123.000200`: a channel's configured name and the message's ID. |

`src/model.rs` holds the JSON shapes the core exchanges with sources and surfaces. A
module could later run out of process, or on another machine, and only how it is
called would change. `src/core/` knows no tool's vocabulary.

## What a message does

A message counts when it is in a configured channel, its sender is in the directory
(`[people]`), and it either opens with a real mention of the bot (`<@U…>`, first in the
text) or sits in a thread that has a file. The surface drops the bot's own posts,
edits and deletions before the core sees them.

**The rule.** To deliver message M to person P the core asks every source for P's
candidates (one `list` per event) and takes:

- if `scope[P]` is not empty, the candidates in it; otherwise those their source marks
  `default`. The Meridian source marks none unless `default = "<agent>"` is configured,
  so a new thread wakes nobody;
- of those, every one that is not **cold** (idle, with a `warm_until` that has passed).

All three ways in (a message in a thread, `send`, the Continue click) go through one
function, `deliver` in `src/core/deliver.rs`. It takes, per person, either the refs chosen
or "run the rule"; wakes each ref once however many picks name it; records the ref each
wake *returned* (a source may open a new thread per wake) in `scope` and `woken`; and
sets `pending` and `streak`. In a paused thread it wakes nobody: the choice still takes the scope, and the message waits as `pending`. Each one woken gets a notice naming the others, and
carrying the message text and a link for a source that cannot run `read` (the Meridian
source leaves the text out of its line). When nobody is woken the message
becomes `pending`, shown by `meridian-comms list`, and the bot says in the thread who
was skipped and why. `pending` holds the message and the picks it was delivered with
(`{person: [refs] | "rule"}`), and Continue replays exactly those. Whether the bot owes
that explanation reads a separate `explained` flag, set when it posts and cleared by any
wake, so an agent's `send` that woke nobody never silences the explanation a person's
next message is owed. On a thread's first message that post is the picker: who is
running, with model and activity, and the line "reply `to: <ref>`", which is the base
form because an agent can't click.

**The picker's menu** is the same post's second form. The surface trait's `post` may carry
one `Menu` (groups of options plus a button label); the core builds it, one group per
person and project, each option's value a ref and its label the short ID, label, model
and activity. Slack fits it to its limits (75-character labels, 100 groups and options,
150-character values; an option whose ref is longer is left out and stays in the text).
On Slack the menu is a `multi_static_select` as the section's accessory, because Slack
refuses a multi-select inside an `actions` block, and "Wake" is a button in the `actions`
block under it. Picking in the menu sends nothing the daemon uses. The Wake click carries
the selection in the payload's `state`, and becomes the `choose` action: a choice exactly
like a `to:` line from whoever clicked (they must be in the directory and not
`agent = "always"`), delivered for the waiting message, or the thread's first one. The
bot then edits the picker post: the menu and button go, and a line says who chose what.
Refs the sources no longer list are named in a reply and left out. A click in a paused
thread takes the choice and wakes nobody, as a `to:` line does.

**A choice always wakes**, cold or not: a message opening `to: p41 desk/x/p7` (or
`to: all` for the defaults), or `send --to`. A short ID stands for its ref when exactly
one listed candidate has it. A choice replaces the scope of the person whose refs it
names; sessions woken before and no longer in the scope get one "now with" notice.
A `send --to` with no text is a pointer at an existing message, not a hand-off: the refs
it wakes join the scope and nobody is released. That is the form of the retry line.
A `send` that posts text and names refs is a hand-off and replaces the scope.
A session that `send`s in a thread joins its person's scope, and a choice never drops
the sender from it.

**Wakes cost money:** a wake is a paid turn, and a cold session re-reads its whole
context. Nothing wakes by guessing, and a message nobody was woken for waits until
someone pulls it or chooses.

## Config

`~/.config/meridian-comms/config.toml`, mode 0600. The Slack tokens sit beside it in
`slack.env` ([slack-setup.md](slack-setup.md)). The daemon refuses to start if either
file can be read by anyone but the owner.

```toml
owner = "jimmy"                  # the person whose agents this daemon wakes
machine = "desk"                 # optional; default: the hostname. Starts every ref.

[surface.slack.channels.C0123ABCD]   # the channel's Slack ID
name = "meridian-cli"                # the name in message refs
project = "meridian-cli"             # optional; none means every project
post = false                         # sessions may open threads here (needed with no project)
external = false                     # set true to allow a channel shared with another org

[people.jimmy]
slack = "U0123ABCD"                  # Slack member ID
agent = "maybe"                      # no | maybe | always: may an agent post as this person?
[people.alice]
slack = "U0456EFGH"
note = "cofounder, works in Amp"     # shown in notices
agent = "maybe"

[sources.meridian]
# default = "product-lead"           # optional: primaries of this agent wake with no choice
[sources.meridian.projects.meridian-cli]
root = "/path/to/meridian-cli"
# default = "tech-lead"              # optional, this project only
```

A channel the config doesn't list is ignored. A message counts only if its sender is
under `[people]`. `agent` feeds the [loop limit](#loop-limit): a message from a person
marked `maybe` or `always` counts toward the pause, and Continue from an `always` person
is ignored.

## Loop limit

`streak` counts messages in a row that woke someone and may be from agents: a
`send --to`, a message the surface marks as an app's, or one from a person whose entry
has `agent` set. A message from anyone else resets it. At 12 the thread is `paused`:
later messages wait as `pending` with their picks, `send --to` is refused, and the bot posts a Continue
button. A person in the directory whose `agent` is not `always` may press it. There is
no typed `continue`: an agent posting under a person's name could type it.

## State

One JSON file per thread in `~/.local/state/meridian-comms/threads/`, written whole
(temp file, fsync, rename, fsync of the directory) by the daemon alone, under one lock
per thread. A thread's events run in arrival order. A `send` that opens a thread takes
its lock and writes its file as soon as Slack returns the post, before it wakes, so a
reply that arrives mid-wake waits its turn and finds the scope. Wakes happen before
the final write, so a crash repeats a notice and never loses one. `handled` holds the last 50 message IDs,
so a redelivery is dropped.

A `to:` line that names a word no answering source resolves while a source is down is
neither text nor a miss yet. The daemon says once in the thread that a source did not
answer and retries every 30 s (one loop per message, not per delivery). If a later
message in the thread was handled by then, the choice is dropped with a post saying so
rather than applied out of order.

A thread file that does not parse is moved to `*.corrupt-<time>` and the thread is
treated as new. It loses the scope, the waiting message and `handled`; a reply without
the mention is ignored from then on, and with a `default` configured a redelivered first
message wakes it again. The log names the moved file. Only "not found" means a new
thread: any other read error keeps the event in flight, so an intact file is never
replaced.

`cursor.json` holds per channel an ID below which every event was handled. Saves happen
under the cursor's mutex, each with its own temp file name. It never moves past an
event still in flight, and a channel's position stops while that channel's recovery is
failing. On start and on every reconnect each channel recovers on its own: one
`conversations.history` call for the last 7 days (or back to the cursor, if older), then
the replies, after the cursor, of the threads whose `latest_reply` is past it. What
`handled` already has is dropped. A channel that fails is logged once and retried every
30 s while the others go on. An event whose handling errors stays in flight and is
handled again at the next recovery. A channel's first run starts at now.

## Slack

Hand-rolled, no SDK. Socket Mode: `apps.connections.open`, then a websocket of JSON
envelopes, each acknowledged by `envelope_id` before it is handled. A `disconnect`
frame or a silent link (120 s) means reconnect.

**One bot, one daemon.** Slack hands each event to any one of an app's connections, so
a second daemon on the same bot would take some of the events. Each connection's `hello`
carries `num_connections`, and only the new connection learns the count: the one already
connected is never told. So the daemon checks its first `hello` after start, and exits
with status 2 when the count is above 1. It doesn't check on reconnect, because Slack's
own refresh briefly overlaps the old connection with the new one. After a crash, Slack
may count the dead connection for a while, so a restart can be refused until Slack
drops it; run the service with a restart delay. Web API calls: `chat.postMessage`,
`reactions.add`, `conversations.replies`, `conversations.history`, `auth.test`,
`conversations.info`; a 429 waits out `Retry-After`. A post replaces `<!` with `&lt;!`
so text from a session can't ping a channel. A channel shared with another
organization is refused at start unless its entry says `external = true`.

## Client and daemon

The client learns who is calling from the source (`identify`): a Meridian primary is a
session of its project; a child spawn may read but not post; a session whose project is
not in the config is refused; a process with no Meridian variables is the owner at a
terminal. Requests go over a Unix socket (mode 0600, peer uid checked) as one JSON
object each way (`src/proto.rs`). A session reads the channels of its own project and
those with no project. A channel with no project needs `post = true` before sessions
may open threads there. `send` waits for the post and the wakes. A cold session the rule skips is reported and
is not a failure; the command exits non-zero, with a retry line naming only the refs
whose wake errored. A request runs to the end in its own task in the daemon whether or
not the client is still waiting (the client gives up after 300 s and says the daemon
may still finish). The socket is `/run/user/<uid>/meridian-comms.sock` where that
directory exists, whatever `XDG_RUNTIME_DIR` the caller has, so a session started
without it finds the same path as the daemon.

The daemon calls `meridian` with every `MERIDIAN_*` variable except `MERIDIAN_HOME`
removed, and uses only `meridian -C <root> spawn list --primary --json --limit 200`
and `spawn message`. Rows may carry `agent`, `work_id` and an `idle` object; their
absence means activity is unknown, never cold. `idle` means "since the last user-loop
turn ended": a Claude primary running a turn that something injected (a wake, a task
notification) keeps its old `since`, so an `idle` row may be working, and a candidate
that looks cold may be mid-turn. The row has no `waiting_on_user`, and the source never
reports "waiting".

## Not built yet

The Amp source, and joining several machines. Refs already start
with the machine name, and the client already talks to the daemon in JSON.
