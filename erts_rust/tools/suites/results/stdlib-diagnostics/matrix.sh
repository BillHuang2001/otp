#!/bin/bash
# Ad-hoc matrix runner for the stdlib-diagnostics probes: run one Erlang
# expression under both runtimes and print a side-by-side line.
set -u
ROOT=${ROOT:?}
RT=$ROOT/erts_rust/target/release/beam_rt
P=$TMPDIR/stdlib-diag
cd "$P/cwd" || exit 1
while IFS= read -r e; do
  [ -z "$e" ] && continue
  o=$(erl -noshell -eval "io:format(\"~p\",[catch $e]), halt(0)." 2>&1 | tr '\n' ' ')
  r=$("$RT" -noshell -eval "io:format(\"~p\",[catch $e]), halt(0)." 2>&1 | tr '\n' ' ')
  printf '%-44s | sys=%-40s | rust=%s\n' "$e" "$o" "$r"
done
