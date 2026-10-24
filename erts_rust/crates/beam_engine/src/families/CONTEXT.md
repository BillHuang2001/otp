# `beam_engine::families` — the per-opcode-family executors

## Intent
The **instruction-family seam** of the typed-IR engine: every `beam_loader::Instr` the core dispatch
loop (`../interpreter/`) does not execute natively is routed here by [`mod.rs::dispatch`] to the single
family that owns it.
One family per opcode group; each exposes `exec(...) -> Result<Option<Step>>` where `Ok(Some(step))` =
"this family executed it", `Ok(None)` = "not my variant", `Err` = the family raised.
Family ownership is **disjoint and variant-complete** (`mod.rs`'s `dispatch` arms): each of the 77 claimed
`Instr` variants has exactly one owning family, so the route is a jump table on the `Instr` discriminant —
semantically identical to the old first-claim-wins probe loop (see the module doc). One arm is a
**predicate** rather than a variant list: the bitstring family's `_ if bitstrings::claimed(instr)`
(`mod.rs:206`), which is the family's own statement of its variant set (see API Surface).

## API Surface
- `pub(crate) fn dispatch(machine, entry_module, module, labels, instr, func, pc, ctx) -> Result<Step>` — `mod.rs:124`.
- `pub(crate) fn lists::put_list(machine, head, tail, dst, ctx)` / `lists::get_list(machine, module, source,
  head, tail, ctx)` — both `-> Result<Option<Step>>`, the two bodies the router's hot fast-path arms call
  directly (see Notes for Agents); `lists::exec` calls the same two, so ownership cannot drift.
- `pub(crate) fn bitstrings::claimed(&Instr) -> bool` (`bitstrings/mod.rs:135`) — the bitstring family's **one**
  statement of the 20 `Bs*` variants it owns. `dispatch` routes on it, and so do the native baseline tier's
  emitted arm (`jit/baseline/emit/block.rs`) and its in-block helper (`jit/baseline/bitstr.rs`), so the
  router and the tier cannot disagree about who owns an opcode; the family's `exec` `Ok(None)` contract is
  the authority it is pinned against (`jit/baseline/bitstr_tests.rs`).
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
- **The latch is duplicated only where a caller bypasses the router.** `bitstrings::exec` installs it itself
  (`bitstrings/mod.rs:176`), because the native baseline tier's `jit_bitstr` calls that entry directly; on
  the routed path it is a second `bool` load and a not-taken branch, and the family that owns it also owns
  the raise sites it protects. Pinned by `bitstrings/mod.rs`'
  `exec_installs_the_class_atom_anchor_for_a_direct_caller`.
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
  The engine makes **no host consultation between `loop_rec`'s `None` answer and the park** — the `None` arm is one `Step::Goto` to the fail label (`wait`/`wait_timeout`) that then suspends, and `receive_next` is the only receive-seam call in the whole scan. C's `i_loop_rec` instead drains the receiver's own incoming **signal** queue there (`erts_proc_sig_receive_helper`, `emu/msg_instrs.tab:80-112`) and re-peeks before it may jump to the `wait`, so a host that keeps signals in a queue separate from the mailbox must reproduce that drain **inside `receive_next`** before it answers `None`; the only other mid-slice host point is `ExecutionContext::reduce`, which is charged per **interpreted** instruction and only per driver-loop iteration on the native tier.
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
  now allocate exactly what C allocates — `to_lower`/`capitalize_token` **13 words/call** — and `decode`
  133, `encode` 45 (below the oracle's 57): nothing beyond the single `bs_init_writable` pair plus each
  iteration's own field binary and match context.
- **Oracle word parity — the strongest evidence in this section.** Reclaimed words per call from
  `erlang:statistics(garbage_collection)` on the real system ERTS (OTP 29.1.1 / erts-17.1),
  `{min_heap_size, 233}`, N = 200 000, in a dedicated process, on the harness' own inputs
  (`<<"The Quick Brown Fox 0123456789">>`, `<<"the-quick-brown-fox-0123456789">>`, `<<1..12>>`,
  `<<"ABCDEFGHIJKLMNO">>`):

  | workload | ERTS words/call | ours | verdict |
  |---|---|---|---|
  | `cowboy_bstr:to_lower/1` | 13.00 | 13 | exact |
  | `cowboy_bstr:capitalize_token/1` | 13.00 | 13 | exact |
  | `emqx_base62:decode/1` | 133.00 | 133 | exact |
  | `emqx_base62:encode/1` | 57.00 | 45 | ours lower (the oracle's extra words are `iolist_to_binary/1` inside the harness BIF floor) |

  The three binary-comprehension workloads allocate **word-for-word what C allocates**, so no
  avoidable allocation remains on these paths. Per-call decomposition and the allocator's absence
  from the `perf` profile: `bitstrings/CONTEXT.md` → `## Measured`.
- Paired `post/pre` medians for the changes that landed, each its own ≥7-round alternating run:
  match-context reuse + `bs_get_position` small integer `to_lower` 0.958 / `capitalize_token` 0.908
  (interpreter tier 0.963 / 0.963); `bs_create_bin` cold-body outlining `to_lower` 0.995 /
  `capitalize_token` 0.992 / `encode` 0.990 / `long_append` 0.982; the `bs_match` dispatch pre-test
  `to_lower` 0.982 / `capitalize_token` 0.982 / `decode` 0.990 / `encode` 0.990; the
  `read_size`/`decode_flags` hot/cold split `capitalize_token` **0.934**, `to_lower` **0.953**,
  `encode` **0.971**, `decode` **0.972** (7 rounds, load band 8.43–10.25, 7/7 rounds below 1.0 on
  all four, counters unchanged). `list_walk` measures
  0.995–1.005 medians against all of them (it executes no bitstring instruction), so the estimator
  carries no cross-workload bias.
- **Measured negatives, do not re-attempt:** recognising the six emitted `bs_match` command shapes as
  whole-operand-list slice patterns ahead of the reference loop made `to_lower` **1.027×** *slower*
  (`capitalize_token` 1.016, `encode` 1.012; 9 alternating rounds, 9/9 ≥ 1.0); inlining
  `matching::match_ctx::read_ctx`/`reread_ctx` regressed every bitstring workload (`to_lower` 1.026,
  `capitalize_token` up to 1.050); and testing `small_value` before `raw::big_parts_safe` in
  `create/mod.rs`'s integer-segment classification made `to_lower` 1.023 / `encode` 1.009 slower.
  Full numbers: `bitstrings/CONTEXT.md` → `## Measured`. The residual
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

## Rooting / lifetime audit of the families (verified against the code)
Every in-crate construction arm routes through a `*_rooted` twin and obeys reserve -> read -> write; the
findings below are the residuals that survived that pass. Mechanism that makes a stale word possible at all:
`beam_rt`'s `call_registered` **rewinds** the pooled 16 MiB `bif_heaps` region to its call `mark`
(`beam_rt/src/context/calls.rs`), and `Runtime::grow_eval_heap` **retires** an exhausted region; a term built
in either and read afterwards is the "dangling boxed argument" class of `../../../../beam_rt/M1_GAPS_GC.md`
(fixed once for a raise reason by commit `2f1bdc1909`'s copy-at-the-escape-boundary).
- **`reserve_slack(machine, ctx, 0)` still runs the rooted door.** `terms/slack.rs`'s `reserve` ->
  `machine.alloc_rooted(ctx, 0, ALL_LIVE)` is not short-circuited for `n == 0`, so a builder whose computed
  graph size is zero (`symbolic_stacktrace`'s `total = 2*shapes.len() + Σ ItemShape::words + if head {2}`
  with an empty/skipped snapshot and `head == false`; `carve_supplied_frames` likewise) performs a
  collection-capable admission for a graph that needs no storage. Benign today (the shape metadata holds only
  `Atom`s and borrowed `&str` file names, and `Sink::list(&[])` returns `NIL` without carving), but it is a
  pointless admission point in the raise path — skip the reservation at `total == 0`.
- **`MakeFun3` builds the closure block with no root between the reserve and the write.** The fun block is a
  Rust-local `Eterm` until `write_operand(dst, fun)`, so the only thing keeping it alive is that
  `make_fun_rooted`'s reader closure performs *pure operand loads*. That invariant is load-bearing and
  undocumented at the call site (contrast `raise_badarity`, which states it explicitly).
- **`apply_target` / `call_fun` re-derive every seam-read term from the machine after the seam.** `register_args`
  copies `x(0..arity)` into a `Vec<Eterm>` that is **not** in any `RootSet`, so it is never held across a host seam:
  `apply_target` builds it only *after* `ctx.resolve_external_call` declines and re-reads the callee's atom words
  (`x(arity)`/`x(arity+1)`) after `dispatch_bif_call` before building the `undef` head item, while `call_fun`
  re-reads the target through `FunSource::read` after `ctx.resolve_local_fun` and immediately before
  `terms::fun_free` borrows it (with nothing allocation-capable between that read and the `set_x` free-variable
  writes), and re-collects both the fun and the argument list for the `ctx.apply_fun_outcome` fallback. The engine
  therefore does not depend on any host's non-collecting property at these seams; `tests/fun_seam_rooting.rs` pins
  it under a seam that collects.
- **`total == 0` aside, the map builder's bound is sound (do not re-open).** `beam_term::map::map_words_capacity`
  = `21n + 18` for `n > 32`, and the true HAMT footprint is `2n` cells + `trie_words` = `2N + g` where `N` is
  the node count and `g` the hash-group count; since every node exists only from a >=2-way split,
  `n_L <= g/2` at each of the 15 levels, so `trie_words <= 16g + 2` and the block needs `<= 18n + 2`. The
  `debug_assert!` in `write_map` therefore never fires in release, and `terms::make_map_rooted`'s single
  reservation cannot overrun into its neighbours.
- **`InPlace`'s raw payload base is republished, not fragile.** `BitWriter::push_bits_from` promotes on
  `source_overlaps` **before** any growth (order explicitly load-bearing), and `InPlace::grow` updates
  `self.base` + `republish_over(self.src, new_base)`.
  `republish_over` writes **exactly the two words that name the payload** — `BinRef.val` (word 1 of the
  `orig` block) and `ErlSubBits.base_flags` (word 1 of the `src` block, flag bits preserved) — as **two
  independent writers**, re-deriving `orig` fresh from `src` on every call. It must never be replaced by
  `beam_term::refc::write_refc_binary`, which writes the whole contiguous 8-word
  `[BinRef(3)][ErlSubBits(5)]` pair from one address: a collection copies the pair as **two separate
  allocations** (`beam_gc::copy::move_size` copies the `ErlSubBits` block's own five words and forwards
  the `BinRef` as its child), so that adjacency does not hold — such a write spills five words past the
  end of the 3-word `BinRef`. Pinned by `bitstrings/raw_tests.rs`'s split-pair canary cases
  (`a_split_pairs_payload_move_keeps_the_next_append_in_place` plants an 8-word canary directly behind
  the `BinRef` and asserts it word-for-word; a whole-pair write fails it in **release**, not only via
  `debug_assert`). A first-position append after a move stays **in place**, so the amortisation is
  unchanged.
  The one surviving caveat is `raw::subject_bytes`'
  unconstrained `&'a [u8]` (see `## Duplication with sibling crates`): a caller must re-derive it after any
  `ensure_room`, which the current append arm does not need because it grows only through the writer.

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
- **`src/lib.rs` declares `families` as `pub(crate) mod families`**, so this node's `pub(crate)` entry points
  (the match/dispatch/segment seams listed in `bitstrings/CONTEXT.md`) are reachable from `src/jit/**` directly;
  no item inside is `pub`. `bitstrings`' `claimed(&Instr)` is the single statement of that family's variant
  ownership, consumed by the router and by the baseline emitter.
- **The census gate is the latched `Machine::census_on` field** — one load and a branch on the per-instruction
  funnel, the same latched-gate idiom as `trace_on`/`reduce_on`; a disabled census performs no thread-local
  access per instruction (figures: `../MEASURED.md`). **OWNER: the parent `src/` node.**## Notes for Agents
- The router (`mod.rs:124`) and every family `exec` are **inlined into one function** — the measured shape
  (outlining the family bodies with `#[inline(never)]` cost **+2.52 % instructions / +3.34 % cycles** on
  `bench_gc_bound:churn/1`, since each dispatched instruction then pays a call/ret pair and a second
  prologue). Two consequences:
  - The **only** arm kept out of the hot text is the diagnostic one: `mod.rs`'s `unclaimed()` is
    `#[cold] #[inline(never)]` so the `format!`/reason-atom path does not sit in the router's frame.
  - The two hottest dispatched instructions are routed **straight to their bodies**: the router's
    `Instr::PutList`/`Instr::GetList` arms call `lists::put_list`/`lists::get_list` with the already-bound
    operands, because going through `lists::exec` re-matched the variant the router had just proved (a
    second jump table) and re-derived the operand slice. `lists::exec` remains the family's complete
    ownership statement and calls the same bodies; `GetHd`/`GetTl` still route through it.
- `operand_slack_words` (`../operand.rs`) answers 0 for every operand kind but a non-small
  `Integer`/`Character`; its 60-bit range test sits behind an `#[inline(never)]` helper so the
  register-operand path is one tag compare (the branchless select it replaced evaluated the range check
  for both `put_list` operands: **-2.12 % instructions** on the churn row). `read_operand_pure` is
  `#[inline(always)]` because its 72-byte `Result` return otherwise round-trips through the caller's sret
  slot per operand (**-2.84 % instructions**). Together with the `put_list`/`get_list` routing this is
  **-6.03 % instructions / -4.17 % cycles** on `bench_gc_bound:churn/1`; measured rows and the A/A noise
  floor are in `../PERF.md`.
- A family's `Ok(None)` is **not** a silent no-op: it reaches the router's last arm and yields the sentinel.
  The only way to "claim then no-op" would be an arm returning `Ok(Some(Step::Next))` without doing the
  operation — none exists (verified across all families).
- Ground truth (read-only, never modify): `../../../../erts/emulator/beam/emu/*.tab` and `erl_map.c`/`erl_bits.c`/`erl_record.c`.

## Per-family opcode ownership (moved from `../INSTRUCTION_SURFACE.md`)
One `src/families/` module per family; the `exec`/`dispatch` contract is the router section above, and each module's own `CONTEXT.md` owns its edge-case detail.
- `tuples.rs`: `PutTuple2`, `GetTupleElement`, `SetTupleElement`, `SelectTupleArity` (a non-tuple source or shape/arity mismatch takes `fail`; `{badmatch, Value}` only on the no-fail path; `SetTupleElement` writes `tuple_val(Tuple)[1 + offset]` **in place**, raising the read path's `{badmatch, Source}` for a non-tuple/out-of-range target and the typed `unsupported` for a term in a module's immutable literal area).
- `records/{mod,raw}.rs`: `UpdateRecord`, `IsAnyNativeRecord`, `IsNativeRecord`, `GetRecordElements`/`GetRecordElementsId`, `PutRecord`, `IsRecordAccessible`, `GetRecordField`, `UpdateRecordId` -- the whole family, `erl_record.c` branch for branch (incl. `PutRecord`'s *create* form, where `badfield` outranks a delayed `novalue`, and C's export/scope rules: `IsRecordAccessible` strict only when `scope = external`, else the `$MODULE` wildcard, and `GetRecordField` needs no export for an atom `Id`); a foreign-module id resolves through `ExecutionContext::foreign_record_definition` (exported definitions only, else the C `{badrecord,Id}` raise), and a host answering `Ok(None)` is the typed `unsupported`.
- `lists.rs`: `PutList`, `GetList`, `GetHd`, `GetTl` (a non-cons source raises `{badmatch, Value}`).
- `funs/{mod,tests}.rs`: `MakeFun3` (arity = descriptor arity minus `num_free`), `CallFun` (fun in `x(arity)`, args `x(0..arity)`), `CallFun2`, `Apply` (M in `x(arity)`, F in `x(arity+1)`), `ApplyLast` (pop the frame first). `CallFun`/`CallFun2` consult the in-machine closure-apply seam `ExecutionContext::resolve_local_fun` **first** (the shared `call_fun`): on `Some((callee, label))` the engine copies the closure's free variables into `x(arity..arity + num_free)` (C's `reg[i + arity] = funp->env[i]` order -- arguments first, captures after), registers `callee` as a module context, resolves `label` against that module's **own** label map, stores the caller's return address in the live frame and reports `Step::Enter`, so a `receive`/raise inside the closure stays in *this* process; on the seam's `Ok(None)` default the value-returning `ExecutionContext::apply_fun` is the fallback (the fun kinds with no local label -- an external `fun M:F/A` and the `-eval` evaluator's closure). `Apply`/`ApplyLast` resolve their `M,F` target through the cross-module-call seam (`ExecutionContext::resolve_external_call`) **first**: on `Some((callee, label))` the callee is registered as a module context and entered **in this machine** -- `Apply` stores the **caller's** return address (`Continuation::Address { func, pc + 1, context: current_context() }`) in the live frame and reports `Step::Enter`, and `ApplyLast` reports `Step::Enter` on its already-popped frame (so the callee's `return` follows the caller's continuation); on the seam's `Ok(None)` default the target is the synchronous `call_bif` route, and `ApplyLast` then follows its (popped) frame's continuation context-aware via `Machine::continuation_step` (a tail apply must return into the **caller's** module, so a continuation in another module context yields `Step::Enter`, never a bare `Goto`). A non-atom `apply` target is `badarg`. Every raise from the applied target -- the `Apply`/`ApplyLast` host BIF error and the `call_fun`/`call_fun2` seam/fallback error alike -- routes through `Machine::raise_bif_error`, so it lands in a live `try`/`catch` (the `apply` path reports it as `ApplyTarget::Raised`) instead of escaping; the `call_ext*`-encoded `erlang:apply/3` shares `apply_target` through `resolve_call_ext_apply`, while `erlang:apply/2` is not special-cased.
- `exceptions/{mod,raise,stacktrace,tests}.rs`: `Badmatch`, `CaseEnd`, `TryCaseEnd`, `IfEnd`, `Badrecord`, `Raise` (re-raise with the handled class), `Try`/`Catch`, `TryCase`/`TryEnd`/`CatchEnd` (each consumes one catch frame), `BuildStacktrace` (`in Known Issues`), `RawRaise`.
- `floats/{mod,arith,convert,tests}.rs`: `Fmove`, `Fconv`, `Fadd`/`Fsub`/`Fmul`/`Fdiv`, `Fnegate` (non-float operand / non-finite result -> `fail` when live, else `badarith`; `Fconv` converts a bignum via `beam_term::number::big_to_f64`, over-range -> `badarith`).
- `maps/{mod,read,write,tests}.rs`: `GetMapElements`, `HasMapFields`, `PutMapAssoc`, `PutMapExact` (both layouts read via `terms::map_entries`; keys via `beam_term::cmp` so `1` and `1.0` differ; a non-map source `fail`s on the compiled forms, else `{badmap, Source}`; built via `terms::make_map`, >32 keys -> hashmap; the write side sizes its reservation from the O(1) head-node size word, so the source is walked once per instruction).
- `bitstrings/{mod,raw}.rs` + `{matching,create}/*`: `BsStartMatch3`/`4`, `BsMatch`, `BsMatchString`, `BsTestTail2`, `BsGetTail`, `BsGetPosition`/`BsSetPosition`, `BsGetInteger2` (any width), `BsGetFloat2`, `BsGetBinary2`, `BsSkipBits2`, the UTF reads, `BsCreateBin` (integer/binary/float/utf8/16/32/string), `BsInitWritable` + an `append`/`private_append` segment in **any** position (the source's bits are concatenated at the point the segment appears — in place when it is the first `private_append` of a writable accumulator, which grows the payload rather than abandoning it, copy-rebuild otherwise). A result is an on-heap `ErlHeapBits` <=64 bytes else an off-heap refc binary; the match-context is an on-heap `ErlSubBits`-shaped box re-derived per opcode, so matching survives calls and branches.
- `receive.rs` (registered **last**; no term inspection -- every mailbox position/removal goes to the `Mailbox` seam): `Send` (deliver `x(1)` to `x(0)`, store **the message itself** in `x(0)`), `LoopRec` (candidate -> `Dst`, else `Fail`), `LoopRecEnd`, `Wait`, `WaitTimeout` (a small-int `0` falls through to `timeout`, else suspends like `wait`), `Timeout`, `RemoveMessage`, the four `RecvMarker*`.

## Per-opcode core-loop arms (from `INSTRUCTION_SURFACE.md`)
Moved verbatim from `INSTRUCTION_SURFACE.md`; section cross-references inside the text name sections of that file unless stated otherwise.
**Core-loop arms.** Metadata/no-ops `Label`, `Line`, `IntCodeEnd`, `ExecutableLine`, `DebugLine`, `NifStart`, `OnLoad` (`ops.tab` lists them with no body, so `Step::Next` is exact). `FuncInfo` raises class `error` with the **bare atom** `function_clause` from the running module's atom table (call args ride in the stacktrace, not the reason; a module lacking the atom is a typed `unsupported`) -- a function's callable entry label is the label *after* `func_info`, reachable only by a JUMP from a failed head-pattern/guard `fail` label, matching C's `i_func_info_IaaI`; the baseline JIT deopts it. The uncaught raise goes through the uniform funnel `Machine::with_uncaught_frames`, which attaches the frames `Machine::raise_stacktrace(entry_module, func, pc)` derives from the live call frames (the raising function first, each frame resolved in its own module context), so the bare-atom reason is still localizable; a caught raise is unaffected (the frames are metadata on the uncaught `Raise` only). `Move`/`Swap`/`InitYregs`. `Test` (**all** `TestKind`s) and `SelectVal` (exact `=:=` against each `(value, label)`, else `fail`, else -- `fail == NO_FAIL` -- class `error` with the bare atom `function_clause`). The control ops `Jump`/`Call`/`CallLast`/`CallOnly`/`CallExt`/`CallExtOnly`/`CallExtLast`/`Allocate`/`AllocateHeap`/`TestHeap`/`Deallocate`/`DeallocateReturn`/`Trim`/`Return` (frame discipline in sec. Registers, stack & frames; the rest match C), `GcBif`/`Bif0`/`Bif`, and the eight native-record variants. `Instr::Generic` executes **nothing** (see the dispatch loop). `DeallocateReturn(N)` is the loader's fused `deallocate N; return` superinstruction (`beam_loader::lower::fuse`): `pop_frame_checked(N)` then the shared `Machine::return_step` the plain `Return` arm also uses; `Deallocate`/`Return` stay live for hand-built modules that bypass the loader.

## The mailbox seam: the nine methods and the engine-side defaults (from `../CONTEXT.md`)
Moved verbatim from the crate `CONTEXT.md` (which keeps the contract and a pointer).
The mailbox lives in the Stage-5 runtime (`beam_process`/`beam_message`), never here: the host supplies it through this gate.
`pub trait Mailbox` is exactly nine `Result`-returning methods: `send_message`; the scan position `receive_next` (the candidate at the position, `None` = exhausted), `receive_skip` (`loop_rec_end`, advance one), `receive_remove` (`remove_message`, unlink the candidate then reposition at first) and `receive_reset` (`timeout`, reposition at first); and the four marker calls `receive_marker_reserve`/`receive_marker_bind`/`receive_marker_use`/`receive_marker_clear`.
- `ExecutionContext` carries the **same nine names as defaulted methods**, each returning the typed `error::unsupported("<method>")`, so every implementor compiles unchanged; `MinimalContext::with_mailbox`/`set_mailbox` install one.
- **The engine never inspects a message or a marker**: selection is the compiled code's job (the matching instructions between `loop_rec` and `loop_rec_end`), and a marker term is **opaque** (produced by `receive_marker_reserve`, handed back verbatim by `receive_marker_bind`; only a separate *reference* reaches `receive_marker_use`/`receive_marker_clear`). Installing a mailbox does **not** change the non-blocking entries: `run_export`/`run_label` still surface the typed `error::blocked` (Handoff #1).
- **`send_message_with_off_heap(to, message, src_binaries)`** is the `Instr::Send` arm's entry **and** the `call_ext*` `erlang:send/2`/`'!'/2` case's entry: a tenth defaulted `ExecutionContext` method whose default **forwards to `send_message` and discards `src_binaries`**, so every existing implementor is byte-identical. `src_binaries` is the **source off-heap table** the receiver-side copy needs to resolve an engine-built off-heap (refc) binary in the message. See the off-heap Known-Issues bullet.
- **Ordering invariant (the host contract this driving relies on).** `receive_skip`/`receive_remove`/`receive_reset` take **no candidate argument** -- the host acts on its **own** persistent scan position, so the driving is correct only while that position is stable between the `loop_rec` that produced a candidate and the `remove_message`/`loop_rec_end` that consumes it; that holds by construction (between those two the engine issues no other seam call and never blocks). A `receive` reaches `wait`/`wait_timeout` **only** via `loop_rec`'s `None` arm (the scan is already **exhausted**), so a `resume_outcome` re-entry sees only messages appended since. Host contract: FIFO with a **saturating** scan index (`beam_message::QueueCursor` + `beam_rt::MailboxState`). Detail: `src/families/CONTEXT.md`, `tests/receive.rs`.

## See Also
- `../CONTEXT.md` → the crate (in particular the execution model + `Handoffs`), `README`-level detail in `../INSTRUCTION_SURFACE.md`.
- `../../terms/` → the term builders/readers the families call (`terms::map::map_entries` for map enumeration).
- `../beam_term/src/{bitstring.rs,refc.rs,map.rs,record.rs}` → the LCA term layout these families must forward to.
