# `bitstring/` — the bitstring layout, and the shape check over it

## Intent
The two halves of `beam_term`'s bitstring support, split along the *unchecked layout* / *shape-validating*
seam: `../bitstring/mod.rs` owns the frozen interior layout and its raw readers, `checked.rs` owns the layer
that proves a block is well shaped before it hands out a pointer.
Ground truth: `../../../../erts/emulator/beam/erl_bits.h:60-176` (the `ErlSubBits`/`ErlHeapBits` definitions,
`NBYTES`, `heap_bits_size`) and `erl_bits.c:490-519` (`erl_sub_bits_init`).

## API Surface
### `mod.rs` — the unchecked layout layer
The `ErlHeapBits` (`[thing_word, n_bits, data…]`, `HEAP_BITS_SUBTAG`) and `ErlSubBits`
(`[HEADER_SUB_BITS, base_flags, start, end, orig]`, 5 words) writers — `write_heap_bits`, `write_binary`,
`write_sub_bits` — and the unchecked readers `sub_bits_parts`, `bitstring_parts`, `bit_at`, plus the crate's
`pub(crate)` `raw_bit` bit test.
Constants: `SUB_BITS_{BASE,START,END,ORIG}_WORD`, `ERL_SUB_BITS_FLAG_{MASK,MUTABLE,VOLATILE}`,
`BITSTRING_POINTER_FLAG_MASK`; sizing: `bit_count_to_bytes` (`NBYTES`), `heap_bits_size_words`
(`heap_bits_size`), both overflow-free for every input.
`float_bits`/`float_value` live here too: the crate's **single** public definition of the float payload read
(`compare`, `hashing` and `thing` forward to it).
Bits are MSB-first inside each byte; the writers zero the trailing padding bytes and clear the unused trailing
bits, so a round-trip is canonical.
Readers are **heap-agnostic and allocation-free**: the caller reserves the words and passes `hp: *mut Eterm`.

### `checked.rs` — the shape-validating layer
`try_sub_bits_parts`, `try_bitstring_parts`, `try_bit_at`, re-exported by `mod.rs` so every public path is
`beam_term::bitstring::*` / `beam_term::*` exactly as before.
All three route through one private `checked_bits` → `CheckedBits::{Sub,Heap}`, so the rules cannot drift
between them; `try_bit_at` then reuses the same `raw_bit` bit test on the validated base/offset pair.

## The enforced invariant (quotable)
A checked reader is a typed refusal, never a wrong pointer or a wrong bit: a word that is not a term
(`is_term_word` false — `THE_NON_VALUE`, a bare `HEADER`) or a term that is not boxed (an immediate, a cons
cell) is `TermError::NotATerm`; a `HEADER_SUB_BITS` block must have `start <= end`, a **non-null** masked base
pointer, and a recognised `orig` — a boxed `BinRef` with a non-null `val`, or a boxed **on-heap** bitstring
whose header arity agrees with its own bit-size word (`arityval(hdr) + 1 == heap_bits_size_words(size)`); an
on-heap bitstring header must satisfy that same arity/size agreement on its own block; every other boxed header
(tuple, map, float, bignum, fun, record, `BinRef`, …) is `TermError::BadBitstring`; and `try_bit_at`
additionally refuses `i >= size_bits` before the byte is touched.

## Constraints
- **The honest limit is provenance.** The check proves *shape* only: a block that has been freed, recycled or
  moved is word-for-word indistinguishable from a live one (this crate has no region registry and no accessor
  validates provenance — `../../CONTEXT.md`, *Notes for Agents*). That is why the `try_*` forms stay `unsafe`,
  with `# Safety` narrowed to "the boxed pointer must be readable".
- The unchecked readers must stay **panic-free on corrupt data** (a debug-profile panic on runtime data is
  forbidden): `bitstring_parts` saturates `end - start` and `bit_at` answers `false` on an overflowing
  `offset + i`.
- A `start == end` window is legal (zero bits), not a corruption; a null masked base is refused on its own,
  whatever `orig` says.
- `unsafe` is confined to the deref/read sites of both files; every `unsafe fn` documents `# Safety` and every
  `unsafe` block a `// SAFETY:` rationale. `mod.rs`'s `WORD_BYTES` is `const`-asserted to `8` (the frozen
  `ARCH_64` ABI).
- Nothing here allocates, validates liveness, or knows about node namespaces: a `BinRef`/`orig` word is read
  verbatim.

## Test Strategy
In-module tests, moved with the code they cover: `mod.rs` pins the sizing boundaries, the exact
`ErlHeapBits`/`ErlSubBits` raw words, MSB-first round-trips for bit lengths 1..=64, the short-byte-count and
overflow refusals and the float readers; `checked.rs` pins every refusal class (poison base, null base,
`end < start`, unrecognised/floating/immediate/tuple `orig`, null-`val` `BinRef`, arity-disagreeing heap block,
every non-bitstring boxed header, non-term and non-boxed words) and that the unchecked readers stay panic-free
on the same blocks.
The public-surface fuzz-shaped sweep over the checked readers is `../../tests/bitstring_checked.rs`.

## Routing Table
- `mod.rs` → the frozen layout: constants, sizing helpers, the three writers, the unchecked readers
  (`sub_bits_parts`/`bitstring_parts`/`bit_at`/`raw_bit`) and the float readers
- `checked.rs` → `checked_bits`/`CheckedBits`/`is_recognised_origin` + the three `try_*` readers (private
  module, re-exported by `mod.rs`)
