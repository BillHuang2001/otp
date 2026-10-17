# `beam_engine::terms` — the boxed-term builder/reader over the heap seam

## Intent
The engine's term-heap glue. Term *construction/inspection* is owned by `beam_term` (the
frozen-layout LCA, which never allocates); this node obtains raw storage from the execution
context and passes it to the `beam_term` writers, and reads terms back for the interpreter.
It is the crate's — and the widened interpreter's — **single point of raw-heap access**
(`lib.rs` lifts the crate-wide `#![deny(unsafe_code)]` for `pub mod terms` alone, alongside
`predicates`). Only the **heap binary** family has no `beam_term` owner and hand-rolls its
layout here, pinned to the C ground truth. Both heap-binary builders zero the whole data
section before the payload copy, so the padding tail of the last data word is always zero
and the term never carries stale heap content past its bit count — the same discipline as
`beam_term::bitstring::write_heap_bits`.

## Two builder flavours: classic (public) and `*_rooted`
The module exposes **both** an allocation seam's worth of builders, side by side, and both are
public:

- **Classic builders — the public API consumed cross-crate** (`beam_rt`, the integration tests
  under `tests/**`). Signatures are the frozen baseline ones and each obtains its storage with a
  plain `ExecutionContext::alloc_words`:
  `make_tuple(ctx, elems)`, `make_cons(ctx, head, tail)`, `make_list(ctx, elems)`,
  `badmatch_reason(ctx, module, value)`, `make_heap_binary(ctx, bytes)`, `make_float(ctx, v)`,
  `make_integer(ctx, v)`, `make_flatmap(ctx, pairs)`, `make_map(ctx, pairs, atoms)`,
  `make_fun(ctx, arity, uniq, index, free)`.
  They take already-materialized operands and hold no term across the allocation, so they are
  correct for any host whose `alloc_words` never collects (the seam contract reserves collecting
  behaviour for `alloc_words_rooted`).
- **`*_rooted` builders** — the engine's own term-construction path, reserving through the rooted
  seam so a **moving** collector may run. Every one is
  `(machine: &mut Machine, ctx, live: u32, …, read: R)` where `R: FnOnce(&Machine, &mut Slack)
  -> Result<…>` supplies the operands read *after* the reserve:
  `make_tuple_rooted(machine, ctx, live, n_elems, extra, read)`,
  `make_cons_rooted(machine, ctx, live, extra, read)`,
  `make_list_rooted(machine, ctx, live, n, extra, read)` (**one** contiguous `2n`-word
  reservation), `badmatch_reason_rooted(machine, ctx, live, module, read)`,
  `make_fun_rooted(…)`, `make_map_rooted`/`make_flatmap_rooted`, `make_float_rooted`,
  `make_integer_rooted`, `make_heap_binary_rooted(machine, ctx, bytes)` (whole-X-file live).
  They live beside their classic twins in the same file (`construct.rs`/`number.rs`/`map.rs`/
  `fun.rs`) and are re-exported from `mod.rs`. `make_integer_unrooted` is the operand-immediate
  path (never collects) and stays `pub(crate)`.

`mod.rs` re-exports both families under their own names; a caller picks the seam it needs.

## The frozen allocation discipline: reserve → read → write
`ExecutionContext::alloc_words_rooted` may be backed by a **moving** collector: it relocates
every term reachable from the `RootSet` it is handed (`x`/`stack`/`fr`) and rewrites those
slices **in place**. Therefore any `Eterm` *copy* held in a Rust local across a collecting
allocation is a stale pointer, and a `*mut Eterm` from a previous allocation is dead. Every
`*_rooted` builder follows (`slack.rs` module docs):
1. reserve the whole block **first** with *exactly one* rooted allocation (`reserve` /
   `reserve_slack`) — the collection happens here;
2. then read every operand from the machine's **post-collection** state (a builder's `read`
   closure, reading through `crate::operand::read_operands_slack` / `read_operand_slack`);
3. then write the header/tag words.
Nothing may allocate from the BEAM heap between the reserve and the final write. An operand
that would itself need a heap block (a non-small `integer`/`character` → an exact bignum) is
materialized into the reserved block's spare tail via `Slack::integer`, so the build still
costs a single allocation. `live` is `RootSet::ALL_LIVE` whenever the caller has no
operand-derived count (always a safe over-approximation for a copying collector).

Every term **builder** call site (`families/**`, `machine/**`) is on this discipline:
`rg -n 'ctx.alloc_words' src/families` is empty. Two sanctioned **unrooted** paths survive, both
sound only because `alloc_words` never collects:
- `crate::operand::read_operand` (`../operand.rs:69`/`:75`) materializes a non-small
  `Operand::Integer`/`Character` through `number.rs::make_integer_unrooted` → `ctx.alloc_words`, so
  **any** instruction reading a bignum immediate allocates once per operand on the unrooted seam —
  from the families and from the interpreter's core arms, on both tiers (the JIT's family/deopt
  bodies reach the same reader). Moving-safe: the bignum is consumed immediately and no
  machine-heap term is held across the call.
- the four `&self` accessors in `../machine/location.rs` (see Handoff below).

## API Surface (`crate::terms::*`)
- **Classic builders** (public, unrooted) — listed in the section above; each allocates with
  `ctx.alloc_words` and takes already-materialized operands.
- **`*_rooted` builders** (public, rooted) — listed in the section above; each reserves once
  through the rooted seam and reads its operands through a closure.
- **`reserve(machine, ctx, n, live) -> *mut Eterm`** — the single rooted admission point.
- **`reserve_slack(machine, ctx, n) -> Slack`** — reserve `n` words rooted and wrap the
  **whole** region in a `Slack`; the entry point for a multi-node build (below).
- **`Sink`** (`sink.rs`) — the node-shape abstraction a shape writer carves from:
  `tuple(&[Eterm])` (arityval + elements), `list(&[Eterm])` (contiguous `[CAR,CDR]` cells,
  empty ⇒ `NIL`) and `cons(car, cdr)` (a single `[CAR|CDR]` cell — the one-node **prepend**
  primitive, for a writer whose graph is not a single proper list, e.g. a synthesized
  callee head item consed in front of a carved frame list). Two implementations, **one**
  definition per node shape:
  - `impl Sink for Slack` — the **rooted** sink: every node is carved out of the one
    reservation via the bump cursor (`Slack::alloc_words`). Nothing touches the context heap,
    so nothing can collect between nodes and the nodes already written (whose pointers are not
    roots) stay put. This is what makes a multi-node graph — e.g. a `{M,F,A,Location}`
    stacktrace item plus its `[{file,File},{line,N}]` location, or a list of such items —
    correct under a moving collector.
  - `ContextSink::new(ctx)` — the **unrooted** sink: one `ctx.alloc_words` per node (a
    `&self` caller that cannot lend `&mut` roots uses it; `alloc_words` never collects). It is
    only moving-safe for shapes with **no** machine-heap operand held across an allocation.
  A region too small for a node is a typed `crate::error::unsupported`, never a wrong term.
  `tuple` additionally rejects more than `MAX_ARITYVAL` (`(1 << 24) - 1`, the `arityval`
  header's 24-bit field) elements with `BeamError::SystemLimit` **before** carving (one
  private `check_tuple_arity` used by both sinks), so a slice no header can encode is never
  written as a bogus arity.
- **Readers** (`read.rs`, `map.rs::map_entries`, `fun.rs`): `tuple_arity`, `tuple_element`,
  `cons_head`, `cons_tail`, `list_elements`, `binary_bytes`, `fun_*`, `is_fun`, `is_hashmap`.
  Wrong type / out-of-range ⇒ `BeamError::BadArg`; no reader panics.
- Constants: `MAP_SMALL_MAP_LIMIT` (32, the flat-map/HAMT boundary), `HAMT_*` node offsets,
  `ERL_ONHEAP_BINARY_LIMIT` (64), `HEAP_BIN_HEAD_WORDS`, `WORD_BYTES`.
- `test_support` (`#[cfg(test)]`) — wrappers preserving the old 2-arg ergonomics **over the
  rooted builders**; they keep the plain names (`make_tuple`, `make_cons`, …) so a test module
  doing `use super::test_support::{…}` / `use crate::terms::test_support as terms;` shadows the
  classic glob names. **Never** moving-collector-safe, so they exist only for tests.
- `alloc_tests` (`#[cfg(test)] pub(crate)`) — a mock semi-space **moving** collector
  (`MovingContext`) that relocates every term reachable from a `RootSet` and rewrites the root
  slices in place, plus the tests proving (a) the `*_rooted` builders route every allocation
  through `alloc_words_rooted` and never through the unrooted seam, (b) a built tuple/list/map
  and a one-reservation nested graph stay correct across a relocation, (c) peak live words stay
  bounded under collection while the unrooted arena grows linearly in N. It is `pub(crate)` so
  the family suites reuse it: `../families/records/tests/rooted.rs` and
  `../families/bitstrings/matching/rooted.rs` drive those two families under the same collector.

## Constraints
- Safe Rust here only in the sense that raw-pointer work lives **only** in this node; every
  `unsafe` block carries a `// SAFETY:` rationale and every `unsafe fn` a `# Safety` section.
- `beam_common::{Result, BeamError}` only; never panic, never `unwrap()` runtime data, never
  produce a wrong result — an unsupported case is a typed error.
- Frozen term layout is `beam_term`'s; reuse its writers rather than re-deriving a layout
  (the tuple/list node writers are shared by `construct.rs` and `sink.rs` via
  `write_tuple_at`/`write_list_at`).
- Files ~1000 lines, split by responsibility.

## Routing Table
- `mod.rs` → module wiring, the root re-exports (classic **and** `*_rooted`) and the layout
  constants.
- `slack.rs` → the rooted reserve seam, `Slack` (bump cursor, `alloc_words`, `integer`),
  `reserve`, `reserve_slack`; the reserve→read→write contract in the module docs.
- `sink.rs` → the `Sink` trait, `impl Sink for Slack` (rooted carve), `ContextSink` (unrooted),
  `check_tuple_arity` (both `tuple` entry points' `MAX_ARITYVAL` guard), and the shared
  `write_tuple_at`/`write_list_at`/`write_cons_at` node writers.
- `construct.rs` → the classic tuple/cons/list/`badmatch_reason`/heap-binary builders **and**
  their `*_rooted` twins; the heap-binary builders zero the data section before the payload
  copy (padding tail zeroed).
- `number.rs` → the classic `make_float`/`make_integer` **and** the `*_rooted` twins, plus
  `make_integer_unrooted` (the operand-immediate path).
- `map.rs` → the classic `make_flatmap`/`make_map` **and** the `*_rooted` twins, plus the
  `map_entries` walker.
- `fun.rs` → the classic `make_fun` **and** `make_fun_rooted`, plus the `ErlFunThing` readers.
- `read.rs` → the tuple/cons/list/binary readers.
- `tests.rs` → in-module unit tests; `test_support.rs` → the test-only ergonomic wrappers (over
  the rooted builders); `alloc_tests.rs` → the mock moving collector + the rooted-hook/peak-words
  proofs.

## Handoff — the unrooted paths still open (owner: `beam_rt`)
`../machine/location.rs:277`/`:409` — the four `&self` accessors (`current_function`,
`initial_call`, `current_location`, `current_stacktrace`) cannot lend the machine's `&mut` root
slices, so they build through the **unrooted** [`ContextSink`]. Sanctioned: every term there
comes from module metadata/immediates with no machine-heap operand held across an allocation,
and `alloc_words` never collects. Adopting a rooted `&mut self` twin needs `beam_rt` to change
`engines.engine_at(pid)` → `engines.engine_at_mut(pid)`
(`beam_rt/src/native/memory/mod.rs:519-522`) — a change outside this crate.

`../families/exceptions/stacktrace.rs`: `symbolic_stacktrace` sizes its whole graph from
**metadata** first (`LocationShape`/`ItemShape`), takes **one** `reserve_slack`, then carves
every node with [`Sink`] (a per-node allocation would strand the nodes already written — their
pointers are not roots). `stacktrace_item` keeps its signature (it is `../machine/location.rs`'s
entry point) and builds through [`ContextSink`]; one `write_location`/`write_item` definition
parameterised by `&mut dyn Sink` keeps the rooted and unrooted paths from diverging.

## Test Strategy
In-module `#[cfg(test)]` unit tests against a `MinimalContext` arena: round-trips through the
readers, raw-word layout assertions against stack buffers (allowed here — this is the SAFETY
module), the wrong-type/limit error cases, and the carve discipline (`slack_carves_*` builds a
nested graph out of ONE reservation and asserts the region is consumed exactly once;
`a_carve_region_too_small_*` pins the typed error; `context_sink_*` covers the unrooted sink).
Two further pins: `terms::tests::DirtyContext` — a context whose heap is pre-filled with a
non-zero byte pattern, so "these bytes were never written" is observable — backs
`a_heap_binary_zeroes_the_padding_of_its_last_data_word`, which reads the **whole** first data
word of a 3-byte heap binary back as bytes for both the classic and the rooted builder; and
`a_sink_tuple_beyond_max_arityval_is_a_system_limit` pins the `MAX_ARITYVAL` guard of both
`Sink::tuple` implementations (`a_sink_tuple_writes_the_arityval_header` pins the header they
do write).
The moving-collector discipline is proved by `alloc_tests.rs` (builders) and by the family
suites `../families/records/tests/rooted.rs` and `../families/bitstrings/matching/rooted.rs`
(which assert a real relocation occurred, the result is correct, and the vacated address was
poisoned so a stale read could not pass). Cross-crate behaviour (the actual
`{M,F,A,Location}` shapes, ETS/map HAMT copies) is covered by the owning families and by
`beam_gc/tests/{map_copy,refc_binary_copy}.rs`; oracle cross-checks live in
`beam_term/tests/oracle.rs`.

## See Also
- `../../CONTEXT.md` → the crate-level contracts and Handoff #11 (the `beam_rt` term-builders
  still on the unrooted seam), plus the `{M,F,A,Location}` shape consumer
  (`../families/exceptions/stacktrace.rs`) and the `&self` accessors
  (`../machine/location.rs`).
- `../../../beam_term/CONTEXT.md` → the frozen term/`map`/`bitstring`/`refc`/`fun` writers this
  node calls.
- `../context.rs` → `ExecutionContext::{alloc_words, alloc_words_rooted}` and `RootSet`.
