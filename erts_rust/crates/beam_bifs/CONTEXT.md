# `beam_bifs` — BIF Registry (`bif.tab` surface) & Implementations

## Intent
Owns the **Built-In Function layer** of the Rust ERTS rewrite (Stage 5, Part II §H): the registry
mapping `(module, function, arity)` → implementation, and the curated core of the
`erlang`/`erts_internal`/`io`/`io_lib`/`lists`/`maps`/`binary`/`unicode`/`ets` BIF surface behind it.
Implements compatibility contract **Part I §A.7** (the OTP app contracts: the `bif.tab` surface, the
`erlang`/`erts_internal`/`prim_*` built-ins) — BIF *names, modules and arities reproduce the real
`../../erts/emulator/beam/bif.tab`* and are never invented.
Consumed by `beam_rt`, which adapts `BifRegistry` into the engine's
`ExecutionContext::call_bif(module, function, arity, args) -> Result<Eterm>` seam (see
`../beam_engine/CONTEXT.md`).

Failure is **return-based** (workspace `../CONTEXT.md` §3): a bad argument is `BeamError::BadArg`,
non-numeric arithmetic is `BeamError::BadArith`, a wrong arg count is `BeamError::BadArity`, a
60-bit overflow is `BeamError::SystemLimit`, and `error`/`exit`/`throw`/`undef`/`unsupported_bif` are
`BeamError::Raise`. The engine turns these into a BEAM raise (exception tag in `x(0)`) — there is no
`setjmp`/`longjmp` and no silent failure.

Ground truth (read-only, never modify): `../../erts/emulator/beam/bif.tab` (the authoritative BIF
roster), `bif.c`/`bif.h`, `erl_bif_op.c`/`erl_bif_guard.c`/`erl_bif_lists.c`/`erl_bif_tuple.c`/
`erl_bif_proc.c`/`erl_bif_binary.c`, `erl_arith.c`, `erl_map.c`, `external.c` (the term wire
format), `erl_unicode.c`, `erl_time_sup.c`/`erl_bif_unique.c`, `../../erts/emulator/beam/erl_bits.h`
(the bitstring layout), and the stdlib/preloaded sources (`erlang.erl`, `erts_internal.erl`,
`lists.erl`, `maps.erl`, `io_lib.erl`/`io_lib_format.erl`/`io_lib_pretty.erl`, `unicode.erl`).

## API Surface (FROZEN — `beam_rt` and tests design against this)
Re-exported at the crate root and via `beam_bifs::prelude::*`: [`BifRegistry`]/[`BifFn`]/
`default_registry`, [`BifContext`] + its seams (`OutputSink`/`SendHook`/`AtomInterner` and the sink
impls `StdoutSink`/`NullSink`/`BytesSink`), the raise builders
(`raise_error`/`raise_exit`/`raise_throw`), the reason-term mapping
(`exit_reason_from_term`/`reason_term_from_exit_reason`) and `TableRegistry`.

### `registry.rs` — the seam
```rust
/// A BIF implementation. Receives the calling context and the argument slice; the
/// registry guarantees `args.len()` equals the registered arity.
pub type BifFn = fn(&mut BifContext<'_>, &[Eterm]) -> Result<Eterm>;

pub struct BifRegistry { /* HashMap<(String, String, u32), BifFn> */ }

impl BifRegistry {
    pub fn new() -> Self;
    /// Every BIF the per-module `register` functions install (the 183-BIF roster below).
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

/// The registry with every core BIF registered (a `BifRegistry::with_core_bifs()`).
pub fn default_registry() -> BifRegistry;
```
- **Keying is `(module: &str, function: &str, arity: u32)`** — exactly how the loader/engine resolves
  a BIF from `Module.imports`, and the shape A.7 pins. Operator BIFs keep their *quoted* names
  (`"+"`, `"=<"`, `"=:="`, `"!"`, …), matching `bif.tab`.
- `with_core_bifs` calls each module's `pub(crate) fn register(reg)` in sequence (`arith`, `compare`,
  `type_test`, `term_ops`, `convert`, `lists`, `maps`, `binary`, `binconv`, `unicode`, `time`, `proc`,
  `reason`, `io`, `ets`).

### `context.rs` — what a BIF may touch
```rust
pub trait OutputSink { fn write_str(&mut self, s: &str) -> Result<()>; }
pub struct StdoutSink;   // writes to the process's stdout (the default)
pub struct NullSink;     // discards
pub struct BytesSink;    // captures (test/embedding): contents() -> &[u8], as_str() -> String, clear()

/// Delivers `term` to the process `to` (an internal pid). The runtime supplies this over
/// `beam_process`/`beam_message`; without it `send/2`/`"!"/2` raise `unsupported_bif`.
pub trait SendHook { fn send(&mut self, to: Eterm, term: Eterm) -> Result<()>; }

/// Interns a *new* atom name into the runtime's global atom table. The context borrows the atom
/// table immutably, so `list_to_atom`/`binary_to_atom`/`binary_to_existing_atom`/`binary_to_term`
/// use this hook to create a name that is not already interned; without it an unknown name reports
/// `BeamError::TermNotFound` (never a fabricated index).
pub trait AtomInterner { fn intern(&mut self, name: &str) -> Atom; }

pub struct BifContext<'a> { /* heap, atoms, self_pid, output, sender, tables, interner */ }
impl<'a> BifContext<'a> {
    pub fn new(heap: &'a mut beam_alloc::Heap, atoms: &'a AtomTable, self_pid: ProcessId) -> Self;
    pub fn with_output(self, sink: &'a mut dyn OutputSink) -> Self;      // default when unset: stdout
    pub fn with_sender(self, hook: &'a mut dyn SendHook) -> Self;        // default when unset: none
    pub fn with_tables(self, reg: &'a mut TableRegistry) -> Self;        // default when unset: none
    pub fn with_atom_interner(self, interner: &'a mut dyn AtomInterner) -> Self; // default when unset: none
    pub fn heap(&mut self) -> &mut beam_alloc::Heap;
    pub fn atoms(&self) -> &AtomTable;
    pub fn self_pid(&self) -> ProcessId;
    pub fn write_output(&mut self, s: &str) -> Result<()>;   // sink, or stdout
    pub fn sender(&mut self) -> Option<&mut (dyn SendHook + 'a)>;
    pub fn tables(&mut self) -> Option<&mut TableRegistry>;
    // constructors / writers (all fallible → Result)
    pub fn int(&self, v: i64) -> Result<Eterm>;          // try_make_small; out of 60-bit range ⇒ SystemLimit
    pub fn atom(&self, name: &str) -> Result<Eterm>;     // AtomTable::lookup; absent ⇒ TermNotFound
    pub fn intern_atom(&mut self, name: &str) -> Result<Eterm>; // lookup first, else the AtomInterner hook
    pub fn boolean(&self, b: bool) -> Result<Eterm>;     // atom("true"/"false")
    pub fn ok(&self) -> Result<Eterm>;                   // atom("ok")
    pub fn make_float(&mut self, v: f64) -> Result<Eterm>;
    pub fn tuple(&mut self, elems: &[Eterm]) -> Result<Eterm>;
    pub fn cons(&mut self, car: Eterm, cdr: Eterm) -> Result<Eterm>;
    pub fn list(&mut self, elems: &[Eterm]) -> Result<Eterm>;   // NIL for empty
}
```
- The four seams (`output`/`sender`/`tables`/`interner`) are **optional**: unset ⇒ `io:format` writes
  to stdout, `send/2`/`"!"/2` raise `unsupported_bif`, `ets:*` raises `unsupported_bif`, and a
  new-atom BIF reports `TermNotFound`.
- `intern_atom` resolves an already-interned name without the hook; only a *new* name needs it.

### `access.rs` — the crate's ONLY `#[allow(unsafe_code)]` module
`beam_term`'s raw-term accessors (`is_tuple`, `tuple_element`, `car`, `cdr`, `is_map`, …) are
`unsafe fn` (they dereference a header word). Following `beam_engine::predicates`, this module holds
the crate's single `#[allow(unsafe_code)]` and exposes safe wrappers; every other module is safe Rust.
The SAFETY invariant: every wrapped term is a **live BEAM term** (a BIF argument from the machine's
registers/stack, or a term built on a live heap) — never unvalidated raw memory.
Provides: classification (`is_atom`, `is_small`, `is_nil`, `is_list`, `is_tuple`, `is_float`,
`is_map`, `is_big`, `is_bitstring`, `is_binary`, `is_function`, `is_integer`, `is_number`, `is_pid`,
`is_port`, `is_reference`, `is_internal_*`/`is_external_*`), structure (`tuple_size`,
`tuple_element`, `car`, `cdr`), floats (`make_float`, `float_value`, `float_bits`), the M1 ref thing
(`make_ref_thing`/`ref_thing_id`, the ETS handle + read-back), the heap bitstring builder/readers
(`make_heap_bitstring`, `bitstring_bit_size`, `binary_bytes`), and flatmap accessors (`is_flatmap`,
`is_hashmap`, `make_flatmap`, `flatmap_pairs`).

### `error.rs`
`pub(crate)` constructors (`badarg`, `badarith`, `badarity`, `system_limit`, `undefined_bif`,
`unsupported_bif`) and the public raise builders `raise_error(reason)`, `raise_exit(reason)`,
`raise_throw(reason)` over `beam_common::{Raise, ExitReason, ExceptionClass}`.

### `reason.rs`
```rust
pub fn exit_reason_from_term(term: Eterm, atoms: &AtomTable) -> ExitReason;
pub fn reason_term_from_exit_reason(reason: &ExitReason, atoms: &mut AtomTable) -> Result<Eterm>;
```
`exit_reason_from_term` maps the well-known atoms (`normal`, `shutdown`, `noproc`, `noconnection`,
`killed`, `aborted`) to their dedicated `ExitReason` variants, any other atom to `Atom(name)`, and a
non-atom (or a name missing from `atoms`) to `ExitReason::Structured`.
`reason_term_from_exit_reason` rejects `Structured` with `BadArg` (it carries no term).

### `ets.rs` — the `ets:*` bridge over `beam_ets::Table`
```rust
pub struct TableRegistry { /* HashMap<u64, Table> */ }   // Default = empty
impl TableRegistry {
    pub fn new() -> TableRegistry;
    pub fn insert(&mut self, handle: u64, table: Table) -> Option<Table>;
    pub fn get(&self, handle: u64) -> Option<&Table>;
    pub fn get_mut(&mut self, handle: u64) -> Option<&mut Table>;
    pub fn contains(&self, handle: u64) -> bool;
    pub fn remove(&mut self, handle: u64) -> Option<Table>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
```
The handle is an on-heap `Ref` thing built by `access::make_ref_thing`
(`[make_header(3, REF_SUBTAG), 0, 0, id]`); `ref_thing_id` recovers its numeric id, which is the
registry key (the M1 substitution for a real `tid()`).

## The implemented BIFs (183)
`default_registry()` registers **183** BIFs (count the `reg.register(` calls across `src/*.rs`;
its `len()` is 183). Names/arities are verbatim from `bif.tab` / the stdlib export lists.

Module `erlang`:
- `arith.rs` (9): `"+"/2 "-"/2 "*"/2 "/"/2 "div"/2 "rem"/2 "abs"/1 "-"/1 "+"/1`.
- `compare.rs` (9): `"=="/2 "/="/2 "=:="/2 "=/="/2 "<"/2 ">"/2 "=<"/2 ">="/2`, plus
  `erts_internal:cmp_term/2`.
- `type_test.rs` (14): `is_atom/1 is_list/1 is_tuple/1 is_float/1 is_integer/1 is_number/1 is_pid/1
  is_port/1 is_reference/1 is_binary/1 is_bitstring/1 is_map/1 is_boolean/1 is_function/1`.
- `term_ops.rs` (9): `element/2 setelement/3 tuple_size/1 tuple_to_list/1 list_to_tuple/1 length/1
  hd/1 tl/1 size/1`.
- `convert.rs` (22): `integer_to_list/1,2 list_to_integer/1,2 float_to_list/1,2 list_to_float/1
  atom_to_list/1 list_to_atom/1 list_to_existing_atom/1 min/2 max/2 round/1 trunc/1 float/1 ceil/1
  floor/1 make_tuple/2,3 append_element/2 raise/3 node/0`.
- `binconv.rs` (24): `list_to_binary/1 binary_to_list/1,3 byte_size/1 bit_size/1 binary_part/2,3
  integer_to_binary/1,2 binary_to_integer/1,2 float_to_binary/1,2 binary_to_float/1 atom_to_binary/1,2
  binary_to_atom/1,2 binary_to_existing_atom/1,2 term_to_binary/1,2 binary_to_term/1,2`.
- `maps.rs` (3 of its 21): `map_get/2 map_size/1 is_map_key/2`.
- `time.rs` (9): `monotonic_time/0,1 system_time/0,1 time_offset/0,1 unique_integer/0,1 timestamp/0`.
- `proc.rs` (3): `self/0 send/2 "!"/2`.
- `reason.rs` (4): `error/1 error/2 exit/1 throw/1`.

Module `lists` (`lists.rs`, 25): `member/2 reverse/1,2 keyfind/3 append/1,2 nth/2 nthtail/2 last/1
sublist/2,3 seq/2,3 flatten/1,2 sum/1 max/1 min/1 duplicate/2 delete/2 keysort/2 sort/1 usort/1 zip/2
unzip/1`.
Only `member/2`/`reverse/2`/`keyfind/3` are real `bif.tab` `lists` BIFs; the rest are `lists.erl`
exports registered as Rust fast paths (identical observable results). `keymember/3` and
`keysearch/3` (the two other real `lists` BIFs) and the `fun`-taking functions are not registered.

Module `maps` (`maps.rs`, 18 of its 21): `new/0 from_list/1 to_list/1 get/2,3 find/2 put/3 update/3
remove/2 keys/1 values/1 size/1 is_key/2 merge/2 with/2 without/2 iterator/1 next/1`.

Module `binary` (`binary.rs`, 21): `at/2 part/2,3 copy/1,2 list_to_bin/1 bin_to_list/1,2,3
decode_unsigned/1,2 encode_unsigned/1,2 first/1 last/1 split/2,3 concat/2 match/2,3
longest_common_prefix/1`.

Module `unicode` (`unicode.rs`, 5): `characters_to_binary/1,2,3 characters_to_list/1,2`.

Module `io` (`io.rs`, 4): `io:format/1,2` and `io_lib:format/1,2` (one shared directive engine).

Module `ets` (`ets.rs`, 4): `new/2 insert/2 lookup/2 delete/1` over `TableRegistry`.

Module `erts_internal`: only `cmp_term/2` (above).

## `io`/`io_lib` directive set and the printer
`src/io.rs` (the exact support, verified against erts-17.1):
- Directives: `~w` (write), `~p` (print — the printable-string rule), `~W`/`~P` (`~w`/`~p` with an
  explicit *depth* argument), `~s` (string: an atom name, a proper list of byte-sized small integers,
  or a binary), `~c` (a character; `~tc` uses the full code point), `~n` (newline), `~~` (literal
  `~`), `~i` (consume and ignore one argument), `~b`/`~B` (base 10, or `~.<base>b`/`~.<base>B` for
  base 2–36), `~x`/`~X` (a two-argument `(Integer, Prefix)` integer), `~e`/`~f`/`~g` (floats).
- Control-sequence prefix: optional `-` (left adjust), decimal field width (or `*`), optional `.`
  precision (decimal or `*`), optional `.*` pad character, and the `t`/`l` modifiers.
- `io:format/1,2` write to the context's `OutputSink` and return `ok`; `io_lib:format/1,2` return the
  rendering as an **iolist** (a flat list of small ints; `NIL` for empty) and write nothing.
- Rejected (`badarg`): `~d` (not a directive in the oracle — `~b` is the base-10 form), `~+`, `~#`, a
  base outside `2..=36`, a too-small precision (`~.1e`, `~.0f`, `~.0g`), wrong arg type/count,
  running out of / leaving unconsumed arguments, a trailing `~`, a left-adjusted `~p`/`~-<n>p`, and a
  `~p`/`~P` whose single-line rendering would need re-flow. `~k`/`~K` are rejected (this printer has
  no map-printing-order switch).

`src/print.rs` (`~w`/`~p`/`~W`/`~P` renderer; registers no BIFs): small ints, atoms (bare iff
`[a-z][A-Za-z0-9_@]*`, else `'…'`), `NIL`, floats, proper/improper lists, tuples, flat maps,
binaries (each class as the oracle renders it). Depth semantics reproduce `io_lib:write1/4` (depth
`0` ⇒ `...`, depth `1` elides container contents, negative ⇒ unlimited). The `~p` printable-string
rule (a non-empty proper list of small ints in `32..=126` ⇒ `"…"`, escaping `"`/`\`; also applied to
binaries ⇒ `<<"…">>`) only under `printable = true`. Maps render `#{k => v,…}` in the oracle's
flatmap key order.

## Notes for Agents
- **`access.rs` and `bits.rs` are PRIVATE modules** (`mod access;`/`mod bits;` in `src/lib.rs:37,41`) — their `pub(crate)` safe wrappers are *not* reachable from other crates (e.g. `beam_rt`). The crate's cross-crate public surface is exactly the `pub use` list in `src/lib.rs:60-66` + `prelude` (`BifContext` and its seams, `BifRegistry`/`BifFn`/`default_registry`, the raise builders, `TableRegistry`, the reason mapping). There is **no public term construction/destruction helper module**.
- **A dependent needing to destructure/construct boxed terms from safe Rust has two routes outside this crate:** (a) call the registered BIFs through `BifRegistry::call` (`hd`/`tl`/`element`/`map_get`/`binary:at`/`is_tuple`/`is_map`/`is_float`/`maps:from_list`/`list_to_binary`/`binary:copy`/`float`/`erlang:'/'` are all registered); or (b) use the sibling **safe** helper modules `beam_engine::terms::*` (`make_tuple`/`make_cons`/`make_list`/`make_float`/`make_flatmap`/`make_heap_binary`/`make_fun`, `tuple_arity`/`tuple_element`/`cons_head`/`cons_tail`/`list_elements`/`float_value`/`map_entries`/`binary_bytes`, fun readers `is_fun`/`fun_arity`/`fun_num_free`/`fun_uniq`/`fun_index`/`fun_free`; take `&mut dyn ExecutionContext`) and `beam_codec::term::*` (`make_float`/`make_big`/`make_flatmap`/`make_binary`/`make_bitstring`, `flatmap_parts`/`tuple_arity`/`tuple_element`/`cons_head`/`cons_tail`/`bitstring_bytes`/`float_value`; take `&mut Heap`). Both are `#[allow(unsafe_code)]` modules exposing safe `Result` APIs; `beam_term`'s own boxed accessors are all `unsafe fn` with no safe wrapper.
- `BifContext` constructs **floats, lists, tuples, cons, small ints, atoms** safely (`make_float`/`list`/`tuple`/`cons`/`int`/`atom`/`intern_atom`) but has **no** binary/bitstring or map constructor — build those via the routes above.

## Constraints
- **Errors:** `beam_common::{Result, BeamError}` only — never a crate-local error enum, never
  `panic!`, never `unwrap()` on runtime data, never a wrong result. Argument violations ⇒ `BadArg`;
  non-numeric arithmetic ⇒ `BadArith`; wrong arg count ⇒ `BadArity`; 60-bit overflow ⇒ `SystemLimit`.
- **Never re-implement term order/size/hashing/construction.** Comparison ⇒ `beam_term::compare::{cmp,
  cmp_arith}`; term construction/inspection ⇒ `beam_term` accessors only (via `access.rs`);
  allocation ⇒ `beam_alloc::Heap`.
- **`unsafe` confinement.** The crate is `#![deny(unsafe_code)]`; `src/access.rs` is its **only**
  `#[allow(unsafe_code)]` module (the `beam_engine::predicates` convention). `unsafe_op_in_unsafe_fn`
  stays clean.
- **Deps (fixed by `Cargo.toml`):** `beam_common`, `beam_term`, `beam_alloc`, `beam_process`,
  `beam_message`, `beam_codec`, `beam_ets`, `beam_gc`, `libc`. In use: `beam_common`, `beam_term`,
  `beam_alloc`, `beam_process`, `beam_ets`, `beam_message` (the `ets` copy-out), `beam_codec` (the
  `term_to_binary`/`binary_to_term` image and `print`'s bignum reader). `beam_gc`/`libc` remain
  declared seams. Adding a dependency edge requires reporting to `../`.
- **Files target ~1000 lines.** Several `src/*.rs` files exceed 1000 total *only* because of their
  comprehensive in-module `#[cfg(test)]` suites; each file's implementation section is under ~1300
  lines (the documented "legitimately long file" exception). Split by responsibility when the
  implementation itself grows.
- Licensing Apache-2.0.

## Test Strategy
- Per-module `#[cfg(test)]` unit tests build a `BifContext` over a real `beam_alloc::Heap` + a seeded
  `beam_term::AtomTable` (the well-known atoms `true`, `false`, `ok`, …) and a `BytesSink`/`NullSink`
  capture. `cargo test -p beam_bifs` runs **216** lib tests (units + in-module integration).
- **`tests/` integration suite** (public API only — `beam_bifs::*` plus the declared dependency
  crates); 16 tests total:
  - `tests/registry.rs` (4) — the `BifRegistry` seam: `default_registry()` is a **superset** of the
    54-key Milestone-1 core roster (`len() >= 54`; every core key present) with `names()` sorted and
    `entries().count() == len()`, a fresh registry is empty, `register` returns the replaced impl,
    `lookup` is exact on `(module, function, arity)`, and `call` reports an unknown BIF (`undef`
    raise) and a bad arity (`BadArity`).
  - `tests/erlang_bifs.rs` (8) — the `erlang`/`erts_internal` core BIFs driven through `call`
    (arithmetic, comparison incl. `1 == 1.0` vs `1 =:= 1.0` and `erts_internal:cmp_term/2`, type
    tests incl. `is_list([])`, term ops incl. their `badarg` cases), `self/0` + `send/2`/`"!"/2` (the
    `SendHook` seam), the raise BIFs (`error`/`exit`/`throw` classes) and the public reason mapping
    (`exit_reason_from_term`/`reason_term_from_exit_reason`).
  - `tests/io_ets.rs` (4) — `io:format` **exact captured bytes** through a `BytesSink` (`"~p~n"` ⇒
    `{a,1}\n`, `"~w ~s ~b~n"` ⇒ `[97,98,99] a 255\n`), `~d`/`~x` ⇒ `BadArg`, and the
    `ets:new/insert/lookup/delete` round-trip over a real `TableRegistry` (structural `compare::eq`,
    including the empty-list miss and the `badarg` after delete).
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
- **`BifContext` holds optional seams (`output`/`sender`/`tables`/`interner`), not concrete runtime
  types.** The engine depends only on `beam_loader`; `beam_rt` supplies the concrete context. Optional
  seams keep the crate testable with a bare heap + atom table and keep the Stage-1/Stage-5 DAG
  acyclic (no `beam_engine` edge).
- **`AtomInterner` exists because atom indices are global and owned by the runtime.** The context
  borrows the atom table immutably, so a BIF that must *create* an atom routes through the hook;
  without one it resolves an existing name or reports `TermNotFound` — never a fabricated index.
  `binary_to_term` decodes into a clone of the context's table and hands the decoder's appended names
  back through `intern_atom` (sound because `AtomTable` assigns indices in insertion order).
- **`self/0` reads the pid from the context, not a global.** There is no ambient "current process" in
  the Rust core (the engine is a pure state machine); `BifContext::self_pid` is the injected identity.
- **All BIF allocation uses the upstream `beam_alloc::Heap`.** `beam_term::heap::Heap` is a test arena
  only (see `../beam_alloc/CONTEXT.md`).
- **Comparison delegates to `beam_term`.** `==`/`/=` and `< > =< >=` use `cmp_arith` (arithmetic
  order); `=:=`/`=/=` and `erts_internal:cmp_term/2` use `cmp` (exact order) — the OTP semantics
  (`1 < 1.0` is false; `1 =:= 1.0` is false).
- **`lists` comparison order is per function, pinned to the oracle.** Exact (`eq`): `member/2`,
  `delete/2`. Arithmetic (`cmp_arith`): `keyfind/3`, `sort/1`, `usort/1`, `keysort/2`, `max/1`,
  `min/1`. `sort/1`/`usort/1`/`keysort/2` are stable; `usort/1` removes the later of each
  arithmetically-equal run.
- **`maps` key identity is exact (`=:=`).** `#{1.0 => a}` and `#{1 => a}` are distinct maps, and
  iteration/keys/values/to_list return the *stored* order (OTP documents map order as unspecified).
- **The `ets` handle is an on-heap `Ref` thing.** A documented M1 substitution for a real `tid()`,
  mirroring `beam_process::MonitorRef::to_eterm`; when `beam_term` gains `make_ref`, only the handle
  constructor changes. `insert`/`lookup` take the table *out* of the registry (releasing the
  `ctx.atoms()`/`ctx.tables()` double borrow), operate on the owned table, then restore it — safe
  because a `beam_ets::Table`'s storage is a never-relocating bump arena.
- **`bits.rs` owns the safe on-heap bitstring API** (`ErlHeapBits`, the frozen `[header, size, data…]`
  block), layering byte/bit semantics on top of `access`'s raw words; the `binary`/`unicode`/`size`
  BIFs are its consumers. Every binary M1 builds stays on the heap (no off-heap `BinRef`/`SUB_BITS`).
- **`io:format` and the directive set follow the oracle, not the objective's example list.** `~b` is
  base 10 and `~d` is `badarg` in real erts-17.1; `~t` is the unicode *modifier* (there is no column
  directive); `float_to_binary/3` is not registered (the oracle has only `/1`,`/2`). Reproducing the
  oracle wins over illustrative wording.
- **Float rendering** is Rust's shortest round-trip with a `.0` appended when integral (a documented
  deviation from OTP's `%.*g` / `[short]`).

## Known Issues / Intentional M1 gaps
Every gap below is a typed `BeamError`, never a silently wrong result. The recurring cause is
`beam_common::ExitReason` carrying a non-atom reason only as `ExitReason::Structured` (no term), and
`beam_term` having no bignums / no off-heap binaries / no `function_clause` variant.
- **No bignums (everywhere).** An integer result outside the 60-bit small range is `SystemLimit`
  (`arith`, `lists`, `convert`, `binconv`, `binary:decode_unsigned`, `time`); a bignum operand is
  `BadArg`/`BadArith`. Real Erlang promotes to a bignum.
- **`function_clause` collapses to `BadArg`.** `lists.erl`'s no-clause failures (`nth(3,[a,b])`,
  `last([])`, `zip([1],[2,3])`, `sublist([1],0,1)`, …) are reported as `BadArg` (no
  `function_clause` in `BeamError`).
- **Structured `error:{badkey,..}` / `error:{badmap,..}` reasons collapse to `BadArg`.** `maps`'s
  missing-key / non-map failures and `convert:raise/3` with a non-atom reason are `BadArg` rather
  than the structured term the oracle raises (the `Structured`-has-no-term gap).
- **`reason_term_from_exit_reason(&Structured)` is `BadArg`** (no term to reproduce).
- **Flat maps only.** A map with more than `MAP_SMALL_MAP_LIMIT` (32) pairs is `SystemLimit`
  (`maps`); no M1 code can build a HAMT hashmap, and reading an existing `is_hashmap` operand is
  likewise `SystemLimit` (defensive).
- **`maps:iterator/1` yields the map's `{Key, Value}` list**, not an opaque iterator (`next/1` then
  peels one pair, returning `{Key, Value, Next}` or `none`).
- **`beam_codec` cannot encode or decode a binary** (`BINARY_EXT`/`BIT_BINARY_EXT`): `term_to_binary`
  of a term containing a binary and `binary_to_term` of an image containing one report the codec's
  `unsupported_external_term` error.
- **`beam_codec` encodes only external pids/ports/refs.** `term_to_binary(self())` reports
  `unsupported` instead of `NEW_PID_EXT`.
- **`term_to_binary/2` `compressed`/`{compressed, Level}` raises `error:unsupported_bif`** (no zlib).
  `{minor_version, 0|1}` is validated but has no effect (the image still decodes identically).
- **`float_to_list/2` and `float_to_binary/2` option `short` is `badarg`** (Ryu's shortest round-trip
  is unimplemented); `{decimals, N}`/`{scientific, N}`/`compact` are supported.
- **`unicode` implements `latin1` and `unicode`/`utf8` only** (input and output, all registered
  arities); `utf16`/`utf32` (and their `little`/`big` forms) report `BadArg`.
- **Sub-binaries are copied, not shared.** `binary:part`/`binary_part`/`binary:copy` return a fresh
  on-heap binary; M1 has no off-heap `BinRef`/`SUB_BITS`.
- **Compiled binary match patterns are unsupported** (`{bm,Ref}`/`{am,Ref}` ⇒ `BadArg`); the endian
  atom is `big`/`little` only (`native` is `BadArg`, matching the oracle).
- **`binary:copy/2` cannot outgrow the destination heap** (a too-large result is `SystemLimit`); the
  same convention `convert::make_tuple` uses.
- **The `~p` printer never line-breaks** (the oracle's `io_lib_pretty` re-flow is unimplemented);
  `io:format` rejects a `~p`/`~P` whose single-line form would need re-flow. Binary/atom/sub-binary
  edge cases: non-byte-aligned bitstrings are `BadArg` (no bit-level reader); quoted atoms are not
  re-escaped; non-ASCII output is UTF-8, not raw latin1 (the ASCII subset is byte-exact).
- **`float_to_binary/3` does not exist** in erts-17.1, so it is not registered (deliberate — see
  Design Decisions).
- **`list_to_integer/1,2` requires byte-sized list elements** (`0..=255`); erts truncates each element
  to a byte.
- **`list_to_atom`/`binary_to_atom`/`binary_to_existing_atom` need the `AtomInterner` hook to create
  an atom**; without it an unknown name is `TermNotFound` (`beam_rt` installs one).
- **`prim_*` modules are unimplemented.** The registry supports them; nothing is registered. Only
  `erts_internal:cmp_term/2` exists among the `erts_internal` surface.
- **Process-coupled BIFs are NOT implemented:** no `apply/3`, `spawn*`, `process_info`,
  `process_flag`, `get`/`put`/`erase`, `make_ref`, `list_to_ref`, `fun`-taking `lists`/`maps` BIFs
  (`map/2`, `filter/2`, `foldl/3`, `foldr/3`, `sort/2`, `maps:fold/3`, …). They need an apply/callback
  seam and are left to the interpreted stdlib / a later pass.
- **No in-crate message delivery.** `send/2`/`"!"/2` require a `SendHook`; without one they raise
  `unsupported_bif`. Wiring to `beam_process`/`beam_message` is `beam_rt`'s.
- **`error/2` ignores its second argument** (behaves exactly like `error/1`); the argument list is not
  rendered into the reason.
- **`time` native unit is fixed to nanoseconds.** `system_time/0`, `time_offset/0`,
  `system_time(nanosecond)` and `system_time(native)` exceed the 60-bit small range and report
  `SystemLimit` (real ERTS returns a bignum); `monotonic_time/0` uses a process-global `Instant`
  origin (arbitrary, as the oracle's absolute value is opaque); `monotonic`/`positive`
  `unique_integer` values are `>= 0` (the oracle's non-positive space returns negatives).
- **`ets` bridge is minimal:** the option subset `set`/`ordered_set`/`bag`/`duplicate_bag`,
  `public`/`protected`/`private`, `named_table`, `{keypos,N}`; `delete/1` removes the **whole** table
  (there is no `delete/2`); access control/ownership are not enforced here (`beam_ets`'s own gaps);
  the table id is a synthesized `Ref` thing.
- **Registry lookup allocates a key tuple per call** (`HashMap<(String,String,u32), _>`); correctness
  does not depend on interning module/function ids.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`, `#![deny(unsafe_code)]`
- `src/registry.rs` → `BifFn`, `BifRegistry` (register/lookup/call/names/entries), `with_core_bifs`, `default_registry`
- `src/context.rs` → `BifContext` + `AtomInterner`/`OutputSink`/`SendHook` and the sink impls (`StdoutSink`/`NullSink`/`BytesSink`)
- `src/error.rs` → `pub(crate)` failure constructors + the public `raise_error`/`raise_exit`/`raise_throw` builders
- `src/access.rs` → the ONLY `#[allow(unsafe_code)]` module: term classification/structure/float wrappers, the M1 ref thing, heap-bitstring builders, flatmap accessors
- `src/arith.rs` → `+ - * / div rem abs` and unary `-`/`+` BIFs
- `src/compare.rs` → `== /= =:= =/= < > =< >=` BIFs + `erts_internal:cmp_term/2`
- `src/type_test.rs` → the `is_*` type-test BIFs
- `src/term_ops.rs` → `element setelement tuple_size tuple_to_list list_to_tuple length hd tl size`
- `src/convert.rs` → pure `erlang` conversions/numeric helpers (`integer_to_list`, `list_to_integer`, `float_to_list`, `min`/`max`, `round`/`trunc`/`float`/`ceil`/`floor`, `make_tuple`, `raise`, `node`, …)
- `src/lists.rs` → the `lists` module BIFs/fast paths (no `fun`-taking functions)
- `src/maps.rs` → the `maps` module BIFs + `erlang:map_get`/`map_size`/`is_map_key`
- `src/binary.rs` → the `binary` module BIFs (`at part copy list_to_bin bin_to_list decode/encode_unsigned first last split concat match longest_common_prefix`)
- `src/binconv.rs` → `erlang` binary/atom/integer conversions + `term_to_binary`/`binary_to_term` (via `beam_codec`)
- `src/unicode.rs` → `unicode:characters_to_binary/1,2,3` + `characters_to_list/1,2` (latin1 + utf8)
- `src/time.rs` → `monotonic_time`/`system_time`/`time_offset`/`unique_integer`/`timestamp`
- `src/proc.rs` → `self/0`, `send/2` (`"!"/2`)
- `src/reason.rs` → the `error`/`exit`/`throw` BIFs + `ExitReason` ↔ reason-term mapping
- `src/io.rs` → `io:format/1,2` + `io_lib:format/1,2` + the directive engine
- `src/print.rs` → the `~w`/`~p`/`~W`/`~P` term printer (registers no BIFs)
- `src/bits.rs` → the safe on-heap bitstring/binary facade shared by `binary`/`unicode`/`term_ops`
- `src/numeric.rs` → shared `pub(crate)` numeric helpers (int/float parse + render, base/digit handling, `FloatFormat`/`parse_float_options`) used by `convert.rs` and `binconv.rs`
- `src/ets.rs` → `TableRegistry` + the `ets:new/insert/lookup/delete` bridge over `beam_ets::Table`
- `tests/registry.rs` → the `BifRegistry` seam (core-roster superset, unknown/bad-arity contract)
- `tests/erlang_bifs.rs` → the `erlang`/`erts_internal` BIF sweep + `SendHook` + reason mapping
- `tests/io_ets.rs` → `io:format` exact bytes + the `ets` bridge round-trip
- `../beam_engine/CONTEXT.md` → the `ExecutionContext::call_bif` seam this registry plugs into (read-only)
- `../beam_process/CONTEXT.md` → `ProcessId`, the exit-reason handoff (read-only)
- `../beam_ets/CONTEXT.md` → `Table`/`TableOptions`/`TableId` the `ets` bridge drives (read-only)
- `../beam_codec/CONTEXT.md` → the ETF codec `term_to_binary`/`binary_to_term` route through (read-only)
- `../../erts/emulator/beam/bif.tab` / `bif.c` / `erl_bif_*.c` / `erl_arith.c` / `erl_map.c` / `external.c` / `erl_unicode.c` / `erl_time_sup.c` / `erl_bits.h` → authoritative names/arities/semantics (read-only)

## See Also
- `../CONTEXT.md` → crate-layer contracts, the `BifRegistry`+`BifFn` seam, the wave-5 dependency position.
- `../../CONTEXT.md` → workspace contracts (§1 A.7, §2 frozen term layout, §3 error strategy, §6 test gate).
- `../../ERTS_RUST_REWRITE_EVALUATION.md` §A.7 → the OTP app contracts (preloaded/boot/`erts_internal`/`prim_*`).
- `../beam_term/CONTEXT.md` → the frozen term API + the shared `compare`/`size`/`thing` utilities.
- `../beam_rt/` → the runtime that supplies the concrete `BifContext` and adapts the registry into the engine.
