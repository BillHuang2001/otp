# Handoff: `bs_init_writable` + `bs_create_bin` `append` / `private_append`

**Status:** deliberately **not implemented** in `beam_engine`. The two opcode forms below
still return a typed `error::unsupported(..)`. This note records exactly why, and the two
candidate ways to close the gap.

## What is unsupported

| Instruction / operand | Where it is rejected | Current error |
|---|---|---|
| `bs_init_writable` (`Instr::BsInitWritable`) | `src/families/bitstrings/create.rs`, the `Instr::BsInitWritable` arm | `unsupported("bs_init_writable: needs a writable off-heap ref-counted binary")` |
| `bs_create_bin` with a first segment of type `append` or `private_append` | `src/families/bitstrings/create.rs`, the `other =>` fall-through of `assemble` | `unsupported("bs_create_bin: segment type append")` / `… private_append` |

`bs_create_bin` `string` constant segments — the third documented gap — **are now
implemented** (see `tests/bitstrings_extra_string.rs`); this note does not cover them.

## Why it is not implementable inside the engine's frozen seams

Ground truth:
* `emu/bs_instrs.tab:285-289` (`bs_init_writable`), `:541-569` (`BSC_APPEND` /
  `BSC_PRIVATE_APPEND` allocation), `:627-633` + `erl_bits.c:1641` (`erts_bs_put_string`).
* `erl_bits.c:1724` `erts_bs_append_checked`, `:1927` `erts_bs_private_append_checked`,
  `:1674` `build_writable_bitstring`, `:2021` `erts_bs_init_writable`.

The semantics that must be reproduced:

1. `bs_init_writable` allocates a **fresh, mutable, off-heap ref-counted binary**
   (`erts_bin_nrml_alloc(1024)`, or the size in `x(0)`) and returns an `ErlSubBits` match
   context whose `orig` is a `BinRef` (`HEADER_BIN_REF`) over that binary, flagged
   `BIN_FLAG_WRITABLE | BIN_FLAG_ACTIVE_WRITER`.
2. A following `bs_create_bin` whose first segment is `append`/`private_append` **grows and
   reallocates that same off-heap binary in place** (`erts_bin_realloc`), rewrites the
   `BinRef`, and returns a **new `ErlSubBits` that aliases the same payload**. The returned
   term is itself writable, so the pattern is re-applied in a loop (e.g.
   `base64:encode_binary/4`, `binary:encode_hex2/3`).
3. `append` (public) may also be applied to a non-writable bitstring; it then allocates a
   writable binary and copies the old contents into it.
4. The result can be arbitrarily large (the whole point of the construct), and it is
   *shared* with the context: `bs_start_match4` `resume` later reads the bytes written so
   far out of the same payload.

The engine cannot express any of steps 1–4:

* **No off-heap binary.** The engine builds only on-heap `ErlHeapBits` terms through
  `raw::make_bits`, capped at `ERL_ONHEAP_BINARY_LIMIT = 64` bytes (`beam_term::refc`,
  `beam_engine/src/families/bitstrings/raw.rs:41`). There is no way to allocate the
  ref-counted payload, and no `BinRef`-rooted term the engine may write.
* **No mutable/aliasing storage.** An on-heap bitstring owns its bytes; two terms cannot
  share one payload. The writable context and every result of an append must alias.
* **Frozen seams.** The crate depends on exactly `beam_common`, `beam_term`, `beam_loader`
  and `libc`; it has no `beam_alloc` edge. `ExecutionContext` (`src/context.rs`) is a frozen
  public trait and `alloc_words` returns heap words valid **only until the next
  `ensure_heap`/GC**, so it cannot hand out a stable, growable, off-heap payload either.
* **`HEADER_BIN_REF` is rejected by the read path.** `raw::heap_bits` explicitly returns
  `None` for a `HEADER_BIN_REF` match context, and `raw::subject_bytes` errors with
  "subject is not an on-heap binary", so a writable context could not even be matched.

A partial implementation would be **wrong**, not merely incomplete: dropping the
writable/aliasing property, or silently truncating the result at 64 bytes, changes the
observable bytes of any program whose appended output exceeds the on-heap limit. The
engine therefore keeps the typed `unsupported` rather than guessing.

## Candidate fixes (both outside this node's scope)

### (a) A binary-allocation seam on `ExecutionContext`, supplied by `beam_rt` — **preferred**

Add (a future round) one method to the frozen `ExecutionContext` trait, e.g.

```rust
/// Allocate an off-heap ref-counted binary payload of `size` bytes and return a
/// writable `ErlSubBits`/`BinRef` term over it, registered for GC liveness by the
/// context. Valid until the next `ensure_heap`.
fn alloc_writable_binary(&mut self, size: usize) -> Result<Eterm>;
```

`beam_rt` implements it over `beam_alloc::RefcBinary` + `beam_term::refc`, registers the
handle in `beam_gc::OffHeap`, and supplies the lifetime/GC rules; the engine only calls it
and then writes the payload through the family's `raw.rs` (the one `unsafe` module).

* Pros: keeps the Stage-1/Stage-5 DAG acyclic and the "the engine owns no allocator"
  discipline; keeps all raw layout work in `beam_term::refc`/`beam_alloc` (the owners of the
  frozen `BinRef`/`ErlSubBits`/`RefcBinary` layout); the GC liveness of the writable payload
  is owned where MSO already is.
* Cons: expands a trait the crate `CONTEXT.md` declares frozen; needs the realloc/grow
  protocol (`erts_bin_realloc` + `update_wb_overhead` semantics) to be modelled by the
  context, since growth invalidates the previously handed-out pointer; the engine must learn
  to *re-read* the context term after an append rather than caching the payload pointer.

### (b) An engine → `beam_alloc` dependency edge

Give `beam_engine` a direct `beam_alloc` dependency and let `create.rs` build
`RefcBinary`/`BinRef`/`ErlSubBits` itself through `beam_term::refc` + `beam_alloc::RefcBinary`.

* Pros: no trait change; the engine holds the payload handle directly.
* Cons: adds a layer-DAG edge (wave 0 → wave 4) that the crate layer's routing notes flag as
  an escalation, and moves off-heap term-layout construction into the engine — the engine
  would hand-roll a layout it does not own, exactly the thing the current design forbids;
  `beam_alloc`'s `RefcBinary` refcount/`RefCount` contract would then also have to be
  honoured by the engine.

Either way the work must also cover the read side: a writable context is an `ErlSubBits`
over a `BinRef`, so `raw::heap_bits`/`subject_bytes` need a `HEADER_BIN_REF` path (or the
context must materialise the bytes back into a heap term at each append) before
`bs_start_match4 resume` can read a partially-built binary.

## Evidence that this is a real, load-bearing gap

`bs_init_writable` appears 71 times across the system OTP tree's `ebin`. Confirmed
end-to-end in the dialect of this crate's own fixtures:

```
erl -noshell -eval 'io:format("~p~n",[beam_disasm:file(code:which(binary))]),halt().'
```

* `base64.beam` — `base64:encode/1` and `encode/2` (`bs_init_writable` at
  disassembly line 87/106, with the initial size `256` left in `x(0)`), feeding
  `base64:encode_binary/4`, which uses `bs_create_bin` `private_append`.
* `binary.beam` — `binary:encode_hex1/2` (`bs_init_writable` at line 818) feeding the
  `binary:encode_hex2/3` loop (`private_append` at disassembly line 918).
* Also `beam_asm`, `beam_dict`, `beam_ssa_check`, `beam_types`, `asn1ct_gen`,
  `asn1ct_value`, …

Neither `bs_init_writable` nor `append`/`private_append` can produce a correct result in
`beam_engine` until capability (a) or (b) lands upstream.
