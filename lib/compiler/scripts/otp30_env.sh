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
# Build and test the `compiler' application in an otherwise unbuilt OTP tree.
#
# This tree needs an OTP-30 runtime: the checked-in primary bootstrap
# (`bootstrap/lib/compiler`) is compiled for OTP 30 opcodes that the host
# `erl` (if it is older) cannot load, and an in-tree compiler that is compiled
# by an older compiler gets an incompatible record representation. The script
# therefore builds the OTP-30 emulator first, then uses the primary bootstrap
# on top of it to build kernel/stdlib/compiler, and finally builds
# `common_test` so the CT suites can run on the same (OTP-30) runtime.
#
# Usage (from the repository root, i.e. $ERL_TOP):
#     lib/compiler/scripts/otp30_env.sh setup
#     lib/compiler/scripts/otp30_env.sh test <SUITE> [<SUITE> ...]
#     lib/compiler/scripts/otp30_env.sh shell
#
# The `test' subcommand compiles test_lib, the named suites and the
# `ts_install_cth' hook with the in-tree compiler, then runs them under
# Common Test with `lib/compiler/ebin' first on the code path, so the
# in-tree compiler is the one under test.
#
# NOTE: not installed, not part of `SUB_DIRECTORIES'; a development helper
# for constrained environments that have no prebuilt OTP-30 available.

set -e

ERL_TOP="${ERL_TOP:-$(cd "$(dirname "$0")"/../../.. && pwd)}"
export ERL_TOP
# Always use a bash that exists here: the ambient CONFIG_SHELL may point at a
# stale store path, which would make the generated `configure' scripts fail.
export CONFIG_SHELL="$(command -v bash)"
cd "$ERL_TOP"
JOBS="$(nproc 2>/dev/null || echo 4)"

TARGET_DIR="$ERL_TOP/make/x86_64-pc-linux-gnu"
BIN="$ERL_TOP/bootstrap/bin"
COMPILER_EBIN="$ERL_TOP/lib/compiler/ebin"
TESTDIR="$ERL_TOP/lib/compiler/test"
LOGDIR="${LOGDIR:-$ERL_TOP/lib/compiler/scripts/ct_logs}"
# Compiled test modules go next to the suites (as the app's own test Makefile
# does: EBIN = .); Common Test locates a suite by its module beam in that dir.
# *.beam is ignored by the global .gitignore, so this never dirties the tree.
CTBIN="$TESTDIR"

setup() {
    # 1. Per-directory autoconf. `make/configure' produces the top-level
    #    Makefile and output.mk; `erts/configure' produces make/$(TARGET)/otp.mk
    #    (which every application Makefile includes).
    [ -f "$ERL_TOP/make/output.mk" ] || (cd "$ERL_TOP/make" && ./configure)
    [ -f "$TARGET_DIR/otp.mk" ] || (cd "$ERL_TOP/erts" && ./configure --without-termcap)
    # erl_interface provides `erl_call'; the emulator build installs it.
    [ -f "$ERL_TOP/lib/erl_interface/src/x86_64-pc-linux-gnu/Makefile" ] || \
        (cd "$ERL_TOP/lib/erl_interface" && ./configure)

    echo "=== building erl_interface, emulator, bootstrap wrappers ==="
    make -j"$JOBS" erl_interface
    make -j"$JOBS" emulator bootstrap_setup
    # Build kernel, stdlib and compiler in-tree with the primary bootstrap
    # compiler (which now runs on the freshly built OTP-30 emulator). The
    # `primary_bootstrap' target is deliberately NOT used: it also rewrites
    # the version-controlled files under bootstrap/, which must stay pristine.
    echo "=== building kernel, stdlib, compiler with the primary bootstrap ==="
    PATH="$BIN:$PATH" make -j"$JOBS" -C "$ERL_TOP/lib/kernel" opt
    PATH="$BIN:$PATH" make -j"$JOBS" -C "$ERL_TOP/lib/stdlib" opt
    PATH="$BIN:$PATH" make -j"$JOBS" -C "$ERL_TOP/lib/compiler" opt

    echo "=== building common_test ==="
    [ -f "$ERL_TOP/lib/common_test/priv/x86_64-pc-linux-gnu/Makefile" ] || \
        (cd "$ERL_TOP/lib/common_test" && ./configure)
    make -j"$JOBS" -C "$ERL_TOP/lib/common_test"
    echo "=== setup done ==="
}

test_suites() {
    [ $# -gt 0 ] || { echo "usage: $0 test <SUITE> [<SUITE> ...]" >&2; exit 2; }
    rm -rf "$LOGDIR"
    mkdir -p "$LOGDIR"
    export ERL_LIBS="$ERL_TOP/lib"
    local suites=()
    for s in "$@"; do suites+=("$TESTDIR/$s.erl"); done
    "$BIN/erlc" -o "$CTBIN" \
        -I "$ERL_TOP/lib/compiler/src" -I "$ERL_TOP/lib/stdlib/include" \
        +clint +ssalint +nowarn_missing_spec_documented \
        "$TESTDIR/test_lib.erl" "${suites[@]}"
    "$BIN/erlc" -o "$CTBIN" "$ERL_TOP/lib/common_test/test_server/ts_install_cth.erl"
    local suites_arg
    suites_arg=$(printf '"%s",' "$@"); suites_arg="[${suites_arg%,}]"
    "$BIN/erl" -pa "$COMPILER_EBIN" -pa "$CTBIN" -noshell -eval "
        R = ct:run_test([{dir,\"$TESTDIR\"},
                         {suite,$suites_arg},
                         {logdir,\"$LOGDIR\"},
                         {auto_compile,false}]),
        io:format(\"RESULT: ~p~n\",[R]),
        case R of {_,0,_} -> halt(0); _ -> halt(1) end."
}

shell() {
    export ERL_LIBS="$ERL_TOP/lib"
    exec "$BIN/erl" -pa "$COMPILER_EBIN" -pa "$CTBIN" "$@"
}

case "${1:-}" in
    setup) setup ;;
    test)  shift; test_suites "$@" ;;
    shell) shift; shell "$@" ;;
    *) echo "usage: $0 {setup|test <SUITE>...|shell}" >&2; exit 2 ;;
esac
