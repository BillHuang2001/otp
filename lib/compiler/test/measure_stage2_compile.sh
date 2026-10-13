#!/usr/bin/env bash
##
## %CopyrightBegin%
##
## SPDX-License-Identifier: Apache-2.0
##
## Copyright Ericsson AB 2026. All Rights Reserved.
##
## Licensed under the Apache License, Version 2.0 (the "License");
## you may not use this file except in compliance with the License.
## You may obtain a copy of the License at
##
##     http://www.apache.org/licenses/LICENSE-2.0
##
## Unless required by applicable law or agreed to in writing, software
## distributed under the License is distributed on an "AS IS" BASIS,
## WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
## See the License for the specific language governing permissions and
## limitations under the License.
##
## %CopyrightEnd%
##
##
## measure_stage2_compile.sh -- compile-throughput measurement for the opt-in
## "Stage-2" compiler passes (`loop_opt`+`loop_arith_licm`, `fusion_opt`,
## `fbip_opt`).
##
## It compiles a whole Erlang corpus in ONE Erlang session, once per option set,
## and reports the wall-clock cost of each set relative to the default (OFF)
## build, together with the number of modules whose generated `Code' chunk
## actually changed -- i.e. whether the pass fired.
##
## Two corpora are available (CORPUS):
##     stdlib (default)  $ERL_TOP/lib/stdlib/src/*.erl   96 modules
##     all               $ERL_TOP/lib/*/src/*.erl        ~772 modules
## The `all' corpus is the source of the full-lib/*/src reach numbers; a handful
## of its files fail for unrelated include/host reasons and are listed, and the
## run continues.
##
## Usage:
##     lib/compiler/test/measure_stage2_compile.sh
##     CORPUS=all REPS=1 lib/compiler/test/measure_stage2_compile.sh
##
## Environment:
##     CORPUS=stdlib|all  corpus to measure (default stdlib)
##     REPS=N          measured repetitions per option set (default 5, >= 1)
##     RIG_EBIN=DIR    skip the rig build and use an existing rig ebin
##     KEEP_SCRATCH=1  keep the scratch dir holding the compiled helper module
##
## It never builds the OTP emulator and never touches the repository: the rig
## compiler is built by lib/compiler/scripts/host_env.sh into /tmp.  See
## ../scripts/CONTEXT.md for the rig, and ./CONTEXT.md for the recipe used to
## drive suites without ct_run.
##
## HONESTY NOTE: this is a shared, loaded machine, so *absolute* wall-clock
## numbers are not reproducible and must not be quoted as timings.  Only the
## ratios between the option sets, measured inside a single session with the
## option order rotated between repetitions, carry any signal -- and even those
## are dominated by noise when the pass only removes a handful of instructions.
## Read the "changed" column: it says whether the pass did anything at all on
## this corpus.

set -u

script_dir="$(cd "$(dirname "$0")" && pwd -P)"
ERL_TOP="${ERL_TOP:-$(cd "$script_dir/../../.." && pwd -P)}"
REPS="${REPS:-5}"
CORPUS="${CORPUS:-stdlib}"

die() { echo "measure_stage2_compile: $*" >&2; exit 1; }

case "$CORPUS" in
    stdlib) corpus_glob="$ERL_TOP/lib/stdlib/src/*.erl" ;;
    all)    corpus_glob="$ERL_TOP/lib/*/src/*.erl" ;;
    *)      die "unknown CORPUS=$CORPUS (expected stdlib or all)" ;;
esac

# ---------------------------------------------------------------- 1/3 rig ---
# The host `erl'/`erlc' are usually an older OTP that cannot load this tree's
# OTP-30 beams, so the rig builds this tree's `lib/compiler' sources for the
# host runtime.  RIG_EBIN may be preset to skip the (idempotent, ~20 s) build.
RIG_EBIN="${RIG_EBIN:-}"
if [ -z "$RIG_EBIN" ]; then
    echo "== 1/3 building the host-side rig compiler (lib/compiler/scripts/host_env.sh) =="
    build_out="$(ERL_TOP="$ERL_TOP" "$ERL_TOP/lib/compiler/scripts/host_env.sh" build 2>&1)"
    rc=$?
    if [ $rc -ne 0 ]; then
        printf '%s\n' "$build_out" | tail -20 >&2
        die "host_env.sh build failed (rc=$rc)"
    fi
    printf '%s\n' "$build_out" | grep -E '^=== |^RIG_EBIN=' \
        | sed 's/^/   /'
    RIG_EBIN="$(printf '%s\n' "$build_out" | sed -n 's/^RIG_EBIN=//p' | tail -1)"
else
    echo "== 1/3 using preset RIG_EBIN=$RIG_EBIN =="
fi
[ -n "$RIG_EBIN" ] || die "could not determine RIG_EBIN"
[ -f "$RIG_EBIN/compile.beam" ] || die "no compile.beam in RIG_EBIN=$RIG_EBIN"

HOST_ERL="$(command -v erl || true)"
HOST_ERLC="$(command -v erlc || true)"
[ -n "$HOST_ERL" ] || die "no erl on PATH"
[ -n "$HOST_ERLC" ] || die "no erlc on PATH"

# ------------------------------------------------------------- 2/3 helper ---
SCRATCH="${SCRATCH:-$(mktemp -d "${TMPDIR:-/tmp}/stage2_bench.XXXXXX")}" \
    || die "cannot create a scratch dir"
mkdir -p "$SCRATCH/ebin"
echo "== 2/3 compiling the helper (stage2_bench.erl) into $SCRATCH/ebin =="
"$HOST_ERLC" -o "$SCRATCH/ebin" "$script_dir/stage2_bench.erl" \
    || die "cannot compile stage2_bench.erl with $HOST_ERLC"

# --------------------------------------------------------------- 3/3 run ---
# `-pa' prepends, so the LAST -pa wins the front of the code path: the rig
# compiler must come after the scratch dir.  stage2_bench verifies that
# code:which(compile) really is inside RIG_EBIN and refuses to measure anything
# otherwise (an escript or a forgotten -pa would silently benchmark the host
# OTP compiler, whose default pass pipeline knows nothing about the Stage-2
# flags).
echo "== 3/3 measuring: REPS=$REPS, CORPUS=$CORPUS, corpus=$corpus_glob =="
RIG_EBIN="$RIG_EBIN" "$HOST_ERL" -noshell \
    -pa "$SCRATCH/ebin" -pa "$RIG_EBIN" \
    -s stage2_bench main -extra "$REPS" "$ERL_TOP" "$CORPUS"
rc=$?

if [ "$rc" -ne 0 ]; then
    echo "measure_stage2_compile: harness FAILED (rc=$rc)" >&2
else
    echo "measure_stage2_compile: harness completed (rc=0); scratch dir: $SCRATCH"
fi
if [ -z "${KEEP_SCRATCH:-}" ]; then
    rm -rf "$SCRATCH"
fi
exit "$rc"
