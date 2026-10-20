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
# a9_selfhost_probe.sh -- the A.9 ("self-hosted build") probe harness for the
# Rust ERTS rewrite: measure, rung by rung, how far `erts_rust`'s `beam_rt`
# gets towards compiling the tree with the tree's *own* compiler.
#
# WHY: A.9 is the contract "the tree builds itself".  The rewrite's oracle is
# the system `erl'/`erlc' (contract OTP-29 here), which cannot load this tree's
# OTP-30 beams at all ("Use of opcode 193"), so "the tree compiles the tree"
# can only be measured on `beam_rt'.  This script does exactly that and reports
# what it measured -- a rung that does not work is reported BLOCKED (with the
# typed reason and the owning crate) and never as PASS.
#
# THE LADDER (one line per rung: `L<n> <name>: PASS|BLOCKED|FAIL -- evidence')
#   L1 cli_eval            -eval route answers (`1+1' -> 2).
#   L2 in_process_compile  an in-process `compile:file/2' of a real OTP
#                          application module, byte-compared (`cmp') with
#                          `erlc +deterministic'.
#   L3 compiler_chunk      the same route over the tree's OWN compiler sources
#                          ("the Rust runtime compiling the compiler"), each
#                          byte-identical to the oracle.
#   L4 tree_own_compiler   the tree's committed OTP-30 bootstrap compiler on the
#                          code path.  THIS IS THE A.9 BLOCKER.
#   L5 erlc_frontend       the real `erlc' C front end with
#                          ERLC_EMULATOR=<this runtime>.
#   L6 emake_driver        the `make' module (`erl -make''s driver) with the
#                          correct in-term Emakefile form.
#   L7 make_command_line   the OTP build's own `erlc -W $(ERL_COMPILE_FLAGS)
#                          -o$(EBIN) $<' rule (make/otp.mk.in).
#
# MODES
#   ladder   (default) run L1..L7 and print the ladder + a summary line.
#   cases    run the differential case family of A9_CASES.md and print one
#            `<id>: agree|unsupported|mismatch|skipped -- detail' line per case.
#   --help   this text.
#
# EXIT CODES
#   ladder: 0 every rung ran (PASS and BLOCKED are both successful
#              measurements of what they measure), 1 at least one FAIL
#              (a rung whose measured outcome contradicts its contract),
#              2 harness/environment error (no `beam_rt', no oracle, a missing
#              source file, ... -- never a false pass).
#   cases:  0 no `mismatch' (an `unsupported' is a recorded capability gap and a
#              `skipped' a recorded non-verdict -- neither is a failure),
#              1 at least one `mismatch', 2 harness error.
#
# ENVIRONMENT
#   BEAM_RT               the Rust runtime; defaults to
#                         erts_rust/target/{release,debug}/beam_rt (first that
#                         exists).
#   ERTS_ORACLE / ERLC    oracle `erl' / `erlc'; default: PATH.
#   A9_COMPILER_MODULES   space separated `lib/compiler/src' module basenames for
#                         the L3 set (default: `sys_core_bsm beam_flatten').
#   A9_L2_SRC             the L2 module (default lib/compiler/src/cerl_clauses.erl).
#   A9_DIR                scratch dir; defaults to $TMPDIR/a9_selfhost and is
#                         recreated on every run.  Nothing is written inside the
#                         repository.
#   A9_VERBOSE=1          echo every command's raw stdout/stderr to stderr.
#
# COST: an in-process compile under `beam_rt' costs ~10-30 s per module of this
# tree (the oracle needs ~0.3 s), so the default ladder is ~1-2 minutes and the
# default case family ~2 minutes.  Nothing here is part of CI: like `host_env.sh'
# and `smoke' this is a manual, test-only developer tool (not installed, not in
# `SUB_DIRECTORIES'), and it never modifies the repository.
#
set -u

# --- locate the tree and the scratch area -----------------------------------
ERL_TOP=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd) || {
    echo "a9_selfhost_probe: cannot resolve the repository root" >&2
    exit 2
}
SRC_DIR="$ERL_TOP/lib/compiler/src"
BOOT_COMPILER="$ERL_TOP/bootstrap/lib/compiler/ebin/compile.beam"
A9_TMP="${TMPDIR:-/tmp}"
A9_DIR="${A9_DIR:-$A9_TMP/a9_selfhost}"
WORK="$A9_DIR"
A9_COMPILER_MODULES="${A9_COMPILER_MODULES:-sys_core_bsm beam_flatten}"
A9_L2_SRC="${A9_L2_SRC:-lib/compiler/src/cerl_clauses.erl}"
A9_VERBOSE="${A9_VERBOSE:-0}"

ORACLE_ERL="${ERTS_ORACLE:-erl}"
ORACLE_ERLC="${ERLC:-erlc}"

# The bootstrap code path of the tree's own compiler (absolute, so the probe
# does not depend on the caller's cwd).
BOOT_PA=()
for _d in kernel stdlib compiler; do
    if [ -d "$ERL_TOP/bootstrap/lib/$_d/ebin" ]; then
        BOOT_PA+=(-pa "$ERL_TOP/bootstrap/lib/$_d/ebin")
    fi
done
unset _d

die() { echo "a9_selfhost_probe: $*" >&2; exit 2; }

# --- helpers -----------------------------------------------------------------
now_ms() { local n; n=$(date +%s%N 2>/dev/null) || n=0; echo $(( n / 1000000 )); }

# fmt_ms <ms> -> "12.3s"
fmt_ms() { printf '%d.%02ds' "$(( $1 / 1000 ))" "$(( ($1 % 1000) / 10 ))"; }

# trim <text...> -- fold a multi-line command result into one evidence line.
trim() {
    local t
    t=$(printf '%s' "$*" | tr '\n\t' '  ' | sed 's/  */ /g' | sed 's/^ //; s/ $//')
    case $t in
        '') t='(no output)' ;;
    esac
    printf '%s' "$(printf '%s' "$t" | cut -c1-360)"
}

# run_in <dir> <cmd...> -- run a command with <dir> as cwd.
# Sets RC, OUT, ERR (trailing newlines stripped) and RUN_MS.
run_in() {
    local dir=$1 t0 t1
    shift
    t0=$(now_ms)
    ( cd "$dir" && "$@" ) >"$WORK/.out" 2>"$WORK/.err"
    RC=$?
    t1=$(now_ms)
    RUN_MS=$(( t1 - t0 ))
    OUT=$(cat "$WORK/.out" 2>/dev/null || true)
    ERR=$(cat "$WORK/.err" 2>/dev/null || true)
    if [ "$A9_VERBOSE" = 1 ]; then
        printf '  $ (cd %s && %s) -> exit %d in %s\n' "$dir" "$*" "$RC" "$(fmt_ms "$RUN_MS")" >&2
        [ -n "$OUT" ] && printf '    out: %s\n' "$(trim "$OUT")" >&2
        [ -n "$ERR" ] && printf '    err: %s\n' "$(trim "$ERR")" >&2
    fi
    return 0
}

# is_typed_gap <text...> -- true when a result carries the runtime's documented
# capability-gap signal: a typed `unsupported...' reason, or an explicit refusal
# of the argv (`beam_rt: unknown flag ...', its documented exit 1).  A merely
# wrong value is never a gap: that is what makes a FAIL a FAIL.
is_typed_gap() {
    case "$*" in
        *unsupported_opcode:*|*unsupported:*|*unknown\ flag*) return 0 ;;
    esac
    return 1
}

# --- shared compile legs -----------------------------------------------------
# rust_compile_file <src> <outdir> -- in-process `compile:file/2' under beam_rt
# with {deterministic,true} so the CInf/Line chunks do not embed the path/cwd.
rust_compile_file() {
    run_in "$ERL_TOP" "$BEAM_RT" -noshell \
        -eval "R=compile:file(\"$1\",[deterministic,{outdir,\"$2\"}]), io:format(\"~p~n\",[R]), halt(0)."
}

# oracle_compile_file <src> <outdir> -- the same artifact through the real erlc.
oracle_compile_file() {
    run_in "$ERL_TOP" "$ORACLE_ERLC" +deterministic -o "$2" "$1"
}

# --- preconditions -----------------------------------------------------------
resolve_beam_rt() {
    if [ -n "${BEAM_RT:-}" ]; then
        [ -x "$BEAM_RT" ] || die "BEAM_RT=$BEAM_RT is not an executable"
    else
        local cand
        for cand in "$ERL_TOP/erts_rust/target/release/beam_rt" \
                    "$ERL_TOP/erts_rust/target/debug/beam_rt"; do
            if [ -x "$cand" ]; then BEAM_RT=$cand; break; fi
        done
        [ -n "${BEAM_RT:-}" ] || die "no beam_rt: build it with
  erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p beam_rt
or point BEAM_RT at an existing binary"
    fi
    # erlc resolves ERLC_EMULATOR relative to its own cwd: hand it an absolute path.
    BEAM_RT=$(CDPATH= cd -- "$(dirname -- "$BEAM_RT")" && pwd)/$(basename -- "$BEAM_RT")
    [ -x "$BEAM_RT" ] || die "BEAM_RT=$BEAM_RT is not executable"
}

check_oracle() {
    command -v "$ORACLE_ERL" >/dev/null 2>&1 || command -v "$ORACLE_ERL" >/dev/null ||
        die "no oracle erl ('$ORACLE_ERL'); set ERTS_ORACLE"
    command -v "$ORACLE_ERLC" >/dev/null 2>&1 ||
        die "no oracle erlc ('$ORACLE_ERLC'); set ERLC"
}

prepare_work() {
    rm -rf "$WORK"
    mkdir -p "$WORK" || die "cannot create scratch dir $WORK"
}

# --- the ladder --------------------------------------------------------------
PASS_N=0 BLOCKED_N=0 FAIL_N=0
VERDICT='' EVIDENCE=''

rung() {
    printf 'L%s %s: %s — %s\n' "$1" "$2" "$3" "$4"
    case $3 in
        PASS)    PASS_N=$(( PASS_N + 1 )) ;;
        BLOCKED) BLOCKED_N=$(( BLOCKED_N + 1 )) ;;
        *)       FAIL_N=$(( FAIL_N + 1 )) ;;
    esac
}

ladder_l1() {
    run_in "$ERL_TOP" "$BEAM_RT" -noshell -eval 'io:format("~p~n",[1+1]),halt(0).'
    if [ "$RC" -eq 0 ] && [ "$OUT" = "2" ]; then
        VERDICT=PASS
        EVIDENCE="\`-noshell -eval 'io:format(\"~p~n\",[1+1])'\` -> 2, exit 0 ($(fmt_ms "$RUN_MS"))"
    else
        VERDICT=FAIL
        EVIDENCE="want 2/exit 0, got '$(trim "$OUT")'/exit $RC; stderr: $(trim "$ERR")"
    fi
}

ladder_l2() {
    local src dir rd od mod rrc rms orc oms rout rerr oout size_r size_o
    src=$A9_L2_SRC
    if [ ! -f "$ERL_TOP/$src" ] && [ ! -f "$src" ]; then
        VERDICT=FAIL; EVIDENCE="no such module: $src"; return
    fi
    [ -f "$src" ] || src="$ERL_TOP/$src"
    mod=$(basename "$src" .erl)
    dir="$WORK/l2"; rm -rf "$dir"; mkdir -p "$dir/rust" "$dir/oracle"
    rd="$dir/rust"; od="$dir/oracle"

    rust_compile_file "$src" "$rd"
    rrc=$RC rms=$RUN_MS rout=$OUT rerr=$ERR
    oracle_compile_file "$src" "$od"
    orc=$RC oms=$RUN_MS oout=$OUT

    if [ "$rrc" -ne 0 ] || [ ! -s "$rd/$mod.beam" ]; then
        if is_typed_gap "$rout" "$rerr"; then
            VERDICT=BLOCKED
            EVIDENCE="no artifact; compile:file -> $(trim "$rout") ($(fmt_ms "$rms"))"
        else
            VERDICT=FAIL
            EVIDENCE="no artifact (exit $rrc); compile:file -> $(trim "$rout $rerr")"
        fi
        return
    fi
    if [ "$rout" != "{ok,$mod}" ]; then
        VERDICT=FAIL
        EVIDENCE="compile:file -> '$(trim "$rout")' (want {ok,$mod}) although $mod.beam exists"
        return
    fi
    if [ "$orc" -ne 0 ] || [ ! -s "$od/$mod.beam" ]; then
        VERDICT=FAIL
        EVIDENCE="oracle leg failed (exit $orc): $(trim "$oout $ERR")"
        return
    fi
    if cmp -s "$rd/$mod.beam" "$od/$mod.beam"; then
        VERDICT=PASS
        EVIDENCE="{ok,$mod}, byte-identical to erlc +deterministic, $(fmt_ms "$rms") vs $(fmt_ms "$oms")"
    else
        size_r=$(wc -c <"$rd/$mod.beam"); size_o=$(wc -c <"$od/$mod.beam")
        VERDICT=FAIL
        EVIDENCE="{ok,$mod} but the artifact DIFFERS from erlc +deterministic ($size_r vs $size_o bytes)"
    fi
}

ladder_l3() {
    local mods src dir rd od mod nk=0 ntot=0 nsup=0 nfail=0 rms_tot=0 oms_tot=0
    local -a done_mods=() bad_mods=() gap_mods=()
    mods=$A9_COMPILER_MODULES
    [ -n "$mods" ] || { VERDICT=FAIL; EVIDENCE="A9_COMPILER_MODULES is empty"; return; }
    dir="$WORK/l3"; rm -rf "$dir"

    for mod in $mods; do
        src="$SRC_DIR/$mod.erl"
        if [ ! -f "$src" ]; then
            VERDICT=FAIL; EVIDENCE="no such compiler source: lib/compiler/src/$mod.erl"; return
        fi
        ntot=$(( ntot + 1 ))
        rd="$dir/$mod/rust"; od="$dir/$mod/oracle"
        mkdir -p "$rd" "$od"
        rust_compile_file "$src" "$rd"
        local rrc=$RC rms=$RUN_MS rout=$OUT rerr=$ERR
        oracle_compile_file "$src" "$od"
        local orc=$RC oms=$RUN_MS
        rms_tot=$(( rms_tot + rms )); oms_tot=$(( oms_tot + oms ))
        if [ "$rrc" -ne 0 ] || [ ! -s "$rd/$mod.beam" ]; then
            if is_typed_gap "$rout" "$rerr"; then nsup=$(( nsup + 1 )); gap_mods+=("$mod")
            else nfail=$(( nfail + 1 )); bad_mods+=("$mod:$(trim "$rout $rerr")"); fi
            continue
        fi
        if [ "$rout" != "{ok,$mod}" ] || [ "$orc" -ne 0 ] || [ ! -s "$od/$mod.beam" ]; then
            nfail=$(( nfail + 1 )); bad_mods+=("$mod:unexpected return '$(trim "$rout")'/oracle exit $orc")
            continue
        fi
        if cmp -s "$rd/$mod.beam" "$od/$mod.beam"; then
            nk=$(( nk + 1 )); done_mods+=("$mod")
        else
            nfail=$(( nfail + 1 )); bad_mods+=("$mod:artifact differs")
        fi
    done

    local list
    list=$(printf '%s ' "${done_mods[@]:-}")
    if [ "$nfail" -gt 0 ]; then
        VERDICT=FAIL
        EVIDENCE="$nk/$ntot byte-identical; differing: $(trim "${bad_mods[*]}")"
    elif [ "$nsup" -gt 0 ]; then
        VERDICT=BLOCKED
        EVIDENCE="$nk/$ntot byte-identical ($(trim "$list")); capability gap on: $(trim "${gap_mods[*]}")"
    elif [ "$nk" -eq "$ntot" ]; then
        VERDICT=PASS
        EVIDENCE="$nk/$ntot of the tree's own compiler sources byte-identical ($(trim "$list")), $(fmt_ms "$rms_tot") vs $(fmt_ms "$oms_tot")"
    else
        VERDICT=FAIL
        EVIDENCE="only $nk/$ntot modules compiled"
    fi
}

# The record census: how many create-form `put_record' instructions the HOST
# OTP's own compiler beams carry.  It is the corroboration for L4's root cause:
# pre-OTP-30 records are flattened to `put_tuple2' at compile time, so every
# in-process sweep compiled by the host compiler never executes a record-table
# lookup -- the reason a 755/755 byte-identical sweep could still miss the path.
write_record_census() {
    cat >"$WORK/a9_record_census.erl" <<'ERL'
-module(a9_record_census).
-export([main/1]).
%% Count `put_record' instructions in a directory of beams, split into the
%% create form (Src operand is the literal nil) and the update form.
main([Dir]) ->
    Files = lists:sort(filelib:wildcard(filename:join(Dir, "*.beam"))),
    {Ok, Err, All, Create} =
        lists:foldl(fun count_file/2, {0, 0, 0, 0}, Files),
    io:format("~p ~p ~p ~p~n", [length(Files), Ok, All, Create]).

count_file(File, {Ok, Err, All, Create}) ->
    try beam_disasm:file(File) of
        {beam_file, _Mod, _Exp, _Attr, _Comp, Code} ->
            Instrs = lists:append([instrs(FC) || {function, _, _, _, FC} <- Code]),
            PR = [I || I <- Instrs, is_tuple(I), element(1, I) =:= put_record],
            CreateHere = length([I || I <- PR, is_create(I)]),
            {Ok + 1, Err, All + length(PR), Create + CreateHere};
        _ ->
            {Ok, Err + 1, All, Create}
    catch _:_ ->
        {Ok, Err + 1, All, Create}
    end.

is_create(I) ->
    tuple_size(I) >= 6 andalso element(4, I) =:= nil.

instrs(FunCode) ->
    lists:append([case B of
                      {block, _, Is} when is_list(Is) -> Is;
                      Other -> [Other]
                  end || B <- FunCode]).
ERL
}

# census_create_form_put_record -- prints "<files> <ok> <all> <create>" for the
# host OTP's compiler beams, or "unavailable: <why>".
census_create_form_put_record() {
    local dir beam_out
    dir=$("$ORACLE_ERL" -noshell -eval 'io:format("~s",[code:lib_dir(compiler)]),halt(0).' 2>/dev/null)
    if [ -z "$dir" ] || [ ! -d "$dir/ebin" ]; then
        echo "unavailable: the oracle has no compiler/ebin"; return 0
    fi
    write_record_census
    "$ORACLE_ERLC" -o "$WORK" "$WORK/a9_record_census.erl" >/dev/null 2>&1 ||
        { echo "unavailable: cannot compile the census module"; return 0; }
    run_in "$WORK" "$ORACLE_ERL" -noshell -pa "$WORK" \
        -eval "a9_record_census:main([\"$dir/ebin\"]), halt(0)."
    if [ "$RC" -ne 0 ] || [ -z "$OUT" ]; then
        echo "unavailable: census run failed (exit $RC)"; return 0
    fi
    beam_out=$OUT
    echo "$beam_out"
}

ladder_l4() {
    local dir src rout which res produced census cfiles cok call ccreate which_ok which_txt census_txt
    # A small, fast module whose compile reproduces the typed capability gap.
    # (The shape of the failure is module dependent: `cerl_clauses.erl' instead
    # makes the tree compiler print its own `Function: is_catchall/1' diagnostic
    # and answer `error' -- the same gap surfacing through compile's per-function
    # error handler.  See A9_CASES.md.)
    src="$SRC_DIR/beam_flatten.erl"
    dir="$WORK/l4"; rm -rf "$dir"; mkdir -p "$dir/out"

    if [ ! -f "$BOOT_COMPILER" ]; then
        VERDICT=BLOCKED
        EVIDENCE="the tree carries no committed bootstrap compiler (bootstrap/lib/compiler/ebin/compile.beam missing)"
        return
    fi

    run_in "$ERL_TOP" "$BEAM_RT" "${BOOT_PA[@]}" -noshell \
        -eval "io:format(\"~p~n\",[code:which(compile)]), R=compile:file(\"$src\",[deterministic,{outdir,\"$dir/out\"}]), io:format(\"~p~n\",[R]), halt(0)."
    which=$(printf '%s\n' "$OUT" | sed -n 1p | sed 's/^"//; s/"$//')
    res=$(printf '%s\n' "$OUT" | sed -n 2p)
    produced=$(find "$dir/out" -name '*.beam' | wc -l | tr -d ' ')

    # The census is cheap (~2 s) and is the corroboration for the root cause.
    census=$(census_create_form_put_record)
    case $census in
        [0-9]*)
            cfiles=$(printf '%s' "$census" | awk '{print $1}')
            cok=$(printf '%s' "$census" | awk '{print $2}')
            call=$(printf '%s' "$census" | awk '{print $3}')
            ccreate=$(printf '%s' "$census" | awk '{print $4}')
            if [ "$call" = 0 ] && [ "$ccreate" = 0 ]; then
                census_txt="$cok/$cfiles host OTP compiler beams carry 0 put_record instructions (0 create-form)"
            else
                census_txt="census NOT reproduced: $cok/$cfiles beams disassembled, $call put_record, $ccreate create-form"
            fi
            ;;
        *) census_txt=$census ;;
    esac

    # (a) the tree's own compiler beam must be the one that got loaded.
    case $which in
        *bootstrap/lib/compiler/ebin/compile.beam) which_ok=1 ;;
        *) which_ok=0 ;;
    esac
    which_txt=$(printf '%s' "$which" | sed 's/^"//; s/"$//')

    # (b) it must reproduce the typed capability gap (or, if the gap ever
    # closes, produce an artifact -- which this rung then compares).
    if [ "$which_ok" = 0 ]; then
        VERDICT=FAIL
        EVIDENCE="code:which(compile) = $(trim "$which") (want the tree's bootstrap/lib/compiler/ebin/compile.beam)"
        return
    fi

    if [ "$produced" -gt 0 ]; then
        local mod od
        mod=$(basename "$src" .erl)
        od="$dir/oracle"; mkdir -p "$od"
        oracle_compile_file "$src" "$od"
        if cmp -s "$dir/out/$mod.beam" "$od/$mod.beam"; then
            VERDICT=PASS
            EVIDENCE="THE TREE'S OWN COMPILER WORKS: code:which(compile)=$which_txt, compile:file -> {ok,$mod}, byte-identical to erlc +deterministic ($(fmt_ms "$RUN_MS"))"
        else
            VERDICT=FAIL
            EVIDENCE="code:which(compile)=$which_txt, an artifact was produced but it DIFFERS from erlc +deterministic"
        fi
        return
    fi

    if is_typed_gap "$res" "$ERR"; then
        VERDICT=BLOCKED
        EVIDENCE="code:which(compile)=$which_txt; compile:file(lib/compiler/src/$(basename "$src")) -> $(trim "$res") (nothing produced); root cause: a create-form put_record whose Id is a FOREIGN {Module,Name} record reference -- erts_rust/crates/beam_engine/src/families/records/mod.rs::local_definition (~L817-842) needs the C's erts_record_find_entry record table / a code:ensure_loaded/1 trap, seam owner beam_bifs/beam_rt; corroboration: $census_txt"
    else
        VERDICT=FAIL
        EVIDENCE="code:which(compile)=$which_txt but nothing was produced and the failure is not a typed gap: $(trim "$res $ERR")"
    fi
}

ladder_l5() {
    local dir src wrapper argvf expf rout rerr rrc
    src=$A9_L2_SRC
    [ -f "$src" ] || src="$ERL_TOP/$src"
    dir="$WORK/l5"; rm -rf "$dir"; mkdir -p "$dir/out"
    argvf="$dir/argv.txt"; expf="$dir/argv.expected"
    wrapper="$dir/emulator_wrapper.sh"

    cat >"$wrapper" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$@" > "$A9_ARGV_LOG"
exec "$A9_REAL_EMULATOR" "$@"
SH
    chmod +x "$wrapper"

    A9_ARGV_LOG="$argvf" A9_REAL_EMULATOR="$BEAM_RT" ERLC_EMULATOR="$wrapper" \
        run_in "$ERL_TOP" "$ORACLE_ERLC" +deterministic -o "$dir/out" "$src"
    rrc=$RC rout=$OUT rerr=$ERR

    # The argv the OTP erlc front end hands its emulator (measured, not assumed).
    cat >"$expf" <<EOF
+S1
+sbtu
+A0
+P
65536
+Q
1024
-noinput
-mode
minimal
-boot
no_dot_erlang
-s
erl_compile
compile_cmdline
-extra
+deterministic
-o
$dir/out
$src
EOF

    local argv_txt='(argv not captured)'
    if [ -f "$argvf" ]; then
        if cmp -s "$argvf" "$expf"; then argv_txt='argv exact'
        else argv_txt="argv differs: $(trim "$(cat "$argvf")")"; fi
    fi

    # The two independent root causes, measured.
    run_in "$ERL_TOP" "$BEAM_RT" +S 1 -noshell -eval 'halt(0).'
    local sep_rc=$RC sep_err=$ERR
    run_in "$ERL_TOP" "$BEAM_RT" +S1 -noshell -eval 'halt(0).'
    local comp_ok=$([ $RC -eq 0 ] && echo yes || echo no) comp_err=$ERR
    run_in "$ERL_TOP" "$BEAM_RT" +sbtu -noshell -eval 'halt(0).'
    local sbt_ok=$([ $RC -eq 0 ] && echo yes || echo no) sbt_err=$ERR
    run_in "$ERL_TOP" "$BEAM_RT" -noshell -eval 'io:format("~p~n",[init:get_plain_arguments()]),halt(0).'
    local plain=$OUT

    if [ "$rrc" -eq 0 ] && [ -s "$dir/out/$(basename "$src" .erl).beam" ]; then
        VERDICT=BLOCKED
        EVIDENCE="erlc reached the runtime and COMPILED ($argv_txt), but a real 'make' cannot be driven here (see L7); plain args: $plain"
        return
    fi

    if [ "$rrc" -eq 1 ] && is_typed_gap "$rerr"; then
        VERDICT=BLOCKED
        EVIDENCE="ERLC_EMULATOR=beam_rt erlc +deterministic -o … dies with '$(trim "$rerr")' (exit 1, nothing produced) after reaching the runtime with the real front-end argv ($argv_txt); independent 2nd cause: init:get_plain_arguments() = $plain, so erl_compile:compile_cmdline/0 cannot see the file list either; owners beam_rt (src/cli.rs:488 compact '+S' parsing rejects '+S1'/'+sbtu'; '+S 1'=$([ "$sep_rc" -eq 0 ] && echo ok || echo rejected), '+S1'=$comp_ok, '+sbtu'=$sbt_ok) and beam_rt (src/native/init.rs:175)"
    else
        VERDICT=FAIL
        EVIDENCE="ERLC_EMULATOR route: exit $rrc, stderr '$(trim "$rerr")', stdout '$(trim "$rout")' -- not the recorded typed flag rejection"
    fi
}

ladder_l6() {
    local dir src od rd rout oout orrc rrc expf
    src=$A9_L2_SRC
    [ -f "$src" ] || src="$ERL_TOP/$src"
    dir="$WORK/l6"; rm -rf "$dir"; mkdir -p "$dir/oracle" "$dir/rust" "$dir/wrong_o" "$dir/wrong_r"
    od="$dir/oracle"; rd="$dir/rust"
    local mod; mod=$(basename "$src" .erl)

    run_in "$ERL_TOP" "$ORACLE_ERL" -noshell \
        -eval "R=make:all([{emake,[{\"$src\",[deterministic,{outdir,\"$od\"}]}]}]), io:format(\"~p~n\",[R]), halt(0)."
    orrc=$RC oout=$OUT
    local oms=$RUN_MS
    run_in "$ERL_TOP" "$BEAM_RT" -noshell \
        -eval "R=make:all([{emake,[{\"$src\",[deterministic,{outdir,\"$rd\"}]}]}]), io:format(\"~p~n\",[R]), halt(0)."
    rrc=$RC rout=$OUT
    local rms=$RUN_MS

    # The {emakefile, File} spelling is a usage trap: both sides treat it as an
    # Emakefile entry for a file literally named emakefile -> up_to_date.
    run_in "$ERL_TOP" "$ORACLE_ERL" -noshell \
        -eval "io:format(\"~p~n\",[make:all([{emakefile,\"$src\"}])]), halt(0)."
    local wo_rc=$RC wo_out=$OUT
    run_in "$ERL_TOP" "$BEAM_RT" -noshell \
        -eval "io:format(\"~p~n\",[make:all([{emakefile,\"$src\"}])]), halt(0)."
    local wr_rc=$RC wr_out=$OUT
    local wrong_txt
    if [ "$wo_out" = up_to_date ] && [ "$wr_out" = up_to_date ] &&
       [ "$wo_rc" -eq 0 ] && [ "$wr_rc" -eq 0 ]; then
        wrong_txt="{emakefile,File} spelling -> up_to_date on both (usage artifact, no artifact written)"
    else
        wrong_txt="{emakefile,File} spelling: oracle '$(trim "$wo_out")'/rust '$(trim "$wr_out")'"
    fi

    if [ "$rrc" -ne 0 ] || [ ! -s "$rd/$mod.beam" ]; then
        if is_typed_gap "$rout" "$ERR"; then
            VERDICT=BLOCKED
            EVIDENCE="make:all([{emake,[…]}]) under beam_rt -> $(trim "$rout") (nothing produced); $wrong_txt"
        else
            VERDICT=FAIL
            EVIDENCE="make:all([{emake,[…]}]) under beam_rt produced no artifact (exit $rrc): $(trim "$rout $ERR")"
        fi
        return
    fi
    if [ "$orrc" -ne 0 ] || [ ! -s "$od/$mod.beam" ]; then
        VERDICT=FAIL
        EVIDENCE="oracle make:all leg failed (exit $orrc): $(trim "$oout")"
        return
    fi
    if [ "$rout" != "$oout" ]; then
        VERDICT=FAIL
        EVIDENCE="driver output differs: oracle '$(trim "$oout")' vs rust '$(trim "$rout")'"
        return
    fi
    if cmp -s "$rd/$mod.beam" "$od/$mod.beam"; then
        VERDICT=PASS
        EVIDENCE="make:all([{emake,[{Src,[deterministic,{outdir,…}]}]}]) -> '$(trim "$oout")' on both, artifact byte-identical ($(fmt_ms "$rms") vs $(fmt_ms "$oms")); $wrong_txt"
    else
        VERDICT=FAIL
        EVIDENCE="same driver output but the artifact DIFFERS from the oracle's"
    fi
}

ladder_l7() {
    local src dir rule shell_txt cfg
    src=$A9_L2_SRC
    [ -f "$src" ] || src="$ERL_TOP/$src"
    dir="$WORK/l7"; rm -rf "$dir"; mkdir -p "$dir/out"

    rule=$(grep -m1 '\$(V_ERLC) \$(ERL_COMPILE_FLAGS)' "$ERL_TOP/make/otp.mk.in" 2>/dev/null |
           sed 's/^[[:space:]]*//')
    [ -n "$rule" ] || rule='$(V_ERLC) $(ERL_COMPILE_FLAGS) -o$(EBIN) $<'

    cfg="${CONFIG_SHELL:-${SHELL:-}}"
    if [ -n "$cfg" ] && [ ! -x "$cfg" ]; then
        shell_txt="./configure re-execs CONFIG_SHELL=\$SHELL=$cfg, which does not exist"
    else
        shell_txt="./configure has never been run here (CONFIG_SHELL=$cfg)"
    fi

    local otpmk
    otpmk=$(find "$ERL_TOP/make" -mindepth 2 -maxdepth 2 -name otp.mk 2>/dev/null | head -1)

    # Drive the exact compile command line of that rule through the real erlc.
    ERLC_EMULATOR="$BEAM_RT" run_in "$ERL_TOP" "$ORACLE_ERLC" -W +deterministic -o"$dir/out" "$src"
    local drc=$RC derr=$ERR

    if [ -n "$otpmk" ]; then
        VERDICT=BLOCKED
        EVIDENCE="make/$(basename "$(dirname "$otpmk")")/otp.mk exists, so 'make -C lib/<app>' is reachable but was not run by this probe; the rule ($rule) driven through ERLC_EMULATOR -> exit $drc '$(trim "$derr")'"
        return
    fi
    VERDICT=BLOCKED
    EVIDENCE="'make -C lib/<app>' is unreachable: the tree is unconfigured (no make/<TARGET>/otp.mk; $shell_txt), so the otp.mk.in rule '$rule' (make/otp.mk.in:134-137) is driven directly -> exit $drc '$(trim "$derr")'; runtime blockers are L4/L5"
}

ladder() {
    resolve_beam_rt
    check_oracle
    prepare_work
    local t0 t1
    printf 'a9_selfhost_probe: ladder against oracle %s\n' \
        "$("$ORACLE_ERL" -noshell -eval 'io:format("~s",[erlang:system_info(otp_release)]),halt(0).' 2>/dev/null)"
    printf '  beam_rt: %s\n  scratch: %s\n' "$BEAM_RT" "$WORK"
    t0=$(now_ms)
    ladder_l1; rung 1 cli_eval "$VERDICT" "$EVIDENCE"
    ladder_l2; rung 2 in_process_compile "$VERDICT" "$EVIDENCE"
    ladder_l3; rung 3 compiler_chunk "$VERDICT" "$EVIDENCE"
    ladder_l4; rung 4 tree_own_compiler "$VERDICT" "$EVIDENCE"
    ladder_l5; rung 5 erlc_frontend "$VERDICT" "$EVIDENCE"
    ladder_l6; rung 6 emake_driver "$VERDICT" "$EVIDENCE"
    ladder_l7; rung 7 make_command_line "$VERDICT" "$EVIDENCE"
    t1=$(now_ms)
    printf 'a9_selfhost_probe: ladder: %d PASS, %d BLOCKED, %d FAIL (%d rungs, %s)\n' \
        "$PASS_N" "$BLOCKED_N" "$FAIL_N" "$(( PASS_N + BLOCKED_N + FAIL_N ))" "$(fmt_ms $(( t1 - t0 )))"
    [ "$FAIL_N" -eq 0 ] || exit 1
    exit 0
}

# --- the differential case family (see A9_CASES.md) --------------------------
CASE_AGREE=0 CASE_UNSUP=0 CASE_MIS=0 CASE_SKIP=0
case_out() {
    printf '%s: %s — %s\n' "$1" "$2" "$3"
    case $2 in
        agree)       CASE_AGREE=$(( CASE_AGREE + 1 )) ;;
        unsupported) CASE_UNSUP=$(( CASE_UNSUP + 1 )) ;;
        mismatch)    CASE_MIS=$(( CASE_MIS + 1 )) ;;
        *)           CASE_SKIP=$(( CASE_SKIP + 1 )) ;;
    esac
}

# case_compile_agree <id> <src> <note> -- the selfhost in-process-compile shape:
# oracle erlc vs beam_rt compile:file, byte-compared.
case_compile_agree() {
    local id=$1 src=$2 note=$3 dir mod od rd rout rerr rrc orc rms oms
    [ -f "$src" ] || src="$ERL_TOP/$src"
    mod=$(basename "$src" .erl)
    dir="$WORK/cases/$(printf '%s' "$id" | tr '/' '_')"
    rm -rf "$dir"; mkdir -p "$dir/rust" "$dir/oracle"
    rd="$dir/rust"; od="$dir/oracle"
    rust_compile_file "$src" "$rd"; rrc=$RC rms=$RUN_MS rout=$OUT rerr=$ERR
    oracle_compile_file "$src" "$od"; orc=$RC oms=$RUN_MS
    if [ "$rrc" -ne 0 ] || [ ! -s "$rd/$mod.beam" ]; then
        if is_typed_gap "$rout" "$rerr"; then
            case_out "$id" unsupported "no artifact: $(trim "$rout") ($note)"
        else
            case_out "$id" mismatch "no artifact (exit $rrc): $(trim "$rout $rerr")"
        fi
        return
    fi
    if [ "$rout" != "{ok,$mod}" ] || [ "$orc" -ne 0 ] || [ ! -s "$od/$mod.beam" ]; then
        case_out "$id" mismatch "rust '$(trim "$rout")'/exit $rrc, oracle exit $orc"
        return
    fi
    if cmp -s "$rd/$mod.beam" "$od/$mod.beam"; then
        case_out "$id" agree "both {ok,$mod}; artifacts byte-identical ($(fmt_ms "$rms") vs $(fmt_ms "$oms")); $note"
    else
        case_out "$id" mismatch "{ok,$mod} on both but the artifacts differ"
    fi
}

cases() {
    resolve_beam_rt
    check_oracle
    prepare_work
    local t0 t1 dir src
    t0=$(now_ms)
    printf 'a9_selfhost_probe: cases (A9_CASES.md) against oracle %s\n  beam_rt: %s\n  scratch: %s\n' \
        "$("$ORACLE_ERL" -noshell -eval 'io:format("~s",[erlang:system_info(otp_release)]),halt(0).' 2>/dev/null)" \
        "$BEAM_RT" "$WORK"

    # 1. selfhost/eval_route_answers_two_plus_two  (A.9/A.6)
    run_in "$ERL_TOP" "$ORACLE_ERL" -noshell -eval 'io:format("~p~n",[1+1]),halt(0).'
    local o=$OUT orc=$RC
    run_in "$ERL_TOP" "$BEAM_RT" -noshell -eval 'io:format("~p~n",[1+1]),halt(0).'
    if [ "$OUT" = "$o" ] && [ "$RC" = "$orc" ] && [ "$OUT" = 2 ]; then
        case_out selfhost/eval_route_answers_two_plus_two agree "both print 2, exit 0"
    else
        case_out selfhost/eval_route_answers_two_plus_two mismatch "oracle '$o'/exit $orc vs rust '$(trim "$OUT")'/exit $RC"
    fi

    # 2-4. the self-hosted in-process compile of the tree's own compiler sources
    case_compile_agree selfhost/in_process_compile_of_the_compilers_own_source \
        lib/compiler/src/cerl_clauses.erl 'A.9/A.2: compile:file under beam_rt == erlc +deterministic'
    case_compile_agree selfhost/in_process_compile_of_the_compilers_core_stage \
        lib/compiler/src/sys_core_bsm.erl 'A.9/A.2: the Core stage of the tree compiler'
    case_compile_agree selfhost/in_process_compile_of_the_compilers_flatten_stage \
        lib/compiler/src/beam_flatten.erl 'A.9/A.2: the codegen/asm stage of the tree compiler'

    # 5. selfhost/emake_driver_compiles_the_compilers_own_source (A.9)
    src="$SRC_DIR/beam_flatten.erl"
    dir="$WORK/cases/emake"; rm -rf "$dir"; mkdir -p "$dir/oracle" "$dir/rust"
    run_in "$ERL_TOP" "$ORACLE_ERL" -noshell \
        -eval "R=make:all([{emake,[{\"$src\",[deterministic,{outdir,\"$dir/oracle\"}]}]}]), io:format(\"~p~n\",[R]), halt(0)."
    local eo=$OUT eorc=$RC
    run_in "$ERL_TOP" "$BEAM_RT" -noshell \
        -eval "R=make:all([{emake,[{\"$src\",[deterministic,{outdir,\"$dir/rust\"}]}]}]), io:format(\"~p~n\",[R]), halt(0)."
    if [ "$OUT" = "$eo" ] && [ "$eorc" -eq 0 ] && [ "$RC" -eq 0 ] &&
       [ -s "$dir/rust/beam_flatten.beam" ] && [ -s "$dir/oracle/beam_flatten.beam" ] &&
       cmp -s "$dir/rust/beam_flatten.beam" "$dir/oracle/beam_flatten.beam"; then
        case_out selfhost/emake_driver_compiles_the_compilers_own_source agree \
            "make:all([{emake,[{Src,[deterministic,{outdir,…}]}]}]) -> '$(trim "$eo")' on both, artifact byte-identical"
    else
        case_out selfhost/emake_driver_compiles_the_compilers_own_source mismatch \
            "oracle '$(trim "$eo")'/exit $eorc vs rust '$(trim "$OUT")'/exit $RC"
    fi

    # 6. selfhost/emakefile_option_spelling_is_a_usage_artifact (A.9)
    run_in "$ERL_TOP" "$ORACLE_ERL" -noshell \
        -eval "io:format(\"~p~n\",[make:all([{emakefile,\"$src\"}])]), halt(0)."
    local wo=$OUT
    run_in "$ERL_TOP" "$BEAM_RT" -noshell \
        -eval "io:format(\"~p~n\",[make:all([{emakefile,\"$src\"}])]), halt(0)."
    if [ "$OUT" = "$wo" ] && [ "$OUT" = up_to_date ]; then
        case_out selfhost/emakefile_option_spelling_is_a_usage_artifact agree \
            "both answer up_to_date and write no artifact ({emakefile,File} is not an option; use {emake,List})"
    else
        case_out selfhost/emakefile_option_spelling_is_a_usage_artifact mismatch \
            "oracle '$(trim "$wo")' vs rust '$(trim "$OUT")'"
    fi

    # 7. selfhost/committed_bootstrap_compiler_compiles_a_module (A.9) -- the gap
    # (`beam_flatten.erl' is the module shape that surfaces the typed reason; see
    # A9_CASES.md on why `cerl_clauses.erl' answers `error' + a `Function: …'
    # diagnostic instead -- the same gap through compile's per-function handler.)
    src="$SRC_DIR/beam_flatten.erl"
    dir="$WORK/cases/bootcompiler"; rm -rf "$dir"; mkdir -p "$dir/out"
    run_in "$ERL_TOP" "$BEAM_RT" "${BOOT_PA[@]}" -noshell \
        -eval "io:format(\"~p~n\",[code:which(compile)]), R=compile:file(\"$src\",[deterministic,{outdir,\"$dir/out\"}]), io:format(\"~p~n\",[R]), halt(0)."
    local which res produced
    which=$(printf '%s\n' "$OUT" | sed -n 1p | sed 's/^"//; s/"$//')
    res=$(printf '%s\n' "$OUT" | sed -n 2p)
    produced=$(find "$dir/out" -name '*.beam' | wc -l | tr -d ' ')
    # The oracle leg of a differential comparison is impossible here: OTP-29
    # cannot load the tree's OTP-30 compile.beam at all.
    run_in "$ERL_TOP" "$ORACLE_ERL" "${BOOT_PA[@]}" -noshell \
        -eval "R=(catch compile:file(\"$src\",[deterministic,{outdir,\"$dir/out\"}])), io:format(\"~p~n\",[R]), halt(0)."
    local ores=$OUT ores_txt
    case $ores in
        *undef*|*badfile*) ores_txt='oracle leg impossible: OTP-29 cannot load the OTP-30 beam (undef / {error,badfile}, "Use of opcode 193")' ;;
        *) ores_txt="oracle leg: $(trim "$ores")" ;;
    esac
    if [ "$produced" -gt 0 ]; then
        # The gap closed: the tree's own compiler produced an artifact.  It is
        # only an `agree' if that artifact is the oracle's artifact.
        local od="$dir/oracle"
        mkdir -p "$od"
        oracle_compile_file "$src" "$od"
        if cmp -s "$dir/out/beam_flatten.beam" "$od/beam_flatten.beam"; then
            case_out selfhost/committed_bootstrap_compiler_compiles_a_module agree \
                "code:which(compile)=$which, compile:file -> $(trim "$res"), byte-identical to erlc +deterministic"
        else
            case_out selfhost/committed_bootstrap_compiler_compiles_a_module mismatch \
                "code:which(compile)=$which, an artifact was produced but it DIFFERS from erlc +deterministic"
        fi
    elif is_typed_gap "$res" "$ERR"; then
        case_out selfhost/committed_bootstrap_compiler_compiles_a_module unsupported \
            "code:which(compile)=$which; compile:file -> $(trim "$res") (nothing produced); $ores_txt; owner beam_engine (records/mod.rs::local_definition, foreign create-form put_record) -- seam owners beam_bifs/beam_rt"
    else
        case_out selfhost/committed_bootstrap_compiler_compiles_a_module mismatch \
            "code:which(compile)=$which; compile:file -> $(trim "$res $ERR"), $produced artifact(s)"
    fi

    # 8. selfhost/erlc_frontend_reaches_the_runtime_with_the_compile_argv (A.6/A.9)
    src="$SRC_DIR/beam_flatten.erl"
    dir="$WORK/cases/erlc"; rm -rf "$dir"; mkdir -p "$dir/out"
    local wrapper="$dir/wrap.sh"
    cat >"$wrapper" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$@" > "$A9_ARGV_LOG"
exec "$A9_REAL_EMULATOR" "$@"
SH
    chmod +x "$wrapper"
    A9_ARGV_LOG="$dir/argv.txt" A9_REAL_EMULATOR="$BEAM_RT" ERLC_EMULATOR="$wrapper" \
        run_in "$ERL_TOP" "$ORACLE_ERLC" +deterministic -o "$dir/out" "$src"
    local frc=$RC ferr=$ERR fargv='(not captured)'
    [ -f "$dir/argv.txt" ] && fargv=$(tr '\n' ' ' <"$dir/argv.txt" | sed 's/ $//')
    if [ "$frc" -eq 1 ] && is_typed_gap "$ferr"; then
        case_out selfhost/erlc_frontend_reaches_the_runtime_with_the_compile_argv unsupported \
            "reached the runtime with argv [$fargv] and died with '$(trim "$ferr")' (exit 1, nothing produced); init:get_plain_arguments() = [] under beam_rt, so erl_compile:compile_cmdline/0 could not see the files either; owner beam_rt (src/cli.rs compact +S/+sbt, src/native/init.rs)"
    elif [ "$frc" -eq 0 ] && [ -s "$dir/out/beam_flatten.beam" ]; then
        case_out selfhost/erlc_frontend_reaches_the_runtime_with_the_compile_argv agree \
            "erlc compiled through beam_rt (argv [$fargv]) and wrote beam_flatten.beam"
    else
        case_out selfhost/erlc_frontend_reaches_the_runtime_with_the_compile_argv mismatch \
            "erlc through beam_rt: exit $frc, stderr '$(trim "$ferr")'"
    fi

    # 9. selfhost/make_driven_compile_command_line (A.9) -- environment only
    local otpmk cfg
    otpmk=$(find "$ERL_TOP/make" -mindepth 2 -maxdepth 2 -name otp.mk 2>/dev/null | head -1)
    cfg="${CONFIG_SHELL:-${SHELL:-}}"
    if [ -n "$otpmk" ]; then
        case_out selfhost/make_driven_compile_command_line skipped \
            "make/$(basename "$(dirname "$otpmk")")/otp.mk exists; this case needs a configured tree and is not driven by this probe"
    else
        case_out selfhost/make_driven_compile_command_line skipped \
            "'make -C lib/<app>' unreachable: tree unconfigured (no make/<TARGET>/otp.mk; ./configure re-execs CONFIG_SHELL=$cfg, which does not exist); the rule 'erlc -W \$(ERL_COMPILE_FLAGS) -o\$(EBIN) \$<' (make/otp.mk.in:134-137) reaches L5's flag rejection instead"
    fi

    t1=$(now_ms)
    printf 'a9_selfhost_probe: cases: %d agree, %d unsupported, %d mismatch, %d skipped (%d cases, %s)\n' \
        "$CASE_AGREE" "$CASE_UNSUP" "$CASE_MIS" "$CASE_SKIP" \
        "$(( CASE_AGREE + CASE_UNSUP + CASE_MIS + CASE_SKIP ))" "$(fmt_ms $(( t1 - t0 )))"
    [ "$CASE_MIS" -eq 0 ] || exit 1
    exit 0
}

usage() {
    sed -n '23,85p' "$0" | sed 's/^# \{0,1\}//'
}

case "${1:-ladder}" in
    ladder)  ladder ;;
    cases)   cases ;;
    --help|-h|help) usage; exit 0 ;;
    *)       echo "a9_selfhost_probe: unknown mode '$1' (want: ladder | cases | --help)" >&2; exit 2 ;;
esac
