# Handoff — `erlang:process_info/2`'s remaining items (`beam_rt` / `beam_process` / `beam_gc`)

Ground truth for the `process_info` items the runtime floor still answers with a typed
`unsupported` (`beam_rt: unsupported: process_info: <item>`, exit 3).
Measured on OTP 29.1.1 / erts-17.1 (the `erl` on `$PATH`) against the C in
`../../../erts/emulator/beam/erl_bif_info.c`.

Nothing here belongs to `beam_bifs`: `erlang:process_info/1,2` is served by the runtime-native
floor (`beam_rt/src/native/memory/`) **before** the registry, so the `ProcessHook` seam is not on
this path. The file records the derived semantics so the owning nodes do not have to re-probe.

## Item-by-item disposition

### `last_calls` — implementable, needs a saved-calls buffer on the PCB

C `erl_bif_info.c:2184-2238` (`case ERTS_PI_IX_LAST_CALLS:`), the writer
`erl_trace.c:1409-1428` (`void save_calls(Process *p, const Export *e)`), the switch
`bif.c:1902-1915` (`process_flag_aux`'s `am_save_calls` arm).

| state | answer |
|---|---|
| no buffer (`process_flag(save_calls, 0)`, or never set) | the atom **`false`** |
| buffer present, nothing recorded | the empty list `[]` |
| buffer present | a list, **newest first**: `j = cur - i - 1; if (j < 0) j += len` |

Each element is `{Module, Function, Arity}` (3-tuple, small-int arity) for a real call, or one of
the bare atoms **`send`**, **`receive`**, **`timeout`** for the three special events
(`exp_send` / `exp_receive` / `exp_timeout`). Recording is skipped for a
`ERTS_IS_PROC_SENSITIVE` process — `beam_process::Process` already models `sensitive`.
`save_calls = N` with `N` in `1..=10000` allocates a ring of length `N`; `N = 0` stores `NULL`;
`N` out of range or a non-small value is `badarg`.

Measured oracle transcript (module-run, `process_flag(save_calls, 20)`, then a few calls, a send
and a receive) — the recorded set is the consumer's business; the shape and ordering are the
contract:

```
after_flag  = {last_calls,[]}
after_calls = {last_calls,[{io,format,2},{erlang,group_leader,0},{erlang,make_ref,0},
                           {net_kernel,dflag_unicode_io,1},{erlang,monitor,2},
                           {erlang,send,2},send,'receive',{erlang,demonitor,2},
                           {lists,reverse,1},{lists,reverse,2},{erlang,send,2},
                           send,'receive',{lists,foldl,3},{erlang,process_info,2}]}
len = 20
```

Note what is **not** recorded: a local call, and an auto-imported BIF (`length/1`). Only what goes
through an export entry (`call_ext*`) is, which is why the recording site is the runtime's external
call dispatch, not the engine's instruction loop.

Ownership: the buffer + the recording API are `beam_process` PCB additions (owned `String` names,
never bare `Eterm`s — a stored `Eterm` in the PCB is the documented stale-word hazard); the
recording call sites are `beam_rt`'s external-call dispatch and its mailbox receive/timeout paths.

### `parent` — implementable, needs a PCB field

C `erl_bif_info.c:2257-2271`: reads `rp->parent`, whose `ASSERT`s pin it to an internal pid or
`am_undefined`, so the item is `{parent, Pid}` or `{parent, undefined}`. The field is set by the
spawn paths in `erl_process.c` (grep `->parent =`).

Oracle: `process_info(P, parent)` of a `spawn(Fun)` child is `{parent, <the spawning pid>}`; the
`erl -eval` root process answers `{parent, <0.0.0>}` (its parent is `init`).

Ownership: `beam_process` PCB field + accessors; `beam_rt` records it at its spawn entry points and
renders the item.

### `label` — implementable, needs NO PCB change

C `erl_bif_info.c:2305-2315`: `res = erts_pd_hash_get(rp, am_DollarProcessLabel)` — i.e. the item
is a plain **process-dictionary lookup of the key `'$process_label'`**, answering `undefined` when
absent. `proc_lib:set_label/1` is what writes it.

Oracle: `{label,undefined}` by default; `{label,my_label}` after `proc_lib:set_label(my_label)`.

Ownership: `beam_rt` only — `beam_process::Process` already holds the dictionary (`dict: Dict`), so
the item is a read of the target process's dict. No new seam, no new field.

### `priority_messages` — implementable, needs a PCB flag

C `erl_bif_info.c:2321-2322`: `res = rp->sig_qs.flags & FS_PRIO_MQ ? am_true : am_false;` — a
**boolean**, not a count. `FS_PRIO_MQ` is declared in `erl_proc_sig_queue.h`; its set/clear sites
are in `erl_proc_sig_queue.{h,c}` and pin exactly when the answer is `true`.

Oracle: `false` for an idle spawned process, for a freshly spawned one, and after a plain
`erlang:send/2` message — so the honest value for every process this runtime can currently build is
`false`, and modelling the flag (rather than hard-coding `false` in the reader) is what keeps the
item correct once a signal path can drive it.

Ownership: `beam_process` flag + accessor/setter; `beam_rt` renders the item.

### `magic_ref` — implementable with NO new data

C `erl_bif_info.c:2275-2283` → `bld_magic_ref_bin_list` (`erl_bif_info.c:207-230`): a list of
`{Ptr, OrigSize, Refc}` 3-tuples over the process's off-heap **ref-thing** headers (`ErtsMRefThing`,
i.e. magic binaries / NIF resources), `[]` when the off-heap list holds none.

Oracle: `{magic_ref,[]}` for a plain spawned process.

This mirrors the `binary` item the floor already serves (same shape, over the refc-binary subset);
it is the ref-thing subset, which is empty until something binds a resource to a process.

### `garbage_collection_info` — implementable, needs `beam_gc` heap detail

C `erl_bif_info.c:2097-2106` → `erts_process_gc_info(rp, &sz, &hp, 0, 0)`. The oracle's answer is an
**11-key proplist in this exact order**:

```
old_heap_block_size, heap_block_size, mbuf_size, recent_size, stack_size,
old_heap_size, heap_size, bin_vheap_size, bin_vheap_block_size,
bin_old_vheap_size, bin_old_vheap_block_size
```

Measured for an idle spawned process:

```
[{old_heap_block_size,0},{heap_block_size,233},{mbuf_size,0},{recent_size,0},
 {stack_size,1},{old_heap_size,0},{heap_size,2},{bin_vheap_size,0},
 {bin_vheap_block_size,46422},{bin_old_vheap_size,0},{bin_old_vheap_block_size,46422}]
```

Several fields are already readable here (the heap/stack capacities and the live `heap_size`);
`mbuf_size`/`recent_size` are `0` for a parked process; the four `bin_*_vheap_*` fields need block
sizes from `beam_gc::BinVheap`. Ownership: `beam_gc` (`HeapInfo`/`BinVheap` accessors), `beam_rt`
(rendering).

### `backtrace` — genuinely out of scope, keep the typed gap

C `erl_bif_info.c:2173-2183`: `erts_stack_dump` into a temporary buffer, returned as a **binary**.
Oracle shape (a spawned idle process): a 150-byte binary starting

```
Program counter: 0x00007395db65b1d4 (piprobe:'-go/0-fun-0-'/
```

It is a raw emulator stack/instruction-pointer dump. Reproducing it needs an `erts_stack_dump`
equivalent over this engine's stack frames and a byte-identical textual format — a stack-dumper
workstream, not an accessor. It stays a typed gap, and the recorded reason must name
`erts_stack_dump` rather than any vaguer phrasing.

### The position items for a non-`self()` process — the ledger's actual `unsupported: process_info` cases

`catchlevel`, `initial_call`, `current_function` and `current_stacktrace` are answered for the
**executing** process (`self()`, read from its live machine) but raise the typed gap for a
**spawned** pid. The module doc of `beam_rt/src/native/memory/mod.rs` calls them "conditionally
answerable": they are built by `RuntimeContext::position_value` / `catchlevel_value` from the
process's own entry module and engine, reached through the `engines` view of the runner state. A
spawned-and-parked process therefore needs its machine (or its entry module + engine) retained so
the same readers work for it — that is the fix, not a new data source.

## Not this file's business

`beam_bifs`'s own registry reaches `process_info` only through the `ProcessHook` seam
(`process_info_keys` / `process_info_item` → `ProcessInfoItem::{Value, Dead, Unknown}`), and
`ProcessInfoItem::Value` already carries an arbitrary `Eterm`, so none of the above needs a
`beam_bifs` API change.
