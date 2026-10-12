#!/bin/sh
#
# gate.sh — the differential-test GATE for the Rust ERTS rewrite.
#
# Runs the built-in differential conformance corpus under BOTH runtimes (the system-ERTS
# oracle and the Rust `beam_rt`) via `differential suite --strict --allowlist known-gaps.txt`,
# then turns the run's machine-readable report into a CI-friendly exit code. It is a pure
# driver: it never modifies the runtime, the corpus or the report schema.
#
# ---------------------------------------------------------------------------
# EXIT-CODE CONTRACT (this script; the binaries' own codes are 0/1/2)
# ---------------------------------------------------------------------------
#   0  every executed case agreed, OR every non-agreeing case is a capability gap recorded
#      in the allowlist AND the allowlist is fresh (no unauthorized gap, no stale entry)
#   1  at least one MISMATCH — a real, observable behavioral divergence between the oracle
#      and the Rust runtime (including oracle-vs-its-own-golden corpus drift).
#   2  usage / environment error — a missing oracle, a build failure, a missing allowlist,
#      a report that could not be produced, or a report whose top-level `summary`/`ratchet`
#      block cannot be parsed (an unexpected report shape) — never a false pass.
#   3  NO mismatch, but the RATCHET must be updated: >=1 `unsupported`/`skipped` case is NOT
#      in the allowlist (an unauthorized gap appeared — record it deliberately), or >=1
#      allowlist entry did not reproduce as a gap (a stale entry — the gap closed, drop it).
#
# INVARIANT (the whole point of the split): an `unsupported` case — a capability gap the
# Rust runtime signals honestly (its documented exit 3 / `beam_rt: unsupported: …` reason
# convention, or a missing `beam_rt`) — is NEVER reported as a MISMATCH.
#   * exit 1 is reserved for a case whose runtime actually produced a *differing
#     observable result*;
#   * a capability gap can only ever land in the `unsupported`/`skipped` buckets, is
#     classified as a gap, and therefore can only ever cause exit 3 (when it is not
#     allowlisted) or exit 0 (when it is).
# The mismatch-vs-ratchet distinction is read SOLELY from the report's **top-level
# `summary`** and **top-level `ratchet`** objects, which are authoritative. The per-contract
# `by_contract` rollup also carries `mismatch` keys and is deliberately ignored. If the
# summary cannot be parsed the gate FAILS LOUD (exit 1); if the ratchet block cannot be
# parsed (an unexpected report shape) it FAILS LOUD (exit 2) — never a false pass (exit 0).
#
# ---------------------------------------------------------------------------
# WHAT IT RUNS
# ---------------------------------------------------------------------------
#   1. `scripts/cargo.sh build --manifest-path <erts_rust>/Cargo.toml -p beam_rt -p beam_dist -p differential`
#   2. `scripts/cargo.sh run   --manifest-path <erts_rust>/Cargo.toml -q -p differential -- \
#          suite --strict --allowlist <script_dir>/known-gaps.txt --json --out <tmpdir>`
# The corpus JSON report is written to `<tmpdir>/report.json`; the same JSON that the
# binary prints on stdout is captured to `<tmpdir>/stdout.json` (not echoed) so the gate's
# own output stays a concise human summary.
#
# Runtime/env resolution is inherited unchanged from the binary:
#   $ERTS_ORACLE (else `erl` on PATH)   $BEAM_RT (else the workspace target/{release,debug})
#   $ERLC        (else `erlc` on PATH).
# No worktree or absolute paths are baked in: <erts_rust> and the allowlist are derived from
# this script's own location, so the script is scope-safe and copy-safe across worktrees.
#
set -eu

# --- locate the erts_rust workspace + the allowlist relative to this script ------------
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
erts_rust=$(CDPATH= cd -- "$script_dir/../.." && pwd)
manifest="$erts_rust/Cargo.toml"
cargo_sh="$erts_rust/scripts/cargo.sh"
allowlist="$script_dir/known-gaps.txt"

if [ ! -f "$manifest" ]; then
  echo "gate: cannot find the workspace manifest at '$manifest'" >&2
  exit 2
fi
if [ ! -x "$cargo_sh" ]; then
  echo "gate: cannot find the cargo wrapper at '$cargo_sh'" >&2
  exit 2
fi
if [ ! -f "$allowlist" ]; then
  echo "gate: cannot find the known-gaps allowlist at '$allowlist'" >&2
  exit 2
fi

# --- build the runtime + harness --------------------------------------------------------
# Build `beam_rt` (the runtime under test), `beam_dist` (whose `dist_probe` binary is the
# Rust side of the live distribution loopback cases) and `differential` (the harness). A
# build failure is an environment error (exit 2) — the gate never pretends to have run.
if ! "$cargo_sh" build --manifest-path "$manifest" -p beam_rt -p beam_dist -p differential; then
  echo "gate: build failed — cannot run the corpus" >&2
  exit 2
fi

# --- run the corpus as the gate ---------------------------------------------------------
out=$(mktemp -d "${TMPDIR:-/tmp}/differential-gate.XXXXXX")
trap 'rm -rf "$out"' EXIT HUP INT TERM

# The binary's own exit code is captured, never propagated raw: with an allowlist it is `1`
# for a mismatch, an unauthorized gap or a stale entry, so it does NOT by itself distinguish
# 1 from 3 — the report's summary + ratchet do. stdout (the big JSON) is captured; stderr
# passes through so a loud binary error remains visible.
run_rc=0
"$cargo_sh" run --manifest-path "$manifest" -q -p differential -- \
    suite --strict --allowlist "$allowlist" --json --out "$out" >"$out/stdout.json" || run_rc=$?

case "$run_rc" in
  0 | 1) : ;; # the binary's documented `suite --strict --allowlist` exit codes
  2)
    echo "gate: differential reported a usage/environment error (exit 2)" >&2
    exit 2
    ;;
  *)
    echo "gate: differential exited with unexpected status $run_rc (build or run failure)" >&2
    exit 2
    ;;
esac

report="$out/report.json"
if [ ! -f "$report" ]; then
  echo "gate: no report was written at '$report' (differential exit $run_rc)" >&2
  exit 2
fi

# --- parse the TOP-LEVEL `summary` (authoritative; `by_contract` is ignored) ------------
# The report is serde-pretty JSON: the top-level `summary` object is the only key at a
# two-space indent named `summary`, and its five flat integer fields sit at a four-space
# indent. Anchoring on those indents keeps the parse from ever touching the per-contract
# `by_contract` rollup (which repeats `mismatch` etc. at a deeper indent).
summary=$(
  awk '
    /^  "summary"[[:space:]]*:[[:space:]]*\{/ { in_summary = 1; next }
    in_summary && /^  \}/ { in_summary = 0; next }
    in_summary && /^    "[a-z_]+"[[:space:]]*:[[:space:]]*[0-9]+,?[[:space:]]*$/ {
      line = $0
      sub(/^[[:space:]]*"/, "", line)          # drop the leading indent + open quote
      key = line; sub(/".*/, "", key)          # key = up to the closing quote
      val = line; sub(/^[^:]*:[[:space:]]*/, "", val); sub(/,.*/, "", val)
      print key "=" val
    }
  ' "$report"
)

# --- parse the TOP-LEVEL `ratchet` block (same indented-block technique) ----------------
# `allowed`/`unauthorized`/`stale` are the four-space integer fields; the `allowlist` string
# field cannot match the integer-only pattern, so it is (correctly) never read.
ratchet=$(
  awk '
    /^  "ratchet"[[:space:]]*:[[:space:]]*\{/ { in_ratchet = 1; next }
    in_ratchet && /^  \}/ { in_ratchet = 0; next }
    in_ratchet && /^    "[a-z_]+"[[:space:]]*:[[:space:]]*[0-9]+,?[[:space:]]*$/ {
      line = $0
      sub(/^[[:space:]]*"/, "", line)
      key = line; sub(/".*/, "", key)
      val = line; sub(/^[^:]*:[[:space:]]*/, "", val); sub(/,.*/, "", val)
      print key "=" val
    }
  ' "$report"
)

field() { printf '%s\n' "$summary" | sed -n "s/^$1=//p" | head -n 1; }
rfield() { printf '%s\n' "$ratchet" | sed -n "s/^$1=//p" | head -n 1; }
is_uint() { printf '%s' "$1" | grep -Eq '^[0-9]+$'; }

total=$(field total)
agree=$(field agree)
mismatch=$(field mismatch)
unsupported=$(field unsupported)
skipped=$(field skipped)

# A summary we cannot read is a hard failure: fail LOUD rather than risk a false pass.
if ! is_uint "$total" || ! is_uint "$agree" || ! is_uint "$mismatch" \
  || ! is_uint "$unsupported" || ! is_uint "$skipped"; then
  echo "gate: could not parse the top-level summary from '$report' — failing loud (exit 1)" >&2
  exit 1
fi

allowed=$(rfield allowed)
unauthorized=$(rfield unauthorized)
stale=$(rfield stale)

# The ratchet block is expected on every run (the allowlist is always passed): a missing or
# unparsable block means an unexpected report shape, so fail LOUD (exit 2) — never a pass.
if ! is_uint "$allowed" || ! is_uint "$unauthorized" || ! is_uint "$stale"; then
  echo "gate: could not parse the top-level ratchet block from '$report' — unexpected report shape (exit 2)" >&2
  exit 2
fi

# --- non-agreeing cases (for a readable summary) ----------------------------------------
# Case fields sit at a six-space indent; anchoring to it means a `"case_id"`/`"verdict"`
# occurrence inside a string value can never be mistaken for the key.
failing=$(
  awk '
    /^      "case_id"[[:space:]]*:/ {
      line = $0; sub(/.*"case_id"[[:space:]]*:[[:space:]]*"/, "", line); sub(/".*/, "", line)
      case_id = line; next
    }
    /^      "verdict"[[:space:]]*:/ {
      line = $0; sub(/.*"verdict"[[:space:]]*:[[:space:]]*"/, "", line); sub(/".*/, "", line)
      if (line != "agree") print line " " case_id
      case_id = ""
    }
  ' "$report"
)

# --- classify every GAP id against the allowlist ----------------------------------------
# A *gap* is a non-agreeing case whose verdict is `unsupported`/`skipped`. A `mismatch` is
# NEVER a gap — it is a hard failure handled by the `mismatch` branch above — so it is
# deliberately excluded here: the allowlist tolerates a capability gap, never a mismatch, and
# this summary can therefore never imply that a mismatch was "allowed".
# (for a readable summary: which gaps are unauthorized, which entries are stale)
printf '%s\n' "$failing" | awk '$1 == "unsupported" || $1 == "skipped" { print $2 }' > "$out/gap-ids.txt"
awk '!/^[[:space:]]*#/ && NF > 0 { print $1 }' "$allowlist" > "$out/allow-ids.txt"
unauthorized_ids=$(awk -v ids="$(cat "$out/allow-ids.txt")" '
  BEGIN { n = split(ids, a, "\n"); for (i = 1; i <= n; i++) if (a[i] != "") ok[a[i]] = 1 }
  { if (!($0 in ok)) print }
' "$out/gap-ids.txt")
stale_ids=$(awk -v ids="$(cat "$out/gap-ids.txt")" '
  BEGIN { n = split(ids, a, "\n"); for (i = 1; i <= n; i++) if (a[i] != "") ok[a[i]] = 1 }
  { if (!($0 in ok)) print }
' "$out/allow-ids.txt")

# --- human summary ----------------------------------------------------------------------
echo "differential gate: total=$total agree=$agree mismatch=$mismatch unsupported=$unsupported skipped=$skipped allowed=$allowed unauthorized=$unauthorized stale=$stale (differential exit $run_rc)"
if [ -n "$failing" ]; then
  echo "non-agreeing cases:"
  printf '%s\n' "$failing" | sed 's/^/  /'
fi
if [ -n "$unauthorized_ids" ]; then
  echo "unauthorized gaps (record them in the allowlist deliberately):"
  printf '%s\n' "$unauthorized_ids" | sed 's/^/  /'
fi
if [ -n "$stale_ids" ]; then
  echo "stale allowlist entries (the gap closed — drop them):"
  printf '%s\n' "$stale_ids" | sed 's/^/  /'
fi

# --- verdict ----------------------------------------------------------------------------
if [ "$mismatch" -gt 0 ]; then
  echo "gate: FAIL — $mismatch mismatch(es): a real behavioral divergence (exit 1)" >&2
  exit 1
fi
if [ "$unauthorized" -gt 0 ] || [ "$stale" -gt 0 ]; then
  echo "gate: FAIL — no mismatch, but $unauthorized unauthorized gap(s) + $stale stale allowlist entry/entries: the ratchet must be updated (exit 3)" >&2
  exit 3
fi
echo "gate: PASS — every case agreed or is an allowlisted capability gap, and the allowlist is fresh (exit 0)"
exit 0
