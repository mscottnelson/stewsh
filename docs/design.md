# StewardShell: design and delivery plan

## Product promise

StewardShell (`stewsh`) answers one question on demand: **what deserves my
attention right now, why, and where is it?**

It does not poll, notify, or run in the background. The user presses a key and
the queue re-thinks itself. Between presses nothing moves, because a queue that
reshuffles while you read it is not a queue.

Existing shells keep executing commands. StewardShell manages attention and
information. A replacement POSIX shell, a terminal emulator, and an autonomous
agent orchestrator remain separate projects.

## What changed from 0.2, and why

0.2 ranked terminal **panes** on a single additive score. Three things were
wrong with that.

**The unit was wrong.** A pane is not a piece of work. One investigation spans a
worktree, two panes, an agent session and a pull request. Prioritizing panes
means prioritizing fragments.

**One score conflated two questions.** "What is live right now" and "what did I
leave unfinished" pull in opposite directions. 0.1 answered the first by decaying
old work into invisibility. 0.2 corrected that by removing decay entirely, and so
could not tell an active session from an eleven-hour-old one.

**Screen changes bumped the revision counter.** Any changed terminal screen —
a spinner, a clock, a log tail — cancelled a `review` and re-added its points, so
the review verb was least reliable on exactly the panes that mattered most.

## Information model

A **context** is a durable tracked session: an iTerm pane, a shell, or an agent
session. It carries availability, activity state and its source, a user note, an
agent handoff, pin, snooze, resolution, revision and reviewed revision, repo
facts, and a bounded audit event per meaningful mutation.

A **stream** owns contexts and is the unit the user prioritizes and focuses. Its
key is derived from repo context — `repo#branch`, or the worktree path — because
a branch is the work stream most of the time and costs nothing to compute. Every
known checkout becomes a stream, including worktrees with no pane open, so
unfinished work with zero terminal evidence stays visible. Manual grouping
always overrides the derived key, and regrouping never undoes it.

Terminal identity uses iTerm's native UUID; agent sessions use the harness's own
session ID; other shells use a shell-process ID. CWD and title are labels, never
identity. Commands typed by a human resolve unique ID prefixes. `capture` and
`report` match exactly, so an agent cannot adopt a context that merely shares a
prefix with the ID it invented.

## Two clocks

**Heat** is decayed activity with a 90-minute half-life. It sums weighted events
and adds a baseline decayed from the context's last activity, so it measures how
recently work was live rather than how often the user ran `sync`. Deliberate
user acts weigh most, agent transitions less, screen changes least.

**Debt** is unfinished-ness with no decay: notes, waiting agents, failures,
unreviewed work from before today, and repo evidence — dirty trees, unpushed
commits, failing checks — which counts whether or not a pane is open.

The active view leads with heat and adds debt. The debt view sorts on debt alone
and is the morning queue 0.2 was built for. Both are deterministic, pure, and
time-injectable. Scores clamp at zero. Resolved, snoozed and archived work is
excluded unless asked for.

## The ranker adjudicates; it does not score

Heat and debt are computed in Rust and handed to the model as authoritative
inputs. The model's job is the part arithmetic cannot do: read the handoff text,
the failing check and the last error, and decide which stream is genuinely
blocked on the human rather than running fine unattended. It returns an order,
one sentence of reasoning, and a suggested next action per stream.

Three properties make this safe to rely on.

**It is a command, not an SDK.** StewardShell writes instructions and an evidence
document to the ranker's stdin and reads JSON back, so Claude Code, Codex, a
local model or a shell script all satisfy the contract and no API key enters the
tool.

**Its answer is stored.** Ordinals persist, so selecting the second stream acts
on the ranking the user is looking at.

**It cannot break the queue.** A missing, slow or malformed ranker leaves the
deterministic order in place, still assigns every ordinal, and reports the
failure instead of swallowing it.

Churn is controlled by fingerprinting the evidence over the parts that represent
a real change of situation, with time-varying values excluded. An unchanged desk
re-ranks for free without calling a model.

## Observing agent harnesses

Three tiers, cheapest first.

1. **Transcripts.** Claude Code and Codex already write session files. Reading
   their tails yields the session title, working directory, git branch, last
   message and turn boundary with no installation, for sessions that started
   before StewardShell existed. This is the default.
2. **Explicit reports.** `report` with an optional event ID for idempotent
   retries. Agent-sourced state outranks anything inferred.
3. **Process and screen heuristics.** Executable names on a pane's tty and a
   conservative signal from the last visible lines. Labelled heuristic, and
   overridden by either tier above.

A screen fingerprint change is activity, not a revision. A quiet pane is not
assumed complete. Remote agents, renamed executables and nested processes may be
unidentified.

## Interfaces

One binary, one SQLite store, no daemon. The web view is served from the binary
on loopback and is the primary surface; the line-oriented prompt remains for
scripting; every command takes `--json` with a versioned envelope, structured
scoring reasons, complete IDs and clean stdout. The web view calls the same
action layer the CLI does, so the two surfaces cannot drift.

Two things leave the machine, both explicit and both off by default: `--pr`
shells out to `gh`, and `rank` sends the evidence document to the configured
ranker. `rank --dry-run` prints exactly what would be sent.

## Architecture

Modules in one crate: presentation (CLI and web), domain model and ranking,
SQLite storage and migration, repo evidence, harness adapters, the terminal
adapter, and the shared action layer. Ranking stays pure and time-injectable.
Migrations are transactional and versioned. Adapter failure preserves prior
observations rather than declaring everything closed.

## What is deliberately not built

One next action per context. No dependency graph. No auto-execution or approval.
No transcript summarizer. No automatic shell-config edits. No background polling,
and therefore no notifications. The product earns broader control by first being
dependable at remembering, grouping and prioritizing.

## Roadmap

**Next.** An editor and browser window adapter, so focusing a stream restores the
whole workspace rather than one pane. The split that keeps this honest: contexts
with activity signals generate attention, while windows and tabs only receive
focus, so a stale browser tab can never drive the queue.

**Then.** Lifecycle hooks for harnesses that offer them, promoting inference to
ground truth. tmux, Bash and fish adapters. An MCP surface so an agent already in
a session can read the queue and report into it.

**Later.** Ranking hysteresis if stored ordinals prove insufficient in daily use;
undo; lightweight task-manager export.

## How we decide whether it is good

The queue surfaces a useful next action in under a minute. Old unresolved work
survives a week without drowning today's. One decision takes one short command or
one keystroke. Inaccurate status is explainable and correctable. Pressing rank
twice with nothing changed costs nothing. Removing StewardShell leaves every
shell intact.

Automated tests cover migration without data loss, heat decay against undecayed
debt, stream grouping with manual override, ranker failure and caching, the
evidence document, transcript reading, exact-versus-prefix identity, closed-pane
handling, safe preview rendering, the web endpoints over loopback, and hook
behavior. Tests drive real CLI subprocesses, a scripted interactive session, and
a live server.
