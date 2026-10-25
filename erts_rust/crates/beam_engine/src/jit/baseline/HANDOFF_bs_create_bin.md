# HANDOFF — natively-inlined `bs_create_bin` construction arm

Status: **NOT IMPLEMENTED.** The design below is verified against the source, but the
implementation is blocked by one missing primitive in a *different* node
(`src/jit/emitter/encode.rs`), which this node may not edit.

## The blocker (read this first)

`CodeBuffer`'s complete store surface is `mov_mr(base, disp, src)` — **64-bit only**.
There is no `mov_mr8` / `mov_mr16` / `mov_mr32`. Therefore:

| `Field::Bits` width | writable today? | why |
|---|---|---|
| 8 (`cowboy_bstr:to_lower/1`) | **no** | needs a 1-byte store |
| 16 | **no** | needs a 2-byte store (a 8-byte store overruns) |
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
/// The byte-register encoding is ambiguous without a `REX` prefix: codes below `rsp`
/// name `AL`/`CL`/`DL`/`BL` but `AH`/`CH`/`DH`/`BH`. The caller must therefore not use
/// `Rsp`/`Rbp`/`Rsi`/`Rdi` as the 8-bit source (the arm only stores from `rdx`/`rcx`/`rax`).
/// Emitted size follows `mov_mr`'s rule: 7 B, or 8 B when `REX` is needed.
pub fn mov_mr8(&mut self, base: Reg, disp: i32, src: Reg) -> Result<()> {
    self.emit_rex(false, ext(src), 0, ext(base))?;
    self.emit_u8(0x88)?;
    self.emit_mem_modrm(src, base, disp)
}
```

`mov_mr8` alone unblocks **every** width in the table (store `bits/8` bytes big-endian
with a `shr_ri` per byte) plus `ConstBytes`, so one method is the whole dependency.

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
  <for each Field::Bits: is_small guard on its value>             ; load, and 0xF / cmp 0xB / jne fallback
  emit_positioned_helper_call(jit_open_append_bin, &[src_reg, tail_bits], func, pc)  ; spills first, publishes the position
  test rax, rax ; jz fallback                                     ; the only guard after the call, and it precedes every write
  <per field at rax + field_offset: bits/8 byte stores MSB-first>  ; `mov rdx,rcx; shr_ri(rdx, 8*(n-1-i)); mov_mr8(rax,off+i,rdx)`
  <load source into rdx ; dst.store_at(buffer, Y_ADDR_SCRATCH, rdx, allocation, highest_touched)>
  jmp fast_done
fallback: emit_step_and_continue(...)
fast_done:
```

- **No fail arm.** `bs_create_bin`'s `fail`-vs-`NO_FAIL` discrimination stays in the family;
  never branch to a natively-resolved `fail` label.
- The `is_small` guard must stay **before** the helper: a non-small field value is legal (a
  bignum fits an 8-byte field via `integer_non_small`), so it must reach the interpreter,
  and reaching it after the commit would double-append.
- `emit_ctx_guard` is **not** required: `open_append` itself declines a non-accumulator
  (`is_refc_binary` + `start == 0` + the writable-capacity registry), so the helper's zero
  return covers it.

## Anchors (all verified; re-read before editing)

| file | anchor |
|---|---|
| `emit/bitstr_inline/mod.rs` | `SHAPE_COUNT = 8` (`:191`) → 9; `SHAPE_BS_MATCH = 7` (`:215`), add `SHAPE_CREATE_BIN = 8` |
| `emit/block.rs` | new arm **immediately above** `_ if bitstrings::claimed(instr)` (`:1066`); model on `Instr::BsMatch` (`:1039-1056`); must **not** set `terminated` |
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

## Oracle facts to re-derive from, never guess

- `../../../../../../erts/emulator/beam/emu/bs_instrs.tab` (`i_bs_create_bin` / `bs_put_*`).
- `../../../../../../erts/emulator/beam/erl_bits.c` / `.h`.
- In-tree: `families/bitstrings/mod.rs` `push_uint` (`:968`) / `push_byte_field` (`:1142`),
  `create/prepared.rs` `PreparedSeg::ByteInt` (`:415-442`) / `Integer` (`:443-493`),
  `create/mod.rs`'s private-append arm (`:416-445`).
