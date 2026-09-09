# StewardShell (`stewsh`)

A local-first terminal session secretary. Track active contexts, capture urgent
notes, and find what needs attention using priority and recency. No daemon,
network calls, AI service, or external database required.

## Install

Requires latest stable Rust and a C compiler to build bundled SQLite:

```sh
cargo install stewsh --locked
```

Published on [crates.io](https://crates.io/crates/stewsh). To install the latest
development version instead:

```sh
cargo install --git https://github.com/mscottnelson/stewsh --locked
```

## Use

```sh
export STEWSH_SESSION_ID="my-terminal"
stewsh track "$STEWSH_SESSION_ID"
stewsh track "$STEWSH_SESSION_ID" --command 'cargo test' --exit-code 1
stewsh capture "Investigate failing tests"
stewsh triage
stewsh next
```

`track` is silent on success. `triage --limit 5` limits output. `next` prints
one context without consuming it. `capture --session-id ID "note"` targets an
explicit session; otherwise capture uses `STEWSH_SESSION_ID`, then `TTY`.

For passive zsh integration, clone this repository and add the following to
`~/.zshrc` (replace the path with your checkout):

```zsh
source /path/to/stewsh/shell/stewsh.zsh
```

Hooks record commands at preexec, completion exit codes at precmd, and directory
changes at chpwd. Each shell process gets a distinct session ID. Hook failures
are silent; run `stewsh track "$STEWSH_SESSION_ID"` manually to diagnose them.
The hook stores command text, which can contain secrets; enable it only in
shells where you want that metadata recorded. No shell config is modified by installation.

## Ranking and storage

```text
Score = (Base_Priority * 10) / (Time_Elapsed_Minutes + 1)^1.2
```

Normal activity has base priority 1; a nonzero exit code raises it to 5;
manual notes have priority 10. Elapsed time uses `last_active_interaction`.
Future timestamps clamp to zero elapsed time. Ties use most recent activity,
then session ID. A successful completed command clears failure priority;
manual notes survive tracking. Capture replaces the session's previous note.

SQLite lives at `~/.config/stewsh/stewsh.db` on every supported platform,
including macOS. Use `--db PATH` for an isolated database. SQLite is bundled;
WAL mode and a 5ms lock timeout keep shell hooks responsive. Unix database
files are private to their owner. Both creation and last-interaction timestamps
are persisted. Only the latest command, exit code, and note per session are
stored; this is not a full terminal transcript or automatic AI-chat collector.
There is no completion/archive command yet; stale contexts decay naturally.

## Performance and distribution

The target is <15ms for local triage on typical session counts; cold startup,
storage, lock contention, and large databases can exceed it. Ranking currently
reads and sorts all sessions. Benchmark release builds on your own hardware.

On an Apple Silicon Mac with Rust 1.98.1 (2026-09-09), 100 measured warm
process launches after 10 warmups against 1,001 sessions produced:

| Command | Median | p95 | Maximum |
| --- | ---: | ---: | ---: |
| `triage` | 4.90ms | 5.88ms | 7.24ms |
| `next` | 4.77ms | 5.57ms | 6.02ms |
| `track` | 4.93ms | 5.91ms | 9.31ms |

These measurements include process startup with output redirected, and are
observations on one machine, not a universal latency guarantee.

The binary needs no separately installed SQLite or Rust runtime. Linux musl
builds are fully static, checked by CI. macOS uses system libraries and cannot
meet the same fully-static distribution model. The requested Cargo dependencies
are retained; the synchronous CLI does not start a Tokio runtime.

```sh
cargo build --release --locked
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo package --locked
```

## Publishing

Version `0.1.0` is published on crates.io. For a subsequent release, update the
package version and lockfile, run the checks above, and commit the changes.
Authenticate locally with `cargo login`, then:

```sh
cargo publish --locked --dry-run
cargo publish --locked
```

MIT licensed. Contributions via GitHub pull requests are welcome.
