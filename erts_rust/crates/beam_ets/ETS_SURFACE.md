# ETS surface census — where every `ets:*` operation lives
Static census of the **real** ETS surface OTP 29 / erts-17.1 exposes, against this workspace.
Purpose: a future agent must not re-derive "is `ets:X` implemented, and where?".
Gate source of truth for *behaviour* is the differential suite (`tests/differential.rs`, **191 cases**,
live oracle, `mismatch = 0`, `KNOWN_DIVERGENCES` **empty** — every case agrees); this file is the
*coverage* map.
Three owners exist:
* **`beam_ets`** (this crate) — the table state + the observable `ets:*` semantics.
* **`beam_bifs`** — the BIF layer: argument parsing, the `TableRegistry` (id → table, name → id,
  continuation handles, compiled-spec handles), the access gate, and the operations that need runtime
  facilities.
* **missing** — nothing implements it; the reason is named.
`beam_bifs`'s registry keys live in `beam_bifs/src/ets.rs`,
`ets/{matches,specs,extra,adopt,reverse,nav}.rs` and the seam-backed remainder in `runtime/other.rs`.
## 1. Table lifecycle and metadata
| Operation | Status | Where |
|---|---|---|
| `new/2` — kinds `set`/`ordered_set`/`bag`/`duplicate_bag`, `{keypos,N}`, `named_table`, `private`/`protected`/`public`, `compressed`, `{write_concurrency,_}`, `{read_concurrency,_}`, `{heir, Pid}` / `{heir, Pid, Data}` / `{heir, none}`, `{decentralized_counters, Bool}` | state modelled here | `beam_ets::TableOptions` (all of the above, incl. a **table-owned** copy of a creation-time heir's gift data at `Table::build`) + `Table::new` |
| `new/2` — option **parsing** + the name registry + the named-table-conflict `badarg` | implemented in `beam_bifs` | `beam_bifs::ets::new_2` → `resolve::parse_options` accepts every option `TableOptions` models: the bare atoms `set`/`ordered_set`/`bag`/`duplicate_bag`, `public`/`protected`/`private`, `named_table`, `compressed`; the tuples `{keypos, N>0}`, `{write_concurrency, true\|false\|auto}`, `{write_concurrency, {debug_hash_fixed_number_of_locks, N}}` (`N` in `1..=32768`), `{read_concurrency, Bool}`, `{heir, none\|Pid}` / `{heir, Pid, Data}`, `{decentralized_counters, Bool}`. Last option wins (the bare `set`/`protected` atoms are no-ops that never override an earlier choice); an unknown atom/tag/arity and an `ordered_set` with an explicit lock count are `badarg` (see §7.5) |
| `delete/1` (the table) | implemented | `beam_bifs::ets::delete_1` → `TableRegistry::remove` + `remove_name`, **write**-gated |
| `delete/2` (objects) | implemented | `beam_ets::Table::delete` |
| `delete_all_objects/1` | implemented | `beam_ets::Table::delete_all_objects` |
| `delete_object/2` | implemented | `beam_ets::Table::delete_object` (removes **every** equal object — pinned for the multi kinds) |
| `info/1`, `info/2` | implemented | `beam_bifs::ets::{info_1, info_2}` (ungated) over `beam_ets::Table::info` + `TableInfo::entries` (15 fields in oracle order). `owner` and a set `heir` render as the recorded pid (`heir` renders `none` when cleared), `id` as the tid reference, and `decentralized_counters` is read from the resolved `TableInfo` field |
| `all/0` | implemented | `beam_bifs::ets::adopt::all_0` over `TableRegistry::all_tids` (ascending handle) — the name atom for a `named_table`, else its handle reference |
| `whereis/1` | implemented | `beam_bifs::ets::extra::whereis_1` over the name registry |
| `rename/2` | implemented | state in `beam_ets::Table::rename(name, is_taken) -> Result<RenameOutcome>` (oracle-pinned: `named_table` rekeys and returns the new name atom, conflicts/self-rename `badarg`; a table without `named_table` renames silently with no conflict check and returns its id); the BIF `beam_bifs::ets::extra::rename_2` rekeys the registry and is **write**-gated |
| `setopts/2` — `{heir, Pid}` / `{heir, Pid, Data}` / `{heir, none}` | implemented | state in `beam_ets`: `SetOpt::{Heir, ClearHeir}` (`src/table/ownership.rs`) with the owner-gated `Table::setopts(caller, SetOpt) -> Result<()>` (a non-owner attempt is `badarg` before any mutation), the owner-level `Table::set_heir` primitive and the copying heir install `Table::install_heir(owner, Option<Eterm>)` + the read-back `Table::heir_data()`. BIF: `beam_bifs::ets::adopt::setopts_2` (write-gated **plus** an explicit owner check) |
| `setopts/2` — `{protection, Access}` | crate state here; **typed gap** in the BIF | state in `beam_ets`: `SetOpt::Access(Access)`, the owner-gated `Table::setopts(caller, SetOpt::Access(_))`, the owner-level `Table::set_access(&mut self, access: Access)` primitive and the enforcing `TableAccess::set_access(&mut self, access: Access)`; `ets:info(Tab, protection)` reflects the change. The BIF is **not** wired yet: `beam_bifs::ets::adopt::setopts_2` recognises the option (so the parse-level `{protection, bogus}` `badarg` is still its own, `parse_setopts`) but answers the typed `unsupported_bif` for a **valid** `{protection, _}`, leaving the protection untouched — the adoption (map the parsed access onto `SetOpt::Access` and call `table.as_requester(Some(caller)).setopts(opt)`) is a `beam_bifs` task (see `CONTEXT.md` Handoffs) |
| `give_away/3` | crate state here; **delivery gap** in the BIF | state in `beam_ets`: `Table::give_away(caller, new_owner)` (the C-ordered check shared with the gift form via a private `transfer_target`: a non-owner, a dead-pid (`None`) target and a self-gift are all `badarg`) and the gift-carrying `Table::give_away_with_gift(caller, new_owner, gift) -> Result<Transfer>`, which deep-copies `gift` through the store's copy boundary and checks it out (`TermStore::relinquish`, like `take`) into a `Transfer { to, from, data }`. The BIF `beam_bifs::ets::adopt::give_away_3` is **write**-gated + owner-checked but still reads only `args[0]`/`args[1]`: wiring `args[2]` → `give_away_with_gift` and delivering `{'ETS-TRANSFER', Tid, FromPid, GiftData}` to the new owner (via its `SendHook`) is the `beam_bifs` task (see `CONTEXT.md` Handoffs) |
| owner-death / heir hand-off | API here; runtime caller in `beam_rt` | `beam_ets::Table::owner_died(dead, is_alive) -> OwnerDeath::{NotOwner,Deleted,Transferred(Heir)}`; the transfer message is sent **only when `heir.data` is `Some`** (see §6). `beam_rt::runtime::ets_owner::drain_ets_owner_exits` drives it (see §7.4); the roster that decides which tables a dead owner owned is `TableRegistry::handles_owned_by`, so an **unnamed** table is reclaimed too (the `ets:new/2` index only supplies the names to release) |
| `safe_fixtable/2` | implemented | `beam_ets::Table::set_fixation` + the **read**-gated `TableAccess::set_fixation`; `beam_bifs::ets::extra::safe_fixtable_2` takes the table through `take_table_gated(.., Gate::Read)` |
## 2. Reads and navigation
| Operation | Status | Where |
|---|---|---|
| `lookup/2` | implemented | `beam_ets::Table::lookup` |
| `lookup_element/3`, `lookup_element/4` | implemented | `beam_ets::Table::lookup_element` / `lookup_element_or` for `set`/`ordered_set`, and the list forms `lookup_element_all` / `lookup_element_all_or` for `bag`/`duplicate_bag` (the crate's scalar form raises the typed `unsupported` for a multi kind). `beam_bifs::ets::{lookup_element_3, lookup_element_4}` branch on `kind().is_multi()` to the right form |
| `first/1`, `next/2`, `last/1`, `prev/2` | implemented | `beam_ets::Table` (`first`/`next`/`last`/`prev`, `Option::None` ⇒ `'$end_of_table'`). For the **hash kinds** (`set`/`bag`/`duplicate_bag`) `last` **is** `first` and `prev` **is** `next` (`erl_db_hash.c:853-856`) — no backward walk; only `ordered_set` has the real arithmetic order. An absent anchor is `badarg` for a hash kind (an `ordered_set` reads it as a range bound) |
| `member/2` | implemented | `beam_ets::Table::member` |
| `tab2list/1` | implemented | `beam_ets::Table::tab2list` |
| `slot/2` | implemented here; **BIF adoption pending** | `beam_ets::Table::slot(slot: i64, atoms) -> Result<Option<Vec<Eterm>>>` (0-based; `Ok(None)` ⇒ `'$end_of_table'`; an out-of-range **or negative** slot ⇒ `badarg`) and the **read**-gated `TableAccess::slot` (the oracle's `erl_db.c:3744` `ets_slot_2` takes a `DB_READ` gate). An `ordered_set` slot is **exact** — the object with the (n+1)-th smallest key (`erl_db_tree.c:1139` `db_slot_tree_common`), `size` = EOT, `> size` = `badarg`; a hash-kind slot is a hash **bucket** (`erl_db_hash.c:1733` `db_slot_hash`), so only the contract is pinned (slots `0..nactive-1` partition every object, `nactive` = EOT, `> nactive` = `badarg`). `nactive` is this interior's own bucket count, and it shrinks back toward `INITIAL_BUCKETS` as objects leave (see §8), so a `slot` boundary is a snapshot of the current count, never a high-water mark. `beam_bifs::runtime::other` still answers the typed `unsupported_bif` (§7) |
| `first_lookup/1`, `last_lookup/1`, `next_lookup/2`, `prev_lookup/2` | implemented | `beam_bifs::ets::nav` (**read**-gated) over the table's `first`/`last`/`next`/`prev` |
| `foldl/3`, `foldr/3` | implemented | `beam_bifs::ets::adopt::{foldl_3, foldr_3}` (**read**-gated) — the per-object fun is applied through `BifContext::call_fun` (the `FunCaller` seam), so a non-empty fold with no `FunCaller` hook is the typed `unsupported_bif`; an empty table returns `Acc0` without ever calling the fun |
| `take/2` | implemented | `beam_ets::Table::take` (checked-out copies; the stored originals are reclaimed) |
| `is_compiled_ms/1` | implemented | `beam_bifs::ets::extra::is_compiled_ms_1` over the registry's spec table |
## 3. Writes
| Operation | Status | Where |
|---|---|---|
| `insert/2` | implemented | `beam_ets::Table::insert` / `insert_list` (**atomic**: the whole list is validated and deep-copied first, then inserted); `beam_bifs::ets` splits a list argument |
| `insert_new/2` | implemented | `beam_ets::Table::insert_new`; the oracle rule — `false` whenever the **key** already has an object, for **all four kinds** (including `duplicate_bag`) — is implemented per interior and pinned by the `kind`/`multikind` groups |
| `update_counter/3`, `update_counter/4` | implemented | `beam_ets::Table::{update_counter, update_counter_with}` (`badarg` on the multi kinds, matching the oracle) |
| `update_element/3` | implemented | `beam_ets::Table::update_element`; `beam_bifs::ets` parses the update list |
| `update_element/4` (create form) | implemented | `beam_bifs::ets::extra::update_element_4` / `update_element_with_default` (builds the default object, then `Table::insert`/`update_element`) |
## 4. Match / select family
| Operation | Status | Where |
|---|---|---|
| `select/2`, `select/3`, `select/1` | implemented | `beam_ets::Table::{select, select_limit, select_continue}` |
| `select_count/2` | implemented | `beam_ets::Table::select_count` |
| `select_delete/2` | implemented | `beam_ets::Table::select_delete` |
| `select_replace/2` | implemented | `beam_ets::Table::select_replace` (the C's **static** body check `db_match_keeps_key`: a body outside `'$_'`/`{{…}}`/`{const,{…}}`/`{element,KeyPos,'$_'}` ⇒ whole-call `badarg` even with no match; then a two-phase rewrite — `bag` ⇒ `badarg`, `duplicate_bag` rewrites each matched copy) |
| `select_reverse/1,2,3` | implemented | `beam_bifs::ets::reverse` (**read**-gated), thin wrappers over `beam_ets::Table::{select_reverse, select_reverse_limit, select_reverse_continue}`; `/1` shares the direction-aware `dispatch_continuation` with `ets:select/1`, so a resumed continuation keeps its original direction. For the **hash kinds** the reverse forms return **exactly the same list in the same order** as `ets:select/1,2,3` (`erl_db_hash.c:2310-2313` — `db_select_reverse_hash` delegates to the forward chunk function); only `ordered_set` descends |
| `match/2`, `match/3`, `match/1` | implemented | `beam_ets::Table::{match_, match_limit, match_continue}` |
| `match_object/2`, `/3`, `/1` | implemented | `beam_ets::Table::{match_object, match_object_limit, match_object_continue}` |
| `match_delete/2` | implemented | `beam_ets::Table::match_delete` |
| `match_spec_compile/1` | implemented | `beam_bifs::ets::specs` (registry of raw specs) + `beam_ets::compile` for validation |
| `match_spec_run/2` | implemented | `beam_ets::match_spec_run` driven by the registry (`beam_bifs::ets::specs`) |
| `match_spec_run_r/3` | implemented | `beam_bifs::ets::specs::match_spec_run_r_3` |
| `match_spec_test/3` (`erlang:`) | `table` dialect implemented; `trace` dialect **left-open** | `beam_bifs::match_spec` (native structural lint + the `table` dialect → `beam_ets`, head maps included); the `trace` dialect is left-open, answered by the typed `unsupported_bif` (it folds the trace **run** into `{ok,Return,Flags,Errors}` and needs the trace-only forms — `get_tcw`/`set_tcw`/`message`/`return_trace`/… — i.e. a runner subsystem; see `CONTEXT.md` Known Issues and `src/select/CONTEXT.md`). `match_spec_test/2` is not an OTP 29 export |
| `fun2ms/1` | missing | an `ms_transform` stdlib parse transform, not a BIF; `beam_ets` has the compiled-spec *shape* only (`select` tests are named `fun2ms_*` because they reproduce `fun2ms/1`'s output shape) |
| `internal_select_delete/2`, `internal_delete_all/2` | implemented | `beam_bifs::ets::extra::{internal_select_delete_2, internal_delete_all_2}` (both **write**-gated) |
| `internal_request_all/0` | missing | seam-backed `unsupported_bif` (`beam_bifs::runtime::other` — the `ets` scheduler fan-in) |
| `repair_continuation/2` | missing | stdlib `ets.erl` wrapper |
## 5. Table-file / misc
| Operation | Status | Where |
|---|---|---|
| `tab2file/2,3`, `file2tab/1,2`, `tabfile_info/1` | missing (blocked) | The table-file format is `{Header,Objects,Footer}` where **every** field is an ETF term, so it needs a `beam_codec` edge, plus file I/O (`beam_io`/`std::fs`). Also needs `{extended_info,_}`, `sync`/`sync_verbose`, ~10 000-object chunking and re-raise semantics. **Owner:** a wave-5+ host (e.g. `beam_rt`) that can hold both edges, or a later lift of this crate's wave — neither the `beam_codec` nor the `beam_io` edge is available from wave 1 (the `beam_io` one points *upward*). `tab2list` + `first`/`next` remain the enumeration path |
| `table/1,2` | missing | stdlib `ets.erl` wrappers (exported by `ets`, but not `bif.tab` entries, so no BIF layer implements them) |
| `from_dets/2`, `to_dets/2`, `init_table/2`, `test_ms/2`, `i/0..3` | missing | the remaining exported `ets.erl` helpers: DETS interop (`from_dets`/`to_dets`), the legacy table loader (`init_table/2`), a thin wrapper over the already-implemented `erlang:match_spec_test/3` (`test_ms/2`) and the human-readable `ets:i()` table dump. None is a `bif.tab` entry, so the BIF layer never sees them |
| `clear/1`, `size/1` | not an OTP export | correct absence (verified against `ets:module_info(exports)` on erts-17.1; `ets:info(T, size)` is the `size` spelling) |
## 6. Oracle-probed semantic facts pinned by the differential suite
These are the reference answers, each compared against the live `erl` (`tests/differential`, 191 cases).
* **Access matrix.** `requester == owner` ⇒ read + write always. Otherwise: `public` ⇒ read + write;
  `protected` ⇒ read only; `private` ⇒ neither. Every denial is `badarg`.
  * The matrix is pinned **per operation**:
    `tests/ets_access.rs::every_read_and_write_op_of_the_access_matrix_matches_the_live_oracle` drives
    **every** read, every write, every `ets:info` field, every ownership op and the four continuation
    resumes under owner + non-owner × `public`/`protected`/`private`, asserting each cell's crate
    outcome equals the live oracle's **and** the oracle's `{SizeBefore,SizeAfter,is_list(ets:info(T))}`
    intact triple (so a denial is `badarg` *and* left the table untouched).
  * `ets:info/1` and `ets:info/2` are **never** access-gated (a non-owner may `info` a `private` table).
  * `ets:safe_fixtable/2` and `ets:take/2` are gated as **read** / **write** (a non-owner may fix a
    `protected` table); `ets:rename/2` is **write**-gated.
  * `ets:select_reverse/1,2,3` is gated as **read**, exactly like `ets:select/1,2,3`: a non-owner may
    run all three forms on a `protected`/`public` table; a `private` table denies with `badarg`. A
    failed match-spec **body** yields the atom `EXIT` resolved from the caller's `AtomTable`
    (`select::exec::exit_term`), so a host whose spec body can raise must pre-intern `EXIT` (the C
    `FAIL_TERM` is a static atom that always exists).
  * `ets:setopts/2` is **owner**-gated (the owner check precedes any mutation); a `{protection, Access}`
    option changes `ets:info(Tab, protection)` — at the **crate** level (`Table::setopts(SetOpt::Access)`
    / `TableAccess::set_access`), oracle-probed by `tests/ets_set_access.rs`; the BIF still answers the
    typed `unsupported_bif` for a valid `{protection, _}` (see §1).
* **Rename.** On a `named_table`: a free new name rekeys the registry, the old name stops resolving,
  the new name resolves to the **same tid**, the data/tid/owner/protection survive, and the BIF returns
  the **new name atom**; an already-taken name — **including its own current name** — is `badarg` with
  nothing changed. On a table **without** `named_table`: the rename always succeeds (no conflict check
  at all, not even against a registered name), overwrites the recorded name, leaves `named_table` the
  same (the name is not registered) and returns the **unchanged table identifier**. A non-atom name and
  an unknown/deleted table are `badarg`.
* **Owner death.** With a live heir, the heir receives `{'ETS-TRANSFER', Tid, FromPid, Data}` and
  becomes the owner while `ets:info(T,heir)` keeps reporting the heir (it is **not** reset); `Tid` is
  the table **id** for an unnamed table but the **name atom** for a `named_table`. With no heir — or an
  already-dead heir (`ets:setopts(T,{heir,DeadPid})` is accepted, normalised to `none` at parse time) —
  the table is deleted. `give_away/3` to a dead pid or to itself is `badarg`. `give_away(T,Pid,Gift)`
  sends the recipient `{'ETS-TRANSFER', Tid, FromPid, Gift}` (a 4-tuple; `Tid` the name atom for a
  `named_table`, else a ref) and moves the gift into the recipient's **message**, so the table's
  `ets:info(T, memory)` does **not** grow (`undefined` is delivered verbatim).
* **The heir's two forms.** `{heir, Pid}` (arity 2) inherits the table **silently — no
  `{'ETS-TRANSFER',…}` message at all** (the C `THE_NON_VALUE` sentinel), whereas
  `{heir, Pid, undefined}` (arity 3) delivers the message carrying `undefined`. The gift data is
  **deep-copied into table-owned storage** at creation and at `setopts`/`install_heir` (the C
  `ERTS_ALC_T_DB_HEIR_DATA` copy), so it outlives the installing process's heap.
* **`decentralized_counters`** = the **effective** `write_concurrency` **and**
  (`{decentralized_counters, Bool}` when given explicitly, else the table kind is `ordered_set`).
  The explicit option is accepted for every kind (`{decentralized_counters,true}` +
  `{write_concurrency,true}` on a `set`/`bag`/`duplicate_bag` reports `true`; explicit `false` on an
  `ordered_set`+`write_concurrency` reports `false`); a non-boolean value is `badarg`. The model is
  `TableOptions::write_concurrency_mode()`, a `WriteConcurrency` (`Off`/`On`/`Auto`/`FixedLocks(n)`)
  whose `Off`/`On` projection is the boolean `write_concurrency()`; a `private` table normalises the
  fine-locked flag off **at creation only** (the flags are read-only thereafter, so a runtime
  `ets:setopts(T,{protection,private})` does not re-normalise them), and the resolved
  `TableInfo::decentralized_counters` is the field the `ets:info/2` bridge reads.
* **`ordered_set` keys are arithmetic.** `1` and `1.0` are the same key, the *last-inserted key term*
  is the one iterated (`ets:first`/`last`/`next`/`prev`), and iteration is in numeric order.
* **`insert_new/2` refuses on an existing key** for every kind, including `duplicate_bag`.
* **`insert/2` is atomic**: a list containing an invalid object is `badarg` and inserts nothing.
* **Multi kinds.** A `bag` keeps distinct objects (an exact duplicate is a no-op) while a
  `duplicate_bag` keeps every insert; `delete_object/2` removes **every** equal object;
  `size`/`info(T,size)` count **objects**; `lookup_element/3,4` return the element **list** (`badarg`
  for an absent key or a too-short object; `/4` yields `default` for an absent key); `update_counter`
  and `update_element` are `badarg`; `select_replace/2` works on `duplicate_bag` but is `badarg` on
  `bag`; hash-kind navigation raises `badarg` for an absent anchor (an `ordered_set` reads it as a
  range bound).
* **Hash-kind navigation is aliased to the forward direction.** For `set`/`bag`/`duplicate_bag` the
  oracle implements `ets:last/1` as `ets:first/1` and `ets:prev/2` as `ets:next/2`
  (`erl_db_hash.c:853-856`, repeated at `:891-894`), so there is no backward walk. Probed on the
  system `erl` (OTP 29.1.1 / erts-17.1): a set holding keys `1..5` gives `first` == `last` == `1`,
  `next(T,3)` == `prev(T,3)` == `4` and `prev(T,1)` == `next(T,1)` == `5`; a 6-key set gives
  `first` == `last` == `6` with `prev(K)` == `next(K)` for every stored `K`; a single-key set gives
  `{only, only, '$end_of_table', '$end_of_table'}`; an empty hash-kind table gives `first` ==
  `last` == `'$end_of_table'` and `prev(T,7)` is `badarg`. An unknown anchor stays `badarg` for
  every hash kind (`Table::check_navigable`); `ordered_set` keeps the true arithmetic order
  (`first`/`last`, `next_from`/`prev_from`). Pinned by
  `multikind/hash_last_prev_alias_first_next`, and the reverse-direction consequence by
  `multikind/hash_select_reverse_aliases_select` (for the three hash kinds `ets:select_reverse`
  returns exactly the same list in the same order as `ets:select` — chunked and unchunked, each
  key's stored object order preserved for the multi kinds).
* **Select-family continuations are key-anchored on an `ordered_set`, ordinal on the hash kinds.**
  `MatchContinuation` (`src/select/continuation.rs`) carries a private `Cursor`: the ordered kind
  resumes through `Table::next`/`prev` from the **last key its chunk yielded**, used as an
  **exclusive** range bound, so a resume survives the anchor's own deletion; the hash kinds keep an
  ordinal into the table's iteration order (their ERTS cursor is a bucket/index, not a key —
  `erl_db_hash.c:2310-2313`, and the hash iteration order is an interior freedom). Probed on the
  system `erl` (OTP 29.1.1 / erts-17.1): on an `ordered_set` `{1}..{10}` chunked at 4, chunk 1 is
  `[1,2,3,4]`; deleting `2` + inserting `0` before resuming still yields `[5,6,7,8]` then `[9,10]`,
  and deleting the **anchor** `4` instead yields the same `[5,6,7,8]`; reverse `1..10` chunked at 3
  yields `[10,9,8]`, deleting `10` then `[7,6,5]`; a fully consumed continuation yields
  `'$end_of_table'`. Continuation identity is unchanged (the table id plus — for a `named_table` —
  the name atom, so an `ets:rename/2` invalidates an issued continuation with `badarg`; the read
  gate is checked before the continuation is consumed). A hash-kind mutation landing at or behind
  the ordinal is the single divergence, and only across a resume straddling a mutation — never a
  typed error, never a wrong result for a mutation-free walk.
* **`select_replace/2` enforces the C's static body check** (`db_match_keeps_key`): a replacement clause
  whose body is not syntactically `'$_'`, a tuple construction `{{…}}`, `{const,{…}}` or
  `{element,KeyPos,'$_'}` (with `KeyPos` the *table's* keypos) makes the whole call `badarg` even when
  nothing matches, and the table is unchanged; `bag` is `badarg` regardless. The check runs before the
  traversal, so a never-matching head still rejects.
* **The copy boundary is complete.** `ets:insert/2` accepts every kind the oracle accepts — flat and
  hash maps, funs (local/external/imported), external pid/port/reference and bitstrings/refc binaries
  included — deep-copied into table-owned storage; only a bare `BinRef`, a native record, the reserved
  subtags and a malformed `ErlSubBits` are the typed `badarg` (never a wrong result).
* **Read results and the copy boundary.** A read returns a term **aliasing table-owned storage**; a
  term read through the multi-threaded handle ([`SharedTable`]) is a guard-scoped `TableTerm` that
  cannot outlive its guard (a compile error), and callers that need to keep a term copy it out
  (`TableTerm::copy_into`, `TableRead::select`). The raw `Table`/`TableAccess` read family still
  returns bare `Eterm`s (see §7.6).
## 7. Runtime-level gaps (NOT in this node — the exact API each needs)
1. **BIF-path access gating is enforced.** Every table-taking `ets:*` BIF decodes its table through
   `take_table_gated(ctx, handle, resolve::Gate::{Read, Write})` (`beam_bifs/src/ets.rs`), and
   `Gate::check` calls `beam_ets::Table::check_read` / `check_write` for the calling process
   (`resolve::caller_owner`). The check runs **before** the registry take, so a denial is `badarg`
   **and leaves the table in the registry** (it is never destroyed by a denied call). The gate per
   verb: **write** — `delete/1`, `delete_all_objects/1`, `insert/2`, `insert_new/2`, `delete/2`,
   `delete_object/2`, `take/2`, `update_counter/3,4`, `update_element/3,4`, `rename/2`,
   `select_delete/2`, `select_replace/2`, `match_delete/2`, `internal_delete_all/2`,
   `internal_select_delete/2`, `setopts/2`, `give_away/3`; **read** — `tab2list/1`, `lookup/2`,
   `lookup_element/3,4`, `member/2`, `first/1`, `last/1`, `next/2`, `prev/2`, `select/1,2,3`,
   `select_reverse/1,2,3`, `match/1,2,3`, `match_object/1,2,3`, `select_count/2`, `safe_fixtable/2`,
   `foldl/3`, `foldr/3`, the `*_lookup` family, and a continuation resume (`resume_continuation`
   checks a read against the continuation's own table **before** consuming it, so a denied resume
   stays resumable); **ungated** — `info/1,2` alone (`take_table` = `Gate::Ungated`). `setopts/2` and
   `give_away/3` additionally require the caller to be the table's **owner**. The BIFs call the
   owner-level `Table::*` primitives behind the gate; `setopts`/`give_away` go through
   `Table::as_requester(caller)` (`TableAccess`), while `rename/2` calls `Table::rename` behind its
   write gate rather than `TableAccess::rename`.
2. **The owner is recorded.** `beam_bifs::ets::new_2` calls
   `Table::set_owner(Some(resolve::caller_owner(ctx)))`, so a table's `OwnerId` is its creating pid
   and `ets:info(T, owner)` / `T, heir` render the recorded pid (`heir` renders `none` when cleared).
   `TableRegistry::owner_of(ProcessId) -> OwnerId` is the pid→owner bridge
   (`OwnerId::new(pid.to_eterm().raw() as u64)`), and `TableRegistry::handles_owned_by(owner) ->
   Vec<u64>` is the public owner→handle roster — **unnamed tables included**. A host sweeping a dead
   owner's tables uses `handles_owned_by(TableRegistry::owner_of(dead_pid))`.
3. **A process-death hook exists.** `beam_process` exposes `ProcessExitObserver` + `ProcessExit`
   (`src/exit_observer.rs`), installed on the `ProcessTable` with `set_process_exit_observer`
   (removed with `take_process_exit_observer`) and invoked **exactly once per death** from the crate's
   sole finalizer (`terminate_into`, before the dying heap is released). It mirrors the
   `DistSignalRouter` seam, so `beam_process` gains no dependency on the observing subsystem; with no
   observer installed the table behaves as before. `beam_rt` installs its own (see §7.4).
4. **`beam_rt` drives the owner-death entry point.** `beam_rt::runtime::ets_owner` installs an
   `EtsExitObserver`, and `Runtime::drain_ets_owner_exits(table, scheduler)` sweeps the dead owners it
   has recorded: per owned handle it calls `Table::owner_died(dead, is_alive)` (liveness probed
   through `ProcessTable::contains`), then acts on
   `OwnerDeath::{NotOwner, Deleted, Transferred(heir)}` — `Deleted` drops the table and releases its
   name; `Transferred` with `heir.data == Some` delivers `{'ETS-TRANSFER', Tid, FromPid, Data}` (the
   registered **name atom** as `Tid` for a `named_table`, the tid reference for an unnamed one) — and
   finally `scheduler.resume_woken(table)`. The **authority** on *which* tables a dead pid owned is the
   `beam_bifs::TableRegistry::handles_owned_by` roster, which covers an **unnamed** table too; the
   `OwnerIndex` recorded at `ets:new/2` is unioned in (deduplicated, ascending) and contributes only the
   **registered name** a `named_table`'s drop must release. Evidence:
   `beam_rt/tests/ets_owner.rs::unnamed_owner_death_flows_match_the_live_oracle`.
5. **The `beam_bifs` `ets:new/2` option parser matches this crate's model.** `resolve::parse_options`
   accepts the full option set (see §1) and hands the parsed mode/flag to `TableOptions`
   (`set_write_concurrency_mode`, `set_decentralized_counters`), so the model resolves them; it
   rejects an unknown atom/tag/arity and an `ordered_set` with an explicit lock count with `badarg`.
   The `{write_concurrency, …}` surface is
   `WriteConcurrency::{Off, On, Auto, FixedLocks(n)}` (`TableOptions::write_concurrency_mode()`, the
   parse entry `WriteConcurrency::from_atom_name`), and `decentralized_counters` is the resolved
   `TableInfo` field the `ets:info/2` bridge reads directly. The registry-side `TableProfile` sidecar
   carries only what the model's boolean cannot express: the `auto` atom and the **normalized**
   `{debug_hash_fixed_number_of_locks, N}` rendering (a requested count rounded down to a power of
   two, then clamped up to `NLOCKS_WITH_ITEM_COUNTERS` = 64) for `ets:info(T, write_concurrency)`.
6. **The raw `Table`/`TableAccess` read family still returns bare `Eterm`s that alias table storage.**
   A caller can hold such a term past its borrow (or past a `take`, or past the table itself) and read
   back words a later same-size insert has reclaimed and reused. The term-returning raw forms are
   `Table::{lookup, lookup_element, lookup_element_or, lookup_element_all, lookup_element_all_or,
   first, last, next, prev, tab2list, take, select, select_limit, select_continue, select_reverse,
   select_reverse_limit, select_reverse_continue, match_, match_limit, match_continue, match_object,
   match_object_limit, match_object_continue}` and the mirrored `TableAccess` methods; the scalar
   forms (`member`, `size`, `memory`, `is_empty`, `select_count`, `select_delete`, `match_delete`,
   `select_replace`) return counts/bools. Only the shared handle (`SharedTable`) is escape-proof: its
   `TableRead`/`TableWrite`/`TableTerm` views bind every term to the guard's borrow (escaping is a
   compile error). The view layer carries the traversal API a host needs to avoid the raw reads —
   `TableRead::{iter_keys, iter_keys_rev, iter_objects, iter_objects_rev, foldl, foldr, tab2list_into}`,
   `TableTerm::tuple_elements` and the copy-out `TableTerm::copy_into` — so `beam_bifs` can adopt the
   safe views instead of the raw read family. `Table::set_owner` **is** called (at `ets:new/2`, §7.2).
   `take/2` carries a storage-lifetime consequence of its own: `Table::take` hands back table-owned
   `Eterm`s and `relinquish`es each checked-out copy, so `store.words()`/`Table::memory`/`size` drop by
   exactly the object's word count, and a `take` that drains the table releases the interior as well
   (`Table::take` calls `release_empty_interior`, so the emptied table reports its exact fresh floor).
   The relinquished words stay valid and are never returned to the free lists — a repeated take churn
   grows the store's block count (bounded only by the total taken volume) until
   `delete_all_objects/1`'s `reset()` or the table drop. Inherent
   to `take` returning a live checked-out term; reproduced by
   `tests/ets_reclamation.rs::take_style_relinquish_churn_retains_its_arena_until_the_store_is_reset`.
7. **Match-spec arithmetic operators default to the small-int/float subset.** `src/select/expr/arith.rs`'s
   `DefaultArithHook` computes checked `i64` arithmetic and faithful float `+ - * /`, and raises the
   typed `unsupported_ets` for a bignum operand or an integer result that leaves the 60-bit small
   range. Full arithmetic is a host-installed hook (`beam_ets::select::expr::set_arith_hook`), and the
   runtime installs one: **`beam_bifs`** (`beam_bifs/src/ets/arith_hook.rs`, arbitrary precision over
   `beam_term`'s number layer) opens it around every match-spec-evaluating BIF, so the runtime answers
   the oracle's value while the un-hooked `DefaultArithHook` stays the typed gap — never a wrong result.
   This crate never grows its own arithmetic (workspace Prevent-Duplication rule).
8. **`{write_concurrency,_}` is advisory.** `read_concurrency`/`write_concurrency` are parsed and
   reported (`TableOptions`/`TableInfo`) but there is no fine-grained locking; every correctness
   property rests on `SharedTable`'s coarse `Arc<RwLock<Table>>`, whose one `write()` guard spans a
   whole check-then-act sequence, so no update is lost. Fine-grained locking is a **throughput**
   workstream (owner: this crate) — never a correctness gap.
## 8. Storage-lifetime invariants (no residual gap)
* **An interior index key aliases a live, table-owned object.** The stored key term points *into* the
  object's `TermStore` storage, so an upsert that replaces (or a removal that reclaims) the aliased
  object re-points it: `ChampInterior` re-points its slot on an upsert and `HashInterior` re-points its
  entry on a `set` upsert. Covered by `tests/ets_reclamation.rs`.
* **A multi-kind `delete_object` re-anchors a surviving entry.** `HashInterior::with_keypos(kind,
  keypos)` remembers the key position so `delete_object` re-derives a surviving object's key term
  (`key_of(survivor, keypos)`), falling back to the previous key only when no object of the key
  remains.
* **`Table` derives a rebuilt object's index key from the table-owned copy, never the caller's key**
  (`Table::relocate_rebuilt_object`).
* **The B+-tree re-materialises every branch separator from the rightmost live leaf**
  (`NodeStore::subtree_max_key`): an upsert replaces the object that established a subtree maximum and
  reclaims its storage, so a stored separator term would dangle even when the key *value* is unchanged.
  The branch descent is a lower bound and binary-searches the strictly ascending separators.
  Covered by `tests/ets_reclamation.rs` (focused reproductions + memory-accounting stress across every
  kind and interior shape).
* **An interior's index storage follows live occupancy.** `ets:info(T, memory)` is the table-owned term
  storage plus the index storage, and every interior now releases that storage as objects leave
  instead of holding its high-water allocation: the chained hash index (`bag`/`duplicate_bag`, opt-in
  `set`) rebuilds its bucket array through `HashInterior::rehash_into` once the object load falls below
  an eighth (`maybe_shrink`, never below `INITIAL_BUCKETS` = 8, halving while it stays under 1/8 — a
  6× band below the 3/4 grow threshold) and trims an entry's object vector past 4× slack; the CHAMP
  `set` runs `trim_capacities` over the nodes a removal walked, releasing a slot vector once
  `capacity > 8 && capacity > 4 * len`; the `ordered_set` B+-tree compacts its node arena once retired
  slots outnumber live ones (`NodeStore::needs_compaction`/`compact` — a dense renumbering that
  re-trims each surviving node) and trims each leaf's entry vector on removal; the sorted-`Vec`
  `ordered_set` releases past `CAPACITY_SHRINK_FACTOR` = 4. An interior that empties is still rebuilt
  (`Table::release_empty_interior`, reached from `delete`/`delete_object`/`take` and
  `delete_all_objects`), so the **empty floor is exact**; at small sizes the residual is one slack
  allocation per the interior's own factor. The interior layout is not externally pinned
  (`../CONTEXT.md` §2), so the contract is the **shape** — memory falls with live data, no
  per-removal churn (a stable-size insert/delete churn keeps its maximum at the base), the exact empty
  floor — and the word counts themselves are this crate's own (measured trajectory in `PERF.md` §2;
  pinned by `tests/ets_capacity.rs`). `ets:safe_fixtable/2` is the one deliberate exception: while a
  table is fixed, removed storage waits in `deferred_frees`, so memory falls at unfix.
