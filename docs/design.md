# Design

`meridian-comms` lets people on different agent tools hand work to each other's agents
in a shared Slack channel and get the answer in the same thread. Jimmy runs sessions
through Meridian; his cofounder works in Amp. This page says what the code does and
why. Slack setup is in [slack-setup.md](slack-setup.md).

## Shape

```
Slack ── surface ──┐                        ┌── source: meridian  (the primaries of this
                   ├── core: thread state ──┤    machine and every saved one)
session ─ client ──┘        + wake rule     └── source: amp (later)
   (here, or over SSH through `bridge`)
```

| Word | Meaning |
|---|---|
| surface | Where people talk (Slack). Delivers messages and clicks, takes posts. `src/surface/` |
| source | Where agents run. Answers `list` (who could take a message) and `wake`, and may keep an inbox. `src/source/` |
| inbox | A source's place for messages nobody was woken for, where its sessions look. For Meridian, the machine's inbox (`meridian inbox list`). |
| item | One message in an inbox, named by the ID its source gave it: `m412`. |
| offer | Telling a running session that an item looks like its own. The session decides whether to take it. |
| candidate | One thing a source can wake: a Meridian primary. Has a ref, the names it answers to, a person, a label, a model, `wakeable` and `default` flags its source sets, and when known an activity. |
| ref | A candidate's name, unique across sources and unchanged by a rename: `6020323b2671/mellow-hollow-brood/p41`, the machine's ID first. The core stores and compares refs and never takes one apart. People see it as `p41 on jimyao-pop-os`. |
| names | The other words that name a candidate: `p41`, `jimyao-pop-os/p41`, and for one minor release its old name-based ref. `to:`, clicks and `--to` match them as they match a ref. |
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

**The rule.** To deliver message M to person P the core reads a listing no older than
15 s (see [The listing](#the-listing)) and takes, of P's candidates:

- if `scope[P]` is not empty, the candidates in it; otherwise those their source marks
  `default`. The Meridian source marks none unless `default = "<agent>"` is configured,
  so a new thread wakes nobody;
- of those, every one that is not **cold** (idle, with a `warm_until` that has passed).

All the ways in (a message in a thread, `send`, the Wake and Continue clicks, the form) go through one
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
person and project, each option's value a ref and its label how the session is shown
(`p41 on jimyao-pop-os`), label, model and activity. Slack fits it to its limits (75-character labels, 100 groups and options,
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

**The form** starts a thread for one session, on any machine the bot reaches. A person opens it in a configured channel
(on Slack, `/agent`). It has a line naming the machines whose sessions it lists, a session
field the bot fills as they type, and a message box. Sending it opens a thread in the
channel with the message and delivers it through `deliver` as that person's choice of the
session, the way a `to:` line opening a thread is: the session is woken, cold or not, and
becomes the person's scope, so later messages in the thread go to it. The bot reacts 👀
when it woke someone, and otherwise posts the picker with the reason. Only a person in the
directory who is not `agent = "always"` may open or send it, and only in a configured
channel; anyone else gets a reply only they see, never a post. Replies in the thread keep
`to:` and the picker: Slack refuses slash commands in threads without calling the app.

The form is opened for one channel or for several to choose from, all of one project, and
optionally with a session already chosen. `/agent` passes the channel it was typed in and
no session. The Message button on the [Home tab](#the-home-tab), whose click names no
channel, passes the channels configured for the session's project (with exactly one, that one; with
several, the form offers them; with none, there is no button). On Slack the modal carries
its channels and that session to the submission in `private_metadata`, and the submission
checks the channel and the person again and the session against the listing.

The bot posts the thread's first message, since it can't post as the person. The post
shows the person's name above their text, and its `text` is their words alone. The
thread file's `authors` maps that post's ID to the person, and `author` in
`src/core/mod.rs` reads it, so `read`, Continue and `send --to` without text take the post
as the person's message. The delivery counts it toward the loop limit as theirs (by their
`agent` mark).

**Answering within seconds.** Slack waits 3 s for the acknowledgement of the command, of
each request for the session field's options, and of the submission, and the answer
travels in that acknowledgement: the options, errors that keep the form open, or a reply
only the person sees. So the socket acknowledges those envelopes only once they are
answered. They reach the core on a channel of their own, apart from the event stream, and
each is answered in its own task, so none waits behind another or behind recovery. The
core answers within 2.2 s, and the socket gives up at 2.5 s with its own "try again". For
the command, the socket opens the form (`views.open` with the command's `trigger_id`)
before it acknowledges, and says so if that failed.

The options come from the newest listing (`online` in `src/core/online.rs`), kept to
what can be woken in the channel's project (every configured project for a channel with
none), cold ones included. The form never waits for a listing that is running, since a
listing across machines takes seconds; it starts one when the newest is over 15 s old.
Until the first is done, the field shows one option saying it is still listing; with no
sessions, it says none are running, and so does the line above the fields once that is
known. A listing over 5 minutes old counts as none, and callers wait for the new one as
for the first.

A submission is checked against the newest listing, and a session missing from it is an
error on the form that says to send again, and starts a listing so the next send sees a
session that started since. The post is made before the answer, and a post that fails, or is still
running when the time is up, is an error on the form that says to send again. Sending
again is safe: each send names the open form (on Slack, the view's ID, the same each time
it is sent), and the core remembers the forms it posted for an hour, so a form sent again,
because the first answer was lost or late, gets that send's outcome and is never posted
or woken twice. A post known not to have gone out (`not_sent`, `too_large`) is forgotten, so
sending again posts. One that may have gone out (`maybe_sent`) stays remembered, and the
error tells the person to look in the channel before using the form again. The wake runs after
the answer, against the listing the submission was checked with, since listing again
would leave the post unwoken for seconds; a session that ended in between fails its wake
and is named in the thread as left out. On a `disconnect` frame the socket sends the
acknowledgements it holds before it reconnects.

## The listing

**One listing, not one per project.** A source's `list` takes nothing: it gives every
running primary on every machine it reaches, in every project, configured or not, each
with `wakeable` and `names`, plus `groups` and `machines` (below). The core keeps the
newest answer as one snapshot (`src/core/online.rs`), so a refresh is one fan-out, never
one per project, and the bot's machine never runs two at once. Each caller filters it,
in one place (`Core::reach`):

| Caller | Candidates it uses |
|---|---|
| the wake rule, `to:`, picker clicks, `send --to`, Continue, the picker's menu | `wakeable`, and either in the channel's project or already among the thread's refs |
| the form, `list` | `wakeable` in the channel's (or the asked) project |
| the Home tab | all of the owner's, wakeable or not, in every project; Message only where `wakeable` |
| the router | all of the owner's from its source, configured projects or not |

`default` is set only where `wakeable` is, so a default agent in an unconfigured project is
never picked, and a bare `to: p41` is checked for uniqueness only among the candidates its
caller uses. A name two of them share is ambiguous, and the bot lists what to type for
each (`jimyao-pop-os/p41`).

**Freshness.** Each caller says what it needs (`Need`):

| Caller | Reads |
|---|---|
| the form's options and submission, the Home tab | the snapshot, with no wait (Slack's 3 s clock). The Home tab lists again when the newest is over a minute old, or always for Refresh. |
| everything else: the wake rule for each handled message, `to:`, picker clicks, Continue, `send --to`, `list`, the router | a listing no older than 15 s: the newest, the running one if it started since, or else the next, which starts as soon as the running one ends |
| any caller but the form, for a word that matched nothing | one listing started after the miss, before the word is reported as matching nothing |

So a handled message waits for a listing only when the newest is over 15 s old: in a busy
thread, most messages, at 8 to 11 s each on a Raspberry Pi asking one other machine.

The worst case is a few fan-outs, all while the thread's lock is held. The message's own
listing can mean waiting out the one running and then the next. A `to:` word that
matches nothing then needs one more listing that started after the miss. With every
machine up that is at most three fan-outs, about 30 s on the Pi before the wakes start;
four when a fan-out runs past 15 s, since the miss can then wait out a running one too. A machine that hangs makes each fan-out its full
per-machine limit, about 15 s (2.5 s of `machine list` plus 12 s). The code caps each of
the two waits at 75 s, and the wakes, which run at once, at 100 s, so a handled
message takes at most about 250 s. `send --to`'s 300 s on the client covers that.

A retry comes 30 s after the
attempt it repeats, so it never reuses the listing that attempt failed on. Up to 15 s
old, a default agent that just started isn't woken (the message is filed), and a scoped
session that just ended gets a wake that fails as `not_running`, named in the thread.
Each source has 30 s for its listing.

**What didn't answer, per machine.** A listing's `machines` names each machine the source
looked on, by ID and name, and whether it answered; a `machine list` that fails is one
entry with no ID. A source that fails whole is in `failed`. The core can't tell which
machine a word means, so it asks the source: `could_be_unchecked(word, missing)` is true
only when the word could name a session on a machine that didn't answer. For Meridian
that is a bare spawn ID, or a word that starts with that machine's name or ID (a word of
the message's text, as in `to: p41 please`, is neither). `<a machine that answered>/p4` is
a real miss.

- A `to:` word that could be unchecked makes the message wait and retry every 30 s
  (`Flow::SourceDown`). After 5 minutes it answers: "`jimyao-pop-os` didn't answer, and
  `p4` isn't running on the machines that did". A saved machine can be off for days.
- A scoped session the listing lacks is "not checked" when its machine didn't answer,
  and "no longer running" otherwise.
- The router offers among the machines that answered; only a listing where its source
  failed stops it.

**A choice always wakes**, cold or not: a message opening `to: p41 jimyao-pop-os/p7` (or
`to: all` for the defaults), or `send --to`. A name stands for its ref when exactly
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
`off` and the message waits. `meridian-comms route [ITEM]` is a thin client:
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
3. List every session the item could be offered to (the router's reader in
   [The listing](#the-listing)): the owner's running sessions in every project, on this
   machine and on each [saved machine](#saved-machines), from a listing no older than
   15 s. A machine that didn't answer is
   left out and named in the log; only a source that fails outright fails the routing.
4. Keep a session only if it is in the item's project (an item with no project: any), is
   not the item's sender, and is working or idle with `warm_until` ahead. A session with
   no activity is dropped. None kept, or more than 253: stop, the item waits.
5. Ask Jev (`src/jev.rs`), TypeSafe AI's decision model on OpenCode: one HTTP request
   with a `state` string and one `choice` question. The state is the item's text (first
   4 KiB) with its sender, `via` and project. The options are `s1`…`sN`, one per kept
   session, described by machine, project, agent, work item and activity, plus `none` and
   `more`.
   Jev answers with a choice and a probability per option; it writes no text.
6. A listed session whose probability is at least `floor` is offered the item. `more`,
   or a session under the floor, goes to the source's [second step](#the-second-step).
   Anything else is `none`.
7. Offer through the source (`meridian inbox offer <item> <spawn-id>`, or
   `<machine>/<spawn-id>` for a session on a saved machine). Meridian sends its own
   fixed notice, which never carries the text, and refuses a session that is cold, not
   running, not a primary, or in another project. A refusal is logged with Meridian's
   word for it and the item waits.
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
  machines that didn't answer the listing (`missing`, each with its ID, name and why), the
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
start or offer anything. Meridian ships the profile from 0.28.1 on, and finds it in any
directory after the project's and the user's own Mars agents, so nothing installs it.
Meridian picks the harness. The profile prefers OpenCode with the model it names
(`opencode-go/hy4-flash-free` today); when OpenCode isn't usable from the router
directory, Mars falls back to the user's harness order, and a `mars.toml` above the
directory can narrow the harnesses it may use (a `~/mars.toml` with `targets` does, since
the directory is under `~`). So it may run on Pi, on that harness's account. Setup prints
what Meridian chose. Its `model`, `harness` and `effort` are set by hand in
`[inbox.router]` in the user's `~/.meridian/config.toml`; Meridian reads that section
from no project file. `[router].model` does not change it: that setting is Jev's, and Jev
is paid for by `OPENCODE_API_KEY`.

- **The router directory** is `~/.local/state/meridian-comms/router/`: a Meridian project
  of its own, which holds the prompt files. `meridian-comms setup router` sets it up, and
  the daemon never does. Setup first runs the daemon's version check (below), before
  anything in the directory changes, so with a Meridian that doesn't ship the profile a
  router that worked keeps working. It then removes an earlier setup's Mars dependency
  (next bullet), gives the directory a project identity with `meridian init <dir>`, and
  runs `meridian -C <dir> spawn -a inbox-router --dry-run`, which composes the launch and
  starts nothing. It prints the path the profile resolved to, saying when that is a
  package copy under the directory's `.mars/agents/`, and the harness and model, saying
  when the harness didn't come from the profile. Without an identity Meridian keeps the
  router's spawns as state for that path alone and prunes it once idle, losing spawns the
  routing log names; `init` keeps the identity a directory already has. The daemon counts
  the directory as set up when it holds a `meridian.toml`; without one the step fails,
  the log says to run setup, and the item waits.
- **Migrating an earlier setup.** Earlier setups made the directory a Mars project that
  installed the profile from `meridian-base`, and never ran `meridian init`; a directory
  got a `meridian.toml` only if a second step ever ran there. Once that package drops the
  profile a sync there fails, and while it has it the copy shadows Meridian's. When
  `mars.toml` is exactly what that setup wrote (one dependency, a source and `agents =
  ["inbox-router"]`), setup runs `meridian -C <dir> mars remove <key>` with the
  dependency's key. That leaves an empty `mars.toml`, a `mars.lock` and `.mars/`, which
  install nothing; setup creates no Mars project. Any other `mars.toml` is left alone,
  and setup says if it shadows the profile; an unreadable one stops setup. Until setup
  runs on the new build, a directory with no `meridian.toml` fails the step as above, and
  one with a `meridian.toml` launches the package copy with a warning to run setup. A
  `mars.toml` the user wrote is their choice and gets no warning.
- **The prompt** names the item, says whether the agent may ask, gives the sender, `via`
  and project, lists each kept session by spawn ID with its project, agent, work item,
  activity and the work item's goal, and ends with the text (first 4 KiB) between two
  marker lines carrying a random number, marked as someone else's words; a reply to the
  router's question follows the same way. A goal is read with `meridian -C <root> work
  show <work> --json`, once per project root and work item, four at a time, 20 seconds
  each; the root is the folder the listing gave a session on this machine. A session on
  another machine is listed without its goal. It
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
- **The decision** is the last non-empty line of the report: `offer <session>` for a
  session on the list, by the name the list gave it (`p41`, or `jimyao-minibox/p41` on
  a saved machine; this machine's name in front of a session here is accepted too), `none`, or `ask <question>`. Anything else is `none`: more words
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
refuses to start if either file can be read by anyone but the owner. It starts without the
router's key, and then routes nothing: filed messages wait, and `meridian-comms routes`
shows each as `off`. It starts with no `[sources.meridian]` too, on a machine without
Meridian: threads are read and posted, nobody is woken, and every message waits and shows
in `meridian-comms list`. A `[sources.meridian]` that is there is checked when the daemon
starts: a Meridian older than 0.28.1 (`meridian --version`), a project key that is not
shaped like a Meridian project ID, or a `meridian` that can't be asked, stops it with an
error that says which and what to do. 0.28.0 is the first Meridian with the machine
inbox, machine names and IDs, and a `kind` on every JSON error, and the source relies on
all three; 0.28.1 is the first that ships the `inbox-router` profile the second step
starts. One minimum covers both, so a daemon that passes the check can run every step it
is configured for. So a machine upgrades Meridian before it restarts a newer daemon
(the README gives the order). A `machine` key, which older builds read, is ignored with a warning.

```toml
owner = "jimmy"                  # the person whose agents this daemon wakes

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
[sources.meridian.projects.mellow-hollow-brood]   # a Meridian project ID
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

A project's key is a Meridian project ID, the one `meridian context` prints in the
project, and a channel's `project` is the same ID. One value then names the project in a
channel, a candidate, a ref and a filed item, so a message filed for a channel's project
is seen by that project's sessions. A project is its ID alone, not a folder: its agents
may run on any machine the bot reaches, in any checkout. A key no listing can check is
caught by the first listing in which every machine answered: each key with no agent
running anywhere is named in a warning, once per daemon. `root`, which older builds read,
is ignored with a warning to remove it, for one minor release.

**On another machine** the config is only where the bot is:

```toml
[bot]
ssh = "jimmy@raspberrypi"        # any target `ssh` takes; a known host key and a key that signs in without a prompt
# command = "meridian-comms"     # the program on the bot's machine, if not on its PATH
```

With `[bot]`, every client command reaches the daemon there through `bridge` (see
[Client and daemon](#client-and-daemon)), and nothing else may be in the file. The
daemon refuses to start with it: a `[bot]` machine runs no daemon.

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
so a redelivery is dropped. `authors` names the person each of the bot's posts made for
one (the form's) belongs to. A build older than the form drops `authors` the next time it
saves the file, and that post then reads as the bot's.

A `to:` line that names a word no answering source resolves, while a machine that could
have it is down, is neither text nor a miss yet. The daemon says once in the thread that
a machine did not answer and retries every 30 s (one loop per message, not per
delivery), for at most 5 minutes; then the word is a miss, and the answer names the
machines that didn't answer. If a later
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
envelopes, each acknowledged by `envelope_id` before it is handled, except the form's,
which are acknowledged with their answer (above). A `disconnect` frame or a silent link
(120 s) means reconnect.

**One bot, one daemon.** Slack hands each event to any one of an app's connections, so
a second daemon on the same bot would take some of the events. Each connection's `hello`
carries `num_connections`, and only the new connection learns the count: the one already
connected is never told. So the daemon checks its first `hello` after start, and exits
with status 2 when the count is above 1. It doesn't check on reconnect, because Slack's
own refresh briefly overlaps the old connection with the new one. After a crash, Slack
may count the dead connection for a while, so a restart can be refused until Slack
drops it; run the service with a restart delay. Web API calls: `chat.postMessage`,
`reactions.add`, `conversations.replies`, `conversations.history`, `auth.test`,
`conversations.info`, `views.open`, `views.publish`; a 429 waits out `Retry-After`. A post replaces `<!` with
`&lt;!` so text from a session can't ping a channel. A post over Slack's 40,000 characters,
which Slack would cut, is refused before it is sent, for `send` as for `deliver`. A
channel shared with another
organization is refused at start unless its entry says `external = true`.

## The Home tab

The bot's Home tab shows the owner's agents: the listing across machines (`online.rs`),
grouped by project (its label) and group, each candidate in the picker's words (short
ID, label, model, activity), what else runs in each group as a count, and a line saying
when it was listed, which machines answered (and why not), and whether a new listing
runs. Only the owner sees it: `src/core/view.rs` compares the Slack user on every open
and every click with `people.<owner>.slack`, and anyone else is shown "This shows the
owner's agents." Statuses and group names are the owner's agents' own words, and
opening the view to others means deciding what each may see.

- **Events:** Slack's `app_home_opened` with `tab` `home` (the Messages tab sends the
  same event, and is dropped), and the Refresh click, come in as events. A Message click
  must open a modal within Slack's 3 s, so it is an interaction of its own
  (`ViewMessage`), answered like the form's command: the core checks the owner and
  answers with the form, and the socket opens it with the click's `trigger_id` before
  acknowledging. An answer in words (someone not the owner) opens as a modal of its own,
  since a Home click has no channel. A click the core didn't answer by the socket's
  2.5 s is only logged: the half second left of the trigger's life is too little to open
  a modal.
- **Freshness:** opening shows the newest listing at once, with its age, and starts a
  new one when it is over 60 s old; Refresh always starts one, or joins the one running.
  Nothing polls. One task publishes the owner's view (`views.publish`), again whenever
  the listing changes or the owner opens it, always from the newest, so two publishes
  never land out of order.
- **Message** is a button on a candidate whose source marks it `wakeable` and whose
  project has a configured channel. Its value carries the session and the project's
  channels, and opens the `/agent` form with them; the form checks them again.
- **Layout:** every text is `plain_text`, so nothing a source wrote becomes a mention, a
  link or formatting. Slack shows at most 100 blocks: the view is cut at the end, with a
  line counting the groups not shown in full.

## Client and daemon

The client's commands are `read`, `send` and `list` for a session or a person, `deliver`
for `meridian message @…` (below), `route` for a source's inbox hook, and `routes`, which reads
the routing log itself. The client says who is calling and the daemon checks it. The claim is
made where the session runs, with no config (`source::claim`): a Meridian primary gives
its project ID (`MERIDIAN_PROJECT_ID`, else `meridian context --json` in
`MERIDIAN_PROJECT_DIR`), its spawn ID and depth, and its machine's ID
(`MERIDIAN_MACHINE_ID`, else `meridian machine name --json`, cached per `MERIDIAN_HOME`
in the state directory's `machine-ids.json`, written whole). Meridian sets both
variables in every session it launches from 0.29.0 on; with an older Meridian they are
unset and the two calls run. The daemon's source turns
it into a session (`Source::session`): a ref, how it is shown, and the person. A child
spawn may read but not post. A session whose project is not in the config is refused
with what to add and where; one in an ad-hoc project (an ID that names a folder on one
machine) is told to give the project an ID first, since no key can name it. The daemon
looks the claimed machine up in the newest listing's machines, if that listing is under 5
minutes old, and runs `machine list` (2.5 s on the Pi) only for an ID that isn't there,
so a machine removed from Meridian's saved list is still accepted for at most 5 minutes. A machine that is neither this one
nor saved is `unknown_machine`, and the client claims once more without its cache, in
case the machine's ID changed. A process with no Meridian variables is the owner at a terminal.
A claim from an older client, with no machine, is taken as this machine's. Requests go over a Unix socket (mode 0600, peer uid checked) as one JSON
object each way (`src/proto.rs`). On a machine whose config has `[bot]`, the same
request goes over SSH instead (`src/bridge.rs`): the client runs `ssh <options> --
<target> <command> bridge`, with Meridian's options for a call nobody watches (no
prompts, known host keys only, a dead connection noticed within a minute). `bridge`
connects to the daemon's socket and prints `meridian-comms-ready {"v":1}`, or
`meridian-comms-error {"text":…}` when it can't; lines a login prints before it are
skipped. Only after the ready line does the client write the request, on stdin, so no
text of it is ever on a command line, local or remote. Every failure before the ready
line is `unreachable`, and says nothing was sent and which: no `ssh`, a sandbox that
blocks it, an unknown or changed host key, a refused sign-in, a host that can't be
reached, no `meridian-comms` there (exit 127), a build there older than `bridge`, a
different handshake version, or the daemon down there. After it, a post with no whole
answer is `maybe_sent`, and anything else says running it again is safe. `bridge` ends
when the answer is out, when its stdin ends, and after 330 s, so a killed client never
leaves it holding a connection. `route` and `routes` belong to the bot's machine and are
refused on a `[bot]` one. A session reads the channels of its own project and
those with no project. A channel with no project needs `post = true` before sessions
may open threads there. `send` waits for the post and the wakes. A cold session the rule skips is reported and
is not a failure; the command exits non-zero, with a retry line naming only the refs
whose wake errored. A request runs to the end in its own task in the daemon whether or
not the client is still waiting (the client gives up after 300 s and says the daemon
may still finish; `deliver` gives up after 60 s). The socket is `/run/user/<uid>/meridian-comms.sock` where that
directory exists, whatever `XDG_RUNTIME_DIR` the caller has, so a session started
without it finds the same path as the daemon.

The daemon calls `meridian` with every `MERIDIAN_*` variable except `MERIDIAN_HOME`
removed. A wake is `message <address> <line> --wake --json`, the address being the bare
spawn ID here and `<saved name>/<spawn-id>` on a saved machine; a session here runs it
with `-C` at the folder its row gave, a session elsewhere with none. That is for a message notice (`--wake` because Meridian
holds a message to a cold session without it, and a message notice goes only to a session
the rule or a person chose), and the same without `--wake` for a release notice, which
nobody chose: Meridian refuses it for a cold session (kind `cold`, nothing sent), and
that session simply gets no more of the thread's messages. A wake reads the error's kind: `not_running` or
`not_found` is a session that is gone (on another machine, `not_found` may also be a
session from before machine-wide IDs, and the text says so), `maybe_delivered` a notice that may have arrived
(reported as not woken, so the message still waits), anything else a failed wake. For the machine it runs in the home
directory with no `-C`: `--version` and `machine name --json` (once each, at start), `spawn list --global --json`,
`message inbox`, `inbox read`, `inbox take`, `inbox offer`, and `spawn show` to turn an
item's sender (a chat ID) into the spawn ID a candidate carries. For saved machines it
adds `machine list --json` and `--machine <name>` on that listing and that `spawn show`. The second step adds `work show` in a
candidate's project and, in the router directory, `spawn`, `spawn wait`, `spawn show`
and `spawn cancel`. `setup router` runs `--version`, then in the router directory
`mars remove`, `init` and `spawn --dry-run`. Rows may carry `agent`, `work_id`,
`activity` and an `idle` object. `activity` of `turn_active`, `starting` or `finalizing`
is working; otherwise `idle` is idle with its cache deadline. `activity` of `idle` with
no `idle` object (a primary with idle notifications off) is idle with no stretch and no
cache deadline: shown as idle, never offered to, since its warmth is unknown. Neither
means activity is unknown, never cold. `activity` is read first because `idle` means "since the last
user-loop turn ended": a Claude primary running a turn that something injected (a wake,
a task notification) keeps its old `since`. The row has no `waiting_on_user`, and the
source never reports "waiting".

### Posting for a person

The `post` request (`src/proto.rs`, `post_to` in `src/core/api.rs`) posts text for a
person or in a thread and wakes nobody. It goes through `send`'s path, so there is one
way a post is made, and its caller must be allowed to post, as for `send`: a child spawn
is not.

- **For a person** in `[people]`, it opens a new thread that starts "From … to
  <mention>", the mention being the surface's (`<@U…>` on Slack) so the person is
  notified. The channel is the one whose `project` is the caller's; with none, the one
  channel with no project and `post = true`; for the owner at a terminal, the one channel
  they may open threads in. Several is `ambiguous`, none is `no_channel`.
- **In a thread** named by a message ref, as `send <ref>` does.

**A thread, not a DM.** The person's reply then reaches the session through what exists:
the session that posted joins its person's scope, and the rule delivers the reply. A DM
would need `im:write`, `im:history` and the `message.im` event (a reinstall), and its
reply would reach nothing, because the core reads configured channels only. The cost is
that the channel sees the message and the person has to be in it.

**Its errors say whether the post may have gone out.** The kind (`FailKind` in
`src/model.rs`) travels as a `Failure` inside the error, set where it is known, and the
daemon's reply carries it (`Reply.kind`). Every kind but `maybe_sent` says nothing was
posted. An error that names no kind is `not_sent`. `send` is split at the post:
`prepare_send` resolves everything and builds the post, the surface posts, and every
error of `finish_send` (the thread file, the wakes) becomes `maybe_sent` in one place.
A kind the client doesn't know, from a newer daemon, is read as `maybe_sent`. A request
that posts nothing (`read`, `list`, `route`) and gets no answer says that running it
again is safe.

| Kind | Where it comes from |
|---|---|
| `invalid` | A ref that is not `<channel>/<id>`, empty text. |
| `not_found` | No such person, channel or message (Slack's `thread_not_found` included). |
| `ambiguous`, `no_channel` | Choosing the channel, above. |
| `not_allowed` | A caller that may not post (a child spawn, an unlisted project), a channel of another project. |
| `too_large` | Over Slack's 40,000 characters, checked before posting; a request over the socket's 4 MiB, checked by the client. |
| `unreachable` | The client could not connect to the socket, or to the bot's machine before `bridge`'s ready line. |
| `unknown_machine` | The claim names a machine that is neither the bot's nor saved there; the client claims again once without its cache. |
| `not_sent` | Slack answered with an error, the request never had a connection, or the daemon refused before posting (another build reads `post` as a bad request). |
| `maybe_sent` | The Slack request went out with no answer, or with a 5xx, unreadable JSON, `internal_error`/`fatal_error` (Slack says these may follow a partial success), or no `ts`; anything failing after the post; a panic in the request; the socket dropping after the request was written; the client's time limit. |

The Web client (`src/surface/slack/web.rs`) tells the two apart at the call: its error
says `unanswered` unless the request never had a connection (connecting, TLS included,
is limited to 10 s so a stalled connect is a connect error) or Slack's answer refused
it, and the surface's `post` turns that into the kind. A read before the post keeps no
kind, so a failed read is `not_sent`.

### `deliver`, for Meridian

`meridian message @alice "…"` runs the program in `[message] people` of
`~/.meridian/config.toml` with the address as its last argument, the text on stdin and
the environment `meridian` started with, and reads one JSON line back within 120 s.
`meridian-comms deliver <address>` is that program, and all of it that knows Meridian's
contract is in `src/source/meridian/people.rs`: `@<person>` and `@<channel>/<id>` become
a `post`, waiting at most 60 s; the answer is `{"ok": true, "delivery": "acknowledged",
"note": <link>}` once Slack returned the post's `ts`, or `{"ok": false, "error": {"kind",
"text"}}` with exit 1, the kind mapped onto Meridian's closed list (`invalid` → `usage`,
`no_channel` → `no_route`, `not_allowed` → `refused`, `not_sent` → `failed`, `maybe_sent`
→ `maybe_delivered`, the rest by the same name). The wake notice still names
`meridian-comms send`: a notice naming `meridian message @…` would fail on a machine
without `[message] people`.

## Saved machines

A ref is `<machine ID>/<project>/<spawn-id>`: the ID, not the name, so no rename on any
machine changes a ref or orphans one stored in a thread. People see a ref as
`p41 on jimyao-pop-os` (`Source::show`) and type a candidate's `names`: the spawn ID,
`<machine name>/<spawn-id>` (the name it is saved under), and, for a session here, its
old name-based ref, `<this machine's name or a name in was>/<project>/<spawn-id>`, so a
picker click, a retry line or a `send --to` from an older build still resolves for one
minor release. `meridian machine name --json` is asked once when the daemon starts, for
the name, the machine's ID and the names it had before (`was`). When it can't answer,
the daemon does not start and says why. Only the source composes refs: the client
claims an ID, project and spawn ID, and the daemon checks them.

At each start the daemon rewrites the refs in its thread files (`scope`, `woken` and
`pending`) that start with this machine's name or a name in `was` to start with its ID,
so threads an older build wrote keep the sessions they chose. Every such ref was this
machine's: older builds stored no other. A ref that already starts with an ID is left
alone, so the rewrite runs once.

The Meridian source lists the sessions of this machine and of every
machine Meridian has saved (`meridian machine list`): one `spawn list --global --json
--machine M` each, all at once, beside the local listing. `machine list` and then each
machine have 12 seconds. A design review measured a Raspberry Pi (raspberry, Meridian
0.28.3) asking a desktop (homebox): about 4.9 s alone, 9.5 s beside five local listings.
One that is down, too old or slow contributes no sessions, and is named in the answer's
`machines` with why. Each listing logs at info who started and joined it, how long it
took, and how long `machine list` and each saved machine took and whether it answered.
The whole listing stays under the core's 30-second limit however many machines are
saved.

That answer carries more than candidates. Each candidate has `wakeable`: whether `wake`
from this daemon reaches it, which for Meridian is a running primary in a configured
project, on any machine it lists. `groups` has one entry per `(machine, project, group)` with
anything running, with how many of what runs there are not candidates (for Meridian,
spawns, which is why this listing drops `--primary`) and the project's label. The label
is the name of the folder most of its primaries run in, since Meridian's rows give each session's own folder, which can
be a worktree's, and no command gives a project's main checkout. A folder inside
`<name>.worktrees/` is named `<name>`. A primary that is starting or ending is counted
nowhere. The view adds the project ID to each of two projects whose labels are the
same. `machines` has each machine looked on, by ID and name, and whether it answered; a
`machine list` that fails is one entry with no name. A row's `desc` (a spawn's prompt
opening) is never read.

Two machines can each have a `p41`. Their refs differ, a bare `p41` is ambiguous and the
bot lists `homebox/p41` and `raspberry/p41`, the model's options are numbered, and the
second step's agent names a session on another machine as `<machine>/p41`, which is also
what `meridian inbox offer` and `meridian message` take. The offer always runs on this
machine, where the item is.

A saved machine has two names: the one it is saved under here and the one it calls
itself. They differ when Meridian could not take over a rename because the name is
taken. Names and addresses carry the saved name, because that is the one `--machine`
and `<machine>/p41` resolve; what a row says its machine is called is not read.

A sender is left out of the candidates wherever it runs. Its machine is told by
`from.machine_id`, which no rename changes. This machine's own ID: `spawn show <chat>`
here gives its spawn ID. A saved machine's: `spawn show <chat> --machine <saved name>`
does. Neither: the sender is on a machine that is not saved, which lists no candidates,
and its chat ID is not looked up, since here it would name some other session. An item
with no `machine_id` is read by its `machine` name the same way, own names first: a
name this machine has or had is here, a saved machine's name is there, any other name
is no candidate, and no name at all is looked up here.

Only Meridian's kind `not_found` means the sender is gone: the chat is not there, or no
machine is saved under the name. Meridian gives it at the top of the error here and
inside `error` through `--machine`. Any other kind, `failed` included, fails the routing
and the item waits: the sender's machine can't be reached (`unreachable`) or is too old
(`version`), its shell finds no `meridian` (`failed`), or `machine list` can't be read.
Read as "no sender", each would leave the sender among the candidates whenever the
listing, a moment later, reaches its machine, and it could be offered its own message; a waiting
item costs only a later `meridian-comms route`. The listing, by contrast, reads a failed
`machine list` as no saved machines, since a missing candidate offers nobody a wrong
message. Two cases still let a sender through:

- A live chat issued before Meridian's machine-wide IDs: `spawn show` answers `not_found`
  for it, so its session can be offered its own message. This fades as those
  sessions end.
- This machine's ID is read once at start. If `machine.json` is replaced while the
  daemon runs, items carry an ID that is neither this machine's nor a saved one, so
  every sender here is unknown until a restart. An unknown ID under this machine's name
  logs a warning to restart.

An offer that crosses machines can fail in more ways, and the log carries Meridian's
word for each. `maybe_delivered` means the notice may have arrived: the routing is
final and is not repeated. Every other word means nothing was sent and the item waits:
`unreachable`, and from a machine whose Meridian is too old for the inbox `refused`,
`usage` or `version`.

**Who may be woken is the bot's config.** A session on any machine is woken, picked and
named in `to:` when its project is configured here; the rules about who may post are
checked here too, for a client on any machine. A session in an unconfigured project shows
in the Home tab with no Message button, and the router may still offer it an item.

## Not built yet

The Amp source. The client already talks to the daemon in JSON.

A goal for a session on a saved machine, which needs `work show --machine` in Meridian.
