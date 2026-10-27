#!/bin/sh
#
# gate.sh — the differential-test GATE for the Rust ERTS rewrite.
#
# Runs the built-in differential conformance corpus under BOTH runtimes (the system-ERTS
# oracle and the Rust `beam_rt`) via
# `differential suite --strict --allowlist <gaps> --drift-allowlist <drift>`, then turns the
# run's machine-readable report into a CI-friendly exit code. The two allowlists default to
# the committed `known-gaps.txt` and `known-drift.txt` next to this script and are each
# overridable (`--allowlist <file>`, `--drift-allowlist <file>`). With `--report <file>` the
# build and the corpus run are skipped entirely and an existing report is judged instead
# (report-only mode). It is a pure driver: it never modifies the runtime, the corpus or the
# report schema.
#
# ---------------------------------------------------------------------------
# EXIT-CODE CONTRACT (this script; the binaries' own codes are 0/1/2)
# ---------------------------------------------------------------------------
#   0  every executed case agreed, OR every non-agreeing case is a capability gap recorded
#      in the gaps allowlist, OR is an ORACLE DRIFT recorded in the drift allowlist — and
#      BOTH allowlists are fresh (no unauthorized entry and no stale entry in either). Read
#      "recorded"/"fresh" as the STRICTER of the report's own ratchet counters and a live
#      re-classification against the allowlists passed to this script (see below).
#   1  at least one MISMATCH — a real, observable behavioral divergence: our side failed the
#      committed golden, or the two sides' observables differed without our side proving the
#      golden is still reproducible. ONLY a mismatch reaches 1: a capability gap and a
#      recorded oracle drift are never exit 1. A report whose top-level `summary` cannot be
#      parsed also fails LOUD here (never a false pass).
#   2  usage / environment error — a missing oracle, a build failure, a missing allowlist or
#      drift allowlist, a report that could not be produced / does not exist, or a top-level
#      `ratchet` block that is present but cannot be parsed (an unexpected report shape)
#   3  NO mismatch, but a RATCHET must be updated: >=1 gap is NOT in the gaps allowlist (an
#      unauthorized gap appeared — record it deliberately), or >=1 gaps-allowlist entry did
#      not reproduce as a gap (a stale entry — the gap closed, drop it), or the same for the
#      DRIFT ratchet (an unauthorized oracle drift, or a stale drift entry).
#
# ---------------------------------------------------------------------------
# THE VERDICT CLASSES AND WHERE EACH ONE LANDS
# ---------------------------------------------------------------------------
#   agree            both sides reproduced the golden                       -> exit 0
#   unsupported /    a capability gap: the Rust side could not be observed   -> gaps ratchet
#   skipped          (its documented exit-3 / `unsupported: …` convention)      (known-gaps.txt)
#                    or the case was intentionally not executed
#   oracle_drift     OUR side reproduced the committed golden and the HOST   -> drift ratchet
#                    ORACLE did not; the two sides' observables differ, so      (known-drift.txt)
#                    this is a host-oracle RELEASE difference, never a
#                    divergence of our runtime
#   mismatch         our side diverged from the golden, or the observables   -> exit 1
#                    differed without our side proving the golden            (never tolerated)
#
# INVARIANT (the whole point of the split): a capability gap and an oracle drift are NEVER
# reported as a MISMATCH.
#   * exit 1 is reserved for a case whose runtime actually produced a *differing observable
#     result* that our side could not show to be the wrong side of the comparison;
#   * a capability gap can only ever land in the `unsupported`/`skipped` buckets and an
#     oracle drift only in `oracle_drift`, so either can only ever cause exit 3 (when it is
#     not recorded) or exit 0 (when it is);
#   * the two ratchets are strictly separate: an oracle drift is never tolerated by the gaps
#     allowlist and a gap is never tolerated by the drift allowlist.
# The mismatch-vs-ratchet distinction is read from the report's **top-level `summary`**
# object (authoritative for `total`/`agree`/`mismatch`/`unsupported`/`skipped`/`oracle_drift`)
# and from a top-level `ratchet` object that is expected to be present. The per-contract
# `by_contract` rollup also carries `mismatch` keys and is deliberately ignored. If the
# summary cannot be parsed the gate FAILS LOUD (exit 1); if the ratchet block cannot be
# parsed (an unexpected report shape) it FAILS LOUD (exit 2) — never a false pass (exit 0).
#
# ---------------------------------------------------------------------------
# HOW THE RATCHET VERDICT IS REACHED (the report is never trusted alone)
# ---------------------------------------------------------------------------
# The report's top-level `ratchet` counters are that run's OWN classification, produced by
# whatever allowlists drove it. In `--report <file>` mode the report can come from a
# DIFFERENT sweep with DIFFERENT allowlists (an earlier run, another worktree, a wider or
# narrower ratchet), so its self-classification must never be trusted alone: judging only
# from it lets a report say "every gap/drift is allowed" while the allowlists passed to
# THIS invocation do not contain those ids — a false pass. The gate therefore judges with
# the STRICTER of two views, as a per-counter maximum:
#   (i)  the report's own top-level ratchet counters
#        (`allowed`/`unauthorized`/`stale` and `drift_allowed`/`drift_unauthorized`/
#        `drift_stale`);
#   (ii) a LIVE re-classification of the report's CASE VERDICTS against the allowlist files
#        THIS invocation loaded: an id whose verdict is `unsupported`/`skipped` is a gap and
#        one whose verdict is `oracle_drift` is a drift; it is `allowed` when its id appears
#        in the corresponding allowlist and `unauthorized` otherwise, and an allowlist entry
#        whose id did not reproduce as a gap (resp. a drift) is `stale`.
# A disagreement between the two views is printed as one loud `gate: note …` line together
# with both views and the counters actually used, and `max()` per counter means it can only
# ever push the gate to exit 3 (the ratchet must be updated) — never to exit 0. The normal
# (build + run) path produces the report with the very allowlists this script passes, so
# there the two views agree and the maximum is a no-op.
# `mismatch` is treated the same way (the larger of the report's `summary.mismatch` and the
# live count of `mismatch` verdicts) but stays FIRST and absolute: a mismatch is never
# tolerated by either allowlist and wins over any ratchet state.
# A report whose `ratchet` block carries no drift classification (produced without a drift
# allowlist, e.g. an older report handed to `--report`) is simply a special case of the live
# view: its own drift counters are read the documented way (allowed=0,
# unauthorized=<all drifts>, stale=0) and a note line says so, while the live
# re-classification is applied on top and the per-counter maximum keeps the old strictness —
# a report that cannot vouch for its own drifts can never pass.
#
# ---------------------------------------------------------------------------
# WHAT IT RUNS (default mode; `--report <file>` skips both steps)
# ---------------------------------------------------------------------------
#   1. `scripts/cargo.sh build --manifest-path <erts_rust>/Cargo.toml -p beam_rt -p beam_dist -p differential`
#   2. `scripts/cargo.sh run   --manifest-path <erts_rust>/Cargo.toml -q -p differential -- \
#          suite --strict --allowlist <gaps> --drift-allowlist <drift> --json --out <tmpdir>`
#      (<gaps> is `--allowlist <file>` when given, else `<script_dir>/known-gaps.txt`;
#       <drift> is `--drift-allowlist <file>` when given, else `<script_dir>/known-drift.txt`.)
# The corpus JSON report is written to `<tmpdir>/report.json`; the same JSON that the
# binary prints on stdout is captured to `<tmpdir>/stdout.json` (not echoed) so the gate's
# own output stays a concise human summary.
#
# Runtime/env resolution is inherited unchanged from the binary:
#   $ERTS_ORACLE (else `erl` on PATH)   $BEAM_RT (else the workspace target/{release,debug})
#   $ERLC        (else `erlc` on PATH).
# No worktree or absolute paths are baked in: <erts_rust> and both allowlists are derived
# from this script's own location, so the script is scope-safe and copy-safe across
# worktrees, and it never writes into the repository.
#
set -eu

# --- locate the erts_rust workspace + the script itself relative to this script --------
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
erts_rust=$(CDPATH= cd -- "$script_dir/../.." && pwd)
manifest="$erts_rust/Cargo.toml"
cargo_sh="$erts_rust/scripts/cargo.sh"

# --- arguments --------------------------------------------------------------------------
# `--allowlist <file>` overrides the committed `known-gaps.txt` next to this script (the
# default) and `--drift-allowlist <file>` the committed `known-drift.txt`. `--report <file>`
# selects report-only mode (judge an existing report; no build, no corpus run). `--help`/`-h`
# prints the usage and succeeds; any other argument is a usage error. All of this is handled
# before anything is built, so an explicit-but-missing file fails fast.
allowlist=""
drift_allowlist=""
report_file=""
while [ $# -gt 0 ]; do
  case "$1" in
    --allowlist)
      if [ $# -lt 2 ]; then
        echo "gate: '--allowlist' requires a file argument" >&2
        exit 2
      fi
      allowlist=$2
      shift 2
      ;;
    --allowlist=*)
      allowlist=${1#--allowlist=}
      shift
      ;;
    --drift-allowlist)
      if [ $# -lt 2 ]; then
        echo "gate: '--drift-allowlist' requires a file argument" >&2
        exit 2
      fi
      drift_allowlist=$2
      shift 2
      ;;
    --drift-allowlist=*)
      drift_allowlist=${1#--drift-allowlist=}
      shift
      ;;
    --report)
      if [ $# -lt 2 ]; then
        echo "gate: '--report' requires a file argument" >&2
        exit 2
      fi
      report_file=$2
      shift 2
      ;;
    --report=*)
      report_file=${1#--report=}
      shift
      ;;
    -h | --help)
      echo "usage: gate.sh [--allowlist <file>] [--drift-allowlist <file>] [--report <file>]"
      echo "  --allowlist <file>        known-gaps ratchet allowlist (default: <script_dir>/known-gaps.txt)"
      echo "  --drift-allowlist <file>  known host-oracle drift ratchet allowlist"
      echo "                            (default: <script_dir>/known-drift.txt)"
      echo "  --report <file>           report-only mode: judge an existing differential/1 report"
      echo "                            instead of building and running the corpus (exercises the"
      echo "                            exit-code mapping without the serial sweep)"
      echo "  -h, --help                print this usage and exit 0"
      exit 0
      ;;
    *)
      echo "gate: unknown argument '$1' (try --help)" >&2
      exit 2
      ;;
  esac
done
if [ -z "$allowlist" ]; then
  allowlist="$script_dir/known-gaps.txt"
fi
if [ -z "$drift_allowlist" ]; then
  drift_allowlist="$script_dir/known-drift.txt"
fi

if [ ! -f "$allowlist" ]; then
  echo "gate: cannot find the known-gaps allowlist at '$allowlist'" >&2
  exit 2
fi
if [ ! -f "$drift_allowlist" ]; then
  echo "gate: cannot find the known-drift allowlist at '$drift_allowlist'" >&2
  exit 2
fi

# --- a scratch dir for the report + the derived id files (never inside the repo) --------
out=$(mktemp -d "${TMPDIR:-/tmp}/differential-gate.XXXXXX")
trap 'rm -rf "$out"' EXIT HUP INT TERM

if [ -n "$report_file" ]; then
  # --- report-only mode ---------------------------------------------------------------
  # Judge an existing `differential/1` report: no build, no corpus run, so the exit-code
  # mapping can be exercised on a report already on disk. A missing/unreadable report is an
  # environment error (exit 2) — the gate never pretends to have judged something.
  report=$report_file
  if [ ! -f "$report" ] || [ ! -r "$report" ]; then
    echo "gate: cannot read the report to judge at '$report'" >&2
    exit 2
  fi
  run_note="report $report"
else
  # --- build the runtime + harness ------------------------------------------------------
  # Build `beam_rt` (the runtime under test), `beam_dist` (whose `dist_probe` binary is the
  # Rust side of the live distribution loopback cases) and `differential` (the harness). A
  # build failure is an environment error (exit 2) — the gate never pretends to have run.
  if [ ! -f "$manifest" ]; then
    echo "gate: cannot find the workspace manifest at '$manifest'" >&2
    exit 2
  fi
  if [ ! -x "$cargo_sh" ]; then
    echo "gate: cannot find the cargo wrapper at '$cargo_sh'" >&2
    exit 2
  fi
  if ! "$cargo_sh" build --manifest-path "$manifest" -p beam_rt -p beam_dist -p differential; then
    echo "gate: build failed — cannot run the corpus" >&2
    exit 2
  fi

  # --- run the corpus as the gate -------------------------------------------------------
  # The binary's own exit code is captured, never propagated raw: with the two allowlists it
  # is `1` for a mismatch, an unauthorized gap, a stale gap entry, an unauthorized drift or a
  # stale drift entry, so it does NOT by itself distinguish 1 from 3 — the report's summary +
  # ratchet do. stdout (the big JSON) is captured; stderr passes through so a loud binary
  # error remains visible.
  run_rc=0
  "$cargo_sh" run --manifest-path "$manifest" -q -p differential -- \
      suite --strict --allowlist "$allowlist" --drift-allowlist "$drift_allowlist" \
      --json --out "$out" >"$out/stdout.json" || run_rc=$?

  case "$run_rc" in
    0 | 1) : ;; # the binary's documented `suite --strict --allowlist/--drift-allowlist` codes
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
  run_note="differential exit $run_rc"
fi

# --- parse the TOP-LEVEL `summary` (authoritative; `by_contract` is ignored) ------------
# The report is serde-pretty JSON: the top-level `summary` object is the only key at a
# two-space indent named `summary`, and its six flat integer fields sit at a four-space
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
# `allowed`/`unauthorized`/`stale` are the gaps fields; `drift_allowed`/`drift_unauthorized`/
# `drift_stale` are the drift fields, present only when a drift allowlist drove the run. The
# `allowlist`/`drift_allowlist` string fields cannot match the integer-only pattern, so they
# are (correctly) never read.
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
oracle_drift=$(field oracle_drift)

# A summary we cannot read is a hard failure: fail LOUD rather than risk a false pass.
if ! is_uint "$total" || ! is_uint "$agree" || ! is_uint "$mismatch" \
  || ! is_uint "$unsupported" || ! is_uint "$skipped" || ! is_uint "$oracle_drift"; then
  echo "gate: could not parse the top-level summary from '$report' — failing loud (exit 1)" >&2
  exit 1
fi

# --- the report's OWN ratchet counters (one of the two views; never trusted alone) ------
# They describe the classification of the run that PRODUCED the report, against the
# allowlists that run used. `--report <file>` may hand us a report from a different sweep, so
# these are only ever combined (per-counter `max`) with the live re-classification below.
report_allowed=$(rfield allowed)
report_unauthorized=$(rfield unauthorized)
report_stale=$(rfield stale)

# The ratchet block is expected on every run (both allowlists are always passed): a missing or
# unparsable block means an unexpected report shape, so fail LOUD (exit 2) — never a pass.
if ! is_uint "$report_allowed" || ! is_uint "$report_unauthorized" || ! is_uint "$report_stale"; then
  echo "gate: could not parse the top-level ratchet block from '$report' — unexpected report shape (exit 2)" >&2
  exit 2
fi

report_drift_allowed=$(rfield drift_allowed)
report_drift_unauthorized=$(rfield drift_unauthorized)
report_drift_stale=$(rfield drift_stale)

# The `drift_*` fields are absent when the report was produced WITHOUT a drift allowlist (an
# older report, or one built by a command that only ratcheted gaps). That is not an unparsable
# block — it is a report that carries no drift classification: read as every `oracle_drift`
# case being UNRECORDED (allowed=0, unauthorized=oracle_drift, stale=0). The live view below
# is applied on top of that, so this special case can only ever fail, never pass.
drift_classified=1
if ! is_uint "$report_drift_allowed" || ! is_uint "$report_drift_unauthorized" \
  || ! is_uint "$report_drift_stale"; then
  drift_classified=0
  echo "gate: report carries no drift classification — its own drift counters are read as all $oracle_drift drift cases unrecorded" >&2
  report_drift_allowed=0
  report_drift_unauthorized=$oracle_drift
  report_drift_stale=0
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

# --- classify every non-agreeing id against its own allowlist ---------------------------
# A *gap* is a non-agreeing case whose verdict is `unsupported`/`skipped`; a *drift* is one
# whose verdict is `oracle_drift`. A `mismatch` is NEITHER — it is a hard failure handled by
# the `mismatch` branch below — so it is deliberately excluded here: an allowlist tolerates a
# capability gap (resp. an oracle drift), never a mismatch, and this summary can therefore
# never imply that a mismatch was "allowed".
# (for a readable summary: which gaps/drifts are unauthorized, which entries are stale)
printf '%s\n' "$failing" | awk '$1 == "unsupported" || $1 == "skipped" { print $2 }' > "$out/gap-ids.txt"
printf '%s\n' "$failing" | awk '$1 == "oracle_drift" { print $2 }' > "$out/drift-ids.txt"
printf '%s\n' "$failing" | awk '$1 == "mismatch" { print $2 }' > "$out/mismatch-ids.txt"
awk '!/^[[:space:]]*#/ && NF > 0 { print $1 }' "$allowlist" > "$out/allow-ids.txt"
awk '!/^[[:space:]]*#/ && NF > 0 { print $1 }' "$drift_allowlist" > "$out/drift-allow-ids.txt"

# ids_absent <ids-file> <allowlist-ids-file>: the ids of <ids-file> that <allowlist-ids-file>
# does not record. This is the single membership test behind both the live classification and
# the stale-entry check (the same awk, with the two files swapped).
ids_absent() {
  awk -v ids="$(cat "$2")" '
    BEGIN { n = split(ids, a, "\n"); for (i = 1; i <= n; i++) if (a[i] != "") ok[a[i]] = 1 }
    { if (!($0 in ok)) print }
  ' "$1"
}
count_ids() {
  if [ -z "$1" ]; then
    printf '0\n'
  else
    printf '%s\n' "$1" | wc -l | tr -d ' '
  fi
}

unauthorized_ids=$(ids_absent "$out/gap-ids.txt" "$out/allow-ids.txt")
stale_ids=$(ids_absent "$out/allow-ids.txt" "$out/gap-ids.txt")
drift_unauthorized_ids=$(ids_absent "$out/drift-ids.txt" "$out/drift-allow-ids.txt")
drift_stale_ids=$(ids_absent "$out/drift-allow-ids.txt" "$out/drift-ids.txt")
recorded_drift_ids=$(cat "$out/drift-ids.txt")

# --- view (ii): the LIVE re-classification of the report's CASE VERDICTS against the -----
# allowlists THIS invocation loaded. These counters are derived from the case list, so they
# stay correct even when the report was produced with different allowlists (or with none).
# `live_allowed` is "gap ids the gaps allowlist records"; `live_stale` is "allowlist entries
# that did not reproduce as a gap" (and likewise for the drift ratchet).
live_gaps=$(count_ids "$(cat "$out/gap-ids.txt")")
live_drifts=$(count_ids "$(cat "$out/drift-ids.txt")")
live_mismatch=$(count_ids "$(cat "$out/mismatch-ids.txt")")
live_unauthorized=$(count_ids "$unauthorized_ids")
live_stale=$(count_ids "$stale_ids")
live_drift_unauthorized=$(count_ids "$drift_unauthorized_ids")
live_drift_stale=$(count_ids "$drift_stale_ids")
live_allowed=$((live_gaps - live_unauthorized))
live_drift_allowed=$((live_drifts - live_drift_unauthorized))

# --- JUDGE with the STRICTER of view (i) and view (ii), as a per-counter maximum ---------
# A disagreement can therefore only ever push the gate to exit 3 (the ratchet must be
# updated) or, for a mismatch, to exit 1 — never to exit 0.
max_of() {
  if [ "$1" -ge "$2" ]; then
    printf '%s\n' "$1"
  else
    printf '%s\n' "$2"
  fi
}
judged_mismatch=$(max_of "$mismatch" "$live_mismatch")
judged_allowed=$(max_of "$report_allowed" "$live_allowed")
judged_unauthorized=$(max_of "$report_unauthorized" "$live_unauthorized")
judged_stale=$(max_of "$report_stale" "$live_stale")
judged_drift_allowed=$(max_of "$report_drift_allowed" "$live_drift_allowed")
judged_drift_unauthorized=$(max_of "$report_drift_unauthorized" "$live_drift_unauthorized")
judged_drift_stale=$(max_of "$report_drift_stale" "$live_drift_stale")

views_disagree=0
if [ "$mismatch" -ne "$live_mismatch" ] \
  || [ "$report_allowed" -ne "$live_allowed" ] \
  || [ "$report_unauthorized" -ne "$live_unauthorized" ] \
  || [ "$report_stale" -ne "$live_stale" ] \
  || [ "$report_drift_allowed" -ne "$live_drift_allowed" ] \
  || [ "$report_drift_unauthorized" -ne "$live_drift_unauthorized" ] \
  || [ "$report_drift_stale" -ne "$live_drift_stale" ]; then
  views_disagree=1
fi
# Which counters, if any, the REPORT view makes stricter than the live one. Such a count has
# no id list of its own (the ids belong to the report's own allowlist, which is not here), so
# the note names the counter and points at the case ids printed below.
report_stricter=""
if [ "$report_unauthorized" -gt "$live_unauthorized" ]; then
  report_stricter="$report_stricter gaps:unauthorized"
fi
if [ "$report_stale" -gt "$live_stale" ]; then
  report_stricter="$report_stricter gaps:stale"
fi
if [ "$report_drift_unauthorized" -gt "$live_drift_unauthorized" ]; then
  report_stricter="$report_stricter drift:unauthorized"
fi
if [ "$report_drift_stale" -gt "$live_drift_stale" ]; then
  report_stricter="$report_stricter drift:stale"
fi

# --- human summary ----------------------------------------------------------------------
echo "differential gate: total=$total agree=$agree mismatch=$mismatch unsupported=$unsupported skipped=$skipped oracle_drift=$oracle_drift ($run_note)"
if [ "$drift_classified" -eq 1 ]; then
  echo "report ratchet counters (the report's own view):   gaps allowed=$report_allowed unauthorized=$report_unauthorized stale=$report_stale; oracle drift allowed=$report_drift_allowed unauthorized=$report_drift_unauthorized stale=$report_drift_stale"
else
  echo "report ratchet counters (the report's own view):   gaps allowed=$report_allowed unauthorized=$report_unauthorized stale=$report_stale; oracle drift allowed=$report_drift_allowed unauthorized=$report_drift_unauthorized stale=$report_drift_stale (no drift classification in the report)"
fi
echo "live re-classification against the allowlists here: gaps allowed=$live_allowed unauthorized=$live_unauthorized stale=$live_stale; oracle drift allowed=$live_drift_allowed unauthorized=$live_drift_unauthorized stale=$live_drift_stale"
if [ "$views_disagree" -eq 1 ]; then
  if [ -n "$report_stricter" ]; then
    echo "gate: note — the report's own ratchet counters disagree with a re-classification against the allowlists passed here; judging with the stricter of the two (per-counter max) — the report view is stricter for:$report_stricter (its ids are the case ids listed below)" >&2
  else
    echo "gate: note — the report's own ratchet counters disagree with a re-classification against the allowlists passed here; judging with the stricter of the two (per-counter max)" >&2
  fi
fi
echo "judged counters (per-counter max — these drive the exit code): mismatch=$judged_mismatch; gaps allowed=$judged_allowed unauthorized=$judged_unauthorized stale=$judged_stale; oracle drift allowed=$judged_drift_allowed unauthorized=$judged_drift_unauthorized stale=$judged_drift_stale"
if [ -n "$failing" ]; then
  echo "non-agreeing cases:"
  printf '%s\n' "$failing" | sed 's/^/  /'
fi
if [ -n "$recorded_drift_ids" ]; then
  echo "recorded oracle drift:"
  printf '%s\n' "$recorded_drift_ids" | sed 's/^/  /'
fi
if [ -n "$unauthorized_ids" ]; then
  echo "unauthorized gaps (record them in the gaps allowlist deliberately):"
  printf '%s\n' "$unauthorized_ids" | sed 's/^/  /'
fi
if [ -n "$stale_ids" ]; then
  echo "stale gap-allowlist entries (the gap closed — drop them):"
  printf '%s\n' "$stale_ids" | sed 's/^/  /'
fi
if [ -n "$drift_unauthorized_ids" ]; then
  echo "unauthorized oracle drift (record it in the drift allowlist deliberately):"
  printf '%s\n' "$drift_unauthorized_ids" | sed 's/^/  /'
fi
if [ -n "$drift_stale_ids" ]; then
  echo "stale drift-allowlist entries (the drift closed — drop them):"
  printf '%s\n' "$drift_stale_ids" | sed 's/^/  /'
fi

# --- verdict ----------------------------------------------------------------------------
# A mismatch is the ONLY path to exit 1 (and it is checked first, so a mismatch can never be
# masked as a ratchet update). It is read from the STRICTER of the report's `summary.mismatch`
# and the live count of `mismatch` verdicts, and neither allowlist can tolerate one. Only
# then do the two independent ratchets decide 3 vs 0, again on the stricter counters.
if [ "$judged_mismatch" -gt 0 ]; then
  echo "gate: FAIL — $judged_mismatch mismatch(es): a real behavioral divergence (exit 1)" >&2
  exit 1
fi
if [ "$judged_unauthorized" -gt 0 ] || [ "$judged_stale" -gt 0 ] \
  || [ "$judged_drift_unauthorized" -gt 0 ] || [ "$judged_drift_stale" -gt 0 ]; then
  echo "gate: FAIL — no mismatch, but a ratchet must be updated (judged counts): gaps unauthorized=$judged_unauthorized stale=$judged_stale, oracle drift unauthorized=$judged_drift_unauthorized stale=$judged_drift_stale (exit 3)" >&2
  exit 3
fi
echo "gate: PASS — every case agreed or is a recorded capability gap or oracle drift (tolerated $judged_drift_allowed recorded oracle drift(s)); both ratchets are fresh (exit 0)"
exit 0
