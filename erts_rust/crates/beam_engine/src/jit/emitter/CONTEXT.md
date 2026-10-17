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
- move — `mov_rr` `:39`, `mov_ri` (movabs, 64-bit imm) `:46`, `mov_rm` (`[base+disp32]` load) `:53`, `mov_mr` (store) `:60`, `lea` `:67`.
- arith/logic — `add_rr` `:84`, `sub_rr` `:89`, `cmp_rr` `:94`, `test_rr` `:99`, `and_rr` `:104`, `or_rr` `:109`, `xor_rr` `:114`, `imul_rr` `:127` (two-operand `0F AF /r`; sets OF/CF on 64-bit overflow — the `erlang:'*'/2` guard).
- shifts — `sar_ri` `:137`, `shl_ri` `:142` (`C1 /n ib`, imm ≤ `u8::MAX` else `BadArg`).
- stack — `push_r` `:164`, `pop_r` `:172`.
- control flow — `jmp` (`rel32`) `:182`, `jcc` (`rel32`) `:188`, `call_label` (`rel32`) `:195`, `call_r` (indirect) `:201`, `ret` `:210`.
- filler — `nop` `:217`, `int3` `:222`.
Absent on purpose (add a tested method, don't guess): `mov r/m64, imm32`, memory-operand arithmetic, indexed addressing, SSE/AVX, `syscall`.

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
