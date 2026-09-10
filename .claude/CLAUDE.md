# stewsh (StewardShell)

@../README.md

The README above is the shared human and agent reference: what the tool does,
both clocks and their point values, the ranker contract, what leaves the
machine, the platform split, and the `make` targets. `docs/design.md` holds the
reasoning and the roadmap. This file adds only what a contributor needs and
those two have no reason to carry.

## Module map

| File | Owns |
| --- | --- |
| `main.rs` | clap surface, dispatch, text rendering, exit codes |
| `actions.rs` | the shared action layer both surfaces call |
| `serve.rs` | axum web view on loopback, `/api/*` |
| `model.rs` | `Context`, `Stream`, heat decay, debt scoring |
| `stream.rs` | grouping, regrouping, assembly, mode ordering |
| `store.rs` | schema, migrations, rows, events |
| `rank.rs` | evidence document, ranker subprocess, fingerprint cache |
| `repo.rs` | git facts, `gh` PR status |
| `harness.rs` | Claude Code / Codex transcript reading |
| `iterm.rs` | osascript bridge (`iterm-collect.js`, `iterm-action.js`) |
| `browser.rs` | osascript bridge (`browser-collect.js`, `browser-action.js`) |
| `src/web/` | `index.html` and vendored `alpine.js`, embedded into the binary |
| `shell/stewsh.zsh` | the passive zsh hooks; loaded at runtime by `tests/cli.rs` |

## Invariants

**Both surfaces go through `actions.rs`.** A CLI command and its web endpoint
must call the same function. Adding behavior in `main.rs` or `serve.rs` alone is
how the two drift, which is the thing that layer exists to prevent.

**Reason codes are stable API.** The `code` on a scoring reason is what an agent
switches on instead of parsing prose, so renaming one is a breaking change. The
README's points table is a hand-maintained mirror of `model.rs`: change a value
and edit both in the same commit.

**Time is injected, never read.** `model.rs`, `stream.rs` and `rank.rs` take
`now: i64`. Calling `Utc::now()` inside them makes scoring untestable, which is
why the decay tests can assert exact values.

**Migrations are append-only.** Add a new `Vn` const in `store.rs`, bump
`SCHEMA_VERSION`, apply transactionally. Never edit an existing `Vn`: every
database that already ran it has that `user_version`, so the block never re-runs
and the change silently reaches only fresh databases.

**Don't re-couple `revision` to the screen fingerprint.** The README says what
the behavior is; the warning is that the opposite shipped once, and it made
`review` least reliable on exactly the panes that mattered most.

**No build step for the web view.** `index.html` and the vendored `alpine.js`
are `include_str!`-ed into the binary. Keep it that way: no npm, no bundler.

**`Cargo.toml`'s `include` is hand-maintained.** A new top-level directory that
ships with the crate must be added there or `cargo package` drops it.

## Gates and platform

Beyond the targets the README lists, CI builds a static
`x86_64-unknown-linux-musl` release and asserts it has no `INTERP` segment: no
dependency may pull in dynamic C. SQLite is `bundled` for that reason.

A phantom test failure straight after a bare `cargo package` is the stale
binary the README describes, not a bug. Run `make clean` and re-run before
spending any time on it.

Don't `cfg`-gate a whole module to make a macOS-only feature build. The
osascript adapters compile on Linux and fail at runtime with a clear error,
which is the intended degradation.

## Working here

`tests/cli.rs` drives the real binary as a subprocess against a temp `--db`. Add
tests in that style rather than unit tests reaching into private state: the CLI
surface is the contract. Never let a test touch `~/.config/stewsh`.

Don't edit `scripts/dev.sh` while an instance of it is running. Bash re-reads a
script mid-execution, so the running loop starts executing the new bytes at its
old file offset.

Comments say why, not what. `README.md` and `docs/design.md` describe present
reality, not history: when behavior changes, edit the claim rather than
appending a note about the change.
