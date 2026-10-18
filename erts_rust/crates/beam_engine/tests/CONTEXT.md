# `beam_engine` Integration Test Suite (`tests/`)

## Intent
Black-box integration tests for the `beam_engine` crate: each `tests/<name>.rs` compiles as its
own crate and drives the engine through its **public** API. Hand-authored cases load committed
`.beam` fixtures from `fixtures/`, while the whole-corpus suites load **real installed OTP**
modules discovered generically at run time (`support/discovery.rs::all_otp_beams`/`all_otp_ebins`:
`otp_lib_root()` -> one entry per application directory whose `ebin` holds at least one `.beam`,
no application name hardcoded) in `stdlib_sweep.rs`, `census.rs`, `jit_real_module.rs`,
`jit_cranelift_real.rs` and `apps_lists.rs` — such a suite SKIPS LOUDLY when `erl` or the OTP
install is absent (a green run without `erl` proves nothing about corpus parity). `src/**` is
never touched from here.

## API Surface (what a test crate may rely on)
- `beam_engine::{Machine, MinimalContext, ExecutionContext, RootSet, RunOutcome, Step, Tier,
  TraceEvent, unsupported, MAX_REGISTER, MAX_FLOAT_REGISTER, ...}` and `beam_engine::terms::*`
  (tuple/cons/float/map builders and readers).
- `beam_loader::{Module, Instr, Operand, HeapNeed, Label, TestKind, load_file, ...}` and
  `beam_term::{Eterm, make_small, make_atom, NIL, ...}`.
- `support::*` — the shared host: the multi-module `ModuleHost` `ExecutionContext`, the
  shared-atom loader, OTP discovery, the local BIF floor, and term assertion helpers; plus the
  `support::probe` harness (`Probe`/`Expect`/`Verdict`/`ProbeRun`) for a row that must drive an
  instruction with valid operands.

## Constraints
- Read-only consumers: never edit `src/**` (owned elsewhere); never edit the crate-root docs.
- Failures are typed `beam_common::BeamError` values; never `unwrap()` engine runtime data;
  a harness-only invariant may panic with a message.
- Every `.rs` carries the OTP Apache-2.0 `// %CopyrightBegin% … // %CopyrightEnd%` header block;
  files target ~1000 lines. Eighteen suites are above that target, each one cohesive harness rather
  than a mix of subjects (splitting any of them is a parity-neutral refactor): `jit_cranelift.rs`
  (4298), `opcodes.rs` (2761), `jit.rs` (2701), `exceptions.rs` (1523), `engine_bench.rs` (1482),
  `census.rs` (1462), `dynamic_census.rs` (1415), `jit_cranelift_real.rs` (1423),
  `call_ext_apply.rs` (1333), `jit_compare.rs`
  (1319), `bitstrings_extra_big.rs` (1204), `records.rs` (1191), `raise_rooting.rs` (1098),
  `trace.rs` (1079), `stdlib_text.rs` (1016), `jit_linking.rs` (1009), `stdlib_exports_more.rs`
  (1002). `raise_rooting.rs`'s cohesion is spelled out in the routing table below; `census.rs`'s
  is the hand-maintained exhaustive `Instr`/`TestKind` rosters, which only make the "108 rows / 108
  distinct names" completeness assertion real if they live beside the census that consumes them.
- `dynamic_census.rs` is one instrument split by role, not by subject: that file is the driver
  (three layers, one `reset()` + boundary snapshots, the ranking / declared-raise / blocked-mechanism
  rendering, every assertion) and `dynamic_census/catalog.rs` (1198) is its data half (the named
  workload closures + the fixture entry points, each carrying its provenance comment). The catalog is
  reached by `#[path = "dynamic_census/catalog.rs"] mod catalog;` — a **directory** under `tests/` is
  not compiled as a test target, while a second top-level `.rs` would become one.
- One support module is past that target: `support/probe/mod.rs` (1117) is the single probe builder
  (atoms, mailbox, hooks, `Expect`/`Verdict`, the generated module and the run/verdict funnel) that
  every probe row in `support/probe/rows/` is written against — splitting it would duplicate the
  builder per fragment.
- Always the wrapper, never a bare `cargo`, never `cd`:
  `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine [--test <name>]`.

## Notes for Agents
- `TEST_NOTES.md` (crate-root, not owned here) is the authoritative per-suite enumeration; do not
  duplicate it here.
- A suite needing the system toolchain (a real `.beam`, the `erl` oracle, or an `erlc` fixture
  build) must SKIP LOUDLY **only when the tool is absent** — a spawn failure
  (`Command::new("erl"/"erlc").output()` returned `Err`, no tool on PATH); a green run without
  `erl` proves nothing about corpus parity. A tool that RAN and exited non-zero is a HARNESS
  failure and must PANIC with its captured stdout/stderr, never be folded into "no oracle".
  Reference: `trace.rs::oracle_eval`.
- The `fixtures/` directory holds stand-in modules (`lists.beam`, `recv.beam`, …) that SHADOW
  stdlib when PREPENDED, so an oracle invocation must APPEND it (`erl -pz <dir>`), never prepend
  (`-pa`, which crashes the emulator at boot with an EMPTY stdout+stderr).
- `support/` is shared by every test crate; its facade re-exports the whole surface, so a
  re-export an individual crate does not use is not an unused import.

## Routing Table
- `support/` → shared multi-module `ExecutionContext` host: OTP discovery (`otp_lib_root`/`all_otp_beams`), the shared atom universe, the BIF floor, assertion helpers
- `support/probe/` → the valid-operand probe harness (`mod.rs`: `ProbeAtoms`, `ProbeMailbox`, `Expect`, `Observed`, `Verdict`, the `Probe` builder, `ProbeRun`, `render_term`, the fixed entry/aux/fail-sink labels) and its roster in `rows/` — one file per instruction group (`core.rs`, `data.rs`, `exceptions.rs`, `receive.rs`, `bitstrings.rs`, unioned by `rows/mod.rs::all_rows`), keyed by the variant name **derived** from the constructed `Instr` so a row can override a legacy row but never invent a name
- `fixtures/` → committed system-`erlc`-produced `.beam` fixtures + `.erl` sources + target lists
- `rooted_alloc.rs` → proves the engine's term-construction instruction arms route storage through `ExecutionContext::alloc_words_rooted` (whole-root hand-off; a counting context whose `alloc_words` stays the typed-error default so any fallback fails loudly; a moving-collector relocation proof)
- `raise_rooting.rs` → the raise-window counterpart: a hand-built `function_clause` raise driven through a semi-space **moving** mock collector (`MovingContext`, which relocates every term it is handed and poisons the region it vacated), proving the parked `raise_function_clause_args` latch rides `Machine::with_roots`'s published root set. One cohesive file by design (~1100 lines): the single mock collector, the single raise fixture and every latch-survival shape (parked vs unparked hand-off, cooked vs uncaught trace, post-move assertions) share that one harness, so splitting it would duplicate the collector per fragment.
- `type_sizes.rs` → observes the frozen IR size guards (`Instr` ≤ 64 / `Operand` ≤ 16 / `HeapNeed` ≤ 24) over the public types (authoritative pins are the compile-time asserts in `beam_loader/src/instr.rs`)
- `census.rs` → the `Instr`-variant / `TestKind` execution census + whole-OTP corpus reachability. Four buckets (`EXECUTED` / `ZERO-CORPUS` forward-compat / `GENUINE-GAP` / `UNCLAIMED`) partition the 108-variant roster, pinned by `EXECUTED_VARIANTS` / `ZERO_CORPUS_VARIANTS` / `GENUINE_GAP_VARIANTS`; 64 rows are probe-driven with valid operands and a declared `Expect`, the other 44 keep the compact degenerate-operand module. `ZERO_CORPUS` is the 13-name forward-compat pin, asserted **equal** to the live zero-occurrence set measured over the whole installed OTP, so a variant the toolchain starts or stops emitting fails the build
- `dynamic_census.rs` + `dynamic_census/catalog.rs` → the **dynamic** per-variant ranking: one serialized test drives real compiled Erlang on `Tier::Interpreter` over three layers (the whole-OTP `module_info/0,1` sweep, the named stdlib workload catalog, the committed fixtures incl. a mailbox-driven `recv` section), attributes weight per layer from boundary snapshots of `beam_engine::exec_census`, and reports the combined ranking, the DECLARED-raise vs UNEXPECTED error split, the blocked-mechanism table, the computed not-reached list and the TSV dump. `catalog.rs` holds only the workload/fixture data
- `reductions.rs` → per-instruction reduction charging (exactly-once, host pre-emption) and the suspend/resume slice boundary
- `gc_roots.rs`, `gc_arena.rs`, `bif_alloc.rs` → GC root-set hand-off, the region heap, BIF-driven allocation
- `jit.rs`, `jit_real_module.rs` → native baseline tier equivalence (synthetic + real-`erlc` modules); `jit_linking.rs`, `jit_cache.rs`, `jit_code_map.rs`, `jit_compare.rs`, `jit_coverage.rs` → block linking, the cross-run cache, the host code map, and baseline-vs-interpreter coverage; `jit_bench.rs` → the thin driver over the `crate::jit::bench` measurement harness; `jit_cranelift.rs`, `jit_cranelift_real.rs` → the feature-gated Cranelift hot tier
- `no_fail.rs` → the integration-level `NO_FAIL`-guard evidence: drives `test` / `select_val` / `get_map_elements` / `has_map_fields` with `fail == NO_FAIL` through the real public entry `Machine::run_label` and asserts the DEFINED raise is class `error` + the bare atom `function_clause`, **both** uncaught (a `BeamError::Raise` carrying the live reason term) **and** caught in a live `try` frame (handler observes class `error` / reason `function_clause`). The positive counterpart to `gaps.rs`'s typed "no such atom" pins; complements the in-crate unit tests in `src/interpreter/tests.rs` and `src/families/maps/tests.rs`
- `stdlib_text.rs` → the text / byte-encoding / collection-list stdlib surface on the real installed stdlib: `string` (case mapping + `trim`/`slice`/`join`/`tokens`), `base64` (`decode/1`, `encode/2` with its option maps), `unicode` normalisers, the `io_lib` writers/predicates, the reshaping + fun-taking `lists` exports, and `ordsets`; targets still needing an unserved NIF (`unicode:characters_to_list/1`, `binary:compile_pattern/1`) are recorded as blocked mechanisms with their exact typed error
- `stdlib_text_more.rs` → the second text half: the `unicode` character-property surface (`is_whitespace`/`is_id_*`/`category`/`bin_is_7bit`/`bom_to_encoding`/`encoding_to_bom` + the list-only `characters_to_nfkc_list`/`nfkd_list`), the obsolete/alignment `string` helpers (`strip`/`left`/`right`/`substr`/`equal`/`prefix`/`centre`/`pad`), `io_lib` `fwrite/2`/`nl/0`/`printable_*`, the `lists` tuple-key/`prefix`/`suffix`/`merge` family, and the n-ary `ordsets` `union/1`/`intersection/1`
- `stdlib_collections.rs` → the additional collection/misc stdlib exports: `calendar` (tuple-form entry points + the second-based conversions), `filename` (with `kernel`'s `os` on the search path), `array` (`make_tuple`-built interiors + `setelement` updates), `dict` (a hash dictionary reaching captured-variable closures + `phash/2` bucketing), `orddict`
- `stdlib_collections_trees.rs` → the balanced-tree collections `gb_trees` (`from_orddict`/`is_defined`/`update`/`take*`/`smallest`/`largest`/`iterator`+`next`/`balance`) and `gb_sets` (`union`/`intersection`/`subtract`/`singleton`/`is_equal`/`take_smallest`/`take_largest`), including the set algebra
- `stdlib_collections_queue_maps.rs` → `queue` (the two-list FIFO inserters/rear accessors/rear droppers/`join`/`split`/`delete` family) and `maps` (whose map-BIF operations are asserted as a **host gap** with their exact typed error, never a wrong result)
- everything else (`stdlib_lists`, `stdlib_more`, `stdlib_sweep`, `apps_lists`, `coverage`, `opcodes`, `tuples_lists`, `maps`, `funs`, `floats`, `records`, `bitstrings*`, `exceptions*`, `receive`, `tailcall`, `external_call`, `call_ext_apply`, `cross_module_catch`, `local_call_seam`, `apply_atom_value`, `clause_dispatch`, `func_info_stacktrace`, `current_module`, `process_info`, `wait_deadline`, `trace`, `gaps`, `bignum_compiled`, `slice`, and the `*_bench`/`*_cost` measurement suites) → per-family / per-contract execution and error-surface suites; see `TEST_NOTES.md`.
