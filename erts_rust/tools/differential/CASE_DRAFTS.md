# Draft corpus cases — gap recipes, promotions, and corrections

**Recipes, promotions, corrections.** Each numbered section carries a one-line **Status:** and
either a ready-to-add `Case` literal for the conformance corpus (`src/corpus/*.rs`) or a
correction saying why no case is possible. A recipe is added only **after** the owning crate
closes the gap it names, then `known-gaps.txt`/`GAPS.md` are refreshed in the same change (see
`../CONTEXT.md` §"Notes for Agents").

At a glance: **§1**, **§2**, **§4b**, **§4c**, **§5a**, **§5b** and **§6** are now **landed** — every
one agrees in `beam_rt` and is pinned by the `src/corpus/**` groups (`zlib.rs`, `bifs.rs`,
`system_info.rs`, `flags.rs`, `flags_apply.rs`); **§3** and **§7** record items that were named as
gaps but are not gaps at all. The only remaining zlib divergences are **§4a** (the repetitive-body
wrapped size form) and **§4**'s large-body image-choice (the 100-byte raw-DEFLATE `zip/1` stream),
both unpinnable and recorded in `divergences.md`, not drafts.

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
  capability gap. The four `process_info/2` position items in §5b were this shape before they landed.

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
  pinned): `erlang:process_info(self(), registered_name)` → oracle `[]`, `beam_rt`
  `{registered_name,init}`; `length(element(2, erlang:process_info(self(), links)))` → oracle `1`,
  `beam_rt` `0`; `erlang:process_info(self(), trap_exit)` → oracle `true`, `beam_rt` `false`.

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
| `timer:now_diff/2` | **agrees** — a case could be ADDED (§2), not a gap | — |
| `string:concat/1` | `undef` on the **oracle too** — not a usable case; `string:concat/2` and `lists:concat/1` agree (§3) | — |
| `enif_atom_len` | **not a NIF C ABI symbol at all** — absent from `erl_nif.h` *and* `erl_nif_api_funcs.h` (oracle **and** in-tree `erts/emulator/beam/erl_nif.h`); nothing to export, so no case is possible | — |
| `enif_make_ref` / `enif_term_to_binary` / `enif_binary_to_term` | declared **and** exported by `beam_rt`; **already closed** — each is covered by an agreeing `nif_abi/*` case (`ref_make_ref_is_a_ref`, `term_to_binary_valid_term`, `binary_to_term_valid_encoding`) | — |
| `process_info/1` gc key set | the `garbage_collection` proplist has the **same 5 keys** in both runtimes (only volatile values differ) (§5c) | — |
| `process_info/2` `registered_name`/`links`/`trap_exit` | **agree on a spawned process** (§5a); only the `self()` boot shape diverges | — |
| `catchlevel` | a genuine typed `unsupported` on the `-eval`/fun-bodied process, **not** a value mismatch (§5b) | `beam_rt` |

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
