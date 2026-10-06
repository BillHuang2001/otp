%% Oracle fixture for the bitstring *construction* forms the `bs_create_bin`
%% implementation covers beyond integer/binary/string segments: `float` segments
%% (16/32/64 bits, big- and little-endian) and the `utf8`/`utf16`/`utf32` segments.
%%
%% Every function here is compiled to a `bs_create_bin` whose segment list holds
%% exactly one `float` or `utf*` segment; the committed `bscreate.beam` is produced
%% by the system `erlc` (OTP 29 / erts-17.1) exactly as `tests/fixtures/README.md`
%% describes (build from a neutral directory so no worktree path is baked in).
%% See `tests/bitstrings_extra_create.rs` for the expected values, every one of them
%% derived from the same system `erl` oracle.
-module(bscreate).

-export([f16/1, f16le/1, f32/1, f32le/1, f64/1, f64le/1, f0/1, f8/1,
         u8/1, u16/1, u16le/1, u32/1]).

%% Float segments: 16/32/64-bit IEEE-754, big- and little-endian.
f16(F) -> <<F:16/float>>.
f16le(F) -> <<F:16/float-little>>.
f32(F) -> <<F:32/float>>.
f32le(F) -> <<F:32/float-little>>.
f64(F) -> <<F:64/float>>.
f64le(F) -> <<F:64/float-little>>.
%% A zero-bit float field: `erlc` emits the segment with a "bad float bit size"
%% warning (the size operand is the integer 0, unit 1) and the construction fails
%% at run time with `badarg` -- `erts_bs_put_float` accepts only 16/32/64 bits.
f0(F) -> <<F:0/float>>.
%% An 8-bit float field is not a valid float size; the compiler emits the segment
%% with a "bad float bit size" warning and the construction fails at run time.
f8(F) -> <<F:8/float>>.

%% UTF segments: `utf8` (1-4 bytes), `utf16` big- and little-endian (2 or 4
%% bytes), `utf32` (4 bytes). A codepoint outside the valid scalar range is a
%% construction failure.
u8(C) -> <<C/utf8>>.
u16(C) -> <<C/utf16>>.
u16le(C) -> <<C/utf16-little>>.
u32(C) -> <<C/utf32>>.
