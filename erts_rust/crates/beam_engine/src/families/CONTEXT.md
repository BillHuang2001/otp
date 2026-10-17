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
- `pub(crate) fn dispatch(machine, module, labels, instr, func, pc, ctx) -> Result<Step>` — `mod.rs:112`.
- `pub(crate) const UNCLAIMED_PREFIX: &str = "widened opcode not yet executed"` — `mod.rs:82` (frozen sentinel, pinned by `mod.rs:321`).
- Every family: `pub(crate) fn exec(machine, module, labels, instr, func, pc, ctx) -> Result<Option<Step>>`.
- All computation goes through `crate::context::ExecutionContext`; term construction via `crate::terms`.
  Families build no term layout of their own **except** `bitstrings/raw.rs` (see Duplication).

## Routing Table (file → owned variants)
- `tuples.rs` — `PutTuple2` / `GetTupleElement` / `SetTupleElement` / `SelectTupleArity`; `SetTupleElement` is the destructive in-place write (`tuple_val(Tuple)[1 + offset]`, `../terms/update.rs`), whose non-tuple/out-of-range target raises the read path's `{badmatch, Source}` and whose literal-area target is the typed `unsupported`.
- `records/` (`mod.rs` + `raw.rs` + `tests/`) — `UpdateRecord`, `UpdateRecordId`, and the eight native-record variants (`IsAnyNativeRecord` … `UpdateRecordId`). `raw.rs` **forwards all** record layout to `beam_term::record`.
- `lists.rs` — `PutList` / `GetList` / `GetHd` / `GetTl`.
- `funs/` — `MakeFun3` / `CallFun` / `CallFun2` / `Apply` / `ApplyLast` (builds closures via `crate::terms::make_fun`). `call_fun` **classifies its target's shape before the closure-apply seam** and raises the oracle's structured `error` reasons `{badfun,Fun}` / `{badarity,{Fun,Args}}` (C's `call_fun` + `expand_error_value`); see `funs/CONTEXT.md`.
- `exceptions/` (`mod.rs`, `raise.rs`, `stacktrace.rs`, `tests/`) — `Try`/`Catch`/`Badmatch`/`CaseEnd`/`TryCaseEnd`/`Badrecord`/`IfEnd`/`Raise`/`TryCase`/`TryEnd`/`CatchEnd`/`BuildStacktrace`/`RawRaise`.
- `floats/` (`mod.rs`, `arith.rs`, `convert.rs`) — `Fmove`/`Fconv`/`Fadd`/`Fsub`/`Fmul`/`Fdiv`/`Fnegate`.
- `maps/` (`mod.rs`, `read.rs`, `write.rs`) — `GetMapElements`/`HasMapFields`/`PutMapAssoc`/`PutMapExact`.
- `bitstrings/` (`mod.rs`, `raw.rs`, `create/`, `matching/`) — the whole `Bs*` family; `raw.rs` is this sub-node's `#[allow(unsafe_code)]` module (`records/raw.rs` is the other one in the node); `create/` = `bs_create_bin`, `matching/` = the match commands. Endianness: `matching/reads.rs::field_is_little` is the single rule for **reads**, `mod.rs::BitWriter::push_le_groups` for **writes**, and `mod.rs::flag_bit` normalizes a `native` flag atom; `BSF_NATIVE` is a **live** flag (a standalone read's raw `u32` flags operand carries it).
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
- `records/mod.rs` `fail_step`: `fail == NO_FAIL` ⇒ `unsupported("{instr}/no_fail")`.
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
- **`bitstrings/raw.rs` hand-rolls term layout a PUBLIC `beam_term` writer/reader already owns** (reachable,
  fixable — unlike the map walk below):
  - `raw.rs` re-declares `const ERL_ONHEAP_BINARY_LIMIT = 64` == `beam_term::refc::ERL_ONHEAP_BINARY_LIMIT` (`beam_term/src/refc.rs:103`).
  - `raw.rs` `make_bits` mirrors `beam_term::bitstring::write_heap_bits` (`beam_term/src/bitstring.rs:170-198`) —
    including zeroing the **whole** reserved data region (padding bytes of the last data word included) before
    the payload copy, matching the LCA invariant.
  - `raw.rs` `make_ctx` copies `beam_term::bitstring::write_sub_bits` (`bitstring.rs:234-251`, `base_flags = 0`).
  - `raw.rs` `ctx_parts` copies `beam_term::bitstring::sub_bits_parts` (`bitstring.rs:267-279`); `set_ctx_start` writes word 2 directly (no LCA setter exists).
  - `raw.rs` `heap_bits` derives the data pointer itself instead of `beam_term::bitstring::bitstring_parts` (`bitstring.rs:299`; used only for the refc case).
  - **Forwarded (no duplication):** the bignum path (`raw.rs` uses `beam_term::{big_size_words, write_big, big_parts}`) and the refc path (`raw.rs` uses `beam_term::refc::{ERL_REFC_BITS_SIZE, write_refc_binary, is_refc_binary}`).
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
- `funs/` `call_fun`'s target-shape classification is **engine-side and kind-agnostic** (the header `fun_arity` field, which `beam_term::fun::write_fun_external` populates for an external `fun M:F/A` just as the local writer does). The engine has **no safe external-fun builder** (`crate::terms` builds only `FunKind::Local`), so the external-fun arity shape is pinned by the C source (`fun_arity(funp)`), the header-encoding invariants and an oracle probe rather than an in-node unit test.
- `records/mod.rs` `write_local_record` writes the same block **twice** (placeholder hash `0`, then the real `compute_record_definition_hash`) — intentional (the hash reads the block) but a double full write.
- `maps/mod.rs` `lookup`/`position` are O(n) `beam_term::cmp` scans (no hash); `maps/write.rs` makes `put_map_assoc` O(n²) in the pair count.
- `bitstrings/raw.rs` `subject_bytes` returns `&'a [u8]` with an unconstrained lifetime ("caller keeps it alive") — a safe-signature soundness caveat, documented as the `ExecutionContext` ownership contract.
- Dead/stub remnants (harmless): `bitstrings/mod.rs` `#[allow(dead_code)] const BSF_ALIGNED`.

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
  `bitstrings/*/tests.rs`, `bitstrings/create/*_tests.rs`, `bitstrings/matching/*_tests.rs`, `bitstrings/raw.rs`.
- The router's diagnostics contract is pinned by `mod.rs:321+` (`the_prefix_is_unchanged`); the receive
  family's tests assert an owned variant never reaches the sentinel again.
- End-to-end coverage lives in the crate integration suites (`../TEST_NOTES.md`).
- **No production-path `unwrap()`/`panic!`/`unreachable!` in any family** — every grep hit for those is inside
  a `#[cfg(test)]` module. This is a genuinely rigorous result.

## Known Issues
- **`records`' `NO_FAIL` fail-arm diverges from `maps`' — OPEN, engine-side, small.** `records/mod.rs:463-475`
  `fail_step` returns the typed `unsupported("<instruction>/no_fail")` for `NO_FAIL`, while
  `maps/mod.rs:260-271` `fail_or_function_clause` forwards the same situation to the engine's single
  failed-clause definition `crate::interpreter::raise_function_clause_no_fail`. C's `$FAIL(0)`
  (`emu/macros.tab:56`) is an unresolvable self-jump — `NO_FAIL` is only ever emitted where the compiler
  proved the test cannot fail — so neither choice can be observed from compiler-generated IR; the divergence
  is a consistency defect (two different answers for one unreachable state), not a wrong value. Fix: route
  `fail_step`'s `NO_FAIL` arm through `raise_function_clause_no_fail`, which needs `machine`/`module` threaded
  into `fail_step` from its four call sites.
- **`bitstrings/raw.rs:69` re-declares `const ERL_ONHEAP_BINARY_LIMIT = 64` and hand-rolls the heap-binary
  layout** instead of forwarding to `beam_term::bitstring` as `records/raw.rs` already forwards to
  `beam_term::record`. A duplicated frozen constant: if `beam_term`'s limit ever moves, this copy silently
  disagrees. OPEN, engine-side, small.
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
