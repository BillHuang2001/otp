# Draft corpus cases — outstanding Rust-runtime capability gaps

**Drafts, not cases.** Every recipe below is a ready-to-add `Case` literal for the conformance
corpus (`src/corpus/*.rs`), but **none is in the corpus yet**: each currently makes the
compatibility gate diverge (a `mismatch`, or an `unsupported` that is not in `known-gaps.txt` ⇒
gate exit 3). Add a recipe only **after** the owning crate closes the gap named in it, then
refresh `known-gaps.txt`/`GAPS.md` in the same change (see `../CONTEXT.md` §"Notes for Agents").

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

For a recipe that needs extra argv (the `-pa`/`+S` cases) run both runtimes by hand with the same
argv, remembering that `beam_rt` is a drop-in `erl` (contract A.6):

```sh
erl     -noshell <extra…> -eval 'io:format("~w~n",[EXPR]),halt().'
beam_rt -noshell <extra…> -eval 'io:format("~w~n",[EXPR]),halt().'
```

Every observable below was produced that way against the live oracle (system `erl`, OTP 29.1.1 /
erts-17.1) and the release `beam_rt`, not guessed. A `CaseKind::Eval`/`EvalEnv` recipe uses the
shared argv `["-noshell", <extra…>, "-eval", <probe>]`; `Probe::Value` prints the oracle's `~w`
rendering as the single golden line (no trailing newline), exit `0`.

## How a recipe will classify until its owner fixes it

* A runtime that returns a **differing observable** (a plain Erlang value or a plain reason such as
  `exception error undef`) is a `mismatch` — the gate's hard failure. §1, §4 and §6 are this shape
  today.
* A runtime that signals the gap through its documented convention — exit `3` with
  `beam_rt: unsupported: …`, or a probe-caught `exception … unsupported: …` — is an `unsupported`
  capability gap. The four `process_info/2` position items in §5 are this shape today.

---

## 1. `timer` — owner `beam_bifs` (root cause: `erlang:convert_time_unit/3`)

`timer` **loads and runs** in `beam_rt` already (`timer:now_diff/2`, `timer:sleep/1`,
`timer:seconds/1` … all agree; `code:which(timer)` resolves to the stdlib `timer.beam`). The gap is
**not** the `timer` module surface: `timer:tc/1,2,3,4` and `timer:apply_after/4` all fail, and the
missing piece for the `tc/*` family is a single absent BIF —
**`erlang:convert_time_unit/3` is `undef`**:

```sh
erts_rust/target/release/differential compare 'erlang:convert_time_unit(1000, native, microsecond)'
# oracle: ok 1000   rust: exception error undef   -> MISMATCH
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

* oracle: `ok 42`, exit `0`. `beam_rt`: `exception error undef`, exit `1` → **mismatch**.
* `timer:tc/2,3,4` fail identically (each computes the elapsed time with `convert_time_unit/3`).
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

* oracle: `ok false`, exit `0`. `beam_rt`: exit `1` with
  `exception exit {noproc,{gen_server,call,[kernel_sup,{start_child,#{id => timer_server,…}},…]}}`
  → **mismatch**.
* The gap is `beam_rt`'s missing kernel supervision tree (`kernel_sup` / `timer_server`), not the
  `timer` module: the module's `apply_after/4` reaches `supervisor:start_child(kernel_sup, …)` and
  the parent is absent. A stable alternative that observes the same failure:
  `element(1, timer:apply_after(50, erlang, self, []))` → oracle `ok ok`.
* repro: `erts_rust/target/release/differential compare 'is_reference(timer:apply_after(50,erlang,self,[]))'`

## 2. `timer:now_diff/2` — **already agrees**; a case could be ADDED

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
  `differential compare 'timer:now_diff({0,0,0},{1,2,3})'`). Not a gap — a case the corpus is simply
  missing.

## 3. `string:concat` — **not a usable case** (`string:concat/1` is `undef` on the oracle too)

* `string:concat/1` does **not** exist: `differential compare 'string:concat("a")'` → **`[AGREE]`**
  with both sides printing `exception error undef`, exit `1`. It cannot be a `mismatch` and it is
  not a gap; do not add it.
* The intended functions both exist and **agree**, so they are the ones to pin if desired:
  * `string:concat/2` — `differential compare 'string:concat("a","b")'` → `ok [97,98]`, exit `0`.
  * `lists:concat/1` — `differential compare 'lists:concat(["a","b",1])'` → `ok [97,98,49]`, exit `0`.

## 4. `zlib` — owner `beam_bifs`

`code:which(zlib)` is the atom `preloaded` in **both** runtimes, and `beam_rt` lists `zlib` in its
preloaded set (`crates/beam_rt/src/boot.rs`), but none of its functions are served — every
`zlib:*` call is `undef`. Two recipes, one volatile and one byte-exact:

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

* oracle: `ok 17` and the byte string above, both exit `0`. `beam_rt`: `exception error undef`,
  exit `1` for both → **mismatch**.
* The exact-bytes form is fully deterministic (zlib level 6 on a 3-byte body is a stored/fixed-huffman
  stream), so the `<<120,156,…>>` golden is host- and order-stable.
* owner: `beam_bifs` (the `zlib` module's NIF-backed surface); `beam_rt` advertises the module as
  preloaded.
* repro: `erts_rust/target/release/differential compare 'zlib:compress(<<"abc">>)'`

## 5. `process_info/2` items — owner `beam_rt`

The most stable receivable form inspects a **freshly spawned** process, not `self()`: the oracle's
`-eval` root process has a different boot shape than `beam_rt`'s (registered / links / trap_exit),
so a `self()`-based golden is not host-independent.

### 5a. `registered_name`, `links`, `trap_exit` on a spawned process — **already agree**; ADD

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

* All three **`[AGREE]`** today (verified by hand under both runtimes: oracle `[]` / `0` /
  `{trap_exit,false}`, `beam_rt` identical). Not gaps — the corpus is simply missing the
  spawned-process form. (`case(...)` is the `system_info.rs` helper that builds an `Eval` case with
  `Probe::Value`.)

* For completeness, the `self()` forms **do** diverge (a boot-shape artifact, deliberately **not**
  pinned): `erlang:process_info(self(), registered_name)` → oracle `[]`, `beam_rt`
  `{registered_name,init}`; `length(element(2, erlang:process_info(self(), links)))` → oracle `1`,
  `beam_rt` `0`; `erlang:process_info(self(), trap_exit)` → oracle `true`, `beam_rt` `false`.

### 5b. the four position items — **typed `unsupported`** (capability gap; add once served)

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

* oracle: the key atom (`ok current_function` …), exit `0`. `beam_rt`: exit `3` with
  `beam_rt: unsupported: process_info: <item> (this runtime holds no engine for that process…)` →
  **`unsupported`** (a capability gap; it must be added to `known-gaps.txt` if pinned before the
  crate serves it). Verified for both the `self()` and the spawned-process form — the runtime
  answers the typed gap either way (`current_stacktrace` is in the same family).
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

## 6. `-pa` / `-pz` — owner `beam_rt`

`beam_rt`'s CLI (`crates/beam_rt/src/cli.rs`) consumes exactly **one** value per `-pa`/`-pz` and
adds every `-pa` directory to the code path unconditionally.

### 6a. a non-existent `-pa` directory

```rust
Case {
    id: "flags/pa_nonexistent_dir_is_not_on_the_code_path",
    description: "erl filters a non-existent -pa dir out of code:get_path/0; beam_rt adds it",
    contracts: &[Contract::Cli],
    kind: CaseKind::Eval {
        expr: "lists:member(\"/no/such/dir\", code:get_path())",
        extra_argv: &["-pa", "/no/such/dir"],
        probe: Probe::Value,
    },
    golden: Golden { stdout: Some("ok false"), exit_code: 0 },
}
```

* oracle: `ok false`, exit `0`. `beam_rt`: `ok true`, exit `0` → **mismatch**.
* repro: `beam_rt -noshell -pa /no/such/dir -eval 'io:format("~w~n",[lists:member("/no/such/dir", code:get_path())]),halt().'`

### 6b. multiple values in one occurrence

```rust
Case {
    id: "flags/pa_multiple_values_in_one_occurrence",
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

* oracle: `ok {ok,[[[97],[98]]]}`, exit `0` (one occurrence carrying `["a","b"]`). `beam_rt`:
  rejects the token with `beam_rt: error unexpected argument "b"` and the usage banner, exit `2`
  → **mismatch**.
* `-pz` has the identical single-value limitation (`crates/beam_rt/src/cli.rs`, the `-pz` arm).
* repro: `beam_rt -noshell -pa a b -eval 'io:format("~w~n",[init:get_argument(pa)]),halt().'`

### 6c. `+S N:online` — owner `beam_rt`

```rust
Case {
    id: "flags/plus_s_online_half_sets_schedulers_online",
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

* oracle: `ok 2`, exit `0` (with `erlang:system_info(schedulers)` = `4`). `beam_rt`: `ok 4`,
  exit `0` → **mismatch**: `parse_schedulers` (`crates/beam_rt/src/cli.rs`) parses the `:2` half but
  drops it, and `schedulers_online` is reported equal to `schedulers`
  (`crates/beam_rt/src/info/schedulers.rs`).
* repro: `beam_rt -noshell +S 4:2 -eval 'io:format("~w~n",[erlang:system_info(schedulers_online)]),halt().'`

## 7. Corrections — things named as gaps that are **not** gaps

| named item | finding | owner |
|---|---|---|
| `timer:now_diff/2` | **agrees** — a case could be ADDED (§2), not a gap | — |
| `string:concat/1` | `undef` on the **oracle too** — not a usable case; `string:concat/2` and `lists:concat/1` agree (§3) | — |
| `enif_atom_len` | **not a NIF C ABI symbol at all** — absent from `erl_nif.h` *and* `erl_nif_api_funcs.h` (oracle **and** in-tree `erts/emulator/beam/erl_nif.h`); nothing to export. No case possible. | — |
| `enif_term_to_binary` | declared **and** exported by `beam_rt`; already covered by `nif_abi/term_to_binary_valid_term` (`[AGREE]`) | — |
| `enif_binary_to_term` | declared **and** exported; covered by `nif_abi/binary_to_term_valid_encoding` (`[AGREE]`) | — |
| `enif_make_ref` | declared **and** exported; covered by `nif_abi/ref_make_ref_is_a_ref` (`[AGREE]`) | — |
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
