# `beam_bifs::unicode::grapheme` — the extended-grapheme-cluster port (`src/unicode/grapheme/`)

## Intent
The extended-grapheme-cluster layer the unicode string directives need: a faithful port of the oracle's `string:next_grapheme/1` and `string:length/1`, both of which are `unicode_util:gc/1` over the Unicode extended-grapheme-cluster rules.
It is the authority the unicode `~ts` arm of `io_lib:format/3` with an active `{chars_limit, N}` counts and truncates by (`../io_ts.rs` — the oracle's `io_lib_format:string/6` measures with `io_lib:chars_length/1` and slices with `string:slice/3`, so a combining-mark cluster counts and slices as **one** character); the latin1 `~s` arm does not need it.
Ground truth (read-only): `lib/stdlib/src/string.erl` (`next_grapheme/1`, `length/1`), the generated `lib/stdlib/src/unicode_util.erl` (the `gc/1`/`gc_1/2` family, the `gcb/1`/`seqs` predicates, `cp/1`), `lib/stdlib/uc_spec/gen_unicode_mod.escript` (the generator whose grammar/range-merging this port reproduces) and `../../../../erts/emulator/beam/erl_unicode.c` for the C-side helpers.

## API Surface
- `mod.rs` — the `unicode_util:gc/1` port: the cluster rules in clause order (the `gc_1/2` clause **order** is a property of the walk, not of the tables, so it lives here), the `gcb/1`/`seqs` predicates and the property-free fast paths.
- `api.rs` — the two `string:` entry points: `next_grapheme/1` (`= unicode_util:gc(CD)`, the first cluster + the continuation) and `grapheme_count` (`= string:length/1`, clause-by-clause including its `length_gc` walk and its fast paths).
- `chardata.rs` — the chardata → code-point walk (the `unicode_util:cp/1` + `cpl*`/`cp_no_bin*` helpers) the cluster rules consume.
- `props.rs` — **GENERATED, never hand-edit**: the Unicode 17.0 property tables (sorted, non-overlapping, adjacency-merged `(lo, hi)` ranges) plus the `in_ranges` membership search. Data, not logic — that is why it is long.
- `tests.rs` — the golden corpus + a fail-loud live-oracle cross-check of `string:next_grapheme/1`/`string:length/1`.

## Constraints
- Errors are `beam_common::{Result, BeamError}` only; `badarg` for a non-chardata argument. No `unsafe` (crate-wide `#![deny(unsafe_code)]`).
- **`props.rs` is generated from the in-repo UCD files** `lib/stdlib/uc_spec/{GraphemeBreakProperty,emoji-data,IndicSyllabicCategory}.txt`, using the same grammar and range merging as `lib/stdlib/uc_spec/gen_unicode_mod.escript` (which generates the oracle's `unicode_util.erl`). Regenerate — never patch the output:
  `ERL_TOP="$PWD" escript erts_rust/crates/beam_bifs/src/unicode/grapheme/gen_props.escript`

## Test Strategy
- `tests.rs` pins the property tables and the cluster rules against a golden corpus, plus a live-oracle cross-check of `string:next_grapheme/1`/`string:length/1` that **fails loudly** when `erl` is absent or errors (a silently-skipping harness once hid a regression in this repo).
- End-to-end coverage of the consumer: the unicode `~ts` `{chars_limit,N}` matrix in `../../../tests/io_lib_format_ts_limited.rs` (a byte-exact live-oracle cross-check) and the unlimited `~ts` cases in `../../../tests/io_lib_format_ts.rs`.

## Routing Table
- `./mod.rs` -> the `unicode_util:gc/1` cluster-rule port and the `gcb/1`/`seqs` predicates
- `./api.rs` -> `next_grapheme/1` + `grapheme_count` (the `string:` entry points)
- `./chardata.rs` -> the chardata-to-code-point walk (`unicode_util:cp/1` + helpers)
- `./props.rs` -> the GENERATED Unicode 17.0 property tables + `in_ranges` (do not edit; regenerate)
- `./gen_props.escript` -> the generator for `props.rs`
- `./tests.rs` -> the golden corpus + the fail-loud live-oracle cross-check
- `../CONTEXT.md` -> the `unicode` node (the six `unicode` BIFs, the wide codecs)
- `../io_ts.rs` (`../../io_ts.rs`) -> the unicode `~ts` `{chars_limit,N}` consumer (sibling file, same crate)
