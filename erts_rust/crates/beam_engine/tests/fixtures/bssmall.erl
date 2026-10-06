%% Oracle fixture for **integer bitstring segments of at most 64 bits** whose
%% value can still exceed `MAX_SMALL` (2^59-1), plus the little-endian fields
%% that are *not* a whole number of bytes.
%%
%% The committed `bssmall.beam` is produced by the system `erlc` (OTP 29 /
%% erts-17.1) exactly as `tests/fixtures/README.md` prescribes; the expected
%% values in `tests/bitstrings_extra_big.rs` are the real system `erl` oracle's
%% answers for the same inputs.
%%
%% Every read is `<<Y:N, _/bitstring>> = B`, i.e. an `ensure_at_least N 1`
%% followed by an `integer` command of `N` bits, so a whole-byte subject of at
%% least `N` bits can be supplied from the test side (the engine has no partial
%% bit subject builder). `u64x/1` is the plain exhaustive `<<Y:64>>` form.
%%
%% `e256/1` writes `Value mod 2^256` so a bignum result can be pinned
%% byte-exactly through the crate's public `terms::binary_bytes` (no `unsafe`
%% bignum reader is reachable from an integration test).
-module(bssmall).
-export([u9/1, u12/1, u17/1, u20/1, u36/1, u59/1, u60/1, u61/1, u62/1, u64/1,
         u64x/1,
         s59/1, s60/1, s61/1, s64/1,
         u9le/1, u12le/1, u17le/1, u20le/1, u36le/1, u60le/1, u64le/1, s64le/1,
         c_u64/1,
         e256/1]).

%% --- unsigned, big-endian ---------------------------------------------------
u9(B)  -> <<Y:9,  _/bitstring>> = B, Y.
u12(B) -> <<Y:12, _/bitstring>> = B, Y.
u17(B) -> <<Y:17, _/bitstring>> = B, Y.
u20(B) -> <<Y:20, _/bitstring>> = B, Y.
u36(B) -> <<Y:36, _/bitstring>> = B, Y.
u59(B) -> <<Y:59, _/bitstring>> = B, Y.
u60(B) -> <<Y:60, _/bitstring>> = B, Y.
u61(B) -> <<Y:61, _/bitstring>> = B, Y.
u62(B) -> <<Y:62, _/bitstring>> = B, Y.
u64(B) -> <<Y:64, _/bitstring>> = B, Y.
%% The compiler's exhaustive 64-bit form (`ensure_exactly 64`).
u64x(B) -> <<Y:64>> = B, Y.

%% --- signed, big-endian -----------------------------------------------------
s59(B) -> <<Y:59/signed, _/bitstring>> = B, Y.
s60(B) -> <<Y:60/signed, _/bitstring>> = B, Y.
s61(B) -> <<Y:61/signed, _/bitstring>> = B, Y.
s64(B) -> <<Y:64/signed, _/bitstring>> = B, Y.

%% --- little-endian (the fields that are *not* a whole number of bytes take
%% `erts_bs_get_integer_2`'s general path, `erl_bits.c:280-392`) --------------
u9le(B)  -> <<Y:9/little,  _/bitstring>> = B, Y.
u12le(B) -> <<Y:12/little, _/bitstring>> = B, Y.
u17le(B) -> <<Y:17/little, _/bitstring>> = B, Y.
u20le(B) -> <<Y:20/little, _/bitstring>> = B, Y.
u36le(B) -> <<Y:36/little, _/bitstring>> = B, Y.
u60le(B) -> <<Y:60/little, _/bitstring>> = B, Y.
u64le(B) -> <<Y:64/little, _/bitstring>> = B, Y.
s64le(B) -> <<Y:64/little-signed, _/bitstring>> = B, Y.

%% --- `case`-wrapped 64-bit read: the failure path raises `case_clause` ------
c_u64(B) ->
    case B of
        <<Y:64, _/bitstring>> -> Y
    end.

%% --- construction helper: the value modulo 2^256, big-endian ----------------
e256(A) ->
    <<A:256>>.
