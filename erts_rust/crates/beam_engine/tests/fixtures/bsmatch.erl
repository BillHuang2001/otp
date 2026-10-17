%% Oracle fixture for the bitstring **match** forms that `tests/bitstrings.rs` does
%% not reach: `bs_match_string` (a literal string in a binary pattern), the `'=:='`
%% `bs_match` command, the fixed float reads (`bs_get_float2`: 16/32/64 bits, big and
%% little endian) and the UTF reads/skips (`bs_get_utf{8,16,32}` /
%% `bs_skip_utf{8,16,32}`).
%%
%% The committed `bsmatch.beam` is produced by the system `erlc` (OTP 29 /
%% erts-17.1) exactly as `tests/fixtures/README.md` describes; see
%% `tests/bitstrings_extra_match.rs` for the expected values (derived from the real
%% system `erl` oracle).
%%
%% The first group uses **function-head** patterns (the idiom the milestone fixture
%% recipe prefers). A head-pattern mismatch jumps to the function's `func_info`
%% label, which raises the bare atom `function_clause` (C's `i_func_info_IaaI`), so
%% head-pattern functions are only exercised on their *success* paths. The `c_*`
%% group wraps each pattern in a `case`, whose failure path raises `case_clause` —
%% that group is what the malformed/short-input tests call.
%%
%% `strlong/1` is deliberately an 80-bit literal: the compiler folds strings of at
%% most 64 bits into the `'=:='` `bs_match` command (`str/1`), while a longer literal
%% is emitted as the standalone `bs_match_string` instruction.
-module(bsmatch).

-export([str/1, strlong/1,
         f16/1, f32/1, f32le/1, f64/1, f64le/1,
         u8/1, u16/1, u16le/1, u32/1,
         sk8/1, sk16/1, sk32/1,
         c_str/1, c_strlong/1, c_f16/1, c_f32/1, c_f64/1,
         c_u8/1, c_u16/1, c_u32/1,
         c_sk8/1, c_sk16/1, c_sk32/1]).

%% --- literal string in a binary pattern -------------------------------------
%% 16 bits: the compiler folds this into the `'=:='` `bs_match` command.
str(<<"ab", R/binary>>) ->
    R.

%% 80 bits: too long for `'=:='`, so this is the standalone `bs_match_string`.
strlong(<<"abcdefghij", R/binary>>) ->
    R.

%% --- fixed float reads (`bs_get_float2`) ------------------------------------
f16(<<F:16/float>>) ->
    F.

f32(<<F:32/float>>) ->
    F.

f32le(<<F:32/float-little>>) ->
    F.

f64(<<F:64/float>>) ->
    F.

f64le(<<F:64/float-little>>) ->
    F.

%% --- UTF reads (`bs_get_utf{8,16,32}`) --------------------------------------
u8(<<C/utf8>>) ->
    C.

u16(<<C/utf16>>) ->
    C.

u16le(<<C/utf16-little>>) ->
    C.

u32(<<C/utf32>>) ->
    C.

%% --- UTF skips (`bs_skip_utf{8,16,32}`) -------------------------------------
sk8(<<_/utf8, R/binary>>) ->
    R.

sk16(<<_/utf16, R/binary>>) ->
    R.

sk32(<<_/utf32, R/binary>>) ->
    R.

%% --- `case`-wrapped variants: the failure path raises `case_clause` ----------
c_str(Bin) ->
    case Bin of
        <<"ab", R/binary>> -> R
    end.

c_strlong(Bin) ->
    case Bin of
        <<"abcdefghij", R/binary>> -> R
    end.

c_f16(Bin) ->
    case Bin of
        <<F:16/float>> -> F
    end.

c_f32(Bin) ->
    case Bin of
        <<F:32/float>> -> F
    end.

c_f64(Bin) ->
    case Bin of
        <<F:64/float>> -> F
    end.

c_u8(Bin) ->
    case Bin of
        <<C/utf8>> -> C
    end.

c_u16(Bin) ->
    case Bin of
        <<C/utf16>> -> C
    end.

c_u32(Bin) ->
    case Bin of
        <<C/utf32>> -> C
    end.

c_sk8(Bin) ->
    case Bin of
        <<_/utf8, R/binary>> -> R
    end.

c_sk16(Bin) ->
    case Bin of
        <<_/utf16, R/binary>> -> R
    end.

c_sk32(Bin) ->
    case Bin of
        <<_/utf32, R/binary>> -> R
    end.
