# `beam_engine::jit::emitter` — x86-64 machine-code emitter + executable `CodeBuffer`

## Intent
The low-level half of the baseline JIT tier: turns calls like `CodeBuffer::mov_rr`/`CodeBuffer::jcc`
into real x86-64 machine-code encodings, accumulates them in an executable mapping, resolves
forward/backward `Label` references, and hands back a callable function pointer.
It is a **standalone building block** — it knows nothing about the typed IR, `Machine` or the tier
seam; the baseline tier (`../baseline/emit/`) lowers IR onto it. System V AMD64 ABI, `x86_64-unknown-linux-gnu`.
The encoder itself implements **no** prologue/epilogue — that is the codegen layer's job (module docs `mod.rs:48-50`).

## API Surface (what an emitter consumer uses)
Files: `mod.rs` (buffer lifecycle, labels/fixups, raw byte emission, W^X), `encode.rs` (the instruction encoders, no `unsafe`), `tests.rs`.
### Types
- `CodeBuffer` (`mod.rs:288-303`) — owns one executable mapping.
- `Reg` (`mod.rs:137-199`) — 16 GPRs, discriminant = architectural encoding; `Reg::ALL` (`:176-193`), `Reg::code()` (`:196-198`).
- `Cond` (`mod.rs:207-242`) — `Jcc` condition codes, discriminant = `tttn`; `Cond::code()` (`:244-249`). Signed `L/Ge/Le/G`, unsigned `B/Ae/Be/A`, plus `O/No/E/Ne/S/Ns/P/Np`.
- `Label` (`mod.rs:257-263`) — buffer-local (`id` + owning-buffer `nonce`).
- `pub const DEFAULT_CAPACITY: usize = 64 * 1024` (`mod.rs:108`).
### Buffer lifecycle (all fallible ops return `beam_common::Result`)
- `CodeBuffer::new()` (`:307`), `with_capacity(n)` (`:315`) — `n` rounded up to whole pages.
- `len()`/`is_empty()`/`capacity()`/`is_sealed()` (`:345-362`), `code_ptr()` (`:368`).
- `label()` (`:396`), `bind(Label)` (`:410`), `seal()` (`:440`), `as_fn_ptr::<F>()` (`:382`).
- `Drop` → `munmap` (`:572-583`).
### Encoders (`encode.rs`, all `Result<()>`; every encoding `REX.W` unless noted)
- move — `mov_rr` `:39`, `mov_ri` (movabs, 64-bit imm) `:46`, `mov_rm` (`[base+disp32]` load) `:53`, `mov_mr` (64-bit store) `:110`, `lea` `:192`.
- narrow stores — `mov_mr8` `:143` (`88 /r`), `mov_mr16` `:152` (`66 89 /r`), `mov_mr32` `:161` (`89 /r`, `REX.W` clear) plus the shared private `emit_mov_mr_narrow`; see *Narrow memory stores* below.
- arith/logic — `add_rr` `:209`, `sub_rr` `:214`, `cmp_rr` `:219`, `test_rr` `:224`, `and_rr` `:229`, `or_rr` `:234`, `xor_rr` `:239`, `imul_rr` `:252` (two-operand `0F AF /r`; sets OF/CF on 64-bit overflow — the `erlang:'*'/2` guard).
- divide — `cqo` `:266` (`REX.W 99`, sign-extend `rax` into `rdx:rax`) and `idiv_rr` `:283` (`F7 /7`, 64-bit signed divide: quotient in `rax`, remainder in `rdx`; a zero divisor or `i64::MIN / -1` traps `#DE`, so the caller must refuse both).
- shifts — `sar_ri` `:292`, `shr_ri` `:303`, `shl_ri` `:308` (`C1 /n ib`; `sar_ri`/`shl_ri` take a `u32` count and reject one above `u8::MAX` with `BadArg`, `shr_ri` takes the `u8` immediate directly) and the `cl`-count forms `shr_cl` `:318`/`shl_cl` `:326` (`D3 /n`).
- stack — `push_r` `:357`, `pop_r` `:365`.
- control flow — `jmp` (`rel32`) `:375`, `jcc` (`rel32`) `:381`, `call_label` (`rel32`) `:388`, `jmp_r` (indirect, `FF /4`, `0x41` REX.B for `r8`–`r15`, pinned by `tests.rs::jumps_through_a_register`) `:402`, `call_r` (indirect) `:411`, `ret` `:420`.
- filler — `nop` `:427`, `int3` `:432`.
### Narrow (zero-extending) loads and run-time shift counts
- `mov_rm32` `:65` — `8B /r` with `REX.W` **clear**, so the 32-bit register write zero-extends into the full 64-bit destination (the x86-64 rule) instead of keeping the old high half; a `REX` prefix is still emitted (without `W`) for an `r8`–`r15` operand.
- `movzx_byte` `:78` — `REX.W 0F B6 /r`: an 8-bit load zero-extended into the 64-bit destination, so a `0xFF` source reads as `0xFF` and never `-1`, and only the addressed byte is read.
- `movzx_word` `:88` — `REX.W 0F B7 /r`: a 16-bit load zero-extended into the 64-bit destination, the same unsigned-extension guarantee two bytes wide.
- `mov_rm`/`mov_rm32`/`movzx_byte`/`movzx_word` share the one `emit_mov_rm`/`emit_movzx` path over `emit_mem_modrm`, so an `rsp`/`r12` base forces its `SIB` byte in every width.
- `shr_ri` `:303` — `REX.W C1 /5 ib`: a **logical** (zero-filling) right shift by immediate, the counterpart of the arithmetic `sar_ri` (`/7`).
- `shr_cl` `:318` / `shl_cl` `:326` — `REX.W D3 /5` and `REX.W D3 /4`: shifts by the count in `cl`, which the caller must have loaded into `rcx`; the CPU masks the count to 6 bits for a 64-bit operand, so counts 64/65/68 shift by 0/1/4 and never trap.
### Narrow memory stores and the 8-bit `REX` aliasing rule
`mov_mr8`/`mov_mr16`/`mov_mr32` are the store-direction counterparts of the narrow loads above: `mov [base+disp32], src` in an 8-, 16- or 32-bit width, each `Result<()>`, each writing **exactly its own width** — so a binary payload's byte field is written without disturbing its neighbours (the 64-bit `mov_mr` always writes eight bytes). All three share the private `emit_mov_mr_narrow`, which emits the optional `66`, the `REX` (subject to the rule below), the `88`/`89` opcode and the shared `emit_mem_modrm` operand.
Emitted bytes, for a low base and a low source **outside** the 8-bit aliasing group:

| method | encoding | bytes |
|---|---|---|
| `mov_mr8(base, disp, src)` | `88 /r` | `88 modrm disp32` — 6 |
| `mov_mr16(base, disp, src)` | `66 89 /r` | `66 89 modrm disp32` — 7 |
| `mov_mr32(base, disp, src)` | `89 /r` (`REX.W` clear) | `89 modrm disp32` — 6 |
| `mov_mr(base, disp, src)` | `REX.W 89 /r` (the 64-bit form) | `48 89 modrm disp32` — 7 |

The prefix order is the architectural one — the legacy `66` operand-size prefix *before* the `REX` — and the `REX` bits are width-independent (`REX.R` for an `r8`–`r15` source, `REX.B` for one as a base, plus the `SIB` byte an `rsp`/`r12` base forces), so each of those adds one byte.
**The trap that is handled — the 8-bit `REX` aliasing rule.** Without a `REX` prefix, byte-register codes `4..=7` name the **legacy high bytes** `AH`/`CH`/`DH`/`BH`, not `SPL`/`BPL`/`SIL`/`DIL`; the low-byte names exist only when a `REX` prefix is present. `emit_rex` emits *nothing* when every field is low, so a naive byte store with `src` = `Rsp`/`Rbp`/`Rsi`/`Rdi` (codes 4–7) and a low base would silently store a **different register** — `DH` instead of `SIL`, i.e. bits 8–15 of `rdx` — which is a wrong store, not a typed error. `emit_mov_mr_narrow` therefore **forces a bare `REX` (`0x40`, no `W`/`R`/`B`)** exactly when `src`'s low three bits are `4..=7` **and** no `REX` extension bit is needed anyway, making that case 7 bytes instead of 6. When an `r8`–`r15` operand already forces a `REX`, that prefix satisfies the rule and no byte is added. `mov_mr16`/`mov_mr32` carry no such rule — the 16- and 32-bit names have no high-byte alias — so they emit only the `REX` bits their operands need.
Absent on purpose (add a tested method, don't guess): `mov r/m64, imm32`, **every `_ri` immediate arithmetic/logic form** (`add_ri`/`sub_ri`/`and_ri`/`or_ri`/`xor_ri`/`cmp_ri`/`test_ri` — synthesize as `mov_ri` into a scratch + the `_rr` form, 13 B for a 3 B op), `setcc` (a boolean in a register is `mov_ri 0` + `cmp`/`jcc` + `mov_ri 1`, or two labels), a **sign-extending** `movsx` (only the `movzx` forms above exist), `ud2`, `neg`/`not`, memory-operand arithmetic, indexed addressing (`[base + index*scale]` — no SIB index is ever emitted, so scale with `shl_ri` + `lea`/`add_rr`), SSE/AVX, `syscall`.

## W^X design (never RWX)
`with_capacity` maps anonymous private `PROT_READ|PROT_WRITE` (`mod.rs:320-329`); `seal` flips the
whole mapping to `PROT_READ|PROT_EXEC` via `mprotect` (`mod.rs:451-457`). While writable no
instruction may be executed; once sealed it can no longer be patched — every emitting call after
`seal` is a typed `BadArg` (`ensure` `mod.rs:468-476`; `bind` `:411-413`). `as_fn_ptr<F>` transmutes the
base to a fn pointer (`:382-393`) and is only valid after `seal`. `seal` is idempotent (`:441-443`) and
refuses if any label is unbound (`:444-446`). `tests.rs::seal_is_w_xor_x` (`:380-399`) asserts the
`rw-p`/`r-xp` permissions via `/proc/self/maps`.

## Labels, bind, seal, rel32 fixups
- `Label { id, nonce }` (`mod.rs:257-263`): `id` = index into the buffer's `labels: Vec<Option<usize>>`, `nonce` = owning-buffer identity (`NEXT_BUFFER_NONCE` AtomicU32 `:103`).
- `label()` (`:396`) allocates the next id; `label_slot()` (`:561-569`) rejects a foreign-`nonce` (`BadArg`) or out-of-range label.
- `emit_rel32(label)` (`:545-558`): writes a 4-byte placeholder; if the label is already bound (backward ref) it patches immediately via `rel32()`; else records `Fixup { label, at }` (`:269-273`).
- `rel32(target, at)` (`:598-600`) = `target - (at + 4)` (measured from the instruction end), `i32`-checked (`SystemLimit` on overflow).
- `bind(label)` (`:410-432`): rejects seal-time/foreign/double binds (`BadArg`), stores `Some(len)`, then patches every matching pending `Fixup` (kept via `mem::take`).
- `seal()` rejects any still-unbound label with `BadArg` (`:444-446`).
- `patch_i32` (`:508-517`) overwrites a deferred displacement (`write_unaligned`).
- Raw primitives (`ensure`/`emit_u8`/`emit_bytes`/`emit_i32`/`emit_i64`/`emit_rex`/`emit_mem_modrm`) are private to `mod.rs`; `emit_mem_modrm` (`:532-541`) emits uniform `mod=10` `disp32` + a `SIB` byte when base is `rsp`/`r12`, sidestepping the `rbp`/`r13` `mod=00` RIP-relative case.

## How the baseline tier uses it (prologue/epilogue + register save-restore)
Encoder support is `push_r`/`pop_r` + `lea`/`mov_mr`/`mov_rm`; the tier composes them across `../baseline/emit/`:
- **Prologue** (`block.rs::emit_block`): `push_r` over `SAVED_REGISTERS = [Rbx,Rbp,R12,R13,R14,R15]` (`mod.rs` `SAVED_REGISTERS`, 6 pushes keep `rsp` 16-byte aligned), `lea Rsp,[Rsp-24]` reserves the `rt` slot, `mov_mr Rsp,0,Rdi` stores the `rt` pointer, `mov_rm Rbx,Rdi,RUNTIME_XREGS_DISP` loads `rt->x_regs`, then `mov_rm(reg, Rbx, x)` fills each cached x-register.
- **Register save/restore across a helper call**: `spill` (`regs.rs::spill`) = `mov_mr(Rbx, x, reg)` for each cached slot; `fill` (`regs.rs::fill`) = `mov_rm(reg, Rbx, x)`. `emit_helper_call` (`mod.rs::emit_helper_call`) spills first, reloads `rt` into `rdi`, then `mov_ri R11, helper; call_r R11`.
- **Epilogue / frame teardown** (`link.rs::emit_frame_teardown`): `lea Rsp,[Rsp+24]` then `pop_r` over `SAVED_REGISTERS` reversed, then `ret` (the shared `done` epilogue in `block.rs::emit_block`). The exact inverse of the prologue; shared by the `done` epilogue and a linked jump so they cannot drift.
`baseline/emit/mod.rs`'s module docs document the register-allocation + spill/fill discipline
(`CACHE_REGISTERS = [R12,R13,R14,R15,Rbp]` in `mod.rs`; residency is block-fixed, while **both**
directions of the cache are partial — `spill` stores only the cached registers **dirty at its own `pc`**
(`Allocation::spill_slots_at`, derived by `regalloc.rs`'s forward dirty walk), so an already-clean
resident costs no store and the shared `done` epilogue stores nothing, and `fill` reloads only
`Allocation::fill_slots(pc)` (the continuation's live set), with the prologue loading the block's
live-in set; the authoritative statement is `../baseline/emit/CONTEXT.md` and
`../baseline/emit/regs/CONTEXT.md`).

## Constraints
- **Crate's designated JIT unsafe zone**: module-local `#![allow(unsafe_code)]` (`mod.rs:84`) is the crate's only executable-memory mapping and only raw writes into it; `encode.rs` holds **no** `unsafe`. Every `unsafe` block carries a `// SAFETY:` rationale.
- Family convention: `#!`-deny at the crate root stays; this module is an explicit exception (alongside `predicates`/`terms`).
- Errors only `beam_common::{BeamError, Result}`: failed `mmap`/`mprotect` → `Io`; exhausted fixed capacity → `SystemLimit`; misuse (sealed-emit, foreign/double label, unbound-at-seal) → `BadArg`. Nothing panics.

## Test Strategy
`tests.rs` (in-module `#[cfg(test)]`) is the encoder's byte/behaviour gate: register codes (`:51`),
page-rounded capacity (`:65`), `add`/`imul`/`sar`/`shl` (`:78`,`:93`), `r8`/`r9` + 6-arg ABI (`:119`,`:134`),
`r12`/`rsp`/`rbp` bases incl. forced `SIB` (`:151`,`:172`,`:191`), `lea` (`:212`), `jcc` select (`:226`),
`call_r` + `push`/`pop` discipline (`:246`), forward/backward/conditional jump patching (`:274`,`:290`,`:309`),
forward `call_label` (`:336`), unbound-label seal error (`:356`), emit-after-seal error (`:368`),
W^X perms (`:380`), capacity exhaustion (`:401`), buffer-local labels (`:419`), sealed-drop `munmap` (`:434`).
The narrow-load and `cl`-count encoders are executed against real values, not just byte-pinned:
`emits_shr_ri_as_a_logical_shift` (`:571`) shifts bits out of the low byte and shows `-1` coming back
positive (against a `sar_ri` control), `emits_shr_cl_and_shl_cl` (`:611`) drives the count through
`rcx` including the mask-mod-64 cases, `emits_movzx_byte_and_word_without_sign_extension` (`:658`)
loads `0x80`/`0x1234` out of an all-`0xFF` neighbourhood (with a `mov_rm` control that *does* pick the
neighbours up), and `emits_mov_rm32_which_clears_the_upper_half` (`:733`) reads a word whose high 32
bits are garbage (control: `mov_rm` keeps them).
The narrow **stores** follow the same discipline — executed, not merely byte-pinned (they start at
`tests.rs:814` with the `byte_store_through` fixture):
- `emits_mov_mr8_which_stores_exactly_one_byte` drives a byte store through a `src` from every low
  group (`Rax`/`Rcx`/`Rdx`/`Rbx`/`Rsi`/`Rdi`/`Rbp`) plus `R8`/`R11`/`R13`, after its fixture
  (`byte_store_through`) poisons the `AH`-family homes with distinct second bytes (`rax`=`0x1111`,
  `rbx`=`0x2222`, `rcx`=`0x3333`, `rdx`=`0x4444`) — so a mis-encoded high-byte store lands `0x11`/
  `0x33`/`0x44`/`0x22` in the scratch instead of the value under test, and the failure is visible as
  a wrong byte. Every other scratch byte must stay `0xA5`, only the low byte of a 64-bit value is
  stored, and two displacements in one buffer are honoured. The fixture pushes/pops every
  callee-saved register it writes (`rbx` always, plus a callee-saved source) and moves the base
  pointer to `rax` when `rdi` is itself the source.
  `rsp` cannot be loaded with a value without destroying the stack, so it is covered byte-wise only.
- `emits_mov_mr8_with_the_bare_rex_the_high_byte_aliases_require` pins the exact bytes of all four
  aliasing codes as `src` (`40 88 A7/AF/B7/BF disp32` — `SPL`/`BPL`/`SIL`/`DIL`, 7 bytes instead of
  6), of the extension-bit forms (`REX.R`, `REX.B`+`SIB`), of the case where an extended base's
  `REX.B` alone satisfies the rule, and a `mov_mr` width control (`48 89 …`, one byte longer).
- `emits_mov_mr16_and_mov_mr32_which_write_exact_widths` executes both widths into an all-`0xFF`
  scratch with the value `0x1122_3344_5566_7788`, so the 16-bit form must leave `88 77` at `[3]` and
  the 32-bit form `88 77 66 55`, every other byte intact — against a `mov_mr` control that does
  overrun the window. It then pins the encodings (`66 89 /r`, `89 /r`, `REX.W` clear), that **no**
  forced `REX` appears for an `rsp`/`rsi` source in those widths, and the `66`-then-`REX`-then-opcode
  prefix order with `REX.R`/`REX.B`.
The baseline tier's own `baseline/emit/*` tests assert the emitted byte sequences.

## Routing Table
- `encode.rs` → the per-instruction encoders (the `impl CodeBuffer` instruction surface).
- `mod.rs` → buffer lifecycle, `Reg`/`Cond`/`Label`/`Fixup`, raw byte-emission primitives, W^X.
- `tests.rs` → the encoder unit tests.
- `../baseline/` → the consumer: IR→native lowering, prologue/epilogue, `spill`/`fill`, block linker.

## See Also
- `..` (`src/jit/`) → the tier seam (`Tier`/`JitBackend`), the cross-run `CodeCache`, the Cranelift hot tier.
- `../../CONTEXT.md` → `beam_engine` overall; `../../INSTRUCTION_SURFACE.md` → execution semantics.
- `../../../CONTEXT.md` → crate-layer contracts. `../../../../CONTEXT.md` §4 / Part II §F → the JIT rationale.
