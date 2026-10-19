# `beam_engine::families` — the per-opcode-family executors

## Intent
The **instruction-family seam** of the typed-IR engine: every `beam_loader::Instr` the core dispatch
loop (`../interpreter/`) does not execute natively is routed here by [`mod.rs::dispatch`] to the single
family that owns it.
One family per opcode group; each exposes `exec(...) -> Result<Option<Step>>` where `Ok(Some(step))` =
"this family executed it", `Ok(None)` = "not my variant", `Err` = the family raised.
Family ownership is **disjoint and variant-complete** (`mod.rs:121-218`): each of the 77 claimed `Instr`
variants has exactly one owning family, so the route is a jump table on the `Instr` discriminant —
semantically identical to the old first-claim-wins probe loop (see the module doc, `mod.rs:93-110`).

## API Surface
- `pub(crate) fn dispatch(machine, entry_module, module, labels, instr, func, pc, ctx) -> Result<Step>` — `mod.rs:112`.
- `pub(crate) const UNCLAIMED_PREFIX: &str = "widened opcode not yet executed"` — `mod.rs:82` (frozen sentinel, pinned by `mod.rs:321`).
- Every family: `pub(crate) fn exec(machine, module, labels, instr, func, pc, ctx) -> Result<Option<Step>>`; `funs` and
  `exceptions` additionally take the run's `entry_module` (the stacktrace/callee-item cooks need it).
- `pub(crate) fn bit_size(t: Eterm) -> Option<usize>` (`bitstrings/raw.rs`, re-exported once from
  `bitstrings/mod.rs` under `#[allow(unused_imports)]`) — the host-facing `erlang:bit_size/1` reader: a heap
  bits/binary takes its size word, while a refc window **and** an `ErlSubBits` match context take
  `end - start`, exactly `erts_bin_bits_size`; a non-bitstring — and a bare off-heap `BinRef`, which no
  `bs_start_match` has wrapped yet — is `None`, never a substituted size. Its two
  intended adopters are the BIF floors that serve `erlang:bit_size/1` — the in-crate harness floor
  `src/bench/counting.rs`, which **has** adopted it (that is what makes `emqx_base62:decode/1`
  measurable: `instr_per_call` 531, `rooted_calls_per_call` 32), and the external test host
  `tests/support/bifs.rs`, which still cannot (integration tests see the crate from outside and
  `families` is a private module, so it would need a `pub` re-export).
- All computation goes through `crate::context::ExecutionContext`; term construction via `crate::terms`.
  Families build no term layout of their own **except** `bitstrings/raw.rs` (see Duplication).

## The exception-class-atom choke point (`dispatch`)
`dispatch` installs the still-missing `error`/`exit`/`throw` class atoms through the engine's **live atom namespace**
(`Machine::ensure_class_atoms(ctx, module)` → `namespace_atom`, i.e. `ctx.atoms(module)` with the running module's
load-time snapshot as the fallback) **before** the routing `match` runs.
- **Why here.** This is the single function **every** family-owned instruction passes through, on **both** tiers (the
  interpreter's catch-all arm calls it and the native baseline tier's `jit_family` helper reaches the same call), so
  it is the one place that covers all nine families' raises at once. A family body raises through
  `Machine::throw`/`throw_error` without naming a class atom; without the resolution here, a landing whose class atom
  the *running module's* table cannot name would select its catch frame and then abandon it with a typed
  `unsupported` (C reads the class atoms from its `am_error`/`am_exit`/`am_throw` globals, so it can never be in that
  state). Resolving in each family (or at each raise site) instead would duplicate the same lookup nine times and
  still miss `bitstrings`/`records`' internal raises.
- **Cost.** `dispatch` tests the per-run `class_atoms_live` latch (`../machine/CONTEXT.md`) **itself** and only
  calls `Machine::ensure_class_atoms` when it is clear, so the steady state is one `bool` load and a not-taken
  branch instead of a function call per family-owned instruction; the resolution runs at most once per run, and
  an atom already installed is never overwritten. The saving is small but deterministic: **−359 retired
  instructions per call (−0.19 %)** on `cowboy_bstr:to_lower/1` (see `## Measured`).
- **Honesty.** A class name that **neither** source can name stays unresolved, and the landing reports the typed
  `unsupported("exceptions: no atom for the '<class>' exception class …")` — never a substituted atom index
  (pinned by `mod.rs`'s `a_family_raise_lands_…` / `a_family_raise_whose_class_is_unnameable_…` pair).

## Routing Table (file → owned variants)
- `tuples.rs` — `PutTuple2` / `GetTupleElement` / `SetTupleElement` / `SelectTupleArity`; `SetTupleElement` is the destructive in-place write (`tuple_val(Tuple)[1 + offset]`, `../terms/update.rs`), whose non-tuple/out-of-range target raises the read path's `{badmatch, Source}` and whose literal-area target is the typed `unsupported`.
- `records/` (`mod.rs` + `raw.rs` + `tests/`) — **nine** variants: the OTP-26 tagged-tuple `UpdateRecord`, plus the eight OTP-29 native-record ones (`IsAnyNativeRecord`, `IsNativeRecord`, `GetRecordElements`, `GetRecordElementsId`, `PutRecord`, `IsRecordAccessible`, `GetRecordField`, `UpdateRecordId`). `raw.rs` **forwards all** record layout to `beam_term::record`. Detail: `records/CONTEXT.md`.
- `lists.rs` — `PutList` / `GetList` / `GetHd` / `GetTl`.
- `funs/` — `MakeFun3` / `CallFun` / `CallFun2` / `Apply` / `ApplyLast` (builds closures via `crate::terms::make_fun`). `call_fun` **classifies its target's shape before the closure-apply seam** and raises the oracle's structured `error` reasons `{badfun,Fun}` / `{badarity,{Fun,Args}}` (C's `call_fun` + `expand_error_value`); see `funs/CONTEXT.md`.
- `exceptions/` (`mod.rs`, `raise.rs`, `stacktrace.rs`, `tests/`) — `Try`/`Catch`/`Badmatch`/`CaseEnd`/`TryCaseEnd`/`Badrecord`/`IfEnd`/`Raise`/`TryCase`/`TryEnd`/`CatchEnd`/`BuildStacktrace`/`RawRaise`.
- `floats/` (`mod.rs`, `arith.rs`, `convert.rs`) — `Fmove`/`Fconv`/`Fadd`/`Fsub`/`Fmul`/`Fdiv`/`Fnegate`. Detail: `floats/CONTEXT.md`.
- `maps/` (`mod.rs`, `read.rs`, `write.rs`) — `GetMapElements`/`HasMapFields`/`PutMapAssoc`/`PutMapExact`.
- `bitstrings/` (`mod.rs`, `raw.rs`, `create/`, `matching/`) — the whole `Bs*` family; `raw.rs` is this
  sub-node's `#[allow(unsafe_code)]` module (`records/raw.rs` is the other one in the node); `create/` =
  `bs_create_bin`, `matching/` = the
  match commands (with the allocation-aware context re-read elision). A re-entered match costs nothing:
  `enter_match`'s reuse arm returns the **same `Eterm`** (C's `$Dst = context`), and `bs_get_position`
  publishes a **small integer** rather than a context snapshot — the binary-comprehension loop therefore
  reserves no match context at all. `matching/mod.rs::exec` pre-tests `Instr::BsMatch` ahead of its 18-arm
  jump table and hands the rest to an outlined `exec_other`; `matching/commands.rs` keeps its cold command
  bodies (`skip`/`binary`/`get_tail`/`'=:='`) out of line, and `create/segments.rs` hosts the outlined
  `binary`/`string`/`float`/`utf` segment bodies (`SegOutcome`), which `assemble` calls.
  Endianness: `matching/reads.rs::field_is_little`
  is the single rule for **reads**, `mod.rs::BitWriter::push_le_groups` for **writes**, and `mod.rs::flag_bit`
  normalizes a `native` flag atom; `BSF_NATIVE` is a **live** flag (a standalone read's raw `u32` flags operand
  carries it). Whole-byte fast paths on both sides: the writers (`push_uint`/`push_uint_le`/`push_le_groups`/
  `push_wide_int`) and the readers (`read_uint_be`/`extract_bits`) take a byte-at-a-time arm only when the field
  is a whole number of bytes **and** byte-aligned, after every range/width check, so an out-of-range field keeps
  the oracle's error and a non-aligned shape keeps the bit-exact rule.
- `receive.rs` — `Send`/`LoopRec`/`LoopRecEnd`/`Wait`/`WaitTimeout`/`Timeout`/`RemoveMessage`/`RecvMarkerReserve`/`RecvMarkerBind`/`RecvMarkerUse`/`RecvMarkerClear`.

## The router sentinel + the honest-error convention
- A variant **no** family owns, or one a family declines with `Ok(None)`, reaches `mod.rs:221-224`:
  `Err(error::unsupported("<UNCLAIMED_PREFIX>: <name>"))`.
- `instr_name` (`mod.rs:242`) knows only `bif0`/`bif`/`init_yregs`/`executable_line`/`debug_line`; every
  other unclaimed variant degrades to the literal `"instruction"` (documented diagnosability loss).
- **No family claims a variant and then silently no-ops it.** Verified by reading all nine families +
  submodules: every `Instr` arm performs the operation, raises, or returns a typed `unsupported`.
- Every capability gap is a typed `error::unsupported(...)`/`BadArg`/`SystemLimit` — **never** a wrong value.

## Capability gaps — each a typed `unsupported` (NEVER a wrong value)
- `records/mod.rs` `fail_step`: `fail == NO_FAIL` ⇒ the shared failed-clause raise
  (`crate::interpreter::raise_function_clause_no_fail`, the same route `maps/mod.rs` `fail_or_function_clause`
  takes): class `error` with the bare atom `function_clause` resolved through `machine::namespace_atom`
  (`ctx.atoms(module)` first, the module's load-time snapshot second). A namespace that can name **neither**
  yields `unsupported("<instr>: no 'function_clause' atom in the module")` — never a substituted atom index.
  The raise is uncaught-structured out of the machine (`ExitReason::Structured` + the atom as the raise's
  live reason term), so a caller reading `as_atom()` sees `None` by design.
- `records/mod.rs` `local_definition`: a **foreign** `{Module,Name}` record id ⇒ `unsupported`
  ("create: no record definition table for another module"). **The running module's own
  `put_record/6` create form IS implemented** (`create_native_record`) — see the Handoff below.
- `maps/mod.rs` `fail_or_function_clause` (absent key + `NO_FAIL` ⇒ the bare atom `function_clause`,
  through the shared `crate::interpreter::raise_function_clause_no_fail`); `fail_or_raise`/
  `raise_error_tuple` raise `{badmap,_}`/`{badkey,_}`; a missing tag atom ⇒ `unsupported`.
- `exceptions/mod.rs` `Raise` with no `pending_class`; `try_case` without `THE_NON_VALUE`;
  `catch_end` with x(1)/x(3) out of range or the missing `EXIT` atom; `exceptions/raise.rs` a missing
  tag atom ⇒ `unsupported`.
- `exceptions/stacktrace.rs` a missing `file`/`line` atom ⇒ `unsupported`.
- `floats/*` — a non-float FR slot or a non-finite result ⇒ the documented `badarith` failure, never a fabricated value.
- `bitstrings/` — every typed gap is a guard for a **malformed `.beam`** or an internal invariant the installed
  compiler cannot reach (verified by a `beam_disasm` census over the installed OTP): an unknown/missing segment
  type, an integer segment of size `all`, a `string` segment's out-of-table/negative/overflowing `StrT` offset,
  a truncated segment list, a `bs_match` command outside the seven `run_commands` covers, an unknown
  `bs_start_match4` fail atom, and `raw.rs`'s unreachable `> 64`-byte / zero-width-bignum / short-buffer guards.
  `bs_match_string` resolves its `StrT` offset with an `ok_or_else`: **strictly beyond** the table is the typed
  `unsupported`, **exactly at** the end a valid empty pattern. A non-integer value in an integer segment is the
  oracle's bare `badarg` at **every** width; a bignum `float` segment that overflows a finite double is `badarg`
  (C's `big_to_double < 0`). Exact list: `bitstrings/CONTEXT.md`.
- `receive.rs` `marker_call` — a host that does not serve the marker seam is rejected **by name**.

## Error-swallowing sites (a `Result`/failure is discarded)
- `exceptions/mod.rs` and `exceptions/raise.rs` — `let _ = install_class_atom(...)`: the failure is
  **deliberately deferred** ("lazy landing"), so a raise whose class atom the module lacks fails later, at
  landing time, with the typed error. Not a wrong value, but the immediate failure is lost.
- The remaining `Err(_) =>` discs are intentional and correct (type tests → the documented failure):
  `lists.rs` (non-cons ⇒ badmatch), `floats/arith.rs` (non-float ⇒ badarith), `bitstrings/mod.rs`
  (out-of-small-range ⇒ build a bignum).

## Duplication with sibling crates (audit finding)
- **`bitstrings/raw.rs` FORWARDS the heap-bits/binary layout** to the `beam_term` LCA: the on-heap limit is
  `beam_term::refc::ERL_ONHEAP_BINARY_LIMIT` (imported, not re-declared), the block is sized by
  `beam_term::bitstring::heap_bits_size_words` and written by `beam_term::bitstring::write_heap_bits`, and the
  reads go through `beam_term::bitstring_parts` / `beam_term::bitstring::{sub_bits_parts, write_sub_bits}`.
  `raw.rs` keeps only the family's own reserve-and-call plumbing (the `*_rooted` builder route) and the
  unreachable malformed-input guards; `set_ctx_start` writes `ErlSubBits` word 2 directly because the LCA
  exposes no setter for it.
- **`../terms/map.rs`** (`hashmap_entries`/`collect_hashmap_node`/`hashmap_node_shape`) re-implements
  HAMT node walking + node-shape decode that `beam_term::map::{map_pairs, map_pairs_storage_order, map_get}`
  (`beam_term/src/map.rs:458,471,518`) already do. It is what `maps/{read,write}.rs` uses for enumeration.
  (The engine variant additionally rejects `THE_NON_VALUE`.) Out of this node's scope but on the `maps` family's
  path — the natural LCA home is `beam_term::map`.
- **Workspace-wide posture, not families-specific:** the same hand-rolled HAMT/refc layout re-derivation
  exists in `beam_message/src/layout.rs` + `copy.rs`, `beam_bifs/src/access.rs`, and `beam_gc/src/copy.rs`,
  because `beam_term::size::describe_object` and `beam_gc::copy::{move_size,scan_step}` are **private**.
  So the families duplication is the house pattern, not an isolated defect. Additionally `beam_bifs/src/phash.rs`
  duplicates `beam_term::hashing::{make_hash, make_hash2}` even though those are public.
- **Families that forward correctly (no duplication):** `records/raw.rs` (all layout via `beam_term::record`),
  `funs/` (`crate::terms::make_fun`), `floats/` (`beam_term::number::big_to_f64`, `try_make_small`),
  `exceptions/`, `tuples.rs`, `lists.rs`, `receive.rs`.

## Other semantics / divergence notes
- **The receive/message host contract (`receive.rs` + the `Mailbox` seam).** The engine **never inspects a message or a marker** — selection is the compiled matching instructions between `loop_rec` and `loop_rec_end`, and a marker term is opaque. `receive_skip`/`receive_remove`/`receive_reset` take **no candidate argument**: the host acts on its **own** persistent scan position (`receive_remove` unlinks whatever is at that position, NOT the term the engine read into `Dst`), so the engine's driving is correct only while that position is stable between the `loop_rec` that produced a candidate and the `remove_message`/`loop_rec_end` that consumes it — which holds by construction (the engine issues no other seam call and never blocks in between). A `receive` reaches `wait`/`wait_timeout` **only** via `loop_rec`'s `None` arm (the host scan is already exhausted), so a `resume_outcome` re-entry sees only messages appended since. Host contract: FIFO with a **saturating** scan index (`beam_message::QueueCursor` + `beam_rt::MailboxState`); pinned by `tests/receive.rs`.
- `receive.rs` `WaitTimeout`: a non-zero timeout suspends **with no timer** (the engine arms none); only the small integer `0` falls through to `pc+1`. A host recovers the parked deadline through `Machine::wait_deadline` and re-enters at the `timeout` continuation (`../machine/CONTEXT.md`).
- `receive.rs` `marker_call` classifies "host does not serve markers" by **exact string match** on `unsupported_opcode: {method}` — a fragile, string-based error taxonomy (documented as the discriminator).
- `bitstrings/matching/reads.rs` `decode_utf8` ignores `BSF_LITTLE` (documented: `bs_get_utf8 -little` is not meaningful in ERTS).
- `funs/` `call_fun`'s target-shape classification is **engine-side and kind-agnostic** (the header `fun_arity` field, which `beam_term::fun::write_fun_external` populates for an external `fun M:F/A` just as the local writer does), so a matching-arity external fun is *dispatched* and a mismatched one raises `{badarity,{Fun,Args}}`. The shape is pinned in-node: `crate::terms::make_fun_external` (`terms/fun.rs`, `#[cfg(test)]`-only — the production builder stays `FunKind::Local`) materializes a real `EXPORT_EXT` term and `funs/tests.rs` pins the arity classification for external `fun M:F/A` on both the dispatch and the `{badarity,_}` route.
- `records/mod.rs` `write_local_record` writes the same block **twice** (placeholder hash `0`, then the real `compute_record_definition_hash`) — intentional (the hash reads the block) but a double full write.
- `maps/mod.rs` `lookup`/`position` are O(n) `beam_term::cmp` scans (no hash); `maps/write.rs` makes `put_map_assoc` O(n²) in the pair count.
- `bitstrings/raw.rs` `subject_bytes` returns `&'a [u8]` with an unconstrained lifetime ("caller keeps it alive") — a safe-signature soundness caveat, documented as the `ExecutionContext` ownership contract.
- Dead/stub remnants (harmless): `bitstrings/mod.rs` `#[allow(dead_code)] const BSF_ALIGNED`.

## Measured — the baseline-JIT (default) tier on foreign workloads
`BENCH_TIER=baseline` (the default tier), a pinned core, `setsid -w`, one process at a time, on a **shared,
loud host**. Record the load band with every row: only the **ratios** and the deterministic structural
counters are meaningful, never a bare absolute — an absolute moves with the host load (observed ±15 %
between sessions and up to 5× on a loaded one).

| workload | min / median ns | instr/call | ns/instr |
|---|---|---|---|
| `cowboy_bstr:to_lower/1` | 3 410.4 / 3 447.3 | 438 | 7.87 |
| `cowboy_bstr:capitalize_token/1` | 4 376.7 / 4 404.2 | 660 | 6.67 |
| `emqx_base62:decode/1` | 4 581.0 / 4 634.8 | 531 | 8.73 |
| `emqx_base62:encode/1` | 2 648.6 / 2 656.8 | 244 | 10.89 |
| `bench_list_walk:walk/1` | 373 065 / 374 416 | 169 418 | 2.21 |

- **Deterministic structural counters are the primary evidence.** `rooted_calls_per_call` /
  `rooted_words_per_call`: `to_lower` **2 / 13**, `capitalize_token` **2 / 13**, `encode` 18 / 45,
  `decode` 32 / 133, `list_walk` 1000 / 2000; `driver_round_trips_per_call` 3 (`to_lower`),
  `in_block_calls_per_call` 30 with 0 declines, `refc_payloads_per_call` 1. Both comprehension workloads
  reserved **32 / 163** and **63 / 318** before the match-context reuse landed, and now reserve nothing
  beyond the single `bs_init_writable` pair.
- Paired `post/pre` medians for the changes that landed, each its own ≥7-round alternating run:
  match-context reuse + `bs_get_position` small integer `to_lower` 0.958 / `capitalize_token` 0.908
  (interpreter tier 0.963 / 0.963); `bs_create_bin` cold-body outlining `to_lower` 0.995 /
  `capitalize_token` 0.992 / `encode` 0.990 / `long_append` 0.982; the `bs_match` dispatch pre-test
  `to_lower` 0.982 / `capitalize_token` 0.982 / `decode` 0.990 / `encode` 0.990. `list_walk` measures
  0.995–1.005 medians against all of them (it executes no bitstring instruction), so the estimator
  carries no cross-workload bias.
- **Measured negative, do not re-attempt:** recognising the six emitted `bs_match` command shapes as
  whole-operand-list slice patterns ahead of the reference loop made `to_lower` **1.027×** *slower*
  (`capitalize_token` 1.016, `encode` 1.012; 9 alternating rounds, 9/9 ≥ 1.0). The residual
  `bs_match` cost belongs to the loader (see Handoffs).
- The writable-accumulator growth curve is unchanged for 64 KiB of 512-byte appends from a 1 KiB hint:
  1 payload handle, 2 in-place grows, 4 moves and 61 440 bytes copied. `bitstrings/create/grow_tests.rs`
  asserts the *shape*, not those exact figures (≤ 12 moves, `bytes_copied ≤ 2×` the appended bytes, ≥ 1
  in-place grow, exactly one payload handle), so a quadratic regression still fails it.

## Constraints
- `#![deny(unsafe_code)]` crate-wide; the node's two `#[allow(unsafe_code)]` modules are `bitstrings/raw.rs`
  and `records/raw.rs` (every `unsafe` block carries a `// SAFETY:` rationale).
- Errors are `beam_common::{Result, BeamError}` only — no crate-local error enum, no panic, no `unwrap()` on
  loaded data, no wrong result.
- Term construction/inspection is **only** `beam_term`; ordering uses `beam_term::{cmp, eq}`, never a reimplementation.
- Files ~1000 lines, split by responsibility; licensing Apache-2.0 (standard OTP header on every `*.rs`).

## Test Strategy
- Per-family `#[cfg(test)]` unit tests over hand-built `Instr`s with a recording `ExecutionContext` stub:
  `mod.rs`, `receive.rs`, `records/tests/*`, `exceptions/tests/*`, `funs/tests.rs`,
  `bitstrings/*/tests.rs`, `bitstrings/create/*_tests.rs` ,
  `bitstrings/matching/*_tests.rs` (incl. the `matching/elision_tests.rs` re-read-elision suite), `bitstrings/raw.rs`.
- The router's diagnostics contract is pinned by `mod.rs:321+` (`the_prefix_is_unchanged`); the receive
  family's tests assert an owned variant never reaches the sentinel again.
- End-to-end coverage lives in the crate integration suites (`../TEST_NOTES.md`).
- **No production-path `unwrap()`/`panic!`/`unreachable!` in any family** — every grep hit for those is inside
  a `#[cfg(test)]` module. This is a genuinely rigorous result.

## Known Issues
- **`bitstrings` differential-corpus hole (tools-owned).** `bitstrings/CONTEXT.md` records the missing
  `wire`/bitstring gate cases; the owner is `tools/differential`, not this crate.

## Handoffs (owned elsewhere — NOT closeable in this node)
- **`records` foreign-module `put_record/6` create form** (a `{OtherModule,Name}` record id) ⇒ typed `unsupported`;
  the oracle creates it (C `erl_record.c` consults the code server's record table). Needs a host record-definition
  source / code-server record table. **OWNER: `beam_rt`/`beam_bifs`** (a `RecordSource` seam). The running module's
  own `put_record/6` create form IS implemented (`records/mod.rs` `create_native_record`).
- **`receive.rs` markers / `after` timer** — the marker seam and a host timer for a non-zero `after` are the host's.
  **OWNER: `beam_rt`** (markers are served; the timer is not).
- **`exceptions/stacktrace.rs` supplied-frame atom source** — `encode_supplied_stacktrace` requires a `Module`
  carrying the ONE shared atom table; a name it cannot resolve is the typed `unsupported`. **OWNER: `beam_rt`** (its `-eval` route).
- **`bs_match`'s command-list generator expansion belongs in `crates/beam_loader`.** C never interprets a
  flattened command list at run time: `emu/ops.tab:1102` + `emu/generators.tab:1096-1112` make the
  loader/assembler generator explode `bs_match` at load time into `i_bs_ensure_bits`/`_unit`/`i_bs_read_bits`/
  `i_bs_eq`/`i_bs_extract_integer`/`i_bs_get_fixed_integer`/`i_bs_get_fixed_binary`/`i_bs_get_tail`/`i_bs_skip`/
  `i_bs_drop`, including the fused `i_bs_read_bits Ctx=x u==8 | i_bs_extract_integer u==8 Dst=x =>
  i_bs_read_integer_8`. Our loader preserves the raw list, so `run_commands` re-derives a load-time fact on
  every execution (a comprehension enters it once per byte, two commands each). An in-family re-discovery of
  the shapes was measured a **regression** (1.027× on `to_lower`), so the loader is the only route left.
  **OWNER: `crates/beam_loader`** (and `crates/beam_engine/src/lib.rs` for the `Instr` variants).
- **`src/lib.rs` declares `families` as a private module**, so every `pub(crate)` entry point inside this node
  (the match/dispatch/segment seams listed in `bitstrings/CONTEXT.md`) is unreachable from `src/jit/**`; a tier
  that wants a per-command entry needs `pub(crate) mod families` or targeted root re-exports. **OWNER: the
  parent `src/` node.**
- **`src/exec_census.rs` measured 2.20 % of `to_lower`'s self time while disabled** — the thread-local `bool`
  load sits on the per-instruction funnel. **OWNER: the parent `src/` node.**

## Notes for Agents
- The router (`mod.rs:112`) and `lists::exec` are `#[inline]`, so a dispatched instruction hands straight to its owning family.
- A family's `Ok(None)` is **not** a silent no-op: it reaches the router's last arm and yields the sentinel.
  The only way to "claim then no-op" would be an arm returning `Ok(Some(Step::Next))` without doing the
  operation — none exists (verified across all families).
- Ground truth (read-only, never modify): `../../../../erts/emulator/beam/emu/*.tab` and `erl_map.c`/`erl_bits.c`/`erl_record.c`.

## See Also
- `../CONTEXT.md` → the crate (in particular the execution model + `Handoffs`), `README`-level detail in `../INSTRUCTION_SURFACE.md`.
- `../../terms/` → the term builders/readers the families call (`terms::map::map_entries` for map enumeration).
- `../beam_term/src/{bitstring.rs,refc.rs,map.rs,record.rs}` → the LCA term layout these families must forward to.
