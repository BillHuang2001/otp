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
## (`lib/compiler/scripts/host_env.sh build'), compiles the suites into a
## private scratch ebin and drives the Common Test callbacks with
## `stage2_suite_driver' (see that module for what the driver does).
##
## HOW THE SUITES ARE COMPILED (the host-only adaptation)
##   The suites are *executed* by the rig compiler (`-pa $RIG_EBIN' first: they
##   call `compile:file/2' and the pass modules such as
##   `beam_ssa_loop_opt:opt/1' directly), but they are *compiled* by the host
##   `erlc' from a copy of the sources in which
##   `lib/compiler/scripts/host_env_xform.py' has flattened the OTP-30 native
##   records (`#b_set{}'/`#t_integer{}'/`#opt_st{}' from `beam_ssa.hrl',
##   `beam_types.hrl' and `beam_ssa_opt') onto classic tuple records -- exactly
##   the rewriting the rig compiler itself was built with.  Otherwise the two
##   sides of every hand-built SSA term disagree:
##     * a suite compiled by the *rig* compiler builds native-record terms and
##       its beam uses the OTP-30-only `update_record_id' instruction (opcode
##       193), which the host emulator refuses to load at all;
##     * compiling such a suite with `no_native_record_opt' (as this runner used
##       to) made the beam load, but the hand-built terms no longer agreed with
##       the rig's pass modules: 9 of `beam_ssa_loop_opt_SUITE''s test cases
##       failed with `error:{badrecord,{beam_ssa,b_literal}}' or
##       `error:{badrecord,{beam_types,t_integer}}'.
##
##   Compiling the suite from the flattened headers instead leaves its
##   hand-built terms and the rig's (identically flattened) pass modules
##   agreeing on classic tuple records.
##
##   The flattened sources are produced in the scratch directory:
##     $SCRATCH/xform_in   a flat copy of `lib/compiler/src/*.{erl,hrl}' -- the
##                         transform resolves `-import_record(Mod,[...])'
##                         against the record definitions found in `Mod''s own
##                         source, so the compiler sources must be there -- plus
##                         `test_lib.erl', `test_lib.hrl',
##                         `stage2_suite_driver.erl' and the suites being run;
##     $SCRATCH/xform_out  the transformed sources and the generated
##                         `imp_*.hrl' import headers; it is the `-I' directory
##                         the suites are compiled with.
##
##   Only the files that are compiled are handed to the transform, so the strict
##   checks of `host_env_xform.py' (it refuses a `#_{...}' wildcard or an
##   arity-1 `is_record/1' it cannot rewrite) can never be tripped by an
##   unrelated suite; a *requested* suite that trips it aborts the run with the
##   file and the construct named by the transform.  Two suites of this
##   directory would trip it (neither is a Stage-2 suite or in the default set):
##   `compile_SUITE' (arity-1 `is_record/1') and `native_record_SUITE' (`#_{...}'
##   wildcards and arity-1 `is_record/1') -- both constructs are OTP-30
##   native-record tests in the first place, which cannot be preserved on a host
##   that has no native-record term type.
##
## USAGE
##     lib/compiler/test/run_stage2_suites.sh [SUITE ...]
##
## With no arguments the five Stage-2 suites are run:
##     beam_ssa_loop_opt_SUITE beam_ssa_fus_SUITE beam_ssa_fbip_SUITE
##     ssa_opt_budget_SUITE beam_ssa_opt_equiv_SUITE
##
## Output: one `PASS'/`FAIL'/`SKIP'/`WARN' line per test case plus per-suite and
## total summaries, on stdout and in `$SCRATCH/report.txt'.  The exit
## status is 1 if any test case failed, else 0 (2 for a usage/environment
## error).
##
## ENVIRONMENT
##     RIG_EBIN            use this prebuilt rig compiler ebin instead of
##                         running `host_env.sh build' (must contain
##                         `compile.beam').
##     STAGE2_SCRATCH      scratch directory (default: a fresh
##                         `$TMPDIR/stage2_suites.XXXXXX'); it is kept, so the
##                         report, the flattened sources, the compiled suites
##                         and the probe modules the suites write stay available
##                         for inspection.
##     TMPDIR              where the scratch directory and the suites' probe
##                         directories are created.
##
## CAVEATS
##   * This is a *host* rig, not an OTP-30 run: the suites are compiled with the
##     host compiler and with native records flattened to tuples (see above), so
##     the OTP-30 native-record mechanism itself is never exercised.  The report
##     says so (`NOTE' lines) for every suite the flattening touched.
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
XFORM="$ERL_TOP/lib/compiler/scripts/host_env_xform.py"
SRC_DIR="$ERL_TOP/lib/compiler/src"

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
XIN="$SCRATCH/xform_in"
XOUT="$SCRATCH/xform_out"
rm -rf "$XIN" "$XOUT"          # a reused STAGE2_SCRATCH must not carry over sources
mkdir -p "$EBIN" "$PRIV" "$XIN" "$XOUT"

# ---------------------------------------- 3. flatten the sources for the host
command -v python3 >/dev/null 2>&1 ||
    die "no python3 on PATH (needed for host_env_xform.py)"
for suite in "${SUITES[@]}"; do
    [ -f "$TEST_DIR/$suite.erl" ] ||
        die "no such suite: $suite ($TEST_DIR/$suite.erl)"
done

cp "$SRC_DIR"/*.erl "$SRC_DIR"/*.hrl "$XIN/"
cp "$TEST_DIR/test_lib.erl" "$TEST_DIR/test_lib.hrl" \
   "$TEST_DIR/stage2_suite_driver.erl" "$XIN/"
for suite in "${SUITES[@]}"; do
    cp "$TEST_DIR/$suite.erl" "$XIN/"
done

echo "== flattening native records: python3 $XFORM $XIN $XOUT"
python3 "$XFORM" "$XIN" "$XOUT" ||
    die "host_env_xform.py failed (its message names the file and construct)"

# Which suites needed that adaptation?  A suite is affected when the transform
# rewrote its own source (it declares or imports a native record itself, e.g.
# `-import_record(beam_ssa_opt, [opt_st])') or one of the headers it includes
# (the suite then builds or matches former native records as tuples).  The
# comparison is against the untransformed copy in $XIN, so it is the transform's
# own verdict rather than a guess about the record syntax.
flatten_affected() {
    local hdr
    cmp -s "$TEST_DIR/$1.erl" "$XOUT/$1.erl" || return 0
    while read -r hdr; do
        hdr=${hdr##*/}
        [ -f "$XIN/$hdr" ] || continue
        cmp -s "$XIN/$hdr" "$XOUT/$hdr" || return 0
    done < <(sed -n -e 's/^-include("\([^"]*\)").*/\1/p' \
                    -e 's/^-include_lib("\([^"]*\)").*/\1/p' \
                    "$TEST_DIR/$1.erl")
    return 1
}

FLATTENED=()
for suite in "${SUITES[@]}"; do
    if flatten_affected "$suite"; then
        FLATTENED+=("$suite")
    fi
done
echo "== suites needing the host record flattening: ${FLATTENED[*]:-none}"

# ------------------------------------------------------------- 4. compile it
# The *host* erlc, with the flattened tree as the include directory: the suites
# must be compiled against the transformed `beam_ssa.hrl'/`beam_types.hrl' and
# the generated import headers, never against `lib/compiler/src' (whose headers
# still declare native records).
INCLUDES=(-I "$XOUT" -I "$ERL_TOP/lib/stdlib/include")
LINT=(+clint +clint0 +ssalint +nowarn_missing_spec_documented)

HOST_SRCS=("$XOUT/test_lib.erl" "$XOUT/stage2_suite_driver.erl")
for suite in "${SUITES[@]}"; do
    HOST_SRCS+=("$XOUT/$suite.erl")
done

compile_all() {
    rm -f "$EBIN"/*.beam
    erlc "${INCLUDES[@]}" "$@" -o "$EBIN" "${HOST_SRCS[@]}"
}

echo "== compiling ${#HOST_SRCS[@]} modules with the host erlc" \
     "($(command -v erlc)) -> $EBIN"
if ! compile_all "${LINT[@]}"; then
    echo "!! the host erlc rejected the lint flags ${LINT[*]};" \
         "retrying without them" >&2
    compile_all || die "compilation failed"
    echo "!! compiled WITHOUT the lint flags" >&2
fi

# --------------------------------------------------------------- 5. run them
# From the repository root: the suites fall back to repository-relative paths
# when they look for their own source (ssa_opt_budget_SUITE:suite_source/0).
cd "$ERL_TOP"
echo "== running ${SUITES[*]} with the rig compiler first on the code path"
set +e
STAGE2_PRIV_BASE="$PRIV" \
STAGE2_RIG_EBIN="$RIG_EBIN" \
STAGE2_FLATTENED_SUITES="${FLATTENED[*]:-}" \
    erl -noshell -pa "$RIG_EBIN" -pa "$EBIN" \
        -run stage2_suite_driver main "$REPORT" "${SUITES[@]}"
STATUS=$?
set -e

echo
echo "run_stage2_suites: report=$REPORT scratch=$SCRATCH status=$STATUS"
exit "$STATUS"
