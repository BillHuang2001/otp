# ETS surface census — where every `ets:*` operation lives

Static census of the **real** ETS surface OTP 29 / erts-17.1 exposes, against this workspace.
Purpose: a future agent must not re-derive "is `ets:X` implemented, and where?".
Gate source of truth for *behaviour* is the differential suite (`tests/differential.rs`, 102 cases,
`mismatch = 0`); this file is the *coverage* map.

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
| `new/2` — kinds `set`/`ordered_set`/`bag`/`duplicate_bag`, `{keypos,N}`, `named_table`, `private`/`protected`/`public`, `compressed`, `{write_concurrency,_}`, `{read_concurrency,_}` | implemented | `beam_ets::TableOptions`/`Table::new`; option **parsing** + the name registry + the named-table-conflict `badarg` in `beam_bifs::ets` (`new_2`, `parse_options`) |
| `new/2` — `{heir, Pid, Data}` / `{heir, none}` / `{decentralized_counters,_}` | missing | `beam_bifs::ets::parse_options` raises `badarg`; the creation-time heir is not modelled (only `ets:setopts/2` carries a heir, and its BIF is itself missing) |
| `delete/1` (the table) | implemented | `beam_bifs::ets::delete_1` → `TableRegistry::remove` + `remove_name` |
| `delete/2` (objects) | implemented | `beam_ets::Table::delete` |
| `delete_all_objects/1` | implemented | `beam_ets::Table::delete_all_objects` |
| `delete_object/2` | implemented | `beam_ets::Table::delete_object` |
| `info/1`, `info/2` | implemented | `beam_ets::Table::info` + `TableInfo::entries`; `beam_bifs::ets::info_field` renders the 15-field proplist (`id` is the tid handle, `owner` renders `undefined` — see *Runtime-level gaps*) |
| `all/0` | missing | no `beam_bifs` key; the stdlib `ets.erl` wrapper is not on the runtime's path |
| `whereis/1` | implemented | `beam_bifs::ets::extra::whereis` over the name registry |
| `rename/2` | missing | seam-backed `unsupported_bif` (`beam_bifs::runtime::other`); needs a name-registry rekey plus a `Table` name setter |
| `setopts/2` — `{heir, Pid, Data}` / `{heir, none}` | state here, BIF missing | `beam_ets::Table::setopts` + `SetOpt`; the BIF is a seam-backed `unsupported_bif`. Every other option (`{write_concurrency,_}`, …) is `badarg` in the oracle and unrepresentable here |
| `give_away/3` | state here, BIF missing | `beam_ets::Table::give_away`; the BIF is a seam-backed `unsupported_bif`. The `{'ETS-TRANSFER',…}` delivery is the runtime's |
| owner-death / heir hand-off | API here, no runtime wiring | `beam_ets::Table::owner_died` → `OwnerDeath`; no caller exists (see *Runtime-level gaps*) |
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
| `insert_new/2` | implemented | `beam_ets::Table::insert_new`; the oracle rule — `false` whenever the **key** already has an object, for **all four kinds** (including `duplicate_bag`) — is implemented per interior and pinned by the `kind` differential group |
| `update_counter/3`, `update_counter/4` | implemented | `beam_ets::Table::{update_counter, update_counter_with}` |
| `update_element/3` | implemented | `beam_ets::Table::update_element`; `beam_bifs::ets` parses the update list |
| `update_element/4` (create form) | implemented | `beam_bifs::ets::extra::update_element_with_default` (builds the default object, then `Table::insert`/`update_element`) |

## 4. Match / select family

| Operation | Status | Where |
|---|---|---|
| `select/2`, `select/3`, `select/1` | implemented | `beam_ets::Table::{select, select_limit, select_continue}` |
| `select_count/2` | implemented | `beam_ets::Table::select_count` |
| `select_delete/2` | implemented | `beam_ets::Table::select_delete` |
| `select_replace/2` | implemented | `beam_ets::Table::select_replace` (two-phase value-validated rewrite) |
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

These are the reference answers, each compared line-by-line against the live `erl` (`tests/differential`).

* **Access matrix.** `requester == owner` ⇒ read + write always. Otherwise: `public` ⇒ read + write;
  `protected` ⇒ read only; `private` ⇒ neither. Every denial is `badarg`.
  * `ets:info/1` and `ets:info/2` are **never** access-gated (a non-owner may `info` a `private` table).
  * `ets:safe_fixtable/2` and `ets:take/2` are gated as **read** / **write** respectively (a non-owner
    may fix a `protected` table).
  * `ets:rename/2` is **write**-gated (`ok` for a non-owner of a `public` table).
* **Owner death.** With a live heir, the heir receives `{'ETS-TRANSFER', Tid, FromPid, Data}` and
  becomes the owner while `ets:info(T,heir)` keeps reporting the heir (it is **not** reset to `none`);
  with no heir — or an already-dead heir (`ets:setopts(T,{heir,DeadPid})` is accepted) — the table is
  deleted. `give_away/3` to a dead pid or to itself is `badarg`; a successful one delivers the same
  message.
* **`ordered_set` keys are arithmetic.** `1` and `1.0` are the same key, the *last-inserted key term*
  is the one iterated (`ets:first`/`last`/`next`/`prev`), and iteration is in numeric order.
* **`insert_new/2` refuses on an existing key** for every kind, including `duplicate_bag`.
* **`insert/2` is atomic**: a list containing an invalid object is `badarg` and inserts nothing.
* **`lookup_element/3,4` on `bag`/`duplicate_bag` returns a list** of that element from every object.

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
   `Table::owner_died(dead, is_alive)` per owned table, deletes on `Deleted`, and delivers
   `{'ETS-TRANSFER', Tid, FromPid, Data}` on `Transferred`. The hand-off target must be a live pid,
   and the oracle normalises a dead heir to `none` *at option-parse time* (so `ets:info(T,heir)` reads
   `none`), which `beam_bifs`'s `setopts`/`new` option parser must reproduce.
5. **`SharedTable` read results can outlive their read guard.** See the crate `CONTEXT.md`
   (*Known Issues*) — reads alias table-owned storage, so a result kept past the `RwLock` read guard
   can be reclaimed and reused by a concurrent writer. Needed: a copy-out-on-read path, or a
   guard-scoped view type, for hosts that hand terms across the lock.
