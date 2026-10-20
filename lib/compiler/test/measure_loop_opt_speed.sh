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
## measure_loop_opt_speed.sh -- RUNTIME speed measurement for the opt-in
## "Stage-2" loop passes (`loop_opt` + `loop_arith_licm`).
##
## Stage 2 asks for a measured runtime speedup on a real loop workload.  This
## harness compiles a set of realistic tail-recursive Erlang loops twice with
## THIS tree's compiler -- once with the flags, once without -- loads both builds
## side by side in ONE VM and times them with interleaved paired samples, so VM
## warm-up, JIT state and host load cancel out.  For every kernel it also
## reports whether the pass changed the generated code at all (the reach
## assertion) and the per-variant GC/allocation counters.
##
## Usage:
##     lib/compiler/test/measure_loop_opt_speed.sh
##     REPS=21 ROUNDS=7 lib/compiler/test/measure_loop_opt_speed.sh
##
## Environment:
##     REPS=N          measured repetitions per variant (default 15, odd)
##     ROUNDS=N        whole-program cross-check rounds (default 5, 0 = skip)
##     RIG_EBIN=DIR    skip the rig build and use an existing rig ebin
##     KEEP_SCRATCH=1  keep the scratch dir holding the kernel beams
##
## It never builds the OTP emulator and never touches the repository: the rig
## compiler is built by lib/compiler/scripts/host_env.sh into /tmp.  See
## ../scripts/CONTEXT.md for the rig and ./CONTEXT.md for the suite recipe.
##
## HONESTY NOTE: this is a shared, loaded machine, so *absolute* times are not
## reproducible and must not be quoted.  Only the ON/OFF ratio measured inside a
## single session carries any signal, and even that only together with the
## reported spread and with the "Code changed" column -- a ratio measured on
## unchanged code is noise.  The harness fails when a kernel that is declared to
## reach does not reach.

set -u

script_dir="$(cd "$(dirname "$0")" && pwd -P)"
ERL_TOP="${ERL_TOP:-$(cd "$script_dir/../../.." && pwd -P)}"
REPS="${REPS:-15}"
ROUNDS="${ROUNDS:-5}"

die() { echo "measure_loop_opt_speed: $*" >&2; exit 1; }

# ---------------------------------------------------------------- 1/3 rig ---
# The host `erl'/`erlc' are usually an older OTP that cannot load this tree's
# OTP-30 beams, so the rig builds this tree's `lib/compiler' sources for the
# host runtime.  RIG_EBIN may be preset to skip the (idempotent, ~25 s) build.
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
SCRATCH="${SCRATCH:-$(mktemp -d "${TMPDIR:-/tmp}/loop_opt_bench.XXXXXX")}" \
    || die "cannot create a scratch dir"
mkdir -p "$SCRATCH/ebin" "$SCRATCH/beam"
echo "== 2/3 compiling the driver (loop_opt_bench.erl) into $SCRATCH/ebin =="
"$HOST_ERLC" -o "$SCRATCH/ebin" "$script_dir/loop_opt_bench.erl" \
    || die "cannot compile loop_opt_bench.erl with $HOST_ERLC"

# --------------------------------------------------------------- 3/3 run ---
# `-pa' prepends, so the LAST -pa wins the front of the code path: the rig
# compiler must come after the scratch dir.  loop_opt_bench verifies that
# code:which(compile) really is inside RIG_EBIN and refuses to measure anything
# otherwise (an escript or a forgotten -pa would silently benchmark the host OTP
# compiler, whose default pass pipeline knows nothing about the Stage-2 flags,
# so every kernel would come out "unchanged").
echo "== 3/3 measuring: REPS=$REPS, ROUNDS=$ROUNDS =="
RIG_EBIN="$RIG_EBIN" SCRATCH="$SCRATCH" HOST_ERL="$HOST_ERL" \
    "$HOST_ERL" -noshell -pa "$SCRATCH/ebin" -pa "$RIG_EBIN" \
    -s loop_opt_bench main -extra "$REPS" "$ROUNDS"
rc=$?

if [ "$rc" -ne 0 ]; then
    echo "measure_loop_opt_speed: harness FAILED (rc=$rc)" >&2
else
    echo "measure_loop_opt_speed: harness completed (rc=0); scratch dir: $SCRATCH"
fi
if [ -z "${KEEP_SCRATCH:-}" ]; then
    rm -rf "$SCRATCH"
fi
exit "$rc"
