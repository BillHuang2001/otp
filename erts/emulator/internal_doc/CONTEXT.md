# erts/emulator/internal_doc — BEAM VM Internal Design Notes

## Intent
Vendor-neutral design documentation for BEAM VM internals, kept *inside* the emulator source tree so the rationale lives next to the code it governs.
Read the relevant document **before** changing scheduling, GC, tracing, code loading, port/IO, allocator, term-format, or opcode-codegen code — these notes explain invariants and the "why" that the code alone does not.
Documentation only: no compiled code, the only build artifact is the diagram/asset pipeline under `assets/`.

## API Surface — document index
| Document | Topic (one line) | Governed sources | Read before touching |
|---|---|---|---|
| `AutomaticYieldingOfCCode.md` | YCF (Yielding C Fun) source-to-source transformer that turns C functions into coroutine-style yielding NIFs/BIFs; ERTS conventions (`INCLUDE_YCF_TRANSFORMED_ONLY_FUNCTIONS`, `erts_ycf_trap_driver`, `*_ycf_gen_destroy`, `-debug`, pitfalls: stack pointers, macros, alloc/free on yield) | `../../lib_src/yielding_c_fun/`, `../beam/erl_map.c`, `../beam/utils.c` (`erts_qsort_ycf_gen_*`), `../beam/erl_db.c` (ets), `../beam/global.h` | adding or editing any C function that yields (all four callers cite this doc in a comment) |
| `BeamAsm.md` | The Erlang JIT: runtime BEAM→native conversion via asmjit, static register allocation, code loading/updating, export tracing, NIF loading, frame pointers, per-file layout, Linux `perf` + flame graphs | `../beam/jit/` (`asm_load.c`, `beam_asm.h`, `beam_jit_util/metadata/common/*.cpp`, `x86/`, `arm/`) | changing the JIT, an emitter, or opcode codegen |
| `beam_makeops.md` | The `beam_makeops` Perl generator and the `.tab` DSL: external/internal generic instructions, transformation rules, typed operands & predicates, specific instructions, fragment / combined instructions, C-interpreter and BeamAsm codegen | `../utils/beam_makeops`, `../beam/emu/*.tab` (`ops.tab`, `instrs.tab`, `bif_instrs.tab`, `arith_instrs.tab`, `generators.tab`, `predicates.tab`, …), `../beam/jit/{x86,arm}/*.tab`, `lib/compiler/src/genop.tab` (external MFA list), generated `beam_hot.h`/`beam_warm.h` | adding/changing an opcode or its generated implementation |
| `GarbageCollection.md` | Per-process generational semi-space copying GC (Cheney), from/to space, move markers, high-watermark/promotion, literals & literal tag, binary heap + MSO list, virtual binary heap, message-queue `off_heap`/`on_heap` | `../beam/erl_gc.c`, `../beam/erl_gc.h`, `../beam/erl_term.h`, `../beam/erl_message.c`, `../beam/erl_process.h`, `../beam/emu/beam_emu.c` | touching GC, heap/binary layout, or message queue data |
| `ThreadProgress.md` | Thread-progress mechanism replacing reference counting: managed vs unmanaged threads, memory-barrier guarantees, two-refcounter scheme to delay progress | `../beam/erl_thr_progress.c`, `../beam/erl_thr_progress.h` | any staging / deferred-free / "wait until all threads have passed" logic |
| `CodeLoading.md` | Non-blocking module loading (since OTP R16): prepare vs finish phase, single-loader permission, staging code index, 3 generations of code access structures, consistent code view | `../beam/beam_load.c`, code index (`../beam/beam_code.h`, `../beam/erl_process.h`), `erts_try_seize_code_load_permission` / `erts_release_code_load_permission`, `erlang:prepare_loading`/`finish_loading` | changing the code server, loader, or code access structures |
| `Tracing.md` | Non-blocking breakpoint setting; call/return/exception/export tracing, `GenericBp`, isolated trace sessions (`ErtsTraceSession`, OTP 27), `call_time`/`call_memory` per-scheduler hash tables (OTP 29) | `../beam/erl_trace.c`, `../beam/beam_bp.c`, JIT breakpoints (`../beam/jit/`) | changing tracing, trace patterns, or breakpoints |
| `PortSignals.md` | Scheduling of port signals to remove port-lock contention; port task/queue, busy-port flow control (high/low watermarks), preparing signals before taking the port lock, low-latency fast path | `../beam/erl_port_task.c`, `../beam/erl_port.h`, `../beam/erl_ptab.c` | changing driver/port signalling or port scheduling |
| `CheckIO.md` | The check-I/O subsystem: `erl_poll` (unix/win32) + portable `erl_check_io`, kernel (`epoll`/`kqueue`//dev/poll) vs user polling, fallback pollset, ONESHOT, `+IOt`/`+IOp`, scheduler pollset with `erts_port_task_outstanding_io_tasks` | `../sys/common/erl_poll.c`, `../sys/win32/erl_poll.c`, `../sys/common/erl_check_io.c`, `../beam/erl_port_task.c` | changing `driver_select` / `enif_select` / socket IO event delivery |
| `PTables.md` | The process & port tables: `pid`/port-id → struct mapping, lock-free-ish slot allocation, id reuse, iterating BIFs, rwlock, benchmark results | `../beam/erl_ptab.c`, `../beam/erl_ptab.h` | changing pid/port lookups, table growth, or iterating BIFs (`erlang:processes/0`, `erlang:ports/0`) |
| `ProcessManagementOptimizations.md` | Lock-free process status (32-bit bit-field flags), run queues, process migration/steal, atomically published load-balancing info reused per thread progress | `../beam/erl_process.c`, `../beam/erl_process.h` | changing scheduler run queues, process state transitions, or load balancing |
| `DelayedDealloc.md` | Lock-free per-allocator-instance deallocation "message box" (single linked list, split cache lines, 2 unmanaged refcounters, marker element) to cut allocator lock contention | `../beam/erl_alloc_util.c` | changing allocator free paths or inter-thread dealloc |
| `CarrierMigration.md` | Migrating multi-block carriers between allocator instances: carrier pool, per-instance `pooled_tree`, TRAITOR/BUSY states, avoiding the "bad cluster"/sentinel problem, interaction with `aoff*`/`gf`/`bf` strategies | `../beam/erl_alloc_util.c`, `../sys/common/erl_mmap.c` | changing allocator strategies, carrier pool, or `spcf`/`mseg` behaviour |
| `SuperCarrier.md` | Super carrier (`+MMscs`, R16B03): a pre-reserved region acting as ERTS' own `mmap`; `sa`/`sua` areas, `ErtsFreeSegDesc` + size/address RBTrees, `+MMscrpm`/`+MMsco`/`+MMscrfsd` | `../sys/common/erl_mmap.c`, `../beam/erl_alloc_util.c` | changing memory reservation, mmap, or carrier allocation |
| `CountingInstructions.md` | How to build `TYPE=icount`, run `cerl -icount`, and use `erts_debug:ic/1` to count per-instruction execution | `../Makefile.in` (`icount` target), the `erts_debug:ic` BIF, interpreter instruction counters | measuring opcode/codegen coverage |
| `erl_ext_dist.txt` | **Stub** (1 paragraph): the external term format + distribution protocol spec has moved to the ERTS User's Guide; retained only for old links | reference for `../beam/external.c` and `../beam/dist.c` | external term format / distribution protocol (go to `../../doc/` instead) |
| `NewLinking.tla` | TLA+ specification + TLC model of the **new link protocol** (OTP 23.3): per-process link state, link/unlink/unlink_ack messages, `ValidState` invariant; checked for 2 procs / ≤15 signals | distribution signal code (`../beam/dist.c`, `../beam/erl_proc_sig_queue.c`, kernel `dist_util`) | changing link/unlink protocol or signal ordering |
| `dec.dat`, `dec.erl` | `dec.dat` = 942 lines of `{RevList,Translation}` unicode denormalisation tuples (Apple TN1150 derived); `dec.erl` = a `-compile(export_all)` script that groups them and emits `erl_unicode_normalize.h`. Both kept "for reference"; the generated header is checked in and consumed by `../beam/erl_unicode.c` | output: `../beam/erl_unicode_normalize.h` | regenerating Unicode composition/normalisation tables |
| `assets/` | Diagrams referenced from the markdown. `assets/Makefile` exports `*.dia` → `*.eps` via `dia`, then `*.eps` → `*.png` via ImageMagick `convert -resize 65%`; `make update_png` commits the PNGs. `.gitignore` excludes the intermediate `*.eps`. GC + BeamAsm docs reference these images | n/a (referenced by markdown only) | — |

## Constraints
- Docs must stay in sync with the code: they are read as authoritative, so a change to a mechanism documented here entails updating the doc in the same commit series.
- Do not delete or rename files under `assets/` while a markdown file links to them (`GarbageCollection.md` links 6 `gc-*.png`; `BeamAsm.md` links `perf-beamasm.png`, `perf-beamasm.svg`, `perf-beamasm-merged.svg`, `beamasm-perf-annotate.png`).
- Binary/image assets carry a REUSE-style `.license` sidecar with `SPDX-License-Identifier: Apache-2.0` (same `%CopyrightBegin%`/`%CopyrightEnd%` wrapper); markdown/`.erl`/`.txt` files carry the license inline instead.
- `dec.dat`/`dec.erl` are not part of any build — do not wire them into the Makefiles; regeneration is manual and its output is committed.
- Prefer relative links between the docs (existing cross-refs use `beam_makeops.md#...`, `PortSignals.md`, `BeamAsm.md`).

## Design Decisions
- Design notes live in `erts/emulator/internal_doc/` (not a separate docs repo) so the rationale is versioned with the code and reviewable in the same commit; source files cite them by name (e.g. `../beam/erl_map.c`, `../beam/erl_db.c`, `../beam/utils.c`, `../beam/global.h` all point at `AutomaticYieldingOfCCode.md`).
- Several docs are written as *problem → solution → benchmark* narratives (CarrierMigration, DelayedDealloc, PTables, PortSignals, ProcessManagementOptimizations, ThreadProgress, CodeLoading, Tracing). That structure is deliberate: it records why the lock-free/staging design was chosen over the obvious alternative.
- `erl_ext_dist.txt` was deliberately gutted to a stub so the term format/dist spec has one canonical home (the ERTS User's Guide) while old URLs keep resolving.

## Known Issues / Gotchas
- **Outward links are pinned to old release tags.** `GarbageCollection.md` (20 links to `blob/OTP-18.0|OTP-19.0`), `CheckIO.md` and `AutomaticYieldingOfCCode.md` link to `github.com/erlang/otp` at old tags — line numbers AND paths refer to those tags, not HEAD (e.g. the interpreter's `beam_emu.c` now lives at `../beam/emu/beam_emu.c`). Always re-locate the symbol in the current tree.
- Docs reference sources as `$ERL_TOP/...`-relative paths; `$ERL_TOP` itself appears in the docs.
- Some sections describe mechanisms that have since been removed and are not flagged as historical in the text: the `Halfword emulator` section of `SuperCarrier.md` (no halfword emulator or its configure option exists in the current tree), and the `Future Improvements` section of `PTables.md` (written for R16B). Verify before relying on a detail.
- Several docs were authored when their feature landed (2013/2014 copyright starts) and have had only copyright/content touch-ups since; the "previously… / before OTP Rxx…" passages are design rationale, not a description of the current state.
- `CountingInstructions.md` shows a stale example banner (`Erlang/OTP 22`) and stale `cd erts/emulator && make icount` invocation — the principle holds, but build paths change.
- `dec.dat` is a plain-text Erlang term file (not binary) despite the `.dat` extension; the `dec.erl` output path in its comment (`$ERL_TOP/erts/beam`) is stale — the header now lives at `../beam/erl_unicode_normalize.h`.
- `assets/.gitignore` ignores `*.eps`, so a freshly built tree will show missing `.eps` files — this is expected, they are intermediates.
- `perf-beamasm.svg` and `perf-beamasm-merged.svg` have no `.license` sidecar while every other tracked asset does.

## Notes for Agents — practical reading paths
- Add/change an opcode → `beam_makeops.md` (+ its "Transformation rules", "Defining the implementation", "Code generation for BeamAsm" sections) together with `BeamAsm.md` and `../beam/emu/ops.tab` / `../beam/jit/*/ops.tab`.
- GC / heap / binary layout → `GarbageCollection.md` then `../beam/erl_gc.c`.
- Port, driver or socket I/O → `CheckIO.md` + `PortSignals.md` (+ `../beam/erl_port_task.c`, `../sys/common/erl_check_io.c`).
- Scheduler / run queue / process state → `ProcessManagementOptimizations.md` + `ThreadProgress.md`.
- Allocator, carrier, memory reservation → `DelayedDealloc.md` + `CarrierMigration.md` + `SuperCarrier.md` (+ `../beam/erl_alloc_util.c`, `../sys/common/erl_mmap.c`).
- Deferred/staged activation (code loading, tracing, breakpoints) → `ThreadProgress.md` is the prerequisite for both `CodeLoading.md` and `Tracing.md`.
- Yielding C code → `../../lib_src/yielding_c_fun/README.md` first, then `AutomaticYieldingOfCCode.md` for the ERTS conventions.
- Term format / distribution protocol → do NOT read `erl_ext_dist.txt`; it is a stub. Use the ERTS User's Guide under `../../doc/`.

## Dependencies
- None at build time. `assets/Makefile` needs `dia` and ImageMagick (`convert`) *only* to regenerate the figures — both are optional and not required to build or test OTP.
- Rendering the docs assumes GitHub-flavoured Markdown (image embeds, relative links, `#anchor` cross-refs).
- TLC (TLA+ model checker) is needed to re-run `NewLinking.tla`; not part of the build.

## Test Strategy
None — this directory is documentation. The only executable content is `assets/Makefile` (figure regeneration, `make diagrams` / `make png`) and `dec.erl` (manual unicode-table generator). Correctness is enforced by review: doc edits accompanying the corresponding code change.

## See Also
- `../beam/` → the C term/process/GC/tracing/port/allocator code these docs govern
- `../beam/jit/` → BeamAsm implementation (`BeamAsm.md`, `beam_makeops.md` § "Code generation for BeamAsm")
- `../beam/emu/` → traditional BEAM interpreter and its `*.tab` instruction definitions (interpreter half of `beam_makeops.md`)
- `../utils/` → build-time codegen tools, including `beam_makeops`
- `../sys/` → platform-specific code documented by `CheckIO.md`, `SuperCarrier.md`
- `../../lib_src/yielding_c_fun/` → YCF itself (docs referenced by `AutomaticYieldingOfCCode.md`)
- `../../../lib/compiler/src/genop.tab` → external generic instruction list consumed by `beam_makeops` (compiler side)
- `../../doc/` → user-facing ERTS User's Guide / reference (now owns the External Term Format & Distribution Protocol specs, and the "New Link Protocol")
- `../../CONTEXT.md` → ERTS overview and build/test strategy

## Routing Table
This node is a leaf: it holds flat documentation with no subdirectories warranting their own CONTEXT.md (`assets/` is a figure pipeline described above). Route work by document, not by subdirectory:
- Scheduling / run queues / process migration → `ProcessManagementOptimizations.md`, `ThreadProgress.md`
- GC & heap → `GarbageCollection.md`
- Ports, drivers, socket I/O → `PortSignals.md`, `CheckIO.md`
- Tracing / breakpoints / trace sessions → `Tracing.md`
- Code loading / upgrades → `CodeLoading.md`
- JIT / opcodes / instruction DSL → `BeamAsm.md`, `beam_makeops.md`
- Allocators / carriers / mmap → `DelayedDealloc.md`, `CarrierMigration.md`, `SuperCarrier.md`
- Process & port tables → `PTables.md`
- Yielding C code (NIF/BIF) → `AutomaticYieldingOfCCode.md`
- Link protocol model → `NewLinking.tla`
- Unicode tables → `dec.dat`, `dec.erl`
- External term format / dist protocol → `erl_ext_dist.txt` (stub; see `../../doc/`)
