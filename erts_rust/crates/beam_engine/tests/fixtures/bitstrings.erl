%% Oracle fixture for the bitstring (`bs_*`) instruction family.
%%
%% Every function here is compiled to opcodes the Milestone-1 `beam_engine`
%% bitstring family implements; the committed `bitstrings.beam` is produced by the
%% system `erlc` (OTP 29 / erts-17.1) exactly as `tests/fixtures/README.md` and the
%% other fixtures describe. See `tests/bitstrings.rs` for the expected values.
-module(bitstrings).

-export([mk/2, pair/2, first8/1, take2/1, tail1/1, head16/1]).

%% Construction: `bs_create_bin` with an `integer` segment (8 bits) and a
%% `binary` segment of literal size 16 bits (2 bytes).
mk(A, B) ->
    <<A:8, B:16>>.

%% Construction: two `integer` segments (8 bits each).
pair(A, B) ->
    <<A:8, B:8>>.

%% Matching: `bs_start_match3` + `bs_get_integer2` + a tail binary segment.
first8(Bin) ->
    case Bin of
        <<X:8, _/binary>> -> X
    end.

%% Matching: two `bs_get_integer2` reads followed by a tail.
take2(Bin) ->
    case Bin of
        <<A:8, B:8, _/binary>> -> {A, B}
    end.

%% Matching: skip one byte, bind the remaining bits (`bs_get_tail`).
tail1(Bin) ->
    case Bin of
        <<_:8, Rest/binary>> -> Rest
    end.

%% Matching: an exact 16-bit read, with the tail test (`bs_test_tail2`).
head16(Bin) ->
    case Bin of
        <<X:16>> -> X
    end.
