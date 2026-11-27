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
| source | Where agents run. Answers `list` (who could take a message) and `wake`, and may keep an inbox. `src/source/` |
| inbox | A source's place for messages nobody was woken for, where its sessions look. For Meridian, the machine's inbox (`meridian inbox list`). |
| item | One message in an inbox, named by the ID its source gave it: `m412`. |
| offer | Telling a running session that an item looks like its own. The session decides whether to take it. |
| candidate | One thing a source can wake: a Meridian primary. Has a ref, a person, a label, a model, a `default` flag its source sets, and when known an activity. |
| ref | A candidate's name, unique across sources: `desk/mellow-hollow-brood/p41`. The core stores and compares refs and never takes one apart. |
| scope | Per thread and person: the refs that person's messages go to. Empty means the defaults. |
| notice | The one line a woken session gets. It names the message; the session fetches the text with `read`. |
| message ref | `meridian-cli/1731600123.000200`: a channel's configured name and the message's ID. |
| project | A Meridian project ID (`mellow-hollow-brood`). A channel's `project`, a candidate's project and a filed item's project are this one value. |

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
context. Nothing wakes a cold session by guessing, and a message nobody was woken for
waits until a session takes it or someone chooses.

## A message nobody was woken for

**It is filed.** When the rule ran (nobody named a session), the thread is not paused
and nobody was woken, the core asks the first source that has an inbox to file the
message (`src/core/filing.rs`). The Meridian source runs `meridian message inbox - --json`
with the text on stdin, `--project-id` the channel's project, `--reply-to
@<the thread's message ref>`, `--via-program meridian-comms`, `--via-ref` the message ref
and `--via-sender` the sender's directory name. The thread file records `filed`: the
source, the item, where it waits, the message, and the bot's post that names it. That
post is the one the message gets anyway (the picker, or the "nobody was woken"
explanation) with one more line, "In desk's inbox as m412."; when the thread was already
explained, the line is a post of its own.

- A thread has at most one waiting item. A later unwoken message asks the source whether
  the item still waits: if it does, nothing new is filed; if a session took it, the new
  message is filed.
- A paused thread files nothing, or the inbox would be a way round the loop limit.
  Continue files the replayed message if it still wakes nobody.
- A message someone named a session for is not filed, even when the wake failed: it was
  addressed.
- A choice that wakes someone (`to:`, the picker, `send --to`) takes the thread's item
  out of the inbox for the daemon (`meridian inbox take --via-program meridian-comms
  --via-sender <who chose>`) and clears `filed` once it is out: taken now, or already
  taken by a session. After a take that failed the thread still names the item.
- A filed message leaves `meridian-comms list`: the inbox is where a session looks for
  it. `pending` stays in the thread file, for Continue and a later choice to replay.
- Filing fails when Meridian does not answer in 20 s or refuses the message. The
  failure is logged and the message waits as `pending` alone, listed as before.

The thread's lock is held while the message is filed. Filing makes Meridian start the
`inbox.received` hook, so the route request for the item can reach the daemon before
`meridian message inbox` has returned the item's ID. A gate (`Core::filing`) is held
shared from before the filing call until the thread file names the item; the router takes
it exclusively once before it looks for the item's thread, so it never looks too early.

**It is routed, when the router is on:** the config has a `[router]` section and the
daemon has `OPENCODE_API_KEY` ([Config](#config)). Otherwise each `route` is logged as
`off` and the message waits. Filing and routing need Meridian 0.28 or later. The daemon
asks at start whether Meridian has the `inbox` command; on one without it the daemon
says so once in its log, files and routes nothing, and each message waits as `pending`
and shows in `meridian-comms list`.
`meridian-comms route [ITEM]` is a thin client:
it hands the item to the daemon and exits when the daemon has accepted it. Run as
Meridian's hook it reads the item from `MERIDIAN_INBOX_MESSAGE_ID` (the source's
`routed_item` knows the variable; the client does not):

```toml
# ~/.meridian/config.toml
[[hooks]]
name = "comms-route"
event = "inbox.received"
command = "meridian-comms route"
```

The daemon routes each accepted item in its own task (`src/core/route.rs`):

1. Read the item through its source. Stop if it is taken.
2. Find the thread it was filed from by `filed.item` in the thread files, never by the
   item's `via`: anyone can file an item that claims to come from here.
3. List the owner's running sessions in every project (`ListRequest.any_project`; the
   Meridian source makes one `spawn list --global --primary --json` call).
4. Keep a session only if it is in the item's project (an item with no project: any), is
   not the item's sender, and is working or idle with `warm_until` ahead. A session with
   no activity is dropped. None kept, or more than 253: stop, the item waits.
5. Ask Jev (`src/jev.rs`), TypeSafe AI's decision model on OpenCode: one HTTP request
   with a `state` string and one `choice` question. The state is the item's text (first
   4 KiB) with its sender, `via` and project. The options are `s1`…`sN`, one per kept
   session, described by project, agent, work item and activity, plus `none` and `more`.
   Jev answers with a choice and a probability per option; it writes no text.
6. A listed session whose probability is at least `floor` is offered the item. `more`,
   or a session under the floor, goes to the source's [second step](#the-second-step).
   Anything else is `none`.
7. Offer through the source (`meridian inbox offer <item> <spawn-id>`). Meridian sends
   its own fixed notice, which never carries the text, and refuses a session that is
   cold, not running, not a primary, or in another project. A refusal is logged with
   Meridian's word for it and the item waits.
8. If the item came from a thread, take the thread's lock and add "m412 offered to
   `<ref>` (<activity> when listed)" under the post that names the item. The picker keeps
   its menu: an offer is a guess, and a person can still choose.

No thread lock is held while a source or the model is asked.

- **One routing per item.** Meridian fires the hook again when a repeated filing matches
  an existing item. An item whose line in the log is anything but `off`, `capped` or
  `failed`, or one being routed, is not routed again. Those three leave the item waiting,
  and a later `meridian-comms route <item>` runs it.
- **A cap:** 20 routings a minute, counted in memory. Past it the log says `capped`.
- **A failed run changes nothing:** no key, a timeout, any answer but a 2xx with a
  choice in it (429 and 529 included), a Meridian without the inbox.
- **The routing log** is `~/.local/state/meridian-comms/routes.jsonl` (`src/routes.rs`): one JSON
  line per routing with the time, source, item, the thread message ref if any, the refs
  the model chose among, its choice and probabilities, the second step's spawn ID, the
  outcome (`offered`, `not_offered`, `maybe_offered`, `none`, `asked`, `off`, `capped`,
  `failed`), the outcome in words, and the error. It never holds message text.
  A line is appended in one `write` on an `O_APPEND` file and fsynced; a reader skips a
  line that does not parse. At daemon start a file over 5 MB is replaced by its newest
  half. `meridian-comms routes [ITEM | <message>] [--json]` reads the file directly: every
  routing, those of one inbox item (`m412`), or those of one thread message
  (`meridian-cli/1712345678.000100`).

### The second step

When Jev answers `more`, or picks a session under the floor, the core asks the item's
source to decide among the kept sessions (`start_decision`, which names the run for the
log, then `decision`, which waits for it). A source without one
ends the routing as `none`. The Meridian source's is a small agent
(`src/source/meridian/router.rs`): a Meridian spawn of the `inbox-router` profile, which
may run Meridian's read commands to see what the listed sessions are doing and ends its
report with one decision line. Code carries the decision out; the agent can't message,
start or offer anything. It runs on OpenCode with the model the profile names
(`opencode-go/hy4-flash-free` today), on the OpenCode account this machine is signed in
to. `[router].model` does not change it: that setting is Jev's, and Jev is paid for by
`OPENCODE_API_KEY`.

- **The router directory** is `~/.local/state/meridian-comms/router/`: a Meridian project
  of its own holding only that profile. `meridian-comms setup router [--package SPEC]`
  makes it with `meridian -C <dir> mars init` and `mars add <package> --agents
  inbox-router`, and is run again to update the profile. The default package is
  `haowjy/meridian-base`, which does not carry `inbox-router` until Meridian's prompts
  are released: until then pass the `meridian-base` directory of a local checkout of
  them. The daemon never sets it up:
  with no directory, or no OpenCode, the step fails, the log says so and the item waits.
- **The prompt** names the item, says whether the agent may ask, gives the sender, `via`
  and project, lists each kept session by spawn ID with its project, agent, work item,
  activity and the work item's goal, and ends with the text (first 4 KiB) between two
  marker lines carrying a random number, marked as someone else's words; a reply to the
  router's question follows the same way. A goal is read with `meridian -C <root> work
  show <work> --json`, once per project root and work item, four at a time, 20 seconds
  each; the root is the one the listing gave the candidate. It
  is written to a 0600 file in the router directory and removed as soon as the launch
  command returns, whatever it returned. Files a killed daemon left are removed at the
  next start.
- **The run:** `meridian -C <dir> spawn -a inbox-router --bg --timeout 5
  --resident-rearm-budget 0 --prompt-file F --json`, then `spawn wait <id> --timeout 5
  --no-observe`, then `spawn show <id> --json --full --no-observe`. The limit is on the
  spawn itself, so it ends within 5 minutes even if the daemon dies. The wait only
  blocks: what happened is read from the spawn, and a wait that returns while the spawn
  still runs is repeated. A spawn that ended any way but `succeeded` is a failed run; one
  still running 30 seconds past its limit is cancelled. The spawn's ID is in the log
  either way.
- **The decision** is the last non-empty line of the report: `offer <spawn-id>` for a
  session on the list, `none`, or `ask <question>`. Anything else is `none`: more words
  after the ID, an ID not on the list, a bare `ask`, a line wrapped in backticks or a
  code fence.
- **One at a time.** The core runs one second step at once; the next waits, and one that
  has waited 15 minutes is dropped as a failed run with its item left waiting. No thread
  lock and no gate is held while a step runs.
- **`ask`** is honoured only for an item this daemon filed from a thread, and once per
  item. The question is cut down in the core (line breaks to spaces, control and format
  characters removed, spaces collapsed, the first 300 code points) and posted in the
  thread under the daemon's own label: "The router asks about m412: …", then a fixed
  line saying that a reply there makes the router try once more, and that a session can
  still be chosen with `to:` or the picker. The post is marked `plain`: Slack shows it
  as one literal rich-text element, so nothing in it becomes a mention, a channel ping,
  a link or formatting, and the fallback text has `&`, `<` and `>` escaped. The question
  is never logged or kept. An `ask` for an item with no thread is `none`.
- **The reply.** Posting the question sets `filed.question` to `asked` in the thread
  file. The next message there that the rule wakes nobody for, from a person in the
  directory who is not an `agent = "always"` account, while the item still waits and the
  thread is not paused, routes the item once more. The reply's text (first 4 KiB) goes to
  Jev in `state` and to the agent in its prompt, as more of the sender's words; the
  inbox item is not rewritten, and the session that takes it reads the reply with
  `meridian-comms read`. This routing may offer or leave the item; an `ask` from it is
  `none`. It counts toward the cap, and its log line carries `again`, never the reply.
  When it comes to an end the question becomes `answered`, and later replies route
  nothing. If it ended with no offer, the daemon posts "The router found no session for
  m412; choose one with to: or the picker."; after a failed or capped run it stays `asked`, so the next reply tries again.

**What a hostile message can do.** The text is a stranger's. It goes to Jev, which can
only return one of the options the daemon listed, so the worst it does there is pick the
wrong listed session (which gets the fixed notice, one turn on a warm cache) or `none`.
Through the second step it can steer the agent to read anything Meridian's read commands
reach (other sessions' transcripts, reports, work items, configured values), and with
`ask` get up to 300 code points of that posted in its own Slack thread. Cleaning the
question does not close that path. Turning the router on is choosing it: a stranger's
text may steer an agent that can read those things and post one short line of them. The agent can't message, start or
cancel a session, write a file, or take or offer an item. Nothing from the item itself
goes into a Slack post, a command line or the routing log.

**What leaves the machine.** Each routing sends OpenCode, for Jev, the item's text (first
4 KiB), sender, `via` and project ID, the reply to the router's question when there is
one, and for each kept session its project ID, agent name, work item name and activity.
A second step sends the model the profile names the same, each session's work item
goal, and whatever the agent reads.

## Config

`~/.config/meridian-comms/config.toml`, mode 0600. The Slack tokens sit beside it in
`slack.env` ([slack-setup.md](slack-setup.md)), and so does the router's key,
`OPENCODE_API_KEY`: an API key from the OpenCode Console, which pays for Jev. The daemon
refuses to start if either file can be read by anyone but the owner. It starts without
the router's key, and then routes nothing: filed messages wait, and `meridian-comms
routes` shows each as `off`. It starts with no
`[sources.meridian]` too, on a machine without Meridian: threads are read and posted,
nobody is woken, and every message waits and shows in `meridian-comms list`. A
`[sources.meridian]` that is there is checked when the daemon starts: a project key that
is not its root's Meridian project ID, or a root or a `meridian` that can't be asked,
stops it with an error that says which.

```toml
owner = "jimmy"                  # the person whose agents this daemon wakes
machine = "desk"                 # optional; default: the hostname. Starts every ref.

[surface.slack.channels.C0123ABCD]   # the channel's Slack ID
name = "meridian-cli"                # the name in message refs
project = "mellow-hollow-brood"      # optional: a Meridian project ID; none means every project
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
[sources.meridian.projects.mellow-hollow-brood]   # the root's Meridian project ID
root = "/path/to/meridian-cli"
# default = "tech-lead"              # optional, this project only

[router]                             # optional: present, filed messages are routed
# Each line below is the default; an empty section is enough.
# model = "jev-1.13-free"
# floor = 0.6
# url = "https://opencode.ai/inference/systemone/v1/systemone"
```

`floor` is the probability, from 0 to 1, that Jev must give a session before the daemon
offers it the message without asking further. Under it, the second step decides. Higher
means fewer direct offers and more second steps. 0.6 is a placeholder until it has been
measured against real messages.

A project's key is the Meridian project ID of its root, the one `meridian -C <root>
context` prints, and a channel's `project` is the same ID. One value then names the
project in a channel, a candidate, a ref and a filed item, so a message filed for a
channel's project is seen by that project's sessions. At start the Meridian source asks
Meridian for each root's ID. If a key differs, or the root has no ID of its own (no
`[project] id` in its `meridian.toml`), the daemon refuses to start and the error names
the key and the ID to write.

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
removed. Per project it uses `meridian -C <root> spawn list --primary --json --limit 200`,
`spawn message` and, at start, `context --json`. For the machine it runs in the home
directory with no `-C`: `spawn list --global --primary --json`, `message inbox`,
`inbox read`, `inbox take`, `inbox offer`, and `spawn show` to turn an item's sender
(a chat ID) into the spawn ID a candidate carries. The second step adds `work show` in a
candidate's project and, in the router directory, `spawn`, `spawn wait`, `spawn show`
and `spawn cancel`. Rows may carry `agent`, `work_id`,
`activity` and an `idle` object. `activity` of `turn_active`, `starting` or `finalizing`
is working; otherwise `idle` is idle with its cache deadline; neither means activity is
unknown, never cold. `activity` is read first because `idle` means "since the last
user-loop turn ended": a Claude primary running a turn that something injected (a wake,
a task notification) keeps its old `since`. The row has no `waiting_on_user`, and the
source never reports "waiting".

## Not built yet

Offers to sessions on other machines, and the Amp source. Refs already start with the machine name, and the
client already talks to the daemon in JSON.
