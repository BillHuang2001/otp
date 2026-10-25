# `families/bitstrings/create` — the family's construction half (`bs_init_writable` / `bs_create_bin`)

## Intent
The construction side of the bitstring family: `bs_init_writable` (a 0-bit refc window over a capacity-carrying payload this machine registered writable) and `bs_create_bin` (the segment walk that appends field bytes to that accumulator).
It owns the **shape rules** — which `bs_create_bin` is an in-place accumulator build, and with which fields — and the **bytes**, one encoder per segment type.
It also owns the seam a native lowering codes against (the shape rules stay here, the emission rules stay in `src/jit/**`).
Ground truth is `erts/emulator/beam/erl_bits.c` (`erts_bs_private_append_checked`, the `erts_bs_put_*` family, `sb->end = new_position`); this directory's own bodies are the in-tree reference every native path is pinned against.

## API Surface
- `mod.rs` → `exec` (the `bs_init_writable` and `bs_create_bin` instruction bodies) and `assemble` (the flat six-operand `[Type, Index, Unit, Flags, Value, Size]` walk).
- `prepared.rs` → `prepare` folds that flat list once per `(module image, function, code offset)` into a `CreateProgram` of `PreparedSeg` variants for `assemble_prepared`; a shape it declines keeps the generic walk and its exact typed error.
- `size.rs` → the segment size validation (`SegSize`/`segment_size`, `seg_nbits`, `float_nbits`) and the `SegReason` raise (`fail_seg`).
- `segments.rs` → the per-segment sources and encoders (`segment_source`, `string_source`, `float_source`, `put_float`/`f64_to_f16`, `put_utf8/16/32`) plus the cold-body host the hot block calls.
- `seam.rs` → the family-private surface `src/jit/**` reads as `crate::families::bitstrings::create::seam`: `append_plan(module, segments) -> Option<AppendPlan>` (pure, takes no `Machine`), the run-time doors `open_append` (`#[cfg(test)]`, the byte-aligned reference answer) and `open_append_any` (the general, unit-gated door), and the bit-granular write `write_bit_field`.
- `AppendPlan { source, unit, tail_bits, fields, binary_all }` with `Field::{Bits { value, bits }, ConstBytes { bytes }}`.
- `AppendPlan::binary_all` carries the whole-binary tail shape: `Some(source)` exactly when the plan is a `private_append` of the accumulator followed by **one** `binary all` segment and nothing else — an `integer`/`string` field before it, or any segment after it, keeps the plan `None`, because a run-time width fixes no static offset for anything later.
  Such a plan's `fields` is empty and its tail is the source's own run-time width, so `tail_bits` is `0` for it.
- `tail_bits` means the **static fields' summed width** — exactly the bits the accumulator's window end advances by — and never a run-time figure.
- `unit` is the accumulator (first) segment's append unit (`1|2|4|8`); the doors hand it to C's append-**position** gate (`append_unit_allows`), so a shape the family raises `badarg` on is never reported writable.
- Every operand a plan or a door hands back is a **locatable** register: an x-register or an addressable `Y` slot; the tier owns the `Y` frame-window guard.

## Constraints
- **Never a wrong result.** A shape the seam cannot express exactly is `None` (the family body answers) and a shape a door cannot serve is a refusal, never an approximation.
- Errors are `beam_common::{Result, BeamError}` only; a non-integer field value is the bare `badarg` at every width, a non-whole `size * unit` is `badarg`, a size overflow is `system_limit`.
- The doors are `&Machine` queries and mutate nothing; the build's only mutations are the payload bytes and the single `ErlSubBits.end` write, in that order.
- `write_bit_field` is the family's one bit-granular write rule (MSB-first, low `nbits` bits, entered byte cleared, window-end byte masked) and is re-execution idempotent.
- `open_append` stays `#[cfg(test)]`: it is the byte-aligned door's reference answer, not a production entry.
- The seam is `pub(crate)` and consumed only by `src/jit/**`; do not add a `Field::Binary` — the `binary all` tail is not a static field and its copy is the runtime door's.
- Apache-2.0 header on every `.rs` (byte-exact; the repo license scanner enforces it).

## Test Strategy
- `tests.rs` → the segment encoders' unit tests.
- `prepared_tests.rs` → the prepared-versus-generic equivalence suite (same bytes, same bit count, same typed error), plus the per-instruction reservation leaving `RefcBinary::growth_counters` untouched.
- `seam_tests.rs` → the seam's accept and decline sets, both doors' run-time answers (including the unit gate), the word-identity/no-growth pin, and `write_bit_field`'s byte-for-byte agreement with the family's own `BitWriter` at every width in `1..=64`.
- `inplace_tests.rs`, `grow_tests.rs`, `wide_tests.rs`, `le_tests.rs`, `float_seg_tests.rs` → the in-place append window, the growth/relocation schedule, the wide-integer masking, the bit-granular little-endian field, and the bignum `float` segment.
- `../run_tests.rs` → the end-to-end rows through the real public execution entries, plus a live `erlc`/`erl` oracle cross-check.

## Notes for Agents
- The seam decides *plannability*; a tier's shape predicate decides *emittability*. A change here can silently widen or narrow what `src/jit/baseline/emit/bitstr_inline/create.rs` may emit — update both sides and the tier's pins (`jit/baseline/bs_create_bin_tests.rs`, `jit/baseline/foreign_kernel_tests.rs`) together.
- `prepared.rs` resolves the segment list once per instruction; a per-element re-walk in `assemble` is the fallback for declined shapes, not the hot path.
- Inlining the four cold segment bodies (`float_source`/`put_float`/`put_utf8`/`put_utf16`/`put_utf32`) was measured a net regression — do not re-attempt it.

## See Also
- `../CONTEXT.md` → the bitstring family node: the instruction set, the matching side, and the construction seam's contract in full.
- `../../../jit/baseline/emit/bitstr_inline/CONTEXT.md` → the native routes that consume this seam, the `binary all` route among them.
- `../../../jit/baseline/emit/bitstr_inline/create.rs` → `CreateBinShape::of`, the tier's per-shape predicate.