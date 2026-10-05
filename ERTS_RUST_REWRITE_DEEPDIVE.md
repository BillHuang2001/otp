# Evaluating a Rust Rewrite of ERTS/BEAM — Part II: Deep Dives and a Staged Plan

*Continuation of [`ERTS_RUST_REWRITE_EVALUATION.md`](./ERTS_RUST_REWRITE_EVALUATION.md) (Part I).*
*Part I mapped the external compatibility surface, the current-ERTS baseline, and the JIT/GC/data-structure/I-O
landscape, and recommended a seam-first, incremental strategy.*

*Part II answers the follow-up questions in depth — (1) can we rewrite only the **core VM** and keep the loadable
NIFs, the OTP modules, and the existing C; (2) are **data structures** really the bottleneck; (3) what **GC** should
a Rust VM use; (4) how far can the **compiler**, the **JIT** and the **interpreter** be pushed; (5) how the
**process memory model** (per-process heaps, message copying, ETS) constrains shared data, and how far messaging /
ETS can be made cheaper — opportunistic common-case messaging wins on the current VM, and, in a rewrite, a
shared-object ETS redesign — and, given the user's stance that *development cost is acceptable as long as the path is
straightforward and achievable*, turns the picture into a **staged plan with explicit seams and break-points**.*

*Method: parallel read-only investigations grounded in this tree (ERTS `30.0-rc0` / `erts-17.1`) — the
execution engine, term/GC internals, messaging/ETS, the compiler, the interpreter, the JIT, and the stdlib — cross-checked against
current public sources. In-tree claims cite `file:line`; external claims are referenced in the appendix.*

---

## 0. Headline revisions to Part I

Part I's compatibility map, baseline numbers and history stand. Five conclusions are **sharpened or corrected**,
and one is **inverted**:

1. **A "core-only" rewrite does not shrink the compatibility burden — it relocates it.** Keeping the C periphery
   (NIF loader, ETS, distribution, drivers) forces the Rust core to reproduce the *internal* representation
   (`erl_term.h`, `Process`, the bump heap, `erts_alloc` types) bit-for-bit. The only *clean, precedent-backed*
   seam is the **execution engine** (`beam_asm.h` + register/offset/`ErtsCodeInfo` contract) — a Ruby-YJIT-shaped
   move. Everything else is a **link-time** boundary sharing frozen headers, not an API boundary. (§A)

2. **Data structures: the abstract value is fixed, the interior is an open, cache-friendly design space.**
   A list must stay a cons list (`[H|T]` O(1) and shareable) and the map stays persistent — but the *internal*
   layout beneath that interface is legitimately open, and OTP already ships dual layouts behind one abstract
   value (small map flatmap → HAMT; small on-heap binary → off-heap `BinRef`). The promising direction is a
   **third hybrid**: cache-friendly interior representations — chunked/unrolled cons lists with opportunistic
   chunking, and RRB/finger-tree interiors for sequence-like loads — paired with FBIP. Its payoff is gated on the
   §E compiler work, and it is not on its own the dominant bottleneck (that is per-element dispatch + allocation);
   **ETS `ordered_set` (AVL → K-ary B-tree)** is the one independent, low-risk, self-contained win. (§B, §C)

3. **The Enum-vs-Stream paradox is real and explains itself.** Eager `Enum.map` allocates a cons cell per element,
   and BEAM's bump allocation makes that about as cheap as an allocation can be (~60 cells = 1 reduction); a *lazy*
   stream replaces it with one **higher-order closure invocation per element per stage** (each = 1 reduction). So
   for moderate N the lazy pipeline's *dispatch overhead* is tens of times larger than the intermediate list it
   avoids; lazy only wins once the eager pipeline's *live set* starts driving GC. The fix is **compiler fusion +
   FBIP**, not tuning `Stream`. (§C)

4. **GC: keep the per-process model — it is the architecture, not legacy.** BEAM's per-process generational
   copying collector has two properties no shared-heap/MMTk design can match: **no global stop-the-world** and a
   **zero-cost generational barrier** (the "old never points to young" invariant falls out of term immutability, so
   there is literally no write barrier — `internal_doc/GarbageCollection.md:108`). MMTk assumes one global heap
   (its multi-instance mode is explicitly unsupported); GenImmix is the measurement-gated modernization of the
   *mature* generation; RC-with-reuse (Perceus/Koka) is a research track. (§D)

5. **Inverted: the plan's *most valuable* ideas are all reachable on the existing codebase.** Compiler fusion/loop
   opts/FBIP run on the existing BEAM-SSA IR and are **language-agnostic** (Erlang, Elixir, LFE all lower through
   `v3_core` → SSA). The JIT register-allocation win is real and the interpreter is already near its ceiling. So the
   right shape is a **staged program behind existing seams**, in which the "rewrite the VM in Rust" ambition is
   *last*, not first — and every earlier stage is independently shippable. (§E–§H)

6. **The memory model is the architecture, and messaging/ETS copies are its price.** Per-process heaps with
   independent moving GCs forbid *any* cross-heap pointer, so a local send **deep-copies the whole term graph**
   (`copy.c:647`) and every ETS put/read deep-copies in and out; in exchange process death is **O(1)** and there is
   **no write barrier**. Only objects that are **pointer-free, immutable and refcounted-or-permanent** — refc
   binaries, literal areas, `persistent_term` — cross the boundary cheaply today. *General* zero-copy messaging
   still needs a new shared-heap term class plus write barriers (a fundamental redesign), but there are concrete
   **opportunistic common-case messaging wins** (bounded single-pass small copy; static → auto-tuned message queue;
   fragment ownership transfer), and in a rewrite **ETS whole-object reads need not keep copying**: a
   shared-object model (immutable refcounted objects + COW writes + lock-free reads) is on the table, gated on a
   shared-term enabler whose real cost is **refcount/lifetime**, not the access path — literals already prove a
   zero-indirection off-heap compound representation. (§D)

Given the user's explicit acceptance of a large development effort, §H lays out the full staged plan with the
break-points at which each stage is independently valuable and abandonable.

---

## A. Can we rewrite only the core VM? — the seam question

**Question:** rewrite only the "core BEAM VM", keep the original loadable NIFs, keep the OTP (Erlang) modules
unchanged, and *re-use existing C*.

**Answer in three parts:**

1. **"Keep the OTP modules unchanged" — free.** They are bytecode; any core must satisfy the same behavioral
   contract (556 BIFs, the 22-preloaded-module boot sequence, `erts_internal`/`prim_*`), which is required at every
   level anyway.

2. **"Keep the loadable NIFs (Rustler / telemetry / crypto / jiffy) unchanged" — realistic, and the *easy* part.**
   On Unix, NIFs are `.so`s whose `enif_*` symbols are resolved by `dlopen` against the **`-rdynamic`-linked
   executable's dynamic symbol table** (`configure.ac:918-962`; `Makefile.in:1366`; `sys/unix/erl_unix_sys_ddll.c`).
   A Rust binary that *exports the same C symbols* (either by linking the retained C `erl_nif.c` + friends into it,
   or `#[no_mangle] extern "C"`) satisfies the ABI *by construction*. The NIF ABI is not only functions: it pins
   raw data — `ERL_NIF_TERM` is a raw word (`erl_nif.h:108`); `ErlNifBinary.data` is a **direct payload pointer**;
   `enif_get_tuple` returns a pointer *into the heap* (`*array = ptr+1`, `erl_nif.c:1586-1596`);
   `enif_get_list_cell` walks `[CAR|CDR]` (`erl_nif.c:1971`); `ErlNifMonitor` is exactly 32 bytes
   (`erl_drv_nif.h:67-69`); 180 `enif_*` API entries (`erl_nif_api_funcs.h`). On Windows it is a layout-frozen
   callback table. Good news: BIF/NIF failure is **return-based**, not `setjmp`/`longjmp` (`bif.h:221-457`), so no
   Rust unwind shim is needed for normal errors.

3. **"Re-use existing C (ETS/distribution/NIF loader/allocators)" — only as a *link-time sibling*, never behind a
   clean API.** This is the finding that matters. `erts/emulator/beam/CONTEXT.md` states it plainly: *"No internal
   library boundary: this is one flat compilation set."* Concretely:
   - **84 of 107 `beam/*.c` include `global.h`**, which pulls in the term/process/port/allocator headers
     (`global.h:26-50`) and even *defines* `struct enif_environment_t` (`global.h:57-209`).
   - The **tagged `Eterm` is header macros inlined into every translation unit** (`erl_term.h:70-305`): primary
     tags, small-int/atom encodings, boxed sub-tags, `CAR`/`CDR`, `make_arityval`. A Rust core and a C periphery
     **cannot differ by one bit** here.
   - The **heap is a bump allocator exposed as bare pointers**: `HAllocX` advances `HEAP_TOP(p)`/`HEAP_LIMIT(p)`
     (`erl_vm.h:148-156`), and `enif_environment_t` embeds `Process* proc; Eterm* hp; Eterm* hp_end;`
     (`global.h:57-72`). `erl_nif.c` even reaches into the heap and the off-heap MSO list by hand to save/restore
     state around a NIF call (`erl_nif.c:5440-5500`).
   - **`Process` and `ErtsSchedulerRegisters` have pinned offsets** (`offsetof < 128`) chosen for the JIT's short
     displacements and machine-register pinning (`erl_process.h:626-685,1043+`; `beam_jit_main.cpp:300-309`); the
     `ErtsCodeInfo` size is a comment-enforced contract ("if you change the size… you also have to update the code in
     ops.tab", `code_ix.h:106-125`).
   - **Thread-progress** (`erl_thr_progress.c`) is the shared invariant underpinning code loading, tracing, PTab
     deletion, delayed dealloc and carrier migration — i.e. shared between the "core" and the "periphery you want to
     keep".

**Consequence.** A "core-only rewrite" is a **link boundary, not an API boundary**: the Rust core inherits the
*entire internal representation as a frozen ABI*. In terms of size the natural "core" (execution engine ~57 kLOC,
scheduler/processes ~36 kLOC, BIF dispatch ~32 kLOC, term ops ~18 kLOC, loader ~16 kLOC, GC ~7 kLOC,
thread-progress ~1.5 kLOC) is ≈ **134 kLOC of the 220 kLOC of `beam/*.c`** — it does *not* dodge the compatibility
work; it converts *external* compatibility into *internal layout* compatibility.

**The two clean, already-existing seams:**
1. **The execution-engine seam** — `beam_asm.h` + the frozen register/offset/`ErtsCodeInfo` contract. The engine is
   already a leaf that calls back into the runtime via `beam_jit_*` helpers; the interpreter (`FLAVOR=emu`) and the
   JIT (`FLAVOR=jit`) already coexist behind it. **Replacing the engine only (Ruby-YJIT-style) is the one bounded,
   precedent-backed version of "rewrite the core".**
2. **The OS/driver seam** — `sys/` owns only `main()` and the OS callbacks (`beam/sys.h`); NIFs/drivers load via
   `dlopen` against the `-rdynamic` symbol table. Clean and language-agnostic (this is exactly what `Tyn` exploits).

**Prior art** (all keep the *object model* in the host language): **Ruby YJIT** (Rust engine inside a C runtime,
operating on CRuby's existing `VALUE`/frames through a frozen interface — the closest analogue), **MMTk** (Rust GC
bound to foreign runtimes via a `VMBinding` trait — proves the C-ABI seam is possible but also shows the interface
is large and assumes a global heap), **Wasmtime/Cranelift** (Rust exposing a C API), **CPython 3.13** (engine swap
under a stable C object model). **No prior art** exists for replacing BEAM's *term/GC/scheduler* representation in
Rust while keeping the C `erl_nif.c`/ETS/distribution linkage at this fidelity.

**Verdict.** The idea is half-right: the *execution engine* is genuinely swappable behind a clean seam; the rest of
the "core" is not a module. Do the engine first (also the highest-ROI change, §F); treat the broader core as a
link-boundary port that **freezes the internal headers as an ABI** and proceeds module-by-module — and scope it as
the *last* stage, not the entry point.

---

## B. Data structures — the abstract value is fixed; the cache-friendly interior is open

Part I's blanket "changing the representation of lists is a trap" **overshot and misread the intent**. The intent is
**not** to replace cons lists with vectors. At the interface a list **stays a cons list** — `[H|T]` O(1) and
shareable, value semantics, persistent map — but the **interior layout** beneath that interface is a legitimate,
cache-friendly design space. That is not foreign to OTP: it already ships **two physical layouts behind one abstract
value**, switching on size, invisibly — small map = packed `flatmap_t` (sorted keys tuple + parallel values) →
large map = HAMT at `MAP_SMALL_MAP_LIMIT` (`erl_map.h:57-97`); small binary = on-heap `ErlHeapBits` (≤64 B) →
off-heap ref-counted `BinRef` (`erl_bits.h:135-172`). The question is whether a **third hybrid — for lists — is worth
it**. Answer: *promising, and gated on compiler/FBIP work.*

### B.1 The promising direction — cache-friendly hybrid lists
Keep the cons-list *interface*; make the interior cache-friendly the way small maps already are:
- **Chunked / unrolled cons list** — a node holds up to K elements (K ≈ 16–32) inline + a next pointer, turning
  build / walk / `map` / `filter` / `foldl` / `++` into tight array loops with one allocation per K (the classic
  unrolled-list technique; cf. "colony" lists).
- **Opportunistic chunking, cons fallback** — emit chunks only where the producer/consumer pattern allows and fall
  back to plain 2-word cells otherwise (the same "packed-small / structured-large" switch the map already does); a
  list *could* be either representation, chosen by construction pattern rather than by type.
- **RRB-trees / finger trees as internal sequences** — for `array`-/`queue`-like loads needing O(log n)
  concat/split, a relaxed-radix or finger-tree interior beats a cons chain (they back Clojure/Scala persistent
  vectors and Haskell `Data.Sequence`).

**Payoff, and its precondition.** Fresh lists are *already* bump-allocated contiguously today (`HAllocX`,
`erl_vm.h:148-156`), so raw allocation locality is not the win. The win is deleting **per-element cons decode +
allocation + dispatch** in the sequential/bulk patterns that dominate real code (`lists:map` & friends are one
indirect fun call per element today — §E). That win is realized only **together with FBIP/reuse and a chunk-aware
compiler**; otherwise every boundary to ordinary cons code pays a de-chunking cost. Bulk producer/consumer workloads
(big-list building, `Enum`/`lists` pipelines, iolists/charlists, traversals) are the natural fit; `[H|T]`
destructuring *at an unknown offset* is the awkward case — hence the plain-cell fallback.

### B.2 What is fixed — design tensions to resolve, not prohibitions
The fixed interface is small: a list term is a **single word** (`ERL_NIF_TERM == Eterm`); `[H|T]` is O(1) and
shareable; `enif_get_list_cell`/`enif_make_list_cell` keep their "head+tail" / "build a pair" semantics
(`erl_nif.c:1971,2183`); the **ETF/distribution** wire format is a canonical `LIST_EXT` cons chain (`external.c`);
and the language algebra holds (`is_list([])` true but `is_nonempty_list([])` false; `hd`/`tl`/`length` `badarg`
cases; `++`/`=` semantics). Three things need a *decision* at implementation time:
1. **The partial-chunk "tail view"** — `[H|T]` on a chunk must yield a first-class `T`. The JIT reads CAR/CDR as
   **raw fixed-offset loads** (`Mem(ptr,-1)` / `Mem(ptr,-1+8)`), which cannot express "next element, same chunk".
   Several resolutions exist (keep a real cons cell for the live head window; a chunk+offset descriptor node;
   normalise at the true boundaries); **which is best is left to the implementation**.
2. **Introspection surfaces** — `erts_debug:same/2` (raw pointer identity), `erts_debug:size/1` / `flat_size/1`,
   `process_info(_,memory)` report physical word counts (2 per cons). Chunking changes these; they are debug APIs
   (not the language), so they can be defined or relaxed — a product decision.
3. **Level** — these bite hardest for a **Level-1** VM that must run existing `.beam`/NIFs (the inline offset loads
   especially); a **Level-2** VM owns every list opcode and has correspondingly more freedom.

### B.3 The independent, low-risk win
**ETS `ordered_set`** is an **AVL tree, one separately-allocated node per key** (`erl_db_tree.c:25`,
`erl_db_tree.h:28-32`) — a pointer chase per level. A K-ary B-tree (K ≈ 16–32) / cache-oblivious layout collapses
the height and packs keys per cache line; only *total key order* is observable, so it is fully observation-preserving
and self-contained (~2 kLOC), deliverable inside the current VM. A rewrite can go further — a B+-tree with in-node
range scans, on top of the §D shared-object/COW redesign.

**Unchanged representation facts:** a cons cell is a header-less 2-word `[CAR,CDR]` tagged `TAG_PRIMARY_LIST`
(`erl_term.h:71,219-244`); `++`/`lists:append` copies its LHS and `member`/`keyfind` are O(n) C BIFs
(`erl_bif_lists.c`) — chunking + FBIP improve their *constant factor* and in-place-ness, not their asymptotic class.

**Bottom line:** the abstract list/map stay exactly as they are; the interior is a legitimate, cache-friendly design
space with in-tree precedents — chunked lists with opportunistic chunking + FBIP, and RRB/finger-tree interiors for
sequence loads — but its payoff is gated on the §E compiler work, and it is not on its own the dominant bottleneck
(that is per-element dispatch + allocation — §C, §E). ETS `ordered_set` → K-ary B-tree is the one independent,
low-risk, self-contained win.

---

## C. The Enum-vs-Stream paradox

**(Elixir is not in this repo; the mechanism follows from the shared BEAM object model and public Elixir docs.)**

Three costs, separated:

- **(a) The intermediate list eager `Enum` materializes.** One cons cell per element; on BEAM that is a bump of
  `htop` by 2 words + two stores. List BIFs charge ~60 cells per reduction, so an N-cell intermediate costs ≈
  **N/60 reductions**. `Enum.map` is implemented as accumulate + reverse (tail-recursive, JIT-friendly), so it is
  two cheap passes.
- **(b) What the lazy `Stream` adds.** A k-stage lazy pipeline threads a control tuple (`{:cont, acc}`/… ) through
  a **higher-order reducer invoked once per element per stage** — each such step is an Erlang-level **function
  call = 1 reduction**. So the lazy path pays ≈ **k·N reductions** of pure dispatch to *avoid* ≈ **N/60 reductions**
  of copying. For moderate N, **the overhead of avoiding the copy is tens of times larger than the copy.**
- **(c) The `%Stream{}` object.** Honest correction: the struct is built once *per stage* (O(#stages)), not per
  element; it only matters at small N. The real per-element lazy cost is the composed-reducer machinery and control
  tuples — largely folded into (b).

**Why lazy eventually wins:** for **large N** the eager pipeline's *peak live set* is a full intermediate list per
stage, which drives repeated copying GCs (the collector copies live young data), whereas lazy keeps the working set
≈ O(final acc + one element). The verdict flips to lazy at large N — consistent with public benchmarks and the
standard "Enum by default, Stream for large/expensive/infinite data" guidance.

**Consequence for the plan:** Stream is not "buggy" and does not need a heavier/leaner struct. The win is to
**give eager pipelines Stream's "no intermediate" without Stream's per-element closure protocol**, i.e. a *fusion*
pass — plus *reuse* so an intermediate that is genuinely needed is built in place. Both are compiler features (§E).

The same reasoning explains why BEAM's *hand-written C BIFs* (`lists:member`, `++`) are fast while *higher-order
Erlang loops* are comparatively slow: the C loop has zero per-element call overhead, whereas `lists:map`/`filter`/
`foldl` pay one indirect fun call + `FCALLS--` per element. And the compiler currently **does not fuse** these
chains (below).

---

## D. Memory management in Rust — GC, process heaps, messaging and ETS

### The current collector, and why its two properties are load-bearing
Per-process **generational semi-space Cheney copying** (`internal_doc/GarbageCollection.md:25,29`; `erl_gc.c`,
`copy.c`): stack and heap share one block and grow toward each other; GC fires when they meet; minor GC promotes
below the high-watermark; fullsweep copies everything; `fullsweep_after` default 65535 (`erl_init.c:843`); heap size
from a curated table (`erl_gc.c:265-291`); large immutable objects live off-heap as ref-counted binaries with a
per-process "virtual binary heap" that can force an early GC; the MSO list is swept as a prefix from the first
old-heap item (`erl_gc.c:3079-3085`); large GCs escape to dirty schedulers above 128 Ki words (`erl_gc.h:26`).

Two properties make it hard to beat:
1. **No global stop-the-world, ever.** GCs are per-process, on the owning scheduler (or a dirty one); collection of
   one process never pauses another.
2. **No generational write barrier** — a deliberate, *free* consequence of term immutability:
   *"no term on the old heap may refer to a term on the young heap … this comes naturally for Erlang because the
   terms are immutable"* (`GarbageCollection.md:108`). A repo-wide grep finds **no GC barrier** anywhere (only CPU
   memory barriers). The mutator pays **zero** per-store overhead.

### Constraints any replacement must honor
NIF/off-heap **pointer stability** (ref-counted binaries/resources stay put across GC; writability flags clear on
observation); `erts_debug:same/2` pointer identity; contiguous tuples; per-process `erts_alloc`'d heaps + the
**delayed-dealloc** message box + **thread-progress** barrier for cross-scheduler frees; and the **JIT root-set
contract** — the root set is `x_reg_array[0..Live)` + the whole Erlang stack, with **no frame liveness maps** (`Live`
is a compile-time instruction operand). Notably the AArch64 JIT keeps `x0..x5` in CPU registers and **must flush
them before any GC/call** (`arm/beam_asm.hpp:518-534`) — the one existing "hidden root" hazard.

### Fit matrix (Rust GC options)
| Option | Moving? | Per-process/multi-heap? | Parallel & low-latency? | Verdict for BEAM |
|---|---|---|---|---|
| **Port generational copying (unsafe Rust)** | ✔ | ✔ native | ✔ *across* processes | **Best baseline** |
| **GenImmix / StickyImmix** (copy nursery + Immix mature) | ✔ | ✔ | ✔ | Best *modernization* candidate (mature gen), measurement-gated |
| **Immix** alone | ✔ | ✔ | ✔ (STW per process) | Worse for small heaps (per-line metadata) |
| **MMTk** | ✔ | ✘ **one global heap**; multi-instance "not fully supported" (`docs.rs/mmtk`) | ✔ | **Poor fit** |
| **Boehm-Demers-Weiser** | ✘ **never moves** | ~ single heap | ~ STW | **Incompatible** (NIF pointer/representation needs a moving heap) |
| **`gc-arena`/`rust-gc`/`shifgrethor`/Servo** | mixed | ✘ single heap; dictate the API | ~ | Not for the term heap |
| **Regions (Verona/Cyclone)** | ✘ | ✔ | ✔ | Needs **static** ownership typing — impossible for untyped terms |
| **Sapphire / compiler-relocation** | ✔ | ~ | ~ | Research (needs whole-program compiler cooperation) |
| **LXR / RCImmix** (MMTk plan, PLDI 2022) | ✔ (brief STW) | ✘ shared-heap bias | ✔ parallel, **no concurrent evacuation** | Research; not production-proven in an actor VM |
| **RC-with-reuse (Perceus/Koka/Lean 4)** | n/a | ✔ possible | ~ (decrement cascades) | Research track (see below) |

### The crux: keep per-process heaps — yes
For a message-passing actor VM the per-process heap is **architecturally correct**: isolation ⇒ no global STW / no
cross-process barrier / natural parallelism; immutability ⇒ free generational invariant; **O(1) process death**
(one block freed); millions of tiny (~233-word) heaps; and no static types to enforce regions. **Pony/ORCA** is the
cross-language analogue — it gets exactly BEAM's "per-actor GC, no STW, no barriers" by co-designing the type
system with per-actor RC; a Rust BEAM *without* such a type system falls back to copying (status quo) or barriers.
**Go**, by contrast, pays a write barrier on every store to obtain sub-ms STW on one shared, non-moving,
non-generationally-collected heap.

Within the per-process heap: **generational copying wins for the young generation** (the dominant BEAM pattern)
and uniquely costs nothing at mutator time. **GenImmix** (copy nursery + Immix mature) is the credible *mature-gen*
modernization; crucially, a STW mark-region mature space **still needs no generational write barrier** under the
immutability invariant, so it does not dismantle the barrier-free design — but it must be **measurement-gated**
(Immix's per-line metadata and free-list allocation can hurt small heaps and mutator locality).

**RC-with-reuse** is attractive *on paper* for a pure, immutable, **acyclic** term language — no cycle collector is
even needed for term data — but BEAM's live object graph includes ETS, ports, monitors and NIF resources, it needs
compiler-inserted RC (whole-compiler change), and it introduces decrement cascades (latency spikes).

**Prior art:** `enigma` (Rust BEAM) used a **per-process arena** GC and listed "explore Immix" as an *unreached*
roadmap goal; it stalled for *community* reasons, not a GC failure. That the one serious Rust-BEAM attempt chose
per-process GC (matching this analysis) is a useful confirmation.

**Recommendation.** **Port-and-modernize:** re-implement the per-process generational copying collector in (unsafe)
Rust, **preserve the barrier-free generational invariant**, keep the off-heap ref-counted large-object space, and
treat **GenImmix as a measurement-gated mature-generation option**. Reject MMTk and Boehm; keep LXR and
RC-with-reuse as research tracks. The port is *bounded* (the algorithms are ~4.2 kLOC of `erl_gc.c` + `copy.c`), and
the external contract (GC-ABI entry points, root set = x-array + stack, MSO ordering, sizing table, dirty-scheduler
escape) is small and enumerable.

### The copy boundary — messaging, ETS, and what may be shared
A local (same-node) send of a non-immediate term **deep-copies the whole term graph** into the receiver's
heap/queue: `!` → `do_send` (`bif.c:2458`) → `erts_send_message` (`erl_message.c:698`) → `copy_struct_x`
(`copy.c:647`), charged as reductions proportional to size; remote sends are ETF-encoded (`external.c`). The copy is
**fundamental, not an oversight**: with per-process heaps and independent moving GCs **no cross-heap pointer may
exist** (a peer GC would move or dangle it), process death frees one block in **O(1)**, and ordinary terms carry
**no refcount and no generational write barrier** — a deliberate barrier-free consequence of immutability
(`internal_doc/GarbageCollection.md:108`). What crosses **cheaply** is the "small heap-resident handle + payload
outside every process heap" pattern: immediates; **refc large binaries (>64 B)** — only the ~2-word descriptor and a
refcount bump, bytes shared (`copy.c:812-848`, `erl_binary.h:63`); **literals** (`global.h:948`); **`persistent_term`**
(returned by reference, `erl_bif_persistent.c:359-372`); external pid/port/ref node-table entries — whereas funs
(incl. env), lists, maps, tuples, bignums, floats and small (≤64 B) binaries are copied. Exactly three conditions
make an object shareable — (a) **pointer-free payload** (no tagged `Eterm` into any process heap), (b) **immutability
while shared** (no write barrier to track writes), (c) **refcount-or-permanent lifetime** — and only those three
kinds meet them. **ETS is already a shared store off-heap** (table-owned `DB_TERM`, `DbTerm` `erl_db_util.h:68`,
charged to no process), yet **every put is a full deep copy in and every read is a full deep copy out**: `lookup`
copies the whole object, `lookup_element` **only the requested element** (`erl_db_util.c:3763`), `first`/`next`
**only the key**, match/select each matched result (`ERTS_PAM_COPY_RESULT`, `erl_db_util.c:2638`) — only **nested refc
binaries** are shared by reference on both put and read. **Precedent:** **Pony/ORCA** (OOPSLA'17) gets per-actor
heaps *plus* zero-copy messaging only via a co-designed **data-race-free type system** (`iso`/`val`/`tag`);
**JVM/.NET** expose one shared heap to all threads (no copies, but a global/STW collector) — the opposite trade-off.

**What is actually sent, and what it costs.** The dominant traffic is a small atom-tagged 2–3 element envelope —
`{'$gen_call',{Pid,Ref},Req}` (~8 words), `{Ref,Reply}` (~5), `{'$gen_cast',Req}` (~4),
`{system,{Pid,Tag},Req}` (~8), `{notify,Event}` (~4), `{'EXIT',…}`, `{'DOWN',…}` (~7)
(`lib/stdlib/src/{gen.erl,gen_server.erl,gen_statem.erl,erpc.erl}`). Only immediates are genuinely free
(`IS_CONST`, `copy.c:677`; a self-send skips the copy, `erl_message.c:824-833`) — and a *local* `reference()` is
**boxed** (3 words, `ERTS_REF_THING_SIZE`, `erl_term.h:873-877,990`), so the ubiquitous gen-server request id forces
a real `copy_struct` pass on every call and every reply, not an immediate-only fast path. A non-immediate message
pays **two full passes** — `size_object_litopt` (`erl_message.c:839`) then `copy_struct_litopt`→`copy_struct_x`
(`copy.c:647`, call site `erl_message.c:860`) — with **no word-threshold size specialization**: a 5-word
`{Ref,Reply}` is walked and copied exactly like a 5 000-word term.

### Cheaper messaging and ETS — what is reachable
**General zero-copy messaging is not reachable** without a fundamental redesign — a new shared term class understood
by every consumer (BIFs, guards/PAM, ETF, GC, introspection) *plus* **write barriers** for shared mutation, the cost
immutability lets BEAM avoid. Within that frame the work sorts into three tiers.

**(a) Opportunistic messaging wins (in the current VM).** A bounded **single-pass small copy**: today even a 5-word
envelope is walked twice (size at `erl_message.c:839`, copy at `copy.c:647`/`erl_message.c:860`) with no
size specialization, so a `size_object_bounded(msg, N)` probe that reserves exactly and falls back to re-size/copy on
overflow removes the first pass for the dominant shape. **Message-queue auto-tuning**: the on-/off-heap choice is
**static** — default `SPO_ON_HEAP_MSGQ` (`erl_process.c:132`), changed only at spawn, by `+hmqd`
(`erl_init.c:1580-1585`) or `process_flag` (`erl_process.c:12305-12315`) — although it is purely a GC trade-off (an
on-heap queue joins the **rootset on every GC**, `erl_gc.c:2709-2777`; an off-heap one does not), so a queue-length
threshold can flip it through the existing thread-progress transition `erts_change_message_queue_management`
(`erl_message.c:1022`). **Fragment ownership transfer** (rewrite-only): `enif_send` already builds a term in a
private fragment chain and hands it over **with no copy** (`erl_nif.c:815-841,860-994`), and receive already adopts
fragments (`save_heap_frag_eterm` `erl_proc_sig_queue.c:3924`; `erts_link_mbuf_to_proc` `erl_message.c:563`); letting
`!` do the same means building into a detached fragment — a representation/semantics change. All three must respect
the accounting invariant: copy cost is charged only above 64 words (`ERTS_MSG_COPY_WORDS_PER_REDUCTION`,
`erl_message.h:33-35`; `erl_message.c:881-886`) and the copy is synchronous and non-yielding, so any sharing must keep
charging on **logical term size** or scheduling de-fairs.

**(b) ETS in a rewrite — the shared-object redesign.** ETS is already an off-heap shared store, but per-object
`DbTerm` allocations (`erl_db_util.h:68`) make every whole-object read a copy (`db_copy_object_from_ets`
`erl_db_util.h:425`; `erl_db_hash.c:3614`; `erl_db_tree.c:1038`; PAM `ERTS_PAM_COPY_RESULT`, `erl_db_util.c:2639`),
and even a fine-locked table takes a per-slot rlock per read (`erl_db_hash.c:404-413`). The redesign **stores each
object as an immutable refcounted shared off-heap object** (its top level is already a `tpl[]` tuple array — a
literal-style direct-pointer candidate), returns it **by reference** from lookup/match/select, **COW**s every
insert/update/delete, and reclaims by refcount and/or RCU/epoch (`ErtsThrPrgrLaterOp`, already used for CATree node
free, `erl_db_catree.h:50,59`) — giving **lock-free reads** and deleting `{compressed}` re-decode
(`erl_db_util.c:3726`) and per-match temp allocation (`erl_db_hash.c:1903`). Independent **in-place** ETS wins need no
enabler: keep the already-narrow reads narrow (`lookup_element` element-only, `erl_db_util.c:3763`; `first`/`next`
key-only), trim match-result copies, chunk `select`, keep same-size `update_element` in place — plus an `ordered_set`
**B+-tree** (§B.3), **opt-in secondary indexes** for match/select (only the key is indexed today,
`erl_db_hash.c:3194,3250-3276`; everything else full-scans, `:1835-1951`) and **open addressing**
(SwissTable/CHAMP) for the non-fine-locked hash tables.

**(c) The enabler: a refcounted shared off-heap compound term.** The *representation* is precedented: literals are
off-heap compound term graphs laid out **identically** to heap terms, with a `TAG_LITERAL_PTR` bit the accessors mask
off (`erts_set_literal_tag` `erl_term.c:36-69`; `erl_term.h:210-216,232-238,247`), so `is_tuple`/`element`/`CAR`/
`is_map` read them at **zero extra indirection or branch** (`erl_term.h:530,1311-1317`) — the access path is **not**
the obstacle; per-reference **lifetime** is (literals are permanent, `erl_gc.c:2379`, `copy.c:1118-1535`). **Route A**
wraps the block in a heap-resident handle, reusing the `BinRef`/MSO lifetime model verbatim (`erl_bits.h:60,135`;
sweep `sweep_off_heap` `erl_gc.c:2995-3110`) at the cost of a wrapper term ⇒ real accessor indirection plus a new case
at every boxed-subtag switch site (size/copy, GC, eq/hash, ETF, NIF, PAM, printing); **Route B**, a literal-style
direct pointer, costs nothing on access but needs new GC-traced lifetime (a per-process registration marked during the
heap scan, or a heap-tracked handle) and a *detectable* block space (`TAG_LITERAL_PTR` exists only on ARCH_64 without
reserved memory/DEBUG). Both require the block to stay **immutable for life** and **free of pointers into process
heaps**, to own its inner refc binaries, and to keep `=:=`/`==`/term **order**/hashing **structural**
(`utils.c:1125,1169,1658`) — otherwise sharing flips `=:=` true → false; sharing is observable
(`erts_debug:same/2`, `size/1`, `flat_size/1`, `process_info(memory)`), a product decision rather than a free win.
**The concrete mechanism is left to implementation** — directions and trade-offs, not commitments.

---

## E. Compiler optimizations — feasibility, universality, compile-time cost

All three candidate optimizations slot into the existing **BEAM-SSA** layer and are therefore **language-agnostic**.

### Where they slot in (grounded)
The pipeline is *data* in `lib/compiler/src/compile.erl`: `standard_passes/0` (`:1875`) → `abstr_passes/1`
(`:1895`) → `core_passes/1` (the Core block, `:1925`) → `kernel_passes/0` (the SSA block, `:1954`) →
`asm_passes/0` (`:2001`) → `binary_passes/0` (`:2033`). A new pass goes either in the `{delay,[…]}` of
`core_passes/1` (Core level) or of `kernel_passes/0` (SSA level, **before `beam_ssa_pre_codegen` `:1992`**, which
does register allocation). Inside `beam_ssa_opt.erl`, sub-passes are data in six phases (`:111-394`), and
`passes_1/2` (`:384-394`) **auto-derives a `no_<name>` kill-switch for every pass** — a ready-made guardrail.

### (1) Loop optimizations — confirmed absent; cheapest, safest
Search confirms the tree has **no** induction-variable analysis, unrolling, LICM or strength reduction; the only
"loop-aware" code is `beam_ssa:is_loop_header/1` (`beam_ssa.erl:394-408`), which is **receive-specific** and used
as a *barrier* for other optimizations. But the substrate exists: integer interval types
(`#t_integer{elements=…}`, `beam_types.erl:145`; `beam_bounds.erl:36-48`), **loop-counter range inference already
computed** as analysis (`beam_ssa_type.erl:1109-1230`), and CFG primitives (`rpo`/`dominators`/`successors`). Wins
are **narrow** (numeric/array/`seq`-style loops); cost is low and mostly reuses existing analysis.

### (2) Operation fusion / deforestation — absent; medium payoff, high semantic risk
There is **no fusion pass** (zero matches for `deforest|fusion|fuse`). The closest is `sys_core_fold_lists.erl` — an
**opt-in, default-OFF** inliner of exactly 10 `lists` HOFs (`sys_core_fold.erl:116,827-835`) that substitutes a
**verbatim copy of the library loop body, including its output `cons`/`++`** — so `map(F2, map(F1, L))` becomes two
independent loops and **both intermediates are still materialized**. Comprehension specialization only eliminates a
*singleton* generator (`sys_core_fold.erl:2225-2305`). The block-local `beam_ssa_opt` merges
(`merge_updates_bs:525-597`, `update_tuple_opt:1081-1142`, `combine_put_maps:1392-1426`, literals-only) are update
peepholes, not fusion. A real fusion pass would collapse `map∘filter∘foldl`, comprehension chains and Elixir
`Enum` chains into one loop. **Hard part = semantics**: error position/stacktrace line, strict left-to-right
evaluation order, side-effect ordering, and the domain of accepted inputs (improper lists) must be preserved
*exactly* — `beam_ssa_throw` (`:1820`) exists precisely because these are observable.

### (3) FBIP / reuse analysis — one conservative instance exists; highest payoff, highest cost
`beam_ssa_destructive_update.erl` (OTP-27) adds the `inplace` hint for `update_record` and writable-binary append,
but it **consumes** uniqueness from `beam_ssa_alias.erl` (a whole-module Kotzmann–Mössenböck-style escape/alias
analysis) and **excludes lists and maps** — `put_list`/`put_map` deliberately **inhibit** reuse
(`beam_ssa_opt.erl:3902-3934`). Extending it to maps/lists/binaries (Koka/Perceus-style reuse) is the principled
generalization and attacks the *actual* bottleneck (allocation/GC pressure), but it is the **heaviest compile-time
pass** and the riskiest (value semantics + sharing must be preserved; the analysis is already seeded `aliased` at
exported-function boundaries, `beam_ssa_alias.erl:330-348`).

### Universality — confirmed
Every mainstream BEAM front-end lowers through the *same* back-end: **Elixir** runs `elixir_erl` → Erlang abstract
forms → `:compile` → `v3_core` → `beam_ssa_opt` (public sources in the appendix; the `.core` entry is
`compile.erl:1690-1691`). **LFE** and other Core-Erlang front-ends enter via `core_passes/1`. So a fusion/loop/FBIP
pass benefits Erlang's `lists` and Elixir's `Enum` **identically**. Exceptions: Elixir's own `Kernel`/protocol
inlining happens *before* BEAM (not shared), and a front-end emitting **BEAM assembly** (`.S`) bypasses the SSA
passes entirely.

### Compile-time cost — manageable with guardrails
Ranked: **loop opts (low, SSA-local, reuses existing ranges) < fusion (moderate: loop recognition + fixpoint) ≪
FBIP (high: whole-module escape/alias fixpoint)**. Existing guardrails: `no_*` flags, the auto `no_<pass>` toggles,
`{delay}` blocks + in-source `-compile` directives, `rNN` feature gating, and the **clone-and-recompile CT matrix**
(`_no_opt`/`_no_ssa_opt`/`_no_copt`/… in `lib/compiler/test/Makefile`). **Gaps to close:** there is **no per-pass
budget/watchdog** (only hard caps `MAX_REPETITIONS=16`, `RETURN_LIMIT`, `MAX_TYPE_DEPTH`) — a runaway pass hangs
until CI timeout; and the **self-hosting bootstrap** hazard (the compiler is built by an earlier compiler; precedent
for cost-consciousness: `core_parse` is exempted from `+inline` "because inlining is slow"). Add: per-pass budget +
opt-in default-off flags + a compiler-bootstrap timing benchmark in CI.

**Recommendation:** pursue in order **loop opts → fusion → FBIP**, all behind opt-in flags and the differential
test matrix.

---

## F. The JIT — register allocation, backends, tiering

### The current design and the exact ceiling
BeamAsm is a **load-time, per-instruction template translator** that does *"hardly any cross-instruction
optimizations"* and keeps the `x`/`y` register arrays **in memory** (`internal_doc/BeamAsm.md:30-34,69-72`). Process
state is pinned to a handful of machine registers (x86: `rbx`=`x_reg_array`, `r12`=`code_ix`, `r13`=`c_p`,
`r14`=`FCALLS`, `r15`=`HTOP`; `x86/beam_asm.hpp:104-141`) — all callee-saved, so no spilling across C calls.
BEAM registers are reached as `[rbx + i*8]` / stack slots, so a typical op costs **2–3 memory operands** (e.g.
`i_plus` = load+ALU+store). The `RegisterCache` (`beam_jit_register_cache.hpp`) only elides *reloads* within a
call-free straight-line region — it **is not a register allocator** (`consolidate()` nukes it at every label,
`:71-77`).

### The opportunity — and why it is low-risk
A **block/extended-basic-block register allocator** that caches live `x`/`f` values into the caller-saved scratch
pool (x86 `ARG1-6`/`TMP1-2`/`RET`) and **spills the live set to the canonical memory image at every safepoint**
(call/GC/trap/yield) removes the memory operands the cache can't. **The AArch64 backend already proves the exact
pattern**: it keeps `x0..x5`/`f0..f7` in registers (`arm/beam_asm.hpp:98-193`) and spills them via
`emit_enter_runtime` before any C call/GC (`:518-534`). Because the GC root set stays `x_reg_array[0..Live)` +
stack, **the collector, tracing, `perf` and code-loading need ZERO changes** (option A). The alternative —
per-safepoint root maps (Cranelift-style stack maps) — is a wide, risky change to the *runtime*; avoid it first.

Two BEAM-specific limits bound the win: the canonical memory array must remain the backing store/spill area (1024
x-registers vs ~8–14 allocatable GPRs), and **calls are barriers** (caller-saved scratch is clobbered by every
BIF/NIF/fragment). So the win concentrates in **compute-heavy straight-line kernels**, not call/GC/port-dominated
code. And **deopt is unusually cheap** here: BEAM's observable state *is* the memory image, so a deopt is "jump to
the baseline at the same BEAM label" — no V8-style frame translation. That is a genuine architectural advantage.

### Backends
| Backend | Compile latency | Quality | Reg. alloc | Security | Portability | Effort |
|---|---|---|---|---|---|---|
| asmjit (incumbent) | ns | baseline "template" | none | vendored small API | x86-64/aarch64 | — |
| **Copy-and-patch stencils** | **µs** | baseline-class | light/greedy | AOT, tiny attack surface | build-time stencils | alt baseline |
| **Cranelift** | ~10× faster than LLVM | ~2% behind TurboFan, ~14% behind LLVM | **real (regalloc2)** | Rust, fuzz/regalloc-verified | x86-64/aarch64/riscv64/s390x | **best hot tier** |
| LLVM ORC | wrong latency (historical failure) | best | best | large surface | everywhere | reject |
| hand-written Rust encoder | lowest | yours | yours | yours | per-arch | duplicates ~40 kLOC |

**Recommendation:** **baseline = keep the BeamAsm-style encoder + block RA** (or its Rust port); **hot tier =
Cranelift** (the only option combining real RA, order-of-magnitude-faster-than-LLVM compile, memory safety, Apache-2.0,
and the target-arch spread). Copy-and-patch is the right *baseline* only if the goal is to shrink per-arch codegen
maintenance (a from-scratch VM); LLVM stays rejected.

### A pluggable IR seam
Insert a **typed, block-structured IR** between the loader's existing `.tab` transformations and the backends:
`BEAM loader (ops.tab transforms) → typed block IR → {interpreter, BeamAsm encoder, copy-and-patch, Cranelift}`.
The IR must carry: value types (the loader's `BeamTypeId`), **safepoints** (live set + canonical-memory mapping),
failure/branch semantics (a `j`-operand of 0 means *raise*), catch/exception info, the fixed entry prologue for
breakpoints/NIF trampolines, line/`beam_ranges`/coverage tables, and all relocation patch points. Coexistence and
**differential testing** are already solved in BEAM (the `jit`/`emu` flavors and the flavor-differential suites);
extend them to the new backends.

### Tiering (fits the "low latency" compass)
Tier 0 interpreter (fallback/diagnostics) → **Tier 1 BeamAsm + block RA** (all the *certain* value, no speculation)
→ **Tier 2 optional Cranelift hot tier** (per-function recompile of already-loaded code, runtime type narrowing +
deopt, compiled **off the critical path** with a bounded budget, under thread-progress + W^X). Tier 2 needs a cheap
**per-function call counter** (BEAM has none today; reductions are per-scheduler). Keep Tier 2 strictly optional and
out of the boot path.

### Realistic numbers
- **Baseline + block RA:** ~0–5% on call/BIF/GC/port-dominated code; **~5–15% typical**; **~20–30% on
  arithmetic/tuple/list/binary-syntax tight loops**.
- **Adding the Cranelift hot tier:** a further **~5–15% aggregate** on typical workloads, **~20–30%** on
  compute-heavy kernels the tier applies to — *not* a whole-program multiplier.
- Reference: BeamAsm already reports *"no difference to up to 4×"* vs the interpreter (`BeamAsm.md:525-528`),
  ~25% average in practice; the JIT loads code ~20% slower than the interpreter.

---

## G. The interpreter — how much is left

### The current engine is already highly tuned
BEAM's interpreter is **direct-threaded** — the "code" is an array of raw label addresses (`beam_emu.c:70-71`;
`erl_vm.h:286`), with a **per-instruction indirect branch** and **software dispatch pipelining** (`next_pf` loads
the next word before the indexed jump; `beam_makeops:1798-1803`). `x`/`y` live in a memory array (`reg =
esdp->registers->x_reg_array.d`) while hot control state (`c_p`,`E`,`HTOP`,`I`,`FCALLS`) is pinned to C locals via
**asm constraints** (`beam_emu.c:247-265`), and a **`check_emu_registers` target fails the build-time check if any of
them spills to `%rsp`** (`Makefile.in:1489-1491`). Even label alignment was tried and **rejected at ~5% slowdown**.

Crucially, **the loader already does much of what CPython's PEP 659 "quickening" does, at load time**: peephole
transformation rules (`beam_load.c:452-493`), **specific-op selection by operand tags / `x(0)`-vs-`x(N)`**
(`beam_load.c:496-650`), first-class **superinstructions** (`:=` fragments; `deflate`/`select_val`→`jump_tab`,
etc.), and it writes label addresses into the threaded stream (`emu_load.c:907`).

### What is missing, and the two levers
- **Run-time adaptive specialization / inline caches: absent** (only the breakpoint machinery patches code). The
  highest-value sites are the **lookups** (`get_map_elements`/`put_map`, ETS, `apply`/`call_ext`, BIF dispatch) —
  not arithmetic (an `Eterm`'s type is a 2-bit/4-bit tag test, already cheap).
- **The inferred value-type layer (`beam_types`/`BeamTypeId`) is JIT-only** — compiled under `#ifdef BEAMASM`
  (`beam_file.c:388`; the `r`-operand rewrite at `beam_load.c:543`). The interpreter never sees it.
- **Levers:** (1) **un-gate `beam_types` for the interpreter** so the loader can pick type-specific
  specific-instructions — pure load-time, no W^X/deopt, directly closing the biggest interpreter↔JIT gap;
  (2) **polymorphic inline caches** for the lookup/call sites, stored in a **separate writable side-array indexed by
  code offset** (keeps code RX, avoids colliding with W^X/breakpoint trampolines/purge; deopt = a plain store).

### Modern techniques judged
- **CPython tail-call interpreter (`musttail`)** — the headline gain was inflated by an unrelated `_PyStackRef`
  change; the robust effect is **~1–5% mean**. Its main motive (per-opcode indirect branches) **BEAM already
  satisfies** via direct threading. Rust has **no stable guaranteed TCO** (`become` is nightly; stabilization
  targeted ~2027). ⇒ **secondary, feature-flagged; do not architect around it.**
- **Lua / JVM template interpreter / V8 Ignition** — BEAM already embodies the register machine + computed goto +
  static fusion; the transferable lesson is *generated* stubs (HotSpot `TemplateTable`/V8 `InterpreterAssembler`)
  so one instruction description lowers to both interpreter and JIT, removing today's 3-way hand-maintained mirror.

### Ranked interpreter work
1. **Preserve the loader + `.tab` + `beam_makeops` pipeline verbatim** — it is data, and it is the interpreter's
   dominant asset.
2. **Un-gate `beam_types` for the interpreter.** (low cost; low–moderate gain on type/map/record code)
3. **PICs for lookup/call sites** in a side-array. (medium cost; moderate gain on lookup-heavy code)
4. **Direct-threaded port with a Rust equivalent of `check_emu_registers`** — this is the *floor*: without a
   register-resident state model and a spill guard, a Rust port is strictly slower.
5. Tail-call form (feature-flagged); generated stubs (longer-term).

**Anti-recommendations:** do **not** self-modify the code stream (W^X/breakpoints/purge risk); do **not** budget for
PEP-659-sized (25–50%) interpreter wins (CPython started from a switch over dict/attr-lookup-heavy untagged
bytecode BEAM does not have).

---

## H. The staged plan (development cost accepted)

The user's stance — *"I don't care about the development burden, as long as the path is straightforward and
achievable"* — is exactly what the evidence supports: **almost every gain is achievable behind an existing seam on
the existing codebase**, and each stage is independently valuable. "Straightforward" here means *the decomposition
and the seams are known*, not that the work is small.

**Stage 0 — Foundations (no behavior change).**
Freeze and version the internal ABI if a Rust core is the goal (`erl_term.h`/`erl_process.h`/`erl_alloc.h`/`global.h`
→ a versioned contract with `static_assert`s already partly present); build the **differential-test compatibility
harness** (run modules under interpreter / BeamAsm / new backends and compare) — the safety net for everything else.

**Stage 1 — Execution engine (highest ROI, well-bounded, no compatibility risk).**
Block-level register allocation in BeamAsm (or a Rust port of the engine); **optional Cranelift hot tier** with a
bounded, off-critical-path compile budget and cheap deopt; interpreter: **un-gate `beam_types`** + lookup **PICs**.
*This is the one "rewrite the core" piece that is already an interface.*

**Stage 2 — Compiler (universal across Erlang/Elixir/LFE).**
Loop opts → fusion → FBIP, on the existing BEAM-SSA IR, behind opt-in flags and the differential matrix, with a
per-pass budget and a bootstrap benchmark. *Delivers the user's "fuse map/update chains" and the Enum-vs-Stream fix
without touching the VM.*

**Stage 3 — Data structures (interface fixed, interior open).**
Two tiers: **independent, low-risk wins now** — ETS `ordered_set` AVL → K-ary B-tree, flatmap search / CHAMP node
layout, cons-cell allocation batching (these land in `erts`, not the compiler); and the **cache-friendly list
interior** — chunked/unrolled lists with opportunistic chunking + plain-cons fallback, RRB/finger-tree interiors
for sequence loads — which is a **compiler+VM co-design** that rides on Stage 2's FBIP and so is sequenced *with*,
not before, it. Target bulk producer/consumer patterns; keep the cons interface bit-identical.
Also in `erts`: the **incremental, semantics-preserving ETS wins** from §D (element/key reads, match-copy trimming,
`ordered_set` node layout, opt-in secondary indexes, open addressing); the **opportunistic messaging wins** from §D
(bounded single-pass small copy, message-queue auto-tuning replacing the static `+hmqd` choice, and — as a rewrite
item — fragment ownership transfer); and, as the single **enabler** shared by message sharing and the ETS
shared-object/COW redesign, a **refcounted immutable off-heap compound term** — budgeted as a research item because
its cost is lifetime/refcount plus `copy.c`/`erl_gc.c` integration, not the access path.

**Stage 4 — GC modernization.**
Port the per-process generational copying collector to unsafe Rust (preserving the barrier-free invariant and the
off-heap ref-counted space); evaluate **GenImmix** for the mature generation, gated on per-workload measurement
against the copying baseline.

**Stage 5 — Rust core (only if Stages 1–4 show the *core* is the remaining bottleneck).**
Port the core **module-by-module behind the C ABI**, sharing the frozen headers, execution engine first, then loader,
scheduler, GC, term ops. Never as a big-bang; keep the `FLAVOR=emu` fallback.

**Stage 6 — Level-2 clean-room VM (research track, explicit exit criterion).**
A new ISA + fusion + FBIP as a whole-toolchain change. Its best ideas (fusion, loop opts, FBIP) already ship in
Stage 2 on the existing IR, so Stage 6's *marginal* contribution is the ISA/toolchain redesign — justified only if
Stage 2 hits a wall the current ISA cannot cross.

**What this buys over Part I's "targeted only" framing:** with the development appetite accepted, the ambition is
fine — but the *ordering* still matters, and the evidence says the value is front-loaded (Stage 1) and the "rewrite"
is back-loaded (Stage 5). That is the straightforward, achievable path.

---

## I. Revised risk / payoff register

| Workstream | Effort | Perf payoff | Robustness | Risk | When |
|---|---|---|---|---|---|
| **Stage 1a** BeamAsm block register allocation | Medium–high | Medium: 5–15% typical, 20–30% compute | Medium | Medium (safepoint correctness; ARM pattern proven) | **First** |
| **Stage 1b** Optional Cranelift hot tier + deopt | High | Medium (additional 5–15%; 20–30% kernels) | Medium | Medium–high (compile thread, promotion, deopt) | After 1a |
| **Stage 1c** Interpreter: un-gate `beam_types` + lookup PICs | Low–medium | Low–moderate | Low | Low (side-array caches) | Parallel with 1a |
| **Stage 2** Compiler loop opts | Low–medium | Medium (narrow: numeric/array) | Low | Medium (semantics) | **High ROI** |
| **Stage 2** Compiler fusion/deforestation | Medium | Medium (functional pipelines; Enum↔Stream) | Low | High (error position/eval order) | After loop opts |
| **Stage 2** Compiler FBIP / reuse (maps/lists/binaries) | High | **High** (allocation/GC pressure) | Low | **Highest** (value semantics + GC barrier) | Last of Stage 2 |
| **Stage 3** ETS `ordered_set` → K-ary B-tree | Medium | High (ordered-set lookups) | Low | Low | Any time |
| **Stage 3** Map flatmap/CHAMP, cons batching | Low–medium | Low–medium | Low | Low | Any time |
| **Stage 3** Cache-friendly list interior (chunked/unrolled + opportunistic chunking; RRB/finger-tree) | High (VM+compiler co-design) | Medium–high (bulk list/`Enum` workloads) | Low | Medium–high (partial-chunk tail view; introspection APIs) | With Stage 2 FBIP |
| **Stage 3** ETS in-place wins (element/key reads, match-copy trimming, `ordered_set` node layout, opt-in secondary indexes, open addressing) | Low–medium | Low–medium | Low | Low | Any time |
| **Stage 3** Opportunistic messaging (bounded single-pass small copy; message-queue auto-tune) | Low | Low–medium | Low | Low (must keep logical-size reduction accounting) | Any time |
| **Stage 3** Shared refcounted immutable off-heap term — **enabler** for message sharing *and* the ETS shared-object redesign | High | Medium | Low | High (refcount/lifetime + `copy.c`/`erl_gc.c` integration; every boxed-subtag consumer; `=:=`/order/hash must stay structural) | Research; after Stage 3 |
| **Stage 3** Zero-copy ETS reads (immutable shared objects + COW writes + lock-free reads) | High | Medium (read-heavy tables: no whole-object copy, no read lock) | Low | High | Rewrite only; gated on the enabler above |
| **Stage 3** Fragment ownership transfer for `!` (zero-copy send of a detached fragment) | High | Medium–high (large/structured messages) | Low | High (representation/semantics change) | Rewrite only |
| **Stage 4** Port per-process copying GC to Rust | High | Neutral (preserves behavior) | Medium | High (correctness) | After Stage 1 |
| **Stage 4** GenImmix mature generation | High | Uncertain (measure) | Medium | High | Measurement-gated |
| **Stage 5** Rust core behind frozen C ABI (link boundary) | Very high (134 kLOC core) | Low–modest | Medium (periphery) | Very high | Only if 1–4 insufficient |
| **Stage 6** Level-2 new ISA + toolchain | Very high + ecosystem fork | Moderate | Medium | Very high | Research only |

---

## Appendix — references

**In-tree grounding (this repository).** `erts/emulator/beam/{erl_term.h,erl_vm.h,erl_process.h,erl_map.c,erl_map.h,erl_bif_lists.c,binary.c,erl_bits.c,erl_iolist.c,bif.c,beam_debug.c,external.c,erl_gc.c,copy.c,erl_gc.h,erl_alloc.h,global.h}`;
`erts/emulator/beam/emu/{beam_emu.c,ops.tab,instrs.tab,macros.tab,select_instrs.tab,arith_instrs.tab,msg_instrs.tab,bif_instrs.tab}`;
`erts/emulator/beam/jit/{beam_asm.h,asm_load.c,beam_jit_register_cache.hpp,beam_jit_common.{hpp,cpp},x86/*,arm/*}`;
`erts/emulator/beam/{erl_db_tree.c,erl_db_tree.h,erl_db_util.c,erl_nif.c}`; `erts/emulator/utils/beam_makeops`;
`erts/emulator/beam/{erl_message.c,erl_message.h,copy.c,erl_proc_sig_queue.c,erl_db_hash.c,erl_db_util.{c,h},erl_db_catree.h,erl_binary.c,erl_bits.h,erl_term.c,utils.c,erl_bif_persistent.c}` (messaging / ETS / shared off-heap, shared-term enabler);
`erts/emulator/internal_doc/{GarbageCollection.md,BeamAsm.md,ThreadProgress.md,CodeLoading.md,beam_makeops.md}`;
`erts/emulator/Makefile.in`; `lib/compiler/src/{compile.erl,beam_ssa_opt.erl,beam_ssa_alias.erl,beam_ssa_destructive_update.erl,sys_core_fold.erl,sys_core_fold_lists.erl,beam_types.erl,beam_bounds.erl,beam_ssa.erl,beam_ssa_type.erl}`;
`lib/compiler/test/Makefile`; `lib/stdlib/src/{lists.erl,proplists.erl,string.erl,unicode.erl}`; `lib/stdlib/src/{gen.erl,gen_server.erl,gen_statem.erl,gen_event.erl,erpc.erl}` (common message shapes).

**External.**
- Erlang/OTP blog — *The Road to the JIT* (HiPE removal; abandoned LLVM tracing JITs): https://www.erlang.org/blog/the-road-to-the-jit
- BeamAsm documentation: https://www.erlang.org/doc/apps/erts/beamasm.html ; Erlang GC: https://www.erlang.org/doc/apps/erts/garbagecollection.html
- Ruby YJIT (Rust engine in a C runtime): https://railsatscale.com ; MMTk crate (multi-instance unsupported): https://docs.rs/mmtk/latest/mmtk ; Jikes RVM/MMTk one-Plan model: https://www.jikesrvm.org/UserGuide/MMTk/
- Ruby 3.4 modular GC (MMTk MarkSweep only; Immix/LXR future): https://railsatscale.com/2025-01-08-new-for-ruby-3-4-modular-garbage-collectors-and-mmtk
- Immix (PLDI 2008): https://www.steveblackburn.org/pubs/papers/immix-pldi-2008.pdf ; LXR (PLDI 2022): https://www.steveblackburn.org/pubs/papers/lxr-pldi-2022.pdf ; RCImmix (OOPSLA 2013): https://www.cs.utexas.edu/users/mckinley/papers/rcix-oopsla-2013.pdf
- Perceus (Koka/Lean 4 RC-with-reuse): https://www.microsoft.com/en-us/research/wp-content/uploads/2020/11/perceus-tr-v1.pdf
- Pony/ORCA (per-actor GC, no STW/barriers): https://janvitek.org/pubs/oopsla17a.pdf ; Go GC guide: https://go.dev/doc/gc-guide
- Process memory model: Pony **ORCA** (OOPSLA 2017 — per-actor GC + type-system co-design ⇒ zero-copy messaging): https://www.ponylang.io/media/papers/orca_gc_and_type_system_co-design_for_actor_languages.pdf ; Erlang binary handling (refc binaries / large-binary sharing): https://www.erlang.org/doc/system/binaryhandling.html ; Erlang `persistent_term` (off-heap term, returned by reference): https://www.erlang.org/doc/apps/erts/persistent_term.html
- Off-heap read-mostly reclamation: **read-copy update** (McKenney & Slingwine): https://www.kernel.org/doc/html/latest/RCU/whatisRCU.html ; epoch-based reclamation (Crossbeam `epoch`): https://docs.rs/crossbeam-epoch
- Cache-friendly lookup geometry: **SwissTable** (Abseil flat hash map/set): https://abseil.io/docs/cpp/guides/container ; **CHAMP** (Steindorfer & Vinju, OOPSLA 2015): https://doi.org/10.1145/2814270.2814312 ; B+-tree (Comer, "The Ubiquitous B-Tree"): https://doi.org/10.1145/356770.356776
- Cranelift (fast, secure, regalloc2): https://cranelift.dev ; regalloc2: https://cfallin.org/blog/2022/06/09/cranelift-regalloc2 ; Copy-and-Patch (OOPSLA 2021) + CPython 3.13 (PEP 744); V8 Maglev: https://v8.dev/blog/maglev
- PEP 659 (specializing adaptive interpreter): https://peps.python.org/pep-0659 ; CPython tail-call interpreter — careful re-analysis (~1–5%): https://blog.nelhage.com/post/cpython-tail-call ; Rust tail calls (`become`, nightly; ~2027 target): https://trifectatech.org/blog/tail-calls-project-goal
- Elixir lowers via Erlang abstract forms → Core → SSA (Dashbit, "How to debug Elixir/Erlang compiler performance"): https://dashbit.co/blog/how-to-debug-elixir-erlang-compiler-performance ; The BEAM Book §2.4: https://stenmans.org/theBeamBook/
- Cache-friendly persistent structures: unrolled/"colony" linked lists; **RRB-Trees** (Bagwell & Rompf, EPFL — the basis of Clojure/Scala persistent vectors); Haskell `Data.Sequence` (finger trees); Clojure `PersistentArrayMap` (≤8, linear scan) → `PersistentHashMap` (32-way) — the small-size switch that mirrors BEAM's flatmap → HAMT.
- Prior art: `archseer/enigma` (Rust BEAM; per-process arena GC; stalled): https://github.com/archseer/enigma ; *Tyn* (Rust microkernel hosting unmodified ERTS).
