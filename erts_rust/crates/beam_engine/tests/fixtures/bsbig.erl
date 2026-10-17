%% Oracle fixture for **integer bitstring segments wider than 64 bits** — the
%% wide-field forms of `bs_get_integer2`, the `integer` command of the flattened
%% `bs_match` command list, and the `integer` segment of `bs_create_bin`.
%%
%% The committed `bsbig.beam` is produced by the system `erlc` (OTP 29 /
%% erts-17.1) exactly as `tests/fixtures/README.md` describes; the expected values
%% in `tests/bitstrings_extra_big.rs` are the real system `erl` oracle's answers for
%% the same inputs.
%%
%% Every function keeps its argument a plain binary (never an integer) so a
%% non-matching argument cannot reach `func_info`, which raises the bare atom
%% `function_clause`; the `c_*` group wraps each pattern in a `case` so the failure
%% path raises `case_clause`.
-module(bsbig).
-export([g72/1, g72s/1, g72le/1, g68/1, g68le/1, g65/1, g128/1, g9u8/1,
         seq/1, c_g72/1, c_g72s/1, c_g72le/1,
         e72/1, e72s/1, e72le/1, e68/1, e68le/1, e128/1, e9u8/1, e256/1]).

%% --- matching: a wide unsigned field (`bs_get_integer2`) --------------------
g72(<<X:72, _/binary>>) ->
    X.
%% --- matching: a wide signed field ------------------------------------------
g72s(<<X:72/signed, _/binary>>) ->
    X.
%% --- matching: a wide little-endian field (a whole number of bytes) ---------
g72le(<<X:72/little, _/binary>>) ->
    X.
%% --- matching: a 68-bit field (not a whole number of bytes) -----------------
g68(<<X:68, _/binary>>) ->
    X.
%% --- matching: a wide little-endian field that is *not* a whole number of
%% bytes (the `*MSB >>= 8 - BIT_OFFSET(Size)` adjustment of
%% `erts_bs_get_integer_2`) ----------------------------------------------------
g68le(<<X:68/little, _/binary>>) ->
    X.
%% --- matching: a 65-bit field (just over one machine word) ------------------
g65(<<X:65, _/binary>>) ->
    X.
%% --- matching: a 128-bit (two-machine-word) field ---------------------------
g128(<<X:128, _/binary>>) ->
    X.
%% --- matching: a 72-bit field spelled `size * unit` -------------------------
g9u8(<<X:9/unit:8, _/binary>>) ->
    X.
%% --- matching: two wide reads in one pattern --------------------------------
%% The compiler flattens a pattern whose reads all have literal sizes into a
%% single `bs_match` instruction with an `integer` command per read.
seq(<<A:72, B:8, _/binary>>) ->
    {A, B}.
%% --- `case`-wrapped variants: the failure path raises `case_clause` ---------
c_g72(Bin) ->
    case Bin of
        <<X:72>> -> X
    end.
c_g72s(Bin) ->
    case Bin of
        <<X:72/signed>> -> X
    end.
c_g72le(Bin) ->
    case Bin of
        <<X:72/little>> -> X
    end.

%% --- construction: a wide integer segment (`bs_create_bin`) -----------------
e72(A) ->
    <<A:72>>.
e72s(A) ->
    <<A:72/signed>>.
e72le(A) ->
    <<A:72/little>>.
e68(A) ->
    <<A:68>>.
e68le(A) ->
    <<A:68/little>>.
e128(A) ->
    <<A:128>>.
e9u8(A) ->
    <<A:9/unit:8>>.
e256(A) ->
    <<A:256>>.
