#!/usr/bin/env bash
#
# %CopyrightBegin%
#
# SPDX-License-Identifier: Apache-2.0
#
# Copyright Ericsson AB 2026. All Rights Reserved.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#
# %CopyrightEnd%
#
# host_env.sh -- build and run the in-tree `lib/compiler' application on the
# *host* Erlang/OTP, without building this tree's emulator.
#
# WHY: `erl'/`erlc'/`escript' on PATH are usually a released OTP (e.g. 29.x) that
# cannot load the OTP-30 beams in `bootstrap/lib/**' ("compiled for a later
# version of the runtime system ... opcode 193") and whose `compiler' application
# would provide the record layouts for `-import_record'.  Building the OTP-30
# emulator needs autoconf/m4 and is out of scope.  The host erlc *can* compile
# this tree's `lib/compiler/src/*.erl', so this script rewrites a scratch copy of
# those sources into a host-compilable shape (see host_env_xform.py) and builds
# them with the host erlc.  The result is a compiler that runs on the host and
# can be driven with `compile:file/2' to exercise the tree's pass pipeline.
#
# Usage (from anywhere; reads only, writes only to the scratch dir):
#     lib/compiler/scripts/host_env.sh build      # build, prints RIG_EBIN=<dir>
#     lib/compiler/scripts/host_env.sh shell      # erl with the rig compiler first
#     lib/compiler/scripts/host_env.sh cleandir   # remove the scratch dir
#
# `build' is idempotent: it re-transforms and recompiles from scratch.  The
# scratch dir defaults to /tmp/host_env_<hash of $ERL_TOP> and can be overridden
# with HOST_ENV_DIR.  TEST-ONLY DEV HELPER: not installed and not part of
# `SUB_DIRECTORIES' or CI, and it never modifies the repository.

set -e

ERL_TOP="${ERL_TOP:-$(cd "$(dirname "$0")"/../../.. && pwd)}"
SRC="$ERL_TOP/lib/compiler/src"
SCRIPTS="$ERL_TOP/lib/compiler/scripts"
HOST_ERLC="$(command -v erlc || true)"

_abspath() { (cd "$1" 2>/dev/null && pwd -P) || printf '%s' "$1"; }
if [ -z "${HOST_ENV_DIR:-}" ]; then
    HASH="$(printf '%s' "$(_abspath "$ERL_TOP")" | sha1sum | cut -c1-10)"
    HOST_ENV_DIR="/tmp/host_env_${HASH}"
fi
TSRC="$HOST_ENV_DIR/tsrc"      # transformed sources + generated import headers
EGEN="$HOST_ENV_DIR/egen"      # generated core_parse.erl / beam_opcodes.*
EBIN="$HOST_ENV_DIR/ebin"      # the rig compiler

die() { echo "host_env: $*" >&2; exit 1; }

build() {
    [ -n "$HOST_ERLC" ] || die "no erlc on PATH"
    command -v python3 >/dev/null || die "no python3 on PATH (host_env_xform.py)"
    command -v perl >/dev/null || die "no perl on PATH (beam_makeops)"
    [ -f "$SRC/genop.tab" ] || die "not an OTP tree? $SRC/genop.tab missing"

    echo "=== host_env: scratch dir $HOST_ENV_DIR (host OTP: $("$HOST_ERLC" -version 2>/dev/null | head -1)) ==="
    rm -rf "$EGEN" "$EBIN"          # always build from scratch (tsrc is recreated below)
    mkdir -p "$EGEN" "$EBIN"

    echo "=== 1/4 transforming $SRC (read-only) -> $TSRC ==="
    python3 "$SCRIPTS/host_env_xform.py" "$SRC" "$TSRC" >&2

    echo "=== 2/4 generating src/core_parse.erl (yecc) and src/beam_opcodes.* ==="
    # beam_makeops is a plain perl script: no emulator build required.
    perl "$ERL_TOP/erts/emulator/utils/beam_makeops" -compiler \
         -outdir "$EGEN" "$SRC/genop.tab"
    "$HOST_ERLC" -o "$EGEN" "$SRC/core_parse.yrl"
    # Both are generated files of the `compiler' application, compiled as part of it.
    cp "$EGEN/core_parse.erl" "$EGEN/beam_opcodes.erl" "$EGEN/beam_opcodes.hrl" "$TSRC/"

    echo "=== 3/4 compiling the transformed sources with the host erlc ==="
    VSN="$(sed -n 's/^COMPILER_VSN[[:space:]]*=[[:space:]]*//p' \
                "$ERL_TOP/lib/compiler/vsn.mk" | tr -d '[:space:]')"
    [ -n "$VSN" ] || die "cannot read COMPILER_VSN from lib/compiler/vsn.mk"
    "$HOST_ERLC" -o "$EBIN" -I "$TSRC" -I "$ERL_TOP/lib/stdlib/include" \
        +inline -DCOMPILER_VSN="\"$VSN\"" "$TSRC"/*.erl
    echo "    compiled $(ls "$EBIN" | wc -l) beams (compiler-$VSN)"

    echo "=== 4/4 self-check: compile and run a probe with the rig compiler ==="
    probe="$HOST_ENV_DIR/probe.erl"
    cat > "$probe" <<'EOF'
-module(probe).
-export([f/1]).
f(L) -> lists:sum([X*2 || X <- L]).
EOF
    erl -noshell -pa "$EBIN" -eval "
        Opts = [return,binary,loop_opt,loop_arith_licm,fusion_opt,fbip_opt],
        case (catch compile:file(\"$probe\", Opts)) of
            R when is_tuple(R), element(1, R) =:= ok ->
                {module,_} = code:load_binary(element(2, R), \"probe\", element(3, R)),
                12 = probe:f([1,2,3]),
                io:format(\"    compiler: ~s~n    probe:f([1,2,3]) = 12~n\",
                          [code:which(compile)]);
            Other ->
                io:format(\"SELF-CHECK FAILED~n~P~n\", [Other, 12]),
                halt(1)
        end,
        halt()." || die "self-check failed"

    echo
    echo "RIG_EBIN=$EBIN"
    echo "Usage examples (add -I/{-i,Dir} for whatever the module under test includes):"
    echo "  $0 shell                             # erl, rig compiler first on the path"
    echo "  erl -noshell -pa $EBIN -eval 'R = compile:file(\"m.erl\", [{i,\"lib/stdlib/include\"},"
    echo "      return,binary,loop_opt,loop_arith_licm,fusion_opt,fbip_opt]), io:format(\"~p~n\",[element(1,R)]), halt().'"
    echo "See lib/compiler/scripts/CONTEXT.md for the full recipe and caveats."
}

shell() {
    [ -d "$EBIN" ] || die "not built yet: run '$0 build' first"
    exec erl -pa "$EBIN" "$@"
}

cleandir() { rm -rf "$HOST_ENV_DIR"; echo "removed $HOST_ENV_DIR"; }

case "${1:-}" in
    build)    build ;;
    shell)    shift; shell "$@" ;;
    cleandir) cleandir ;;
    *)        sed -n '23,38p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
