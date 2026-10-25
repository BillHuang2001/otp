# `src/nif/enif/` — the `enif_*` family implementations (contract A.3)

## Intent
One file per `enif_*` family: the `#[no_mangle] pub unsafe extern "C"` bodies a NIF `.so` imports, written against the `NifEnv`/`NifHost` seam owned by `../CONTEXT.md` and the frozen `nif::api`/`nif::types` shapes owned by `../../../CONTEXT.md`.
This node also owns the **A.3 host-extension seams** the C gets from emulator state: the `enif_schedule_nif` continuation trampoline, the off-call-path env reachability, `enif_select` and the resource `stop`/pending-stop pair, and the five resource seams (`resources_keep`, `resources_monitors`, `resources_release`, `resources_term`, `resources_stop`).
The slot *list* is the code: `erts/emulator/beam/erl_nif_api_funcs.h` is the ground truth, `nif::api` mirrors it field for field, and `../../../tests/abi_conformance.rs` re-derives both. This file states behaviour, not rosters.
Ground truth for behaviour (read-only, never modify): `erts/emulator/beam/erl_nif.c`, `erl_map.c`, `utils.c`, `erts/emulator/sys/common/erl_check_io.c`, and `erts/lib_src/common/erl_printf_format.c` for the `*printf` subset (paths are repo-root relative).

## API Surface
### The family roster (MEASURED — `Some`/total slots)
| family file | subject | `Some`/total |
|---|---|---|
| `mem` | `enif_alloc`/`free`/`realloc`, binaries, ETF | 13/13 |
| `get` | `enif_get_*` accessors | 12/12 |
| `make` | `enif_make_*` constructors (maps excepted) | 18/18 |
| `map` | map accessors/constructors | 7/7 |
| `pred` | `enif_is_*`, `compare`, `is_identical`, `hash`, `term_type`, `term_size` | 17/17 |
| `proc` | self/pid/port, atom-cache queries | 7/7 |
| `process` | monitors, send, port command, whereis, `schedule_nif` | 12/12 |
| `exception` | badarg/raise/pending/clear | 4/4 |
| `threads` | mutex/cond/rwlock/tsd/thread | 34/34 |
| `ioq` | I/O queue + I/O vector | 10/10 |
| `mapiter` | `enif_map_iterator_*` | 7/7 |
| `time` | time, env, system info, unique integers, timeslice, `dl*` | 11/11 |
| `resources` | resource types + allocations | 11/11 |
| `select` | `enif_select`/`enif_select_x` | 2/2 |
| `envs` | `enif_alloc_env`/`enif_free_env`/`enif_make_copy`/`enif_priv_data` | 4/4 |
| `variadic` | the seven C-variadic slots (cfg `beam_abi_variadic_shim`) | 7/7 |

Every one is `#[no_mangle] pub unsafe extern "C"` with the exact signature of its matching `nif::api` field, driving the `NifEnv`/`NifHost` seam (`beam_term`, `beam_alloc::Heap`, `NifHost::intern_atom`).
The seven C-variadic slots are `Some` only with the `csrc/nif_variadic.c` shim compiled (else `None`, a loud `dlopen` failure, never a wrong result).
`pub fn enif_api_table() -> &'static ErlNifApi` returns the `#[used] static ENIF_API` built by the family `populate` chain from `ErlNifApi::empty()`; the `#[used]` static keeps the `#[no_mangle]` symbols in the final link.

### `mem` — allocation, binaries, ETF
libc-backed `enif_alloc`/`enif_free`/`enif_realloc`; the binary family (`alloc_binary`, `realloc_binary`, `release_binary`, `make_new_binary`, `make_binary`, `make_sub_binary`, `inspect_binary`, `inspect_iolist_as_binary`); the ETF pair `term_to_binary`/`binary_to_term` through `NifHost` (the C's failure `0` without the hook).
The binary I/O shapes are **oracle-exact** (`erl_nif.c`, `erts_inspect_iolist_as_binary`, `erts_copy_bits_fwd`, `erts_build_sub_bitstring`):
- `enif_binary_to_term(env, data, len, &term, opts)` answers the **consumed byte count** of the first term (`erts_binary_to_term`'s `used`, the C's `return bp - data`), not `len`: a `term_to_binary(T) ++ junk` image answers `size_of(T)` and writes `T`; only a decode failure answers `0`.
  `opts` may be `0` or `ERL_NIF_BIN2TERM_SAFE` — anything else answers `0` **without** decoding (the C's `switch`); a null `data`/`term` answers `0`.
  The host seam is `NifHost::binary_to_term_used`; the term is written only on success (the C also leaves its internal non-value there on failure, so a NIF must not read the operand after a `0` either way).
- `enif_inspect_iolist_as_binary` accepts a byte integer only in head (CAR) position, a proper list, a byte-sized binary/bitstring, or the empty list; `[]`/`nil` **succeeds** (`size = 0`, non-NULL dummy `data` = `(unsigned char*)&bin->data`, `erl_nif.c:1400`).
  A bare integer, a non-nil non-list tail (`[1|2]`), a non-byte integer (`256`, `-1`) or a non-byte-sized bitstring (`<<1:7>>`) are **failures → `0`** (`size = 0`, `data = -1`).
- `enif_inspect_binary` accepts a bit-offset, byte-sized bitstring (`TAIL_BITS(size) == 0` ⇒ `1`); an unaligned start is copied by `copy_bits_fwd` and handed back **owned** (null `__spare__[0]`, freed once by `enif_release_binary`).
- `enif_make_sub_binary` windows **any bitstring by bits**: offset `source_bit_offset + pos*8`, span `size` bits; `size == 0` ⇒ `<<>>` for any `pos`; ≤ `ERL_ONHEAP_BITS_LIMIT` (512) bits copies into an `ErlHeapBits`, above it an `ErlSubBits` window; `badarg` only for a non-bitstring source or `pos*8 + size*8 > source_bit_size` with `size > 0` (`checked_mul`).

### `get` — the accessor family
`enif_get_*` for int/uint/long/ulong/double/list_cell/tuple/list_length/string/string_length/atom/atom_length.
The `ERL_NIF_UTF8` arm of `enif_get_string` is **byte-faithful** to `erts_unicode_list_to_buf` (`utils.c:2862-2942`): byte budget `len-1`, char budget `len`, an incompletely fitting multibyte char dropped **whole** — `buf[written] = '\0'` + negative `-(written+1)`, else `buf[0] = '\0'` + `0`; `enif_get_string_length` agrees.

### `make` — the constructor family
`enif_make_*` for int/uint/long/ulong/double/atom/new_atom/existing_atom/string/list_cell/reverse_list/ref/tuple_from_array/list_from_array (plus the `_len` variants).
The four make-atom entry points **record** each interned atom on the env; `enif_make_existing_atom(_len)` consults that record first, then `NifHost::existing_atom` (else `0` with `*atom` untouched).
`enif_make_ref` raises `badarg` when `NifHost::make_ref` is absent.

### `map` — the map family
`enif_get_map_size`, `enif_get_map_value`, `enif_make_new_map`, `enif_make_map_from_arrays`, `enif_make_map_put`, `enif_make_map_update`, `enif_make_map_remove`.
`enif_make_map_from_arrays` reproduces `erts_map_from_ks_and_vs`'s **duplicate-key refusal** (`erl_map.c`: `THE_NON_VALUE` on a duplicate): a duplicate anywhere in the key array writes `Eterm::non_value()` into `*map_out` (nothing when that pointer is null) and answers `0`; only a duplicate-free set builds the map and answers `1`.
The basis is **`=:=`** (`compare::eq` — the flat-map writer's `erts_cmp(exact)` and the hash arm's `reject_dupkeys`), so `1` and `1.0` are **distinct** keys on both sides of the `MAP_SMALL_MAP_LIMIT = 32` flat/hash switch.
The check is an all-pairs scan run **before** any allocation, and the no-duplicate path is unchanged (same content, same key order).

### `pred` — predicates, comparison, hashing
`enif_is_*`, `enif_compare`, `enif_is_identical`, `enif_hash`, `enif_term_type`, `enif_term_size`.
`enif_compare` is the C's `CMP` = `erts_cmp(a, b, 0, 0)` (**arithmetic** mode: `enif_compare(1, 1.0) == 0`) and orders an atom pair by **name** (`erts_cmp_atoms`, `erl_utils.h:193-215`), each name resolved *current env atom record → `NifHost::atom_name` → raw atom index* — which is what makes in-call `enif_make_atom` atoms resolvable.
`enif_is_identical` is `EQ` (exact mode); `enif_hash` is PHASH2 over the term word. Atom pairs are resolved at **top level** only — see *Known gaps*.

### `proc` — self, pid and port handles
`enif_self`, `enif_get_local_pid`, `enif_get_local_port`, `enif_set_pid_undefined`, `enif_is_pid_undefined`, `enif_get_atom_cache_index`, `enif_max_atom_cache_index`.
`enif_make_pid`/`enif_compare_pids` are **not slots and must not become any**: on LP64 both are header macros (`erl_nif_api_funcs.h:655-658`) that expand inline next to `enif_compare`, so no NIF imports them, and the oracle's `beam.smp` exports exactly the same 176 `enif_*` dynamic symbols (`nm -D`).

### `process` — liveness, monitors, send, scheduling
`enif_monitor_process`/`demonitor_process`/`compare_monitors`/`make_monitor_term`, `enif_is_process_alive`/`is_current_process_alive`/`is_port_alive`, `enif_send`, `enif_port_command`, `enif_whereis_pid`/`whereis_port` (0 when the `NifHost` hook is absent), `enif_schedule_nif`.
`enif_monitor_process` reproduces the C's three codes **in the C's order**:
- a resource type opened **without** a `down` callback answers `-1` *before* the pid is even looked at (`resources::resource_type_has_down` — crate-local knowledge, so `-1` is reachable with no host);
- a null/`undefined` target answers `1`; a monitor that could not be armed because the target is already dead answers `1`;
- success writes **the whole reference block** into the 32-byte `ErlNifMonitor` (`REF_THING_BYTES` = 32 = the entire payload: this tree's frozen 4-word `[header, node, serial, id]` block, whose identity is the id word — so two monitors armed from different references stay distinct) and answers `0`;
- a caller-supplied C-layout 3-word `ErtsORefThing` is accepted at its own width with a zeroed tail (its packed `num[]` ids are inside the words copied); any other header arity is refused (`ref_block_of`'s never-over-read rule).
The arming host reports the outcome through `NifHost::monitor_process_result` ⇒ `NifMonitorResult::{Armed, NotAlive, NoDownCallback}`; an `Armed` term that is not an internal reference is refused (`1`) rather than stored.
A successful arm also records the binding in the crate's monitor tree (`super::resources::record_monitor`, the C's `erts_monitor_tree_insert`, `erl_nif.c:3983-3986`, done before the block is stored) so `down` can be delivered later; a foreign `obj` (not one of this crate's live payload pointers) or a type without `down` is simply not tracked, and the answer stays `0` either way.
`enif_demonitor_process` consults **its own tree first** (`super::resources::take_monitor`) and answers `0` when that removal succeeded, else falls back to `NifHost::demonitor_process` through the rebuilt reference term; `1` when neither found it (already gone / target dead) — the C's `:4043-4051`.

### `schedule` — the continuation trampoline
`schedule.rs` has no `enif_*` slot of its own (`process` owns `enif_schedule_nif`); it *is* the trampoline the slot drives.
An explicit `Outcome { Value(Eterm), Reschedule(Continuation), Raised }` over a thread-local pending-continuation slot and nesting counter: a **nested** schedule records the continuation and answers the C's `THE_NON_VALUE` (`0`), the **outermost** call loops to `Value`/`Raised`, so a long self-rescheduling chain never grows the Rust stack.
The refusals stay the C's, in the C's order (`argc > MAX_ARG` (255) first, then `argc < 0`, then a `flags` bit outside `ERL_NIF_DIRTY_JOB_{CPU,IO}_BOUND`, then null/non-UTF-8 `fun_name`, then null `fptr`, plus a null `argv` with `argc > 0`), each answering the `badarg` term the host turns into a raise.
`argv` is passed straight to `fptr` — **nothing is copied**, the arguments live on the caller's env heap for the whole trampoline — and `fun_name` is **interned and never resolved** (measured `sched_ghost_name(99)` → `{ghost,99}`).
`NifHost::nif_yield_point(flags)` runs once per iteration and is an **honest no-op** (fairness, never correctness); a thread-local dirty marker makes `enif_thread_type()` answer `ERL_NIF_THR_DIRTY_CPU_SCHEDULER` (2) / `..._IO` (3) for a dirty-flagged continuation, while the thread identity stays the caller's because no dirty pool exists here.
A raise inside a continuation is **terminal** (checked before a recorded reschedule), leaves the pending exception on the env, and is attributed to the **scheduling** NIF with the original MFA/argv (the engine owns the stacktrace head, so no frame is fabricated this side of the seam).
Substitution: the outer call returns the continuation's **value** where the C returns the `THE_NON_VALUE` trap marker — the Erlang-visible result is identical because the emulator only re-runs the trap.

### `exception`, `threads`, `ioq`, `mapiter`, `time`
- `exception` — `enif_make_badarg`, `enif_raise_exception`, `enif_has_pending_exception`, `enif_clear_env` (which does **not** touch the exception slot, as the C, `erl_nif.c:700-723`).
- `threads` — the 34 mutex/cond/rwlock/tsd/thread slots are cast-and-forward facades over `crate::sync`; only `enif_thread_type` and the `*_name` accessors (`enif_thread_name`/`erl_drv_thread_name`) do more.
- `ioq` — the queue/`enq_binary`/`enqv`/`size`/`deq`/`peek` family plus `enif_inspect_iovec`/`enif_free_iovec`, as a `VecDeque` of buffers preserving the vector *count* rule (one `SysIOVec` per non-empty enqueued buffer).
- `mapiter` — the seven `enif_map_iterator_*` slots over the frozen 56-byte `ErlNifMapIterator`, in the map's **storage order** (`hashmap_iterator_next`); the hash arm's `ErtsDynamicWStack` is a boxed pair list and `get_pair` answers the same sequence.
- `time` — the time/`convert_time_unit`/`now`/`cpu`/`system_info`/`getenv`/`unique_integer`/`consume_timeslice`/`dlopen`/`dlsym` family. `enif_system_info` (no env) reaches the host through `with_current_host` and prefers `NifHost::system_info`; `enif_make_unique_integer` prefers `NifHost::unique_integer`. The monotonic-time / time-offset relation is reproduced and pinned by the file's own unit tests (`monotonic_time_is_monotone_and_unit_consistent`, `monotonic_plus_offset_is_wall_time`); there is **no** `erl`-oracle pin for `enif_time_offset/1`.

### `resources` — types, allocation, and term identity
The eleven slots over a crate-internal type registry + a real refcounted allocation (`open_resource_type`, `_x`, `init_resource_type`, `alloc_resource`, `release_resource`, `keep_resource`, `make_resource`, `make_resource_binary`, `get_resource`, `sizeof_resource`, `dynamic_resource_call`).
A resource *term* is a reference **minted here** (`make_ref`, `None`), recorded by its **reference identity**: one private `term_key` helper (`TermKey::Ref(internal_ref_id)` for a frozen internal reference, `TermKey::Word` for every other term shape) drives both binding and lookup, so a `copy_struct` copy of a minted term resolves to the same object exactly as the C's pointer-identified magic ref does, and `ResourceRef` keeps the minted word so the release seam still enumerates what was minted.
An exporting call (`enif_make_resource`/`enif_make_resource_binary`) takes **its own reference** as the C magic ref does, so a released-but-term-wrapped object stays alive and `enif_get_resource` keeps resolving it (oracle-verified).
The term's reference is dropped by the **release seam** (`resources_release.rs`) — the crate has no GC of its own, so only the host knows a term died.
The destructor runs **synchronously** on whichever thread drops the last reference (the C schedules it on a misc aux-work thread): counts, ordering and the callback's environment/payload match the oracle, only the timing differs; the asynchronous form **cannot be reproduced safely** (`Send`, `*mut dyn NifHost`).
`Registry` also carries the **monitor tree** `monitors: HashMap<MonitorId, MonitorBinding>` (`obj`, `ty`, and the recorded target pid word) behind the *same* mutex as `types`/`terms`/`objects` — a second lock would invert against `destroy`/`forget_object` — and `Registry::forget_object` cancels an object's monitors (`forget_monitors_for_object`, the C's `dtor_demonitor`, `erl_nif.c:2940-2948`) so a destroyed resource's monitors die silently with no `down`.

### The explicit keep/unkeep seam (`resources_keep.rs`)
`nif::enif::{keep_resource, unkeep_resource}` — the env-free Rust half of the C's `enif_keep_resource`/`enif_release_resource` (`erl_nif.c:3177`/`:3164`), for a caller that has no `*mut ErlNifEnv` and is `#![deny(unsafe_code)]` (`beam_rt`).
`pub fn keep_resource(obj: *mut c_void) -> bool` (`true` = a reference was taken) and `pub fn unkeep_resource(obj: *mut c_void) -> bool` (`true` = a reference was dropped) are the pair `enif_select`'s route needs: arm an interest ⇒ keep, stop/cancel ⇒ unkeep (the C's per-interest `enif_keep_resource(resource->data)`, `erl_check_io.c:1446`/`:1509`, released at `:1533`/`:1834`/`:2242`).
Both are safe `pub fn` with `#[allow(clippy::not_unsafe_ptr_arg_deref)]` and the rationale inline, and **neither is an `enif_*` slot**: no `#[no_mangle]`, no new C symbol.
A null handle ⇒ `false`; a pointer that is not a live payload of this crate (`with_registry(|r| r.object_type(obj))` ⇒ `None`) ⇒ `false`, and the helper **never dereferences** an unrecognised pointer (the C computes `DATA_TO_RESOURCE(obj)` unconditionally).
`keep_resource` takes its reference with the guarded `retain_object_if_alive` (the C's `erts_refc_inc_unless(…, 0, 0)`, `erl_nif.c:3092-3096`) and answers `false` for an **already dying** object rather than resurrecting it — the state the C asserts against with `ASSERT(!erts_dbg_is_resource_dying(resource))` (`erl_check_io.c:1261`).
`unkeep_resource` drops its reference with the guarded `release_object_if_alive` (a `fetch_update` decrement, never a `fetch_sub`), so a concurrent destroy cannot underflow the count; the release that reaches zero runs the destructor **synchronously** and frees the block.
An unkeep after destruction answers `false` because `destroy` → `Registry::forget_object` unregisters the payload under the registry lock **before** the block is freed.
Honest residuals (documented in the module): the liveness set is keyed by payload **address**, so a stale pointer whose address a later `enif_alloc_resource` reused is indistinguishable (the registry's existing substitution hazard), and a **double** unkeep of a still-live object stays the C's caller error (the guard only refuses to underflow).

### The resource `stop` callback + the pending-stop table (`resources_stop.rs`, `stop.rs`)
`nif::enif::{resource_type_has_stop, call_resource_stop, record_pending_stop, run_pending_stops, pending_stop_count}` — the ABI half of `erts_resource_stop` (`erl_nif.c:3032-3041`): a **nooproc message env** built from `ResourceType::host` exactly as `destroy` builds the `dtor` env, then `type->fn.stop(&env, obj, event, is_direct_call)`.
`call_resource_stop` answers `true` only when this crate allocated the object, its type has a `stop`, and the callback ran; a null, foreign, already-freed or `stop`-less target answers `false` and is **never dereferenced** — the oracle SIGSEGVs there (`ASSERT(resource->type->fn.stop)`, `erl_check_io.c:1279`, compiled out in a release build), so this is a deliberate typed decision.
`select`'s STOP arm drives it **after** `NifHost::select` answers (the host owns the poller and picks the verdict) and returns the host's code **unchanged**:
- `STOP_CALLED` ⇒ a direct call (`is_direct_call = true`);
- `STOP_SCHEDULED` ⇒ `record_pending_stop(obj, event)` parks the pair taking **one** `keep_resource` reference, and `run_pending_stops() -> usize` drains it with `is_direct_call = false` and gives that reference back;
- anything else (plain `0`, the `*_CANCELLED` bits, a negative refusal) calls nothing.
Per-verdict reference accounting: `CALL_STOP` leaves the caller's reference alone, `CALL_STOP_AND_RELEASE` means the runtime already released the select's own, `STOP_SCHEDULED` is released by the drain. A second STOP for the same `(obj, event)` **steals** the parked entry (the C's `steal_pending_stop_nif`) — one reference, never a leak and never a double release.
`STOP_SCHEDULED` is reachable only for a host that really defers the stop: on Linux/epoll an epoll-able fd is always synchronous, so `beam_rt`'s immediately-deregistering store faithfully answers `STOP_CALLED`.
The balanced pin (measured on the oracle, reproduced in-crate) is 8 objects → 8 destructors across all four verdicts, each exactly once.

### The resource `down`-delivery seam (`resources_monitors.rs`)
`nif::enif::{deliver_resource_down, deliver_resource_down_id, resource_monitors, MonitorId}` — the ABI half of `erts_fire_nif_monitor` (`erl_nif.c:3058-3115`).
`MonitorId` is the **whole stored reference block** this tree carries — `[u8; REF_THING_BYTES]`, 32 bytes filling the `ErlNifMonitor` payload (the C's `ERTS_REF_THING_SIZE * sizeof(Eterm)` form of the same notion is 24 because the C's packed `num[]` ids hold its identity, `io.c:6994-7010`), not the `Eterm` it was minted from: `MonitorId::from_term(Eterm) -> Option<MonitorId>`, `unsafe MonitorId::from_holder(*const ErlNifMonitor) -> Option<MonitorId>`, `MonitorId::bytes() -> [u8; 32]`.
`deliver_resource_down(monitor: Eterm, pid: Eterm) -> bool` is the reference-term route (`false` for a non-reference term); `deliver_resource_down_id(monitor: MonitorId, pid: Eterm) -> bool` is the holder route a host takes when the minted reference lived on a NIF-call heap that is gone.
Delivery, in order:
1. removes the binding **under the lock** — only one caller can ever take it, the C's unlink-before-call `:3091` — and answers `false` with no binding;
2. still under the lock requires the object to be a live registered payload and takes a **protective** reference with `retain_object_if_alive` (the C's `erts_refc_inc_unless(...,0,0)` `:3092`; `false` is the C's `brefc == 0` case, `:3093-3096`, where `down` is *not* called);
3. releases the lock and only then runs the callback — `nif_pid = ErlNifPid { pid }`, `nif_mon = holder_from_block(id.bytes())`, env from the **type's** host exactly as `destroy` builds the `dtor` env (`:3105-3108`) — so a re-entrant `enif_*` call from `down` cannot deadlock;
4. drops the protective reference with `enif_release_resource(obj)` (`:3110`), which may synchronously run the `dtor`, the lock being released.
Returns whether the callback ran. `resource_monitors() -> Vec<(MonitorId, Eterm)>` snapshots the armed monitors sorted by `MonitorId`; the pid is reconstructed with `Eterm::from_raw` and is an **identity** (compare raw words), readable only while its heap is alive — the `resource_terms()` caveat.
A binding deliberately holds **no reference of its own**: holding one would keep a monitored resource alive past refcount zero, where the oracle destroys it and cancels the monitor without firing `down` (`run_resource_dtor`, `:2974-3030`); the protective inc-unless-zero is what keeps the block alive across the callback instead.

### The resource-term release seam (`resources_release.rs`)
`nif::enif::{release_resource_terms, resource_terms}` — the sound way for a caller that owns the reachability decision to tell this crate "that term is dead".
`resource_terms() -> Vec<Eterm>` snapshots the live term words (sorted, under the registry lock, which is released before returning); `release_resource_terms<F: Fn(Eterm) -> bool>(predicate) -> usize` snapshots, evaluates `predicate` **with no lock held**, then removes each unreachable binding under the lock and drops exactly one reference per removed binding outside it (through the very path `enif_release_resource` takes).
**`predicate(term) == false` is the caller's soundness assertion**: a false "unreachable" for a readable term is a use-after-free, a false "reachable" is only a leak.
Idempotent by term identity, so a repeated or concurrent prune releases a term exactly once (the binding removal is the structural guard), and a term whose object still holds a user reference is released once with no destructor run.
Callers compare `Eterm::raw()` words rather than inspecting the terms (a term word is only meaningful while its heap is alive).

### The resource-term minting seam (`resources_term.rs`)
`pub fn resource_term(host: &mut dyn NifHost, obj: *mut c_void) -> Option<Eterm>` mints the resource **term** that designates a bare `void*` handle.
Ground truth: the C's `prepare_select_msg` (`erl_check_io.c:699-767`) names a selected object by `erts_mk_magic_ref(&hp, &mp->hfrag.off_heap, &bin->binary)` (`:746`) inside the `{select, ResourceTerm, Ref, EventAtom}` tuple (`TUPLE4`, `:757`), and `enif_select`'s per-interest `enif_keep_resource(resource->data)` (`:1446`, `:1509`) is what keeps the object alive while the interest is armed.
Semantics, in order: a null `obj` ⇒ `None`; `with_registry(|r| r.object_type(obj))` ⇒ `None` for a pointer this crate never allocated (the "not one of ours" rule `enif_get_resource` uses — the helper **never** dereferences an unrecognised pointer); `RESOURCE_REF_WORDS` words off `host.heap()` ⇒ `None` on exhaustion; the block is written by the **one** writer `write_resource_ref(hp) -> Option<Eterm>` (which `mint_resource_ref`, the `enif_make_resource` route, also calls; `None` = an unrepresentable reference id, and it then writes **no** word); `retain_object_if_alive(obj)` ⇒ `false` means the count is already zero, i.e. the object is dying, so `None` — it **refuses to resurrect** (the C's `ASSERT(!erts_dbg_is_resource_dying(resource))`, `:1261`), the one deliberate difference from `enif_make_resource`'s unconditional retain; then `Registry::bind_term` + `release_evicted`, exactly as `enif_make_resource` does.
It is `pub fn`, not `unsafe fn` (`#[allow(clippy::not_unsafe_ptr_arg_deref)]` with the rationale inline), so the caller contract is documented rather than a `# Safety` section: the returned term is a full resource term (`enif_get_resource` resolves it to `obj`, and the registry's identity key makes a `copy_struct` copy resolve too), it holds **one reference** on `obj` which the caller must drop through the reachability prune `release_resource_terms` or a deliberate `enif_release_resource`, and a term never released is only a leak.
Like `enif_make_resource`, the mint's `id` is one draw from the **one** local reference-id space — `beam_common::next_ref_id()` via `next_resource_ref_id`/`narrow_ref_id` — shared with `erlang:make_ref/0`, the NIF `make_ref`, monitors, aliases, timers, `seq_trace` references and `beam_process`'s process table, so a resource term never aliases a live plain reference; the draw is narrowed to the block's small-integer id slot **checked** (the bound is `beam_term::MAX_SMALL`), and an unrepresentable id is a typed refusal (`None`), never a wrapped one.
Two calls on one object answer two distinct terms (fresh reference per call, as measured on the oracle).

### `select` — `enif_select`/`enif_select_x`
The C's argument handling and return codes, handed to `NifHost::select` (the poller is `beam_io`'s); `-1` when no host can service the request.
The **complete documented flag set** is modelled and forwarded **bit-identically**: `ERL_NIF_SELECT_READ` `1<<0`, `WRITE` `1<<1`, `STOP` `1<<2`, `CANCEL` `1<<3`, `CUSTOM_MSG` `1<<4`, `ERROR` `1<<5` (`erts/emulator/beam/erl_drv_nif.h:56-61`), so `ERROR`/`STOP`/`CANCEL` reach the host as such and a host decodes the mode with `ErlNifSelectFlags::intersects` (`src/common.rs`, the C's `mode & FLAG` at `erl_check_io.c:1272`, `:1292-1300`, `:1470-1482`) into its poller's `readable`/`writable`/`error` interests.
Two refusals are decided **before** the host is consulted, both answering `INT_MIN | ERL_NIF_SELECT_INVALID_EVENT`: an invalid (negative) event handle (`erl_check_io.c:1266`), and a mask carrying **no documented bit** (the C's `ASSERT(mode)`, `:1293` — a debug abort / release no-op-success in the C, refused here so the `retval < 0` idiom holds and no success is fabricated). A `CUSTOM_MSG`-only mask is documented (`:716-731`) and is forwarded, not refused, and `STOP`/`CANCEL` are forwarded too.
Return codes (all the C's own, passed through from the host, since the host owns the poller): `0` accepted or nothing-cancelled (`:1464`, `:1467`, `:1470`); `INT_MIN | ERL_NIF_SELECT_INVALID_EVENT` (`:1266`); `INT_MIN | ERL_NIF_SELECT_NOTSUP` (`:1305`, the C's ERROR-bit-on-an-error-blind-poller case); `INT_MIN | ERL_NIF_SELECT_FAILED` (`:1433`); `ERL_NIF_SELECT_STOP_CALLED`/`_SCHEDULED` (`:1283`, `:1335`, `:1502`, `:1512`); `ERL_NIF_SELECT_{READ,WRITE,ERROR}_CANCELLED` (`:1472`, `:1477`, `:1482`).
Every negative answer is `INT_MIN | bit`, so `retval < 0` is the failure idiom; this crate's own "not serviceable" answer stays `-1` (negative, so the same branch), pinned by `../../../tests/nif_env.rs`.
The `STOP` verdicts in the returned code drive the resource `stop` callback (see *The resource `stop` callback*).

### `envs` — message envs
`enif_alloc_env` reaches a host in **three documented steps**: (1) the current NIF call's host on this thread, (2) else the host a `ThreadEnvHostFactory` opened for this thread, (3) else `NULL` — never a crate-owned env, which could not intern into the runtime's atom table and would hand a NIF a *wrong* atom index; a current host whose `alloc_env` is unsupported falls through to step 2.
`enif_free_env` returns to the owning host through the env itself (`NULL` and foreign pointers are no-ops — this crate never frees memory it did not allocate); `enif_make_copy` is `NifHost::make_copy` (`THE_NON_VALUE` when unsupported); `enif_priv_data` is `NifHost::priv_data`, published by `NifLibrary::call`.
A use-after-`enif_free_env` is a caller-contract violation the raw-pointer ABI cannot detect.

## Constraints
- **Every slot keeps its frozen signature.** `enif_*` bodies are `#[no_mangle] pub unsafe extern "C"` matching `nif::api` field for field; the table is exactly **176** entries and `../../../tests/abi_conformance.rs` asserts set equality against the header.
- **A family's slots live in that family's file.** Each family owns a `pub(super) const fn populate` filling **only** its own slots; `mod.rs` chains the `populate`s innermost→outermost into the `#[used] static ENIF_API`. Extending a family edits that family's file and its `populate`, never `mod.rs`.
- **A host-extension seam is a `pub fn`, not an `enif_*` slot.** No `#[no_mangle]`, no new C symbol (`ErlNifApi` stays at 176); the caller contract goes in the function's docs rather than a `# Safety` section, with `#[allow(clippy::not_unsafe_ptr_arg_deref)]` and the rationale inline.
- **Never dereference a pointer this crate did not allocate.** A foreign/live-unknown/stale handle is a typed `false`/`None` refusal — the oracle SIGSEGVs on several of those shapes.
- **Nothing in this node reads a `va_list`** (Rust cannot): the seven C-variadic slots are driven by `csrc/nif_variadic.c` (see `../CONTEXT.md` → *The C-variadic shim*).
- **Term lifetime:** a term produced by a NIF lives on the **host arena** behind the call's `NifEnv`, readable only while that arena is alive; a snapshot of raw term words (`resource_terms()`) is an identity, not a readable value.
- Files target ~1000 lines, split by responsibility; a family's slots must stay in one table-building file, so the families that exceed it legitimately do (`resources.rs`, `mem.rs`, `variadic.rs`, `threads.rs`, `time.rs`, and the `*_tests.rs` modules, which are test-only).
- `unsafe` discipline is `../CONTEXT.md`'s and `../../../CONTEXT.md`'s: every `unsafe` block carries a `// SAFETY:` rationale.

## Test Strategy
- **In-module `#[cfg(test)]` unit tests** (no C fixture, no oracle):
  - `tests.rs` — the shared small checks; `process_tests.rs` — `both_reference_block_shapes_are_accepted_at_their_own_width`, `a_reference_shaped_word_of_another_arity_is_refused`, `a_c_layout_arm_is_established_and_stores_its_three_words`, the `enif_compare_monitors` ordering; `mapiter_tests.rs` — the iterator walk.
  - `resources_tests.rs` — `a_copied_resource_term_resolves_to_the_same_object`, `an_unminted_reference_identity_is_not_a_resource`, `a_resource_term_of_another_type_does_not_resolve`, plus the minting seam's `resource_term_refuses_a_handle_that_is_not_ours`, `resource_term_mints_a_resolving_term_that_holds_its_own_reference`, `two_mints_of_one_object_answer_two_distinct_terms`, and the reference-id source pins `two_consecutive_mints_draw_distinct_strictly_increasing_ids`, `a_minted_id_is_the_next_draw_of_the_shared_reference_id_space` (a mint id equals the next `beam_common::next_ref_id()` draw inside a proven interference-free window, so the id provably comes from the one shared local space) and `narrow_ref_id_accepts_exactly_the_blocks_id_slot` (the checked `u64 → i64 ≤ beam_term::MAX_SMALL` narrowing, cross-checked against `beam_term::try_make_small`).
  - `resources_keep_tests.rs` — `a_keep_holds_the_object_past_the_owners_release`, `a_null_handle_is_refused`, `an_unknown_pointer_is_refused_without_dereferencing_it`, `a_dying_object_is_not_resurrected`, `an_unkeep_after_destruction_answers_false_and_runs_no_second_destructor`, `a_keep_and_unkeep_round_trip_leaves_term_resolution_intact`.
  - `resources_monitors_tests.rs` — the `down` seam: arm → deliver → operands, the second-delivery refusal, demonitor codes, dtor/monitor cancellation, the sorted snapshot, callback re-entrancy, `two_monitors_minted_from_different_references_stay_distinct`.
  - `schedule_tests.rs` — `a_continuations_value_reaches_the_caller`, `a_nested_schedule_answers_the_non_value_and_the_outermost_yields_the_value`, `a_self_rescheduling_chain_completes_with_a_constant_stack`, `a_dirty_flagged_continuation_reports_a_dirty_thread_type`, `a_raise_inside_a_continuation_ends_the_chain`, `every_refusal_is_the_badarg_raise_in_the_cs_order`.
  - `stop_tests.rs` — `a_plain_stop_called_runs_the_callback_synchronously`, `a_stop_called_with_a_cancelled_read_is_still_synchronous`, `a_scheduled_stop_runs_from_the_drain_with_no_direct_call`, `no_other_verdict_calls_the_callback_or_parks_a_stop`, `a_second_stop_for_the_same_pair_steals_the_parked_entry`, `eight_objects_across_every_verdict_are_each_destroyed_exactly_once`.
  - Inline `mod tests` in `mem.rs`, `make.rs`, `map.rs`, `envs.rs`, `select.rs` (the flag→mode pin `every_documented_flag_is_forwarded_bit_identically`, `a_combined_mask_arrives_intact`, `intersects_is_the_cs_mode_and_flag_test`, the two ABI-side refusals `a_zero_mask_is_refused_before_the_host_is_consulted` / `an_undocumented_only_mask_is_refused`, `a_stop_and_a_cancel_request_still_reach_the_host`, `a_host_without_a_poller_answers_negatively_for_every_documented_flag`), `time.rs` (`monotonic_time_is_monotone_and_unit_consistent`, `monotonic_plus_offset_is_wall_time`), `threads.rs`.
- **Per-family end-to-end tests live one node up** (`../../../tests/nif_*.rs` + `../../../tests/fixtures/*.c`): a real C fixture compiled at test time with the system `cc` against the system OTP headers, `dlopen`ed (`RTLD_NOW`) through this crate's own loader, so a single unresolved `enif_*` import fails the load. The recipe and the loud-`SKIP` rule are `../CONTEXT.md` → *Test Strategy*.
- **Measured oracle answers the seams must reproduce** (`../../../tests/nif_bin2term.rs`, `nif_monitor_codes.rs`, `nif_resource_release.rs` pair a Rust-side assertion with an oracle half over the **same** `.so`):
  - `enif_binary_to_term`: `{3,42}` for `t2b(42)` alone; `{3,42}` for `t2b(42) ++ "junk"` (7-byte image — trailing bytes tolerated and not counted); `0` for a truncated `t2b(42)`; `3` for a truncated `"junk"` image's first byte; `0` for bad `opts`.
  - `enif_monitor_process`: `{0,0}` live, `{1,1}` `undefined`, `{-1,1}` no-`down` type (with and without a pid — `-1` precedes the pid check), `0` null holder, `{0,1}` demonitor after the target died, `1` demonitor on a zeroed monitor.
  - Resource destructor counts `{0,1,1,2,3}` over the four-scenario transcript (only counts are comparable — the oracle needs a `garbage_collect/0` + settle step that `release_resource_terms` replaces with an explicit reachability predicate).
- **`fixtures/nif_resource_down.c`** is `include_str!`d and compiled at test time for the `down` seam's oracle cross-check; no `.so` is loaded into the Rust side, because `build.rs`'s `--export-dynamic` covers `../../../tests/` targets only.
- Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_abi`; behavioural cross-checking against real ERTS is `erts_rust/tools/differential`'s job (read-only).

## Known gaps (family-local; the node-level A.3 residuals are `../CONTEXT.md`'s)
- **`mem`: on-heap vs off-heap refc binaries (F5-class, audited — not closable inside `beam_abi`).** `enif_alloc_binary`/`enif_make_new_binary`/`enif_make_binary` always build an **on-heap** binary (`ref_bin == NULL`, `orig_size`/`orig_bytes` only), where the oracle keeps the payload off-heap with an `ERL_REFC_BITS_SIZE` 8-word refc pair (`ERL_REFC_BITS_SIZE == 8`, `copy.c:219-242`).
  Measured against OTP 29.1.1 / erts-17.1: **agree** for `size ≤ ERL_ONHEAP_BINARY_LIMIT` (64 B) — 2/3/4/10 words for 0/1/10/64 B — and differ above (65 B: oracle 8 words / `flat_size` 64 vs **11 words / 88** here; the oracle's `b.ref_bin` is non-NULL at every `enif_alloc_binary` size). Both are valid binaries of the same size and bytes; only the representation — and with it `enif_term_size`/`erts_debug:flat_size` — differs.
  Not closable here (`beam_alloc::RefcBinary`, `BinRef.val`, `beam_process`): closure touches `../host.rs`, `mem.rs`, `beam_rt`'s `impl NifHost for NifHostImpl` (`beam_rt/src/nif.rs`, `beam_rt/src/context/mod.rs:1677-1694`) and `beam_gc::OffHeap`; adjacent, `enif_release_binary` differs from the oracle only under `#ifdef DEBUG` (`erl_nif.c:1451-1461`). Pinned by `mem.rs`'s unit tests and `../../../tests/nif_bitslice.rs`/`nif_subbin.rs`.
- **`pred`: a `map`/`compare`/`is_identical`/`hash` call takes no env.** Identical terms take a raw-word fast path. `enif_compare` resolves an atom pair by **name** — a *top-level* pair through `current_recorded_atom_name`/`NifHost::atom_name`, and a pair **nested** (tuple/list/map) inside a compound through the current host's atom table (`NifHost::atom_table`, a defaulted hook). Only with no host, or a host that exposes no table, does a nested pair fall back to `beam_term`'s raw atom-index order — pinned by `../../../tests/nif_compare.rs::a_nested_atom_pair_orders_by_name`. Exact for non-atom keys.
- **`get`/`make`: integers are small ints + one-limb bignums;** a wider value makes the accessor return `0`, never truncate.
- **`ioq`: `enif_ioq_is_empty` does not exist in this OTP tree.**- **`resources`: a *copied* `enif_make_resource_binary` term does not resolve.** That term is an **on-heap** binary here (the C's is an off-heap magic binary, so a copy shares its `mb` pointer and keeps resolving); an on-heap binary carries no shared identity, so the registry keys it by raw word and `copy_struct`'s copy of it is a different term. A typed residual (`enif_get_resource` answers the C's `0`), never a wrong object.
- **`select`/`stop`: `STOP_SCHEDULED` is reachable only for a host that really defers the stop** — see the reference-accounting note above; `beam_rt` answers `STOP_CALLED`.
## Routing Table
- `mod.rs` → module docs, the shared helpers, the submodule wiring, the glob-import allow, the `ENIF_API` `populate` chain and `enif_api_table()`
- `tests.rs` → the shared `TestHost` scaffolding (a `NifHost` over a `beam_alloc::Heap` + `AtomTable`), `#[path]`-included as a child of `enif` so `use super::*` reaches every re-exported `enif_*` function
- `mem.rs` → allocation, binaries, ETF (`*_tests` inline `mod tests`)
- `get.rs` → the `enif_get_*` accessors
- `make.rs` → the `enif_make_*` constructors (maps excepted) + the per-call atom record use (inline `mod tests`)
- `map.rs` → the map family, incl. the all-pairs duplicate-key refusal (inline `mod tests`)
- `pred.rs` → predicates, `enif_compare`/`is_identical`/`hash`, `term_type`/`term_size`
- `proc.rs` → self/pid/port handles and the atom-cache queries
- `process.rs` → liveness, monitors, send, port command, whereis, `enif_schedule_nif`; `process_tests.rs` is its test child
- `exception.rs` → badarg/raise/pending/clear
- `threads.rs` → the mutex/cond/rwlock/tsd/thread facades over `crate::sync` (inline `mod tests`)
- `ioq.rs` → the I/O queue and I/O vector
- `mapiter.rs` → `enif_map_iterator_*` over the frozen 56-byte iterator (storage order); `mapiter_tests.rs` is its test child
- `time.rs` → time/`convert_time_unit`/`now`/`cpu`/`system_info`/`getenv`/`unique_integer`/`consume_timeslice`/`dl*` (inline `mod tests`)
- `resources.rs` → the type registry, the refcounted allocation, `term_key` identity keying, the monitor tree and `Registry::forget_object`; `#[path]`-includes and re-exports the five seams below; `resources_tests.rs` is its test child
- `resources_keep.rs` → the explicit keep/unkeep seam (`keep_resource`/`unkeep_resource`, guarded retain/decrement); `resources_keep_tests.rs` is its test child
- `resources_stop.rs` → the resource `stop` callback helpers (`resource_type_has_stop`, `call_resource_stop`); never dereferences a pointer this crate did not allocate
- `stop.rs` → the pending-stop table keyed by `(obj, event)`, one `keep_resource` reference per entry, steal-not-double-arm; `stop_tests.rs` is its test child
- `resources_monitors.rs` → the `down`-delivery seam: `MonitorId`, `record_monitor`/`take_monitor`/`forget_monitors_for_object` (the crate-internal hooks `process.rs` and `Registry::forget_object` call), `deliver_resource_down`/`deliver_resource_down_id`/`resource_monitors`; `resources_monitors_tests.rs` is its test child
- `resources_release.rs` → the release seam: `resource_terms()` (snapshot) + `release_resource_terms(predicate)` (drop the reference of every term the caller calls unreachable), with the lock discipline and idempotence
- `resources_term.rs` → the minting seam: `resource_term(host, obj)` mints the internal-reference term that designates a bare `void*` handle, refusing a null/foreign handle and a dying object; the caller contract and the reference-id-space rule
- `schedule.rs` → the `enif_schedule_nif` continuation trampoline (`Outcome`, `Continuation`, `run`, the dirty thread-type marker, `in_continuation`), constant Rust stack; `schedule_tests.rs` is its test child
- `select.rs` → `enif_select`/`enif_select_x`: the flag→mode mapping, the two pre-host refusals, the pass-through return codes (inline `mod tests`)
- `envs.rs` → `enif_alloc_env`/`free_env`/`make_copy`/`priv_data` and the three-step reachability order (inline `mod tests`)
- `variadic.rs` → the seven C-variadic slots over the C shim
- `fixtures/nif_resource_down.c` → the real-C `down` fixture `resources_monitors_tests.rs` compiles at test time for the oracle half

## See Also
- `../CONTEXT.md` → the node root: the aliasing contract, the host seam (`NifHost`/`NifEnv`), the thread-env factory, the `enif_*` table invariants, the loader, the C-variadic shim, the `erlang:load_nif/2` contract, the A.3 residuals, the test recipe.
- `../../../CONTEXT.md` → the crate root: the frozen layout table, the ABI version constants, the `ErlNifApi` table/`ERL_NIF_API_FIELD_COUNT`, dependencies, `unsafe` discipline.
- `../../../tests/` → the crate-level NIF integration tests and their real-C fixtures (sibling — read-only, escalate writes to `..`).
- `../../../src/driver/CONTEXT.md` → the sibling driver host layer (A.4).
