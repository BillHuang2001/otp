# HANDOFF — natively-inlined `bs_create_bin` construction arm

Status: **NOT IMPLEMENTED** — blocked on one missing `CodeBuffer` method, the 8-bit store
`mov_mr8`, in `src/jit/emitter/encode.rs`, which this node may not edit.
One-line unblock: add `mov_mr8` to that file (`CodeBuffer`); the arm then follows from it.

## The blocker (read this first)

`CodeBuffer`'s complete store surface is `mov_mr(base, disp, src)` — **64-bit only**.
There is no `mov_mr8` / `mov_mr16` / `mov_mr32`. Therefore:

| `Field::Bits` width | writable today? | why |
|---|---|---|
| 8 (`cowboy_bstr:to_lower/1`) | **no** | needs a 1-byte store |
| 16 | **no** | needs a 2-byte store (an 8-byte store overruns) |
| 24 / 40 / 48 / 56 | **no** | needs byte stores |
| 32 | **no** | a 64-bit store writes 8 bytes; `open_append` only guarantees `end_bits + tail_bits <= capacity_bits`, so 4 extra bytes can overrun the payload |
| 64 | yes | the only width `mov_mr` stores safely |
| `Field::ConstBytes` | **no** | needs byte stores, or 8-byte chunks + a byte tail |

Both motivating shapes are therefore unreachable without the encoder addition:
`cowboy_bstr:to_lower/1` is `append + integer 8`, and
`cowboy_bstr:capitalize_token/1` additionally needs `string "-"` (`ConstBytes`).

`jit/emitter/CONTEXT.md:39` prescribes the resolution: **add a tested method, do not guess**.
The raw primitives (`emit_rex`, `emit_mem_modrm`, `emit_u8`) are private to `jit::emitter`,
so the method must live in `encode.rs`'s `impl CodeBuffer` block:

```rust
/// `mov byte [base + disp], src` — an 8-bit store (`88 /r`, `REX.W` clear).
///
/// **The byte-register codes are ambiguous without a `REX` prefix.** With no prefix, codes
/// 0..=3 name `AL`/`CL`/`DL`/`BL` but codes 4..=7 name `AH`/`CH`/`DH`/`BH`, **not**
/// `SPL`/`BPL`/`SIL`/`DIL`, and `emit_rex(false, …)` emits no prefix at all when every
/// register is low. An `Rsp`/`Rbp`/`Rsi`/`Rdi` source would therefore silently address a
/// *high* byte register. Two acceptable resolutions: force a `REX` prefix (`0x40`) for those
/// four codes — which is what makes `SPL`/`BPL`/`SIL`/`DIL` selectable — or refuse them, as
/// below. The arm satisfies the refusal: it stores only from `Rdx`/`Rcx`/`Rax`, all codes
/// 0..=3.
/// Emitted size: 6 B, 7 B with a `REX` prefix or an `rsp`/`r12` base (`SIB`), 8 B with both.
pub fn mov_mr8(&mut self, base: Reg, disp: i32, src: Reg) -> Result<()> {
    if matches!(src, Reg::Rsp | Reg::Rbp | Reg::Rsi | Reg::Rdi) {
        return Err(BeamError::BadArg);
    }
    self.emit_rex(false, ext(src), 0, ext(base))?;
    self.emit_u8(0x88)?;
    self.emit_mem_modrm(src, base, disp)
}
```

`mov_mr8` alone unblocks **every** width in the table (store `bits/8` bytes big-endian
with a `shr_ri` per byte) plus `ConstBytes`, so one method is the whole dependency.

### Only the 8-bit store is needed

`mov_mr8` plus one `shr_ri` per byte covers every width in the table and `ConstBytes`;
`mov_mr16` and `mov_mr32` are not required for this arm.

Verified against `encode.rs`'s `impl CodeBuffer`, the primitive surface available today is:
`mov_rr` / `mov_ri` (always `movabs`) / `mov_rm` (64-bit load) / `mov_rm32` (32-bit load,
zero-extending) / `movzx_byte` / `movzx_word` / `mov_mr` (64-bit store) / `lea`; the
register-form group `add_rr` / `sub_rr` / `and_rr` / `or_rr` / `xor_rr` / `cmp_rr` / `test_rr`;
`imul_rr` / `cqo` / `idiv_rr`; the shifts `sar_ri` / `shr_ri` / `shl_ri` / `shr_cl` / `shl_cl`;
the stack pair `push_r` / `pop_r`; control flow `jmp` / `jcc` / `call_label` / `jmp_r` /
`call_r` / `ret`; and the `nop` / `int3` fillers.
**The store side of that list is `mov_mr` (64-bit) alone** — that is the whole gap.

## The seam (already delivered in `68eb081423` — read it, do not redesign it)

`crate::families::bitstrings::create::seam`:

- `pub(crate) fn append_plan(module: &Module, segments: &[Operand]) -> Option<AppendPlan>`
  — COMPILE-TIME, pure over the module image, **label-free** (so `is_inlined`,
  `uses_y` and the refusal census can all call it).
- `pub(crate) struct AppendPlan { source: Operand, tail_bits: u32, fields: Vec<Field> }`
- `pub(crate) enum Field { Bits { value: Operand, bits: u32 }, ConstBytes { bytes: Vec<u8> } }`
- `pub(crate) fn open_append(machine: &Machine, src: Eterm, tail_bits: u32)
   -> Option<(usize /*payload base*/, usize /*end_bits*/)>` — RUNTIME, a **pure read**
  (allocates nothing, mutates nothing).

`Some` from `open_append` proves the family would not grow, not republish, not allocate, and
would return the source term **word-identically** (pinned by
`create/seam_tests.rs::an_accepted_plan_builds_word_identically_and_grows_nothing`), so the
only mutation the build performs is `ErlSubBits.end += tail_bits`.

## Design decisions (verified, ready to implement)

1. **`CreateBinShape` modelled on `BsMatchShape`** (`emit/bitstr_inline/bs_match.rs:170-288`):
   `source: Loc`, `dst: Loc`, `tail_bits: u32`, `fields: Vec<…::seam::Field>` (re-use the
   seam's `Field` — do not duplicate it), plus `registers()`/`reads()`/`writes()`/`uses_y()`.
2. **`of` is label-free** — `emit/regalloc.rs:426 is_inlined`, `emit/arith.rs:361 uses_y` and
   the refusal census all call it with no labels/fail. `create_bin_shape(labels, module, fail,
   segments, dst) -> Option<(CreateBinShape, (u32, u32))>` only threads them through.
3. **`reads()` = `source` + every `Field::Bits::value`; `writes()` = `dst`.** The payload
   stores are heap writes, not x-register writes (`GetInteger2Shape` precedent).
4. **Decline every non-x operand.** `source` and every `Field::Bits::value` must be an x Loc.
   This is what makes the post-helper reads guard-free: a Y load can fail its frame-window
   guard, and a guard after the commit is a wrong result. `uses_y()` therefore reduces to
   `dst.is_y()` (still written in the general form).
5. **The helper does the `end` commit, and returns the write cursor.** A thin
   `extern "C" fn jit_open_append_bin(rt, src_reg: i64, tail_bits: i64) -> i64` in
   `runtime.rs` reads `machine.x[src_reg]` (helpers re-derive run-time values from the
   machine — `emit_helper_invoke` can only pass `movabs` immediates), calls `open_append`,
   commits `sb->end = end_bits + tail_bits`, and returns `payload + end_bits/8`. Zero means
   "take the fallback".
   Why not emit the commit: `emit_helper_invoke` clobbers `r11` (`mov r11, helper; call r11`),
   so the `ErlSubBits` block `emit_ctx_guard` leaves in `MOVE_SCRATCH` does **not** survive
   the call, and re-deriving it (or parking it in a `JitRuntime` field) is more machinery than
   a single word store inside the helper. Because every guard precedes the call and nothing
   after it can fail, the commit is still **exactly once per accepted execution**.
6. **Shape predicate declines** (emit zero bytes, fall back to `jit_bitstr`): a non-x source or
   field value; `bits` not a whole number of bytes or outside `8..=64`; more fields than the
   shape budget allows.

## Emitted sequence (order is load-bearing — deopt idempotency)

```
  <dst.guard_store(Y_ADDR_SCRATCH, Y_ADDR_SCRATCH_2, fallback)>   ; a rejected dst writes nothing
  <for each Field::Bits: is_small guard on its value>             ; `mov_rm(rdx, rbx, x_off)`; mask the low 4 bits and compare with TAG_IMMED1_SMALL (0xF), `jcc(Ne, fallback)`
  emit_positioned_helper_call(jit_open_append_bin, &[src_reg, tail_bits], func, pc)  ; spills first, publishes the position
  test_rr(Rax, Rax) ; jcc(Cond::E, fallback)                      ; the only guard after the call, and it precedes every write
  <per field at rax + field_offset: bits/8 byte stores MSB-first>  ; `mov_rm(rdx, rbx, x_off); shr_ri(rdx, 4 + 8*(n-1-i)); mov_mr8(rax, off+i, rdx)`
  <load source into rdx ; re-derive a Y dst's address ; dst.store_at(...)>
  jmp fast_done
fallback: emit_step_and_continue(...)
fast_done:
```

- **The helper call clobbers every caller-saved register** (`rax`, `rcx`, `rdx`, `rsi`,
  `rdi`, `r8`-`r11`), so every operand the store phase needs is re-read after it: each
  field value from `machine.x` through `rbx` (the helper preserves the callee-saved
  registers, and the pre-call spill already made `machine.x` authoritative), and the
  source term likewise.
- **The field-value extraction is a `push_uint` reproduction, not a guess.** A small int is
  `make_small(i).raw() == (i << 4) | 0xF` (`beam_term`'s `TAG_IMMED1_SMALL = 0xF`, mask `0xF`;
  `0xB` is `TAG_IMMED1_IMMEDIATE`, not a small int), so byte `i` of an `n`-byte field is
  `raw >> (4 + 8*(n-1-i))` with a **logical** shift, and `mov_mr8` stores the low byte — which
  masks implicitly. That reproduces `push_uint`'s `unsigned_value(t) = (v as u64) & ((1 <<
  SMALL_BITS) - 1)` byte-for-byte for negative values too (all-ones `>> 4` is the two's-complement
  `u64`), including the truncation an 8-bit field of `0x1FF` needs (`0xFF`).
- **The `is_small` guard stays before the commit** because a *bignum* is legal in a wide field
  (`integer_non_small`) and a non-integer is the family's `badarg`; both must reach the
  interpreter, and reaching it after the commit would double-append.
- **A `Y` destination's address must be re-derived after the call, not re-guarded.**
  `Loc::guard_store` leaves the address in `Y_ADDR_SCRATCH` (`r9`), which the call
  destroys, and `Loc::store_at` consumes the address that guard produced — so the store
  phase re-derives it from `JitRuntime::y_base` (`mov_rm(rt, Rsp, RUNTIME_SLOT_DISP)`,
  `mov_rm(addr, rt, RUNTIME_YBASE_DISP)`, `lea(addr, addr, y_offset(k))`) with **no**
  second guard: a guard after the commit would take `fallback` on an already-committed
  `end`, i.e. a double append.
- **No fail arm.** `bs_create_bin`'s `fail`-vs-`NO_FAIL` discrimination stays in the family;
  never branch to a natively-resolved `fail` label.
- `emit_ctx_guard` is **not** required: `open_append` itself declines a non-accumulator
  (`is_refc_binary` + `start == 0` + the writable-capacity registry), so the helper's zero
  return covers it.

## Anchors (all verified; re-read before editing)

| file | anchor |
|---|---|
| `emit/bitstr_inline/mod.rs` | `SHAPE_COUNT = 8` (`:191`) → 9; `SHAPE_BS_MATCH = 7` (`:215`), add `SHAPE_CREATE_BIN = 8` |
| `emit/block.rs` | new arm **immediately above** `_ if bitstrings::claimed(instr)` (`:1069`); model on `Instr::BsMatch` (`:1042-1059`); must **not** set `terminated` |
| `emit/regalloc.rs` | imports `:103-106`; `is_inlined` `:426`; `cacheable_touches` `:563`; `cacheable_reads` `:759`; `cacheable_writes` `:925` |
| `emit/arith.rs` | `uses_y` `:361` |
| `emit/compile.rs` | `function_capacity_estimate` `:581`, `bs_paths` `:603-618`, consts `:696`/`:704` |
| `emit/test/refusal_census.rs` | `:662` `Instr::BsCreateBin { .. } => BsVerdict::NoArm(…)` |
| `bitstr_inline_tests.rs` | `:519` `cases` array (8 rows) + `assert_eq!(covered, SHAPE_COUNT)` `:553` |
| `runtime.rs` | helper type aliases `:560-591`; `JitRuntime` `:181-307` |

## Measured-byte arithmetic (for the capacity budget)

`emit_mem_modrm` always emits `mod=10` `disp32` (plus a `SIB` byte only for a `rsp`/`r12`
base), so sizes are closed-form: `mov_mr`/`mov_rm` = 7 B (8 B with an `rsp` base),
`mov_ri` = 10 B (always `movabs`), `call_r`/`jmp_r`/`cmp_rr`/`test_rr` = 3 B, `jcc` = 6 B,
`jmp` = 5 B, `shr_ri` = 4 B, `mov_rr` = 3 B. Per field the worst case is
`7 (load) + 39 (is_small guard) + 8 × 14 (8 byte stores)` ≈ 158 B; budget 384 B per field
and ≈512 B for the shape base (helper call, cursor test, dst store, fallback step).
**Under-estimating the capacity is a hard `SystemLimit` compile error** (`compile.rs:559-565`).

## In-scope alternatives that were evaluated and rejected

### 1. Emulating the byte store with an 8-byte read-modify-write

Load the 8-byte word containing the target byte (`mov_rm`), clear that byte (`shl 8` then `shr 8`,
or `and_rr` with a mask built by `mov_ri`), `or_rr` the byte shifted into position, and store the
word back (`mov_mr`).

It is memory-safe only when the whole 8-byte window lies inside the region `open_append`
guarantees writable — payload bytes `[0, (end_bits + tail_bits)/8)` — because the payload's real
capacity never reaches generated code.
For a field byte at payload-relative offset `k` only two window choices exist: a window **ending**
at that byte (`start = k - 7`), which needs `k >= 7`, and a window **starting** at it, which needs
`k + 8 <= capacity_bytes`, a figure generated code does not have.
`k >= 7` excludes the first 7 byte writes of every accumulator, which for `cowboy_bstr:to_lower/1`
over a short token is most of the work, and a per-byte capacity branch would need a run-time window
base and a second scratch register, exploding both the code size and the per-byte cost.
The byte arithmetic is worse too: ≈29–34 B per byte (`mov_rm` 7 + the two clearing shifts 8 B +
`or_rr` 3 + `mov_mr` 7 + the positioning `shl_ri` 4, +5 B for the `mov_ri`-built mask) against ≈14 B
per byte for `mov_mr8` + `shr_ri` (store 6–7 B + `mov_rr` 3 + `shr_ri` 4).
So it is not a substitute for the encoder method.

### 2. Landing only the widths a 64-bit store can write safely

`Field::Bits { bits: 64 }` and `ConstBytes` whose length is a multiple of 8 are the only shapes
`mov_mr` writes without overrunning: an 8-byte store at the field's own start writes exactly that
field's bytes and stays inside `(end_bits + tail_bits)/8`, whereas a 32-bit field would get 4 extra
bytes `open_append` never promised.

Rejected because it excludes **both** motivating shapes — `cowboy_bstr:to_lower/1` is `integer 8`
and `cowboy_bstr:capitalize_token/1` additionally needs `string "-"` — so it buys no improvement on
the two kernels the arm exists for, while adding an arm that must be rewritten the moment the 8-bit
store lands.

## Acceptance criteria for the next round

- `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib` green.
- `bitstr_inline_tests.rs`'s `every_inline_shape_emits_on_a_cache_free_compile` (`:519`/`:553`) gains
  a ninth row — `("bs_create_bin", SHAPE_CREATE_BIN, "BsCreateBin", module_create_bin)` — and its
  `assert_eq!(covered, SHAPE_COUNT)` holds with `SHAPE_COUNT == 9`.
- A non-vacuity pin in the same instrument: `inlined(SHAPE_CREATE_BIN)` moved,
  `visited_instances("BsCreateBin") >= 1` and `static_deopts("BsCreateBin") == 0`, so a
  fully-deopting "implementation" fails it.
- A deopt-idempotency pin in the style of `bs_match_deopt_tests.rs`'s
  `a_legitimately_deopting_shape_is_still_idempotent` (`:809`): a shape whose sequence really takes
  `fallback` answers exactly the interpreter's value, which is what proves nothing was mutated
  before the rejecting guard.
- Parity against the interpreter **and** the live `erl` for the four `foreign` kernels
  (`foreign_kernel_tests.rs`), minimally for `bstr_to_lower` and `bstr_capitalize_token`.
- Boundary rows: an empty result, a zero-length `string`, a 64-bit field, a `string` longer than 8
  bytes, an accumulator exactly at capacity, one byte short of it (which must grow through the
  family path), a growth crossing, and the result used as a heap term afterwards (stored in a tuple
  and read back).
- Declined-shape rows, each emitting **zero** bytes and still matching the interpreter: `binary all`,
  `integer 6` (not a whole number of bytes), a float segment, and a dynamic size or unit.

## Seam insufficiencies (report to the seam's owner)

1. **`open_append` takes `&Machine`, but the block ABI hands a `*mut JitRuntime`.** A wrapper
   is required in `runtime.rs`; the seam could instead expose an `extern "C"` entry.
2. **No `end_bits` channel.** The seam returns `(base, end_bits)` to Rust, but generated code
   receives one 64-bit word. Solved here by returning the **write cursor** and committing
   `end` inside the wrapper; a first-class seam could return the cursor itself.
3. **`Field::Bits` says nothing about the field's run-time *type*** — a non-integer value is
   the family's `badarg` (`erl_bits.c:952`, `prepared.rs:477-479`), so every lowering must
   emit its own `is_small` guard. A seam flag `value_is_small_checked: bool`, or the note in
   the module docs, would make this non-negotiable.
4. **The seam masks nothing.** `push_uint` writes `value.to_be_bytes()[8-n..]`, i.e. the value
   masked to `bits`; an 8-bit field of `0x1FF` must write `0xFF`. A lowering that stores the
   raw 60-bit payload is a wrong result, so the byte-at-a-time `shr` sequence (which masks
   implicitly by storing one byte at a time) is mandatory.
5. **No capacity channel.** `open_append` answers *whether* `tail_bits` more bits fit, never how
   much room the payload has — that figure lives in `Machine::writable_payloads` and stays there —
   so generated code cannot bound a write window of its own. `AppendPlan::tail_bits` is the only
   room figure a lowering can know, which is exactly why the read-modify-write alternative above
   cannot be made safe without a seam or helper change.

## Oracle facts to re-derive from, never guess

- `../../../../../../erts/emulator/beam/emu/bs_instrs.tab` (`i_bs_create_bin` / `bs_put_*`).
- `../../../../../../erts/emulator/beam/erl_bits.c` / `.h`.
- In-tree: `families/bitstrings/mod.rs` `push_uint` (`:968`) / `push_byte_field` (`:1142`),
  `create/prepared.rs` `PreparedSeg::ByteInt` (`:415-442`) / `Integer` (`:443-493`),
  `create/mod.rs`'s private-append arm (`:416-445`).
