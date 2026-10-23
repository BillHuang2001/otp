# Handoff — differential-corpus updates owed by the corpus owner

This node (`erts_rust/crates`) cannot write `erts_rust/tools/differential/**` (sibling, read-only),
so the corpus work that the parity batch implies is collected here for the corpus owner to apply.
Every figure below was **measured on OTP 29.1.1 / erts-17.1** (the system `erl`) and reproduced on
`beam_rt`; the full gate at that state is `total=3439 agree=3438 mismatch=0 unsupported=1 skipped=0`
with the ratchet clean (`allowed=1 unauthorized=0 stale=0`).

Owner: `erts_rust/tools/differential`. Everything here is additive or a deletion of text that is now
false — no allowlist entry may be added, and `known-gaps.txt` stays at exactly one line
(`wire/new_fun_ext_encode`).

## 1. New cases the corpus can express

Case ids follow the existing `<group>/<snake_case_name>` convention. Goldens are the oracle's; where
a figure is interior-dependent the case must pin the *relation*, never the figure.

### A — `process_info/2` (contract A.7/A.8, `corpus/bif_extra.rs`)

| id | shape | measured oracle answer |
|---|---|---|
| `bif_extra/process_info_parent_is_init_and_never_self` | `Eval` | for the `-eval` root, `{parent,<0.0.0>}` and `element(2, process_info(self(), parent)) =:= self()` is `false`; for a spawned child, the item names the spawner pid. Classify the pid into an integer and pin the two relations (`parent_is_self` ⇒ `false`, `parent_is_spawner` ⇒ `true`), exactly as the existing pid-family cases do. |
| `bif_extra/process_info_label_follows_the_process_dictionary` | `Eval` | `{label,undefined}` unset; `{label,my_label}` after `put('$process_label', my_label)`. |
| `bif_extra/process_info_priority_messages_is_a_boolean` | `Eval` | `false` (a boolean, not an integer) for a plain process. |
| `bif_extra/process_info_magic_ref_is_empty` | `Eval` | `[]`. |
| `bif_extra/process_info_garbage_collection_info_key_order` | `Eval` | an **11-pair** proplist in exactly this order: `old_heap_block_size, heap_block_size, mbuf_size, recent_size, stack_size, old_heap_size, heap_size, bin_vheap_size, bin_vheap_block_size, bin_old_vheap_size, bin_old_vheap_block_size`. Pin the key list and that every value is an integer — **never the values** (they are per-VM figures). |
| `bif_extra/process_info_last_calls_three_states` | `Eval` | unarmed ⇒ `{last_calls,false}`; armed-empty (`process_flag(save_calls, 10)` then query) ⇒ `{last_calls,[]}`; and the `process_flag(save_calls, 0)` disarm. Pin the three states and the `save_calls` return-previous-size / `badarg`-for-`10001`/`-1` behaviour. Do **not** pin a recorded call list: the ring is a documented **superset** of the oracle's (the oracle's peephole drops some calls), so a pin would be brittle. |
| `bif_extra/process_info_last_calls_records_the_send_pseudo_entry` | `Module` | an armed idle child queried from its parent measured `[{erlang,send,2},send]`. To keep it deterministic, pin the *presence* relations rather than the whole list: the bare atom `send` appears, the head item is the querying `{erlang,process_info,2}` call, and entries are `{M,F,A}` triples or one of `send`/`'receive'`/`timeout`. |

### B — `ets:info(T, stats)` (contract A.7, `corpus/ets.rs`)

| id | shape | measured oracle answer |
|---|---|---|
| `ets/info_stats_shape_and_the_two_agreeing_figures` | `Eval` | `{Nactive, AvgChainLen, StdDevChainLen, StdDevExpected, MinChainLen, MaxChainLen, KeptItems, NLocks}`. For 5 objects on a plain `set`: `{256,0.01953125,0.13838273286819458,0.13948102295398712,0,1,0,1}` — but the three chain figures describe ERTS's bucket-chained hash and must **not** be pinned: our `set` is CHAMP. Pin `{is_tuple(X), tuple_size(X), element(7,X), element(8,X)}` ⇒ `{true,8,KeptItems,NLocks}` (`{true,8,0,1}` for that fixture), i.e. shape/arity plus the two figures that agree by value. |
| `ets/info_stats_kept_items_counts_objects_missed_by_a_fixed_table` | `Eval` | on a `{debug_hash_fixed_number_of_locks,1024}` table with 1 of 5 deleted, `kept_items` is `1` (and `nlocks` 1024; `nactive` 2304). `nlocks`/`kept_items` agree by value in our implementation; pin those two only. |
| `ets/info_stats_a_plain_ordered_set_is_false` | `Eval` | the bare atom `false`, at every size, for an `ordered_set` without write concurrency. With effective write concurrency (the catree mode) the answer is a 3-tuple whose oracle figures are `{0,1,0}`; pin the *3-tuple shape* rather than the figures. |

`nlocks` normalisation is oracle-measured (`erl_db_hash.c:1028-1058`): `off` ⇒ 1; `{write_concurrency,true|auto}` ⇒ 64 (the `NLOCKS_WITH_ITEM_COUNTERS` clamp-up); `4`/`100` ⇒ 64; `128` ⇒ 128; `1000` ⇒ 512; `32768` ⇒ 32768.

### C — write-shutdown answers (contract A.4, `corpus/inet_write.rs`)

| id | measured oracle answer |
|---|---|
| `inet_write/zero_length_writes_after_write_shutdown_stay_ok` | after `gen_tcp:shutdown(Cl, write)`, **nine** zero-length sends spaced ~40 ms apart all answer `ok` (`[ok,ok,ok,ok,ok,ok,ok,ok,ok]`). Repeat them with a gap — without the gap the answer is trivially correct and the case proves nothing. |
| `inet_write/second_nonempty_write_after_write_shutdown_is_enotconn` | the first non-empty send after the half-close ⇒ `{error,closed}`; every later send, non-empty **or** zero-length ⇒ `{error,enotconn}`. |
| `inet_write/recv_after_a_write_latched_close` | the *first* `gen_tcp:recv` after a write-latched close ⇒ `{error,closed}`; later ones ⇒ `{error,enotconn}`. |

`shutdown(Cl, read_write)` is not worth pinning — it is genuinely racy on the oracle.

### D — calling a decoded closure (contract A.2, `corpus/wire.rs`)

| id | measured oracle answer |
|---|---|
| `wire/decoded_new_fun_ext_closure_is_callable` | `Module` case: `binary_to_term(term_to_binary(F))` for a real local fun of a compiled module, applied ⇒ the normal result (e.g. `42`, and a capturing `X*A+B` closure ⇒ its result). Both engine tiers must agree. |
| `wire/decoded_new_fun_ext_bad_index_raises_badfun` | an image whose `index` names no lambda, or whose 16-byte module checksum is corrupted, still decodes, and calling it raises `error:{badfun, F}`. |
| `wire/decoded_new_fun_ext_unloadable_module_raises_undef` | an image naming a module atom that is not loadable raises `error:undef`, whose **innermost stacktrace item** is `{Fun, Args, []}` (the applied fun, never a real callee frame such as `lists:map/2`) — the reason itself is the bare atom `undef`. |

### E — hashing a `make_fun3` local fun (contract A.2/A.7, `corpus/phash2.rs`, `corpus/phash.rs`)

`phash2/local_fun_make_fun3` and `phash/legacy_local_fun`: a local fun's `phash2/1`, `phash2/2`,
`phash/1` and `phash/2`. The `uniq` rides the module CRC, so the value must match the oracle for the
**same `.beam`** — pin it as a relation/mask over `{identity, stability, range, distinct-fun}`, never
as a bare integer, and never derive it from a source path. The imported (`fun M:F/A`) form is already
pinned by `phash2/local_fun_via_its_imported_image`.

### F — `ets:slot/2` (contract A.7, `corpus/ets.rs`)

`ets/slot_domain_is_a_fixed_slot_space_not_the_object_count`. The oracle's domain is **0-based** over
its interior's slot count: on a small `set` (one object) `slot(T, 0..255)` ⇒ `[]`, `slot(T, 256)` ⇒
`'$end_of_table'`, `slot(T, 257)` ⇒ `badarg`, and `-1`/`1.0`/`foo`/a bignum ⇒ `badarg`; an
`ordered_set` instead answers one slot per object with `N == size`. Our interiors are CHAMP (32 root
slots) / chained buckets / K-ary B+-tree, so **the slot count and the contents are ours**: pin only
list-ness, the single `'$end_of_table'` terminator boundary, the `badarg` shape, and that the slots
partition `tab2list/1`. Never pin ERTS's 256.

## 2. Corpus prose that is now false and must be deleted or corrected

`bif_extra.rs`
- `:49-60` — the section "Why five `process_info` items are absent from this group" claims `parent`, `label`, `priority_messages`, `magic_ref`, `garbage_collection_info` are typed capability gaps that exit 3. They are served; `backtrace` is the only remaining `UNSUPPORTED_ITEMS` entry. Retitle to "Why `backtrace` is the only absent item".
- `:56-60` — "`last_calls` *is* answered (always `false`) … a process with an *armed* `save_calls` buffer … is a runtime-side gap too." All three states are served, plus the ring.
- `:714-717` — the trailing "(an armed buffer's list of recorded calls is a runtime-side gap)" on `process_info_self_last_calls_without_a_buffer`.

`inet_write.rs`
- `:153-161` — the "second non-empty `gen_tcp:send/2` … blocks … exit 3 … typed capability gap" bullet: measured identical on both sides, exit 0.
- `:162-168` — the "second zero-length send is a plain divergence" bullet: measured identical.
- `:99-102` — "Exactly one zero-length send is pinned — a **second** one is a measured divergence."
- `:169-174` — "a `gen_tcp:send/2` after `shutdown(Cl, read_write)` is racy … 2 of 3 runs `einval`": 5/5 runs identical.
- `:183-186` — "the *recv*-side `inet:getstat/1` counters are racy … so no row pins them": 3/3 identical.
- `:187-195` — the bare-integer payload half is stale (both sides write it); keep the `[256]`/improper-list `einval` half.
- Not stale: `:175-182` (a passive read of already-queued data on an *accepted* socket) still diverges.

`ets.rs`
- `:70-81` — the heading "`ets:info(T, stats)` is a capability gap": it is served. Replace with the shape/`nlocks`/`kept_items` guidance in §1B.
- `:64-68` — "`slot/2`, `select_reverse/1,2,3`, `is_compiled_ms/1`, `internal_delete_all/2` … raise a plain `error:undef`": all four are served.
- `:88-90` — "`named_table` lookup by name … `badarg` in the Rust runtime".
- `:91-92` — "`ets:info/1,2` on a deleted table … `badarg` in the Rust runtime": both answer `undefined`.
- `:93-94` — "`ets:info(T, owner)` — the oracle returns a pid, the Rust runtime a non-pid": both answer a pid.
- `:43` — a formatting defect where two doc lines were joined (`… stay relational; and//! * **ownership transfer**`).

`wire.rs`
- `:85-90` — "Calling a fun that was decoded from its own `NEW_FUN_EXT` image … still raises a plain `badarg` … a real divergence": it is callable, with three documented raise classes.
- `:74-77` — "the live handshake / node-to-node loopback and the `DOP_*` framing are **not** expressible": they live in `corpus/dist.rs` and `corpus/dist_extra.rs` (`CaseKind::Command` + `PeerRun`).
- `:99-103` — "The `[deterministic]` option is byte-exact only where no re-sorting is needed": `DFLAG_DETERMINISTIC` is served.
- `:104-107` — "only the `byte_size/1` shape would be pinnable, and the image is deliberately left unpinned": it is pinned in `corpus/wire_ids.rs:183-205`.
- Keep `:81-84` — the `-eval`/`erl_eval` lambda **encode** gap is real and is the sole allowlisted entry.

Adjacent, same node: `divergences.md:491-498` (mirrored into the generated `GAPS.md:504-509`) repeats
the `ets:info(T,stats)` gap claim, and `divergences.md:638-641` ("Calling a decoded `NEW_FUN_EXT` fun
is broken") is false and should be deleted.

## 3. Known coverage holes the sweep surfaced

- `ets:slot/2` had no case at all, which is why a wrong value (`'$end_of_table'` in place of `[]`, and
  a 1-index-only domain) survived until it was found by hand. §1F closes it.
- The `last_calls` ring is a superset of the oracle's (the oracle's byte-code peephole drops some
  calls); a case must pin presence relations, not the list.
- `process_info/2`'s `backtrace` is the one remaining typed gap and is not expressible as an agreeing
  case.
