# `beam_bifs::unicode::normalize` — the composition-only UTF-8 normalizer (`src/unicode/normalize/`)

## Intent
The in-crate port of the C normalizer behind `prim_file:internal_normalize_utf8/1`: `prim_file_internal_normalize_utf8_1` (`erts/emulator/beam/erl_unicode.c:2826`) over `do_utf8_to_list_normalize` (`:1569`) and the committed composition data in `erts/emulator/beam/erl_unicode_normalize.h`.
The observable contract is exactly: **composition-only, mark-adjacent, exclusion-ignoring, reorder-free** — a UTF-8 byte image in, a plain list of code points out.
It is explicitly **not** NFC, **not** NFD and **not** NFKC: no canonical *decomposition*, no canonical *reordering* (marks are never sorted — `a + diaeresis + acute` stays three code points), no Hangul composition, and no composition-exclusion list.
The C table is the raw spec transcription, so an excluded pair the header happens to carry *does* compose: `U+0915 U+093C` ⇒ `U+0958` (`[2392]`), the oracle's own answer too (live-verified on OTP 29.1.1).
A composition happens only where the header holds an explicit chain — 2–4 code points, the combining (rightmost) one first — so the walk visits the input **right to left**, exactly as the C does, holding at most four code points in the C's `Uint16 savepoints[4]` window.
Ground truth (read-only): `erl_unicode.c` — `is_candidate/1` `:1418`, `hashsearch/4` `:1431`, `translate/3` `:1443`, `handle_first_norm` `:1468`, `cleanup_norm` `:1475`, `handle_potential_norm` `:1500`, `do_utf8_to_list_normalize` `:1569` — plus the generated header.

## API Surface
Everything here is crate-private (`pub(crate)`); the crate-external surface is the one registered BIF.
- `mod.rs::normalize_utf8(bytes: &[u8]) -> Result<Vec<u32>>` — decode **and** normalize; the whole BIF body.
- `mod.rs::normalize(code_points: &[u32]) -> Vec<u32>` — `do_utf8_to_list_normalize` over already-decoded code points (the C walks the bytes right to left, this walks the code points right to left).
- `mod.rs::is_candidate(cp: u32) -> bool` — the gate on opening a pending group: below 768 nothing is a candidate, above 4023 only `U+3099`/`U+309A` (12441/12442) are, and in `768..=4023` the bitmap decides; an index outside the bitmap answers `false` (the generator proves the span covers the whole non-short-circuited range, so that arm is unreachable and conservative rather than UB-equivalent).
- `mod.rs::table` (`pub(crate) mod`) — the generated data, below.

**Registration.** `src/runtime/other.rs` registers `("prim_file", "internal_normalize_utf8", 1, internal_normalize_utf8_1)`; the wrapper takes the bytes via `crate::bits::bytes`, normalizes, and builds a plain list of small ints on `ctx.heap()`.
The key is a member of that file's `REAL_BIFS`, so it is served in-crate and is **not** a `RuntimeHooks` seam key (`hook_sets::needs_runtime_hooks("prim_file","internal_normalize_utf8",1)` is `false`, asserted by that file's own tests).
Value shape: `<<65,204,129>>` ⇒ `[193]`, `<<97>>` ⇒ `[97]`, `<<>>` ⇒ `[]`.

**The two C gates, both reproduced.**
1. `erts_get_aligned_binary_bytes` (`erl_bits.h:596`) — only a byte-aligned binary/bitstring has an aligned byte image, here `crate::bits::bytes` → `crate::access::binary_bytes`; a list, an atom, an integer and a non-byte-aligned bitstring (`<<1:3>>`) are all `badarg` (oracle-verified: a list and `97` are `badarg` on OTP 29.1.1, not a charlist reshape).
2. `erts_analyze_utf8` (`erl_unicode.c:1277`, wrapping the static `analyze_utf8` at `:1175`) — anything but `ERTS_UTF8_OK` is `badarg`; this port folds `Decode::Invalid` **and** `Decode::Incomplete(_)` into `BeamError::BadArg`, mirroring the C's single `!= ERTS_UTF8_OK` test.

**Decoder classification** (through the crate's existing `crate::unicode::decode_one`, exactly as implemented there):
- noncharacters are accepted and pass through (`U+FDD0` ⇒ `[64976]`, `U+FFFF` ⇒ `[65535]`);
- a surrogate of **either** kind is `badarg` — high (`U+D800`, second byte `>= 0xA0` after a `0xED` lead) and low (`U+DC00`, which fails the lead-byte test) alike;
- overlong forms are `badarg` (`0xC0`/`0xC1` leads, a `0xE0` lead with second byte `< 0xA0`, a `0xF0` lead with second byte `< 0x90`), as are a stray continuation byte, a code point past `U+10FFFF`, and a truncated tail (`Incomplete`).

**Algorithm internals** (all `mod.rs`):
- `translate(s)` answers `Translate::No`, `Translate::Maybe` (all of `s` is a *proper* chain prefix, so the group stays open) or `Translate::Composed { result, pos }`, by binary-searching `CHAINS` by packed key and `CHAIN_PREFIXES` for prefix liveness; a `pos == 0` hit is reported as `No`, because the C's `return 0` there is indistinguishable from `TRANSLATE_NO`.
- `pack` puts `s[0]` (the combining mark) in the lowest 16 bits and zero in every unused slot, so numeric key order is lexicographic by the C's own right-to-left walk.
- `Norm` holds the four-slot window plus the emitted code points; `emit` is the C `CONS`, and `finish` reverses once into forward order.
- `roll_up(maybe_reopens)` is the one shared body of the C's twice-spelled roll-up loop (`cleanup_norm`'s `<= 0` comparison versus the `TRANSLATE_NO` arm's `== 0` plus its "a maybe means we are not done yet" branch), so the difference is a parameter here.
- `handle_potential_norm` appends only when `cp >> 16 == 0` (the window is `Uint16`), so an astral code point forces an unconditional roll-up.

## The generated table (`table.rs`)
`table.rs` is **GENERATED — never hand-edit it**; it is data (1215 lines) and is exempt from the crate's ~1000-line guideline for that reason.
- `CHAINS: [(u64, u32); 923]` — the **923** composition chains as `(packed key, composed code point)`, sorted by packed key: **672** of two code points, **215** of three, **36** of four.
- `CHAIN_PREFIXES: [u64; 117]` — the **117** *proper* prefixes, packed the same way (61 of depth 1, 50 of depth 2, 6 of depth 3 — the 61 matching the C's `compose_tab_size = 61`, `erl_unicode_normalize.h:34`); this list is what keeps a pending group open instead of rolling it up.
- `CANDIDATE_MAP: [u32; 102]` with `CANDIDATE_MAP_OFFSET: u32 = 24` — the C's `comp_candidate_map` (`erl_unicode_normalize.h:1587`) verbatim, **102** `Uint32` words, one bit per code point, low bit first; 102 words at offset 24 span `cp / 32` in `24..=125`, i.e. `U+0300..=U+0FBF`, the whole range `is_candidate` does not short-circuit.
- The C header holds **1040** `CompEntry` rows across its `compose_tab*` arrays, **923** with a non-zero `res` (a chain end) — that is where 923 comes from.

Regenerate from the repository root (`$HOME` need not be writable):
`ERL_TOP="$PWD" escript erts_rust/crates/beam_bifs/src/unicode/normalize/gen_normalize.escript`
Options: `--from <header>` (default `$ERL_TOP/erts/emulator/beam/erl_unicode_normalize.h`, else found by walking up from the cwd) and `--out <file>` (default `table.rs` beside the script).
The authoritative source is that committed C header, **not** `lib/stdlib/uc_spec/*.txt`: the C table ignores canonical-composition exclusions and its chain keys nest right-to-left (mark first, base last), which no per-code-point property file expresses.
The generator re-validates every structural invariant the C relies on and **fails loudly** (non-zero exit, no output file) on: an unparsable header, a dangling `subs`/`hash` pointer, a hash table that is not exactly what the C's `hashsearch` finds (bucket = `c rem size`, linear probing), a node carrying both a result and children (the C would never reach the children), a chain nesting past the four-code-point window, a chain starting at a code point `is_candidate` denies, two chains packing to one key, or a wrong bitmap length.
Hard rule: to change the tables, change the generator or its input and regenerate — never patch `table.rs`.

## Constraints
Errors are `beam_common::{Result, BeamError}` only; `BadArg` covers both failure classes (a non-byte-aligned argument, an invalid image), and there is no capability-gap arm here: `badarg` never hides a missing capability, and no valid input answers a typed `unsupported_bif`.
No `unsafe` — the crate is `#![deny(unsafe_code)]`, and this node reaches terms only through `crate::bits`/`crate::access`.
Allocation is bounded: the BIF copies the payload out of the term once (`bits::bytes`), `normalize_utf8` materializes the decoded code points in one `Vec` (`with_capacity(bytes.len())`), and `normalize` builds the result in one `Vec::with_capacity(code_points.len())`.
Files target ~1000 lines: `mod.rs` is 460 and holds the algorithm, `table.rs` is data, `gen_normalize.escript` is the generator.
Licensing Apache-2.0: every `*.rs` carries the OTP `// %CopyrightBegin% … // %CopyrightEnd%` block, and the escript carries it in `%%` form.

## Test Strategy
The whole suite is `mod.rs`'s in-module `#[cfg(test)] mod tests` — **13** tests, no `#[ignore]`d case, and nothing here shells out to `erl`.
The tests are `the_candidate_gate_matches_the_c_short_circuits`, `the_packed_key_puts_the_combining_mark_in_the_low_bits`, `translate_answers_no_maybe_and_composed`, `an_empty_image_is_the_empty_list`, `a_lone_ascii_byte_passes_through`, `a_base_plus_a_combining_mark_composes`, `a_three_code_point_chain_composes`, `marks_are_never_reordered`, `a_non_composing_pair_stays_apart`, `only_the_chains_the_table_holds_compose`, `a_dead_end_rolls_the_group_up`, `an_astral_code_point_forces_a_roll_up`, `the_decoder_classifies_exactly_like_analyze_utf8`.
Their expectations are oracle-taken goldens pinned in the bodies (each doc comment names the oracle's answer: `[193]`, `[506]`, `[7844]`/`[7845]`, `[193,778]`, `[7727]`, `[2392]`, `[64976]`, `[65535]`), so the suite is a self-check, not a live cross-check.
End-to-end coverage of the registered BIF lives in the crate's integration suite: `tests/os_bif.rs` calls `prim_file:internal_normalize_utf8/1` through the full registry (`<<"abc">>` ⇒ `[97,98,99]`), beside the neighbouring `prim_file` keys.
**Hard rule — a mark-initial image must NEVER become a differential case or an oracle cross-check.**
The oracle (OTP 29.1.1 / erts-17.1) **aborts the VM** (SIGABRT, `Aborted (core dumped)`, exit 134 — reproduced live with `prim_file:internal_normalize_utf8(<<204,129>>)`, a lone combining mark, and with `<<204,129,65>>`) when the *first* code point of the input is a combining mark: the C's `cleanup_norm` never writes `numpoints` back, so the result term is built over an uninitialised `savepoints[0]` and the crash surfaces the moment that list is inspected.
This port answers the sane thing (the mark passes through), so such a shape gets **in-crate expectations only** — a differential case would compare against a crash rather than a value.

## Routing Table
- `./mod.rs` -> the algorithm: `is_candidate`/`translate`/the `Norm` window/`normalize`/`normalize_utf8`, plus the 13-test in-module suite
- `./table.rs` -> the GENERATED composition data (`CHAINS` 923, `CHAIN_PREFIXES` 117, `CANDIDATE_MAP` 102 words + offset 24) — do not edit, regenerate
- `./gen_normalize.escript` -> the generator: parses `erts/emulator/beam/erl_unicode_normalize.h`, re-validates every hash table against the C's `hashsearch`, flattens the prefix tree, writes `table.rs`

## See Also
- `../CONTEXT.md` -> the `unicode` node (the six `unicode` BIFs, the wide codecs, this module's place among them).
- `../../runtime/CONTEXT.md` -> the runtime-coupled family that hosts the BIF (`runtime/other.rs`, `REAL_BIFS`).
- `../../../CONTEXT.md` -> the crate root (roster census, `BifContext`, typed capability gaps, handoffs).
- `../../../../CONTEXT.md` -> the crate layer: error strategy, `unsafe` confinement, file-size rule.
- `erts/emulator/beam/CONTEXT.md` -> the legacy C core this node is a port of.
