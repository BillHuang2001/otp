# Evaluating a Rust Rewrite of ERTS/BEAM

*Scope: an engineering evaluation of the two proposed rewrite levels — (1) BEAM-assembly-level
drop-in replacement, and (2) source/AST-level compatibility — covering approach, expected
performance and non-performance gains, portability, and an appraisal of the rough plan itself.*

*Method: the evaluation is grounded in a read-only survey of this tree (ERTS at `30.0-rc0`;
`erts-17.1`), including the interpreter/JIT, term/GC/allocator internals, the I/O subsystem, the
compiler pipeline, and a quantified inventory of the external compatibility surface. Prior-art and
technology claims were checked against current public sources (see References).*

*Companion document: [`ERTS_RUST_REWRITE_DEEPDIVE.md`](./ERTS_RUST_REWRITE_DEEPDIVE.md) (Part II) continues this
evaluation with deep dives on the core-only-rewrite seam, data structures, GC, the compiler, the JIT and the
interpreter, and — given that development cost is now an accepted constraint — turns the conclusions into a staged
plan. Part II sharpens Part I's findings; where the two differ, Part II is the current position.*

---

## 0. Executive summary

**Verdict in one line:** the *direction* is sound and several of the sub-goals are worth pursuing,
but a *whole-VM big-bang Rust rewrite* is a multi-year, multi-team effort whose compatibility burden
is dominated by interfaces that **do not change between Level 1 and Level 2**, and whose expected
steady-state performance gain on typical BEAM workloads is modest (low single digits to low double
digits percent, with larger wins only in specific niches). The historical lesson the plan itself
cites — *low-latency, predictable compilation beats heavy optimization* — argues **against** a
"more optimizing JIT" as the headline goal and **for** a tiered design plus targeted, incremental
modernization.

Key findings:

1. **Level 1 is far bigger than "reimplement the interpreter."** A drop-in must reproduce a frozen,
   append-only external surface: ~556 BIFs, ~176 loadable `enif_*` NIF symbols resolved by `dlopen`
   against the executable, ~113 driver functions, the ETF wire format (31 term tags, `VERSION_MAGIC
   131`) and distribution protocol v6 (34 `DFLAG_*`, 29 `DOP_*` opcodes), ~46 documented `+` flags,
   ~123 `system_info` keys, the crash-dump format, and the boot/preloaded-module contract. These are
   **unchanged between Level 1 and Level 2**, so Level 2 does not reduce the hard part — it only
   removes the obligation to *read existing `.beam` files*.

2. **The "naive data structures" premise is only partly true.** Lists are genuine cons cells, but on
   BEAM a freshly built list is *already laid out contiguously* by the bump-allocating process heap.
   Maps are HAMT (4-bit/16-way) with a flatmap fast path, not naive; binaries/funs/nodes are
   off-heap reference-counted. OTP 27 already performs **in-place ("destructive") updates** for
   uniquely-owned tuples/records, and the compiler already fuses some tuple/map updates. The
   residual wins are real but smaller than assumed, and many are deliverable *inside* the current VM.

3. **The JIT's biggest untapped win is register allocation, not backend choice.** BeamAsm keeps the
   `x`/`y` register arrays *in memory* and does "hardly any cross-instruction optimization" — that is
   a deliberate latency/simplicity tradeoff. A Rust JIT built on **Cranelift** (native Rust, ~10×
   faster than LLVM, Apache-2.0, secure) or a **copy-and-patch** baseline can add local register
   allocation and modest cross-instruction optimization *while staying low-latency* — this is the
   concrete, defensible JIT opportunity, and it is achievable behind the existing loader/C-ABI seam.

4. **I/O gains are real but workload-dependent and Linux-centric.** `io_uring` is not a universal
   win (it is faster for ping-pong/zero-copy, sometimes *slower* for streaming than epoll) and is
   Linux-only; the right design is an **optional completion-based backend behind the existing
   `erts_poll`/`erl_check_io` abstraction**, leaving epoll/kqueue/IOCP as portable defaults. The
   largest I/O win is *file* I/O (currently punted to dirty schedulers), where `io_uring` can
   eliminate thread-pool round trips.

5. **The hardest problem — and the plan's biggest gap — is the garbage collector.** Rust has no GC
   and its ownership model does not apply to a self-referential, moving, per-process heap. The
   interpreter/JIT core will be unavoidable `unsafe` Rust. The *robustness* payoff of Rust is real
   but concentrated in the **safe periphery** (syscall/IO layers, distribution codec, term
   decoding, NIF/driver loading, regex, allocators) — which is also where most ERTS security bugs
   actually live. That suggests a *targeted* Rust strategy rather than a whole-VM rewrite.

6. **Realistic alternative with most of the upside:** pursue the four goals as **incremental
   projects behind stable seams** (a new optional JIT tier; an optional `io_uring` poller;
   compiler-level fusion/FBIP; and Rust replacements for isolated C components), rather than a
   from-scratch VM. Reserve the "clean-room Rust BEAM" as a research track, not a product plan.

---

## A. The compatibility map (what "full compatibility" actually requires)

Compatibility is not one thing. Nine distinct contracts must be honored; only one of them (§A.2)
differs between Level 1 and Level 2. **Seven of the nine are runtime-external and are identical for
both levels.**

| # | Contract | What it pins down | Changes between L1/L2? |
|---|----------|-------------------|:--:|
| A.1 | **Language semantics** | term equality/ordering, error reasons, exception/stacktrace shape, process/port semantics, GC-observable behaviour (`erts_debug:same/2`, memory flags) | No |
| A.2 | **`.beam` binary / asm** | IFF container, `BEAM_FORMAT_NUMBER=0`, generic opcode numbering + operand arity, `MAX_GENERIC_OPCODE=193`, module MD5, literals-as-ETF | **Yes** (L2 may change it) |
| A.3 | **NIF C ABI** | ~176 `enif_*` symbols, `ErlNifEntry`/`ErlNifBinary`/`ErlNifMonitor` layouts, `dlopen` symbol resolution, resource/monitor/dirty/select semantics | No |
| A.4 | **Driver C ABI** | ~113 `driver_*`/`erl_drv_*` symbols, `ErlDrvEntry` (24 fields), port-task model, `ddll` loading | No |
| A.5 | **Distribution / wire** | ETF tags, protocol v6 handshake, `DFLAG_*` negotiation, `DOP_*` control messages, atom cache, fragmentation, `epmd` | No |
| A.6 | **CLI / configuration** | `+`/`-` flags, `-M*` allocator switches, ~10 env vars, `ERL_*FLAGS` precedence | No |
| A.7 | **OTP app contracts** | 22 preloaded modules, boot-script/`init.erl` boot sequence, `erts_internal`/`prim_*` APIs called by `kernel`/`stdlib` | No |
| A.8 | **Introspection / tooling** | 123 `system_info` keys, 41 `process_info` keys, tracing (`erl_tracer` 18-callback contract), crash-dump `0.5`, `perf`-map, LTTng/DTrace probes | No |
| A.9 | **Build / bootstrapping** | OTP self-hosts: the VM must run its own compiler to build the tree | No |

**Consequence for the plan:** Level 2 gives freedom *only* over A.2 (and only if you also recompile
the entire ecosystem from source). It does **not** relieve A.3–A.9. Treating Level 1 and Level 2 as
"easier" and "harder" versions of the same task is therefore a category error — they share the same
80% of external-surface work; Level 2 adds ISA/compiler freedom on top. This is the single most
important framing correction for the plan.

### A.2 detail — the bytecode contract is subtle and unforgiving
- The code chunk carries `BEAM_FORMAT_NUMBER = 0`, never bumped. Opcodes are **1-based, append-only,
  never reused**: 193 numbered generic opcodes exist, of which 134 are live and 59 are obsolete
  (reserved). The loader enforces `max_opcode ≤ 193` and `≥ 169` (pre-OTP-25 rejected), plus
  per-instruction operand arity from the generated `gen_opc[]` table.
- **Silent-corruption trap:** renumbering an existing generic opcode while leaving
  `BEAM_FORMAT_NUMBER=0` passes the header bounds check but desynchronizes the loader on operand
  arity. A drop-in must match numbering *and* arity exactly, and accept canary opcodes up to 193
  (e.g. `executable_line/2`=183, `call_fun2/3`=178, `get_record_elements_id/4`=192).
- Only what is *internal* to the loader (specific-opcode numbering, `.tab` transforms, the
  three-generation code index, `ErtsCodeInfo` layout) is free to differ.

### A.3/A.4 detail — the C ABI is the real coupling and the real risk
- On Unix, NIFs and drivers are plain `.so`s whose `enif_*`/`driver_*` references are **undefined
  symbols resolved at `dlopen` from the main executable** (which must be linked `-rdynamic`). A
  single missing or mis-typed symbol breaks loading. On Windows both use **layout-frozen
  callback tables** (~180 NIF entries; 105 driver entries), which is why both APIs are append-only.
- Struct layouts are read directly by third-party code (`ErlNifBinary`, `ErlNifMonitor` = 32 bytes,
  `ErlNifMapIterator` with `__spare__`, `ErlDrvBinary`, `SysIOVec`), and `enif_get_tuple` must return
  a **contiguous** `ERL_NIF_TERM[]` while `enif_inspect_binary` must return a **direct data
  pointer** — these constrain the in-memory term representation even at Level 1.
- Upside of this constraint: it is a *hard, testable, additive* target. Reproducing a frozen ABI is
  well-understood engineering (and MMTk/Cranelift-style Rust projects already do exactly this for
  other runtimes). It is not research.

---

## B. Current ERTS, in numbers (the baseline to beat)

All counts verified against this working tree; emulator excludes vendored 3PP (`asmjit`, `pcre`,
`zlib`, `zstd`, `ryu`).

| Quantity | Value |
|---|---|
| Emulator C/C++/headers | **~440 kLOC**; `erts/` total **~630 kLOC** |
| Instruction-semantics `.tab` sources | ~14.8 kLOC |
| JIT codegen (arch-independent) | ~6.4 kLOC; **x86 ~19.7 kLOC**, **arm ~20.3 kLOC** |
| Interpreter (`beam/emu`) | ~11.1 kLOC |
| BIFs (`bif.tab`) | **556** (476 `bif`, 73 `ubif`, 7 `hbif`); 29 dirty; ~25.5 kLOC across 16 `erl_bif_*.c` |
| NIF API surface | **180 declared, ~176 active** `enif_*`; strictly append-only |
| Driver API surface | **114 declared** (113 functions + 1 data); `ErlDrvEntry` = 24 fields |
| Documented `+` flags | **~46** (`erts/doc/references/erl_cmd.md`); ~120 incl. `-M` sub-switches |
| `system_info` / `process_info` keys | **123** / **41** (docs list 90 of the former) |
| ETF term tags | 31 term + 11 wrapper/internal + `VERSION_MAGIC 131` |
| Distribution | protocol **v6**; **34 `DFLAG_*`**; **29 `DOP_*`** control ops |
| Preloaded modules | **22** (27 files incl. `erts.app.src`) |
| OTP applications | **35** |

**Term/GC/memory model (the part Rust must re-create):**
- `Eterm` is a word; 2-bit primary tag (`HEADER/LIST/BOXED/IMMED1`); on 64-bit the pointer tag mask
  is **3 bits** (a literal-area bit). Small ints are 60-bit immediates; atoms are 6-bit-shifted
  immediates; cons cells are header-less `[CAR,CDR]` two-word cells.
- Maps: flatmap (sorted key tuple, **linear** lookup) below 32 keys, HAMT (4-bit/16-way fanout,
  16-bit bitmap per node) above.
- Binaries: on-heap `ErlHeapBits` (≤64 B / ≤512 bits), else off-heap **reference-counted** `Binary`
  exposed as `BinRef`; sub-bitstrings and match contexts share one `ErlSubBits` struct.
- GC: **per-process generational semi-space (Cheney) copying** collector, 2 generations, curated
  heap-size table, off-heap MSO list swept by refcount; large GCs escaped to dirty schedulers.

**Execution engine:**
- Interpreter: 778-line dispatch loop (computed goto) over generated `beam_hot/warm/cold.h`.
- JIT (BeamAsm, since OTP 24): **load-time, per-instruction "template" translator** — one
  `emit_<instr>` per instruction, **no register allocation** (`x`/`y` live in memory), type-guided
  emission (OTP 25+), shared global fragments for GC/dispatch/NIF-call (code ~10% larger than the
  interpreter), W^X dual mapping, breakpoint/NIF trampolines.

**I/O:**
- `erts_poll` (epoll/kqueue//dev/poll/WaitForMultipleObjects) + `erl_check_io` (ONESHOT readiness,
  fd→state table, 128-striped event locks). Sockets: `enif_select`+pollset (Unix) or IOCP (Win32);
  file I/O via dirty I/O schedulers. 1 poll thread / 1 pollset default; 10 dirty-I/O schedulers
  default; async pool default 1. **No `io_uring` anywhere.**

---

## C. Lessons from Erlang's own history (the plan's stated compass)

The plan explicitly says: *use low-latency design over heavy JIT*. The history backs this strongly.

1. **HiPE (native-code compiler, 1990s–2010s) was removed in favour of BeamAsm.** A full
   optimizing native compiler proved not worth its maintenance and complexity relative to a much
   simpler translator.
2. **Three separate BEAMJIT *tracing* JITs, all LLVM-based, were abandoned** (research projects led
   by RISE/SICS). Reported failure causes:
   - tracing required interpreter instrumentation that *slowed the baseline interpreter*;
   - context-switching between interpreted and native code was hard to do without slowing the base;
   - **LLVM compilation was too slow** to be repaid — even with optimizations disabled, the
     sea-of-nodes IR could not compile fast enough for load-time use.
3. **BeamAsm's explicit tradeoff**: translate each instruction at load time, keep BEAM's memory
   register model, eliminate unpack/dispatch overhead; accept *less-optimized* code in exchange for
   **near-zero compile latency and predictable behaviour**. The team has repeatedly stated LLVM is
   unsuitable for this role.

**Generalized design principles for any new VM/JIT:**
- **Compile latency is a first-class correctness constraint** for soft-real-time/telecom workloads;
  a JIT that lengthens startup or tail latency is a non-starter regardless of peak throughput.
- **Tier, don't monolithic-optimize.** The modern answer (V8: Ignition → Sparkplug → Maglev →
  TurboFan; CPython 3.13: interpreter + copy-and-patch) is a *cheap baseline tier* plus optional,
  speculative, deopt-capable higher tiers. This fits BEAM far better than one heavy JIT.
- **Native code must remain purgeable/upgradeable** (hot code upgrade, `code:load_binary`,
  breakpoints, NIF loading) under W^X — a self-contained, testable subsystem.

---

## D. Technology survey (state of the art, and what fits BEAM)

**JIT backends**
- *asmjit today*: a lightweight x86-64/AArch64 assembler (~1:1 API→asm), vendored, used via a small
  API subset; great encoder, **but it does no register allocation or optimization** — so the current
  ceiling is set by the translator, not the encoder.
- *LLVM ORC*: best raw code quality, **wrong latency profile** (the historical failure mode). Only
  viable, if at all, as an opt-in top tier for a handful of hot functions.
- *Cranelift* (Rust, Wasmtime): designed for **fast compilation** ("about an order of magnitude
  faster than LLVM"), Apache-2.0, **memory-safe and fuzz/verification-hardened** against malicious
  compiler input, targets x86-64/aarch64/riscv64/s390x, does graph-coloring register allocation and
  light optimization. Output is ~2% behind V8 TurboFan and ~14% behind an LLVM system in one
  benchmark — i.e. good enough for a *baseline-to-midtier* JIT with **real register allocation that
  BeamAsm lacks**. This is the most natural Rust-VM backend.
- *Copy-and-patch* (Xu & Kjolstad; CPython 3.13, LuaJIT-Remake): precompile per-opcode "stencils"
  with an AOT compiler, concatenate + patch at runtime; **~µs-scale** compilation, code quality in
  the range of hand-written baseline JITs. Excellent fit for the baseline tier; needs build-time
  stencils per target and an AOT compiler at build time.
- *Hand-written Rust encoder* (e.g. `iced-x86`): maximum control and lowest latency, but replicates
  the ~40 kLOC of per-arch codegen that already exists — only worth it with a specific reason.

**Garbage collection in Rust**
- Rust has **no GC** and ownership does not model a moving, self-referential, per-process heap; the
  collector and the object graph will be `unsafe` Rust. The realistic options are (a) hand-written
  unsafe Rust (port the existing design), or (b) a toolkit such as **MMTk** (a Rust
  memory-management framework) — but MMTk assumes a global heap and does not do root discovery for
  you, so it fits BEAM's *per-process heaps* poorly without significant adaptation.
- Two directions worth evaluating: keep the **generational copying** design (great locality and
  latency, already tuned), or move toward **mark-region / Immix** (better for large heaps and lower
  copy cost; the `enigma` Rust-BEAM project listed "explore Immix" as a goal). Off-heap reference
  counting for large immutable objects (binaries, funs) should be kept — it is the right design.

**Persistent/functional data structures**
- Lists: singly-linked is *correct for value semantics* (O(1) tail sharing). Note **BEAM already
  gives fresh lists spatial locality** via bump allocation, so "lists are cache-hostile" is mostly a
  *post-GC* and *cross-generation* effect, not an intrinsic allocation effect.
- Maps: BEAM's HAMT is a 4-bit/16-way bitmap trie. Modern alternatives include **CHAMP**
  (Compressed Hash-Array Mapped Prefix-tree, used by Clojure/Immer) which improves iteration locality
  and node density, and **binary search / SIMD scan for the flatmap fast path** (currently linear).
  These are concrete, low-risk improvements — deliverable *inside* the existing VM.
- Persistence/versioning structures (RRB-vectors) are attractive for `array`-like loads but are not
  a drop-in replacement for cons lists without breaking tail sharing.

**FBIP / in-place functional update**
- The research frontier is **functional-but-in-place (FBIP)**: reference-counting with **reuse
  analysis** (Koka, Lean 4, Roc's Perceus), allowing a "destructive" update whenever a value is
  provably not shared. BEAM already has a conservative version of this (OTP 27 in-place record/tuple
  update). Extending it to maps/lists/binaries via alias/escape analysis is the highest-value
  *data-structure* idea, and it is a **compiler** feature more than a VM feature.

**I/O**
- `io_uring` (Linux ≥5.1): completion-based, batched submission, registered buffers, zero-copy
  send/recv; can saturate >1M IOPS single-threaded with SQPOLL. But **not a universal win**: it is
  faster in ping-pong patterns, sometimes slower than epoll in streaming, and its biggest edge comes
  with zero-copy (up to ~2.5×). It shines for **file** I/O (avoids thread pools) and high-IOPS
  storage.
- Rust ecosystem: `mio` (portable readiness: epoll/kqueue/IOCP/WASI), `tokio`/`monoio`/`io-uring`
  crates. **Caution:** BEAM's scheduler is a run-queue of green processes, not an async/await
  runtime; a full async runtime would fight the scheduler design. Use a *reactor* abstraction
  (readiness/completion) behind the existing port/select model, not a task runtime.

**Prior art to track**
- `enigma` (archseer): an Erlang VM in Rust — ambitious, incomplete, effectively stalled; a useful
  cautionary data point about scope.
- `Tyn`: a Rust microkernel that boots the **unmodified** OTP ERTS on bare metal — the *opposite*
  strategy (keep ERTS, replace the OS), and evidence that ERTS's portability seams are real.

---

## E. Level 1 evaluation — BEAM-assembly-compatible drop-in

Goal: identical `.beam` input, identical external behaviour, drop-in for `beam.smp`.

### E.1 Rust core — feasibility and payoff
- **Feasible but dominated by compatibility, not by language.** ~440 kLOC of emulator logic (plus
  ~630 kLOC of `erts`) must be reproduced with byte-exact ABI and wire behaviour (§A). This is
  *mostly* mechanical-but-large; the genuinely novel work is in §E.2–E.4.
- **Robustness payoff is real but concentrated.** The interpreter/JIT core is unavoidably `unsafe`.
  Rust's safety benefits land in the *periphery*: the distribution/ETF codec, term decoding, driver
  and NIF loading/validation, regex, allocators, and syscall layers — which is where most ERTS
  CVEs historically occur. So "more robust" is a *targeted* claim, not a whole-VM one.
- **Toolchain/talent payoff** is genuine: `cargo`, `clippy`, `rustfmt`, Miri/fuzzing, a modern module
  system, and easier cross-compilation. But it does not require rewriting the VM to be realized
  (isolated components can be Rust already, e.g. via a C ABI).
- **Bootstrap hazard:** OTP builds itself with itself; the new VM must run the existing compiler and
  the whole 35-app tree early. Plan a shadow-VM bootstrap with a stable fallback (`FLAVOR=emu`).

### E.2 Data structures — what can actually improve
- **Constrained at Level 1** by NIF expectations (contiguous tuple arrays; direct binary data
  pointers) and by observable-identity APIs (`erts_debug:same/2`, `erts_debug:size/1`).
- Concrete, low-risk wins:
  - **Flatmap lookup**: replace the linear scan with binary search (and/or SIMD compare) — trivial,
    measurable for small/medium maps.
  - **HAMT → CHAMP-style node layout** to improve iteration locality and reduce branch-per-node.
  - **Better allocation locality for cons cells** (e.g. batching/segregating list allocation, arena
    reuse) to reduce post-GC pointer chasing.
  - **Reduce copy pressure**: extend the existing destructive-update analysis; keep large immutable
    data off-heap and refcounted (already true).
- **Not recommended:** changing the *representation* of lists (breaks O(1) tail sharing and value
  semantics) or big-int/atom encoding (observable). These are traps, not opportunities, at Level 1.
- **Net:** modest, incremental, partially achievable in the *current* VM without any rewrite.

### E.3 JIT — the strongest concrete opportunity
The plan is right that BeamAsm is not the last word, but the *reason* matters: the ceiling comes from
"no register allocation / no cross-instruction optimization," chosen for latency. A Rust JIT can
raise the ceiling **without** abandoning the latency principle:

- **Recommended architecture — three tiers behind a pluggable interface:**
  1. **Interpreter** (portable fallback; must remain, for `--disable-jit` platforms and diagnostics).
  2. **Baseline tier**: keep the load-time, per-instruction translation (port the current design, or
     use **copy-and-patch** stencils for ~µs compile) — but add a **basic-block register allocator**
     that keeps `x` registers (and `y` slots) in physical registers across a block instead of in
     memory. This is the single most credible baseline-speedup lever.
  3. **Optimizing tier (optional)**: promote hot functions to **Cranelift**-generated code with
     light cross-instruction optimization and type speculation + deopt. Never on the critical path;
     bounded compile budget; opt-out via flag.
- **Pluggability**: define a clean, stable **IR boundary** between the loader and the backends
  (essentially "BEAM asm → a typed, block-structured IR → backend"). This satisfies "more plug-able"
  and lets the interpreter, BeamAsm-style encoder, Cranelift, and copy-and-patch coexist and be
  compared.
- **Must-not-regress list**: W^X/purge/upgrade, breakpoints, NIF-load trampolines, `perf`-map,
  differential JIT-vs-interpreter correctness tests, and **tail-latency/startup budgets**.

### E.4 I/O — real but bounded
- Replace the poller with a **portable reactor abstraction** exposing both *readiness* (epoll/kqueue/
  IOCP) and *completion* (`io_uring`) models; make `io_uring` an **optional Linux backend**, keeping
  epoll/kqueue/IOCP as defaults.
- Biggest wins: (a) **file** I/O without dirty-scheduler round-trips; (b) batched submission and
  registered buffers for high-connection-count servers; (c) zero-copy send/recv where the workload
  allows. Network-only workloads see marginal changes (epoll is already good) and can even regress —
  benchmark per workload, don't assume.
- Keep the current model's virtues: ONESHOT + re-arm, scheduler pollset, backpressure accounting.

**Level 1 verdict:** achievable, but it is a *large compatibility re-implementation* with *modest
net performance upside* and *targeted* robustness upside. The JIT-register-allocation and I/O items
are worth doing **regardless** of whether the rest of the VM is rewritten.

---

## F. Level 2 evaluation — source/AST-level compatibility

Goal: free to redesign the ISA/IR; recompile the ecosystem from source.

### F.1 What Level 2 actually buys, and costs
- **Buys:** freedom over §A.2 (you no longer read old `.beam`). You may redesign the instruction set,
  the SSA IR, and the optimizer without legacy-tagon.
- **Costs:**
  - The entire ecosystem must be **recompiled from source** — fine for OSS, fatal for any
    binary-only `.beam` dependency.
  - The **whole tooling chain** must move in lockstep: `beam_validator`, `beam_disasm`, `dialyzer`,
    `cover`, `debugger`, `erl_eval`, `syntax_tools`, line-number/stacktrace tables, `+J` flags,
    `erts_debug:disassemble`. A new ISA is a *whole-language-toolchain* change, not a VM change.
  - It still must satisfy §A.3–A.9 (NIF/driver ABI, distribution, CLI, boot, introspection). So the
    *marginal* work of Level 2 over Level 1 is the compiler + loader + tooling — significant, but not
    the dominant share.

### F.2 The genuinely interesting Level-2 ideas
1. **Operation fusion / deforestation.** Fuse chains of `lists`/comprehension/`maps` operations
   (`map`∘`map`∘`filter`, `foldl` chains) into single passes, eliminating intermediate lists. The
   compiler already inlines some `lists` HOFs (`sys_core_fold_lists`) and does block-local
   tuple/map-update merging — a real fusion pass generalizing this is a credible win for
   functional-style code. Watch semantics: error positions, evaluation order, and laziness must be
   preserved exactly.
2. **Loop optimization.** The tree has **no LICM, unrolling, strength reduction, or IV analysis**
   (the only loop-aware code is `receive` handling). For numeric/array-heavy Erlang this is a clear
   gap — a proper loop optimizer + the SSA type lattice (`beam_types`) is the most conventional
   compiler win available.
3. **FBIP / in-place functional update** (see §D): extend destructive updates to maps/lists/binaries
   via alias/escape analysis, and add **reuse analysis** so that building a "modified copy" reuses
   the original's cells when uniquely owned. This is the principled version of "fusing tuple/map
   updates" and directly attacks allocation/GC pressure — the *actual* bottleneck the plan cites.
4. **Register-based ISA cleanup / specialization.** Today's ISA is a flat, append-only, 193-opcode
   design with an `x(0)` accumulator convention, name-dispatched generic BIFs, and mini-languages
   embedded in operands (`bs_match`, `get_record_elements`). A redesigned typed ISA could:
   specialize per-type, drop the accumulator convention, fold the BIF dispatch tables, and reduce
   operand decoding — enabling better register allocation downstream.
5. **Type-directed specialization to native.** With full source-level type info, a Rust JIT could
   speculate on types and generate specialized code with deopt — the V8 Maglev-style mid-tier.

### F.3 Risks specific to Level 2
- **Ecosystem coordination**: this is effectively a hard fork; migration/versioning, dual toolchains,
  and a story for Elixir (which compiles to BEAM via the same compiler) must be designed first.
- **Semantic surface is larger than "asm"**: `erl_eval`, stacktrace line numbers, `receive` timing,
  and `+J`/trace semantics are observable; a new IR must reproduce them.
- **Research risk**: fusion and FBIP are proven in research languages but require careful spec
  (value semantics + sharing must be preserved); expect long validation.

**Level 2 verdict:** intellectually the most promising, but it multiplies coordination cost without
removing the ABI/wire/boot burden, and its best ideas (fusion, loop opts, FBIP) are **compiler**
features that can be prototyped on the **existing** SSA IR long before any VM rewrite.

---

## G. Cross-cutting concerns the plan must resolve

1. **GC & memory model (highest risk, least addressed).** Decide early: port the generational
   copying collector (unsafe Rust), or adopt a research GC. This choice constrains *everything* —
   term representation, the NIF pointer guarantees, the JIT's write barriers, and tail latency.
2. **NIF/driver ABI strategy.** Either reproduce the C ABI exactly (recommended; it is a hard,
   testable target) or you break `crypto`, `ssl`, `jiffy`, telemetry, and every Rustler/`enif_select`
   library. There is no middle ground.
3. **Bootstrapping.** The new VM must build OTP (35 apps) — design a shadow-build with a fallback VM.
4. **Portability.** Rust helps cross-compilation, but the **JIT is the portability hazard** (per-arch
   backend). Keep a fast interpreter for unsupported targets; treat `io_uring` as optional-Linux.
5. **Security posture.** Rust hardens the periphery, but a JIT enables code execution and the VM core
   is unsafe; W^X, JIT hardening, and fuzzing of the new decoder/codegen are mandatory.
6. **Licensing.** OTP is Apache-2.0; prefer Apache-2.0/MIT deps (Cranelift ✔, mio ✔, MMTk ✔).
   Re-verify the vendored-library situation (the ASMJIT-behind-the-JIT note in §C is a reminder).
7. **Success metrics.** Define them up front: startup latency, p99 tail latency, throughput on named
   benchmarks, memory footprint, and — critically — **"no compatibility regressions"** measured by a
   differential test suite run against the real ERTS.

---

## H. Appraisal of the rough plan

**What is good**
- The two-level decomposition is the right instinct: it separates a *conservative* (drop-in) path
  from an *exploratory* (source-compat) path.
- The explicit "low-latency over heavy JIT" principle is correct and directly supported by Erlang's
  own history; it should govern all JIT decisions.
- Identifying I/O and data structures as separate axes (not just "make the interpreter faster") is
  sound — these have independent, usable seams.

**What is missing or under-specified**
1. **The plan conflates four independent projects as one.** "Rust rewrite", "better data
   structures", "better/pluggable JIT", and "better I/O" have *different* justifications, risk
   profiles, and — importantly — **can be done mostly independently**. Bundling them into one
   rewrite multiplies risk and delays all value.
2. **It underweights the compatibility surface (§A).** The dominant cost (ABI, wire, CLI, boot,
   tooling) is shared by both levels and is not addressed.
3. **The GC/memory model is unaddressed** — the single hardest and most constraining decision.
4. **It assumes data structures are "naive"** — false in part; much of the low-hanging fruit is
   already taken (HAMT, off-heap refc binaries, OTP-27 in-place updates, some fusion). The plan
   should be re-based on the *actual* residual bottlenecks (post-GC locality, copying cost, absence
   of loop opts, allocation pressure from copy-on-write updates).
5. **No success metrics, no workload definition, no tail-latency budget.**
6. **No incremental/migration or dual-run strategy**, no differential-testing plan, no fallback.
7. **Level 2 is framed as "more freedom" but is actually "more coordination"** — it needs a fork/
   ecosystem migration plan before any code.
8. **No prior-art exploitation**: `enigma` (Rust BEAM) and `Tyn` (Rust microkernel + real ERTS) both
   contain hard-won lessons; the plan should explicitly learn from / reuse their findings.
9. **Alternative strategies are not considered**: e.g. (a) incremental Rust-ification of isolated
   components behind the C ABI; (b) JIT-tiering work inside the current VM; (c) replacing the OS/
   deployment layer (Tyn-style) rather than the VM.

**Overall:** a defensible *vision*, but not yet an executable plan. As written it is a 5–10+ engineer-
year, multi-team program with a high chance of net-negative compatibility/performance in the interim,
and its most valuable ideas are attainable at a fraction of the cost without a rewrite.

---

## I. Recommended strategy (incremental, seam-first)

Pursue the plan's goals as **four independent workstreams behind stable interfaces**, so each
delivers value and can be abandoned without loss:

1. **Workstream 1 — JIT tiering (highest ROI, no compatibility risk).**
   Introduce a typed, block-structured IR and a pluggable backend interface between loader and
   codegen. Ship a **baseline tier with block-level register allocation** (keep `x`/`y` in registers
   across blocks) and an **optional Cranelift optimizing tier** for hot functions with deopt. Keep a
   copy-and-patch baseline as an alternative. Guard with strict startup/tail-latency budgets and
   differential correctness tests.
2. **Workstream 2 — I/O reactor (medium ROI, low risk).**
   Refactor `erts_poll` into a reactor abstraction with readiness *and* completion providers; add an
   optional `io_uring` backend (Linux) and route **file** I/O through it, retiring many dirty-I/O
   round-trips. Keep epoll/kqueue/IOCP defaults. Benchmark per workload (ping-pong vs streaming).
3. **Workstream 3 — compiler/data-structure wins on the existing SSA IR (medium-high ROI).**
   Prototype: loop optimization (LICM/strength reduction/unrolling); generalized operation fusion
   (lists/comprehension/map chains); flatmap binary search / CHAMP nodes; and extend destructive
   update + reuse analysis (FBIP) to maps/lists/binaries. Measure allocation/GC-pressure reduction.
   These can land in `lib/compiler` + `erts` **without changing the ISA**.
4. **Workstream 4 — targeted Rust adoption (robustness, low risk).**
   Rewrite isolated, high-CVE, self-contained components in Rust behind the existing C ABI — e.g.
   the ETF/distribution codec, term decoders, the poller, allocators, or a socket backend — and
   build the compatibility differentially-tested harness. This captures the *robustness/toolchain*
   benefit incrementally, with zero compatibility risk, and doubles as the seed of a future VM.

Only if Workstreams 1–4 demonstrate that the *remaining* VM core (scheduler, GC, term ops) is the
bottleneck should a **fourth phase** contemplate porting the core — and even then, port **module by
module** behind the C ABI until the Rust core can stand alone, rather than as a big-bang rewrite.

Keep a **separate research track** for the Level-2 clean-room VM (new ISA, fusion, FBIP) with an
explicit exit criterion; do not put it on the product path until the differential-test corpus and the
ecosystem-recompile story exist.

---

## J. Risk register and payoff estimates

| Item | Effort | Perf payoff | Robustness payoff | Risk | Do it? |
|---|---|---|---|---|---|
| Whole-VM Rust rewrite (L1) | Very high (person-years) | Low–modest (uncertain sign) | Medium, concentrated in periphery | Very high | No (big-bang) |
| Whole-VM rewrite (L2, new ISA) | Very high + ecosystem fork | Moderate (fusion/loop/FBIP) | Medium | Very high | Research track only |
| Pluggable JIT + register alloc + tiering | High | **Medium–high** | Medium | Medium | **Yes** |
| `io_uring` backend behind reactor | Medium | Medium (file I/O), low (net) | Low | Low | **Yes** |
| Compiler fusion + loop opts + FBIP | Medium–high | Medium | Low | Medium | **Yes** |
| Flatmap/CHAMP/RRB data-structure tweaks | Low–medium | Low–medium | Low | Low | **Yes** |
| Rust for isolated high-CVE components | Medium | Low | **High** | Low | **Yes** |
| GC redesign (mark-region/Immix) | High | Uncertain | Medium | High | Evaluate only |

---

## K. Open questions (decisions needed before any code)

1. **Which workloads and which metric?** Throughput, p99 latency, startup, memory, or all? The JIT
   history says latency/predictability must dominate.
2. **GC strategy**: port the generational copying collector, or invest in mark-region/MMTk?
3. **NIF/driver ABI**: reproduce exactly (recommended) or accept a break in native-code ecosystem?
4. **Is a source-compat fork of the *compiler* + ecosystem acceptable** (the real cost of Level 2)?
5. **Bootstrap & fallback**: what runs the build while the new VM is immature?
6. **Team & timeline**: is this a research project or a shipping replacement, and by when?
7. **Relationship to upstream OTP**: fork, upstream contributions, or parallel track? (Affects
   whether Workstreams 1–4 can be upstreamed — which they mostly can.)

---

## Appendix — References

**In-tree grounding (this repository)**
- `erts/emulator/internal_doc/BeamAsm.md`, `GarbageCollection.md`, `CheckIO.md`,
  `ProcessManagementOptimizations.md`, `CarrierMigration.md`, `SuperCarrier.md`, `DelayedDealloc.md`,
  `ThreadProgress.md`, `PortSignals.md`, `Tracing.md`, `CodeLoading.md`.
- `erts/emulator/beam/erl_term.h`, `erl_gc.c`, `copy.c`, `erl_map.c`, `erl_bits.c`, `erl_binary.h`,
  `erl_alloc*.c`; `beam/jit/` (x86/arm), `beam/emu/`; `sys/common/erl_poll.c`, `erl_check_io.c`;
  `nifs/{common,unix,win32}`; `beam/bif.tab`, `erl_dirty_bif.tab`, `erl_nif*.h`, `erl_driver.h`.
- `lib/compiler/src/` (`compile.erl`, `beam_ssa*`, `beam_types`, `beam_ssa_opt`, `genop.tab`),
  `internal_doc/beam_ssa.md`.

**External**
- Erlang/OTP blog — *The Road to the JIT* (history of BEAMJIT/LLVM tracing JITs and the asmjit
  choice): https://www.erlang.org/blog/the-road-to-the-jit
- Erlang/OTP blog — *The Optimizations in Erlang/OTP 27* (destructive tuple/record update, `update_record_in_place`): https://www.erlang.org/blog/optimizations
- BeamAsm documentation (tracing, W^X, `perf`): https://www.erlang.org/doc/apps/erts/beamasm.html
- Erlang/OTP — External Term Format & distribution protocol docs.
- Haoran Xu & Fredrik Kjolstad, *Copy-and-Patch Compilation* (OOPSLA 2021); CPython 3.13 JIT.
- Cranelift (Wasmtime) — fast, secure Rust codegen: https://cranelift.dev
- V8 — Sparkplug / Maglev / TurboFan tiering: https://v8.dev/blog/maglev
- MMTk — Rust memory-management toolkit (per-process-heap caveat noted).
- `io_uring` evaluations (completion-based, zero-copy; ping-pong vs streaming):
  https://arxiv.org/html/2512.04859v1 ; https://github.com/axboe/liburing/issues/536
- CHAMP / RRB-trees / persistent data structures; FBIP (Perceus/Koka/Lean reuse analysis).
- Prior art: `archseer/enigma` (Erlang VM in Rust, stalled); *Tyn* (Rust microkernel hosting real ERTS).
