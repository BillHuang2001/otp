#!/usr/bin/env sh
# Cargo wrapper for sandboxes where $HOME (and therefore cargo's default CARGO_HOME = $HOME/.cargo)
# is READ-ONLY. It selects the first writable candidate for CARGO_HOME and then execs cargo, so that
# plain `cargo build`/`cargo test` works in this environment without the caller having to remember the
# env var. Preferring a stable shared cache also avoids re-downloading crates per worktree.
#
# Usage:  scripts/cargo.sh build --workspace
#         scripts/cargo.sh test  --workspace
set -eu

if [ -z "${CARGO_HOME:-}" ]; then
  for cand in \
      "${HOME:-/nonexistent}/.cache/cargo-home" \
      "${TMPDIR:-/tmp}/cargo-home" \
      "/var/tmp/cargo-home" ; do
    parent=$(dirname "$cand")
    if [ -d "$cand" ] && [ -w "$cand" ]; then CARGO_HOME="$cand"; break; fi
    if [ -d "$parent" ] && [ -w "$parent" ]; then CARGO_HOME="$cand"; break; fi
  done
fi

if [ -n "${CARGO_HOME:-}" ]; then
  export CARGO_HOME
  mkdir -p "$CARGO_HOME" 2>/dev/null || true
fi

exec cargo "$@"
