# `beam_engine::families::bitstrings` — the bitstring / binary-syntax family

## Intent
Executes the whole `Bs*` opcode family of the frozen `beam_loader` IR: match entry, the
match reads, the `bs_match` command interpreter, position ops, and construction
(`bs_init_writable` / `bs_create_bin`). Ground truth (read-only, never modify):
`../../../../erts/emulator/beam/emu/bs_instrs.tab` (instruction bodies),
`../../../../erts/emulator/beam/emu/generators.tab` / `macros.tab` (the generator macros,
incl. `$NativeEndian` and `$FAIL_HEAD_OR_BODY`) and
`../../../../erts/emulator/beam/erl_bits.c` (the `erts_bs_*` primitives). The shared
failure/label/operand helpers and the safe MSB-first bit arithmetic live in `mod.rs`.

## The subject model
A bitstring **subject** is read through one frozen triple — `(base, size, off)` — from
`raw::heap_bits`: the subject's bit `i` (`0 <= i < size`) is bit `off + i` of the byte
payload at `base`. `off` is `0` for an on-heap `ErlHeapBits` and the refc window's start
modulo 8 for a refc binary (`beam_term::bitstring_parts`), so a window that does **not**
start on a byte boundary is readable bit-exactly. `raw::subject_bytes` returns
`(base, off)` with a slice covering every window bit; every bit reader threads `off` into
its own window-relative bit index. A match context (`ErlSubBits` over an on-heap binary)
is the term `raw::make_ctx` builds; its `orig` word is the subject it reads.

## A field's flags arrive in TWO shapes — both must resolve `native`
- The standalone `bs_get_integer2`/`bs_get_float2`/`bs_get_utf{8,16,32}`/`bs_skip_utf*`
  opcodes carry a **raw `u32` flags operand** (`lower/instr.rs` decodes it with `as_u32`;
  `beam_asm:flag_to_bit/1` encodes `little` as `16#02` and `native` as `16#10`), so
  `BSF_NATIVE` really does reach the readers.
- The `bs_match`-command / `bs_create_bin` segment route carries a **literal flag-atom
  list**, normalized by `mod.rs::flag_bit` (which maps the `native` atom to the target
  order, C's `$NativeEndian`).
`matching/reads.rs::field_is_little(flags)` is the **single** place that resolves both
`BSF_LITTLE` and `BSF_NATIVE` (little on a little-endian target, big otherwise), and every
field reader (`read_integer_field`, `read_wide_integer_field`, `read_float_field`,
`decode_utf16`, `decode_utf32`) goes through it. Testing `BSF_LITTLE` alone silently reads
a `-native` field big-endian — a wrong value, which is the defect this rule prevents.

## Routing Table (file → responsibility)
- `mod.rs` → the family `exec`, the shared `fail_step`/`goto_label`, the operand helpers
  (`read_size`, `is_all`, `decode_flags`/`flag_bit` + the `BSF_*` bits), the safe bit
  arithmetic (`read_uint_be`/`read_uint_le`/`read_uint_le_bits`/`sign_extend`/
  `extract_bits`), the wide-integer (`> 64` bit) byte model, and the `BitWriter` (two-mode: `Dest::Vec` copy-rebuild / `Dest::InPlace` writable accumulator) sink.
  `BitWriter::push_le_groups` is the **single** little-endian layout rule (the field is
  `value mod 2^nbits`, laid out as little-endian byte groups written MSB-first, whose
  final partial group is the LOW remaining bits of the last byte, placed at the current
  bit position — C's `fmt_small_le`/`fmt_big_le` + `restore_and_shift`,
  `erl_bits.c:686/812/891`); `push_uint_le` (a `u64`-sized field) and `push_wide_int`'s
  little branch (a `fmt_big_le` byte array) both delegate to it.
- `raw.rs` → the family's **single** `#[allow(unsafe_code)]` module: `heap_bits` /
  `subject_bytes` (the subject triple), `ctx_parts`/`inner_orig`/`make_ctx`/
  `set_ctx_start` (the match context), `make_big`/`big_parts_safe` (bignums) and
  `make_bits`/`make_binary`/`make_writable` (result materialisation). All term layout is
  forwarded to `beam_term::{bitstring, refc}`; only the frozen-header dereferences live
  here. Every `unsafe` block carries a `// SAFETY:` rationale.
  `make_binary` is the **size-dispatching** result entry: at/below
  `ERL_ONHEAP_BINARY_LIMIT` (64) bytes it builds an on-heap `ErlHeapBits`
  (`make_bits`, its on-heap-only branch), above it the 8-word `[BinRef][ErlSubBits]` refc
  pair over a payload from `Machine::alloc_refc_binary` — the same threshold C
  picks from `ERL_ONHEAP_BITS_LIMIT`.
  `make_bits` zeroes the **whole** reserved data region (padding bytes of the last data
  word included) before copying the payload and then masks the trailing partial byte,
  matching `beam_term::bitstring::write_heap_bits`' invariant.
  `is_refc_binary` is a `#[cfg(test)]` safe wrapper over
  `beam_term::refc::is_refc_binary` for the node's test modules (which live under
  the crate-wide `#![deny(unsafe_code)]`), mirroring `big_parts_safe`.
- `matching/` → the match side:
  - `mod.rs` → `exec` for all `bs_start_match*`/`bs_get_*`/`bs_skip_*`/`bs_match`/
    `bs_match_string`/`bs_test_tail2` opcodes.
  - `match_ctx.rs` → match entry (`enter_match`, the `reuse` path, `resolve_match4_fail`),
    the context operand readers (`read_ctx`/`reread_ctx`) and `bs_set_position`
    (`position_of`).
  - `commands.rs` → the flattened `bs_match` command interpreter (`run_commands`:
    `ensure_at_least`/`ensure_exactly`/`skip`/`integer`/`binary`/`get_tail`/`'=:='`) and
    the `commit` write-back discipline.
  - `reads.rs` → the fixed field readers: `read_integer_field`/`read_wide_integer_field`,
    `read_float_field` + `f16_to_f32`, `decode_utf8`/`decode_utf16`/`decode_utf32`, the
    shared `field_is_little` endianness resolver, and the byte/bit helpers they share.
  - `utf_tests.rs` → `#[cfg(test)]` instruction-arm regressions for the match reads'
    endianness flags (`BsGetUtf32`/`BsSkipUtf32`, and the raw-`BSF_NATIVE`
    float/integer/utf16 reads), oracle-derived.
  - `rooted.rs` → `#[cfg(test)]` rooted-allocation tests for the match path.
  - `tests.rs` → `#[cfg(test)]` unit tests for the pure decoders the `bs_*` match opcodes
    call (`decode_utf8`/`utf16`/`utf32`, `read_float_field`) plus the instruction-level
    regressions for the empty-binary match, `bs_match_string` and the off-heap refc
    results.
- `create/` → the construction side:
  - `mod.rs` → `exec` (`bs_init_writable`, `bs_create_bin`) and `assemble` (the flat
    six-operand segment walk). A non-integer value is the bare `badarg` at **every**
    segment width (checked before the width dispatch, C's `erl_bits.c:952/1152`), and the
    `"integer"` arm routes a value to the shared `push_wide_int` (`mod.rs`, `> 64`-bit
    byte model) whenever `nbits > 64` **or** the value is a bignum, so a bignum is masked
    into a narrow field exactly as C's `fmt_big_be`/`fmt_big_le` do.
  - `segments.rs` → the per-segment-type sources and encoders (`segment_source`,
    `string_source`, `float_source`, `put_float`/`f64_to_f16`, `put_utf8/16/32`). The
    `float` source accepts a float, a small integer **or a bignum** (the nearest finite
    double via `beam_term::number::big_to_f64`, the `big_to_double` port); only a
    non-number or a magnitude that overflows a finite double is `badarg`
    (`erl_bits.c:1341/1373`).
  - `size.rs` → the segment size validation (`SegSize`/`segment_size`, `seg_nbits`,
    `float_nbits`) and the `SegReason` raise (`fail_seg`).
  - `tests.rs` → `#[cfg(test)]` unit tests for the segment encoders.
  - `wide_tests.rs` → `#[cfg(test)]` unit tests for the integer segment's value masking
    (a bignum into a narrow field, big/little endian, signed/unsigned, a literal-operand
    bignum, a runtime `size(Expr)`), oracle-derived.
  - `le_tests.rs` → `#[cfg(test)]` unit tests for the **bit-granular little-endian**
    integer field (12/20/4/1-bit, the `native` variant, a runtime size, a bignum,
    signed, a field following a non-byte-aligned segment), oracle-derived.
  - `float_seg_tests.rs` → `#[cfg(test)]` unit tests for the bignum `float` segment
    (`/float`, `-little`, `:32`, `:16`, the double-range top) and its overflow `badarg`.
- `run_tests.rs` → `#[cfg(test)]` **integration** tests driven through the real public
  execution entries (`Machine::run_label` / `Machine::run_export`) over synthetic
  `beam_loader::Module`s + a `MinimalContext`, plus a live `erlc`/`erl` oracle
  cross-check compiled from an inline source string at test time.

## Constraints
- `#![deny(unsafe_code)]` crate-wide; `raw.rs` is the node's only non-test
  `#[allow(unsafe_code)]` module (`run_tests.rs` carries a local allow for the frozen
  `bitstring_parts`/`write_refc_binary` readers its harness builds terms with), and every
  `unsafe` block carries a `// SAFETY:` rationale.
- Errors are `beam_common::{Result, BeamError}` only — no crate-local error enum, no
  `unwrap()` on runtime data, no panic, never a wrong result. A match failure branches to
  the instruction's fail label (or raises `{badmatch, Value}` when the label is `NO_FAIL`);
  a construction-segment failure raises the bare `SegReason` atom (`badarg` /
  `system_limit`), never the matching side's `fail_step`.
- Term construction/inspection is **only** `beam_term`; a match context's interior is
  never re-derived by hand.
- Term-constructing arms allocate through the rooted seam
  (`raw::make_ctx`/`make_bits`/`make_binary`/`make_writable`/`make_big` →
  `crate::terms::reserve` → `Machine::alloc_rooted`) and re-read the context operand after
  an allocating read (`reread_ctx`), per the frozen **reserve, then read, then write**
  ordering.
- Every **match result** binary (`bs_get_tail`, `bs_get_binary2`, the `bs_match`
  `binary`/`get_tail` commands) is built through `raw::make_binary`, so a result of any
  size gets the frozen representation the limits prescribe.
- Files ~1000 lines; split by responsibility. `matching/mod.rs` holds only the family's
  opcode dispatch table plus the per-opcode arms; its unit tests live in
  `matching/tests.rs`, and new test cases go to `matching/tests.rs` /
  `matching/utf_tests.rs` / `matching/rooted.rs`.

## Test Strategy
Per-module `#[cfg(test)]` unit tests over hand-built operands/direct decoder calls (`raw.rs`,
`mod.rs`, `matching/tests.rs`, each of the `create/*_tests.rs` and `matching/*_tests.rs`
modules above), an end-to-end suite in `run_tests.rs` through `Machine::run_label`/
`run_export`, and live `erlc`/`erl` oracle cross-checks (which skip loudly when the
toolchain is absent).
Every expected byte vector in the newer suites was taken from the system `erl`
(OTP 29.1.1 / erts-17.1) — never invented.
Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine
--lib families::bitstrings` (green: 104 tests) and the whole crate
`... -p beam_engine --no-fail-fast` (1595 passed / 0 failed across 74 targets); the build
`... -p beam_engine --all-targets` is warning-free. Re-run these for the live counts.
**Differential evidence:** the project gate `erts_rust/tools/differential` was run
per-case over the whole `binary/` group plus every bitstring/binary-shaped corpus id
(62 cases: `binary/*`, `semantics/bit_size_of_bitstring`, the `binary*`/bit-size BIF and
`mexec_extra` ids, the four `gc/*binary*` cases, `phash2/non_byte_aligned_bitstring`,
`errors/badmatch_binary_value`, `wire_extra/eval_wide_binary_segment_*`) — **62 agree /
0 mismatch / 0 unsupported**, all exit 0.
**Corpus coverage hole (tools-owned):** no corpus case exercises a **bit-granular
little-endian** integer write, a `float` segment, a `float-native` pattern or a
`utf32-little` pattern (the `binary/bignum_*` LE cases are all byte-aligned 16/64-bit
fields), so those shapes are covered only by this node's oracle-derived unit tests.

## Notes for Agents
- **A test term must be allocated in a context that outlives its use.** `MinimalContext`'s
  region heap is pointer-stable and keeps earlier regions alive, but a context created in
  an inner block drops its arena when the block ends — an `Eterm` built in a throwaway
  context and then fed to a `Machine` (or a different context) is a use-after-free whose
  assertion passes only when the freed bytes still hold the term.
- `matching/reads.rs::field_is_little` is the one endianness rule for match reads;
  `mod.rs::BitWriter::push_le_groups` is the one for writes. Never test `BSF_LITTLE` alone.
- `mod.rs`'s `BSF_NATIVE` is a **live** flag (a raw `u32` flags operand can carry
  `16#10`), not a flag-table placeholder.
- A construction-failure with a **nil** fail label is a **raise** (`fail_seg` →
  `raise_seg_fail`), matching C's `$FAIL_HEAD_OR_BODY` (`macros.tab:266-278`: a nil label
  falls to `goto find_func_info` → `handle_error`), and it IS reachable from compiled
  code (`<<1, X/binary, 2>>` emits a non-append `binary` segment with `{f,0}`). Verified
  oracle-identical for that shape — do not "fix" it into a silent continue.

## Remaining typed capability gaps
Every gap is a typed error, never a wrong value. Each remaining site is a guard for a
**malformed `.beam`** or an internal invariant the installed compiler cannot reach
(verified by a `beam_disasm` census over the installed OTP: 9036 `bs_match` instructions
whose tags are exactly the 7 `run_commands` covers, 1331 `bs_start_match3` and zero
`bs_start_match4`, and no emitted unknown flag atom / segment type):
- A segment list with no type atom, an unknown segment type, a `string` segment with an
  unknown/negative/overflowing/out-of-table `StrT` offset+size (or size `all`), an integer
  segment with size `all`, a truncated segment list, and a literal index out of range are
  typed `unsupported`/`badarg`.
- A match context whose `orig` word is not a readable subject (a bare off-heap
  `HEADER_BIN_REF`, a non-bitstring) yields
  `unsupported("bitstring: subject is not a readable binary")` — never a guessed byte.
- `bs_match_string` resolves its `StrT` pattern with `module.strings.get(offset..)`: an
  offset **strictly beyond** the table is the typed
  `unsupported("bs_match_string: string table offset out of range")`, while an offset
  **exactly at** the end is a valid **empty** pattern (a 0-bit match succeeds).
- `read_size` refuses a negative, oversized or non-integer size operand (a
  **typed-register** size — `{tr,Reg,Type}`, what a runtime `size(Expr)` emits — is
  accepted and resolved through `read_operand`); `read_uint_le`/`read_uint_le_bits` refuse
  a non-byte-aligned little-endian field, but that refusal is **latent**: the bit-granular
  read route goes through `read_wide_integer_field`, so no compiled shape reaches them.
- `bs_get_float2`'s unsupported-size site and `make_bits`' `> 64`-byte branch are
  unreachable from production code (C returns `THE_NON_VALUE` for an unemitted size ⇒ a
  plain match failure; `make_binary` dispatches above the limit to the refc pair first);
  `make_big`'s zero-width-bignum guard is unreachable (a bignum in a 0-bit field yields
  `<<>>`), as are the `short byte buffer` guards.
- A non-finite float result and a non-float FR slot remain the documented `badarith`
  failure; a bignum `float` segment that overflows a finite double remains `badarg`
  (C's `big_to_double < 0`).

## Out-of-node findings (do not fix here)
- **`beam_rt/src/eval/mod.rs` (the `-eval` tree-walker) diverges from the oracle** on
  shapes this node gets right: `BitBuf::push_integer_bits(little = true)` reverses the byte
  order then truncates, so it emits `<<2,0:4>>` where ERTS puts the trailing partial
  byte's bits MSB-aligned (`<<2,1:4>>`, C's `fmt_small_le`/`restore_and_shift`); it also
  returns a typed `unsupported` for `float` segments, `float-native` **patterns** and
  `utf32-little` patterns. The **compiled** route (this node + `beam_rt`'s engine path) is
  oracle-exact for all of them, and `differential compare` drives the `-eval` route, so
  those ad-hoc `compare` mismatches are not this node's.
- **`tools/differential` corpus coverage hole** (see Test Strategy).

## `append` / `private_append` — in place while the payload is writable

`create::assemble` walks the segment list into a `BitWriter` whose destination is either
`Dest::Vec` (the copy-rebuild form: a growable `Vec<u8>` materialised once by `raw::make_binary`)
or `Dest::InPlace` (a `raw::InPlace` sink over a `bs_init_writable` payload). The in-place mode is
entered only for a `private_append` in the **first** segment position whose source is an off-heap
refc binary with `start == 0` over a payload the machine registered through
`Machine::mark_writable` (`raw::writable_append_target`); it writes the new bits at the payload's
own window end and publishes the **same** `Eterm` with a widened `ErlSubBits.end`
(`raw::set_ctx_end`) — C's `erts_bs_private_append_checked` writable arm (`erl_bits.c:1943-2020`).

Promotion, not failure, covers the two bounds: a write that would leave the payload's capacity, and
a segment source that **overlaps** the payload, both copy the already-written prefix out
(`raw::InPlace::into_bytes`) and continue in `Dest::Vec`, so the result is byte-identical to the
copy-rebuild path and the accumulator's window is left unbumped. A **failed** build never bumps
`end` at all (stricter than C, which bumps in its sizing pass, `erl_bits.c:1975`). `push_bits_from`
fast-paths whole bytes when both the source offset and the write position are byte-aligned and
stays bit-exact per-bit otherwise.

Residual (documented, owner `beam_alloc`/`beam_rt`): the payload never *grows*, so a loop that
outgrows the `bs_init_writable` capacity falls back to the copy-rebuild form and stays O(n²);
C's `GROW_PROC_BIN_SIZE` (`erl_bits.c:70-73`) + `apparent_size`, the
`BIN_FLAG_WRITABLE`/`ACTIVE_WRITER` observables and `binary:referenced_byte_size/1`'s
capacity-vs-size answer are unmodelled — see `../../HANDOFF_bs_init_writable.md`.
