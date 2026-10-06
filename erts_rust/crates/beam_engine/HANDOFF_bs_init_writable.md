# Handoff: `bs_init_writable` + `bs_create_bin` `append` / `private_append`

**Status: implemented** in `beam_engine` (copy-rebuild). Both forms previously returned a
typed `error::unsupported(..)`; they now build a correct result, including results larger
than the on-heap binary limit. This note records what landed, the approach, and the honest
divergences that remain.

## What now works

| Instruction / operand | Where it is handled | Behaviour |
|---|---|---|
| `bs_init_writable` (`Instr::BsInitWritable`) | `src/families/bitstrings/create.rs`, the `Instr::BsInitWritable` arm | Capacity from `x(0)` when it is a non-negative small integer, else `DEFAULT_WRITABLE_SIZE = 1024` (C `erts_bs_init_writable`, `erl_bits.c:2021`); builds a **0-bit** refc binary (`raw::make_writable`) and writes it to `x(0)`. |
| `bs_create_bin` first segment `append` / `private_append` | `src/families/bitstrings/create.rs`, the `"append" \| "private_append"` arm of `assemble` | Seeds the bit writer with the source's bits (`raw::heap_bits` + `raw::subject_bytes`, which now accept a refc root), then appends the remaining segments. |
| `bs_create_bin` result > 64 bytes | `src/families/bitstrings/create.rs` → `raw::make_binary` | Builds an off-heap refc binary (`beam_term::refc::write_refc_binary` over a `beam_alloc::RefcBinary` payload) instead of the former `> 64 bytes` typed error. |
| Reading a refc result | `src/families/bitstrings/raw.rs` (`heap_bits`) and `src/terms.rs` (`binary_bytes`) | A refc root is accepted as a readable subject / byte payload. |

## Approach: copy-rebuild, not in-place growth

C's `private_append` (and `append` on a writable source) **reallocates and extends** the
source's off-heap payload in place and returns a new `ErlSubBits` aliasing it, so an append
loop is amortised O(n). The engine's payloads are immutable — `beam_alloc::RefcBinary` is
immutable and `BinRef.val` has no setter — so an append instead seeds a bit vector with the
source's bits, appends the remaining segments, and materialises one fresh term. The result
bytes are identical for every successful build, so the transformation is observationally
correct; it is not the same algorithm.

The read-side gap the old note flagged is closed: a refc root is an `ErlSubBits` over a
`BinRef`, and `raw::heap_bits` recognises it via the public `beam_term::refc::is_refc_binary`
+ `beam_term::bitstring::bitstring_parts` APIs, so a partially-built binary can be read back.

## Dependency edge (escalation to the crate-layer node)

`beam_engine` gained `beam_alloc = { path = "../beam_alloc" }` (a wave-0 → wave-4 downward
edge — directionally legal, but a new layer-DAG edge that must be recorded upstream). The
off-heap payload handles live on `Machine` as `Vec<beam_alloc::RefcBinary>` through
`Machine::alloc_refc_binary(&[u8]) -> Result<*mut u8>` (returning `RefcBinary::data_ptr()`).
The vector is deliberately **not** cleared by `Machine::reset`, because a returned term may
outlive the reset. All frozen `BinRef`/`ErlSubBits` layout construction stays in
`beam_term::refc` — the engine never hand-rolls the layout.

## Honest divergences (unmodelled in M1)

* **No in-place amortised growth.** A long append loop is O(n²) rather than amortised O(n).
* **`BIN_FLAG_WRITABLE` / `BIN_FLAG_ACTIVE_WRITER` are not modelled.** The engine's result is
  an ordinary non-writable refc binary; the next append simply seeds from it.
* **`binary:referenced_byte_size/1`'s "capacity vs size" answer** (C's `apparent_size`) is not
  modelled — the writable buffer's capacity is invisible to the engine (`make_writable`
  builds a 0-bit window, as C's `build_writable_bitstring` reports `current_size = 0`).
* **Bit-offset windows on a refc root are refused.** `raw::heap_bits` returns `None` when a
  refc root's window does not start on a byte boundary (the engine only ever builds
  byte-aligned ones); a `bs_start_match` over such a subject is the documented typed error
  rather than a wrong read.

## Evidence

The end-to-end gate is `base64:encode/1` over the real system stdlib `base64.beam`, driven
through `tests/support/mod.rs`'s module host: a 60-byte input yields the 80-byte oracle string
`<<"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7">>`
(an 80-byte result is above the 64-byte limit, so it exercises the refc path). Focused unit
tests for the new pieces live in `src/families/bitstrings/raw.rs`
(`make_binary_picks_the_frozen_representation`, `make_binary_masks_the_trailing_bits_of_a_refc_result`,
`make_writable_returns_a_zero_bit_refc_window`).

`bs_init_writable` appears ~71 times across the installed OTP `ebin` tree (`base64`,
`binary`/`binary:encode_hex*`, `beam_asm`, `beam_dict`, `beam_ssa_check`, `beam_types`,
`asn1ct_gen`, …); the construct is the compiler's standard accumulator builder.
