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
- `pub(crate) const UNCLAIMED_PREFIX: &str = "widened opcode not yet executed"` — `mod.rs:82` (frozen sentinel, pinned by `mod.rs:315`).
- Every family: `pub(crate) fn exec(machine, module, labels, instr, func, pc, ctx) -> Result<Option<Step>>`.
- All computation goes through `crate::context::ExecutionContext`; term construction via `crate::terms`.
  Families build no term layout of their own **except** `bitstrings/raw.rs` (see Duplication).

## Routing Table (file → owned variants)
- `tuples.rs` — `PutTuple2` / `GetTupleElement` / `SetTupleElement` / `SelectTupleArity`; `SetTupleElement` is the destructive in-place write (`tuple_val(Tuple)[1 + offset]`, `../terms/update.rs`), whose non-tuple/out-of-range target raises the read path's `{badmatch, Source}` and whose literal-area target is the typed `unsupported`.
- `records/` (`mod.rs` + `raw.rs` + `tests/`) — `UpdateRecord`, `UpdateRecordId`, and the eight native-record variants (`IsAnyNativeRecord` … `UpdateRecordId`). `raw.rs` **forwards all** record layout to `beam_term::record`.
- `lists.rs` — `PutList` / `GetList` / `GetHd` / `GetTl`.
- `funs/` — `MakeFun3` / `CallFun` / `CallFun2` / `Apply` / `ApplyLast` (builds closures via `crate::terms::make_fun`).
- `exceptions/` (`mod.rs`, `raise.rs`, `stacktrace.rs`, `tests/`) — `Try`/`Catch`/`Badmatch`/`CaseEnd`/`TryCaseEnd`/`Badrecord`/`IfEnd`/`Raise`/`TryCase`/`TryEnd`/`CatchEnd`/`BuildStacktrace`/`RawRaise`.
- `floats/` (`mod.rs`, `arith.rs`, `convert.rs`) — `Fmove`/`Fconv`/`Fadd`/`Fsub`/`Fmul`/`Fdiv`/`Fnegate`.
- `maps/` (`mod.rs`, `read.rs`, `write.rs`) — `GetMapElements`/`HasMapFields`/`PutMapAssoc`/`PutMapExact`.
- `bitstrings/` (`mod.rs`, `raw.rs`, `create/`, `matching/`) — the whole `Bs*` family; `raw.rs` is the node's **only** `#[allow(unsafe_code)]` module; `create/` = `bs_create_bin`, `matching/` = the match commands.
- `receive.rs` — `Send`/`LoopRec`/`LoopRecEnd`/`Wait`/`WaitTimeout`/`Timeout`/`RemoveMessage`/`RecvMarkerReserve`/`RecvMarkerBind`/`RecvMarkerUse`/`RecvMarkerClear`.

## The router sentinel + the honest-error convention
- A variant **no** family owns, or one a family declines with `Ok(None)`, reaches `mod.rs:221-224`:
  `Err(error::unsupported("<UNCLAIMED_PREFIX>: <name>"))`.
- `instr_name` (`mod.rs:239-248`) knows only `bif0`/`bif`/`init_yregs`/`executable_line`/`debug_line`; every
  other unclaimed variant degrades to the literal `"instruction"` (documented diagnosability loss).
- **No family claims a variant and then silently no-ops it.** Verified by reading all nine families +
  submodules: every `Instr` arm performs the operation, raises, or returns a typed `unsupported`.
- Every capability gap is a typed `error::unsupported(...)`/`BadArg`/`SystemLimit` — **never** a wrong value.

## Capability gaps — each a typed `unsupported` (NEVER a wrong value)
- `records/mod.rs:429-439` `fail_step`: `fail == NO_FAIL` ⇒ `unsupported("{instr}/no_fail")`.
- `records/mod.rs:745-769` `local_definition`: a **foreign** `{Module,Name}` record id ⇒ `unsupported`
  ("create: no record definition table for another module", `:756-758`). **The running module's own
  `put_record/6` create form IS implemented** (`create_native_record`, `:643-732`) — see Known Issues.
- `maps/mod.rs:260-271` `fail_or_function_clause` (absent key + `NO_FAIL` ⇒ the bare atom
  `function_clause`, through the shared `crate::interpreter::raise_function_clause_no_fail`);
  `:238-252` `fail_or_raise`/`:280-301` `raise_error_tuple` raise
  `{badmap,_}`/`{badkey,_}`; a missing tag atom ⇒ `unsupported`.
- `exceptions/mod.rs:155-157` `Raise` with no `pending_class`; `:167-171` `try_case` without `THE_NON_VALUE`;
  `:196-209` `catch_end` with x(1)/x(3) out of range or the missing `EXIT` atom;
  `exceptions/raise.rs:60-63,75-78` a missing tag atom ⇒ `unsupported`.
- `exceptions/stacktrace.rs:151-156` a missing `file`/`line` atom ⇒ `unsupported`.
- `floats/*` — a non-float FR slot or a non-finite result ⇒ the documented `badarith` failure (`arith.rs:72,98`), never a fabricated value.
- `bitstrings/create/mod.rs:369-371` unknown segment type; `:199-201` integer size `all`; `size.rs:173-185` `raise_seg_fail`.
- `bitstrings/mod.rs` — every invalid `Sz`/flag/offset ⇒ a typed result (`read_size` `:156`, `decode_flags` `:187`, `make_small_checked` `:292`).
- `bitstrings/matching/match_ctx.rs:56-71` unknown `bs_start_match4` fail atom; `commands.rs:173-175` unknown `bs_match` command.
- `bitstrings/raw.rs:264-265` result > 64 bytes in `make_bits`; `:222-226` zero-width bignum; `:317-320` short buffer.
- `receive.rs:266-280` `marker_call` — a host that does not serve the marker seam is rejected **by name**.

## Error-swallowing sites (a `Result`/failure is discarded)
- `exceptions/mod.rs:158` and `exceptions/raise.rs:65,80` — `let _ = install_class_atom(...)`: the failure is
  **deliberately deferred** ("lazy landing"), so a raise whose class atom the module lacks fails later, at
  landing time, with the typed error. Not a wrong value, but the immediate failure is lost.
- `bitstrings/matching/mod.rs:279` — `module.strings.get(*offset..).unwrap_or(&[])`: an out-of-range `StrT`
  offset becomes an **empty pattern** (which then merely fails the match) instead of a typed error.
- The remaining `Err(_) =>` discs are intentional and correct (type tests → the documented failure):
  `lists.rs:110,123` (non-cons ⇒ badmatch), `floats/arith.rs:72,98` (non-float ⇒ badarith),
  `bitstrings/mod.rs:381` (out-of-small-range ⇒ build a bignum).

## Duplication with sibling crates (audit finding)
- **`bitstrings/raw.rs` hand-rolls term layout a PUBLIC `beam_term` writer/reader already owns** (reachable,
  fixable — unlike the map walk below):
  - `raw.rs:64-67` re-declares `const ERL_ONHEAP_BINARY_LIMIT = 64` == `beam_term::refc::ERL_ONHEAP_BINARY_LIMIT` (`beam_term/src/refc.rs:103`).
  - `raw.rs:258-291` `make_bits` copies `beam_term::bitstring::write_heap_bits` (`beam_term/src/bitstring.rs:170-198`) — but **does not zero the trailing padding bytes of the last data word** that the LCA writer explicitly zeroes (`write_bytes(data,0,…)`).
  - `raw.rs:178-196` `make_ctx` copies `beam_term::bitstring::write_sub_bits` (`bitstring.rs:234-251`, `base_flags = 0`).
  - `raw.rs:154-170` `ctx_parts` copies `beam_term::bitstring::sub_bits_parts` (`bitstring.rs:267-279`); `:202-208` `set_ctx_start` writes word 2 directly (no LCA setter exists).
  - `raw.rs:88-128` `heap_bits` derives the data pointer itself instead of `beam_term::bitstring::bitstring_parts` (`bitstring.rs:299`; used only for the refc case at `:107`).
  - **Forwarded (no duplication):** the bignum path (`raw.rs:217-250` uses `beam_term::{big_size_words, write_big, big_parts}`) and the refc path (`raw.rs:307-375` uses `beam_term::refc::{ERL_REFC_BITS_SIZE, write_refc_binary, is_refc_binary}`).
- **`../terms/map.rs:155-242`** (`hashmap_entries`/`collect_hashmap_node`/`hashmap_node_shape`) re-implements
  HAMT node walking + node-shape decode that `beam_term::map::{map_pairs, map_pairs_storage_order, map_get}`
  (`beam_term/src/map.rs:458,471,518`) already do. It is what `maps/{read,write}.rs` uses for enumeration.
  (The engine variant additionally rejects `THE_NON_VALUE`, `../terms/map.rs:191`.) Out of this node's scope
  but on the `maps` family's path — the natural LCA home is `beam_term::map`.
- **Workspace-wide posture, not families-specific:** the same hand-rolled HAMT/refc layout re-derivation
  exists in `beam_message/src/layout.rs` + `copy.rs`, `beam_bifs/src/access.rs`, and `beam_gc/src/copy.rs`,
  because `beam_term::size::describe_object` and `beam_gc::copy::{move_size,scan_step}` are **private**.
  So the families duplication is the house pattern, not an isolated defect. Additionally `beam_bifs/src/phash.rs`
  duplicates `beam_term::hashing::{make_hash, make_hash2}` even though those are public.
- **Families that forward correctly (no duplication):** `records/raw.rs` (all layout via `beam_term::record`),
  `funs/` (`crate::terms::make_fun`), `floats/` (`beam_term::number::big_to_f64`, `try_make_small`),
  `exceptions/`, `tuples.rs`, `lists.rs`, `receive.rs`.

## Other semantics / divergence notes
- **The receive/message host contract (`receive.rs` + the `Mailbox` seam).** The engine **never inspects a message or a marker** — selection is the compiled matching instructions between `loop_rec` and `loop_rec_end`, and a marker term is opaque. `receive_skip`/`receive_remove`/`receive_reset` take **no candidate argument**: the host acts on its **own** persistent scan position (`receive_remove` unlinks whatever is at that position, NOT the term the engine read into `Dst`), so the engine's driving is correct only while that position is stable between the `loop_rec` that produced a candidate and the `remove_message`/`loop_rec_end` that consumes it — which holds by construction (the engine issues no other seam call and never blocks in between). A `receive` reaches `wait`/`wait_timeout` **only** via `loop_rec`'s `None` arm (the host scan is already exhausted), so a `resume_outcome` re-entry sees only messages appended since. Host contract: FIFO with a **saturating** scan index (`beam_message::QueueCursor` + `beam_rt::MailboxState`); pinned by `tests/receive.rs` (`scan_skips_a_non_matching_head_before_matching`, `a_suspended_receive_resumes_when_a_message_arrives`, `the_mailbox_repositions_the_scan_at_a_marker`).
- `receive.rs:156-163` `WaitTimeout`: a non-zero timeout suspends **with no timer** (the engine arms none); only the small integer `0` falls through to `pc+1`. A host recovers the parked deadline through `Machine::wait_deadline` and re-enters at the `timeout` continuation (`../machine/CONTEXT.md`, pinned by `../machine/deadline_tests.rs`).
- `receive.rs:266-280` `marker_call` classifies "host does not serve markers" by **exact string match** on `unsupported_opcode: {method}` — a fragile, string-based error taxonomy (documented as the discriminator).
- `bitstrings/create/segments.rs:96-101` `float_source`: a **bignum** float-segment value yields `None` ⇒ `badarg`, even though `beam_term::number::big_to_f64` exists and is used by `floats/convert.rs`. Missed reuse.
- `bitstrings/matching/reads.rs:288-326` `decode_utf8` ignores `BSF_LITTLE` (documented: `bs_get_utf8 -little` is not meaningful in ERTS).
- `records/mod.rs:828-858` `write_local_record` writes the same block **twice** at `hp` (placeholder hash `0`, then the real `compute_record_definition_hash`) — intentional (the hash reads the block) but a double full write.
- `maps/mod.rs:185-198` `lookup`/`position` are O(n) `beam_term::cmp` scans (no hash); `maps/write.rs:75` makes `put_map_assoc` O(n²) in the pair count.
- `bitstrings/raw.rs:137-150` `subject_bytes` returns `&'a [u8]` with an unconstrained lifetime ("caller keeps it alive") — a safe-signature soundness caveat, documented as the `ExecutionContext` ownership contract.
- Dead/stub remnants (harmless): `bitstrings/mod.rs:84-85` `#[allow(dead_code)] const BSF_ALIGNED`;
  `bitstrings/matching/mod.rs:75-84` `#[allow(unused_imports)]` re-exports kept for path compatibility;
  `bitstrings/create/mod.rs:155-158` an empty `if *fail != NO_FAIL { /* comment only */ }` branch.

## Constraints
- `#![deny(unsafe_code)]` crate-wide; the node's **only** `#[allow(unsafe_code)]` module is `bitstrings/raw.rs`
  (every `unsafe` block carries a `// SAFETY:` rationale).
- Errors are `beam_common::{Result, BeamError}` only — no crate-local error enum, no panic, no `unwrap()` on
  loaded data, no wrong result.
- Term construction/inspection is **only** `beam_term`; ordering uses `beam_term::{cmp, eq}`, never a reimplementation.
- Files ~1000 lines, split by responsibility; licensing Apache-2.0 (standard OTP header on every `*.rs`).

## Test Strategy
- Per-family `#[cfg(test)]` unit tests over hand-built `Instr`s with a recording `ExecutionContext` stub:
  `mod.rs:250-485`, `receive.rs:282-905`, `records/tests/*`, `exceptions/tests/*`, `bitstrings/*/tests.rs`,
  `bitstrings/matching/mod.rs:408-566`, `bitstrings/raw.rs:377-439`.
- The router's diagnostics contract is pinned by `mod.rs:315+` (`the_prefix_is_unchanged`); the receive
  family's tests assert an owned variant never reaches the sentinel again.
- End-to-end coverage lives in the crate integration suites (`../TEST_NOTES.md`).
- **No production-path `unwrap()`/`panic!`/`unreachable!` in any family** — every grep hit for those is inside
  a `#[cfg(test)]` module. This is a genuinely rigorous result.

## Known Issues
- **Stale docs (parent, not this node):** `../CONTEXT.md` and `../../../crates/CONTEXT.md` claim
  `put_record/6`'s **create** form is a typed `unsupported`. It is **implemented** for the running module's
  own records (`records/mod.rs:643-732`); only the **foreign-module** id is a typed `unsupported` (`:756-758`).
  Correct the parent docs there, not here.
- `bitstrings/raw.rs:258-291` `make_bits` omits the `write_heap_bits` padding-zeroing, so under an
  uninitialized allocating context (`MinimalContext`) the trailing bytes of a heap binary's last data word
  hold garbage. Not observable through the size/length-driven readers, but a divergence from the LCA invariant.

## Notes for Agents
- The router (`mod.rs:111`) and `lists::exec` are `#[inline]`, so a dispatched instruction hands straight to its owning family.
- A family's `Ok(None)` is **not** a silent no-op: it reaches the router's last arm and yields the sentinel.
  The only way to "claim then no-op" would be an arm returning `Ok(Some(Step::Next))` without doing the
  operation — none exists (verified across all families).
- Ground truth (read-only, never modify): `../../../../erts/emulator/beam/emu/*.tab` and `erl_map.c`/`erl_bits.c`/`erl_record.c`.

## See Also
- `../CONTEXT.md` → the crate (in particular the execution model + `Handoffs`), `README`-level detail in `../INSTRUCTION_SURFACE.md`.
- `../../terms/` → the term builders/readers the families call (`terms::map::map_entries` for map enumeration).
- `../beam_term/src/{bitstring.rs,refc.rs,map.rs,record.rs}` → the LCA term layout these families must forward to.
