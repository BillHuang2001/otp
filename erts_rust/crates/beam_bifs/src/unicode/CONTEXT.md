# `beam_bifs::unicode` — the `unicode` module (`src/unicode.rs` + `src/unicode/wide.rs`)

## Intent
The six registered `unicode` BIFs (`characters_to_binary/1,2,3`, `characters_to_list/1,2`, `bin_is_7bit/1`), its rewrite of the `latin1`/`utf8`/`utf16`/`utf32` conversions.
Ground truth (read-only): `../../lib/stdlib/src/unicode.erl` (the `characters_to_binary_int`/`characters_to_list_int` engine and the `cbv/2`/`o_trans/1` codecs, the specification the wide forms trap to) and `../../../erts/emulator/beam/erl_unicode.c` (the C BIF's own `latin1`/`utf8` fast paths and its trap decision `erl_unicode.c:948-954`).

## API Surface
- `src/unicode.rs` — the module wiring, the `Encoding` enum (`Latin1`, `Utf8`/`unicode`, and the four wide forms) and both engines:
  - **Engine 1** — the C BIF path (output `utf8`/`unicode`): `utf8_need`, the 7-bit/valid fast path, `do_build_utf8`, `build_utf8_return`; `characters_to_list` reuses the builder and `build_list_return` decodes the byte buffer into a codepoint list.
  - **Engine 2** — `characters_to_binary_int/3` (`ml_map` → `i_trans_chk`/`o_trans`), used when the output encoding is `latin1`; the 255 limit and the negative-integer `badarg` are the oracle's.
  - `no_conversion_needed/3` (the `unicode.erl` short-circuit) runs first for every `/2`/`/3` call (`{latin1,latin1}` → `is_binary/1`; `{latin1,utf8}`/`{utf8,latin1}` → `bin_is_7bit/1`); it is never true for a wide encoding.
- `src/unicode/wide.rs` — the self-contained UTF-16/UTF-32 codecs the wide encodings need, all safe Rust (decoders inspect a byte slice, encoders build a `Vec<u8>`): a "one code point at the head" step decoder, the `cbv/2` *prefix length* probe and the `<<One/utf16>>`/`<<One/utf32>>` builders. These live here because the oracle BIF **traps** to `unicode:characters_to_binary_int/2` for the wide forms, so `unicode.erl` (not a C helper) is the authority.
- `src/unicode/grapheme/` (**`./grapheme/CONTEXT.md`**) — the extended-grapheme-cluster layer (`string:next_grapheme/1` / `string:length/1` over the `unicode_util:gc/1` rules, with generated Unicode 17.0 property tables) that the unicode `~ts` `{chars_limit,N}` rendering (`../io_ts.rs`) measures and slices by.

## Supported encodings
`latin1`, `unicode`/`utf8` (exact aliases, as in `erl_unicode.c`) and `utf16`/`utf32` (with the `{utf16,big|little}` / `{utf32,big|little}` forms, the bare atom meaning **big**) — as **input and output** encodings, for every registered arity (`characters_to_binary/1,2,3`, `characters_to_list/1,2`, `bin_is_7bit/1`).
An encoding atom naming anything else is `badarg` (the oracle's class), never a wrong result.

## Pinned oracle behaviours
- `characters_to_binary/2`'s second argument is the **input** encoding; `/1` and `/2` always output `unicode`/utf8.
- A one-element list whose head is a binary is unwrapped before engine 1, so an error reports a *binary* `Rest` (`characters_to_binary([<<"AB">>])` on `<<65,255,66>>` ⇒ `{error, <<65>>, <<255,66>>}`).
- `<<240,128,128>>` is `{incomplete, <<>>, <<240,128,128>>}` (a buffered truncated sequence is a valid prefix), not `error`; likewise `characters_to_list(<<240,128,128>>)` ⇒ `{incomplete, [], <<240,128,128>>}`.
- An overlong encoding, a lone surrogate, a code point past `U+10FFFF` and a stray continuation byte are `error`, not `incomplete`.
- `characters_to_binary([1076,1072], latin1, utf8)` errors with the `Rest` *flattened* (`{error, <<>>, [1076,1072]}`), while the engine-2 counterpart nests it (`{error, <<>>, [[256]]}`) — both shapes are kept.
- A non-atom encoding argument, an unknown encoding atom, a non-list/non-binary `Data`, an improper list, a float/atom element, a big code point and a non-byte-aligned bitstring are all `badarg`.

## Constraints
- Errors are `beam_common::{Result, BeamError}` only; `BadArg` for every argument/encoding failure.
- No `unsafe` — `wide.rs` is pure safe Rust and this module uses `crate::access`/`crate::bits` for term/heap access.

## Test Strategy
- In-module `#[cfg(test)]` suites cover both engines, the wide codecs and the pinned behaviours above (an oracle cross-check via the system `erl` where a value is subtle).
- There is **no dedicated `tests/unicode*.rs` suite**; the module's integration coverage is `tests/atom_quoting.rs`, which drives `unicode:characters_to_binary/1,2,3` against the live oracle (the in-module suite is 25 tests — 15 in `unicode.rs`, 10 in `wide.rs`).

## Routing Table
- `./` (`src/unicode.rs`) -> the six `unicode` BIFs, the `Encoding` selection and the two engines
- `./wide.rs` -> the UTF-16/UTF-32 step decoder, the `cbv/2` prefix probe and the `<<One/utf16>>`/`<<One/utf32>>` builders
- `../CONTEXT.md` -> the crate root (Intent/API Surface/census/Constraints/Test Strategy/Routing Table)
