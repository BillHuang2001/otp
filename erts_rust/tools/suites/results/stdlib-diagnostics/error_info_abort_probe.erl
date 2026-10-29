%% Diagnostic probe for class **D1** of `../stdlib-notes.md` §D — the `binary_module_SUITE`
%% abort that the sweep still reports as `rust-gap` ("case missing from the rust runtime's
%% output", 12 cases lost).
%%
%% Two modes, both driving the suite's own code through the **same shape as
%% `ct_runner:run_case/4`** (a `spawn_monitor`'d child, a `try`/`catch` around the case, and
%% `exit({ct_case_result, Result})`), because that shape is what makes the difference — the
%% very same call in the main process, or in a child that merely *returns* its value, is fine:
%%
%%   {prefix, N}  the first N entries of `binary_module_SUITE:error_info/1`'s literal list,
%%                run through `error_info_lib:test_error_info(binary, Prefix)`. This is the
%%                size knob: the abort appears between N = 69 and N = 70 (73 entries exist).
%%   {one, Atom} one named case of `binary_module_SUITE`, alone in a fresh OS process, so a
%%                case the sweep reports as "missing" can be measured on its own.
%%
%% Both print the child's `DOWN` reason (`MAIN_GOT …` / `RESULT …`) so the two sides can be
%% compared with `diff`; the aborting side never gets there, which is the observation.
%%
%% Prerequisites: `binary_module_SUITE` and `error_info_lib` compiled with the **oracle's**
%% `erlc` into a directory on the code path (the `suites` driver stages them, together with
%% the other helpers, under `$TMPDIR/erts-suites-stage/stdlib/{ebin,src}`).
%%
%% Erlc: `erlc -o ebin error_info_abort_probe.erl`
%% Run (abort: 69 vs 70 entries — rust dies on 70, the oracle answers both):
%%   cd <throwaway cwd>
%%   beam_rt -noshell -pa <probe-ebin> -pa <staged>/ebin \
%%           -eval 'error_info_abort_probe:main({prefix, 69}).'
%%   beam_rt -noshell -pa <probe-ebin> -pa <staged>/ebin \
%%           -eval 'error_info_abort_probe:main({prefix, 70}).'
%%   erl     -noshell -pa <probe-ebin> -pa <staged>/ebin \
%%           -eval 'error_info_abort_probe:main({prefix, 70}).'
%% Run (one case of the 12 the sweep loses):
%%   beam_rt -noshell -pa <probe-ebin> -pa <staged>/ebin \
%%           -eval 'error_info_abort_probe:main({one, referenced}).'
-module(error_info_abort_probe).
-export([main/1, entries/0]).

main(Arg) ->
    case Arg of
        {prefix, N} ->
            prefix(N);
        {one, Case} ->
            one_case(Case);
        all ->
            prefix(length(entries()));
        N when is_integer(N) ->
            prefix(N)
    end,
    halt(0).

%% The literal entry list of `binary_module_SUITE:error_info/1`, verbatim (source order).
entries() ->
    [{at, [[no,binary], bad_pos],[{1,".*"},{2,".*"}]},
     {at, [<<77:7>>, -1],[{1,".*"},{2,".*"}]},
     {bin_to_list, [{no,binary}]},
     {bin_to_list, [<<1:1>>]},
     {bin_to_list,[<<3:3>>, bad_pos_len],[{1,".*"},{2,".*"}]},
     {bin_to_list,[<<"abc">>, {0, 99}]},
     {bin_to_list,[<<1,2,3>>, {1,4}]},
     {bin_to_list,[<<1,2,3>>, 1, 4]},
     {bin_to_list,[<<1,2,3>>, a, b],[{2,".*"},{3,".*"}]},
     {bin_to_list,[<<1,2,3>>, a, 0]},
     {bin_to_list,[<<1,2,3>>, 1, -10]},
     {compile_pattern,[42]},
     {copy,[no_binary]},
     {copy,[no_binary,-42],[{1,".*"},{2,".*"}]},
     {copy,[<<1:1>>,no_integer],[{1,".*"},{2,".*"}]},
     {decode_unsigned,[<<1:1>>]},
     {decode_unsigned,[<<0,42>>, middle]},
     {encode_unsigned,[abc]},
     {encode_unsigned,[42, middle]},
     {encode_unsigned,[abc, big]},
     {first,[<<1:1>>]},
     {first,[<<>>]},
     {last,[<<1:1>>]},
     {last,[<<>>]},
     {join,[no_list,<<>>]},
     {join,[[a|b],<<>>]},
     {join,[[a],<<>>]},
     {join,[[],<<1:7>>]},
     {join,[[],bad_separator]},
     {list_to_bin,[<<1,2,3>>]},
     {list_to_bin,[{1,2,3}]},
     {longest_common_prefix, [abc]},
     {longest_common_suffix, [abc]},
     {match, [<<1:1>>, {bm,make_ref()}],[{1,".*"},{2,".*"}]},
     {match, [<<1:1>>, no_pattern],[{1,".*"},{2,".*"}]},
     {match, [<<"abc">>, no_pattern]},
     {match, [<<"abc">>, <<"a">>, [{scope,{2,-8}}]]},
     {match, [<<"abc">>, <<"a">>, [bad_option]]},
     {matches,[<<1:1>>, {bm,make_ref()}],[{1,".*"},{2,".*"}]},
     {matches,[<<1:1>>, no_pattern],[{1,".*"},{2,".*"}]},
     {matches,[<<"abc">>, no_pattern]},
     {matches, [<<"abc">>, <<"a">>, [{scope,{0,-1}}]]},
     {matches, [<<"abc">>, <<"a">>, [bad_option]]},
     {bin_to_list, [{no,binary}]},
     {bin_to_list, [<<1:1>>]},
     {part, [<<3:3>>, bad_pos_len],[{1,".*"},{2,".*"}]},
     {part, [<<"abc">>, {0, 99}]},
     {part, [<<1,2,3>>, {1,4}]},
     {part, [<<1,2,3>>, 1, 4]},
     {part, [<<1,2,3>>, a, b],[{2,".*"},{3,".*"}]},
     {part, [<<1,2,3>>, a, 0]},
     {part, [<<1,2,3>>, 1, -10]},
     {referenced_byte_size, [a]},
     {replace, [<<"xyz">>, <<"x">>, <<1:1>>]},
     {replace, [<<"xyz",1:1>>, <<"x">>, <<"y">>]},
     {replace, [<<"xyz">>, {bm,make_ref()}, <<"y">>]},
     {replace, [<<"xyz">>, <<"x">>, <<1:1>>, []]},
     {replace, [<<"xyz",1:1>>, <<"x">>, <<"y">>, []]},
     {replace, [<<"xyz">>, {bm,make_ref()}, <<"y">>, []]},
     {replace, [<<"xyz",1:1>>, <<"x">>, <<"y">>, [bad_option]],[{1,".*"},{4,".*"}]},
     {split, [<<1:15>>, <<"a">>]},
     {split, [<<1,2,3>>, {bm,make_ref()}]},
     {split, [<<1:15>>, <<"a">>, []]},
     {split, [<<1,2,3>>, {bm,make_ref()}, []]},
     {split, [<<1,2,3>>, <<"2">>, [bad_option]]},
     {encode_hex, [{no,binary}]},
     {encode_hex, [{no,binary}, lowercase]},
     {encode_hex, [<<"foobar">>, othercase]},
     {encode_hex, [no_binary, othercase], [{1,".*"}, {2,".*"}]},
     {decode_hex, [{no,binary}]},
     {decode_hex, [<<"000">>]},
     {decode_hex, [<<"GG">>]},
     {decode_hex, [<<255>>]}].

prefix(N) ->
    L = lists:sublist(entries(), N),
    io:format("~nRUN ~p of ~p entries~n", [length(L), length(entries())]),
    %% Exactly ct_runner:run_case/4's shape.
    {Pid, Ref} = spawn_monitor(fun() -> exit({ct_case_result, call_case(L)}) end),
    R = receive
            {'DOWN', Ref, process, Pid, {ct_case_result, {ok, V}}} -> {ok, V};
            {'DOWN', Ref, process, Pid, Reason} -> Reason
        after 60000 -> exit(Pid, kill), timeout
        end,
    io:format("~nMAIN_GOT ~P~nDONE~n", [R, 6]).

%% ct_runner:call_case/3 + normalize/1, for `binary_module_SUITE:error_info/1`.
call_case(L) ->
    Result = try error_info_lib:test_error_info(binary, L)
             catch Class:Reason:Stack -> {case_exception, Class, Reason, first_frame(Stack)}
             end,
    normalize(Result).
normalize({case_exception, Class, Reason, Frame}) -> exit({exception, Class, Reason, Frame});
normalize(Other) -> {ok, Other}.
first_frame([F | _]) -> F;
first_frame(_) -> none.

%% One named case of the suite, in ct_runner:run_case/4's shape, in a fresh OS process.
one_case(Case) ->
    {module, _} = code:ensure_loaded(binary_module_SUITE),
    Config = [{suite, binary_module_SUITE},
              {data_dir, "/tmp/erts-suites-stage/stdlib/src"},
              {priv_dir, "/tmp/erts-suites-stage/stdlib/src"},
              {case_timeout, 60000}],
    io:format("~nCASE ~p~n", [Case]),
    {Pid, Ref} =
        spawn_monitor(
          fun() ->
                  Callable = erlang:function_exported(binary_module_SUITE, Case, 1)
                      orelse erlang:function_exported(binary_module_SUITE, Case, 0),
                  R = case Callable of
                          false -> {missing};
                          true ->
                              try case erlang:function_exported(binary_module_SUITE, Case, 1) of
                                      true -> binary_module_SUITE:Case(Config);
                                      false -> binary_module_SUITE:Case()
                                  end
                              of V -> {ok, V}
                              catch C:E:S -> {case_exception, C, E, first_frame(S)} end
                      end,
                  exit({ct_case_result, R})
          end),
    Res = receive {'DOWN', Ref, process, Pid, Reason} -> Reason
              after 60000 -> exit(Pid, kill), timeout
              end,
    io:format("~nRESULT ~p ~P~nDONE~n", [Case, Res, 12]).
