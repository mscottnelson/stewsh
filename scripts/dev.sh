#!/usr/bin/env bash
# The rebuild loop behind `make dev` and `make watch`.
#
# One long-lived fswatch feeds a pipe, rather than a fresh `fswatch -1` per
# iteration. A save made while the build runs would otherwise be missed
# outright, leaving a stale binary serving with nothing to say so; queued
# batches are instead read on the next pass.
set -uo pipefail

# Every watched path is repo-relative, so anchor to the repo rather than to the
# caller: fswatch given a path that does not exist blocks forever in silence.
cd "$(dirname "$0")/.." || exit 1

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

cargo=${CARGO:-cargo}
# The binary is not always under ./target.
bin=${CARGO_TARGET_DIR:-target}/debug/stewsh
watched=(src tests Cargo.toml Cargo.lock)

for path in "${watched[@]}"; do
	[ -e "$path" ] || {
		echo "dev.sh: no $path here; expected the repo root" >&2
		exit 1
	}
done

child=
watcher=

stop() {
	[ -n "$child" ] || return 0
	kill "$child" 2>/dev/null
	wait "$child" 2>/dev/null
	child=
}

quit() {
	stop
	[ -n "$watcher" ] && kill "$watcher" 2>/dev/null
	exit 0
}
trap quit INT TERM

# -o collapses a multi-file save into one message. Excluding everything and
# re-including by extension keeps editor swap files and target/ out.
exec 3< <(fswatch -r -l 0.2 -o -E \
	-e '.*' -i '\.rs$' -i '\.html$' -i '\.js$' -i 'Cargo\.(toml|lock)$' \
	"${watched[@]}")
watcher=$!

while :; do
	printf '\n\033[2m── %s ─────────────────────────\033[0m\n' "$(date +%H:%M:%S)"
	if [ "$mode" = serve ]; then
		# No --locked here: a Cargo.toml edit is one of the changes being
		# watched for, and --locked refuses to update the lock file that edit
		# requires, which would fail this build and every one after it.
		if "$cargo" build; then
			if [ -x "$bin" ]; then
				"$bin" serve "$@" &
				child=$!
			else
				echo "dev.sh: built, but no executable at $bin" >&2
			fi
		else
			status=$?
			# A signalled build is ctrl-c on the way out, not a broken tree.
			[ "$status" = 130 ] || [ "$status" = 143 ] ||
				echo "dev.sh: build failed — waiting for a change" >&2
		fi
	else
		"$cargo" check --all-targets || true
	fi
	# A closed pipe means the watcher died, which is not the same as an idle
	# desk; unreported, the loop would wait forever looking exactly like one.
	if ! read -r _ <&3; then
		echo "dev.sh: watcher exited; stopping" >&2
		stop
		exit 1
	fi
	stop
done
