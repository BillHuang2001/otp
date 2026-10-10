# `beam_engine::families::bitstrings` — the bitstring / binary-syntax family

## Intent
Executes the whole `Bs*` opcode family of the frozen `beam_loader` IR: match entry, the
match reads, the `bs_match` command interpreter, position ops, and construction
(`bs_init_writable` / `bs_create_bin`). Ground truth (read-only, never modify):
`../../../../erts/emulator/beam/emu/bs_instrs.tab` (instruction bodies) and
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

## Routing Table (file → responsibility)
- `mod.rs` → the family `exec`, the shared `fail_step`/`goto_label`, the operand helpers
  (`read_size`, `is_all`, `decode_flags` + the `BSF_*` bits), the safe bit arithmetic
  (`read_uint_be`/`read_uint_le`/`sign_extend`/`extract_bits`), the wide-integer
  (`> 64` bit) byte model, and the `BitWriter` sink.
- `raw.rs` → the family's **single** `#[allow(unsafe_code)]` module: `heap_bits` /
  `subject_bytes` (the subject triple), `ctx_parts`/`inner_orig`/`make_ctx`/
  `set_ctx_start` (the match context), `make_big`/`big_parts_safe` (bignums) and
  `make_bits`/`make_binary`/`make_writable` (result materialisation). All term layout is
  forwarded to `beam_term::{bitstring, refc}`; only the frozen-header dereferences live
  here. Every `unsafe` block carries a `// SAFETY:` rationale.
- `matching/` → the match side:
  - `mod.rs` → `exec` for all `bs_start_match*`/`bs_get_*`/`bs_skip_*`/`bs_match`/
    `bs_match_string`/`bs_test_tail2` opcodes + the decoder unit tests.
  - `match_ctx.rs` → match entry (`enter_match`, the `reuse` path, `resolve_match4_fail`),
    the context operand readers (`read_ctx`/`reread_ctx`) and `bs_set_position`
    (`position_of`).
  - `commands.rs` → the flattened `bs_match` command interpreter (`run_commands`:
    `ensure_at_least`/`ensure_exactly`/`skip`/`integer`/`binary`/`get_tail`/`'=:='`) and
    the `commit` write-back discipline.
  - `reads.rs` → the fixed field readers: `read_integer_field`/`read_wide_integer_field`,
    `read_float_field` + `f16_to_f32`, `decode_utf8`/`decode_utf16`/`decode_utf32` and the
    byte/bit helpers they share.
  - `rooted.rs` → `#[cfg(test)]` rooted-allocation tests for the match path.
- `create/` → the construction side:
  - `mod.rs` → `exec` (`bs_init_writable`, `bs_create_bin`) and `assemble` (the flat
    six-operand segment walk).
  - `segments.rs` → the per-segment-type sources and encoders (`segment_source`,
    `string_source`, `float_source`, `put_float`/`f64_to_f16`, `put_utf8/16/32`).
  - `size.rs` → the segment size validation (`SegSize`/`segment_size`, `seg_nbits`,
    `float_nbits`) and the `SegReason` raise (`fail_seg`).
  - `tests.rs` → `#[cfg(test)]` unit tests for the segment encoders.

## Constraints
- `#![deny(unsafe_code)]` crate-wide; `raw.rs` is the node's only `#[allow(unsafe_code)]`
  module, and every `unsafe` block carries a `// SAFETY:` rationale.
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
- Files ~1000 lines; split by responsibility.

## Test Strategy
Per-module `#[cfg(test)]` unit tests over hand-built operands/direct decoder calls:
`raw.rs` (the subject triple, the refc window, the frozen representation of
`make_binary`/`make_writable`, the machine's enumerable refc handles), `matching/mod.rs`
(the pure decoders' oracle-derived values + the instruction-level empty-binary match),
`matching/rooted.rs` (the rooted match path), `create/tests.rs` (the segment encoders).
End-to-end evidence lives in the crate's integration suites (`../../TEST_NOTES.md`),
notably `tests/gaps.rs` (the remaining typed errors), `tests/census.rs` (opcode
coverage) and `tests/stdlib_sweep.rs` (the real installed `base64:encode/1`).

## Remaining typed capability gaps
Every gap is a typed error, never a wrong value. The node's own, in addition to the
crate-level ones in `../CONTEXT.md`:
- `raw::make_bits` refuses a result of more than `ERL_ONHEAP_BINARY_LIMIT` (64) bytes
  (`unsupported("bitstring: result binary > 64 bytes")`), so a `bs_get_tail` /
  `bs_get_binary2` result above that size — which C materialises as an off-heap
  sub-binary sharing the subject's payload — is not built (use `raw::make_binary` where a
  full result is needed).
- A segment list with no type atom, an unknown segment type, a `string` offset/size
  outside `module.strings`, an integer segment with size `all`, a non-integer value in an
  integer/utf segment, and a truncated segment list are typed `unsupported`/`badarg`.
- A match context whose `orig` word is not a readable subject (a bare off-heap
  `HEADER_BIN_REF`, a non-bitstring) yields
  `unsupported("bitstring: subject is not a readable binary")` — never a guessed byte.
- `bs_match_string` resolves its `StrT` pattern with `module.strings.get(offset..)`
  defaulting to an **empty** pattern when the offset is out of range (which then merely
  fails the match).
- `read_size` refuses a register/negative/oversized size operand
  (`unsupported("bitstring: size is not a non-negative small integer")`);
  `read_uint_le`/`read_uint_le_bits` refuse a non-byte-aligned little-endian field.
- A non-finite float result and a non-float FR slot are the documented `badarith`
  failure; a wide (> 64 bit) segment goes through the byte model (`push_wide_int`,
  `read_wide_integer_field`), so no width is silently truncated.
