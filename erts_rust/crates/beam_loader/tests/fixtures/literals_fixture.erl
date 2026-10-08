%%% The `beam_loader` literal-pool fixture (`LitT` round-trip test, OTP 29 /
%%% `erts-17.1`). Compiled twice with the system `erlc`: once plain
%%% (`literals_verbatim.beam`, OTP 28+ verbatim `LitT`) and once with
%%% `+compressed_literals` (`literals_compressed.beam`, the OTP 27 zlib `LitT`).
%%% Every function returns one constant term, so the whole constant set lands in
%%% the module's `LitT` literal pool; see `tests/fixtures/README.md` for the
%%% regeneration commands and the decoded literal list.
-module(literals_fixture).
-export([tuple/0, list/0, map/0, float/0, bignum/0, binary/0, extfun/0, nested/0, mixed/0]).

%% A tuple containing a nested tuple and the empty list.
tuple() -> {a, 1, {b, 2}, []}.

%% A list containing a nested list, a tuple, and a string (a list of integers).
list() -> [1, 2, [3, 4], {ok, 5}, "abc"].

%% A map with an atom key, a tuple key and a *binary* key whose value is a float.
map() -> #{a => 1, {x, y} => [1, 2], <<"k">> => 2.0}.

%% Floats, including a very large one and a zero.
float() -> [1.5, -2.25, 1.0e300, 0.0].

%% Positive and negative bignums (far beyond the 60-bit small-integer range).
bignum() -> [123456789012345678901234567890, -99999999999999999999999999].

%% Binaries, including a non-byte-aligned bitstring (`<<1:3>>`).
binary() -> [<<1, 2, 3>>, <<"hello">>, <<1:3>>].

%% External (`EXPORT_EXT`) fun references.
extfun() -> [fun lists:reverse/1, fun maps:get/2].

%% A tuple holding a map (with a float and a binary value), a bignum and a list
%% of binaries -- a composite literal of every kind above.
nested() -> {#{list => [a, b], float => 1.5, bin => <<7, 8>>},
             10000000000000000000000000,
             [<<"x">>]}.

%% A tuple holding a list, a tuple with a float and a small map.
mixed() -> {ok, [1, 2, 3], {a, 1.5}, #{k => v}}.
