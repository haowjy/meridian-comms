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

Each one woken gets a notice naming the others. When nobody is woken the message
becomes `pending`, shown by `meridian-comms list`, and the bot says in the thread who
was skipped and why. On a thread's first message that post is the picker: who is
running, with model and activity.

**A choice always wakes**, cold or not: a message opening `to: p41 desk/x/p7` (or
`to: all` for the defaults), or `send --to`. A short ID stands for its ref when exactly
one listed candidate has it. A choice replaces the scope of the person whose refs it
names; sessions woken before and no longer in the scope get one "now with" notice.
A session that `send`s in a thread joins its person's scope.

**Wakes cost money:** a wake is a paid turn, and a cold session re-reads its whole
context. Nothing wakes by guessing, and a message nobody was woken for waits until
someone pulls it or chooses.

## Loop limit

`streak` counts messages in a row that woke someone and may be from agents: a
`send --to`, a message the surface marks as an app's, or one from a person whose entry
has `agent` set. A message from anyone else resets it. At 12 the thread is `paused`:
later messages become pending, `send --to` is refused, and the bot posts a Continue
button. A person in the directory whose `agent` is not `always` may press it. There is
no typed `continue`: an agent posting under a person's name could type it.

## State

One JSON file per thread in `~/.local/state/meridian-comms/threads/`, written whole
(temp file, fsync, rename, fsync of the directory) by the daemon alone, under one lock
per thread. A thread's events run in arrival order. Wakes happen before the write, so
a crash repeats a notice and never loses one. `handled` holds the last 50 message IDs,
so a redelivery is dropped.

A thread file that does not parse is moved to `*.corrupt-<time>` and the thread is
treated as new. That cannot wake anyone, since a thread with no scope wakes only
defaults. It loses the scope, the waiting message and `handled`, and a reply without
the mention in that thread is ignored from then on; the log names the moved file.

`cursor.json` holds per channel an ID below which every event was handled. It never
moves past an event still in flight, and it stops while recovery is failing. On start
and on every reconnect the daemon reads each channel's history after the cursor and
the replies of each thread that is new or changed in the last 7 days, and handles what
`handled` lacks. A channel's first run starts at now.

## Slack

Hand-rolled, no SDK. Socket Mode: `apps.connections.open`, then a websocket of JSON
envelopes, each acknowledged by `envelope_id` before it is handled. A `disconnect`
frame or a silent link (120 s) means reconnect; the `hello` frame's `num_connections`
is logged, and more than one warns. Web API calls: `chat.postMessage`,
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
may open threads there. `send` waits for the post and the wakes, and exits non-zero
with a retry command when a wake failed.

The daemon calls `meridian` with every `MERIDIAN_*` variable except `MERIDIAN_HOME`
removed, and uses only `meridian -C <root> spawn list --primary --json --limit 200`
and `spawn message`. Rows may carry `agent`, `work_id` and an `idle` object; their
absence means activity is unknown, never cold.

## Not built yet

The picker's menu, the Amp source, and joining several machines. Refs already start
with the machine name, and the client already talks to the daemon in JSON.
