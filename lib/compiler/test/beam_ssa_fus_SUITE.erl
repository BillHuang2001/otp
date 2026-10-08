%%
%% %CopyrightBegin%
%%
%% SPDX-License-Identifier: Apache-2.0
%%
%% Copyright Ericsson AB 2026. All Rights Reserved.
%%
%% Licensed under the Apache License, Version 2.0 (the "License");
%% you may not use this file except in compliance with the License.
%% You may obtain a copy of the License at
%%
%%     http://www.apache.org/licenses/LICENSE-2.0
%%
%% Unless required by applicable law or agreed to in writing, software
%% distributed under the License is distributed on an "AS IS" BASIS,
%% WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
%% See the License for the specific language governing permissions and
%% limitations under the License.
%%
%% %CopyrightEnd%
%%
%% Tests for the opt-in "operation fusion" pass beam_ssa_fus/1.
%%
%% The pass is only added to the repeated SSA optimization passes when the
%% compile option `fusion_opt' is given (see beam_ssa_opt:repeated_passes/1),
%% and beam_ssa_opt:passes_1/2 derives the negation from that pass list, so
%% `no_ssa_opt_fus' turns it back into a no-op while `no_ssa_opt' skips the
%% whole beam_ssa_opt pass.
%%
%% Fusion 1 (the only transformation implemented) rewrites the last
%% instruction of a block
%%
%%     has_map_field(Map, Key) -> Bool
%%     br Bool, LSucc, LFail
%%
%% followed on the success edge by nothing but
%%
%%     get_map_element(Map, Key) -> GV
%%     succeeded:guard GV -> Bool2
%%     br Bool2, LSS, LFail2
%%
%% into a single `get_map_element' (which the code generator turns into one
%% `get_map_elements' BEAM instruction) with the failure edge redirected to
%% LFail. The `has_map_field' test disappears, i.e. the map is looked up once
%% instead of twice.
%%
%% The transformation requires the type analysis to have proven that Map is a
%% map at the test site (a `get_map_elements' instruction requires a map
%% operand, while `is_map_key/2' simply returns false for a non-map), so a
%% guard of the form `is_map(M), is_map_key(Key, M)' is what makes the pass
%% fire. Everything else is left untouched.
%%
%% Fusion 4 folds a multiplication by an integer constant whose result is
%% immediately multiplied by a second integer constant:
%%
%%     D1 = V * K1,  D2 = D1 * K2   ->   D2 = V * (K1 * K2)
%%
%% It requires the type analysis to have proven V to be an integer, which makes
%% the fold exact (`*' is associative for integers but not for floats) and keeps
%% the failure set unchanged (`badarith' cannot be reached for a proven
%% integer). The `is_integer/1' guard is what makes the pass fire: an unguarded
%% chain, a chain guarded by `is_float/1', or a chain interrupted by another
%% operation must stay untouched.
%%%% The test approach: a small probe module is compiled with each of the option
%% sets in opt_sets/0 and each build is executed. For every case we check
%%
%%   * that all the builds return exactly the same values *and* the same
%%     exceptions - the pass must be behaviour preserving, and
%%   * that the generated BEAM code has the expected shape, i.e. that the pass
%%     did (or did not) fire where it is supposed to.
%%
-module(beam_ssa_fus_SUITE).

-include_lib("stdlib/include/assert.hrl").

-export([all/0,suite/0,groups/0,init_per_suite/1,end_per_suite/1,
         init_per_group/2,end_per_group/2]).
-export([fuse_is_map_key/1,
         fuse_multi_key_guard/1,
         no_is_map_guard_no_fusion/1,
         no_fusion_multi_key_no_is_map/1,
         no_fusion_key_mismatch/1,
         multi_key_match/1,
         no_fusion_different_key/1,
         no_fusion_not_sole_success_edge/1,
         error_semantics/1,
         fuse_const_mul_chain/1,
         no_fusion_const_mul_chain/1]).

suite() ->
    [{ct_hooks,[ts_install_cth]}].

all() ->
    [{group,p}].

groups() ->
    [{p,[parallel],
      [fuse_is_map_key,
       fuse_multi_key_guard,
       no_is_map_guard_no_fusion,
       no_fusion_multi_key_no_is_map,
       no_fusion_key_mismatch,
       multi_key_match,
       no_fusion_different_key,
       no_fusion_not_sole_success_edge,
       error_semantics,
       fuse_const_mul_chain,
       no_fusion_const_mul_chain]}].

init_per_suite(Config) ->
    Config.

end_per_suite(_Config) ->
    ok.

init_per_group(_GroupName, Config) ->
    Config.

end_per_group(_GroupName, Config) ->
    Config.

%%============================================================================
%% Test cases
%%============================================================================

%% Fusion 1, positive case: the key test is redundant and must be removed,
%% while the extraction must survive as a single map lookup.
%%
%% `match/1' extracts the value with a map pattern and `case_expr/1' with a
%% `case', which are the two spellings that produce a `get_map_element'
%% instruction on the success edge of the test.
fuse_is_map_key(Config) ->
    Builds = build_probe(Config, "beam_ssa_fus_pos", pos_probe()),

    Args = [{[#{a => 1}], {ok,1}},
            {[#{a => x}], {ok,x}},
            {[#{}], {ok,no}},
            {[#{b => 1}], {ok,no}},
            {[[]], {ok,no}},
            {[42], {ok,no}}],
    check_calls(Builds, match, Args),
    check_calls(Builds, case_expr, Args),

    %% With the pass enabled the `has_map_field' test is gone, the extraction
    %% is still there, and the `is_map_key/2' guard is gone as well.
    Fusion = code(fusion, Builds),
    ?assert(has_instr(Fusion, get_map_elements)),
    ?assertNot(has_instr(Fusion, has_map_fields)),
    ?assertNot(has_instr(Fusion, is_map_key)),

    %% Without the pass - including with it explicitly turned off - the
    %% redundant test is still in the code.
    lists:foreach(fun(Set) ->
                          Code = code(Set, Builds),
                          ?assert(has_instr(Code, has_map_fields)),
                          ?assert(has_instr(Code, get_map_elements))
                  end, [plain,no_fus_plain,no_fus]),

    %% `no_ssa_opt' skips the SSA type analysis too, so `is_map_key/2' is
    %% still an ordinary BIF and there is no `has_map_field' to fuse.
    NoSsaOpt = code(no_ssa_opt, Builds),
    ?assert(has_instr(NoSsaOpt, is_map_key)),
    ?assertNot(has_instr(NoSsaOpt, has_map_fields)),
    ok.

%% Fusion 2, positive case: *several* `is_map_key/2' guards on the same map
%% are redundant when the body re-extracts exactly those keys with a map
%% pattern. With the pass enabled the whole `has_map_field' test chain is
%% replaced by the extraction chain, so the map is probed once (one multi-key
%% `get_map_elements') instead of once for the tests plus once for the
%% extractions.
fuse_multi_key_guard(Config) ->
    Builds = build_probe(Config, "beam_ssa_fus_mk", multikey_guard_probe()),

    Args2 = [{[#{a => 1, b => 2}], {ok,{1,2}}},
             {[#{a => 1, b => 2, c => 3}], {ok,{1,2}}},
             {[#{a => 1}], {ok,no}},
             {[#{b => 1}], {ok,no}},
             {[#{}], {ok,no}},
             {[[]], {ok,no}},
             {[42], {ok,no}}],
    check_calls(Builds, m2, Args2),

    Args3 = [{[#{a => 1, b => 2, c => 3}], {ok,{1,2,3}}},
             {[#{a => 1, b => 2, c => 3, d => 4}], {ok,{1,2,3}}},
             {[#{a => 1, b => 2}], {ok,no}},
             {[#{a => 1, c => 3}], {ok,no}},
             {[#{b => 1, c => 3}], {ok,no}},
             {[#{}], {ok,no}},
             {[[]], {ok,no}},
             {[42], {ok,no}}],
    check_calls(Builds, m3, Args3),

    Fusion = code(fusion, Builds),
    %% The test chain is gone, a single multi-key lookup remains, and the
    %% `is_map_key/2' guards are gone as well.
    ?assertNot(has_instr(Fusion, has_map_fields)),
    ?assertNot(has_instr(Fusion, is_map_key)),
    ?assertEqual([{atom,a},{atom,b}], get_map_keys(Fusion, m2)),
    ?assertEqual([{atom,a},{atom,b},{atom,c}], get_map_keys(Fusion, m3)),

    %% Without the pass - including with it explicitly turned off - the
    %% redundant multi-key test is still in the code, next to the extraction.
    lists:foreach(fun(Set) ->
                          Code = code(Set, Builds),
                          ?assert(has_instr(Code, has_map_fields)),
                          ?assert(has_instr(Code, get_map_elements))
                  end, [plain,no_fus_plain,no_fus]),

    %% `no_ssa_opt' skips the SSA type analysis too, so the `is_map_key/2'
    %% guards stay ordinary BIFs and there is no `has_map_field' to fuse.
    NoSsaOpt = code(no_ssa_opt, Builds),
    ?assert(has_instr(NoSsaOpt, is_map_key)),
    ?assertNot(has_instr(NoSsaOpt, has_map_fields)),
    ok.

%% The same shape without the `is_map/1' guard does *not* fuse: the first
%% `is_map_key/2' stays a plain BIF, and although the type analysis turns the
%% second test into a `has_map_field' (a `has_map_field' that returned true
%% implies the operand is a map), the operand of the *first* key test is not
%% proven to be a map, so substituting the lookup would raise `badmap' for a
%% non-map instead of simply taking the false edge. This is a strict no-op and
%% only asserts behavioural identity.
no_fusion_multi_key_no_is_map(Config) ->
    Builds = build_probe(Config, "beam_ssa_fus_mkng", multikey_noguard_probe()),

    Args = [{[#{a => 1, b => 2}], {ok,{1,2}}},
            {[#{a => 1, b => 2, c => 3}], {ok,{1,2}}},
            {[#{a => 1}], {ok,no}},
            {[#{b => 1}], {ok,no}},
            {[#{}], {ok,no}},
            {[[]], {ok,no}},
            {[42], {ok,no}}],
    check_calls(Builds, g, Args),

    Fusion = code(fusion, Builds),
    ?assertEqual(shape(code(plain, Builds)), shape(Fusion)),
    ?assert(has_instr(Fusion, is_map_key)),
    ?assert(has_instr(Fusion, has_map_fields)),
    ok.

%% The guards and the extraction must cover *exactly* the same keys. Here the
%% body extracts `c', which was never tested, so the test chain is not
%% redundant: the two lookups must both stay, and a missing `c' must still
%% raise the `badmatch' from the pattern (not fall through to the `no' clause).
no_fusion_key_mismatch(Config) ->
    Builds = build_probe(Config, "beam_ssa_fus_km", keymismatch_probe()),

    M1 = #{a => 1, b => 2},
    Args = [{[#{a => 1, b => 2, c => 3}], {ok,{1,3}}},
            {[M1], {error,{badmatch,M1}}},
            {[#{a => 1, c => 3}], {ok,no}},
            {[#{b => 1, c => 3}], {ok,no}},
            {[#{}], {ok,no}},
            {[[]], {ok,no}},
            {[42], {ok,no}}],
    check_calls(Builds, g, Args),

    Fusion = code(fusion, Builds),
    ?assertEqual(shape(code(plain, Builds)), shape(Fusion)),
    lists:foreach(fun(Set) ->
                          Code = code(Set, Builds),
                          ?assert(has_instr(Code, has_map_fields)),
                          ?assert(has_instr(Code, get_map_elements))
                  end, [fusion,plain,no_fus]),
    ok.

%% The same shape without the `is_map/1' guard does *not* fuse, and that is
%% expected rather than a limitation: the type analysis cannot know that the
%% operand of `is_map_key/2' is a map at the guard, so no `has_map_field'
%% instruction is created at all and there is nothing for the pass to fuse.
%% (Substituting a `get_map_elements' for the test would in fact be unsafe
%% here, because it would raise `badmap' instead of simply returning false.)
no_is_map_guard_no_fusion(Config) ->
    Builds = build_probe(Config, "beam_ssa_fus_noguard", noguard_probe()),

    Args = [{[#{a => 1}], {ok,1}},
            {[#{}], {ok,no}},
            {[#{b => 1}], {ok,no}},
            {[[]], {ok,no}},
            {[42], {ok,no}}],
    check_calls(Builds, noguard, Args),

    Fusion = code(fusion, Builds),
    %% A strict no-op: the code is unaffected by the pass.
    ?assertEqual(shape(code(plain, Builds)), shape(Fusion)),
    ?assert(has_instr(Fusion, is_map_key)),
    ?assertNot(has_instr(Fusion, has_map_fields)),
    ok.

%% A map pattern with several keys lowers to a single multi-key
%% `get_map_elements' instruction (`beam_block:opt_maps/2' coalesces the
%% per-key lookups), so there is no `has_map_field' test to fuse here. This
%% case only asserts behavioural identity: the same values and, for a failed
%% pattern match, the same `badmatch' exception in every build.
multi_key_match(Config) ->
    Builds = build_probe(Config, "beam_ssa_fus_multikey", multikey_probe()),

    Args = [{[#{a => 1, b => 2, c => 3}], {ok,{1,2,3}}},
            {[#{a => 1, b => 2, c => 3, d => 4}], {ok,{1,2,3}}},
            {[#{}], {error,{badmatch,#{}}}},
            {[#{b => 2, c => 3}], {error,{badmatch,#{b => 2, c => 3}}}},
            {[#{a => 1, c => 3}], {error,{badmatch,#{a => 1, c => 3}}}},
            {[#{a => 1, b => 2}], {error,{badmatch,#{a => 1, b => 2}}}},
            {[[]], {error,{badmatch,[]}}}],
    check_calls(Builds, m3, Args),

    %% No SSA level fusion may be claimed for this shape; the multi-key
    %% lookup is a BEAM level (beam_block) coalescing and is present with and
    %% without the pass.
    lists:foreach(fun({_Set, _Mod, _Bin, Code}) ->
                          ?assert(has_instr(Code, get_map_elements))
                  end, Builds),
    ok.

%% The test and the extraction use different keys, so the test is not
%% redundant and neither instruction may be removed.
no_fusion_different_key(Config) ->
    Builds = build_probe(Config, "beam_ssa_fus_diffkey", diffkey_probe()),

    Args = [{[#{a => 1, b => 2}], {ok,2}},
            {[#{a => 1}], {error,{badmatch,#{a => 1}}}},
            {[#{b => 1}], {ok,no}},
            {[#{}], {ok,no}},
            {[42], {ok,no}}],
    check_calls(Builds, g, Args),

    Fusion = code(fusion, Builds),
    ?assertEqual(shape(code(plain, Builds)), shape(Fusion)),
    ?assert(has_instr(Fusion, has_map_fields)),
    ?assert(has_instr(Fusion, get_map_elements)),
    ok.

%% The extraction is not the only thing on the success edge of the test (a
%% call happens first), so the success block cannot be dropped and the test
%% must be left alone.
no_fusion_not_sole_success_edge(Config) ->
    Builds = build_probe(Config, "beam_ssa_fus_indirect", indirect_probe()),

    M2 = #{a => 1, b => 2},
    Args = [{[#{a => 1}], {ok,{[a],1}}},
            {[M2], {ok,{maps:keys(M2),1}}},
            {[#{b => 1}], {ok,no}},
            {[#{}], {ok,no}},
            {[42], {ok,no}}],
    check_calls(Builds, indirect, Args),

    Fusion = code(fusion, Builds),
    ?assertEqual(shape(code(plain, Builds)), shape(Fusion)),
    ?assert(has_instr(Fusion, has_map_fields)),
    ?assert(has_instr(Fusion, get_map_elements)),
    ok.

%% `maps:get/2' is compiled to the `map_get/2' BIF (and never to a
%% `get_map_element' instruction), so this probe is not fused even though it
%% has an `is_map/1' guard. The BIF raises `{badmap,Term}' for a non-map, and
%% that exception must be identical in every build.
error_semantics(Config) ->
    Builds = build_probe(Config, "beam_ssa_fus_badmap", badmap_probe()),

    Args = [{[#{a => 1}], {ok,1}},
            {[#{}], {error,{badkey,a}}},
            {[42], {error,{badmap,42}}},
            {[[]], {error,{badmap,[]}}},
            {[[{a,1}]], {error,{badmap,[{a,1}]}}}],
    check_calls(Builds, e, Args),

    Fusion = code(fusion, Builds),
    ?assertEqual(shape(code(plain, Builds)), shape(Fusion)),
    lists:foreach(fun({_Set, _Mod, _Bin, Code}) ->
                          ?assert(has_instr(Code, map_get)),
                          ?assertNot(has_instr(Code, has_map_fields)),
                          ?assertNot(has_instr(Code, get_map_elements))
                  end, Builds),
    ok.

%%============================================================================
%% Fusion 4
%%============================================================================

%% Fusion 4, positive case: a chain of multiplications by integer constants is
%% folded into a single multiplication by the product of the constants. The
%% transformed code must contain one multiplication instead of two (three), and
%% every build must return exactly the same values *and* the same exceptions.
fuse_const_mul_chain(Config) ->
    Builds = build_probe(Config, "beam_ssa_fus_mul", mulchain_probe()),

    Args2 = [{[0],{ok,0}},
             {[1],{ok,6}},
             {[-1],{ok,-6}},
             {[7],{ok,42}},
             {[1 bsl 200],{ok,(1 bsl 200) * 2 * 3}},
             %% The guard is what makes the fusion safe, so a non-integer
             %% never reaches the multiplication.
             {[3.0],{error,function_clause}},
             {[a],{error,function_clause}}],
    check_calls(Builds, chain2, Args2),

    Args3 = [{[0],{ok,0}},
             {[1],{ok,24}},
             {[-2],{ok,-48}},
             {[5],{ok,120}},
             {[1 bsl 200],{ok,(1 bsl 200) * 2 * 3 * 4}},
             {[3.0],{error,function_clause}}],
    check_calls(Builds, chain3, Args3),

    Fusion = code(fusion, Builds),
    Plain = code(plain, Builds),
    %% With the pass enabled the two multiplications have been folded into one,
    %% and the three of the longer chain into one as well - and the single
    %% remaining multiplication is by the *product* of the constants of the
    %% chain (2*3 and 2*3*4), not by just one of them.
    ?assertEqual(1, count_instr(Fusion, chain2, '*')),
    ?assertEqual(1, count_instr(Fusion, chain3, '*')),
    ?assertEqual(6, mul_constant(Fusion, chain2)),
    ?assertEqual(24, mul_constant(Fusion, chain3)),

    %% Without the pass - including with it explicitly turned off - both (all
    %% three) multiplications are still there and nothing has been folded.
    lists:foreach(fun(Set) ->
                          Code = code(Set, Builds),
                          ?assertEqual(2, count_instr(Code, chain2, '*')),
                          ?assertEqual(3, count_instr(Code, chain3, '*'))
                  end, [plain,no_fus_plain,no_fus,no_ssa_opt]),
    ?assertEqual(2, count_instr(Plain, chain2, '*')),
    ok.

%% The chain must NOT be folded when the operand is not proven to be an
%% integer: without a guard the operand may be a float and `(X * 2) * 3' is not
%% `X * 6' in floating-point arithmetic, with an `is_float/1' guard the operand
%% is known to be a float, and an addition between the multiplications breaks
%% the chain. This is a strict no-op, and `badarith' must still be raised for a
%% non-number in every build.
no_fusion_const_mul_chain(Config) ->
    Builds = build_probe(Config, "beam_ssa_fus_mulneg", mulchain_neg_probe()),

    FloatArgs = [{[F],{ok,F * 2 * 3}} || F <- [0.1,1.0e-308,2.5e-10,1.5,-3.25]],    check_calls(Builds, unguarded, FloatArgs),
    check_calls(Builds, unguarded,
                [{[X],{ok,X * 2 * 3}} ||
                    X <- [0,1,-7,1 bsl 200]]),
    check_calls(Builds, unguarded,
                [{[a],{error,badarith}},
                 {[[]],{error,badarith}},
                 {[[1.0]],{error,badarith}}]),
    check_calls(Builds, float_operand, FloatArgs),
    check_calls(Builds, no_chain, [{[X],{ok,X * 2 + 3}} || X <- [0,1,-7]]),
    check_calls(Builds, add_const, [{[X],{ok,X + 1 + 2}} || X <- [0,1,-7]]),

    %% The pass is an exact no-op for this probe: with it enabled the
    %% disassembly is identical to the default build, operands included.
    %% Comparing the complete code (rather than only the instruction shape)
    %% also catches a rewrite that changed an operand without changing the
    %% instruction sequence.
    Plain = code(plain, Builds),
    Fusion = code(fusion, Builds),
    ?assertEqual(Plain, Fusion),

    %% The chains are really there, so the check above is not vacuous.
    %% `count_mul/2' counts both the `*' BIF emitted for an integer operand and
    %% the `fmul' instruction emitted for a float operand - a float
    %% multiplication is not a generic `*' - so the source-level number of
    %% multiplications can be pinned without hard-coding which instruction a
    %% given operand type happens to produce. The pass must leave that number
    %% unchanged.
    lists:foreach(fun({Fun, Expected}) ->
                          ?assertEqual({Fun, Expected},
                                       {Fun, count_mul(Fusion, Fun)}),
                          ?assertEqual({Fun, Expected},
                                       {Fun, count_mul(Plain, Fun)})
                  end, [{unguarded, 2}, {float_operand, 2}, {no_chain, 1}]),
    ok.

%%============================================================================
%% Probe modules
%%============================================================================
%% The `is_map/1' guard is what makes the pass fire: it lets the type analysis
%% prove that the operand of `is_map_key/2' is a map, which turns the guard
%% into a `has_map_field/2' SSA test. The extraction that follows it on the
%% success edge is exactly what the pass looks for.
pos_probe() ->
    "-module(beam_ssa_fus_pos).\n"
    "-export([match/1,case_expr/1]).\n"
    "\n"
    "match(M) when is_map(M), is_map_key(a, M) ->\n"
    "    #{a := V} = M,\n"
    "    V;\n"
    "match(_) ->\n"
    "    no.\n"
    "\n"
    "case_expr(M) when is_map(M), is_map_key(a, M) ->\n"
    "    case M of\n"
    "        #{a := V} -> V\n"
    "    end;\n"
    "case_expr(_) ->\n"
    "    no.\n".

%% No `is_map/1' guard: the `is_map_key/2' guard stays a plain BIF and no
%% `has_map_field' is created, so the pass cannot fire.
noguard_probe() ->
    "-module(beam_ssa_fus_noguard).\n"
    "-export([noguard/1]).\n"
    "\n"
    "noguard(M) when is_map_key(a, M) ->\n"
    "    #{a := V} = M,\n"
    "    V;\n"
    "noguard(_) ->\n"
    "    no.\n".

multikey_probe() ->
    "-module(beam_ssa_fus_multikey).\n"
    "-export([m3/1]).\n"
    "\n"
    "m3(M) ->\n"
    "    #{a := A, b := B, c := C} = M,\n"
    "    {A, B, C}.\n".

%% Test and extraction use different keys.
diffkey_probe() ->
    "-module(beam_ssa_fus_diffkey).\n"
    "-export([g/1]).\n"
    "\n"
    "g(M) when is_map(M), is_map_key(a, M) ->\n"
    "    #{b := V} = M,\n"
    "    V;\n"
    "g(_) ->\n"
    "    no.\n".

%% Something else happens on the success edge of the test before the
%% extraction.
indirect_probe() ->
    "-module(beam_ssa_fus_indirect).\n"
    "-export([indirect/1]).\n"
    "\n"
    "indirect(M) when is_map(M), is_map_key(a, M) ->\n"
    "    L = maps:keys(M),\n"
    "    #{a := V} = M,\n"
    "    {L, V};\n"
    "indirect(_) ->\n"
    "    no.\n".

%% Several `is_map_key/2' guards on the same map plus a body that re-extracts
%% exactly those keys: the whole test chain is redundant (Fusion 2).
multikey_guard_probe() ->
    "-module(beam_ssa_fus_mk).\n"
    "-export([m2/1,m3/1]).\n"
    "\n"
    "m2(M) when is_map(M), is_map_key(a, M), is_map_key(b, M) ->\n"
    "    #{a := A, b := B} = M,\n"
    "    {A, B};\n"
    "m2(_) ->\n"
    "    no.\n"
    "\n"
    "m3(M) when is_map(M), is_map_key(a, M), is_map_key(b, M), is_map_key(c, M) ->\n"
    "    #{a := A, b := B, c := C} = M,\n"
    "    {A, B, C};\n"
    "m3(_) ->\n"
    "    no.\n".

%% The same multi-key guard idiom without the `is_map/1' guard: the operand is
%% not proven to be a map at the first key test, so nothing may be fused.
multikey_noguard_probe() ->
    "-module(beam_ssa_fus_mkng).\n"
    "-export([g/1]).\n"
    "\n"
    "g(M) when is_map_key(a, M), is_map_key(b, M) ->\n"
    "    #{a := A, b := B} = M,\n"
    "    {A, B};\n"
    "g(_) ->\n"
    "    no.\n".

%% Guard keys {a,b} but the body extracts {a,c}: the test chain is not
%% redundant, so neither lookup may be removed.
keymismatch_probe() ->
    "-module(beam_ssa_fus_km).\n"
    "-export([g/1]).\n"
    "\n"
    "g(M) when is_map(M), is_map_key(a, M), is_map_key(b, M) ->\n"
    "    #{a := A, c := C} = M,\n"
    "    {A, C};\n"
    "g(_) ->\n"
    "    no.\n".

%% `maps:get/2' on an operand of unknown type: the mismatch of the failure
%% reasons (`{badmap,Term}' here, "false" for `is_map_key/2') is exactly why
%% the pass only fuses when the operand is known to be a map.
badmap_probe() ->
    "-module(beam_ssa_fus_badmap).\n"
    "-export([e/1]).\n"
    "\n"
    "e(M) -> maps:get(a, M).\n".

%% Chains of multiplications by integer constants under an `is_integer/1'
%% guard, which is what lets the type analysis prove the operand to be an
%% integer (Fusion 4).
mulchain_probe() ->
    "-module(beam_ssa_fus_mul).\n"
    "-export([chain2/1,chain3/1]).\n"
    "\n"
    "chain2(X) when is_integer(X) -> X * 2 * 3.\n"
    "\n"
    "chain3(X) when is_integer(X) -> X * 2 * 3 * 4.\n".

%% The same chains without a type proof (no guard at all, or a float guard), and
%% chains that are not multiplication chains: none of these may be folded.
mulchain_neg_probe() ->
    "-module(beam_ssa_fus_mulneg).\n"
    "-export([unguarded/1,float_operand/1,add_const/1,no_chain/1]).\n"
    "\n"
    "unguarded(X) -> X * 2 * 3.\n"
    "\n"
    "float_operand(X) when is_float(X) -> X * 2 * 3.\n"
    "\n"
    "add_const(X) when is_integer(X) -> X + 1 + 2.\n"
    "\n"
    "no_chain(X) when is_integer(X) -> X * 2 + 3.\n".

%%============================================================================
%% Compiling, disassembling and running the probes
%%============================================================================
%% The option sets every probe is compiled with.
%%
%% `fusion' enables the pass, `plain' is the default, `no_fus' enables the
%% pass and then kills it again with the negation that
%% beam_ssa_opt:passes_1/2 derives from the pass list, and `no_ssa_opt' skips
%% the whole beam_ssa_opt pass. `no_fus_plain' is the negation without the
%% enabling option, which is accepted (and ignored) as well.
opt_sets() ->
    [{fusion,       [binary,fusion_opt]},
     {plain,        [binary]},
     {no_fus_plain, [binary,no_ssa_opt_fus]},
     {no_fus,       [binary,fusion_opt,no_ssa_opt_fus]},
     {no_ssa_opt,   [binary,no_ssa_opt]}].

%% Write `Source' - a complete module named `Mod' - to a fresh scratch
%% directory and compile it with every option set in opt_sets/0.
build_probe(Config, Mod, Source) ->
    Dir = tmp_dir(Config),
    File = write_probe(Dir, Mod, Source),
    build_all(File).

build_all(File) ->
    [build(File, Set, Opts) || {Set,Opts} <- opt_sets()].

%% Compile to memory only, so that the builds cannot influence each other.
build(File, Set, Opts) ->
    case compile:file(File, [return_errors,return_warnings|Opts]) of
        {ok, Mod, Bin, []} ->
            {Set, Mod, Bin, disasm(Bin)};
        {ok, _Mod, _Bin, Warnings} ->
            ct:fail({unexpected_warnings, File, Warnings});
        {error, Errors, Warnings} ->
            ct:fail({compile_failed, File, Errors, Warnings})
    end.

%% Call `Mod:Fun(Args)' once per option set and check that every build
%% returns exactly the expected result or exception, and that all builds agree
%% with each other.
check_calls(Builds, Fun, ArgSets) ->
    Expected = [Value || {_Args,Value} <- ArgSets],
    Results = [{Set, [run(Mod, Bin, Fun, Args) || {Args,_} <- ArgSets]}
               || {Set, Mod, Bin, _Code} <- Builds],
    lists:foreach(fun({Set, Values}) ->
                          ?assertEqual({Set, Expected}, {Set, Values})
                  end, Results),
    ?assertEqual(1, length(lists:usort([Values || {_Set,Values} <- Results]))),
    ok.

%% Run `Mod:Fun(Args)' in the code that was just loaded. The result is
%% `{ok,Value}' for a normal return and `{Class,Reason}' for an exception; the
%% stacktrace is deliberately not part of the comparison.
run(Mod, Bin, Fun, Args) ->
    _ = code:purge(Mod),
    {module, Mod} = code:load_binary(Mod, atom_to_list(Mod) ++ ".erl", Bin),
    try apply(Mod, Fun, Args) of
        Value ->
            {ok, Value}
    catch
        Class:Reason ->
            {Class, Reason}
    end.

tmp_dir(Config) ->
    Base = case proplists:get_value(priv_dir, Config) of
               undefined ->
                   case os:getenv("TMPDIR") of
                       false -> "/tmp";
                       "" -> "/tmp";
                       EnvDir -> EnvDir
                   end;
               PrivDir ->
                   PrivDir
           end,
    Sub = "beam_ssa_fus_" ++ integer_to_list(erlang:unique_integer([positive])),
    Dir = filename:join(Base, Sub),
    ok = filelib:ensure_path(Dir),
    Dir.

write_probe(Dir, Mod, Source) ->
    File = filename:join(Dir, Mod ++ ".erl"),
    ok = file:write_file(File, Source),
    File.

%% The code of one build, i.e. the list of `{function,Name,Arity,Label,Is}'
%% entries. beam_disasm:file/1 returns the #beam_file{} record; it is matched
%% as a plain tuple here so that this suite does not have to include the
%% compiler internal beam_disasm.hrl (and so that a change of the record
%% shows up as a test failure).
disasm(Bin) ->
    case beam_disasm:file(Bin) of
        {beam_file, _Mod, _Exports, _Attrs, _CompileInfo, Code} ->
            Code;
        {error, Mod, Reason} ->
            ct:fail({beam_disasm_failed, Mod, Reason})
    end.

code(Set, Builds) ->
    case lists:keyfind(Set, 1, Builds) of
        {Set, _Mod, _Bin, Code} ->
            Code;
        false ->
            ct:fail({missing_option_set, Set})
    end.

%% True if `Name' occurs as an instruction anywhere in the code. Both plain
%% instructions (`get_map_elements', ...) and `test'/`bif' instructions that
%% carry their name in the second element (`{test,has_map_fields,...}',
%% `{bif,is_map_key,...}') are recognized.
has_instr(Code, Name) ->
    lists:any(fun({function,_,_,_,Is}) -> any_instr(Is, Name);
                 (_) -> false
              end, Code).

any_instr([I|Is], Name) ->
    case instr_name(I) of
        Name -> true;
        _ -> any_instr(Is, Name)
    end;
any_instr([], _Name) ->
    false.

instr_name(I) when is_tuple(I) ->
    case element(1, I) of
        test -> element(2, I);
        bif -> element(2, I);
        gc_bif -> element(2, I);
        Name -> Name
    end;instr_name(_) ->
    none.

%% The number of `Name' instructions in the function `Fun'. Fails if the
%% function is missing - which is what makes a count assertion a real check
%% that the function was compiled.
count_instr(Code, Fun, Name) ->
    case [Is || {function,N,_,_,Is} <- Code, N =:= Fun] of
        [Is] -> length([I || I <- Is, instr_name(I) =:= Name]);
        Other -> ct:fail({missing_function, Fun, Other})
    end.

%% The number of multiplication instructions in the function `Fun'. A
%% multiplication by a constant is emitted as the `*' BIF for an integer
%% operand but as the dedicated `fmul' instruction for a float operand, so both
%% spellings must be counted; counting only `'*'' would miss a float chain
%% entirely.
count_mul(Code, Fun) ->
    count_instr(Code, Fun, '*') + count_instr(Code, Fun, fmul).

%% The integer constant of the multiplication by a constant in `Fun'. Fails
%% unless `Fun' contains exactly one `*' instruction with an integer literal
%% operand; used to check that a chain was folded into a single multiplication
%% by the *product* of the chain's constants.
mul_constant(Code, Fun) ->
    Is = case [Is0 || {function,N,_,_,Is0} <- Code, N =:= Fun] of
             [Is1] -> Is1;
             Other -> ct:fail({missing_function, Fun, Other})
         end,
    case [K || I <- Is, {integer,K} <- mul_args(I)] of
        [K] -> K;
        Constants -> ct:fail({unexpected_multiplication, Fun, Constants})
    end.

%% The operands of a multiplication instruction, and [] for every other
%% instruction. Both the `bif' and the `gc_bif' spelling are recognized so that
%% the constant can be read regardless of how the multiplication was emitted.
mul_args({bif,'*',_Fail,Args,_Dst}) ->
    Args;
mul_args({gc_bif,'*',_Fail,_Live,Args,_Dst}) ->
    Args;
mul_args(_) ->
    [].

%% A compact description of the code - the instruction names in order - used%% to compare builds without dumping complete instruction lists on failure.
shape(Code) ->
    [{Name,Arity,[instr_name(I) || I <- Is]} ||
        {function,Name,Arity,_,Is} <- Code].

%% The keys of the single `get_map_elements' instruction of `Fun', as a sorted
%% list of operands. Fails if the function does not contain exactly one such
%% instruction - which is what makes the fused form one *multi-key* lookup.
get_map_keys(Code, Fun) ->
    Is = case [Is0 || {function,Name,_,_,Is0} <- Code, Name =:= Fun] of
             [Is1] -> Is1;
             Other -> ct:fail({missing_function, Fun, Other})
         end,
    case [List || {get_map_elements,_,_,{list,List}} <- Is] of
        [List] -> lists:sort(keys_of(List));
        Lists -> ct:fail({unexpected_get_map_elements, Fun, Lists})
    end.

keys_of([Key,_Dst|T]) -> [Key|keys_of(T)];
keys_of([]) -> [].
