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
## run_stage2_suites.sh -- run the Stage-2 compiler-pass Common Test suites of
## this directory on the *host* Erlang/OTP.
##
## WHY: `suite/0' of each of those suites returns `{ct_hooks,[ts_install_cth]}',
## and `ts_install_cth' is not installed by a plain in-tree build, so neither
## `ct_run' nor `ct:run_test/1' can start them.  This script builds a compiler
## for the host from this tree's `lib/compiler' sources
## (`lib/compiler/scripts/host_env.sh build'), compiles the suites with it into
## a private scratch ebin and drives the Common Test callbacks with
## `stage2_suite_driver' (see that module for what the driver does and for the
## native-record fallback).
##
## USAGE
##     lib/compiler/test/run_stage2_suites.sh [SUITE ...]
##
## With no arguments the five Stage-2 suites are run:
##     beam_ssa_loop_opt_SUITE beam_ssa_fus_SUITE beam_ssa_fbip_SUITE
##     ssa_opt_budget_SUITE beam_ssa_opt_equiv_SUITE
##
## Output: one `PASS'/`FAIL'/`SKIP'/`WARN' line per test case plus per-suite and
## total summaries, on stdout and in `$STAGE2_SCRATCH/report.txt'.  The exit
## status is 1 if any test case failed, else 0 (2 for a usage/environment
## error).
##
## ENVIRONMENT
##     RIG_EBIN            use this prebuilt rig compiler ebin instead of
##                         running `host_env.sh build' (must contain
##                         `compile.beam').
##     STAGE2_SCRATCH      scratch directory (default: a fresh
##                         `$TMPDIR/stage2_suites.XXXXXX'); it is kept, so the
##                         report, the compiled suites and the probe modules
##                         the suites write stay available for inspection.
##     TMPDIR              where the scratch directory and the suites' probe
##                         directories are created.
##
## CAVEATS
##   * The host OTP is a released OTP (>= 29) whose emulator cannot execute the
##     OTP-30-only `update_record_id' instruction; the driver recompiles a suite
##     that needs native records with `no_native_record_opt' (see the NOTE line
##     it prints).  The test cases that *build* native records cannot work on
##     the host at all.
##   * The suites run sequentially; the `parallel' group property and the
##     `timetrap' from `suite/0' are not enforced.
##

set -euo pipefail

SUITES=("$@")
if [ ${#SUITES[@]} -eq 0 ]; then
    SUITES=(beam_ssa_loop_opt_SUITE
            beam_ssa_fus_SUITE
            beam_ssa_fbip_SUITE
            ssa_opt_budget_SUITE
            beam_ssa_opt_equiv_SUITE)
fi

TEST_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ERL_TOP=$(cd "$TEST_DIR/../../.." && pwd)

die() { echo "run_stage2_suites: $*" >&2; exit 2; }

# ---------------------------------------------------------------- 1. the rig
if [ -n "${RIG_EBIN:-}" ] && [ -f "$RIG_EBIN/compile.beam" ]; then
    echo "== using the pre-set rig compiler: RIG_EBIN=$RIG_EBIN"
else
    echo "== building the host rig: lib/compiler/scripts/host_env.sh build"
    RIG_EBIN=$("$ERL_TOP/lib/compiler/scripts/host_env.sh" build |
                   sed -n 's/^RIG_EBIN=//p')
fi
[ -n "${RIG_EBIN:-}" ] && [ -f "$RIG_EBIN/compile.beam" ] ||
    die "no usable rig compiler (RIG_EBIN='${RIG_EBIN:-}')"

# ----------------------------------------------------------- 2. scratch dirs
SCRATCH=${STAGE2_SCRATCH:-$(mktemp -d "${TMPDIR:-/tmp}/stage2_suites.XXXXXX")}
EBIN="$SCRATCH/ebin"
PRIV="$SCRATCH/priv"
REPORT="$SCRATCH/report.txt"
mkdir -p "$EBIN" "$PRIV"

# Compile and run from the repository root: the suites use `?FILE' (the source
# path as given to the compiler) as a fallback when they look for their own
# source (ssa_opt_budget_SUITE:suite_source/0), so the sources are compiled
# under their repo-root-relative path.
cd "$ERL_TOP"

# ------------------------------------------------------------ 3. the sources
SRCS=("$TEST_DIR/test_lib.erl" "$TEST_DIR/stage2_suite_driver.erl")
for suite in "${SUITES[@]}"; do
    src="$TEST_DIR/$suite.erl"
    [ -f "$src" ] || die "no such suite: $suite ($src)"
    SRCS+=("$src")
done

RELS=()
for src in "${SRCS[@]}"; do
    RELS+=("${src#"$ERL_TOP"/}")
done

INCLUDES=(-I lib/stdlib/include -I lib/compiler/test -I lib/compiler/src)
LINT=(+clint +clint0 +ssalint +nowarn_missing_spec_documented)

# ------------------------------------------------------------- 4. compile it
compile_all() {
    rm -f "$EBIN"/*.beam
    erlc -pa "$RIG_EBIN" "${INCLUDES[@]}" "$@" -o "$EBIN" "${RELS[@]}"
}

echo "== compiling ${#RELS[@]} modules with the rig compiler -> $EBIN"
if ! compile_all "${LINT[@]}"; then
    echo "!! the rig compiler rejected the lint flags ${LINT[*]};" \
         "retrying without them" >&2
    compile_all || die "compilation failed"
    echo "!! compiled WITHOUT the lint flags" >&2
fi

# --------------------------------------------------------------- 5. run them
echo "== running ${SUITES[*]}"
set +e
STAGE2_PRIV_BASE="$PRIV" STAGE2_SUITE_SRC_DIR="$TEST_DIR" \
    erl -noshell -pa "$RIG_EBIN" -pa "$EBIN" \
        -run stage2_suite_driver main "$REPORT" "${SUITES[@]}"
STATUS=$?
set -e

echo
echo "run_stage2_suites: report=$REPORT scratch=$SCRATCH status=$STATUS"
exit "$STATUS"
