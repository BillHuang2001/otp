# `beam_bifs` — BIF Registry (`bif.tab` surface) & Implementations
## Intent
Owns the **Built-In Function layer** of the Rust ERTS rewrite (Stage 5, Part II §H): the registry that
maps `(module, function, arity)` → implementation, and the curated core of the `erlang`/`io`/`ets`/
`erts_internal` BIF surface behind it. Implements compatibility contract **Part I §A.7** (the OTP app
contracts: the `bif.tab` surface, the `erlang`/`erts_internal`/`prim_*` built-ins) — the BIF *names,
modules and arities reproduce the real `../../erts/emulator/beam/bif.tab`*; they are never invented.
Consumed by `beam_rt`, which adapts `BifRegistry` into the engine's
`ExecutionContext::call_bif(module, function, arity, args) -> Result<Eterm>` seam (see
`../beam_engine/CONTEXT.md`).

Failure is **return-based** (workspace `../CONTEXT.md` §3): a bad argument is `BeamError::BadArg`,
arithmetic on a non-number is `BeamError::BadArith`, an out-of-range size is `BeamError::SystemLimit`,
and `erlang:error/1`-style BIFs return `BeamError::Raise`. The engine turns these into a BEAM raise
(exception tag in `x(0)`) — there is no `setjmp`/`longjmp` and no silent failure.

Ground truth (read-only, never modify): `../../erts/emulator/beam/bif.tab` (the authoritative BIF
roster), `bif.c`/`bif.h` (helpers, `badarg`/`badarith`, `erts_internal_cmp_term_2`),
`erl_bif_op.c` (arithmetic ops + guards), `erl_bif_guard.c` (comparison + type tests),
`erl_bif_lists.c`, `erl_arith.c`, `../../erts/preloaded/src/erlang.erl` / `erts_internal.erl`
(semantics), and — for the `io` subset — `../../lib/stdlib/src/io_lib_format.erl`.

## API Surface (FROZEN — `beam_rt` and tests design against this)
Re-exported at the crate root and via `beam_bifs::prelude::*`: [`BifRegistry`]/[`BifFn`]/
`default_registry`, [`BifContext`] + its sinks (`OutputSink`/`SendHook`/`StdoutSink`/`NullSink`/
`BytesSink`), the raise builders (`raise_error`/`raise_exit`/`raise_throw`), the reason-term mapping
(`exit_reason_from_term`/`reason_term_from_exit_reason`) and `TableRegistry`.

### `registry.rs` — the seam
```rust
/// A BIF implementation. Receives the calling context and the argument slice; the
/// registry guarantees `args.len()` equals the registered arity.
pub type BifFn = fn(&mut BifContext<'_>, &[Eterm]) -> Result<Eterm>;

pub struct BifRegistry { /* HashMap<(String, String, u32), BifFn> */ }

impl BifRegistry {
    pub fn new() -> Self;
    /// The Milestone-1 core: every BIF in the "Implemented BIF list" below.
    pub fn with_core_bifs() -> Self;
    /// Returns the previous implementation if `(module, function, arity)` was already registered.
    pub fn register(&mut self, module: &str, function: &str, arity: u32, f: BifFn) -> Option<BifFn>;
    pub fn lookup(&self, module: &str, function: &str, arity: u32) -> Option<BifFn>;
    pub fn contains(&self, module: &str, function: &str, arity: u32) -> bool;
    /// Look up + arity-check + invoke. Unknown `(module,function,arity)` ⇒ `error::undefined_bif`;
    /// `args.len() != arity` ⇒ `BeamError::BadArity`.
    pub fn call(&self, module: &str, function: &str, arity: u32,
                ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    /// All registered `(module, function, arity)` keys (sorted) — for docs/diagnostics.
    pub fn names(&self) -> Vec<(String, String, u32)>;
    pub fn entries(&self) -> impl Iterator<Item = (String, String, u32, BifFn)> + '_;
}

/// A registry with the Milestone-1 core BIFs registered.
pub fn default_registry() -> BifRegistry;
```
- **Keying is `(module: &str, function: &str, arity: u32)`** — exactly how the loader/engine resolves
  a BIF from `Module.imports`, and the shape A.7 pins. Operator BIFs keep their *quoted* names
  (`"+"`, `"=<"`, `"=:="`, `"!"`, …), matching `bif.tab`.

### `context.rs` — what a BIF may touch
```rust
pub trait OutputSink { fn write_str(&mut self, s: &str) -> Result<()>; }
pub struct StdoutSink;   // writes to the process's stdout
pub struct NullSink;     // discards
pub struct BytesSink;    // captures (test/embedding): contents() -> &[u8], as_str() -> String, clear()

/// Delivers `term` to the process `to` (an internal pid). The runtime supplies this over
/// `beam_process`/`beam_message`; M1 has no in-crate delivery.
pub trait SendHook { fn send(&mut self, to: Eterm, term: Eterm) -> Result<()>; }

pub struct BifContext<'a> { /* heap, atoms, self_pid, output, sender, tables */ }
impl<'a> BifContext<'a> {
    pub fn new(heap: &'a mut beam_alloc::Heap, atoms: &'a AtomTable, self_pid: ProcessId) -> Self;
    pub fn with_output(self, sink: &'a mut dyn OutputSink) -> Self;   // default when unset: stdout
    pub fn with_sender(self, hook: &'a mut dyn SendHook) -> Self;     // default when unset: none
    pub fn with_tables(self, reg: &'a mut TableRegistry) -> Self;     // default when unset: none
    pub fn heap(&mut self) -> &mut beam_alloc::Heap;
    pub fn atoms(&self) -> &AtomTable;
    pub fn self_pid(&self) -> ProcessId;
    // constructors / writers (all fallible → Result)
    pub fn int(&self, v: i64) -> Result<Eterm>;          // try_make_small; out of 60-bit range ⇒ SystemLimit
    pub fn atom(&self, name: &str) -> Result<Eterm>;     // AtomTable::lookup; absent ⇒ TermNotFound
    pub fn boolean(&self, b: bool) -> Result<Eterm>;     // atom("true"/"false")
    pub fn ok(&self) -> Result<Eterm>;                   // atom("ok")
    pub fn make_float(&mut self, v: f64) -> Result<Eterm>;
    pub fn tuple(&mut self, elems: &[Eterm]) -> Result<Eterm>;
    pub fn cons(&mut self, car: Eterm, cdr: Eterm) -> Result<Eterm>;
    pub fn list(&mut self, elems: &[Eterm]) -> Result<Eterm>;   // NIL for empty
}
```
- `output`/`sender`/`tables` are **optional seams**: unset ⇒ `io:format` writes to stdout, and
  `send/2`/`ets:*` return `error::unsupported_bif`.

### `access.rs` — the crate's ONLY `#[allow(unsafe_code)]` module
`beam_term`'s raw-term accessors (`is_tuple`, `tuple_element`, `car`, `cdr`, `is_map`, …) are
`unsafe fn` (they dereference a header word). Following `beam_engine::predicates`, this module holds
the crate's single `#[allow(unsafe_code)]` and exposes safe wrappers with a module-level SAFETY
argument; every other module is safe Rust. Provides: classification (`is_tuple`, `is_float`,
`is_integer`, `is_number`, `is_binary`, `is_bitstring`, `is_map`, `is_map_key` unsupported,
`is_pid`, `is_port`, `is_reference`, `is_function`, `is_big`, `is_external_*`), structure
(`tuple_size`, `tuple_element`, `car`, `cdr`), and floats (`make_float(heap, f64)`, `float_value`,
`float_bits`). The SAFETY invariant: every wrapped term is a **live BEAM term** (a BIF argument from
the machine's registers/stack, or a term built on a live heap) — never unvalidated raw memory.

### `error.rs`
`pub(crate)` constructors (`badarg`, `badarith`, `badarity`, `system_limit`) and the public raise
builders `raise_error(reason)`, `raise_exit(reason)`, `raise_throw(reason)` over
`beam_common::{Raise, ExitReason, ExceptionClass}`, plus `undefined_bif()`/`unsupported_bif()`.

### The implemented BIF list (Milestone-1 core)
Module `erlang` (names/arities verbatim from `bif.tab`):
- arithmetic `"+"/2`, `"-"/2`, `"*"/2`, `"/"/2`, `"div"/2`, `"rem"/2`, `"abs"/1`, `"-"/1`, `"+"/1`
- comparison `"=="2`, `"/="2`, `"=:="2`, `"=/="2`, `"<"/2`, `">"/2`, `"=<"/2`, `">="/2`
- type tests `is_atom/1 is_list/1 is_tuple/1 is_float/1 is_integer/1 is_number/1 is_pid/1 is_port/1
  is_reference/1 is_binary/1 is_bitstring/1 is_map/1 is_boolean/1 is_function/1`
- term ops `element/2 setelement/3 tuple_size/1 tuple_to_list/1 list_to_tuple/1 length/1 hd/1 tl/1
  size/1`
- process/self/send `self/0`, `"!"/2`, `send/2`
- raise helpers `error/1`, `error/2`, `exit/1`, `throw/1`

Module `erts_internal`: `cmp_term/2` (`-1|0|1`, exact order — the BIF `bif.c:5468` defines).

Module `io`: `format/1`, `format/2`.

Module `ets`: `new/2`, `insert/2`, `lookup/2`, `delete/1`.

`prim_*` modules: **not implemented in M1** (documented gap) — the registry supports them
(`register("prim_file", …)`), nothing is registered yet.

### The `io:format` directive subset (matches the OTP oracle, verified against erts-17.1)
`~w` (write), `~p` (print), `~s` (string), `~c` (character), `~n` (newline), `~b`/`~B` (integer,
base 10 — or `~.<base>b` / `~.<base>B` for base 2–36), `~i` (consume and ignore the argument),
`~~` (literal `~`), plus an optional decimal **field width** (`~10w` → right-justified, space padded).
- `~p` applies the **printable-list** rule: a non-empty proper list of small ints in `32..=126` prints
  as a `"…"` string (escaping `"` and `\`); `~w` never does. (Documented simplification: latin1
  `160..=255` and control-char escapes are not special-cased.)
- **`~d` is NOT a directive** — real ERTS raises `badarg` (verified: `io_lib:format("~d",[255])` →
  `error:badarg`) and this crate matches it. The objective's example list named `~d`; `~b` is the base-10
  integer directive. Documented deviation from the objective's wording, faithful to the oracle.
- Everything else (`~e ~f ~g ~W ~P ~x ~X ~+ ~#`, `~*`, negative/other adjustments) ⇒ `badarg`.
- The format term must be a **flat list of byte-sized integers**; `Args` a proper list.
- Returns the atom `ok`. Also registered is `io:format/1` (empty args).

### The term printer (`print.rs`, used by `~w`/`~p`)
Renders: small ints (decimal), atoms (bare iff `[a-z][A-Za-z0-9_@]*`, else `'…'`), `NIL` (`[]`),
floats, proper/improper lists (`[1,2]`, `[a|b]`), tuples (`{a,1}`), internal pids
(`<Node.Number.Serial>`). Non-M1 classes (maps, binaries, funs, refs, bigints, records, external
pid/port/ref) ⇒ `Err(BeamError::BadArg)` (documented: they are not constructible in M1, so this is
unreachable — "never a silently-wrong result"). Float rendering: Rust's shortest round-trip with a
`.0` appended when integral; **documented deviation** from OTP's `%.*g` for large/small magnitudes.

### `reason.rs` — `beam_common::ExitReason` ↔ reason term
The documented `beam_process`/`beam_rt` handoff (see `../beam_process/CONTEXT.md` Known Issues).
Both functions are **public API**, re-exported from the crate root.
```rust
pub fn exit_reason_from_term(term: Eterm, atoms: &AtomTable) -> ExitReason;  // atom→named/Atom, else Structured
pub fn reason_term_from_exit_reason(reason: &ExitReason, atoms: &mut AtomTable) -> Result<Eterm>; // well-known atoms + Atom(name); Structured ⇒ BadArg
```
`erlang:error/1,2 exit/1 throw/1` map their live reason term via `exit_reason_from_term` and return
`Err(raise_error/exit/throw(..))`. A structured (non-atom) reason is represented by
`ExitReason::Structured`, which carries no term — the live term is owned by the term heap (the engine
/`beam_rt` capture it); documented gap.

### `ets.rs` — minimal `ets:*` bridge over `beam_ets::Table`
```rust
pub struct TableRegistry { /* HashMap<u64, Table> */ }   // Default = empty
impl TableRegistry {
    pub fn new() -> TableRegistry;
    pub fn insert(&mut self, handle: u64, table: Table) -> Option<Table>;  // previous table, if any
    pub fn get(&self, handle: u64) -> Option<&Table>;
    pub fn get_mut(&mut self, handle: u64) -> Option<&mut Table>;
    pub fn contains(&self, handle: u64) -> bool;
    pub fn remove(&mut self, handle: u64) -> Option<Table>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
```
`TableRegistry` maps an opaque handle to a `beam_ets::Table`. `ets:new/2` parses the common option
subset (`set`/`ordered_set`/`bag`/`duplicate_bag`, `public`/`protected`/`private`, `named_table`,
`{keypos,N}`) into `TableOptions`; the handle is an on-heap `Ref` thing built like
`beam_process::MonitorRef` (`[make_header(3, REF_SUBTAG), 0, serial, id]`) — the M1 substitution for a
`tid()`. `insert/2`/`lookup/2`/`delete/1` operate on the table; `lookup` copies the objects out to the
caller's heap (they alias table storage in `beam_ets`). Small and documented; see Known Issues.

## Constraints
- **Errors:** `beam_common::{Result, BeamError}` only — never a crate-local error enum, never
  `panic!`, never `unwrap()` on runtime data, never a wrong result. Argument violations ⇒ `BadArg`;
  non-numeric arithmetic ⇒ `BadArith`; wrong arg count ⇒ `BadArity`; 60-bit overflow ⇒ `SystemLimit`.
- **Never re-implement term order/size.** Comparison ⇒ `beam_term::compare::{cmp, cmp_arith}`;
  hashing ⇒ `beam_term::hashing::term_hash`; object size ⇒ `beam_term::size::size_object`. Term
  construction/inspection ⇒ `beam_term` accessors only (via `access.rs`), allocation ⇒
  `beam_alloc::Heap`.
- **`unsafe` confinement.** The crate is `#![deny(unsafe_code)]`; `src/access.rs` is its **only**
  `#[allow(unsafe_code)]` module (the `beam_engine::predicates` convention). `unsafe_op_in_unsafe_fn`
  stays clean.
- **Deps (fixed by `Cargo.toml`):** `beam_common`, `beam_term`, `beam_alloc`, `beam_process`,
  `beam_message`, `beam_codec`, `beam_ets`, `beam_gc`, `libc`. M1 uses `beam_common`, `beam_term`,
  `beam_alloc`, `beam_process`, `beam_ets`; `beam_message`/`beam_codec`/`beam_gc`/`libc` are the
  declared seams for the documented follow-ups (`send` via `beam_message`, `term_to_binary` via
  `beam_codec`, MSO via `beam_gc`). Adding a dependency edge requires reporting to `../`.
- Files target ~1000 lines; split by responsibility (registry / context / access / arith / compare /
  type tests / term ops / proc / reason / io / print / ets). Licensing Apache-2.0.

## Test Strategy
- Per-module `#[cfg(test)]` unit tests, plus `tests/` integration tests; the crate builds a
  `BifContext` over a real `beam_alloc::Heap` + a seeded `AtomTable` (intern the well-known atoms
  `true`, `false`, `ok`, …) and a `BytesSink` capture.
- **`tests/` integration suite** (public API only — `beam_bifs::*` plus the declared dependency
  crates):
  - `tests/registry.rs` — the `BifRegistry` seam: `default_registry()` registers **exactly** the 54
    core-BIF keys (`len() == 54`, `names()` compared against the roster built and sorted in the
    test), a fresh registry is empty, `register` returns the replaced impl, `lookup` is exact on
    `(module, function, arity)`, and `call` reports an unknown BIF (`undef` raise) and a bad arity
    (`BadArity`).
  - `tests/erlang_bifs.rs` — the `erlang`/`erts_internal` BIFs driven through `call` (arithmetic,
    comparison incl. `1 == 1.0` vs `1 =:= 1.0` and `erts_internal:cmp_term/2`, type tests incl.
    `is_list([])`, term ops incl. their `badarg` cases), `self/0` + `send/2` (the `SendHook` seam and
    the `unsupported_bif` case), the raise BIFs (`error/exit/throw` classes) and the public reason
    mapping (`exit_reason_from_term`/`reason_term_from_exit_reason`).
  - `tests/io_ets.rs` — `io:format` **exact captured bytes** through a `BytesSink`
    (`"~p~n"` ⇒ `{a,1}\n`, `"~w ~s ~b~n"` ⇒ `[97,98,99] a 255\n`), `~d`/`~x` ⇒ `BadArg`, and the
    `ets:new/insert/lookup/delete` round-trip over a real `TableRegistry` (structural `compare::eq`).
- **Required M1 evidence (the gate):**
  - `registry`: a hit returns the fn; a miss is `None`; `call` of an unknown BIF / a wrong arity errors;
    `with_core_bifs` registers every name in the list above (`names()` contains them).
  - `arith`: each of `+ - * / div rem abs` (+ unary `-`/`+`) on small ints; a float case for `/`;
    a `badarith` case (e.g. `1 + a`); `1 div 0` ⇒ `BadArith`; overflow ⇒ `SystemLimit`.
  - `compare`: every operator; **`1 == 1.0` is `true` but `1 =:= 1.0` is `false`**; `1 </= 1.0` vs
    `1 < 1.0`; cross-type (`a < {a}` uses the term order).
  - `type_test`: a representative sample over constructed terms (`is_integer(1)`, `is_float(1.0)`,
    `is_list([])` **true**, `is_list([1])` true, `is_tuple({a})` true, `is_atom(a)`, `is_pid(self())`,
    `is_number(1.0)`, `is_boolean(true)`).
  - `term_ops`: `element/2`, `tuple_size/1`, `length/1`, `hd/1`, `tl/1` (+ their `badarg` cases),
    `tuple_to_list/1`, `list_to_tuple/1`, `setelement/3`.
  - `io`: `io:format("~p~n", [Term])` and `io:format("~w ~s ~b~n", [...])` written to a `BytesSink`,
    asserting the exact captured bytes; an unsupported directive ⇒ `BadArg`.
  - `print`: the `~w`/`~p` renderings pinned against the values probed from the OTP oracle
    (`{a,1}`, `[1,2]`, `[a|b]`, `'Hello'`, `1.0`, `~p` of `"abc"` → `"abc"`, `~w` of `"abc"` →
    `[97,98,99]`).
  - `reason`: `errexit/throw` classes + the atom/structured mapping.
  - `ets`: `ets:new/2` (a `set`) then `insert`/`lookup`/`delete` round-trip over the bridge.
- **Gate (GREEN required):**
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_bifs` and
  `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.
  Always go through `../../scripts/cargo.sh` (`$HOME` read-only); never `cd`; never touch
  `../../tools/` or `../../erts/` (read-only oracle).
- Full differential cross-checks belong to the sibling `../../tools/differential/` harness.

## Design Decisions
- **`BifFn` is a plain `fn` pointer, not a boxed closure.** The registry is built once at boot and
  looked up per BIF call; `fn` keeps it `Copy`, allocation-free to register, and impossible to close
  over state (all BIF state flows through `BifContext`).
- **`BifContext` holds optional seams (`output`/`sender`/`tables`), not concrete runtime types.** The
  engine depends only on `beam_loader`; `beam_rt` supplies the concrete context. Optional seams keep
  the crate testable with a bare heap + atom table and keep the Stage-1/Stage-5 DAG acyclic
  (no `beam_engine` edge).
- **`self/0` reads the pid from the context, not a global.** There is no ambient "current process" in
  the Rust core (the engine is a pure state machine); `BifContext::self_pid` is the injected identity.
- **The upstream `beam_alloc::Heap` is used for all BIF allocation.** `beam_term::heap::Heap` is a
  test arena only (see `../beam_alloc/CONTEXT.md`).
- **Comparison delegates to `beam_term`.** `==`/`/=` use `cmp_arith` (the arithmetic order); `=:=`/
  `=/=` use `cmp` (the exact order); `< > =< >=` use `cmp_arith` — the OTP semantics (`1 < 1.0` is
  false; `1 =:= 1.0` is false). `erts_internal:cmp_term/2` uses `cmp`.
- **`io:format` follows the oracle, not the objective's example list.** `~b` is base 10 and `~d` is
  `badarg` in real erts-17.1 (verified). Reproducing the oracle wins over the illustrative list.
- **The `ets` handle is an on-heap `Ref` thing.** It is a documented M1 substitution for a real
  `tid()`, mirroring `beam_process::MonitorRef::to_eterm`; when `beam_term` gains `make_ref`, only the
  handle constructor changes.

## Known Issues / Intentional M1 gaps
- **Arithmetic is 60-bit small-int + `f64` only.** Bignum (`PosBig`/`NegBig`) has no arithmetic in
  `beam_term`, so integer overflow past `MAX_SMALL`/`MIN_SMALL` returns `BeamError::SystemLimit`
  (real Erlang promotes to a bignum). Mixed int/float arithmetic is supported.
- **The `prim_*` / most of `erts_internal` surface is unimplemented.** Only `erts_internal:cmp_term/2`
  plus the `erlang` core, `io:format`, and four `ets` BIFs are registered. `apply/3`, `spawn*`,
  `process_info`, `term_to_binary`/`binary_to_term`, `make_ref`, `list_to_atom`, `map_*`, `binary_*`
  are not registered (⇒ `undefined_bif`).
- **`~d` is not accepted** (matches the oracle; see above).
- **Float formatting** is Rust's shortest round-trip + `.0`; it deviates from OTP's `%.*g` for large/
  small magnitudes (`1.0e10` prints as `10000000000`).
- **`~p`/`~w` printer coverage**: no maps, binaries, funs, refs, records, bignums or latin1/
  control-char string escapes — such terms error (`BadArg`); the common M1 term set is covered.
- **`io:format` output is a UTF-8 Rust `String`.** Non-`~` format bytes and `~s`/`~c` values
  `>= 128`, and `~p` string escapes for such bytes, are emitted as UTF-8 rather than raw latin1
  bytes; the ASCII subset is byte-exact. A `~n` with a width repeats the newline (oracle), a width
  on `~i` is ignored, and a `.base` precision on any directive other than `~b`/`~B` (e.g. `~.10w`) is
  `badarg`.
- **No in-crate message delivery.** `send/2`/`!/2` require a `SendHook`; without one they raise
  `unsupported_bif`. Wiring to `beam_process::ProcessTable`/`beam_message` is `beam_rt`'s.
- **`error/1,2`'s structured reason** is carried only as `ExitReason::Structured` (no term); the
  engine/`beam_rt` must capture the live reason term from the raise site.
- **`ets` bridge is minimal**: `set`/`ordered_set` core ops; table id is a synthesized `Ref` thing;
  access control/ownership and the full option surface are `beam_ets`'s documented gaps.
- **Registry lookup allocates a key tuple per call** (`HashMap<(String,String,u32), _>`). A future
  optimization interns module/function to ids; correctness does not depend on it.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`, `#![deny(unsafe_code)]`
- `src/registry.rs` → `BifFn`, `BifRegistry` (register/lookup/call), `default_registry`, `register_core_bifs`
- `src/context.rs` → `BifContext` + its constructor/writer helpers, `OutputSink`/`SendHook` + sink impls
- `src/access.rs` → the ONLY `#[allow(unsafe_code)]` module: term classification/structure/float wrappers
- `src/error.rs` → `pub(crate)` failure constructors + the public raise builders
- `src/arith.rs` → `+ - * / div rem abs` (+ unary `-`/`+`) BIFs
- `src/compare.rs` → `== /= =:= =/= < > =< >=` BIFs + `erts_internal:cmp_term/2`
- `src/type_test.rs` → the `is_*` type-test BIFs
- `src/term_ops.rs` → `element setelement tuple_size tuple_to_list list_to_tuple length hd tl size`
- `src/proc.rs` → `self/0`, `send/2` (`"!"/2`)
- `src/reason.rs` → `ExitReason` ↔ reason-term mapping + `error/exit/throw` BIFs
- `src/io.rs` → `io:format/1,2` + the directive engine
- `src/print.rs` → the `~w`/`~p` term printer
- `src/ets.rs` → `TableRegistry` + the `ets:new/insert/lookup/delete` bridge over `beam_ets::Table`
- `tests/` → crate-level integration tests (registry + a representative BIF sweep over a real context)
- `../beam_engine/CONTEXT.md` → the `ExecutionContext::call_bif` seam this registry plugs into (read-only)
- `../beam_process/CONTEXT.md` → `ProcessId`, `SignalAtoms`, the exit-reason handoff (read-only)
- `../beam_ets/CONTEXT.md` → `Table`/`TableOptions` the `ets` bridge drives (read-only)
- `../../erts/emulator/beam/bif.tab` / `bif.c` / `erl_bif_*.c` / `erl_arith.c` → authoritative BIF
  names/arities/semantics (read-only)

## See Also
- `../CONTEXT.md` → crate-layer contracts, the `BifRegistry`+`BifFn` seam, the wave-5 dependency position.
- `../../CONTEXT.md` → workspace contracts (§1 A.7, §2 frozen term layout, §3 error strategy, §6 test gate).
- `../../ERTS_RUST_REWRITE_EVALUATION.md` §A.7 → the OTP app contracts (preloaded/boot/`erts_internal`/`prim_*`).
- `../beam_term/CONTEXT.md` → the frozen term API + the shared `compare`/`size` utilities.
- `../beam_rt/` → the runtime that supplies the concrete `BifContext` and adapts the registry into the engine.
