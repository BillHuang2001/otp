# `beam_term` — Frozen BEAM Term Representation

## Intent
Owns the **frozen, bit-exact tagged term representation** of the Rust ERTS rewrite: the single most
load-bearing shared contract in the workspace (workspace `CONTEXT.md` §2). Implements Part I §A.1–A.2
(language semantics + `.beam` data model) and the term-layout discussion of Part II §B, at **Level-1
drop-in fidelity**: a Rust core and the retained C periphery must not differ by one bit.

Ground truth = the legacy C (`../../erts/emulator/beam/erl_term.h`, `erl_term.c`); this crate reproduces
that layout exactly. Every other crate depends downward on this one for term construction, inspection
and classification. This node is the lowest common ancestor for term operations: **no other crate may
assume a different layout.**

## API Surface (FROZEN — every downstream crate routes here)

`Eterm` is a `#[repr(transparent)]` newtype over `usize`, ABI-identical to a NIF's `ERL_NIF_TERM`
(both are one machine word). `beam_abi` will transmute between them; no other crate may redefine it.

### crate root (`src/lib.rs`)
`pub mod {tag, eterm, immediate, atom, list, boxed, tuple, heap, literal, ops, error}` plus
`pub mod prelude` and flat `pub use` re-exports of every public item below, so downstream code can
`use beam_term::prelude::*;`.

### `eterm.rs` — the word type
- `pub struct Eterm(usize)` — `#[repr(transparent)]`; `Copy + Clone + PartialEq + Eq + Hash + Debug`.
- `pub const fn Eterm::raw(self) -> usize`
- `pub const fn Eterm::from_raw(raw: usize) -> Eterm`
- `impl Default for Eterm` → `NIL` (matches BEAM's "blank = NIL" init convention).
- `pub const fn Eterm::nil() -> Eterm`; `pub const fn Eterm::non_value() -> Eterm`.

### `tag.rs` — raw tag constants, masks, shifts, enums
- Primary: `TAG_PRIMARY_MASK=0x3`, `TAG_PRIMARY_HEADER=0x0`, `TAG_PRIMARY_LIST=0x1`,
  `TAG_PRIMARY_BOXED=0x2`, `TAG_PRIMARY_IMMED1=0x3`.
- Pointer: `TAG_PTR_MASK=0x7` (64-bit), `TAG_LITERAL_PTR=0x4`.
- IMMED1 (mask `0xF`): `TAG_IMMED1_SIZE=4`, `TAG_IMMED1_PID=0x3`, `TAG_IMMED1_PORT=0x7`,
  `TAG_IMMED1_IMMED2=0xB`, `TAG_IMMED1_SMALL=0xF`.
- IMMED2 (mask `0x3F`): `TAG_IMMED2_SIZE=6`, `TAG_IMMED2_ATOM=0xB`, `TAG_IMMED2_CATCH=0x1B`,
  `TAG_IMMED2_NIL=0x3B`.
- Headers: `TAG_HEADER_MASK=0x3F`, `HEADER_ARITY_OFFS=6`, `HEADER_SUBTAG_MASK=0x3C`,
  `MAX_ARITYVAL=(1<<24)-1`.
- `pub enum PrimaryTag { Header, List, Boxed, Immed1 }`
- `pub enum BoxedTag { ArityVal=0x00, PosBig=0x08, NegBig=0x0C, Ref=0x10, Fun=0x14, Float=0x18,
  Record=0x1C, HeapBits=0x20, SubBits=0x24, BinRef=0x28, Map=0x2C, ExternalPid=0x30,
  ExternalPort=0x34, ExternalRef=0x38 }` (+ `BoxedTag::from_raw(subtag)`).
- `pub const fn primary_tag(Eterm) -> PrimaryTag`; `pub const fn is_header(Eterm) -> bool`.

### `immediate.rs` — immediates
- Small ints: `SMALL_BITS=60`, `MAX_SMALL=(1<<59)-1`, `MIN_SMALL=-(1<<59)`;
  `is_immediate`/`is_not_immediate`; `is_small`/`is_not_small`; `make_small(i64) -> Eterm` (unchecked,
  `debug_assert` in range); `try_make_small(i64) -> TermResult<Eterm>`; `unsigned_value(Eterm) -> u64`;
  `signed_value(Eterm) -> i64`; `is_byte(Eterm) -> bool`.
- Atoms (immediate form): `is_atom`/`is_not_atom`; `make_atom(Atom) -> Eterm`; `atom_val(Eterm) -> Atom`.
- NIL: `pub const NIL: Eterm` (= `0x3B`); `is_nil`/`is_not_nil`.
- Catch: `make_catch(u64)`, `is_catch`, `catch_val(Eterm) -> u64`.
- Pid/port immediates: `is_internal_pid`, `make_pid_data(ser,num)`, `pid_number`, `pid_serial`;
  `is_internal_port`, `make_port_data(num)`, `port_number`.

### `atom.rs` — atom indices + interning table
- `pub struct Atom(u32)` — `Copy + Clone + PartialEq + Eq + Hash + PartialOrd + Ord + Debug`.
- `pub const ATOM_INDEX_BITS: u32 = 6`; `pub const MAX_ATOM_INDEX: usize = 2^58-1` (representation
  ceiling); `Atom::MAX` (= `u32::MAX`, the practical ceiling).
- `pub struct AtomTable` — deterministic: **indices are assigned in insertion order starting at 0**,
  reproducing the `atom.names` (`../../erts/emulator/beam/atom.names`) ordering so preloaded modules
  resolve correctly. One index per distinct name.
- `AtomTable::new() -> AtomTable`; `AtomTable::intern(&mut self, name: &str) -> Atom`;
  `lookup(&self, name: &str) -> Option<Atom>`; `name(&self, Atom) -> Option<&str>`; `len`, `is_empty`.
- `AtomTable::seed(&mut self, ordered_names: impl IntoIterator<Item = &str>)` — pre-intern in order.
- `AtomTable::seed_from_atom_names(&mut self, src: &str) -> TermResult<()>` — parse the `atom.names`
  grammar (`atom` <name>+ ; names are bare, `'quoted'`, or `CName='printname'`; `#` comments) and
  intern in file order. `from_atom_names(src) -> TermResult<AtomTable>` convenience.
- Note: generating the standard `am_*`-equivalent `Atom` constants from the full `atom.names` is a
  codegen/boot step (routed to `beam_rt`); this crate owns only the mechanism + parser.

### `list.rs` — cons cells (`[CAR,CDR]`, header-less, `TAG_PRIMARY_LIST`)
- `make_list(*const Eterm) -> Eterm`; `list_val(Eterm) -> *mut Eterm`; `ptr_val(Eterm) -> *mut Eterm`
  (generic tagged pointer, masks `TAG_PTR_MASK` incl. `TAG_LITERAL_PTR`).
- `is_list`/`is_not_list`; `is_not_nil_list` (= `is_list`: NIL is an immediate, not a LIST word).
- `car(Eterm) -> Eterm`; `cdr(Eterm) -> Eterm` (raw 2-word cell access); `CAR_OFFSET=0`, `CDR_OFFSET=1`.

### `boxed.rs` — boxed pointers, header words, boxed sub-types
- `make_boxed(*mut Eterm) -> Eterm`; `boxed_val(Eterm) -> *mut Eterm`; `is_boxed`/`is_not_boxed`.
- Headers: `make_header(size, subtag)`, `header_subtag(Eterm) -> u32`, `header_arity(Eterm) -> usize`,
  `thing_arityval(Eterm) -> usize`.
- Arityval (tuple header): `make_arityval(usize) -> Eterm`, `make_arityval_zero()`,
  `make_arityval_unchecked(usize)`, `try_make_arityval(usize) -> TermResult<Eterm>`, `arityval(Eterm) -> usize`,
  `is_arity_value`, `is_sane_arity_value`.
- Sub-type predicates: `is_map`/`is_map_header`/`is_flatmap`/`is_hashmap`, `is_float`, `is_big`,
  `is_internal_ref`, `is_any_fun`, `is_record`, `is_bitstring`/`is_binary`,
  `is_external_pid`/`is_external_port`/`is_external_ref`.
- Map header field accessors: `MAP_HEADER_TYPE/ARITY/VAL(Hdr)` (per `erl_term.h`).

### `tuple.rs` — tuples
- `make_tuple(*mut Eterm) -> Eterm`; `tuple_val(Eterm) -> *mut Eterm`; `is_tuple`/`is_not_tuple`;
  `is_tuple_arity(Eterm, usize) -> bool`; `tuple_size(Eterm) -> usize`;
  `tuple_element(Eterm, usize) -> Eterm`; `try_tuple_element(Eterm, usize) -> TermResult<Eterm>`.
- `pub fn tuple0() -> Eterm` — the shared, `TAG_LITERAL_PTR`-tagged empty-tuple literal (backed by a
  `LazyLock` over one leaked literal word). Zero-arity heap tuples are discouraged.

### `heap.rs` — term backing store (bump arena)
The allocation interface this representation is built on. **Minimal and `unsafe`-marked by design**;
`beam_alloc` (production allocators/carriers) and `beam_gc` (per-process copying GC) extend/replace it —
see Design Decisions. Never reallocates, so handed-out pointers stay valid for the arena's lifetime.
- `pub struct Heap`; `Heap::new(capacity_words) -> Heap`; `capacity()`, `used()`, `available()`,
  `reset()`, `hp() -> *mut Eterm`, `hend() -> *mut Eterm`.
- `unsafe fn alloc_words(&mut self, n: usize) -> TermResult<*mut Eterm>` (uninitialised, contiguous).
- `cons(&mut self, car, cdr) -> TermResult<Eterm>`; `tuple(&mut self, elems: &[Eterm]) -> TermResult<Eterm>`;
  `unsafe fn alloc_tuple(&mut self, n: usize) -> TermResult<*mut Eterm>`.

### `literal.rs` — off-heap literal areas (`TAG_LITERAL_PTR`)
- `make_literal(Eterm) -> Eterm`; `is_literal(Eterm) -> bool`; `clear_literal(Eterm) -> Eterm`.
- `unsafe fn set_literal_tag(term: *mut Eterm, hp_start: *mut Eterm, hsz: usize)` — the
  `erts_set_literal_tag` graph walk (ORs `TAG_LITERAL_PTR` into every BOXED/LIST word + the root).
- `pub struct LiteralArea` — a bump-allocated, frozen term block (`start`/`end`/`off_heap`), with
  `alloc_words`, `contains(ptr)`, `is_literal(ptr)` (pointer-range test).

### `ops.rs` — cross-cutting classifiers
- `pub enum TermClass { Bitstring, List, Nil, Map, Record, Tuple, Pid, ExternalPid, Port,
  ExternalPort, Fun, Ref, ExternalRef, Atom, Float, Big, Small, BinRef }` — mirrors the C `*_DEF`
  virtual tag values (term order: number < atom < ref < fun < port < pid < tuple < record < map <
  nil < cons < binary) used by comparison/ordering and `terms` code.
- `class(Eterm) -> TermClass` (the `tag_val_def` equivalent).
- `is_integer(Eterm)` (= small ‖ big); `is_number(Eterm)` (= integer ‖ float).
- `THE_NON_VALUE: Eterm` (= `0` in release, `make_header(0, Float)` in debug); `is_non_value`,
  `is_value`, `is_zero_sized`.

## Constraints
- **Bit-exact layout (Level-1 drop-in).** Every constant, mask, shift and encoding must match
  `../../erts/emulator/beam/erl_term.h` exactly; a change here breaks the frozen ABI and every crate.
- **Dependencies:** `beam_common` + `libc` only (this crate is a foundation dependency sink). Nothing
  else. `beam_common` is used for exactly one thing: the `From<TermError> for beam_common::BeamError`
  impl in `error.rs` (owned here so every dependent gets `?`-interop for free); `TermError`/
  `TermResult` remain this crate's error types and must not be redefined per dependent.
- **`unsafe` confinement:** confined to `heap`, `literal`, `list`, `boxed`, `tuple` (raw pointer
  deref). Every `unsafe fn` documents a `# Safety` section; every `unsafe` block carries a
  `// SAFETY:` rationale. `unsafe_op_in_unsafe_fn` stays clean (workspace lint).
- **Pure tag math is `const fn`** where the body allows (`raw`, `from_raw`, `primary_tag`, tag tests,
  `make_small`, `signed_value`, `make_atom`, `atom_val`, `make_arityval`, `arityval`, …) — these are
  hot in the engine.
- Files target ~1000 lines; one responsibility per file.

## Design Decisions
- **`Eterm` = `#[repr(transparent)]` newtype over `usize`**, not a raw `usize` alias: gives type
  safety, methods, and a documented ABI contract while remaining layout/pass-identical to
  `ERL_NIF_TERM` (Level-1 drop-in requires this — Part I §A.3).
- **Checked + unchecked constructors.** `make_small`/`make_arityval` are unchecked (they are the
  engine hot path, mirroring C), but `try_*` variants return `TermResult` for use at trusted
  boundaries (arithmetic BIFs, loader, `beam_alloc`) so range violations become errors, not silent
  corruption (workspace error strategy §3: no silent swallowing).
- **`Atom` is `u32`.** The representation allows 58-bit atom indices; the practical atom table is
  capped far below 2^32 (default `+t` limit 1,048,576), so a 32-bit index is compact and sufficient.
  Atoms are only ever produced via `make_atom(Atom)`, so the encoding invariant always holds.
- **`AtomTable` assigns indices in insertion order (0-based).** This reproduces `atom.names` exactly
  (verified against `../../erts/emulator/utils/make_tables`: `#define am_<alias> make_atom($i)` with
  `$i` the 0-based file order), which is required for preloaded modules that reference atoms
  positionally.
- **`heap::Heap` is a minimal term-construction arena, NOT the production allocator.** The workspace
  routing table assigns the production per-process heap / bump allocator to `beam_alloc`. This module
  exists (per the node objective) only so the representation has a concrete, testable backing store
  for constructing boxed/list terms; `beam_alloc`/`beam_gc` build the real thing on top. It never
  reallocates (fixed capacity) so interior pointers never dangle.
- **`TUPLE0` is `tuple0()`, a function, not a `const`.** The spec asked for `pub const TUPLE0: Eterm`,
  but a shared empty-tuple literal must carry `TAG_LITERAL_PTR`, which requires forming a pointer to a
  static block — and Rust rejects pointer→integer casts during const evaluation. It is therefore
  exposed as `tuple0()`. Same root cause (no const pointer arithmetic): `make_boxed`/`make_list`/
  `make_tuple` are plain `fn`, not `const fn`.

## Test Strategy
- Per-module `#[cfg(test)] mod tests` unit tests.
- `tests/layout.rs` — **pins the exact raw bit patterns** against `erl_term.h` (e.g. `make_small(0)
  == 0xF`, `make_atom(0) == 0xB`, `NIL == 0x3B`, `make_arityval(1) == 0x40`, order-`List` cons tag
  `0x1`), plus boundary sweeps: small-int min/max/±1/0, `arityval` at `MAX_ARITYVAL`, literal-bit
  masking via `boxed_val`/`list_val`. Prefer property-style loops over boundary ranges.
- `tests/abi.rs` — `size_of::<Eterm>() == size_of::<usize>()`, `align_of` equal, `Eterm: Copy`, and a
  `repr(transparent)` assertion (round-trip `Eterm`) — the NIF-ABI guarantee.
- Behavioural tests: atom interning/lookup/name round-trip + `atom.names` parser on an embedded
  snippet (order + `CName='printname'` + quoted names), heap cons/tuple construction + car/cdr +
  exhaustion, literal-graph tagging walk (`set_literal_tag` on a nested tuple/list).
- Exhaustive cross-checks against the real system ERTS belong to `tools/differential` (the
  compatibility gate), not to this crate's unit tests.
- Iteration commands (read-only `$HOME`): `../../scripts/cargo.sh test -p beam_term` (GREEN),
  `../../scripts/cargo.sh build -p beam_term`. Never `cd`.

## Routing Table
- `src/tag.rs` → raw tag constants/masks/shifts, `PrimaryTag`/`BoxedTag`, `primary_tag`, `is_header`
- `src/eterm.rs` → the `Eterm` word newtype (`raw`/`from_raw`), ABI contract
- `src/immediate.rs` → small ints, atom immediates, NIL, catch, pid/port immediates
- `src/atom.rs` → `Atom` index type, `AtomTable` (interning, `atom.names` seeding/parser)
- `src/list.rs` → cons-cell construction/access (`make_list`, `car`, `cdr`, `is_list`)
- `src/boxed.rs` → boxed pointers, header words, arityval, boxed sub-type predicates, map headers
- `src/tuple.rs` → tuple construction/size/element access
- `src/heap.rs` → minimal bump term arena (the representation's allocation interface)
- `src/literal.rs` → `TAG_LITERAL_PTR` tagging + literal-area graph walk + `LiteralArea`
- `src/ops.rs` → `TermClass`/`class`, numeric predicates, `THE_NON_VALUE`
- `src/error.rs` → local `TermError`/`TermResult` + the `From<TermError> for beam_common::BeamError`
  interop impl (upstream reconciliation so dependents get `?`-interop)
- `tests/` → crate-level integration tests (bit-pattern pins `layout.rs`, ABI pins `abi.rs`)
## Notes for Agents
- **Read `../../erts/emulator/beam/erl_term.h` before changing any constant** — it is the ground truth;
  cite it in comments where a value is non-obvious.
- The C `TAG_LITERAL_PTR` (0x4) is enabled on 64-bit when there is no OS physical-memory reservation
  (the common case) or under DEBUG; we always define it (Level-1 fidelity) and mask it in
  `boxed_val`/`list_val` via `TAG_PTR_MASK` (0x7).
- `is_list(NIL)` is **false** — NIL is an `IMMED2` immediate; a "list that is not nil" is exactly a
  cons cell (LIST-tagged word). `List` terms are single words (must stay so for the NIF/ETF/msg ABI).
- `make_arityval` encodes `size << 6` with subtag `0x00`, so an arity of 0 encodes to the raw word
  `0`; table cells/`[Eterm; N]` register files must therefore not treat `0` as "empty" without care.
- Interior layouts that are NOT externally pinned (list chunking, map flatmap/HAMT thresholds, ETS
  tree shape) are open and documented at their own nodes — this crate only owns what the frozen
  layout requires.

## See Also
- `../../CONTEXT.md` — workspace contracts (§2 frozen term layout, §3 error strategy).
- `../../../ERTS_RUST_REWRITE_EVALUATION.md` §A.1–A.3, §B; `../../../ERTS_RUST_REWRITE_DEEPDIVE.md` §A, §B.
- `../../erts/emulator/beam/CONTEXT.md` — legacy core; its "Term Encoding (Eterm) Reference" section.
- `../beam_abi/` — NIF/driver C ABI; transmutes `Eterm` ↔ `ERL_NIF_TERM`.
- `../beam_alloc/`, `../beam_gc/` — production heap/allocator + collector that build on `heap.rs`.
