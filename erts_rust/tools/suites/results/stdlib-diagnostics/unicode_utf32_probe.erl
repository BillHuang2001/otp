%% Diagnostic probe for class D12 of `results/stdlib-notes.md` §D.
%%
%% `unicode_SUITE:latin1` and `unicode_SUITE:roundtrips` fail on a **bit-syntax
%% construction** difference: a `utf32` segment written with the explicit `little`
%% endianness produces **big-endian** bytes when the value is a runtime value.
%%
%%     f(E) -> <<E/utf32-little>>.        %% E a parameter
%%     f(1)  -> oracle <<1,0,0,0>>   rust <<0,0,0,1>>
%%
%% The literal fold (`<<1/utf32-little>>`), the shift form (`<<E:32/little>>`) and the
%% `utf16` counterpart are all correct, the *matching* side (`<<C/utf32-little, …>>`) is
%% correct too, and the interpreter tier reproduces the baseline-JIT answer — so the
%% defect is in the construction family's field writer
%% (`beam_engine/src/families/bitstrings/create/prepared.rs`, `put_utf32` — it takes no
%% endianness flag and always writes `to_be_bytes`, where the neighbouring `put_utf16`
%% takes `little`).
%%
%% Erlc: `erlc -o . unicode_utf32_probe.erl`
%% Run:  `beam_rt -noshell -pa . -eval 'unicode_utf32_probe:main(), halt(0).'`
%%       `erl     -noshell -pa . -eval 'unicode_utf32_probe:main(), halt(0).'`
-module(unicode_utf32_probe).
-export([main/0, u32_little_from_var/1, u32_little_literal/0, u32_shift/1,
         u16_little_from_var/1, suite_helper/0, match_little/1]).

main() ->
    row("u32_little_from_var(1)", fun() -> u32_little_from_var(1) end),
    row("u32_little_literal()", fun() -> u32_little_literal() end),
    row("u32_shift(1)", fun() -> u32_shift(1) end),
    row("u16_little_from_var(1)", fun() -> u16_little_from_var(1) end),
    row("match_little(<<1:32/little>>)", fun() -> match_little(<<1:32/little>>) end),
    row("suite_helper()", fun() -> suite_helper() end),
    ok.

%% The failing shape: a `utf32`/`little` segment whose value is a runtime value.
u32_little_from_var(E) -> <<E/utf32-little>>.

%% Control: the same segment with a literal, folded when the module is loaded.
u32_little_literal() -> <<1/utf32-little>>.

%% Control: the shift form spells the same field, and is correct.
u32_shift(E) -> <<E:32/little>>.

%% Control: `utf16` takes its endianness from the same flag, and is correct.
u16_little_from_var(E) -> <<E/utf16-little>>.

%% Control: the match-context side reads `utf32`/`little` correctly.
match_little(<<C/utf32-little, Rest/binary>>) -> {C, Rest}.

%% `unicode_SUITE:list_to_utf32_little_bsyntax/2` + `flatb/1`, verbatim in shape: the
%% suite compares this build against `unicode:characters_to_binary(All, latin1, utf32)`.
suite_helper() ->
    All = lists:seq(0, 255),
    Bytes = flatb([list_to_binary(All)]),
    Built = list_to_binary([<<E/utf32-little>> || E <- Bytes]),
    Reference = unicode:characters_to_binary(All, latin1, {utf32, little}),
    {Built =:= Reference, byte_size(Built), binary:part(Built, 0, 8)}.

flatb(List) -> lists:reverse(flatb(List, [])).
flatb(<<X:8, Rest/binary>>, Acc) -> flatb(Rest, [X | Acc]);
flatb(<<>>, Acc) -> Acc;
flatb([H | T], Acc) -> flatb(T, flatb(H, Acc));
flatb([], Acc) -> Acc;
flatb(X, Acc) -> [X | Acc].

row(Label, F) ->
    io:format("~-34s ~p~n", [Label, F()]).
