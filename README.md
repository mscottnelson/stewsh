# StewardShell (`stewsh`)

**A local, on-demand attention queue for the work you have in flight.**

StewardShell groups your terminal panes and agent sessions into **work streams**,
scores them on two clocks, and — when you ask it to — hands the whole picture to
an agent that says which stream is actually blocked on you. Selecting a stream
jumps your focus to its pane.

Nothing polls. Nothing runs in the background. You press a button, and the
queue re-thinks itself.

Read the [design and delivery plan](docs/design.md) for the reasoning and roadmap.

## Install

Requires stable Rust and a C compiler to build bundled SQLite:

```sh
cd ~/Code/stewsh
cargo install --path . --locked
stewsh agents      # read Claude Code and Codex transcripts
stewsh sync        # discover iTerm2 panes (macOS)
stewsh             # open the web view
```

`stewsh` with no arguments opens the local web view at `http://127.0.0.1:7777`.
With piped input or `--json` it prints the queue instead. `stewsh shell` still
gives the line-oriented prompt for scripting. This checkout is version **0.3.0**.
No login-shell change, background service, or cloud account is needed.

## The web view

The page is served from the binary on loopback. It is a keyboard-driven list.

| Key | Action |
| --- | --- |
| `j` / `k` | Move the cursor |
| `Enter` | Focus that stream's pane |
| `e` | Expand members |
| `r` | Rank on demand |
| `s` | Sync panes and agent sessions |
| `p` / `z` / `x` | Pin, snooze, resolve |

Each row shows the stream, its two scores, the git facts, and — after a ranking —
one sentence on why it is where it is and what to do next. Expanding a stream
lists its panes and agent sessions with per-member actions, including **Move…**
to regroup a member by hand.

## Two clocks

The old version had one score and an argument about whether recency was good.
Now there are two, and they answer different questions.

**Heat** is decayed activity: how live is this right now. It halves every 90
minutes and adds a baseline from the context's last activity, so it does not
depend on how often you sync. Deliberate acts weigh most (capturing a next
action, focusing a pane), agent state transitions less, and a changed terminal
screen least, because a spinner is not progress. Within a stream the hottest
member leads and each next one counts half as much, so the total converges and
breadth alone can never saturate the score: a sprawling directory grouping
cannot outrank the work actually in progress.

**Debt** is how unfinished something is, with no decay at all: a saved next
action, a waiting agent, a failure, work from before today that you never
reviewed. Plus repo evidence, which stands on its own — uncommitted changes,
unpushed commits, and failing pull request checks all count even when no pane is
open on that worktree.

`--mode active` (the default in the web view once you rank) leads with heat.
`--mode debt` is the morning view: what did I leave hanging. Same data.

| Reason | Points |
| --- | ---: |
| Pinned by you | +100 |
| Saved next action | +50 |
| Waiting / failed / ready | +45 / +35 / +25 |
| Unreviewed work from before today | +30 |
| Failing pull request checks | +40 |
| Never reviewed / changed since review | +10 / +20 |
| Uncommitted changes / unpushed commits | +10 / up to +20 |
| Reviewed, unchanged | -60 |
| Working | -10 |
| Recent activity (heat) | up to +150 |

## Work streams

A stream owns panes and agent sessions. Its key comes from repo context —
`repo#branch`, or the worktree path when you are in one — because a branch is the
work stream most of the time and costs nothing to compute. Every checkout of
every repo StewardShell has seen becomes a stream, including worktrees with no
pane open in them, so a dirty branch you forgot is still visible.

Grouping is automatic with a manual override that always wins:

```sh
stewsh group iterm:PANE-ID --to "Rate bump arc"
stewsh stream list
stewsh stream pin rate-bump-arc
stewsh stream rename rate-bump-arc --name "Rate bump, phase 2"
stewsh stream archive rate-bump-arc
```

Automatic regrouping runs on every sync and never undoes a manual choice.

## Ranking, on demand

```sh
stewsh rank --intent "shipping the rate-bump arc today"
stewsh rank --dry-run          # print the evidence, call nothing
stewsh rank --force            # re-rank even if nothing changed
```

Heat and debt are computed here and handed to the model as authoritative. The
model does the part arithmetic cannot: read the handoffs, the failing check and
the last error, and decide which stream is genuinely blocked on you rather than
running fine unattended. It returns an order, one sentence of reasoning, and a
suggested next action per stream. That answer is **stored**, so `focus 2` acts on
the ranking you are looking at and the queue does not move until you ask again.

**The ranker is a command, not an SDK.** StewardShell writes instructions plus an
evidence document to its stdin and reads JSON back:

```sh
export STEWSH_RANKER='claude -p --model haiku'   # the default
export STEWSH_RANKER='codex exec'
export STEWSH_RANKER='ollama run llama3.2'
export STEWSH_RANKER=~/bin/my-ranker.sh
```

No API key ever enters StewardShell. If the ranker is missing, times out, or
returns something that is not valid JSON, the deterministic order is kept, every
stream still gets an ordinal, and the failure is reported rather than swallowed.

Re-ranking is free when nothing changed. The evidence is fingerprinted over the
parts that represent a real change of situation — member states, revisions,
notes, handoffs, git facts — with time-varying values excluded, so pressing
`r` twice does not call a model twice.

## Watching agents without installing anything

`stewsh agents` reads the transcripts Claude Code and Codex already write, under
`~/.claude/projects` and `~/.codex/sessions`. It picks up the session title, the
working directory, the git branch, the last prompt or reply, and whether the turn
ended. No hook installation, and it sees sessions that started before
StewardShell existed.

```sh
stewsh agents --days 3     # default window
```

Sessions that fall out of the window are marked closed; their unresolved notes
stay recoverable under `--all`. Explicit reports still work and still win over
inferred state:

```sh
stewsh report waiting --session-id SESSION --agent codex \
  --summary 'Need a decision on the migration' --event-id turn-123
```

For iTerm2 panes, `stewsh sync` reads the live inventory and a conservative
signal from the visible screen. A changed screen counts as activity but no longer
counts as a new revision, so a spinner or a log tail cannot silently un-review a
pane you already looked at.

## Browser tabs

```sh
stewsh tabs
```

A tab joins a stream when it is **on the host that repository actually lives on**
and its path names the repository, so a pull request page lands next to the
branch it belongs to while a documentation site that merely mentions the name is
left alone. Branch names are matched as whole path segments, so `main` does not
match inside `maintenance`, and naming the branch pins the tab to that exact
stream.

Tabs are **members, not drivers**. They carry no activity signal, contribute
nothing to heat or debt, and can never put a stream in your queue. They exist so
that returning to a piece of work brings its context back. A tab that is gone is
marked closed rather than deleted, because you may have captured a note on it,
and a browser StewardShell cannot read is never treated as an empty one.

This uses the same macOS Automation permission iTerm2 integration already needs.
Enumerating arbitrary application windows (an editor, Slack) would need
Accessibility, which is a broader grant, so it is not built: see the roadmap.

## Focus

```sh
stewsh focus 2                          # the second row of the default queue
stewsh focus 2 --mode active            # the second row of `queue --mode active`
stewsh focus rate-bump                  # by name fragment
stewsh focus iterm:PANE-ID              # a specific pane
```

A bare number always means a row of the listing you just read, so pass the same
`--mode` you passed to `queue`. Names and IDs need no mode.

Focus selects the stream's primary iTerm2 pane without sending it any input, and
records the jump as activity, because choosing to go somewhere is the clearest
statement of intent there is. Streams whose only members are agent sessions with
no terminal report where the work lives instead.

## Agent-friendly contract

Every command takes `--json`:

```sh
stewsh queue --json --mode active
stewsh rank --json --dry-run
stewsh show SESSION --json
```

Success is `{"schema_version":1,"ok":true,"data":...}`; failure is
`{"schema_version":1,"ok":false,"error":{"message":"..."}}` with a nonzero exit.
Exit code 2 is invalid syntax, 1 is an operational error. Scoring reasons are
structured objects with `points`, a stable `code`, and human text, so an agent
does not parse prose. Timestamps are Unix seconds and IDs are complete.

`capture` and `report` resolve identity from `STEWSH_SESSION_ID`, then iTerm's
session environment, then `--session-id`. Those two commands match IDs
**exactly**: an agent reporting a fresh ID gets its own context rather than
silently adopting one that happens to share a prefix. Commands you type by hand
still accept unique prefixes and still refuse ambiguous ones.

The web view speaks the same contract over `GET /api/queue`, `POST /api/rank`,
`/api/sync`, `/api/focus`, `/api/context`, `/api/stream` and `/api/group`. It
serves loopback only and refuses any request whose `Host` is not its own
address, which is what closes DNS rebinding; the JSON content type already
blocks ordinary cross-site form posts.

## Passive zsh integration

```zsh
source ~/Code/stewsh/shell/stewsh.zsh
export STEWSH_RECORD_COMMANDS=1   # opt in to storing command text
```

Hooks capture command start and completion, exit status, CWD, and shell exit.
They are silent, preserve the command's exit status, and are safe to source
twice. Command arguments can contain secrets, so command text is off by default.
Installation never edits your shell configuration.

## Storage and privacy

SQLite at `~/.config/stewsh/stewsh.db`, overridable with `--db` or `STEWSH_DB`.
Existing 0.1 and 0.2 databases migrate transactionally to schema 3. Files are owner-only on
Unix and WAL supports concurrent local writers.

Stored: context and stream metadata, next actions, optional command text, agent
handoffs, screen fingerprints, git facts, triage decisions, stored rankings, the
last 200 events per context, and report deduplication IDs. Raw terminal screens
and agent transcripts are read but never written to the database.

Two things leave the machine, both off by default and both explicit:
`--pr` shells out to `gh` for pull request check status, and `rank` sends the
evidence document to whatever `STEWSH_RANKER` names. The evidence document
contains session titles, branch names, paths, notes and handoff text. Run
`stewsh rank --dry-run` to see exactly what would be sent. With a local model as
the ranker, nothing leaves at all.

To erase saved data, stop anything using the database and remove it along with
its `-wal` and `-shm` sidecars. There is no daemon to uninstall.

## Platforms, checks, and next steps

Core CLI, the web view, agent transcripts, ranking, streams, and git evidence work
on macOS and Linux. iTerm2 sync, focus, and preview require macOS with iTerm2
running and Automation access granted.

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
```

Tests cover migration, heat decay against undecayed debt, stream grouping with
manual override, ranker failure and caching, the evidence document, transcript
reading, exact-versus-prefix identity, the web endpoints over loopback, iTerm
snapshots, and the zsh hooks.

Not built, deliberately: an adapter for arbitrary application windows. It needs
macOS Accessibility, and when that permission is absent the API returns an empty
list rather than an error, so a silent no-op is the failure mode. That is the
worst kind to ship, and it needs a permission check that can tell "nothing open"
from "not allowed" before it is worth having.

Next: that window adapter, lifecycle hooks for harnesses that offer them, tmux
and fish adapters, and an MCP surface so an agent already in a session can read
the queue and report into it. MIT licensed.
