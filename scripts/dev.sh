#!/usr/bin/env bash
# The rebuild loop behind `make dev` and `make watch`.
#
# fswatch is the watcher: FSEvents costs nothing while idle and it is already
# a dependency of nothing else here. Cargo's incremental cache does the real
# work — a one-line edit relinks in about a second — so restarting the process
# beats every hot-reload scheme Rust currently offers for a binary this shape.
set -euo pipefail

mode=${1:-}
case $mode in
serve | check) shift ;;
*)
	echo "usage: dev.sh serve [stewsh args...] | dev.sh check" >&2
	exit 2
	;;
esac

command -v fswatch >/dev/null || {
	echo "dev.sh: fswatch is required — brew install fswatch" >&2
	exit 1
}

bin=target/debug/stewsh
child=
watcher=

# Block until something that affects the build changes. Excluding everything
# and re-including by extension keeps editor swap files and target/ out.
# The watcher is backgrounded and waited on rather than run in the foreground:
# bash defers a trap until the foreground child exits, so a loop signalled
# directly would leave the server orphaned on the port.
await_change() {
	fswatch -1 -r -l 0.2 -E \
		-e '.*' -i '\.rs$' -i '\.html$' -i '\.js$' -i 'Cargo\.(toml|lock)$' \
		src Cargo.toml Cargo.lock >/dev/null &
	watcher=$!
	wait "$watcher" 2>/dev/null || true
	watcher=
}

stop() {
	for pid in "$child" "$watcher"; do
		[ -n "$pid" ] || continue
		kill "$pid" 2>/dev/null || true
		wait "$pid" 2>/dev/null || true
	done
	child=
	watcher=
}

trap 'stop; exit 0' INT TERM

while :; do
	printf '\n\033[2m── %s ─────────────────────────\033[0m\n' "$(date +%H:%M:%S)"
	if [ "$mode" = serve ]; then
		# A failed build leaves no server up; the last one is already gone.
		if cargo build --locked; then
			"$bin" serve "$@" &
			child=$!
		else
			echo "dev.sh: build failed — waiting for a change" >&2
		fi
	else
		cargo check --locked --all-targets || true
	fi
	await_change
	stop
done
