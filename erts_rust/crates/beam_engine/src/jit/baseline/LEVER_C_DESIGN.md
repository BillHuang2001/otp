<!--
%CopyrightBegin%

SPDX-License-Identifier: Apache-2.0

Copyright Ericsson AB 2025-2026. All Rights Reserved.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.

%CopyrightEnd%
-->
# Campaign Lever C — emitted straight-line overhead (working design note)

Temporary working note for the lever-C implementation pass. It states the measured
problem and the three cuts, in enough detail that the implementing agent does not have
to re-derive them. Delete this file once the cuts are landed and the rules are recorded
in `emit/CONTEXT.md`, `emit/prologue/CONTEXT.md`, `emit/regs/CONTEXT.md`,
`emit/regalloc/CONTEXT.md` and `../measure/CONTEXT.md`.

## The measured problem

`list_walk` (module `bench_list_walk`: body-recursive `sum/1` over a 1000-element list,
walked N times) costs **348.2 retired instructions per element** on the baseline tier
against the oracle BeamAsm's **75**. Of those, **116.25 are inline emitted code**
(`[JIT]`); the remainder are frozen helper bodies and are out of scope.

Two hot blocks were disassembled byte-by-byte:

- `sum/1`'s element block: `test is_nonempty_list` = 6 instructions, `allocate 1,1` =
  11 + 4 for the continue check, `get_list {x,0} -> {y,0},{x,0}` = 13, a `y0` store = 3,
  `call 1 sum/1` = 20.
- `sum/1`'s return-continuation block (~94 static, ~55 executed) and `walk/3`'s element
  body: **every `Y` access pays a 6-instruction frame-window guard**; `walk/3` has three
  consecutive `Y` stores, so eighteen instructions of guard for three stores.

## Cut 1 (primary) — one frame-window guard per helper-free run

`emit/operands.rs::emit_guarded_y_address` is the single home of the guard and costs six
instructions per access:

```
mov  addr,[rsp + RUNTIME_SLOT_DISP]   ; rt
mov  end, [addr + RUNTIME_YEND_DISP]  ; y_end
mov  addr,[addr + RUNTIME_YBASE_DISP] ; y_base
lea  addr,[addr + k*8]                ; the slot address
cmp  addr,end
jae  fallback
```

The guard can only ever reject, so CSE-ing it is safe whenever the window provably cannot
change between two accesses. Define a **run** as a maximal sequence of consecutive block
instructions for which `regalloc::is_inlined(module, labels, instr)` is true. A
non-inlined instruction emits a helper call (`jit_step_one`, `jit_family`, the frame and
heap-admission helpers, the bitstring arms), and such a call may run an interpreter step
— which can reallocate `machine.stack` through `with_parked_loose_terms` — or republish
the window itself. So a run boundary is exactly a non-inlined instruction.

Within a run nothing moves the stack and nothing republishes the window, so:

- **one guard per run** suffices, provided it proves the run's **maximum** slot index:
  the window is `[y_base, y_base + y_count*8)`, so `y_base + maxk*8 < y_end` implies every
  slot `0..=maxk` is inside it;
- emit that guard at the run's **first** `Y` access, branching to that access's own
  fallback label — nothing has been stored yet, and every later access in the run is then
  provably in-frame, so a rejection still leaves the machine exactly as the interpreter
  will re-run the first instruction;
- every **later** `Y` access of the same run uses the same address derivation **without**
  the `cmp`/`jae` (three instructions).

Derive the runs at compile time in `emit/regalloc.rs`, beside `y_last`/`y_first`/
`frame_republish_first` (the existing scans over `code[start..end]`), e.g.
`Allocation::y_runs: Vec<YRun { first_pc, max_slot }>` plus a lookup
`y_guard_for(pc, k)`. Thread the decision into the `Y`-access primitives
(`emit_guarded_y_load`, `emit_guarded_y_store`, `Loc::guard_store`, `Loc::guard_load`,
`guard_y_read`, or a new `emit_unguarded_y_address`) and their call sites
(`move.rs`, `aggregate/list.rs`, `aggregate.rs`, `bif.rs`, `select.rs`, `compare.rs`,
`test.rs`, `block.rs`'s `init_yregs`).

`emit/arith.rs` is **out of scope** — a parallel campaign lever owns the small-int
arithmetic arm there, so its `Y` operand keeps the existing guarded path.

### Why this is sound, in the same terms as `Allocation::prologue_publishes_y`

A block is entered only at its own start — a fresh block entry, or a resume, which parks
on a **block entry** and re-executes the block prefix — so the run's first guard is always
executed before its accesses. The block is linear, and every way out of its middle is a
branch or exit into another range with its own prologue.

Hard rule, inherited from `measure/CONTEXT.md`'s "range-crossing Y-window witness"
(rejected): the derivation and the guard must be a **static** function of the block's own
shape, never of prior run-time state. `runtime.rs::fold_step` encodes `Step::Next` and
`Step::Goto { pc: pc + 1 }` as the same block-ABI word, so a family raise whose catch
handler sits at its own `pc + 1` continues in-block with no driver iteration between
`Machine::unwind_to_frame` and the handler — any scheme that reads a window flag set by
an earlier instruction would then resolve a `Y` slot through a destroyed frame.

## Cut 2 — minimal `get_list` / `get_hd` / `get_tl`

`emit/aggregate/list.rs::emit_list_read` currently spends eight instructions before the
`Y` guard, with two ten-byte `movabs` constants and a **second load of the source**:

```
mov  r11,<source>
mov  r10, NOT_LIST_MASK   ; movabs, 0x2
and  r11,r10
cmp  r11,r10
je   fallback
mov  r11,<source>         ; reload: the and consumed the word
mov  r10, POINTER_MASK    ; movabs, !0x7
and  r11,r10
```

Five instructions, no `movabs`, no reload:

```
mov  r11,<source>
test r11, 2               ; test_ri: `is_not_list` = (word & (TAG_PRIMARY_MASK - TAG_PRIMARY_LIST)) != 0
jne  fallback
and  r11, -8              ; and_ri: POINTER_MASK = !TAG_PTR_MASK = !0x7, fits a sign-extended imm32
```

`test` does not consume its operand, which is what removes the reload.
`emit/emitter/encode.rs` already provides `test_ri`/`cmp_ri`/`and_ri`/`add_ri`, each
documented as the one-instruction replacement for a `mov_ri` plus register-register pair.
Verify the masks against `beam_term::tag` (`TAG_PRIMARY_MASK = 0x3`,
`TAG_PRIMARY_LIST = 0x1`, `TAG_PTR_MASK = 0x7`) and against
`beam_term::list::is_not_list`, so the predicate is bit-for-bit the interpreter's.
Preserve exactly: the single reject edge to the same `fallback`, CAR-before-CDR store
order, the "every `Y` guard before any store" rule (a destination may alias the source),
and the same `Loc` read/write paths.

## Cut 3 — `TestPred::IsNonemptyList` / `IsList`

`emit/test.rs` (~lines 715-729) materialises `LIST_MASK` with a ten-byte `movabs` and
then `and`s. Replace with `test_ri(TEST_T, <mask>)`, keeping the `mov_rr(TEST_T, TEST_A)`
copy only where `TEST_A` must survive (a cache register). Preserve the exact predicate:
the `jcc(Cond::E, after)` reads ZF from `(t & mask) == 0`, so the mask is the
`is_not_list` bit (`0x2`), not `TAG_PRIMARY_MASK`; `IsList`'s `NIL` compare must stay
first. `and`-ZF and `test`-ZF are the same flags by definition.

## Cut 4 (optional) — other provable straight-line redundancy

Look for a `movabs` constant re-materialised while still live in a register, a duplicated
operand load, a store-then-reload of a register the cache already holds, a bounds check an
earlier guard already proved. If the redundancy lies inside `emit/arith.rs` (for example
the re-materialised `movabs rcx,0xf` in `sum/1`), report it and skip it. Do not relocate
helper work into emitted code (measured and rejected).

## Evidence required

1. A `#[cfg(test)]` dump harness that compiles a representative block **cache-free**
   through `emit::compile_function` (a warmed `CodeCache` re-emits nothing) and prints or
   asserts the emitted byte length and instruction count per shape, so each cut's
   before/after sequence is visible.
2. The deterministic corpus counters from the existing census tests (`census.rs`,
   `test/refusal_census.rs`, the register-cache and Y-window traffic census): emitted
   bytes and emitted instructions before and after.
3. `./scripts/cargo.sh test --manifest-path Cargo.toml -p beam_engine --no-fail-fast`
   green and warning-free, in particular `../frame_push_tests.rs`,
   `../raise_tier_tests.rs`, `emit/spill_tests.rs`, `../bs_match_deopt_tests.rs`,
   `../bs_create_bin_tests.rs`, `../foreign_kernel_tests.rs` and
   `tests/jit_real_module.rs` (tier agreement plus the oracle on 727 rows).

No `perf` A/B campaigns: the parent agent owns all measurement. Everything else in
`../measure/CONTEXT.md` (Closed levers, Rejected levers, "Frame-op bodies: dead ends", the
range-crossing witness) is already measured and must not be re-derived.
