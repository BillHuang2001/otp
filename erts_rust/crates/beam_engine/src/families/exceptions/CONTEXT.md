# `beam_engine/src/families/exceptions` — the exception / raise instruction family

## Intent
Owns the in-VM **exception** instruction surface and the machine-side *handler*
opcodes (the pushing/popping/folding half of the catch-frame protocol; the
raise/unwind half lives in `src/machine/catch.rs`, which this family calls through
`Machine::{throw, throw_error, throw_with_trace}` — the **fresh**-raise entry point —
and `Machine::throw_propagating` — the **propagating** one, reached only from
`Instr::Raise`; see `## A propagating exception keeps its original trace` below).

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
  `Machine::with_uncaught_frames`).
  Each frame renders as `{Module,Function,Arity,Location}` with an **arity** in the `A` slot (the model
  carries no argument list) and the `[{file,File},{line,N}]` location the `SourceLocation` describes.
  Every name a frame mentions -- its `Module` and `Function` and the location's `file`/`line` keys -- is
  resolved through the **live** atom namespace (`ExecutionContext::atoms`, via `machine::namespace_atom`)
  with the running module's load-time snapshot as the fallback, and is never substituted.
  A supplied trace's names routinely come from modules the re-raising module never mentioned
  (`proc_lib`/`gen_server`/a supervisor re-raising a captured trace), so a snapshot-only lookup refused
  traces the oracle echoes **verbatim** -- `build_stacktrace`'s `is_raised_exc` branch does no atom
  resolution at all (`erts/emulator/beam/beam_common.c:1219-1223`) -- which is what destroyed most
  third-party (Khepri/Cowboy) crash reports.
  Honest residual: a name **neither** the live namespace nor the module snapshot can name is still the
  typed `unsupported`, never a fabricated atom -- an atom index is valid only against the table it was
  read with.
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

## A propagating exception keeps its original trace
`Instr::Raise` (`raise Trace Value`) is the **only** propagation site: the compiler emits it both for the implicit re-raise that finishes a `try … catch` whose clauses did not match and for an `erlang:raise/3` in the caught-clause position that `sys_core_fold` folds (`beam_ssa_codegen.erl:2471`, `beam_core_to_ssa.erl:491`).
Its arm calls `Machine::throw_propagating` — `throw_inner(.., TraceSource::Propagated)` — so the raise **keeps** the preserved `raise_snapshot` (the chain captured at the original raise site) and the four raise latches (the `undef` callee head item, the failed clause's argument list, the supplied symbolic frames, the supplied trace term) instead of re-deriving them from the live frames, which the first handler has already unwound.
Every other raise door (`bif`, `gc_bif`, `call_ext*`, `func_info`, `Instr::RawRaise`, a host's `Raise` error) is `Machine::throw_with_trace`, i.e. `TraceSource::Fresh`, and rebuilds the snapshot there — so only a **new** raise starts a new trace.
C's ground truth is `i_raise` copying `c_p->ftrace` verbatim with `EXF_SAVETRACE` cleared (`erts/emulator/beam/emu/instrs.tab:1236`, `erts_sanitize_freason`), versus `raw_raise`'s stored `ftrace` and `save_stacktrace`'s `EXF_SAVETRACE` guard (`beam_common.c:1148-1179`, `:505-507`).
A one-shot `Machine::raise_propagated` latch records that the in-flight exception arrived by propagation; `Machine::with_uncaught_frames` consumes it and renders the preserved chain through `Machine::snapshot_stacktrace` (the chain-cooking the uncaught supplied-trace arm already uses) instead of `Machine::raise_stacktrace`.
Every `Machine::reset` and every fresh raise clears the latch, so it can never decorate a later, unrelated raise.
Measured rule and its pin (`tests/propagation.rs`, both tiers, oracle-cross-checked): a non-matching clause's implicit re-raise **keeps the raising frame**; `erlang:raise/3` in the caught clause keeps the original chain with no re-raise-site frame prepended; a matching clause raising a **new** error gets a brand-new trace; `throw` keeps the throw site's own frame; a `catch Expr` behaves identically; three nesting levels preserve the chain verbatim (it never grows); and a propagated `undef`/`function_clause` keeps its synthesized head item and argument list.
`erlang:raise(error,R,[])` still answers the derived chain instead of `[]` — the `beam_common::Raise` supplied-trace-flag gap recorded in `../../CONTEXT.md` §Known Issues, untouched by the propagation rule.


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

## The cooked trace is ONE reservation — the carve is word-exact and collection-safe
The `[{M,F,A,Location}]` list handed to the handler is a **single** rooted reservation (`terms::reserve_slack`, `stacktrace.rs:249` for the snapshot/head path, `:329` for supplied frames), carved node-by-node by the [`Sink`] trait; nothing on that path allocates a second time, so no `Slack` cursor or carved `Eterm` is ever live across a collection-capable door.
- **The size identity is exact, not padded.** `ItemShape::words()` (`stacktrace.rs:505-511`) = `5 + 2*args_len + (2*file_chars + 10)` matches consumption word for word: `5` for the `{M,F,A,Loc}` tuple (`Sink::tuple` charges `elems.len() + 1`, `terms/sink.rs:99`), `2*args_len` for the failed clause's argument list (`Sink::list` charges `2*len`, `:107`), `3` + `3` + `4` = `10` for `{file,F}` / `{line,N}` / the outer two-element list (`write_location`, `stacktrace.rs:653-667`), and `2*file_chars` for the file-name char list.
- **The outer list and the head cons are charged separately and exactly:** `total = 2*shapes.len() + Σ words + if head {2}` (`:246`), `sink.list(&items)` carves `2*shapes.len()` (`:277`), and the head cons carves its own `2` (`Sink::cons`, `:117`). The head item is the raise's **already-built** raw-trace word, never carved here, so `+2` is the whole addition.
- **An undersized reservation is a typed error, never an overrun:** `Slack::alloc_words` answers `Err(error::unsupported("slack: reserved region too small for the node"))` when `remaining() < n` (`terms/slack.rs:122-125`), and `Sink::tuple`'s `check_tuple_arity` (`terms/sink.rs:59`) rejects `> beam_term::MAX_ARITYVAL` (`SystemLimit`) **before** carving. A wrong `words()` therefore degrades to a typed failure, never a write past the region.
- **Every heap-term read happens AFTER the reservation** — a collecting host relocates `x` and the stack in place, so only post-reservation words are valid: the failed-clause args latch is *length-read* before (`Machine::live_raise_function_clause_args`, `:226`, a move-invariant count) and *consumed* after (`Machine::take_raise_function_clause_args`, `:258`, re-seated by `Machine::with_roots`), and the `args` list's elements are carved inside (`write_item`, `:615`).
- **The `head` item is re-read after the reservation too** (`read_operand(machine, ctx, head_operand)`, `:283`, then `sink.cons(item, cooked)`, `:284`) — the register `build_stacktrace` read the raw trace from, so a relocated item is re-derived rather than reused.
- **`raise_snapshot` carries no heap terms.** It is `Vec<(u32,u32,u32)>` (`machine/mod.rs:337`), filled with `(context, func, pc)` triples from Copy metadata (`machine/catch.rs:548-553`, `frames.rs:421`), so `Machine::raise_snapshot().to_vec()` (`:196`) detaches the carve completely and nothing the carving reads can be invalidated by the reservation.
- **The resolved `Arc<Module>` handles and the `&str` file names are owned/borrowed module data**, not heap terms, so keeping them alive in `owned` (`:200-215`) is what lets each item shape borrow its module's `Line` chunk across the carve.
- **The one unrooted allocation on the path is `read_operand` before the head cons** (`:283`): for a non-small `Operand::Integer`/`Character` it calls `make_integer_unrooted` -> `ctx.alloc_words` while the `Slack` base pointer and the carved `cooked` list are live but unpublished. Both call sites pass `Operand::X(0)`, so today it is a pure register read — but the safety rests on the call site, not on the code. Carve the cons **before** reading the operand (or read it into the shape) if a non-register `head_operand` ever appears.
- `Instr::CatchEnd`'s own pair is the same discipline: `make_tuple_rooted(.., 2, 3, ..)` reserves `1 + 2 + 3` and the reader carves exactly the 3-word `{Reason,Trace}` pair after `set_x(0, cooked)` has rooted the cooked trace (`mod.rs:223-312`).

## Known Issues
**A supplied EMPTY trace is its own shape, and the trace TERM is what distinguishes it.**
`Raise::supplied_trace` (an `Option<LiveReason>` whose payload is the trace **term**, an `Eterm`) is the discriminator: `erlang:raise(error, boom, [])` carries the word `[]` and cooks `[]`, while a raise that supplied nothing derives the `raise_snapshot` chain.
The term is read once at the raise site (`../../interpreter/bif.rs`, `supplied_term`) and published through the `pending_supplied_term` / `raise_supplied_term` latch pair into the landed `x(2)` (plus the word-less `pending_supplied_term_verbatim` marker), so `symbolic_stacktrace` echoes it unchanged — C's `is_raised_exc` (`erts/emulator/beam/beam_common.c:1219-1223`) and `raise_3`'s no-copy `c_p->ftrace = stacktrace` (`erts/emulator/beam/bif.c:1641-1643`).
The residual is the **structural** shape only: a host arming `Raise::stacktrace` with an empty `StackTrace` and no term is still indistinguishable from "nothing supplied" (the `pending_supplied_trace` arming condition is `!raise.stacktrace.is_empty()`), so it derives frames — a shape `erlang:raise/3` does not produce, since it always carries the term.
Pinned by `tests/raise_with_trace.rs::{an_empty_supplied_term_cooks_the_empty_list_where_an_absent_one_derives_frames, the_oracle_and_the_engine_both_keep_a_supplied_empty_trace}`.

## Constraints
- Errors are `beam_common::{Result, BeamError}` only. An unsupported case is a typed
  `error::unsupported`; **never** a panic, a substituted atom, or a wrong result.
- Term construction/inspection is only `beam_term` + the crate's `terms` builders;
  the **rooted** twins (`*_rooted`) are what the instruction bodies here call.
- **Depth is heap-bounded here, never native-stack-bounded:** every walk in this module is an explicit loop over a slice/`Vec`
  (`symbolic_stacktrace`'s `for` over `raise_snapshot`/`StackFrame`s, `carve`/`callee_head_item`'s `for 0..argc`, `write_list_at`'s cons loop,
  `nearest_line`'s backward scan), and the argument list + `{error_info,Map}` map are stored into the item verbatim, never re-walked —
  so a 100 000-frame trace grows the Rust heap, not the stack.
  The native-recursive walks a deep trace/reason meets live in sibling crates: `beam_bifs`'s printer (`print.rs::render_chunk`, per nesting level *and* per list cell)
  and `beam_term::compare::cmp_impl` (per tuple element / map pair; lists loop).
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

## The synthesized BIF head item (`bif_frame.rs`)
A raise coming out of a **BIF** makes the BIF itself the innermost cooked item — C's `save_stacktrace` sets `s->current = bif_mfa` and `s->pc = NULL` (`erts/emulator/beam/beam_common.c:877-1063`), so the item has the **real call arguments** in its `A` slot and **no** `{file,_}`/`{line,_}`; `build_stacktrace` then appends an `error_info` **location extension** (`erts_build_mfa_item`, `utils.c:3430-3457`). The engine's raise snapshot is `(context, func, pc)` metadata, so this is the one item it cannot derive: `bif_frame::bif_head_item` synthesizes the whole `{Module, Function, ArgList, Location}` and publishes it as the raise's own raw-trace word, the same protocol `callee.rs` uses for an `undef` (`Machine::raise_bif_error_with_head` arms the one-shot `pending_callee_head`; `symbolic_stacktrace` conses the item in front of its frames **verbatim**, never rebuilding it).
- **The shape rule (closed form): the format-module table plus one reason gate.** `Location` is `[{error_info, #{module => FormatModule}}]` when `format_module(bif_module)` names an owner (`format_module`, C `:937-1006`: `atomics`/`counters`/`erlang`/`erts_internal`/`persistent_term` → `erl_erts_errors`; `code`/`os` → `erl_kernel_errors`; `binary`/`ets`/`lists`/`maps`/`math`/`re`/`unicode` → `erl_stdlib_errors`) **and** the reason is not the bare atom `function_clause`, or `[{error_info, #{cause => Value, module => FormatModule}}]` when the host also answers a cause (next bullet); every other module gets `[]` (C's `default:` arm; oracle-measured for `records:get/2`). The decision is taken in `build` **before** the reservation, so no `Eterm` is read across an allocating door, and the rule consults **nothing about how the name reached the BIF door** — no declared-vs-fast-path classification.
  1. A `beam_bifs` **stdlib fast path** (a registry key `bif.tab` does not declare, e.g. `lists:seq/3`) keeps its module's extension exactly like a declared BIF. OTP's sources re-raise through `error_with_info/2` — `erlang:error(Reason, Args, [{error_info, #{module => erl_stdlib_errors}}])`, an **Erlang-level `error/3`**, not a BIF classification — so the oracle's item carries it. Measured (OTP 29.1.2): `try lists:seq(1,2,0) catch C:R:S -> hd(S) end` ⇒ `{lists,seq,[1,2,0],[{file,"lists.erl"},{line,541},{error_info,#{module => erl_stdlib_errors}}]}`; this runtime cooks `{lists,seq,[1,2,0],[{error_info,#{module => erl_stdlib_errors}}]}`.
  2. The bare **`function_clause`** reason drops the extension whatever the module, because C's only producer is `func_info` (`emu/instrs.tab`'s `i_func_info_IaaI`), which names **its own** frame with a source location and attaches no info. This is what makes `lists:max([])` cook `{lists,max,[[]],[]}` (oracle: `{lists,max,[[]],[{file,"lists.erl"},{line,638}]}`).
  Both arming entry points therefore take the failing `BeamError` (`bif_head_item`/`bif_head_item_regs`' last parameter): the reason lives at the arming site, and only its **name** is read.
  **Residual:** the oracle's `{file,_}`/`{line,_}` is unreachable engine-side (there is no "source location of a loaded function" door, and a `bif.tab` BIF has no Erlang source while a fast path has no code), so every item cooked here carries an empty position. Module, function, the real argument list and the extension are right.
- **The cause pair (C's `EXF_HAS_EXT_INFO` arm).** A host answering `ExecutionContext::bif_error_cause(module, function, args)` (`context.rs`'s defaulted seam) adds C's second pair (`:992-1000`): `Location` becomes `[{error_info, #{cause => Value, module => FormatModule}}]`.
  The seam is asked **before** the reservation, because it may allocate on the host's own heap, and only where the extension is attached at all and a catch frame is live; an `Ok(None)`, an `Err`, an unreadable argument operand or a **boxed** answer each decline to the one-pair form, never to a failed item — the value is embedded verbatim, so it must be an immediate.
  `EXTENSION_WORDS`/`EXTENSION_WORDS_WITH_CAUSE` size the two forms (`map_words_capacity(1)` vs `map_words_capacity(2)` + `3` + `2`), and the `Extension` struct carries the resolved atoms with `cause_key: Option<Atom>`, so sizing and writing cannot disagree.
  The `cause` key atom goes through the same `namespace_atom` path as the other four; an unnameable `cause` atom drops the cause only (keeping the module pair), never the extension.
  **The rendered pair order is the flat map's atom-index storage order** (`beam_term::map::write_map` sorts a flat map's atom keys by index, C's `erts_cmp_flatmap_keys`), so `#{cause => …, module => …}` holds exactly on a table in `erts/emulator/beam/atom.names` order (`cause` line 178, `module` line 471).
  **Residual:** `beam_rt`'s shared table is not seeded in that order and keeps `cause` late, so a host answer there renders `module` before `cause` — a host-side ordering residual (the same rule fixes the `term_to_binary` bytes of any multi-atom map), not something engine-side code can fix; no runtime host answers the seam, so a plain run renders the one-pair form. The deciding half is `beam_bifs::error_cause` (`ErrorCause::{Cause, Undecided}`).
- **Two entry points over ONE carve.** `bif_head_item` takes the failing instruction's `Operand` slice (`bif`/`gc_bif`/`bif0` in `exec_bif`); `bif_head_item_regs` takes a register range, for the `call_ext*`-shaped routes whose arguments are `x(0..arity)` and whose **arglist arity is the import table's declared one** (C's `make_arglist(c_p, reg, bif_mfa->arity)`, `beam_common.c:1062`), not the instruction's own — the `call_ext 3 erlang:apply/3` split. Both route every index through `read_operand` (`Registers` reads `Operand::X(i)`) and share `carve`, so the layout/sizing is defined once; an unreadable register answers `Ok(None)`, never a fabricated argument.
- **Armed at every site that raises a BIF failure through `Machine::raise_bif_error*`:**
  `interpreter/bif.rs::exec_bif`'s `Err(e) if fail == NO_FAIL` arm (the `bif`/`gc_bif`/`bif0` funnel); `interpreter/dispatch.rs`'s `call_ext*` funnel — where the synthesized `undef` callee item is built **first** and takes precedence, the BIF head being built only when that answers `None` (so the `undef` shape keeps `Location == NIL`); the same file's local-callee synchronous route; `families/receive.rs`'s `Instr::Send` failure (the canonical `erlang:send/2`, never `'!'`); and `families/funs/mod.rs`'s `apply_target` failure, again after the `undef` head item.
  Guard failures (`fail != NO_FAIL`) arm nothing anywhere, and the rule-1 gate below is applied at every site.
- **Deliberately NOT armed:** `funs/mod.rs`'s closure-application failure (`fun_apply_error`) — applying a *closure* has no BIF MFA, and naming the BIF a fun happens to wrap would be a fabricated frame (a fun-wrapping-BIF call is dispatched by the host's closure seam, not at an instruction); and the non-BIF raise doors (`raise_badarg`/`_badfun`/`_badarity`/`_badmatch`, the float arms), which have no `bif_mfa` in C either.
- **The closed format-module table** (`format_module`, C `:937-1006`): `atomics`/`counters`/`erlang`/`erts_internal`/`persistent_term` → `erl_erts_errors`; `code`/`os` → `erl_kernel_errors`; `binary`/`ets`/`lists`/`maps`/`math`/`re`/`unicode` → `erl_stdlib_errors`; **every other module → no `error_info` at all**, so its `Location` is `[]` (C's `default:` arm; oracle-measured for `records:get/2`). `format_module` and `is_non_bif_raise` are `pub` and re-exported at the crate root (`beam_engine::format_module`, `beam_engine::is_non_bif_raise`) so `beam_rt`'s `-eval` tree-walking route cooks the same extension and applies the same raise-BIF exclusion from **one** definition of C's closed table.
- **Rule-1 gate** (`is_non_bif_raise`, C `non_bif_stacktrace` `:911-922`): `erlang:error/1,2,3`, `erlang:exit/1`, `erlang:throw/1` get **no** BIF item — their innermost frame is the caller's own (the `erlang:error/2,3` args + `{error_info,Map}` ride that caller frame — see the section below).
- **Rooting:** the item is ONE rooted reservation (`terms::reserve_slack`) sized `2 * arity + 5` plus the extension's own `EXTENSION_WORDS` (or `EXTENSION_WORDS_WITH_CAUSE` when a cause rides along) — and when the shape rule drops the extension those last words are **not charged at all**, because the size is derived from the same `Option` the carve writes (`total = 2 * argc + ITEM_WORDS + extension.map_or(0, |_| EXTENSION_WORDS)`; a wrong size is a typed `unsupported` from `reserve_slack`, never an overwrite). The graph is carved children-before-parents (map → `{error_info,Map}` → its cons → the argument list → the item tuple). The argument operands are the instruction's own and are read **after** the reservation, so no `Eterm` is held across the allocating door; the atoms (`Module`, `Function`, `error_info`, `module`, `cause`, `FormatModule`) are immediates resolved through `namespace_atom` (live namespace first, module snapshot second — a module's snapshot normally lacks all of them).
- **Declines (`None`, an unmodified raise), never a fabricated frame:** the rule-1 gate, **no live catch frame** (`catch_frame_depth() == 0`, where the cooked trace is never reached), an unnameable atom, or an unreadable argument operand (the BIF's own error must not be replaced by a marshalling one).
- **Residuals.** (a) every item cooked here carries an EMPTY `{file,_}`/`{line,_}` position where the oracle names one — see the shape rule above; (b) the `cause` arm is a **host seam**, not an engine-side omission: it renders when `bif_error_cause` answers an immediate and the one-pair `#{module => FormatModule}` form otherwise, so a host that answers no cause leaves the `ets`/`erlang:register`/`erlang:process_flag`/`erlang:start_timer`/bit-syntax rows cause-less (their pair order is the further, host-side residual above); a `Raise::error_info()` payload renders **verbatim** through `pending_error_info`/`raise_error_info`, and no BIF raise in the tree sets one. (c) A BIF module outside the table renders an item with an empty location — correct, not a gap. (d) The **local-callee** route names the module's own function, so a local call into a non-exported NIF stub reports `{Module,Stub,ArgList,…}` where C's NIF epilogue takes `c_p->current` (the *calling* function, `emu/bif_instrs.tab:633`) — the honest local name, a divergence only for a NIF-backed local call. (e) A **fun-wrapping-BIF** call (`(fun erlang:hd/1)(1)`) never reaches an armed site: the host's closure seam dispatches it, so the cooked trace keeps the caller frame.
`tests/error_info.rs` pins the measured table end to end (the arms above — the `lists:seq/3` extension row, the `lists:max/1` `function_clause` row, the declared-BIF row, the outside-the-table rows — plus tier agreement), on top of `bif_frame/tests.rs`'s unit shapes.
Pins: `bif_frame/tests.rs` (the three table arms + the negative, the rule-1 matrix, the item shape for `erlang`/`binary`/`os`/an outside-the-table module, a 0-arity `[]` argument list, the shape rule's two rows — a `function_clause` reason's `[]` location and a non-`function_clause` raise keeping its tabled module's extension — all four declines, three end-to-end cooked traces on **both** tiers with tier agreement asserted before the shape, and the cause matrix: a host answer's two-pair form, the `cause`-before-`module` stored order on an `atom.names`-ordered table, the default/`None`/`Err`/boxed-name declines keeping the one-pair form, the three sites that are never asked at all — outside the table, a `function_clause` reason, no live catch frame — a cause reaching the cooked trace on both tiers, and that the shape-rule helpers are reachable from the crate root).

## See Also
- `../../machine/catch.rs` → the catch-frame stack, `throw`/`throw_with_trace`,
  `with_uncaught_frames`/`raise_stacktrace`, `land_exception`, `unwind_to_frame` (the raise
  half of this protocol).
- `../../CONTEXT.md` → crate contracts, the frozen seams, the execution model.
- `../../INSTRUCTION_SURFACE.md` → per-opcode semantics and the complete gap list.

## `erlang:error/2,3` argument list and `{error_info, Map}` extension
C's `save_stacktrace` sends `erlang:error/1,2,3`, `exit/1` and `throw/1` to its `non_bif_stacktrace:` arm, so the innermost item is the **caller's** function at the call site (`beam_common.c:1005-1023`).
There `args` is the exception's own argument list (`is_list(args) || is_nil(args)` selects it in the `A` slot, else the arity: `utils.c:3449-3457`) and `error_info = c_p->fvalue` for `EXC_ERROR_3` (`:1024-1026`), packed as `{error_info, Map}` into one cons **after** the `{file,_}`/`{line,_}` pairs, so the extended location reads `[{file,F},{line,N},{error_info,Map}]`.
Both belong to the innermost item only: every outer frame is built with `erts_build_mfa_item(..., am_true, NIL)` (`:1283-1291`).
The engine carries the list in the existing `raise_function_clause_args` latch and the map in a new `raise_error_info`/`pending_error_info` pair (`LOOSE_HEAP_TERMS = 7`), armed by `interpreter/bif.rs`'s `throw_with_trace_headed` from `Raise::saved_args`/`Raise::error_info` **only when the raise synthesizes no head item** (`!has_head`) — a head item carries its own location.
The carve charges the extension exactly **5** words (the `{error_info,Map}` 2-tuple's `3` plus the outer list growing from two to three cells, `+2`); the map word is **shared, not copied**, so the map's own nodes are never charged.
Residual: on **this** caller-frame carve the extension is the map the raise carried (`erlang:error/3`'s `{error_info, Map}`), rendered verbatim; C's `EXF_HAS_EXT_INFO` `cause` arm (`ets:lookup(nonexistent,k)` ⇒ `#{cause => id, module => erl_stdlib_errors}`, `erlang:register(a,1)` ⇒ `never`, `erlang:process_flag(1,1)` ⇒ `badopt`, `erlang:start_timer(bad,self(),x)` ⇒ `time`, a bit-syntax failure ⇒ `{Segment,Op,type,Value}`) belongs to the BIF-item carve and its host seam, never here.
Residual: the **uncaught** path (`Machine::with_uncaught_frames`) renders the symbolic `beam_common::StackTrace`, whose `Location` is two-element, so an uncaught `error/3` raise carries no extension there; the path consumes (`take_`) both latches rather than leaving one armed.
The `erlang:error/3` option-scanning rules (last `{error_info,_}` wins, a non-map payload dropped, an improper/non-matching options list invalidating the scan) are `beam_bifs`'s.

## A supplied trace is echoed verbatim
`erlang:raise/3` hands the engine the caller's trace **term** (`Raise::supplied_trace`), and C returns it unchanged: `build_stacktrace`'s `is_raised_exc` arm answers `c_p->ftrace` with no decoding, name resolution, frame counting or re-encoding (`../../erts/emulator/beam/beam_common.c:1219-1223`; `raise_3`'s no-copy path is `c_p->ftrace = stacktrace`, `bif.c:1641-1643`).
- `symbolic_stacktrace` short-circuits on the machine's `raise_supplied_term_verbatim` marker and returns the supplied term itself — no `decode_stacktrace`, no atom lookup, no re-encode, and no fabricated frame when a name is unknown.
- The term `[]` is word-identical to "nothing supplied", so the discriminator is the marker's **presence**, never the value: a supplied `[]` cooks `[]` where an absent trace derives frames.
- Rooting: the term is a loose heap term parked by `Machine::with_roots` (`LOOSE_HEAP_TERMS`), so every collection relocates it; the marker itself carries no word.
- `Instr::RawRaise` (a folded `catch C:R:S -> erlang:raise(C,R,S)`, `sys_core_fold`) re-feeds the flag into `raise_supplied`, so C's `is_raised_exc` echo persists across a folded rethrow (`raw_raise` stores the caught trace object back into `c_p->ftrace`, `beam_common.c:1148-1179`).
- Rows (oracle `erl` 29.1.2 vs this engine, compiled module): `raise(error,boom,[])` ⇒ `[]`; `raise(error,boom,[{m,f,[1,2],[]}])` ⇒ verbatim; an unknown-module frame ⇒ verbatim; a caught `error(Reason,[A,B])` trace round-trips unchanged through a re-raise. Pinned on both tiers by `tests/supplied.rs`.
- Residual: the folded-rethrow row's head item keeps the raised `A` slot on the oracle (`{m,rethrower,[a,b],…}`) where this engine renders the arity `0`; recorded, never guessed away.

