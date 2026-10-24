# `beam_engine` native baseline JIT tier (`src/jit/baseline/`)

## Intent
The first `JitBackend` the engine builds: it lowers basic blocks of a `beam_loader::Module`
into real `extern "C"` x86-64 functions and runs them through a driver loop, deopting any
instruction it does not inline to exactly one interpreter step. Correctness-first mechanism, not an
optimiser (DeepDive §F): every natively-executed result is interpreter-identical by construction.
Backend behind `Tier::BaselineJit`; `jit-beamasm` aliases this tier.

The artifact (`CompiledModule`) comes in **two layouts**:
- **Eager** (`emit::compile_module`): every function lowered up front into **one** sealed
  `CodeBuffer`, with a whole-module block-entry table, so the seal-time `Linker` links a taken
  branch to a successor block of **another** function as a direct jump. Produced by
  `JitBackend::compile` and `CodeCache::get_or_compile`; the layout whole-module introspection and
  cross-function direct links need.
- **Lazy** (`code::CompiledModule::lazy`): nothing lowered up front; each function is lowered into
  its **own** sealed `CodeBuffer` on that function's first entry, driven through
  `NativeCode::enter(rt, module, func, pc)` → `CompiledModule::native_entry`. Produced by
  `CodeCache::compiled_handle` (the path behind `run_baseline_cached*` and
  `cached_module_blocks`), so a fresh process pays only for the functions it enters. A
  cross-function branch is absent from that function's block map, so the `Linker` emits the
  target's `(func, pc)` word and the driver compiles the target on entry.

## API Surface
- `code.rs` — `CompiledModule` (caller-owned artifact in the two layouts; `entry`, `native_entry`
  [the compile-on-entry point], `block_count`, `cached_registers`, `highest_cached_register`,
  `highest_native_register`, `code_len`, `code_address_range`, `code_map`, plus the two per-artifact
  derived values computed **once at creation**: `fingerprint` [the module's content hash, so a
  `CodeCache` hit never re-hashes the IR] and `labels` [the module-wide label map a run reuses
  instead of rebuilding]), plus the lazy layout's unit `CompiledFunction` (`buffer`, `entries` [IR-block
  starts], `sub_entries` [continuation sub-blocks], `summary`), the eager layout's parallel
  `EagerLayout::sub_entries` table, and the `Summary` register/block aggregate. `entry`/`native_entry`
  probe `entries` first and `sub_entries` second, so a real IR-block start always wins.
- `mod.rs` — `BaselineJit` (zero-sized `JitBackend`); the driver
  `run_compiled`/`run_compiled_outcome`/`run_compiled_label_outcome`, the no-reset
  `run_native_resume_outcome` (the `NativeCode`-generic no-reset resume seam, which
  `run_compiled_resume_outcome` wraps), and the shared `enter_machine`/`run_native`/`run_native_outcome`/
  `run_native_label_outcome`/`drive`; the `RunLabels` enum (`Stored` = the artifact's map, `Built` =
  the per-run fallback); the `NativeCode` trait (`enter(rt, module, func, pc)` + `module_blocks` +
  `labels`) and its `BlockEntries` impl; `enter_compiled`/`block_entries`.
- `runtime.rs` — the block ABI (`HALT`=`-1`/`ERROR`=`-2`/`ENTER`=`-3`/`LOCAL_DECLINE`=`-4`/`WAIT_BIT`,
  `encode_target`/`decode_target`/`encode_wait`/`decode_wait`, `JitRuntime` — including its
  `entry_module` field, the run's context-`0` module handed to the BIF-dispatch and raise funnels and
  never read by generated code, and its `literals_base`/`literals_end` publication —
  `RUNTIME_{MACHINE,XREGS,YBASE,YEND,LITERALS,LITERALS_END}_DISP`) and the helpers: `jit_step_one`
  (per-instruction deopt → `Machine::step_instruction_with_entry`; a `Step::Enter` returns the `ENTER`
  word with the target in `JitRuntime::enter`), `jit_ext_call` (the native
  `call_ext`/`call_ext_only`/`call_ext_last` arm: import resolution through the host seam plus the
  variant's frame discipline — the `Continuation::Address{func, pc+1, context}` store, nothing, or
  `pop_frame_checked` — handed to the driver as `ENTER`), `jit_step_ext` (the `call_ext*` deopt
  carrying the host's **already-computed** seam answer into `Machine::call_ext_dispatch`, so a declined
  import is resolved **once**, not twice), `fold_step` (the single `Step`→block-ABI encoder both
  `jit_step_one` and `jit_step_ext` end in), `resolve_ext_entry`, `jit_local_call`/`probe_local_call`
  (the local-call *as-BIF* seam probe, answering `LOCAL_DECLINE`; it takes the whole resolution as
  block-ABI immediates — the `LocalCallHelper` type, the packed `encode_local_call(callee, arity)` word
  and the `LOCAL_CALL_ONLY`/`LOCAL_CALL_LAST` + `deallocate` pair — so it re-decodes no instruction and
  resolves no label; the re-derivation survives only as a `debug_assert!` cross-check, zero release
  cost), `decorate`, `jit_tail_call`
  (`Machine::pop_frame_checked` for a linked local `call_last`),
  `jit_deallocate`/`jit_return`/`jit_deallocate_return` (the frame-discipline helpers: the checked pop
  and/or the continuation read, encoding the instruction's own outcome as the block-ABI word — sharing
  `return_word` for the continuation→word mapping and `report_frame_error` for the typed diagnostic, with
  the `FrameExitHelper`/`ReturnHelper` fn-pointer types; `return_word` also resolves a continuation block
  entry in-block so a `return` need not round-trip the driver — `resume_in_block` →
  `resolve_resume_entry` answers a **same-function** entry from the published `EntryTables` (two `Vec`
  indexings, no artifact lookup) and hands anything else to `cross_function_entry`, which resolves the
  address through `CompiledModule::native_entry_tables` on the artifact the driver published in
  `JitRuntime::compiled` and checks **that function's** register-file bound in Rust (the same-function
  bound argument does not transfer to a different function's code); on a hit `resume_in_block` performs
  the skipped driver iteration's position store, reduction bump and `ctx.reduce(1)` — in the driver's
  order, so a refusal parks exactly where a driver-side refusal would — re-publishes the target
  function's tables for a cross-function transfer, and the emitted arm tail-jumps to the entry with
  `JitRuntime::resume_entry` carrying it, while every other outcome leaves the word for the driver.
  `EntryTables::of` is the one place an `EntryTables` is built, used by both the driver's publication
  and a re-publication), `jit_allocate`/`jit_allocate_heap`/
  `jit_test_heap`/`jit_trim` (the **heap / frame-admission** family: the interpreter's own
  `ensure_heap` / `trim` calls in the interpreter's own order, and for the two `allocate` helpers
  `Machine::push_frame_window` — the push that also hands back the new frame's `(y_base, y_count)`, so
  the helper publishes the Y window from figures already in hand (`JitRuntime::publish_y_window`)
  rather than re-deriving them — each answering `encode_target(func, pc + 1)` or `report_frame_error`'s
  `ERROR`, with the `FrameSizeHelper`/`HeapNeedHelper`/`AllocateHeapHelper` fn-pointer types),
  `jit_set_position` (`Machine::set_current_code`, the block prologue's position store),
  `jit_publish_y` (`jit_set_position` + `JitRuntime::refresh_y_base` in one call — a Y-accessing
  block's prologue publish) and `jit_refresh_y` (the gated post-helper window refresh); plus
  `JitRuntime::refresh_literals`/`literal_bounds` (the literal-area publication `emit/move.rs`'s
  literal guard reads) and the `NativeBlock`/`StepHelper`/`TailHelper`/`PositionHelper`/`RefreshHelper`
  type aliases.
- `call.rs` — `jit_call_store` + `CALL_STORED`/`CallOutcome`: the frame-preserving local `call`'s
  continuation store *and* the local-call seam probe in one helper call, in the interpreter's own
  probe-then-store order. `CALL_STORED` (`-5`) is consumed inside the emitted block and never reaches
  the driver's decoder. Its **linking twin** `jit_call_store_linked` (same signature, the mode is a
  private `CallMode { Store, StoreAndLink }` behind the shared `call_store_impl`) additionally resolves
  the callee's native block entry — on the seam's *declined* path only — so the emitted block tail-jumps
  into it instead of returning a word (see `## Measured` and `emit/CONTEXT.md`). `CallOutcome::Word(i64)`
  is how a declined transfer, or a refused pre-emption charge (`ERROR`), leaves the block.
- `family.rs` — `jit_family`: the shared helper for every family-owned instruction. It records the
  position (`machine.set_current_code`), resolves the instruction, and runs the identical
  `crate::families::dispatch(machine, entry_module, module, labels, instr, func, pc, ctx)` call the
  interpreter's per-instruction step runs, then folds the outcome with `runtime.rs::fold_ok_step` /
  `fold_err`. **The success path never boxes**: the family's own `Result<Step, BeamError>` is matched in
  place and only an `Err` is handed on, so a successful family instruction pays no `Box::new` — a
  boxed `Result` re-boxed the 72-byte `Ok` payload too, i.e. two `malloc`/`free` pairs per instruction
  (`perf annotate`'s top row before the split). The `with_uncaught_frames` decoration still
  runs on the `Err` arm, before the error is stored — **unconditionally**, with the run's true
  `entry_module`: a block this helper is called from never passes through `step_instruction`, so this is
  the one place a raise it reports is decorated, in the entry context and inside a cross-module callee
  alike.
- `bitstr.rs` — `jit_bitstr`: the bitstring family's own helper. The emitter claims on the family's
  own `families::bitstrings::claimed` predicate — the one function the router
  (`families/mod.rs`'s `_ if bitstrings::claimed(instr)` arm) routes on — and reaches
  `crate::families::bitstrings::exec` **directly**, skipping `crate::families::dispatch`'s variant
  re-match: same position store, same resolve-or-decline to `jit_step_one` (also for a variant the
  family answers `Ok(None)` for), same unconditional `with_uncaught_frames` raise decoration (with the
  run's `entry_module`), same `fold_ok_step`/`fold_err` encoding. The `class_atoms_live` latch is the **family entry's** own (`bitstrings::exec` installs it
  before the variant check), so a direct caller gets it too and this helper performs no copy of it.
  It runs **no** per-execution shape predicate: the route is decided at compile time (re-testing it
  per execution measured ≈⅓ of the win). `bitstr_calls()` is `#[cfg(test)]`-only — it proves the
  helper actually ran; the claim set itself has no test-build copy.
- `test_helper.rs` — the comparison-`test` helper (`jit_test`, `TEST_TRUE`/`TEST_FALSE`): it resolves the instruction at `(func, pc)` in the active module and evaluates a two-operand comparison `test` through `beam_term`'s `cmp`/`cmp_arith`, reporting the outcome as a boolean (the block-ABI `ERROR` word on a propagating read error) and handing anything it does not own to `jit_step_one`.
- `receive.rs` — `jit_receive`: the `send`/receive-control family's own body plus the interpreter step's position store and raise funnel.
- `emit/` — `compile.rs` (the compilation entry points `compile_module`/`compile_function`, `emit_function`,
  the continuation sub-block emitters `stores_continuation`/`sub_blocks_capacity_estimate`/`emit_sub_blocks`/
  `emit_shared_sub_blocks`, and the code-size estimates), `block.rs` (the per-instruction lowering
  dispatch `emit_block` + the compiled-block shape) with `prologue.rs` (the compiled-block prologue
  `emit_block_prologue` + its position-store helpers) and `mod.rs` (the shared wiring/constants and the
  helper-sequencing layer), + live-interval regalloc, and the per-group emitters
  (`r#move.rs`, `test.rs` (+ `test/header.rs`/`test/tests.rs`/`test/refusal_census.rs`), `select.rs`,
  `arith.rs`, `compare.rs`, `aggregate.rs` (+ `aggregate/list.rs`), `frame.rs`, `heap.rs`, `family.rs`,
  `link.rs`, `operands.rs`, `liveness.rs`, `regalloc.rs`, `regs.rs`). `Allocation::y_last`/
  `refresh_y_after` and the Y-window publish/refresh emission live in `prologue.rs`/`regalloc.rs`; the
  publish gate itself is `regs.rs::Allocation::prologue_publishes_y`, fed by `regalloc.rs`'s
  `y_first`/`frame_republish_first` derivation.
  See `emit/CONTEXT.md`.


## Key facts (summary — the authoritative per-shape record is `inlineset/CONTEXT.md`)
- **Inline dispatch** is the per-instruction `match instr` in `emit_block`; every group it does not lower falls to the `_` arm's per-instruction deopt (`jit_step_one` = one `Machine::step_instruction_with_entry`), so a tier is interpreter-identical for every supported opcode. `FuncInfo` is deliberately never inlined (it is the failed-clause `function_clause` raise).
- **The whole family-owned surface is one arm** (`emit/family.rs::emit_family` → `../family.rs::jit_family`), which runs the identical `crate::families::dispatch` call the interpreter's per-instruction step runs.
- **`CallExt*` executes natively but the cross-module transfer is a driver round-trip by design**; the frame-preserving local `Call`'s same-module callee is linked **in-block** through the shared `enter_in_block` resolver, as a tail transfer.
- **The register cache is partial in both directions** (dirty-set `spill`, liveness `fill`, live-in prologue `entry_fill`); `emit/regs/CONTEXT.md` is its only owner.
- **A `Y` slot is guarded at run time** by the published frame window; every Y operands' guard can only ever *reject* (branch to `jit_step_one`), never fabricate a result.
- The full inline-set table, every restricted shape, the three deopt levels and the do-not-re-open rationale for each measured lever: `inlineset/CONTEXT.md`.
- Every measured figure (levers, slopes, corpus counters, the honest throughput negative): `measure/CONTEXT.md`.
- The deopt / execution / code-size censuses and their gate commands: `census/CONTEXT.md`.
- The test enumeration: `testing/CONTEXT.md`.

## Emitted-byte arithmetic (derived from the encoder, not measured)
`emit_mem_modrm` (`../../emitter/mod.rs:532-541`) always emits `mod=10` `disp32` (+ a `SIB` byte only when the base is `rsp`/`r12`), so **every** code sequence's size is a closed-form constant — no case needs disassembly to price:
`mov_mr/mov_rm` with base `rbx`/`rdi`/`r11` = **7** B, base `rsp` = **8** B, `mov_ri` (always `movabs`) = **10** B, `call_r`/`jmp_r`/`cmp_rr`/`test_rr` = **3** B, `jcc` = **6** B, `jmp` = **5** B, `lea rsp,[rsp±d]` = **8** B, `push_r`/`pop_r` = 1 B (`rbx`/`rbp`) or 2 B (`r12`-`r15`), `ret` = 1 B.
Consequences, with `S` = `Allocation::spill_slots_at(pc).len()` and `F` = `fill_slots(pc).len()`:
`emit_helper_call` (`emit/mod.rs:323-338`) = `7S + 8 + 10·args + 13` (41 B for 2 args, 71 B for 5); `emit_continue_check` = `19 + 7F` (+21 B for the gated `emit_refresh_y`, `:409`); `emit_frame_change_continue` (`emit/heap.rs:218-229`) = 19 B with **no** refill; `emit_resume_or_exit` (`emit/frame.rs:259-276`) = `27 + teardown`; `emit_prologue_helper` (`emit/prologue.rs:152-163`) = 41 B; `emit_local_call_probe` (`emit/block.rs:1013-1040`) = `7S + 90`. Prologue = `65 + Σpush + 7E` (`E` = `entry_len()`), teardown = `8 + Σpop`, `done` epilogue = teardown + 1.
So a `jit_allocate` in-block lowering is `7S + 70` B (+21 B if a later instruction of the block reads a `Y` slot) where the deopt it replaces is `7S + 7F + 60` B (+21 B): **+10 B of code to remove one interpreter step at run time**, and the only spill/refill it carries is the mandatory pre-call spill `S`.
The authoritative *measured* per-kind bytes are `census.rs::the_per_kind_emitted_code_size_table_is_printed` (`measure_one_block`, `census.rs:846-851`), which is also where a new shape's row is cheapest to add.

## Known Issues
- **A per-run memo for the cross-function in-block transfer is a measured net loss and is not in the tree.**
  `enter_in_block`'s cross-function arm (`resolve_resume_entry` → `cross_function_entry` →
  `CompiledModule::function`) re-resolves its target on every such transfer — `to_lower/1` asks the artifact
  **60.0** times per call. A 2-slot, artifact-keyed per-run memo answered **58** of those (resolutions
  60.0 → 2.0 per call, `in_block_calls` / declines / `driver_round_trips_per_call` identical) and still lost:
  `list_walk` **+1.84 %** (min 1.018353 / median 1.018368, 3 paired rounds, zero leg overlap) against
  `to_lower` −0.18 % (1.001811 / 1.001813). `list_walk` cycles `walk/1 → walk/3 → sum/1` against two slots, so
  every transfer pays the scan and still misses, while its hot recursive `sum/1` call is same-function and
  never consults the memo at all; on a 3-function cycle no wider memo clears the 0.3 % bar. The residual in
  `enter_in_block` is therefore the **frozen skipped-iteration reproduction** (`set_current_code` → `reductions`
  bump → `ctx.reduce(1)?`) plus the `EntryTables` publication, not the artifact lookup — do not re-open it as
  an entry-resolution problem. The instrument survives: `counters.rs::Counters::{cross_function_resolutions,
  cross_function_memo_hits}` + `cross_tests.rs` (both `#[cfg(test)]`, zero production cost).- **The probed local call now costs code size, and its remaining per-call cost is the seam itself.** Baking a tail call's resolution into the probe's block-ABI immediates removes the per-call instruction re-decode and label resolution (measured −0.41 % / −0.66 % retired instructions) but adds three immediate setups per probed site: **+0.78 % emitted bytes** corpus-wide, recovered to +0.47 % by the single-spill exit. Both are accepted trades, and neither changes a result. What is left on the shape is `Machine::local_call_seam` — the one seam the probe calls, which memoizes the callee identity **and** the host's verdict **per callee function index** in the executing module context's table (`LocalVerdicts`), so the per-call residual is one bounds-checked slot load plus the invalidation `Cell` read (`../../machine/CONTEXT.md`) — and the tier must not short-circuit it (its own optimisation belongs to its owner, not to this node).- **The tier's per-slice cost on a message-passing workload is the entry-module resolution, NOT the deopts — and for a real `.beam` that resolution is O(1).** Every `run_baseline_cached*` slice entry/resume resolves the entry module through `CodeCache::compiled_handle(&Module)` (`../cache/mod.rs`), whose `ModuleKey::of` is a **two-variant** identity: `Digest([u8; 16])` = the module's **load-time MD5** (`Module::checksum`, read as a plain `Copy` — no hash, no allocation) for a real `.beam`, else `Content { name, fingerprint }` over the whole-IR hash (`module_fingerprint`, a full `Debug`-render hash of the decoded IR) for a synthetic all-zero-checksum `Module::default()`. A real `.beam` therefore resolves with **zero** IR hashes on every slice, primed or not, and only a synthetic module can fall to `module_fingerprint` per slice; the address memo (filled by `cached_module_blocks(&Arc<Module>)` on the cross-module *callee* path and by the host-facing `cache::remember_module(&Arc<Module>)` on the *entry* path) is a redundant micro-optimisation on that digest path. The interpreter tier never touches `CodeCache`. Measured by the two-process copy-bound probe (`tests.rs::the_entry_module_priming_is_measured_on_both_tiers`, both tiers named, with `/proc/loadavg` and the per-pass `module_fingerprint` count): the probe's **synthetic** modules take the `Content` fallback, so unprimed they re-hash per slice (the probe's printed ≈6.8×-of-interpreter reading) and primed they read ≈0.8× with **zero** hashes — both are **single-run estimates from that probe, not pinned results** (`tests.rs::the_entry_module_priming_is_measured_on_both_tiers` asserts the `module_fingerprint` counts only; the authoritative tier ratios live in `../../../MEASURED.md`) — the tier's own work is parity-or-better and the whole gap is that hash; a real `.beam` needs no priming to reach the same zero-hash result (`cache/tests.rs::a_real_loaded_beam_never_rehashes_its_ir_on_any_entry_path`). `remember_module` is the entry-path priming API and has **no production caller** — its only callers are tests (`jit/baseline/tests.rs`, `jit/cache/tests.rs`); a host would call it once per module it enters, and the tier cannot pre-empt it (`compiled_handle` is called before any `baseline::run_compiled*` body). Deopt inlining — including the natively-lowered receive family and the family arm — does **not** move the wall-clock on this shape: the deopt chain is cheap relative to that hash. (A same-function block entry does not re-resolve its lazy function at all: the driver hands the entry the function artifact its previous entry resolved — `DriverContext::entry_cached` for the context-`0` artifact, `CalleeCode::cached` for each callee — so it costs the two-entry block-table probe with no `RefCell` probe and no `Rc::clone`; only an entry into a *different* function (a cross-function transfer, a call into another function, a fresh slice) re-resolves through `CompiledModule::native_entry_cached`. The driver's one charge per **block entry** against the interpreter's one per **instruction** is a real unit divergence, but ≈1 ns per iteration.)- **Both levers reduce counts but have no measurable throughput effect on this host (honest negative).** Interleaved A/B (before = the pre-lever tree, after = the variable-length frame + the live-in prologue fill; 9 paired rounds with the order reversed between rounds, `bench run --quick --runtime rust --workloads baseline,slice,foreign,list_walk,copy_bound,gc_bound`, one benchmark process at a time, `/proc/loadavg` 8–123 recorded at every run): every case's paired after/before ratio flips sign across rounds and each side's own run-to-run spread is 1.5–5.2×, so no case clears the spread (the min-over-N ratios 0.86–1.98 are confounded, because the two minima come from different rounds). The same harness *can* resolve a per-instruction difference: the tier ratio (default JIT ÷ `BEAM_RT_TIER=interpreter`) is stable and reproduces `tools/bench/RESULTS.md` §5 (`list_walk` **6.87×** here vs 6.86× there; the default tier is cheaper on 9 of 10 cases), so a ≲2 % wall-clock effect — the counters are −62.2 % push/pop ops, −53.4 % prologue loads, −4.39 % emitted bytes — is below this host's resolution. Do not re-run this A/B expecting signal on a loaded host; the levers' evidence is the deterministic emitted-instruction/byte reduction.
- **`emit/block.rs` (993 lines) holds the single per-instruction lowering dispatch** (`emit_block`, one `match` arm per instruction group plus an `emit_deopt` fallback), which is what makes it cohesive — splitting it further would scatter one decision across files or need an abstraction the emitter does not want; the compiled-block prologue it emits first lives in `emit/prologue.rs`. `emit/mod.rs` (401) is the shared wiring/constants and helper-sequencing layer.
- **Pre-existing files above the ~1000-line guidance, none of them a single decision the way `emit/block.rs` is**: `runtime.rs` (1482) is the block ABI plus every per-instruction helper, which all read the one `JitRuntime` layout; `mod.rs` (1286) is `BaselineJit` + the driver + `NativeCode`; `census.rs` (1583), `bif_tests.rs` (1745) and `tests.rs` (6339) are `#[cfg(test)]`-only. Splitting any of them is a separate, parity-neutral refactor.- **`counters.rs` is `#[cfg(test)]`-only**, so only a unit test under `src/jit/baseline/` can reach it; `census.rs` and `emit/test/refusal_census.rs` are the only non-`tests.rs` callers. A warmed `CodeCache` re-emits nothing, so a counter pin must compile cache-free through `emit::compile_function` (as `call_tests.rs::the_frame_preserving_local_call_records_no_static_deopt` does).

## Constraints
- `#![allow(unsafe_code)]` on `mod.rs`/`runtime.rs`/`call.rs`/`family.rs` and `emit/*` (JIT zone); every `unsafe` block carries a `// SAFETY:` rationale, and the safe-only modules (`receive.rs`, `test_helper.rs`, the test submodules) stay under the crate-level deny.
- Never panic / never a wrong result: an unsupported shape is the typed `error::unsupported`.


## Test Strategy
Every suite in this node is `#[cfg(test)]`, in-node and enumerated in `testing/CONTEXT.md` (the `#[path]` submodules of `tests.rs`, the named-BIF/bitstring/family/tail-exit/raise-tier pins, and `emit/test/refusal_census.rs`).
Tier equivalence on real modules is the crate's `tests/jit*.rs`, which drive the **lazy** path through `Machine::run_with_tier`.
The measurement instruments and their gate commands are `census/CONTEXT.md`; a green run without the system `erl` proves nothing about corpus parity (those suites skip loudly).
Run this node's suites with `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine --lib`.

## Routing Table
- `code.rs` → the artifact in its two layouts (`CompiledModule`/`CompiledFunction` [`entries` + `sub_entries`]/`EagerLayout::sub_entries`/`Summary`) + the compile-on-entry point.
- `emit/` → the compilation entry points + the per-instruction emitters (`compile.rs` `compile_module`/`compile_function`/`emit_function` + the continuation sub-blocks `stores_continuation`/`sub_blocks_capacity_estimate`/`emit_sub_blocks`/`emit_shared_sub_blocks` + the code-size estimates; `block.rs` (the per-instruction lowering dispatch `emit_block`) + `prologue.rs` (the compiled-block prologue `emit_block_prologue`) + `mod.rs` (the shared wiring/constants and helper-sequencing layer), `r#move/test/select/aggregate/arith/compare/bif/frame/heap/family/link/operands/liveness/regalloc.rs` (+ `test/header.rs`/`test/tests.rs`/`aggregate/list.rs`), `regs.rs` (the register cache: the `Allocation` + the dirty-set `spill` and partial `fill` (`spill_slots_at(pc)`/`fill_slots(pc)`) + `SpillSite`/`FillSite`), `test/refusal_census.rs` (test-only refusal attribution)) — `frame.rs` is the frame-discipline trio `Deallocate`/`Return`/`DeallocateReturn`, `heap.rs` the heap / frame-admission family `allocate`/`allocate_heap`/`test_heap`/`trim` (`emit_allocate`/`emit_allocate_heap`/`emit_test_heap`/`emit_trim` + the shared `emit_frame_change_continue`), and `family.rs` the shared arm for the family-owned surface (`emit_family`); the Y-window publish/refresh emission (`Allocation::prologue_publishes_y`/`refresh_y_after`), the `emit/operands.rs` readers (`x_offset`/`x_register`/`y_offset`/`y_register`/`y_addressable`/`constant_word`/`small_word`) plus the shared guarded-`Y` emission and the `Loc` abstraction (`locate_all`/`Y_ADDR_SCRATCH`/`Y_ADDR_SCRATCH_2`), the `emit/arith.rs`/`emit/r#move.rs`/`emit/aggregate.rs`/`emit/test.rs`/`emit/select.rs` Y support (`uses_y`, plus `emit/bif.rs::bif_uses_y` for the BIF accessors), and the `emit/regalloc.rs` `y_last`/`y_first`/`frame_republish_first` derivation; see its `CONTEXT.md`.
- `runtime.rs` → block ABI + `jit_step_one`/`jit_ext_call`/`jit_local_call`/`jit_tail_call`/`jit_deallocate`/`jit_return`/`jit_deallocate_return`/`jit_allocate`/`jit_allocate_heap`/`jit_test_heap`/`jit_trim`/`jit_set_position`/`jit_publish_y`/`jit_refresh_y`, the shared in-block resolver `enter_in_block` (`return_word` → `resume_in_block` → `resolve_resume_entry`/`cross_function_entry`) that both in-block transfers — a return's continuation and a non-tail `call`'s callee — go through; `jit_allocate`/`jit_allocate_heap` re-publish the Y window on their success path from `Machine::push_frame_window`'s `(y_base, y_count)` (`JitRuntime::publish_y_window`, no derivation), while `jit_trim`/`jit_deallocate` re-derive it (`JitRuntime::refresh_y_base`)
  + `RUNTIME_YBASE_DISP`/`RUNTIME_YEND_DISP`/`RUNTIME_LITERALS_DISP`/`RUNTIME_LITERALS_END_DISP` + `JitRuntime::refresh_literals`/`literal_bounds`; `tests.rs` → unit tests.
- `bif_helper.rs` → `jit_bif_value` (the `StepHelper`-shaped native evaluation of `erlang:byte_size/1`/`length/1`: `byte_size_value` = `bitstring_size(t).div_ceil(8)`, `list_length` = the proper-list walk, both through the engine's own `read_operand`/`write_operand`; `read_is_allocation_free` gates the one operand kind that could allocate) + `evaluate_bif`; it reuses `emit::arith::import_names` (the one shared import→name resolver) and declines to `jit_step_one` for every shape it cannot prove. `bif_tests.rs` → the named-BIF tier-equivalence/deopt suite above.
- `call.rs` → `jit_call_store` + `jit_call_store_linked` (the shared `call_store_impl` behind a private `CallMode`) + `CALL_STORED`/`CallOutcome`; `receive.rs` → the `jit_receive` `send`/receive-control helper; `family.rs` → `jit_family` (the shared family-owned-instruction helper, unboxed success path via `runtime.rs::fold_ok_step`/`fold_err`) + `tests.rs`'s pins; `bitstr.rs` → `jit_bitstr` + the test-build `bitstr_calls` (the bitstring family's own helper — the router bypass; its claim set is the family's `families::bitstrings::claimed`, with no local copy), `bitstr_tests.rs` → its pins (the one shared claim-set assertion: emitted set == router ownership == the family's `Ok(None)` contract; the decline-reaches-`jit_step_one` pin; tier agreement; the helper-actually-ran assertion); `test_helper.rs` → the comparison-`test` helper (`jit_test`, `TEST_TRUE`/`TEST_FALSE`) + its `jit_step_one` fallback for anything it does not own; `cross_tests.rs` → the in-block-transfer instrument's pins (cross-function resolutions/memo hits per call, the measured memo negative).
- `counters.rs` → the `#[cfg(test)]` structural counters (`Counters`, `record_*`, `snapshot`): the `jit_step_one`/`jit_step_ext` deopts (a `Test` deopt is keyed by kind — `"Test(IsEqExact)"`, `"Test(IsTuple)"`, … — in `deopts_by_variant`, so which `test` shapes still deopt is readable per predicate), the driver round-trips, the prologue position/publish calls, the **register-cache traffic** (`record_spill`/`record_fill`/`record_block`: per-block IR/byte/cache/write-set sizes + spill/fill stores by site), the **Y-window traffic** (`record_y_access`/`record_y_invalidation`/`record_block`: emitted guarded `Y` loads/stores + their emitted bytes, `Y`-touching blocks, the in-block refresh sites, and the same-block `Y`-slot reuse count) — one recorded run yields all **three** spill **store** models
  (the **dirty** set `spill` emits via `spill_slots_at(pc)`, the whole-block **written** set and the
  **full cache**) and both fill models, whose corpus-wide table is the test
  `the_register_cache_traffic_census_over_the_corpus_is_printed` — and the **static deopt census** (`record_instr_instance`/`record_instr_deopt` + the `instr_instances`/`instr_deopts` maps driving `census.rs`).
- `census.rs` → the `#[cfg(test)]` deopt census: the static corpus census (fixtures + generically-discovered OTP modules), the runtime hot-shape census (deopts **and**, from the same workloads on the interpreter tier, the `exec_census` per-variant execution ranking + typed-error table via its `dynamic_execution_census` helper), and the per-kind emitted-byte-size table, each printing a table; see `## Test Strategy` above and the file's own module docs.
- `runtime/` → the block ABI word table, the `JitRuntime` layout offsets, the helper inventory and the driver/`CALL_STORED`/transfer contracts; `measure/` → every measured figure and lever; `census/` → the deopt/execution/code-size instruments + gate commands; `testing/` → the test enumeration; `inlineset/` → the authoritative inline set and the rules each lowering obeys. Each has its own `CONTEXT.md`.
