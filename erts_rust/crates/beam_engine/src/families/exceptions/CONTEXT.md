# `beam_engine/src/families/exceptions` — the exception / raise instruction family

## Intent
Owns the in-VM **exception** instruction surface and the machine-side *handler*
opcodes (the pushing/popping/folding half of the catch-frame protocol; the
raise/unwind half lives in `src/machine/catch.rs`, which this family calls through
`Machine::{throw, throw_error, throw_with_trace}` — the one uniform raise entry
point).

Read `../../CONTEXT.md` first: it holds the crate-level FROZEN API surface, the
execution model (frames, dispatch, raise + catch-frame semantics) and the handoffs.
The per-opcode semantics and the full M1 gap list are in `../../INSTRUCTION_SURFACE.md`.

## API Surface
- `exec(machine, module, labels, instr, func, pc, ctx) -> Result<Option<Step>>` —
  the family's entry point, called by the `families::dispatch` router (which routes
  by the instruction's own `Instr` variant, so ownership is disjoint). `Ok(Some(step))`
  = executed, `Ok(None)` = not this family's variant, `Err` = raised. Every owned
  opcode is position-independent, so `func`/`pc` are unused (kept because the
  signature is the frozen family-router contract).
- Opcodes owned: `try`/`catch` (push a catch frame), `try_case`/`try_end`/`catch_end`
  (the handler opcodes; each consumes exactly one frame), `badmatch` ⇒ `{badmatch,V}`,
  `case_end` ⇒ `{case_clause,V}`, `try_case_end` ⇒ `{try_clause,V}`, `badrecord` ⇒
  `{badrecord,V}`, `if_end` ⇒ the bare atom `if_clause`, `raise Trace Value` (re-raise
  with `Machine::pending_class`), `raw_raise` (the x(0)=class / x(1)=value /
  x(2)=trace protocol, incl. the "unknown class" `badarg` fall-through and the
  trace **term** it supplies through `Machine::raise_supplied`), and
  `build_stacktrace`.
  `raw_raise` follows the reserve → read → write ordering: `x(0)` is classified
  **first** (the class is an atom — an immediate, so it can never be relocated) and
  `x(2)`'s `NIL` tag test decides whether a cooking allocation happens at all, but
  the **reason** (`x(1)`) and the **supplied trace** (`x(2)`) are read only **after**
  the cooking reservation, from the machine's post-collection registers — a
  collecting host relocates `x` in place, so a pre-reservation word would address the
  region it vacated. Nothing allocates between those reads and
  `Machine::raise_supplied`, so both words stay valid to the raise.
- `raise.rs` (private helper module): `raise_error_tuple` / `raise_error_atom` (reason
  construction through the **rooted** builders, so a moving collector can run under the
  reserve), `install_class_atom` (never substitutes an atom the module cannot name) and
  `classify_class` (`raw_raise`'s class protocol).
- Every raise **tag/class** atom in this family resolves through
  `machine::namespace_atom` — the host's live namespace (`ctx.atoms(module)`) first, the
  running module's load-time snapshot second; a name **neither** source knows is the typed
  `unsupported`, never a substituted atom index.
- `callee.rs`: the synthesized **callee head item** of an `undef` raise —
  `{CalleeModule, CalleeFunction, ArgList, []}` (`is_unresolved_undef`,
  `import_callee_atoms`, `import_callee_head_item`, `callee_head_item`). Real OTP names an
  unresolvable call target **only** through the cooked trace's innermost item (with the
  *argument list*), and `logger_server:call_h/4` matches on exactly that shape, but the
  engine's raise snapshot is `(context, func, pc)` metadata and can only emit an arity — so
  the failing call sites (`call_ext`'s import table + `x(0..arity)`; the `apply` register
  shape) materialize the one item the snapshot cannot. The item is built in **one** rooted
  reservation (`2*argc + 5` words) through `terms::reserve_slack` + the `Sink`, reading the
  callee's **real** arguments from `x(0..argc)` *after* it (reserve → read → write). It is
  built only for a bare `undef`, only with a live catch frame, and only when the arguments
  are readable — otherwise `Ok(None)` and the unmodified raise. `Location` is the empty
  list (the callee does not exist, so there is no source position).
- `stacktrace.rs`: `symbolic_stacktrace(machine, module, ctx, head_operand, head)` — the one
  cooked-trace builder. It **first** re-reads `head_operand` and returns an **already-cooked** list
  verbatim (`is_list` — the trace term a landing published in `x(2)`, C's `is_raised_exc` arm,
  `erts/emulator/beam/beam_common.c:1219-1223`); otherwise it renders a supplied symbolic-frame latch;
  otherwise it cooks
  the machine's raise-time snapshot into the symbolic `[{M,F,A,Location}]` list from
  `Machine::raise_snapshot()` (the **pre-unwind** `(context, func, pc)` chain every
  `Machine::throw_with_trace` rebuilds at raise time: the raising frame first, then every
  live caller innermost-first). **Each frame resolves in its OWN module context**: context
  `0` against the `module` argument, but only when `Machine::current_context()` is also `0`
  (otherwise the handler runs in a callee context and the argument is the *callee's* module,
  so the frame is skipped); a non-zero context `id` through `Machine::module_context(id)`. A
  frame whose context cannot be resolved is **skipped** (best-effort metadata, like the
  uncaught path's `Machine::raise_stacktrace`); a frame whose module IS resolved but whose
  function index is out of range is a typed `unsupported`. An **empty** snapshot — or one
  whose every frame is skipped — cooks to the **empty list**, the honest answer, unless a
  head item is present. One rooted reservation for the whole node graph
  (`terms::slack`/`terms::sink`); the resolved `Arc<Module>` handles stay alive in a local
  vector across it, because each item shape borrows its module's `Line` chunk. The failed clause's
  argument list rides in the **first** frame's `A` slot through `ItemShape::args` (the list **term**) and
  `ItemShape::args_len` (its element count): the latch's read half
  (`Machine::live_raise_function_clause_args`) is consulted *before* the reservation for the length alone
  (move-invariant), the latch is consumed *after* it (`Machine::take_raise_function_clause_args` — that
  reservation may have moved the list, which `Machine::with_roots` re-seats), and the list's elements are
  carved **inside** it by `write_item`. When `head` is set the cooking exception carries a
  synthesized **callee head item** as its raw trace
  (`callee.rs` above): the reservation is sized **`+2`** for one extra cons cell, and the
  head word is re-read **after** the reservation through `head_operand` (the very register
  `build_stacktrace` read the raw trace from) and consed in front of the carved frames, so a
  head-bearing raise cooks to the **one-element** list `[Item]` even over an empty frame
  chain. **Supplied traces win:** an already-cooked list in `head_operand` is returned verbatim
  (`is_list`), and when the live raise arrived carrying explicit non-empty symbolic frames
  (`Machine::take_raise_supplied_trace`), `symbolic_stacktrace` consumes that latch next and renders THOSE
  frames (see the section below) — the snapshot is not consulted and any head item is superseded.

## A supplied trace is preserved verbatim; a derived one comes from the raise snapshot
A raise that **arrives carrying** its own trace keeps it: no frame of the re-raise site is ever prepended,
exactly as the oracle's `build_stacktrace` returns a supplied list verbatim.
Two flavours exist: the **explicit symbolic frames** are metadata (an `Option<StackTrace>`, no heap term),
the **trace term** is a real `Eterm` carried by the raise and is one of the machine's **loose heap terms**:
`Machine::with_roots` parks `pending_supplied_term`/`raise_supplied_term` at the Erlang-stack tail for the
duration of every root hand-off and re-seats them (`src/machine/CONTEXT.md`), so a collection between arming
a latch and publishing it relocates the cooked list instead of stranding it.
C's discriminators are `raw_raise`'s `EXF_SAVETRACE` clear and the `c_p->ftrace` it stores
(`erts/emulator/beam/beam_common.c:1148-1179`), `save_stacktrace`'s `EXF_SAVETRACE` guard (`:505-507`) and
`build_stacktrace`'s verbatim return (`:1219-1223`).
- **Explicit symbolic frames** — `erlang:raise/3` whose third argument is a non-empty trace list, modelled
  as [`beam_common::Raise::stacktrace`]. `Machine::raise_error` (`../../interpreter/bif.rs`) arms the
  one-shot `pending_supplied_trace` latch, `Machine::throw_with_trace` promotes it to the live
  `raise_supplied_trace`, and `stacktrace::symbolic_stacktrace` — the one consumer — renders THOSE frames
  instead of the `raise_snapshot` heuristic (the uncaught path renders them through
  `Machine::with_uncaught_frames`). Each frame renders as `{Module,Function,Arity,Location}` with an
  **arity** in the `A` slot (the model carries no argument list) and the `[{file,File},{line,N}]` location
  the `SourceLocation` describes; every name is resolved from the running module's atom table (never
  substituted — a name it cannot intern is a typed `unsupported`).
- **The trace TERM** — `Instr::RawRaise`, the shape `sys_core_fold` folds `catch C:R:S ->
  erlang:raise(C,R,S)` into (`lib/compiler/src/sys_core_fold.erl:2441-2465`). The raise supplies its trace
  as the **cooked** `[{M,F,A,Location}]` **list**: the arm reads `x(2)` (a landed exception published the
  list of the exception being handled there), and when `x(2)` is `NIL` it cooks the machine's preserved
  `raise_snapshot` chain **once**, rooted, at the `raw_raise` site. Either way the list is handed to
  `Machine::raise_supplied`, which arms the one-shot `pending_supplied_term` latch; `throw_with_trace`
  promotes it to the live `raise_supplied_term` and then **skips** the snapshot rebuild — so the preserved
  chain of the raise being re-raised survives — and the term is consumed exactly once: a **landed** raise
  publishes it as the handler's `x(2)` (C's register convention, so the next `raw_raise`/`build_stacktrace`
  reads it straight back), an **uncaught** one renders the preserved chain through
  `Machine::with_uncaught_frames` (`Machine::snapshot_stacktrace`).
  The reason the raise carries is `x(1)`, a heap container whenever the exception is structured, so it is
  read **after** the cooking reservation too, together with a `NIL` `x(2)`: the reservation runs the host's
  collecting door and relocates every register in place, and only the post-collection words survive.
  `symbolic_stacktrace` therefore begins with an **already-cooked passthrough**: a trace register holding a
  cons cell (`is_list`) is returned verbatim (C's `is_raised_exc` arm, `:1219-1223`), because a raw-trace
  word is never a cons — it is `NIL` or the one synthesized callee head item (`callee.rs`).
Both latch pairs are cleared by `Machine::reset`, together with the callee-head and failed-clause-argument
pairs (`../../machine/registers.rs`).
A raise that supplied neither flavour gets the derived trace: `throw_with_trace` rebuilds `raise_snapshot`
(the **pre-unwind** `(context, func, pc)` chain, the raising frame first then every live caller
innermost-first) and the cooking renders it.
The carve itself (`carve_supplied_frames`) is **also public**: the crate root exposes
`beam_engine::encode_supplied_stacktrace` (free function) and its `Machine` twin
(`../../supplied_stacktrace.rs`), both thin wrappers over this one carve, so a host that
runs no compiled handler — `beam_rt`'s `-eval` tree-walking route — renders the same
frames through the same code.
The out-of-crate half of the explicit-frames flavour — `beam_bifs`'s `erlang:raise/3` decoding its third
argument into `Raise::stacktrace` — belongs to `beam_bifs`; this family consumes the frames the `Raise`
already carries.

## The landing order in `Machine::raise_error` (`../../interpreter/bif.rs`)
A raise carrying a **live reason term** (a compound reason) goes through `throw_with_trace` **before** the
`catch_frame_depth() == 0` test, so it lands the same way whether or not a handler is live: caught, the term
lands in `x(1)`; uncaught, `throw_with_trace`'s no-catch-frame arm returns a frameless `Raise` that keeps the
term in its `live_reason` slot, and `Machine::with_uncaught_frames` attaches the symbolic frames at the run
boundary like every other uncaught raise.
An error with no live reason term and no live catch frame propagates unchanged (`Err(error)`) and never
reaches `throw_with_trace`, so no latch is armed for it.

## The raise's raw trace: `NIL`, or a synthesized callee head item
C's raw trace is an **opaque object of saved code pointers** (`c_p->ftrace`) with no engine
equivalent, and the oracle's own cooked `S` for `try ... catch C:R:S -> S end` additionally
holds host `init:*` frames that live *outside* this engine's run. A snapshot-cooked trace is
therefore a strictly-**partial** list that would still differ from the oracle's, so landing it
would trade this documented omission for a different, silently-partial value. So on the
compiled path the raw trace word is `NIL` — **except** for the `undef` raised by an
unresolvable `call_ext*`/`apply` target, whose failing call site publishes its synthesized
`{M,F,ArgList,[]}` item as the raw trace (the one item the snapshot cannot express). In both
cases the compiled handler moves the raw trace from `x(2)` into `x(0)`, which is the operand
`build_stacktrace` reads (`Instr::BuildStacktrace`), and the head item is recognised through
the `raise_head_item`/`pending_callee_head` latch pair rather than by inspecting the word.
C's `add_stacktrace` stays omitted and `catch_end` semantics are unchanged. The word therefore carries no
engine object for a derived raise, which is why a re-raised exception's trace is preserved through the
`raise_snapshot` chain plus the `pending_supplied_term`/`raise_supplied_term` latch pair, and only a raise
that supplied a trace (or landed a re-raised one) has a **list** there — the shape
`symbolic_stacktrace`'s `is_list` passthrough recognises. A landing publishes exactly that: a landed raise
with no supplied term leaves `x(2)` the raise's own raw-trace word (`NIL`, or the head item), never a
partially-cooked list.

## The two one-shot latches that carry the head item
The head item is **not** a machine-held heap term: it travels as the raise's own raw-trace
word, so it sits inside the frozen root set (published in `x(2)`, moved to `x(0)` by the
compiled handler) and is relocated in place by every collection.
- `pending_callee_head` is armed by `Machine::raise_bif_error_with_head` →
  `throw_with_trace_headed` **immediately before** the `throw_with_trace` that actually
  lands, never at the call site (a raise with no live catch frame propagates as a bare `Err`
  and never reaches `throw_with_trace`, so the head is dropped and the latch stays clear).
- `Machine::throw_with_trace`'s **first** act is
  `raise_head_item = std::mem::take(&mut pending_callee_head)`, so `raise_head_item` always
  describes the raise that landed. `Instr::BuildStacktrace` consumes it once via
  `Machine::take_raise_head_item()`; a handler clause that ignores `S` never reaches
  `build_stacktrace`, so the latch is still set when the *next* raise lands and is overwritten
  there — it can never leak into another raise. `Machine::reset` clears both.

## Known Issues
**A supplied EMPTY trace is indistinguishable from "no trace supplied".**
`beam_common::Raise` carries no supplied-trace flag, so `erlang:raise(error, boom, [])` reaches the engine as
the same value as a raise that carried no trace: `Machine::raise_error`'s arming condition is
`!raise.stacktrace.is_empty()` (`../../interpreter/bif.rs`), which a supplied `[]` cannot satisfy, and the
cooked `S` is the derived `raise_snapshot` chain where the oracle keeps `[]`.
C's discriminator is `is_raised_exc` (`erts/emulator/beam/beam_common.c:1119-1126`); `raise_3` stores `NIL`
behind the negative "already cooked" header, so the empty list survives as a *supplied* trace.
The gap is **owned by the sibling `beam_common` node**: the fix is a `supplied_trace: bool` on
`beam_common::Raise`, set by `with_stacktrace`, plus two engine hunks — the arming condition above must test
that flag instead of the list's length, and `Machine::with_uncaught_frames` must keep its derived frames off
when it is set.
The engine-side plumbing is already in place: the `pending_supplied_trace`/`raise_supplied_trace` pair, and
supplied-empty support in `carve_supplied_frames`.
Pinned as a known divergent shape by
`tests/raise_with_trace.rs::an_empty_supplied_trace_is_byte_identical_to_no_supplied_trace` and by the crate
`CONTEXT.md` §Known Issues entry.

## Constraints
- Errors are `beam_common::{Result, BeamError}` only. An unsupported case is a typed
  `error::unsupported`; **never** a panic, a substituted atom, or a wrong result.
- Term construction/inspection is only `beam_term` + the crate's `terms` builders;
  the **rooted** twins (`*_rooted`) are what the instruction bodies here call.
- `try_case`/`catch_end` require the C landing convention `x(0) = THE_NON_VALUE`,
  `x(1) = reason`, `x(2) = trace`, `x(3) = class atom`; the handler opcode that runs
  next pops the frame — a raise *reads* the innermost frame but never pops it.
- **Cross-module:** a raise landing in a catch frame stamped with another module
  context is reported as `Step::Enter { context, func, pc }` (not `Step::Goto`) so the
  driver switches module context and re-binds the literal area. The family relies on
  `Machine::throw` (its `throw_with_trace` entry) for that decision; the symbolic frames
  of an *uncaught* raise are attached separately by the `Machine::with_uncaught_frames`
  funnel at the `step_instruction`/`drive` boundary. It re-derives neither.

## Test Strategy
Unit tests are in-module, split by responsibility under `tests/` (the module tree is
declared in `tests.rs`, which also holds the shared scaffolding: the `Fixture` module
builder, `no_labels`, `exec_one` and `assert_reason_tuple`; several submodules alias
`crate::terms::test_support as terms` for the ergonomic test-only builders).
- `handlers.rs` — the `try`/`catch` frame push, the handler opcodes, the `catch_end`
  fold (throw class, error/exit `{'EXIT',Reason}`, the missing-`EXIT`-atom typed error).
- `callee.rs` — the synthesized callee head item from a hand-built `Module` + a `BifHook`:
  the built item's shape/fields, the `Ok(None)` gates (not a bare `undef`, no live catch
  frame, unreadable argument register) and the latch hygiene (`pending_callee_head` /
  `raise_head_item` armed only by a landing `throw_with_trace`, cleared by `reset`).
- `oracle.rs` — the same mechanism end to end on a **real `erlc`-compiled `.beam`**
  (`p3`, loaded through `beam_loader::load_file`) and cross-checked against the **system
  oracle**: a direct `call_ext` to a missing function of an existing module, both compiled
  `apply/3` routes (the `call_ext erlang:apply/3` encoding and the BEAM `apply`
  instruction), a missing module, the latch-hygiene pair (an `undef` whose handler ignores
  `S`, then an unrelated raise that uses `S`), a head-less raise, the compiler's
  `move {x,2} -> {x,0}` + `build_stacktrace` handler shape (pinned on the loaded IR *and*
  on `erlc -S` text), and **tier equivalence** of the cooked head item on
  `Tier::Interpreter` vs `Tier::BaselineJit`. Every expectation is measurement-derived and
  each test **skips loudly** when `erlc`/`erl` are absent (the discipline of
  [`crate::machine::deadline_tests`]). The engine's cooked list is asserted to be a
  **non-empty prefix** of the oracle's (host `erl_eval`/`init` callers lie outside the run).
- `raising.rs` — the raising opcodes' reason expansion (caught and uncaught), the
  `raise` re-raise opcode, `raw_raise`'s class protocol and `badarg` fall-through, and the trace-term
  supply: `x(2)` holding a cooked list is passed through verbatim, a `NIL` `x(2)` cooks the preserved
  `raise_snapshot` chain once, and a landed raise publishes that term as the next handler's `x(2)`.
  These unit cases drive `exec` directly with a non-collecting host and an immediate reason, so they
  cannot observe a move; the crate-level `tests/raise_rooting.rs`
  (`a_raw_raise_reason_is_re_read_after_the_cooking_reservation`) is the moving-collector gate for the
  reserve → read → write ordering of `raw_raise`'s reason and supplied trace.
- `stacktrace.rs` — `build_stacktrace`: the `NIL` raw trace (`NIL` out), the
  `is_list` passthrough of an already-cooked supplied trace,
  raise-snapshot ordering (a frame the raise *unwound* is still present, proving the
  snapshot is cooked rather than the live stack), a snapshot frame resolved in its own
  module context (with a context-`0` frame skipped from a non-zero handler context),
  the source-location item, and the typed errors.
- `encoder.rs` — the **public supplied-stacktrace encoder**
  (`crate::encode_supplied_stacktrace` and its `Machine` twin): the cooked
  `[{M,F,A,Location}]` layout with and without a location (against a hand-built
  expectation and against the system `erl` oracle's `catch erlang:raise/3` output,
  skipping loudly when `erl` is absent), that the free function and the `Machine`
  method agree, that both produce **exactly** the private `carve_supplied_frames`
  term, and the unresolvable-atom typed `unsupported` error.
- `end_to_end.rs` — whole synthetic sequences.
- Crate-level companion: `tests/stacktrace_frames.rs` pins the cooked `S` of `erlc`-compiled whole modules
  on **both** tiers against the live `erl` oracle, including the `folded/0` (`[folded/0 @24]`) + `refold/0`
  (`[g/1 @17, refold/0 @26]`) rows that exercise this family's `raw_raise` supplied-trace rule and
  `withcall/0` + `inhandler2/0` (`[inhandler2/0 @28]`) that exercise the consumed continuation slot
  (`src/machine/CONTEXT.md`).
- `cross_module.rs` — **cross-module containment**: one shared `beam_term::AtomTable`
  (`Universe`) so exception *class* atoms compare equal across modules, modules entered
  through `MinimalContext::with_external_resolver` (the `resolve_external_call` seam),
  and expectations taken from the system oracle (`erl`, OTP 29 / erts-17.1). Covers the
  `Apply` seam, `try ... of ... catch` (normal path, contained raise, the *uncaught*
  failing `of` clause), nested catches with an inner re-raise landing in an outer handler
  in another module context, a `throw` crossing two module boundaries (A → B → C), and
  compiler-generated raises (`badmatch`/`case_end`/`try_case_end`) contained across a
  module boundary. Each test asserts the exact landed value, the final
  `frame_depth`/`catch_frame_depth`/`current_context`, and the `RunOutcome`.
  `Universe::assert_sealed` fails loudly if a fixture `intern`s a name outside the shared
  table (that would give one name two different atom indices in two modules, which is the
  very divergence the shared table exists to rule out).

Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine`.

## The catch-frame landing convention and the raise entry points (from `INSTRUCTION_SURFACE.md`)
Moved verbatim from `INSTRUCTION_SURFACE.md`; section cross-references inside the text name sections of that file unless stated otherwise.
- **The raise entry points (`src/machine/catch.rs`).** `Machine::throw`/`throw_error` are the uniform raise/unwind entries the families call; `Machine::throw_with_trace` is the same entry carrying the caller-built `trace` term (C's `x(2)`; `NIL` at M1). `throw_with_trace` also rebuilds the machine's **raise-time frame snapshot** (`Machine::raise_snapshot`) at its head -- the pre-unwind `(context, func, pc)` chain, the **raising** frame first then every live caller innermost-first -- *before* the catch-frame branch and `unwind_to_frame`, so it is refreshed on **both** the caught and the uncaught path (this is the engine's replacement for C's saved raw `StackTrace`, `c_p->ftrace`). The symbolic `StackTrace` a raise carries out when it leaves the machine **uncaught** (`Raise::stacktrace`) is attached not at the raise site but by the single funnel `Machine::with_uncaught_frames(err, entry_module, func, pc)` as the raise leaves the machine (at the `step_instruction` / `drive` boundary), so **every** raise kind -- a `throw`/`error`/`exit` `raw_raise` builder, a BIF error through `raise_bif_error`, the `func_info` failed-clause raise -- gets the same frames and no site derives them itself. `Machine::raise_stacktrace(&self, entry_module: Option<&Module>, func, pc)` derives those frames from the machine's live call frames -- the raising function first, then each caller, each resolved in **its own** module context -- which is what localizes a bare-atom reason (the `FuncInfo` arm above). The funnel attaches only to a `BeamError::Raise` that carries no frames yet and whose reason is not an engine diagnostic (`error::unsupported`), so a caught raise, an already-framed raise and an engine diagnostic are all returned unchanged. The stacktrace is **metadata only**: the landing registers, the reason and the pending class are untouched, so a caught raise behaves byte-identically with or without it.

## C plumbing reference for the raise/trace model (read-only ground truth, `erts/emulator/beam/`)
Faithful reproduction of these five facts is what makes this family's `raw_raise`/`build_stacktrace`/`catch_end` arms agree with the oracle; the legacy tree is read-only, so they are recorded here rather than there.
- `c_p->ftrace` is always `TUPLE3 {make_big(&s), X, error_info}` (`error.h:222`). `s->header` is a **positive** bignum header for a *raw* trace saved by `save_stacktrace()` (`beam_common.c:893`, `X` = the top frame's args) and a **negative** header for an *already-cooked* trace supplied through `erlang:raise/3` (`bif.c:1654`, `X` = the caller's cooked list).
- The discriminator is `is_raised_exc()` (`beam_common.c:1119-1126`); `build_stacktrace()` short-circuits on it (`:1219-1223`) and returns the supplied list **unchanged, before any frame lookup** — so re-raising a raised exception is idempotent and no path ever prepends a frame to a supplied trace.
- `raise_3` (`bif.c:1549-1696`) clears `EXF_SAVETRACE` from the class (`:1573`, so `handle_error` will not call `save_stacktrace`), stores the caller's list directly (`:1641-1645`) and zeroes `s->pc`/`s->current`/`s->depth`/`s->max_depth` (`:1656-1659`) so nothing can be appended. `must_copy` rises only for `{Fun,Args}`, old-style `{M,F,A}`, or a list longer than `erts_backtrace_depth` (default 8, `error.h:207`) — and even then the normalized copy gains no frame.
- Entry points that only *store*: `i_raise` (`emu/instrs.tab:1236-1245`) and `raw_raise` (`:1253-1263` → `beam_common.c:1148-1179`) both set `c_p->ftrace = x(2)` and take the class from `erts_sanitize_freason()` (`:1089-1097`); `raw_raise` is what the compiler emits for a generated rethrow.
- At a catch `handle_error` sets `reg[2] = c_p->ftrace` (**raw**) and then clears `c_p->ftrace` (`:541-544`, `:566`). `try_case` only moves `x(3)`→`x(0)` and never cooks; cooking happens in the separate `build_stacktrace` instruction the compiler prepends at the first catch clause that uses the trace variable (`genop.tab:557-562`, `v3_core.erl:1515-1532`), and `catch_end` cooks only for class `error` (`add_stacktrace`, `beam_common.c:730-735`). `try_case_end` (`emu/instrs.tab:1229-1234`) is the one case that **drops** a raised trace and saves a fresh one.
- Homonym to avoid: `erts_build_stacktrace()` (`erl_bif_info.c:2409-2462`) walks the **live** stack for `process_info(P,current_stacktrace)` and the GC trace; it is never used for exceptions. Frame items are rendered by `erts_build_mfa_item()` (`utils.c:3429-3457`) as `{M,F,Args,Loc}`, `Loc` = `[{file,_},{line,_}]` or `[]`.

## See Also
- `../../machine/catch.rs` → the catch-frame stack, `throw`/`throw_with_trace`,
  `with_uncaught_frames`/`raise_stacktrace`, `land_exception`, `unwind_to_frame` (the raise
  half of this protocol).
- `../../CONTEXT.md` → crate contracts, the frozen seams, the execution model.
- `../../INSTRUCTION_SURFACE.md` → per-opcode semantics and the complete gap list.
