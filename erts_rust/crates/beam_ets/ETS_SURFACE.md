# ETS surface census — where every `ets:*` operation lives
Static census of the **real** ETS surface OTP 29 / erts-17.1 exposes, against this workspace.
Purpose: a future agent must not re-derive "is `ets:X` implemented, and where?".
Gate source of truth for *behaviour* is the differential suite (`tests/differential.rs`, **135 cases**,
live oracle, `mismatch = 0`, `KNOWN_DIVERGENCES` empty); this file is the *coverage* map.
Three owners exist:
* **`beam_ets`** (this crate) — the table state + the observable `ets:*` semantics.
* **`beam_bifs`** — the BIF layer: argument parsing, the `TableRegistry` (id → table, name → id,
  continuation handles, compiled-spec handles), and the operations that need runtime facilities.
* **missing** — nothing implements it; the reason is named.
`beam_bifs`'s registry keys live in `beam_bifs/src/ets.rs`, `ets/matches.rs`, `ets/specs.rs`,
`ets/extra.rs` and the seam-backed remainder in `runtime/other.rs`.
## 1. Table lifecycle and metadata
| Operation | Status | Where |
|---|---|---|
| `new/2` — kinds `set`/`ordered_set`/`bag`/`duplicate_bag`, `{keypos,N}`, `named_table`, `private`/`protected`/`public`, `compressed`, `{write_concurrency,_}`, `{read_concurrency,_}`, `{heir, Pid}` / `{heir, Pid, Data}` / `{heir, none}`, `{decentralized_counters, Bool}` | state modelled here | `beam_ets::TableOptions` (all of the above, incl. a **table-owned** copy of a creation-time heir's gift data at `Table::build`) + `Table::new` |
| `new/2` — option **parsing** + the name registry + the named-table-conflict `badarg` | partial in `beam_bifs` | `beam_bifs::ets` (`new_2`, `parse_options`) accepts only the kinds/protection/`named_table`/`{keypos,N}` today; the rest are `badarg` there (see §7.5) |
| `delete/1` (the table) | implemented | `beam_bifs::ets::delete_1` → `TableRegistry::remove` + `remove_name` |
| `delete/2` (objects) | implemented | `beam_ets::Table::delete` |
| `delete_all_objects/1` | implemented | `beam_ets::Table::delete_all_objects` |
| `delete_object/2` | implemented | `beam_ets::Table::delete_object` (removes **every** equal object — pinned for the multi kinds) |
| `info/1`, `info/2` | implemented | `beam_ets::Table::info` + `TableInfo::entries` (15 fields in oracle order; `decentralized_counters` from the measured rule, `id` is the tid handle, `owner` and a set `heir` render `undefined` — see *Runtime-level gaps*) |
| `all/0` | missing | no `beam_bifs` key; the stdlib `ets.erl` wrapper is not on the runtime's path |
| `whereis/1` | implemented | `beam_bifs::ets::extra::whereis` over the name registry |
| `rename/2` | state here, BIF missing | `beam_ets::Table::rename(name, is_taken) -> Result<RenameOutcome>` (oracle-pinned: `named_table` rekeys and returns the new name atom, conflicts/self-rename `badarg`; a table without `named_table` renames silently with no conflict check and returns its id). The registry rekey + the non-atom/unknown-table `badarg`s are `beam_bifs`'s |
| `setopts/2` — `{heir, Pid}` / `{heir, Pid, Data}` / `{heir, none}` | state here, BIF missing | `beam_ets::Table::setopts` + `SetOpt`; the copying install is `Table::install_heir(owner, Option<Eterm>)` and the read-back `Table::heir_data()`. The BIF is a seam-backed `unsupported_bif` |
| `give_away/3` | state here, BIF missing | `beam_ets::Table::give_away`; the `{'ETS-TRANSFER',…}` delivery is the runtime's |
| owner-death / heir hand-off | API here, no runtime wiring | `beam_ets::Table::owner_died` → `OwnerDeath::{NotOwner,Deleted,Transferred(Heir)}`; a host sends the transfer message **only when `heir.data` is `Some`** (see §6) |
| `safe_fixtable/2` | implemented | `beam_ets::Table::set_fixation`, **read-gated** view `TableAccess::set_fixation`; `beam_bifs::ets::extra::safe_fixtable` calls the non-checking primitive |
## 2. Reads and navigation
| Operation | Status | Where |
|---|---|---|
| `lookup/2` | implemented | `beam_ets::Table::lookup` |
| `lookup_element/3`, `lookup_element/4` | implemented | `beam_ets::Table::lookup_element` / `lookup_element_or`; for `bag`/`duplicate_bag` the oracle's result is the element **list** and the crate's scalar form raises — the list forms are `lookup_element_all` / `lookup_element_all_or` (the `beam_bifs` bridge must route the multi kinds there) |
| `first/1`, `next/2`, `last/1`, `prev/2` | implemented | `beam_ets::Table` (`first`/`next`/`last`/`prev`, `Option::None` ⇒ `'$end_of_table'`) |
| `member/2` | implemented | `beam_ets::Table::member` |
| `tab2list/1` | implemented | `beam_ets::Table::tab2list` |
| `slot/2` | missing | seam-backed `unsupported_bif` (`beam_bifs::runtime::other`) |
| `first_lookup/1`, `last_lookup/1`, `next_lookup/2`, `prev_lookup/2` | missing | seam-backed `unsupported_bif` (C-interior internals) |
| `foldl/3`, `foldr/3` | missing | need `FunCaller` (calling a `fun` per element) — documented in `beam_bifs::ets` |
| `take/2` | implemented | `beam_ets::Table::take` (checked-out copies; the stored originals are reclaimed) |
| `is_compiled_ms/1` | implemented | `beam_bifs::ets::extra` over the registry's spec table |
## 3. Writes
| Operation | Status | Where |
|---|---|---|
| `insert/2` | implemented | `beam_ets::Table::insert` / `insert_list` (**atomic**: the whole list is validated and deep-copied first, then inserted); `beam_bifs::ets` splits a list argument |
| `insert_new/2` | implemented | `beam_ets::Table::insert_new`; the oracle rule — `false` whenever the **key** already has an object, for **all four kinds** (including `duplicate_bag`) — is implemented per interior and pinned by the `kind`/`multikind` groups |
| `update_counter/3`, `update_counter/4` | implemented | `beam_ets::Table::{update_counter, update_counter_with}` (`badarg` on the multi kinds, matching the oracle) |
| `update_element/3` | implemented | `beam_ets::Table::update_element`; `beam_bifs::ets` parses the update list |
| `update_element/4` (create form) | implemented | `beam_bifs::ets::extra::update_element_with_default` (builds the default object, then `Table::insert`/`update_element`) |
## 4. Match / select family
| Operation | Status | Where |
|---|---|---|
| `select/2`, `select/3`, `select/1` | implemented | `beam_ets::Table::{select, select_limit, select_continue}` |
| `select_count/2` | implemented | `beam_ets::Table::select_count` |
| `select_delete/2` | implemented | `beam_ets::Table::select_delete` |
| `select_replace/2` | implemented | `beam_ets::Table::select_replace` (two-phase value-validated rewrite; `bag` ⇒ `badarg`, `duplicate_bag` rewrites each matched copy) |
| `select_reverse/1,2,3` | missing | seam-backed `unsupported_bif` |
| `match/2`, `match/3`, `match/1` | implemented | `beam_ets::Table::{match_, match_limit, match_continue}` |
| `match_object/2`, `/3`, `/1` | implemented | `beam_ets::Table::{match_object, match_object_limit, match_object_continue}` |
| `match_delete/2` | implemented | `beam_ets::Table::match_delete` |
| `match_spec_compile/1` | implemented | `beam_bifs::ets::specs` (registry of raw specs) + `beam_ets::compile` for validation |
| `match_spec_run/2` | implemented | `beam_ets::match_spec_run` driven by the registry (`beam_bifs::ets::specs`) |
| `match_spec_test/3` (`erlang:`) | implemented | `beam_bifs::match_spec` (native structural lint + `table` dialect → `beam_ets`); the `trace` dialect is `unsupported_bif`. `match_spec_test/2` is not an OTP 29 export |
| `fun2ms/1` | missing | an `ms_transform` stdlib parse transform, not a BIF; `beam_ets` has the compiled-spec *shape* only (`select` tests are named `fun2ms_*` because they reproduce `fun2ms/1`'s output shape) |
| `internal_select_delete/2`, `match_spec_run_r/3`, `internal_delete_all/2`, `internal_request_all/0` | missing | seam-backed `unsupported_bif` (C-interior internals) |
| `repair_continuation/2` | missing | stdlib `ets.erl` wrapper |
## 5. Table-file / misc
| Operation | Status | Where |
|---|---|---|
| `tab2file/2,3`, `file2tab/1,2`, `tabfile_info/1` | missing (blocked) | The table-file format is `{Header,Objects,Footer}` where **every** field is an ETF term, so it needs a `beam_codec` edge, plus file I/O (`beam_io`/`std::fs`) — both new, and the `beam_io` one points *upward*, forbidden for this wave-1 crate. Also needs `{extended_info,_}`, `sync`/`sync_verbose`, ~10 000-object chunking and re-raise semantics. `tab2list` + `first`/`next` remain the enumeration path |
| `table/1,2`, `clear/1` | missing | stdlib `ets.erl` wrappers |
| `size/1` | not an OTP export | correct absence (`ets:info(T, size)` is the spelling) |
## 6. Oracle-probed semantic facts pinned by the differential suite
These are the reference answers, each compared against the live `erl` (`tests/differential`, 135 cases).
* **Access matrix.** `requester == owner` ⇒ read + write always. Otherwise: `public` ⇒ read + write;
  `protected` ⇒ read only; `private` ⇒ neither. Every denial is `badarg`.
  * `ets:info/1` and `ets:info/2` are **never** access-gated (a non-owner may `info` a `private` table).
  * `ets:safe_fixtable/2` and `ets:take/2` are gated as **read** / **write** (a non-owner may fix a
    `protected` table); `ets:rename/2` is **write**-gated.
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
  the table is deleted. `give_away/3` to a dead pid or to itself is `badarg`.
* **The heir's two forms.** `{heir, Pid}` (arity 2) inherits the table **silently — no
  `{'ETS-TRANSFER',…}` message at all** (the C `THE_NON_VALUE` sentinel), whereas
  `{heir, Pid, undefined}` (arity 3) delivers the message carrying `undefined`. The gift data is
  **deep-copied into table-owned storage** at creation and at `setopts`/`install_heir` (the C
  `ERTS_ALC_T_DB_HEIR_DATA` copy), so it outlives the installing process's heap.
* **`decentralized_counters`** = the **effective** `write_concurrency` **and**
  (`{decentralized_counters, Bool}` when given explicitly, else the table kind is `ordered_set`).
  The explicit option is accepted for every kind (`{decentralized_counters,true}` +
  `{write_concurrency,true}` on a `set`/`bag`/`duplicate_bag` reports `true`; explicit `false` on an
  `ordered_set`+`write_concurrency` reports `false`), a `private` table reports `false` (the
  fine-locked flag is normalised off), and a non-boolean value is `badarg`.
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
* **Read results and the copy boundary.** A read returns a term **aliasing table-owned storage**; a
  term read through the multi-threaded handle ([`SharedTable`]) is a guard-scoped `TableTerm` that
  cannot outlive its guard (a compile error), and callers that need to keep a term copy it out
  (`TableTerm::copy_into`, `TableRead::select`). See the crate `CONTEXT.md` for the raw-`Table` gap.
## 7. Runtime-level gaps (NOT in this node — the exact API each needs)
1. **No BIF path is access-gated.** `beam_bifs`'s `ets:*` bridge calls the owner-level `Table::*`
   primitives directly, so the protection matrix is enforced only by `TableAccess` + the differential
   battery, never by the running runtime's dispatch. Needed: the `ets:*` bridge must build
   `Table::as_requester(self_pid)` per call and go through `TableAccess` (and use
   `TableAccess::set_fixation` for `safe_fixtable/2`), while keeping `info/1,2` ungated.
2. **No owner is ever recorded.** `beam_bifs::ets::new_2` never calls `Table::set_owner`, and
   `beam_bifs::TableRegistry` has no owner index, so `OwnerId` is always `None` and
   `ets:info(T, owner)`/`T, heir` always render `undefined`. Needed: record `Table::set_owner(Some(..))`
   from `ctx.self_pid()` at `new/2`, plus a `ProcessId ↔ OwnerId` bridge.
3. **No process-death hook anywhere.** `beam_process`'s sole finalizer is
   `ProcessTable::terminate` → `terminate_into` (`ptab.rs`), which calls no subsystem back; a
   runner-side hook would be incomplete because linked exits / `exit/2` bypass the runner. Needed: a
   `ProcessExitObserver` trait on `ProcessTable` (mirroring the existing `DistSignalRouter` seam — no
   `beam_process → beam_ets` dependency edge), invoked between draining relationships and
   `drop(process)` so the exit reason is still live.
4. **Nothing calls the owner-death entry point.** Needed in `beam_rt` (the node owning both the
   `ProcessTable` and the `Rc<RefCell<TableRegistry>>`): an owner index over the registry
   (`tables_owned_by`), a `TableRegistry::owner_died(OwnerId)` sweep that calls
   `Table::owner_died(dead, is_alive)` per owned table, deletes on `Deleted`, and on `Transferred`
   delivers `{'ETS-TRANSFER', Tid, FromPid, Data}` — **only when `heir.data` is `Some`**, and using the
   registered name as `Tid` for a `named_table`.
5. **The `beam_bifs` option parser lags this crate's model.** It rejects `compressed`,
   `{write_concurrency,_}`, `{read_concurrency,_}`, `{heir,…}` and `{decentralized_counters,_}` with
   `badarg`, and its `ets:info/2` bridge recomputes `decentralized_counters` from
   `kind.is_ordered() && write_concurrency` instead of reading `TableInfo::decentralized_counters`.
   Needed: accept every option `TableOptions` can hold (mapping `{heir,Pid}` ⇒ `Heir { data: None }`,
   `{heir,Pid,D}` ⇒ `Some(D)`, `{heir,none}` ⇒ `set_heir(None)`), and switch the info field to the
   `TableInfo` value. Options `TableOptions` still cannot express: `{write_concurrency, auto}` and
   `{write_concurrency, {debug_hash_fixed_number_of_locks, N}}` (the flag is a `bool`; the oracle
   accepts `auto` and reports `wc=auto`, `decentralized_counters=true`).
6. **Raw-`Table` reads still alias table storage.** `Table`/`TableAccess` return bare `Eterm`s that a
   caller can hold past the borrowed table (or past a `take`), so only the shared handle is
   escape-proof. This is a cross-crate API decision (a host as `beam_bifs` consumes the raw forms):
   the names are `Table::{lookup, member, lookup_element, lookup_element_or, lookup_element_all,
   lookup_element_all_or, first, last, next, prev, tab2list, take, select, select_count,
   select_delete, match_object, match_, match_delete, select_replace}` and the mirrored
   `TableAccess` methods; the fix is to route them through the guard-scoped `TableRead`/`TableWrite`
   view layer (`src/table/view/`) or a scoped `Table::view(&self)` handle.
