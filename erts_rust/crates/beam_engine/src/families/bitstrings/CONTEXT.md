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
  `make_binary` is the **size-dispatching** result entry: at/below
  `ERL_ONHEAP_BINARY_LIMIT` (64) bytes it builds an on-heap `ErlHeapBits`
  (`make_bits`, its on-heap-only branch), above it the 8-word `[BinRef][ErlSubBits]`
  refc pair over a payload from `Machine::alloc_refc_binary` — the same threshold C
  picks from `ERL_ONHEAP_BITS_LIMIT`.
  `is_refc_binary` is a `#[cfg(test)]` safe wrapper over
  `beam_term::refc::is_refc_binary` for the node's test modules (which live under the
  crate-wide `#![deny(unsafe_code)]`), mirroring `big_parts_safe`.
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
  size gets the frozen representation the limits prescribe — the C
  `i_bs_get_tail`/`i_bs_get_binary_2`/`i_bs_get_fixed_binary` all build through
  `erts_build_sub_bitstring` with no size limit.
- Files ~1000 lines; split by responsibility.

## Test Strategy
Per-module `#[cfg(test)]` unit tests over hand-built operands/direct decoder calls:
`raw.rs` (the subject triple, the refc window, the frozen representation of
`make_binary`/`make_writable`, the machine's enumerable refc handles), `matching/mod.rs`
(the pure decoders' oracle-derived values, the instruction-level empty-binary match, and
`bs_get_tail`/`bs_get_binary2` over a 70-byte refc subject asserting the off-heap refc
result bit-exactly), `matching/commands.rs` (the `bs_match` `binary` command over the same
subject asserting the off-heap refc result), `matching/rooted.rs` (the rooted match path),
`create/tests.rs` (the segment encoders).
`run_tests.rs` is the node's **end-to-end** suite: it builds synthetic `Module`s by hand
and drives them through `Machine::run_label`/`Machine::run_export`, pinning
- a `bs_start_match3` + `bs_get_tail`/`bs_get_integer2` sequence over a **refc subject
  whose window starts one bit in** (`f/1`, entry `Label(1)`; `(1..17)` of the payload reads
  `0x58 0x6B`), and
- a `bs_create_bin` whose `append`/`private_append` segment is **not the first**
  (segment list `[integer,1,1,nil,7,8] [<ty>,2,1,nil,{x,0},all]`, `f/1` exported at
  `Label(1)`), over a heap-binary source and over a 65-byte refc source;
plus a live oracle cross-check (`bitsrun:first3/1` = `7` over `<<16#E5>>`,
`bitsrun:tail16/1` = `516`, `bitsrun:rest16/1` = a 543-bit result and
`bitsrun:rest16bin/1` = a 544-bit `Rest/binary` result, both over the `1..=70` payload and
both asserted to be off-heap refc binaries) that compiles an inline Erlang source with the
system `erlc` at test time and compares against the real system `erl`, **skipping loudly**
when `erlc`/`erl` are absent.
End-to-end evidence also lives in the crate's integration suites (`../../TEST_NOTES.md`),
notably `tests/gaps.rs` (the remaining typed errors), `tests/census.rs` (opcode
coverage) and `tests/stdlib_sweep.rs` (the real installed `base64:encode/1`).

## Remaining typed capability gaps
Every gap is a typed error, never a wrong value. The node's own, in addition to the
crate-level ones in `../CONTEXT.md`:
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
