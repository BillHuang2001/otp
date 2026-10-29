# Draft corpus cases — gap recipes, promotions, and corrections

**Recipes, promotions, corrections.** Each numbered section carries a one-line **Status:** and
either a ready-to-add `Case` literal for the conformance corpus (`src/corpus/*.rs`) or a
correction saying why no case is possible. A recipe whose gap the owning crate has already closed
lands in `src/corpus/**`, and `known-gaps.txt`/`GAPS.md` are refreshed in the same change (see
`../CONTEXT.md` §"Notes for Agents"). A recipe for a gap that is still **open but closable** stays
drafted here and deliberately **not** in `known-gaps.txt` — the allowlist records only genuinely
unclosable gaps, and recording a closable one would defeat the ratchet. §8 and §10 are the two
recipes open at this revision (both closable, each blocked on a named runtime seam) and §9 records
a measured divergence, so no case can pin it; every other section is either landed or a recorded
non-gap.

At a glance: **§1**, **§2**, **§4b**, **§4c**, **§5a**, **§5b** and **§6** are **landed** — every
one agrees in `beam_rt` and is pinned by the `src/corpus/**` groups (`zlib.rs`, `bifs.rs`,
`system_info.rs`, `flags.rs`, `flags_apply.rs`); **§3** and **§7** record items that were named as
gaps but are not gaps at all. The only remaining zlib divergences are **§4a** (the repetitive-body
wrapped size form) and **§4**'s large-body image-choice (the 100-byte raw-DEFLATE `zip/1` stream),
both unpinnable and recorded in `divergences.md`, not drafts. §8 is `blocked-on-beam_rt` — the
`system_info(snifs)` recipe is ready and lands the moment the key is served — §9 is a
`maps` no-op-identity **divergence**, not a case: no golden can pin it while the result-admission
path deep-copies every BIF result, and §10 is `blocked-on-beam_rt`'s host impl of
`ExecutionContext::bif_error_cause` — the `{cause,_}` error_info recipe is ready and lands the
moment a host answers the seam.

Three coverage facts worth knowing: the synthesized callee-head `undef` stacktrace
head item `{Module,Function,ArgList,Location}` is pinned on **both** routes — the `-eval` form
(`errors/undef_head_item_arg_list_from_apply`, `errors/undef_head_item_shape_from_apply`,
`errors/undef_head_item_arg_list_from_call_ext`) and compiled code through the three-argument
`try C:R:St ->` form (`modules/undef_head_item_shape_in_compiled_code`,
`modules/undef_head_item_identity_in_compiled_code`); `erlang:fun_info/1` is pinned for **external**
funs (the five-key proplist, `modules/fun_info_external_fun_created_in_compiled_code`, plus the
`fun_info/external_fun_info_{length,keys}` A.8 cases) **and** for a **compiled local** closure (all
ten items, `bif_extra/fun_info_local_*` — while an `erl_eval` lambda is the recorded unpinnable
shape in `divergences.md` §A.8); and the `-boot start` route carries four independent boot-completion
observations (`boot/kernel_processes_registered_under_boot_flag` plus the three
`BOOT_COMPLETION_CASES` in `src/corpus/boot_prereq.rs`:
`boot/init_get_status_boot_progress_is_started_under_boot_flag`,
`boot/which_applications_includes_kernel_and_stdlib_under_boot_flag`,
`boot/kernel_sup_children_are_live_under_boot_flag`).

This file is a hand-maintained companion to the generated `GAPS.md` and the hand-written
`divergences.md`: `divergences.md` records divergences that are **unpinnable**, this file records
things that are **pinnable once a named crate is fixed**.

## How a recipe was verified, and how to re-verify one

Build the driver once (release; the cache is shared across worktrees):

```sh
erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --release -p beam_rt -p differential
```

Then, for an expression recipe:

```sh
erts_rust/target/release/differential compare '<expr>'      # [AGREE] / [MISMATCH] / [UNSUPPORTED]
```

For a recipe that needs extra argv, run both runtimes by hand with the same argv, remembering that
`beam_rt` is a drop-in `erl` (contract A.6):

```sh
erl     -noshell <extra…> -eval 'io:format("~w~n",[EXPR]),halt().'
beam_rt -noshell <extra…> -eval 'io:format("~w~n",[EXPR]),halt().'
```

Every observable below was produced that way against the live oracle (system `erl`, OTP 29.1.1 /
erts-17.1) and the release `beam_rt` it was written against, not guessed. **Re-check the `beam_rt`
side after any owning-crate change** — a recipe's Rust observation can go stale (§4). A
`CaseKind::Eval`/`EvalEnv` recipe uses the shared argv `["-noshell", <extra…>, "-eval", <probe>]`;
`Probe::Value` prints the oracle's `~w` rendering as the single golden line (no trailing newline),
exit `0`.

## How an open recipe classifies

* A runtime that returns a **differing observable** (a plain Erlang value or a plain reason such as
  `exception error undef`) is a `mismatch` — the gate's hard failure. §1 was this shape before it
  landed.
* A runtime that signals the gap through its documented convention — exit `3` with
  `beam_rt: unsupported: …`, or a probe-caught `exception … unsupported: …` — is an `unsupported`
  capability gap. The four `process_info/2` position items in §5b were this shape before they
  landed. A *closable* gap drafted here is deliberately not allowlisted, so it can never be
  recorded as an allowed gap.

---

## 1. `timer` — **landed**; owner `beam_bifs`

**Status:** `landed` — no longer a gap: `timer:tc/1` and `timer:apply_after/4` now **agree** in
`beam_rt` and are pinned as `bifs/timer_tc_returns_the_fun_value` and
`bifs/timer_apply_after_first_element_is_ok` (the root cause was the absent
`erlang:convert_time_unit/3`).

`timer` **loads and runs** in `beam_rt` already (`timer:now_diff/2`, `timer:sleep/1`,
`timer:seconds/1` … all agree; `code:which(timer)` resolves to the stdlib `timer.beam`). The gap was
**not** the `timer` module surface but a single absent BIF — **`erlang:convert_time_unit/3`**
(`timer:tc/1,2,3,4` and `timer:apply_after/4` all failed on it). It is now implemented, so the
landed recipes below agree:

```sh
erts_rust/target/release/differential compare 'erlang:convert_time_unit(1000, native, microsecond)'
# before the fix: oracle: ok 1000   rust: exception error undef   -> MISMATCH
# now: oracle and rust both ok 1000   -> AGREE
```

`convert_time_unit/3` is injected via `erts_export_put` (`erts/emulator/beam/bif.c:5608`), so it is
**not** a `bif.tab` entry and is therefore outside the 556-entry roster `beam_bifs` audits.

### 1a. `timer:tc/1`

```rust
Case {
    id: "bifs/timer_tc_1_returns_the_fun_value",
    description: "timer:tc/1 returns {Time, Value}; element(2, _) is the fun's value",
    contracts: &[Contract::OtpApps],
    kind: CaseKind::Eval {
        expr: "element(2, timer:tc(fun () -> 42 end))",
        extra_argv: &[],
        probe: Probe::Value,
    },
    golden: Golden { stdout: Some("ok 42"), exit_code: 0 },
}
```

* oracle: `ok 42`, exit `0`; `beam_rt` now **agrees** (`ok 42`, exit `0`) — landed as
  `bifs/timer_tc_returns_the_fun_value`.
* `timer:tc/2,3,4` behaved identically (each computes the elapsed time with `convert_time_unit/3`).
* repro: `erts_rust/target/release/differential compare 'element(2, timer:tc(fun () -> 42 end))'`

### 1b. `timer:apply_after/4` — owner `beam_rt`

```rust
Case {
    id: "bifs/timer_apply_after_returns_ok_tuple",
    description: "timer:apply_after/4 returns {ok, {once, Ref}} (the Ref is per-VM)",
    contracts: &[Contract::OtpApps],
    kind: CaseKind::Eval {
        expr: "is_reference(timer:apply_after(50, erlang, self, []))",
        extra_argv: &[],
        probe: Probe::Value,
    },
    golden: Golden { stdout: Some("ok false"), exit_code: 0 },
}
```

* oracle: `ok false`, exit `0`; `beam_rt` now **agrees** (`ok false`, exit `0`) — landed as
  `bifs/timer_apply_after_first_element_is_ok`.
* The gap was `beam_rt`'s missing kernel supervision tree (`kernel_sup` / `timer_server`), not the
  `timer` module: the module's `apply_after/4` reaches `supervisor:start_child(kernel_sup, …)`; that
  parent is now present, so the stable `element(1, timer:apply_after(50, erlang, self, []))` form
  reads `ok ok` on both runtimes.
* repro: `erts_rust/target/release/differential compare 'is_reference(timer:apply_after(50,erlang,self,[]))'`

## 2. `timer:now_diff/2` — **landed**

**Status:** `landed` — pinned as `bifs/timer_now_diff_of_two_timestamps` (never a gap, just a case
the corpus was missing).

```rust
Case {
    id: "bifs/timer_now_diff_of_two_timestamps",
    description: "timer:now_diff/2 is the microsecond difference of two timestamp tuples",
    contracts: &[Contract::OtpApps],
    kind: CaseKind::Eval {
        expr: "timer:now_diff({0,0,0}, {1,2,3})",
        extra_argv: &[],
        probe: Probe::Value,
    },
    golden: Golden { stdout: Some("ok -1000002000003"), exit_code: 0 },
}
```

* oracle and `beam_rt` both print `ok -1000002000003`, exit `0` → **`[AGREE]`** (verified with
  `differential compare 'timer:now_diff({0,0,0},{1,2,3})'`). Not a gap — the case has now landed.

## 3. `string:concat` — **not a usable case** (`string:concat/1` is `undef` on the oracle too)

**Status:** `stale-fixed-elsewhere` — not a gap and not a case; nothing to add.

* `string:concat/1` does **not** exist: `differential compare 'string:concat("a")'` → **`[AGREE]`**
  with both sides printing `exception error undef`, exit `1`. It cannot be a `mismatch` and it is
  not a gap; do not add it.
* The intended functions both exist and **agree**, and both have now landed as
  `bifs/string_concat_two` and `bifs/lists_concat`:
  * `string:concat/2` — `differential compare 'string:concat("a","b")'` → `ok [97,98]`, exit `0`.
  * `lists:concat/1` — `differential compare 'lists:concat(["a","b",1])'` → `ok [97,98,49]`, exit `0`.

## 4. `zlib` — owner `beam_bifs`

**Status:** the **raw-DEFLATE** surface (`zlib:zip/1` / `zlib:unzip/1`, RFC 1951) is
`now-agreeing-and-promoted` — its cases are pinned in `src/corpus/zlib.rs`. Both **zlib-wrapped**
recipes landed: 4b as `zlib/compress_bytes_are_deterministic`,
`zlib/uncompress_compress_roundtrip_tiny` and
`zlib/uncompress_compress_roundtrip_100_bytes_is_identity`, and 4c (the `zlib:gzip/1` /
`zlib:gunzip/1` pair) as `zlib/gzip_bytes_are_deterministic` and `zlib/gunzip_gzip_roundtrip_tiny`;
two `data_error` failure classes also landed (`zlib/uncompress_three_bytes_is_data_error`,
`zlib/gunzip_non_gzip_is_data_error`). Only 4a (the repetitive-body wrapped size form) stays a
divergence.

`code:which(zlib)` is the atom `preloaded` in **both** runtimes, and `beam_rt` lists `zlib` in its
preloaded set (`crates/beam_rt/src/boot.rs`). The two wrapped-surface recipes, kept for reference:

### 4a. `zlib:compress/1` of a repetitive body — **not a case** (plain-value `mismatch`)

```rust
Case {
    id: "bifs/zlib_compress_size_of_a_repetitive_body",
    description: "zlib:compress/1 deflates a 1 KiB zero body to a fixed 17-byte stream",
    contracts: &[Contract::OtpApps],
    kind: CaseKind::Eval {
        expr: "byte_size(zlib:compress(<<0:8192>>))",
        extra_argv: &[],
        probe: Probe::Value,
    },
    golden: Golden { stdout: Some("ok 17"), exit_code: 0 },
}
```

* oracle `ok 17`, `beam_rt` `ok 23` (the two images are `<<120,156,99,96,24,5,163,96,20,140,84,0,
  0,4,0,0,1>>` and `<<120,156,237,192,1,1,0,0,0,130,32,255,175,110,72,80,192,171,1,4,0,0,1>>`):
  the wrapped image of a repetitive body is the encoder's own choice — a different block choice, not
  a codec contract — so the recipe is a plain-value `mismatch` and must **not** be added. Recorded
  in `divergences.md` §A.7 — zlib.
* repro: `erts_rust/target/release/differential compare 'byte_size(zlib:compress(<<0:8192>>))'`

### 4b. `zlib:compress/1` of a tiny body — **landed**

```rust
Case {
    id: "bifs/zlib_compress_bytes_are_deterministic",
    description: "zlib:compress/1 of a tiny body is a byte-exact, order-stable zlib stream",
    contracts: &[Contract::OtpApps],
    kind: CaseKind::Eval {
        expr: "zlib:compress(<<\"abc\">>)",
        extra_argv: &[],
        probe: Probe::Value,
    },
    golden: Golden { stdout: Some("ok <<120,156,75,76,74,6,0,2,77,1,39>>"), exit_code: 0 },
}
```

* oracle and `beam_rt` both print `ok <<120,156,75,76,74,6,0,2,77,1,39>>`, exit `0` → **`[AGREE]`**
  (zlib level 6 on a 3-byte body is a stored/fixed-huffman stream), so the golden is host- and
  order-stable; the round trip `zlib:uncompress(zlib:compress(<<"abc">>)) =:= <<"abc">>` agrees too.
  Landed as `zlib/compress_bytes_are_deterministic`, `zlib/uncompress_compress_roundtrip_tiny` and
  `zlib/uncompress_compress_roundtrip_100_bytes_is_identity` (the last with the 100-byte body).
* owner: `beam_bifs` (the `zlib` module's NIF-backed surface); `beam_rt` advertises the module as
  preloaded.
* repro: `erts_rust/target/release/differential compare 'zlib:compress(<<"abc">>)'`

### 4c. `zlib:gzip/1` / `zlib:gunzip/1` — **landed**

* the oracle and `beam_rt` both return the gzip container (`zlib:gzip(<<"abc">>)` →
  `<<31,139,8,0,0,0,0,0,0,3,75,76,74,6,0,194,65,36,53,3,0,0,0>>`, and `zlib:gunzip/1` inverts it):
  the pair now **agrees** and is pinned as `zlib/gzip_bytes_are_deterministic` and
  `zlib/gunzip_gzip_roundtrip_tiny`, with the non-gzip failure class pinned as
  `zlib/gunzip_non_gzip_is_data_error`.
* repro: `erts_rust/target/release/differential compare 'zlib:gzip(<<"abc">>)'`

## 5. `process_info/2` items — owner `beam_rt`

**Status:** §5a `landed` (the three `process_info_spawned_*` cases); §5b `landed` — no longer a gap
(the four `process_info_*` position cases now agree); §5c no gap, no case needed.

The most stable receivable form inspects a **freshly spawned** process, not `self()`: the oracle's
`-eval` root process has a different boot shape than `beam_rt`'s (registered / links / trap_exit),
so a `self()`-based golden is not host-independent.

### 5a. `registered_name`, `links`, `trap_exit` on a spawned process — **landed**

```rust
case(
    "system_info/process_info_spawned_registered_name",
    "process_info(P, registered_name) of a fresh process is the bare empty list",
    "begin P = spawn(fun() -> receive after 5000 -> ok end end), \
       erlang:process_info(P, registered_name) end",
    "ok []",
)
case(
    "system_info/process_info_spawned_links_length",
    "a fresh spawned process has no links",
    "begin P = spawn(fun() -> receive after 5000 -> ok end end), \
       length(element(2, erlang:process_info(P, links))) end",
    "ok 0",
)
case(
    "system_info/process_info_spawned_trap_exit",
    "a fresh spawned process does not trap exits",
    "begin P = spawn(fun() -> receive after 5000 -> ok end end), \
       erlang:process_info(P, trap_exit) end",
    "ok {trap_exit,false}",
)
```

* All three **`[AGREE]`** (verified by hand under both runtimes: oracle `[]` / `0` /
  `{trap_exit,false}`, `beam_rt` identical) and have now landed as
  `system_info/process_info_spawned_registered_name`,
  `system_info/process_info_spawned_links_length` and
  `system_info/process_info_spawned_trap_exit`. Not gaps — the corpus was simply missing the
  spawned-process form. (`case(...)` is the `system_info.rs` helper that builds an `Eval` case with
  `Probe::Value`.)

* For completeness, the `self()` forms **do** diverge (a boot-shape artifact, deliberately **not**
  pinned): `length(element(2, erlang:process_info(self(), links)))` → oracle `1`, `beam_rt`
  `0`; `erlang:process_info(self(), trap_exit)` → oracle `true`, `beam_rt` `false`. The
  `registered_name` item is **not** one of them — both runtimes answer `[]` — so it is pinned
  (`bif_extra/process_info_self_registered_name_*`).

### 5b. the four position items — **landed** (now agree, no longer a gap)

```rust
case("system_info/process_info_current_function", "…",
     "element(1, erlang:process_info(self(), current_function))",     "ok current_function")
case("system_info/process_info_initial_call",     "…",
     "element(1, erlang:process_info(self(), initial_call))",         "ok initial_call")
case("system_info/process_info_current_location", "…",
     "element(1, erlang:process_info(self(), current_location))",     "ok current_location")
case("system_info/process_info_catchlevel",       "…",
     "element(1, erlang:process_info(self(), catchlevel))",           "ok catchlevel")
```

* oracle: the key atom (`ok current_function` …), exit `0`; `beam_rt` now **agrees** (the native
  `process_info` floor serves all four items), landed as
  `system_info/process_info_current_function`, `system_info/process_info_initial_call`,
  `system_info/process_info_current_location` and `system_info/process_info_catchlevel`. These were
  previously a typed `unsupported` capability gap (`beam_rt: unsupported: process_info: <item>`,
  exit `3`) for both the `self()` and the spawned-process form (the `current_stacktrace` item is in
  the same family).
* The key-atom form (`element(1, …)`) is the stable observable: the oracle's *values*
  (`{current_function,{erl_eval,do_apply,7}}`, `{initial_call,{erlang,apply,2}}`, a path-bearing
  `current_location`, `{catchlevel,3}`) are `-eval`-shape-specific.
* owner: `beam_rt` (the native `process_info` floor, `crates/beam_rt/src/native/memory/`).
* repro: `erts_rust/target/release/differential compare 'element(1, erlang:process_info(self(), catchlevel))'`

### 5c. the volatile items — **already agree**; no new case needed

`heap_size`, `stack_size`, `total_heap_size`, `memory`, `reductions`, `binary` and
`garbage_collection` are already pinned **structurally** by the `system_info/process_info_*` cases
(`is_integer(element(2, …))` / `is_list(…)` / `element(1, …) =:= <key>`), and those agree. The
`garbage_collection` proplist has the **same five keys on both runtimes**
(`fullsweep_after`, `max_heap_size`, `min_bin_vheap_size`, `min_heap_size`, `minor_gcs`) — only the
`minor_gcs` count and the `max_heap_size` map value differ, and both are volatile, so they stay
unpinned. No gap here.

## 6. `-pa` / `-pz` / `+S N:online` — owner `beam_rt`

**Status:** `now-agreeing-and-promoted` — all three shapes below are faithful in `beam_rt` and are
pinned in `src/corpus/flags.rs` (the ids in the literals are the ones that landed).

`beam_rt`'s CLI (`crates/beam_rt/src/cli.rs`) filters a non-existent `-pa`/`-pz` directory out of
`code:get_path/0` (as `erl` does), accepts a multi-value `-pa`/`-pz` occurrence, and honours the
`:N` half of `+S N:N` for `schedulers_online`. The shadowing half of the same surface is 6d.

### 6a. a non-existent `-pa` / `-pz` directory

```rust
Case {
    id: "flags/pa_nonexistent_dir_is_not_on_the_code_path",
    description: "a non-existent -pa dir is filtered out of code:get_path/0",
    contracts: &[Contract::Cli],
    kind: CaseKind::Eval {
        expr: "lists:member(\"/no/such/dir\", code:get_path())",
        extra_argv: &["-pa", "/no/such/dir"],
        probe: Probe::Value,
    },
    golden: Golden { stdout: Some("ok false"), exit_code: 0 },
}
```

* both runtimes: `ok false`, exit `0`. The append-side twin is the landed
  `flags/pz_nonexistent_dir_is_not_on_the_code_path` (the same body with
  `extra_argv: &["-pz", "…"]`; the landed switch value is `/nonexistent-differential-dir`).
* repro: `beam_rt -noshell -pa /no/such/dir -eval 'io:format("~w~n",[lists:member("/no/such/dir", code:get_path())]),halt().'`

### 6b. multiple values in one occurrence

```rust
Case {
    id: "flags/pa_multiple_values_one_occurrence",
    description: "erl records every following non-switch token as one -pa occurrence's values",
    contracts: &[Contract::Cli],
    kind: CaseKind::Eval {
        expr: "init:get_argument(pa)",
        extra_argv: &["-pa", "a", "b"],
        probe: Probe::Value,
    },
    golden: Golden { stdout: Some("ok {ok,[[[97],[98]]]}"), exit_code: 0 },
}
```

* both runtimes: `ok {ok,[[[97],[98]]]}`, exit `0` (one occurrence carrying `["a","b"]`); the
  append-side twin is the landed `flags/pz_multiple_values_one_occurrence`
  (`init:get_argument(pz)` with `["-pz", "a", "b"]`), which has the identical multi-value shape
  (`crates/beam_rt/src/cli.rs`, the `-pz` arm).
* repro: `beam_rt -noshell -pa a b -eval 'io:format("~w~n",[init:get_argument(pa)]),halt().'`

### 6c. `+S N:online`

```rust
Case {
    id: "flags/plus_s_total_online_sets_schedulers_online",
    description: "+S 4:2 sizes the scheduler pool to 4 but puts only 2 online",
    contracts: &[Contract::Cli],
    kind: CaseKind::Eval {
        expr: "erlang:system_info(schedulers_online)",
        extra_argv: &["+S", "4:2"],
        probe: Probe::Value,
    },
    golden: Golden { stdout: Some("ok 2"), exit_code: 0 },
}
```

* both runtimes: `ok 2`, exit `0` (with `erlang:system_info(schedulers)` = `4`);
  `parse_schedulers` (`crates/beam_rt/src/cli.rs`) keeps the `:2` half and
  `schedulers_online` (`crates/beam_rt/src/info/schedulers.rs`) reports it.
* repro: `beam_rt -noshell +S 4:2 -eval 'io:format("~w~n",[erlang:system_info(schedulers_online)]),halt().'`

### 6d. a stdlib-module shadow — the `-pz` side only

* The `-pz` non-shadowing check **is** expressible and landed as the `ModuleEnv` case
  `flags_apply/erl_aflags_pz_does_not_shadow_a_stdlib_module` (`src/corpus/flags_apply.rs`): the
  `ERL_AFLAGS`-carried directory holds a module *named* like a stdlib module (`proplists`) and
  reaches the code path only through the environment, and the executed module reduces
  `code:which(proplists) =:= <shadow beside ?MODULE>` to the integer `0` (the stdlib module still
  wins) — the raw `code:which/1` answer is an installation-absolute path, so the integer reduction
  is what makes the check portable at all.
* The `-pa` side stays unpinnable: `-pa` *prepends*, so the shadow wins and is loaded during the
  oracle's boot, which crashes it (empty stdout, no comparable golden).

## 7. Corrections — things named as gaps that are **not** gaps

**Status:** `stale-fixed-elsewhere` — every row below is a non-gap; no case can be derived from any
of them. (The three `nif_abi/*` ids are current in `src/corpus/nif_abi.rs`.)

| named item | finding | owner |
|---|---|---|
| `timer:now_diff/2` | **agrees** — pinned as `bifs/timer_now_diff_of_two_timestamps` (§2), not a gap | — |
| `string:concat/1` | `undef` on the **oracle too** — not a usable case; `string:concat/2` and `lists:concat/1` agree (§3) | — |
| `enif_atom_len` | **not a NIF C ABI symbol at all** — absent from `erl_nif.h` *and* `erl_nif_api_funcs.h` (oracle **and** in-tree `erts/emulator/beam/erl_nif.h`); nothing to export, so no case is possible | — |
| `enif_make_ref` / `enif_term_to_binary` / `enif_binary_to_term` | declared **and** exported by `beam_rt`; **already closed** — each is covered by an agreeing `nif_abi/*` case (`ref_make_ref_is_a_ref`, `term_to_binary_valid_term`, `binary_to_term_valid_encoding`) | — |
| `process_info/1` gc key set | the `garbage_collection` proplist has the **same 5 keys** in both runtimes (only volatile values differ) (§5c) | — |
| `process_info/2` `registered_name`/`links`/`trap_exit` | **agree on a spawned process** (§5a); of the `self()` forms only `links`/`trap_exit` still diverge | — |
| `catchlevel` | **agrees** — a plain key-atom observation, pinned as `system_info/process_info_catchlevel` (§5b); not a value mismatch | — |

### The A.3 `enif_*` completeness check (why there is no missing-symbol gap)

Every `enif_*` function declared in the oracle's `erl_nif_api_funcs.h` is exported by the release
`beam_rt` — the diff is empty:

```sh
# the oracle's include dir (the sibling of the `erl` on $PATH that holds erl_nif.h)
INC="$(dirname "$(dirname "$(command -v erl)")")/include"
grep -oE 'ERL_NIF_API_FUNC_DECL\([^,]+,enif_[a-z0-9_]+' "$INC/erl_nif_api_funcs.h" \
  | grep -oE 'enif_[a-z0-9_]+' | sort -u > "$TMPDIR/api.txt"
nm -D --defined-only erts_rust/target/release/beam_rt | awk '{print $3}' \
  | grep -E '^enif_' | sort -u > "$TMPDIR/rt.txt"
comm -23 "$TMPDIR/api.txt" "$TMPDIR/rt.txt"   # empty: 137/137 declared enif_* are exported
```

A NIF `.so` may therefore reference any declared `enif_*` without risking a `{error,{load_failed,…}}`
on the Rust side; when an owning crate ever *does* stop exporting one, the missing symbol shows up
as a non-empty `comm` above and must be added back before a NIF case can pin it.

## 8. `erlang:system_info(snifs)` — **blocked**; owner `beam_rt`

**Status:** `blocked-on-beam_rt` — the roster **data** is ready (`beam_bifs::snifs()` yields the 556 declared `bif.tab` rows in the oracle's list order), but this HEAD's `beam_rt` does not serve the key: `crates/beam_rt/src/info.rs:253` still lists `"snifs"` in `UNSUPPORTED_KEYS`, so the Rust side answers the typed `beam_rt: unsupported: …` and exits `3` (the case reports `[UNSUPPORTED]`). This is a **closable** gap, deliberately **not** recorded in `known-gaps.txt` (that allowlist holds only genuinely unclosable gaps); the recipe lands the moment the key is served.

```rust
Case {
    id: "system_info/snifs_is_the_reversed_bif_tab_roster",
    description: "erlang:system_info(snifs) is the declared bif.tab roster reversed (release-independent: length floor, 3-tuple shape, lists:last/1 order and two member rows, never the exact length or head)",
    contracts: &[Contract::Introspection],
    kind: CaseKind::Eval {
        expr: "(fun() -> S = erlang:system_info(snifs), [is_list(S), length(S) > 500, lists:all(fun(T) -> tuple_size(T) =:= 3 andalso is_atom(element(1, T)) andalso is_atom(element(2, T)) andalso is_integer(element(3, T)) end, S), lists:last(S), lists:member({erlang, abs, 1}, S), lists:member({lists, reverse, 2}, S)] end)()",
        extra_argv: &[],
        probe: Probe::Value,
    },
    golden: Golden { stdout: Some("ok [true,true,true,{erlang,abs,1},true,true]"), exit_code: 0 },
}
```

* **the oracle-recorded golden**: `ok [true,true,true,{erlang,abs,1},true,true]`, exit `0` — re-measured on this host (OTP 29.1.2 / erts-17.1.1) with the `Probe::Value` wrapper `erl -noshell -eval 'try (EXPR) of V -> io:format("ok ~w~n",[V]), halt(0) catch C:R -> io:format("exception ~w ~w~n",[C,R]), halt(1) end.'`.
* **three pinning traps, each re-measured here:**
  * `hd/1` must NOT be pinned: this oracle's head is `{records,get_definition,2}` while this tree's roster head is `{erlang,ref_to_binary,1}` — the last three `bif.tab` rows are the OTP-30-only `erlang:{pid,port,ref}_to_binary/1`.
  * `length/1` must NOT be pinned by value: **553** rows on this OTP-29 oracle vs **556** declared in this tree. The `length(S) > 500` **floor** is what holds on both releases.
  * `{lists,reverse,1}` does not exist in either roster (measured `false`; the entry is `reverse/2`). The reversal is pinned release-independently through `lists:last(S) =:= {erlang, abs, 1}` — the *first* declared row in both releases.
* **the work the owning crate must do** (from `crates/beam_bifs/CONTEXT.md`, read-only): `"snifs"` out of `crates/beam_rt/src/info.rs`'s `UNSUPPORTED_KEYS` into the served set, plus a builder in `crates/beam_rt/src/info/system.rs` whose value is `beam_bifs::snifs()`'s rows as `{Module, Function, Arity}` 3-tuples; the per-key rationale deleted from `crates/beam_rt/src/info/unsupported.rs` (and from its doc table); the two key-count pins retyped (`src/info.rs` `88 → 89` supported and `27 → 26` unsupported); the atom vocabulary widened to the roster's **465** distinct names (25 modules + 440 function names), or `every_supported_key_materializes…` fails on `TermNotFound`; and `tests/system_info_oracle.rs` moving `"snifs"` out of the unsupported set into the **SHAPE-ONLY** class — never `ORACLE_EQUAL`, because the value legitimately differs by release (553 vs 556).
* repro once the key is served: `erts_rust/target/release/differential compare 'erlang:system_info(snifs)'`, and `differential run system_info/snifs_is_the_reversed_bif_tab_roster` once the case lands.

## 9. `maps` no-op identity — **not a case** (a measured divergence); owner `beam_rt`

**Status:** `blocked-on-beam_rt (a divergence, so no case can pin it)` — `crates/beam_bifs`'s `maps` no-op early returns are correct; the divergence lives in the result-admission path.

Measured on this host, both sides, `Probe::Value`, for

```erlang
(fun() -> M = maps:from_list([{a,1},{b,2}]), Q = maps:from_list([{b,2},{a,1}]), [erts_debug:same(M, maps:put(a,1,M)), erts_debug:same(M, maps:update(a,1,M)), erts_debug:same(M, maps:remove(zz,M)), erts_debug:same(M, maps:merge(M,M)), erts_debug:same(M,Q), M =:= Q, erts_debug:same(M, maps:with([a],M)), erts_debug:same(M, maps:put(a,2,M))] end)()
```

* **oracle**: `ok [true,true,true,true,false,true,false,false]`
* **`beam_rt`**: `ok [false,false,false,false,false,true,false,false]`

Both lines are stored structure identity (`erts_debug:same/2`), not equality: the oracle's `maps` returns the *argument's own term* from a no-op, so `maps:put(M, a, 1)`, `maps:update/3`, `maps:remove/2` of an absent key and `maps:merge(M, M)` are all the same word as `M`; the same holds for `maps:with/2` on this shape. `M =:= Q` is `true` on both sides (structural equality is not the issue), and a *changing* call (`maps:put(M, a, 2)`) is a fresh term on both — which is why the divergence is invisible to every equality-based case and can only be seen through `erts_debug:same/2`.

**Mechanism (the measured cause).** A registered BIF's result is always deep-copied into the caller's heap — `move_result_into_process_heap` → `copy_result_fresh` → `beam_message::copy_struct` (`crates/beam_rt/src/context/calls.rs`) — so a no-op BIF can never hand back the argument's word. Only the **inlined guard** BIFs keep it, and only from compiled code: there, measured `{erts_debug:same(M, element(1,{M})), erts_debug:same(M, hd([M])), erts_debug:same(M, lists:nth(1,[M])), erts_debug:same(M, M)}` is `{true,true,true,true}` on the oracle and `{true,true,false,true}` on `beam_rt` (`element/2` and `hd/1` compile to the `get_tuple_element`/`get_list` opcodes, while `lists:nth/2` is a registered BIF), unchanged under `BEAM_RT_TIER=interpreter` — so the tier is not the variable. On the `-eval` tree-walking route the same tuple is `{false,false,false,true}` here (the walker routes even those two through the result copy), which is the same mechanism seen on the other route.

**Owner and why no case can pin it.** `crates/beam_bifs`'s `maps` no-op early returns are correct and are already pinned crate-side (`crates/beam_bifs/tests/maps_identity.rs`); the fix belongs to the result-admission path in `crates/beam_rt` (a BIF whose result is its own argument must be admitted without a copy), not to the BIF. A corpus case is impossible in the meantime: the only observable is a plain `false` (a `mismatch`-class value) rather than the runtime's documented `unsupported` signal, so pinning the oracle line would turn the gate red by design.

## 10. `ets:lookup/2` of an undefined table — the `{cause,_}` error_info pair; owner `beam_engine` (`ExecutionContext::bif_error_cause`, needs a host impl in `beam_bifs`/`beam_rt`)

**Status:** `blocked-on-beam_rt` — the **engine** half is already in place
(`crates/beam_engine/src/families/exceptions/bif_frame.rs` asks `ExecutionContext::bif_error_cause`
per failed BIF and already sizes C's two-pair extension, `EXTENSION_WORDS_WITH_CAUSE`), and
`crates/beam_bifs/src/error_cause.rs::error_cause` *decides* the measured rows
(`ets:lookup(unknown_atom_xyz, k)` ⇒ `ErrorCause::Cause(id)`, `ets:lookup(1, k)` ⇒ `type`,
`erlang:register(a, 1)` ⇒ `none`), but **no host implements the seam**: the only implementations in
the tree are the trait default `Ok(None)` (`crates/beam_engine/src/context.rs`) and a test double
(`families/exceptions/bif_frame/tests.rs`). So a compiled raise renders C's **one-pair**
`#{module => FormatModule}` map where the oracle renders the **two-pair** `#{cause => Fvalue, module
=> FormatModule}` one. This is a **closable** gap, deliberately **not** recorded in
`known-gaps.txt` (that allowlist holds only genuinely unclosable gaps); the recipe lands the moment
a host answers the seam.

```rust
Case {
    id: "errors/ets_bad_table_cause",
    description: "a failing ets:lookup/2 item carries the two-pair {error_info,#{cause => id,module => erl_stdlib_errors}} extension",
    contracts: &[Contract::LanguageSemantics],
    kind: CaseKind::Eval {
        expr: "(fun() -> Self = self(), spawn(fun() -> {Cls, Rsn, St} = try ets:lookup(unknown_atom_xyz, k) catch Cls0:Rsn0:St0 -> {Cls0, Rsn0, St0} end, I = hd(St), Self ! {Cls, Rsn, element(1,I), element(2,I), element(3,I), [case P of {error_info, M} -> {error_info, lists:sort(maps:to_list(M))}; Q -> element(1, Q) end || P <- element(4,I), is_tuple(P), element(1,P) =/= file, element(1,P) =/= line]} end), receive X -> X after 10000 -> timeout end end)()",
        extra_argv: &[],
        probe: Probe::Value,
    },
    golden: Golden {
        stdout: Some(
            "ok {error,badarg,ets,lookup,[unknown_atom_xyz,k],[{error_info,[{cause,id},{module,erl_stdlib_errors}]}]}",
        ),
        exit_code: 0,
    },
}
```

It belongs in `src/corpus/errors_extra.rs` — the group that pins the raise's stacktrace-side
payload, whose `//!` doc already carries the "why the expression spawns a closure" rule (the
`-eval` tree walker renders a caught raise from its own symbolic model and loses the extension, so
the payload is observable only from **compiled** code). The id keeps the plain `errors/` prefix of
the `errors` group: it pins that group's own subject, and the id names the family rather than the
file.

* **the oracle-recorded golden**: `ok {error,badarg,ets,lookup,[unknown_atom_xyz,k],[{error_info,[{cause,id},{module,erl_stdlib_errors}]}]}`,
  exit `0` — re-measured on this host (OTP 29.1.2 / erts-17.1.1) with the `Probe::Value` wrapper
  `erl -noshell -eval 'try (EXPR) of V -> io:format("ok ~w~n",[V]), halt(0) catch C:R -> io:format("exception ~w ~w~n",[C,R]), halt(1) end.'`
  (not with `differential compare`, whose default `Probe::TermToBinary` wrapper prints the ETF
  image instead of the `~w` line).
* **the `beam_rt` side's current output**: `ok {error,badarg,ets,lookup,[unknown_atom_xyz,k],[{error_info,[{module,erl_stdlib_errors}]}]}`,
  exit `0` — the projection of the same raise with the `cause` pair missing. Because both sides
  produce a **plain differing observable** (not the runtime's `unsupported` signal), the case
  classifies as a `mismatch`, which no allowlist can cover (`ratchet::classify` ratchets only
  `Unsupported`/`Skipped`, and `suite_exit_code` returns `1` on any mismatch) — which is exactly
  why it was demoted from `src/corpus/errors_extra.rs` to this recipe rather than recorded as a
  gap.
* **the projection is what makes the case pinnable**: only the item's shape is asserted — the
  class/reason, the item's `M`/`F`/`A` slots and the extension's **sorted** pairs. The location's
  `{file,_}`/`{line,_}` entries are dropped (this runtime cooks none: `divergences.md` §A.1) and
  sorting the map's pairs means the host's atom-interning order cannot leak into the golden.
* **the work the owning crate must do**: implement `ExecutionContext::bif_error_cause`
  (`crates/beam_rt/src/context/execution.rs`, next to `foreign_record_definition`) over the
  `BifContext` the runtime builds per BIF call (`crates/beam_rt/src/context/calls.rs`'s
  `BifContext::new(heap, atoms, self_pid)` chain, with the table registry installed so the `ets`
  half really decides), forwarding to `beam_bifs::error_cause` and mapping
  `ErrorCause::Cause(v) ⇒ Ok(Some(v))`, `ErrorCause::Undecided ⇒ Ok(None)` (never an absence
  claim). No `beam_engine` change is needed — the seam, the two-pair sizing and the rendered key
  order are already there (`crates/beam_engine/src/CONTEXT.md` §"`bif_error_cause`").
* repro once the seam is wired: build, then run the case and expect `[AGREE]` —
  ```sh
  erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p beam_rt -p differential
  erts_rust/target/debug/differential run errors/ets_bad_table_cause
  ```
  and the same expression through `differential compare` must go from `[MISMATCH]` to `[AGREE]`
  (that route compares the two sides' ETF images, so it is a pre-check, not the golden's source).
  Landing the case means adding it to `errors_extra.rs`'s `group()`, its `IDS` table (whose
  `[&str; 7]` size goes back to `[&str; 8]`) and re-adding the per-case shape test; no allowlist
  or `GAPS.md` change is involved, because an agreeing case is not a gap.
