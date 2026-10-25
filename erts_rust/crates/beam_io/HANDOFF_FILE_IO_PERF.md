# Handoff to `beam_rt` — the `file_io` per-operation costs (attribution + patch)

This node owns no code on the `file_io` benchmark's paths, so the whole finding below is a
**handoff**: the attribution, the two patches and the measurements for the parent node to land in
`erts_rust/crates/beam_rt`. Everything here was measured on this tree, and the patches were
applied, built, measured and test-run here before being reverted.

Result in one line: `file_io/handle_rw` went from **1 511 509 ns/op to 36 275 ns/op** (paired, 5
rounds) — `10.8×` dearer than the oracle before, **3.9× cheaper** than the oracle after — from a
12-line code move in `native/prim_file.rs`.

## 1. The attribution (measured, not inferred)

`perf record -e cpu_core/instructions/u -F 2999 --call-graph dwarf` on a fixture that runs the
benchmark's exact `handle_rw` loop (`file:open(P,[write,raw,binary])` + `file:write/2` +
`file:close/1` + `file:open(P,[read,raw,binary])` + `file:read/2` + `file:close/1`, 4 096-byte
payload), driven through the CLI `run` action:

```
__libc_malloc2                                      24.1 %   children
  Arc<[u8]>::copy_from_slice  <-  Arc<str>::from(&str)
    <beam_term::atom::AtomTable as Clone>::clone
      Rc<AtomTable>::make_mut
        CodeServer::atoms_mut                     (codeserver.rs:450  Rc::make_mut(&mut self.atoms))
          RuntimeContext::atom
            RuntimeContext::build_descriptor      (native/prim_file.rs:1027)
              RuntimeContext::prim_open
_int_free_chunk                                     14.1 %   (11.96 % = the same clone being dropped)
HashMap<(Arc<str>,Atom)>::insert                    11.9 %
```

≈**45–50 % of all retired instructions** on the workload is *deep-copying the whole shared atom
table and throwing the copy away*, and the reason is one binding:

```rust
// native/prim_file.rs, build_descriptor — before
let atoms = self.runtime.code_server.atoms_handle();   // Rc::clone(&self.atoms) — lives to EOF
let data = terms::make_map(self, &pairs, &atoms)?;
let tag = self.atom("file_descriptor")?;               // intern, with strong_count == 2
let module = self.atom("prim_file")?;                  // intern, with strong_count == 2
```

`CodeServer::atoms_mut` is `Rc::make_mut`, which copies the table whenever it is not uniquely
owned. `atoms_handle()` is an `Rc` clone that NLL keeps alive to the end of the function, so both
interns deep-clone the table: **one `Arc<str>` allocation + one hash-map insertion per atom, ~9 473
atoms (`system_info(atom_count)` after the default boot script), twice per `open/2`, four times per
benchmark iteration** (~37 900 allocations + frees per iteration). There is no long-lived holder
pinning the strong count — `prim_write`/`prim_read`/`prim_close` intern `ok` through the same
`atoms_mut()` and cost **0.19 % of the run between them**, which is the control that proves the
code server is the unique owner everywhere except inside `build_descriptor`.

**Why `write_read` (the case the runtime wins) avoids it:** `file:write_file/2` and
`file:read_file/1` are served by the native `file_server_2` floor's `{write_file,…}`/`{read_file,…}`
requests and never construct a `#file_descriptor{}`, so they never enter `build_descriptor` — no
handle is taken, no intern happens under a live handle. The asymmetry is exactly "builds a handle
descriptor" vs "does not", not whole-file vs handle.

## 2. The patch (verified)

```diff
--- a/erts_rust/crates/beam_rt/src/native/prim_file.rs
+++ b/erts_rust/crates/beam_rt/src/native/prim_file.rs
@@ -1040,12 +1040,18 @@ impl RuntimeContext<'_> {
             pairs.push((self.atom("r_buffer")?, buffer));
             pairs.push((self.atom("r_ahead_size")?, make_small(modes.read_ahead)));
         }
+        // Every name is interned **before** the table handle below is taken: the handle is an
+        // `Rc` clone and `CodeServer::atoms_mut` is `Rc::make_mut`, so an intern taken while
+        // any other handle is alive deep-clones the whole shared `AtomTable` (one `Arc<str>`
+        // allocation plus a hash-map insertion per atom) instead of appending in place, and the
+        // copy is dropped at once. Interning first keeps this server the table's only owner, so
+        // both appends are a plain `HashMap` hit; the interning order is unchanged.
+        let tag = self.atom("file_descriptor")?;
+        let module = self.atom("prim_file")?;
         // The shared atom table is cloned as an `Rc` handle first, so the borrow of the
         // runtime ends before `make_map` needs the context itself.
         let atoms = self.runtime.code_server.atoms_handle();
         let data = terms::make_map(self, &pairs, &atoms)?;
-        let tag = self.atom("file_descriptor")?;
-        let module = self.atom("prim_file")?;
         terms::make_tuple(self, &[tag, module, data])
     }
```

The same defect, same shape, in `native/net/nodes.rs` (2 clones **per node entry**):

```diff
@@ -97,7 +97,6 @@ impl RuntimeContext<'_> {
         let connection_id_key = self.atom("connection_id")?;
         let node_type_key = self.atom("node_type")?;
         let mut entries = self.selected_node_entries(&types)?;
-        let atoms = self.runtime.code_server.atoms_handle();
         let mut tuples: Vec<Eterm> = Vec::with_capacity(entries.len());
@@ -121,6 +120,10 @@ impl RuntimeContext<'_> {
             if info.node_type {
                 pairs.push((node_type_key, self.atom(kind.atom_name())?));
             }
+            // The handle is taken **after** this entry's interns and dropped with the map:
+            // holding it across an intern would make `CodeServer::atoms_mut`'s `Rc::make_mut`
+            // deep-clone the whole shared atom table (see `prim_file::build_descriptor`).
+            let atoms = self.runtime.code_server.atoms_handle();
             let info_map = terms::make_map(self, &pairs, &atoms)?;
```

Worth adding to `codeserver.rs::atoms_handle`'s docs: **never hold one across an intern** — an
intern with a live handle deep-clones the table, so the handle must be taken after the last intern
of the scope. `port/tests.rs::an_idle_port_round_does_not_clone_the_shared_atom_table` is the
existing template for a regression pin (it compares `Rc::as_ptr` of the table across a round; a
`prim_file` round can be pinned the same way).

## 3. The measurements (paired, `taskset -c 0`, this host)

Fixture marginal, 5 alternating rounds of (`N`=5 000, `N`=0) per side, `ns/op`:

| binary | median | min | max | all five rounds |
|---|---|---|---|---|
| base | **1 511 509** | 1 488 448 | 1 543 145 | 1 488 448 · 1 489 165 · 1 511 509 · 1 520 541 · 1 543 145 |
| patched | **36 275** | 36 108 | 37 421 | 36 108 · 36 109 · 36 275 · 37 105 · 37 421 |

Paired ratio base/patched **41.7×**; the spread inside each side is 1.1–3.7 %, far under the 41.7×.
Retired instructions (`cpu_core/instructions/u`, `N`=1 000 minus `N`=0): **14.47 M → 0.298 M per
op**, i.e. **48.5× fewer** — the two instruments agree that this is one removed mechanism, not
general noise. `tools/bench`'s own paired estimator, `run --quick --workloads file_io`, 4 rounds
alternating:

| case | base | patched | oracle (`sys`) | ratio base→patched |
|---|---|---|---|---|
| `handle_rw` | 1 500 094 / 1 497 526 / 1 507 331 / 1 514 276 ns | 42 007 / 41 478 / 41 423 / (below timer resolution) ns | 140 396–143 165 | **~36×** |
| `write_read` | 58 708 / 57 075 / 56 699 / 61 370 ns | 56 676 / 58 959 / 56 542 / 58 065 ns | 135 280–135 772 | **unchanged (the 0.73× win holds)** |

So `handle_rw`'s rust/sys ratio goes `10.8×` → **≈0.25–0.30×**: the Rust runtime becomes ~3.3–4×
*cheaper* than the oracle on the handle shape, joining `write_read` as a case it wins.
`/proc/loadavg` was 2.4–8.0 across the session; every figure above is either a paired marginal or
a paired ratio, and the only contention-sensitive one (absolute wall) is quoted as a range.

`handle_rw`'s own calibration constant in `tools/bench/src/workloads/file_io.rs`
(`COST_NS`'s `(HANDLE_RW, 1_512_000.0, 1_000)`) is now **42× too high**, which pushes the rust
marginal at `N`=1 000 down to ~42 ms — one round read `-1.9 ms … 51.4 ms` and the figure was
withheld as below timer resolution. `n_rust` for `handle_rw` should rise to ~20 000 once this
lands (a `tools/bench` change, not made here).

## 4. `file_io/ram_file` — why there was no rust row, and the patch that fixes it

The absent row is **not** a `beam_io` gap: `ram_file_drv`, its 13 opcodes and its 77/77 reply
encodings are shipped and pinned here. The runtime instead `execve`d a program called
`ram_file_drv` (`/bin/sh: line 1: exec: ram_file_drv: not found`) and then hung for 60 s, because
`ram_file.erl:383` opens the driver through the **`{spawn, Cmd}`** name form —
`erlang:open_port({spawn,"ram_file_drv"}, Opts)` — and `beam_rt`'s `name_spec` classified every
`("spawn",2)` as an OS child. The C resolves a command's **first token** against its driver list
before it forks (`erts/emulator/beam/io.c:599-628`); this tree already had that rule for the
`[exit_status]` refusal (`opens_a_driver_port`) and simply did not apply it when choosing the arm.

```diff
--- a/erts_rust/crates/beam_rt/src/port.rs
+++ b/erts_rust/crates/beam_rt/src/port.rs
@@ -3537,9 +3537,22 @@ impl RuntimePorts {
             ("spawn", 2) => {
                 let command = self.command_spec(terms::tuple_element(name, 1)?)?;
+                let command = checked_name(command)?;
+                // The C resolves a `{spawn, Cmd}`'s **first token** against its driver list
+                // *before* it forks (`io.c:599-628`), so a command naming a driver IS that
+                // driver's port. `ram_file.erl:383` opens exactly that shape
+                // (`erlang:open_port({spawn,"ram_file_drv"}, Opts)`), which is how
+                // `file:open(Name,[ram,…])` reaches the built-in `ram_file_drv`; taking the
+                // spawn arm here would fork `/bin/sh -c "exec ram_file_drv"` instead and hang
+                // the owner in `ram_file:get_response/1`'s `receive`.
+                let kind = if spawn_names_driver(&command) {
+                    PortName::SpawnDriver
+                } else {
+                    PortName::Spawn
+                };
                 Ok(NameSpec {
-                    kind: PortName::Spawn,
-                    command: checked_name(command)?,
+                    kind,
+                    command,
                     fd: None,
                     inline_options: None,
                 })
@@ -5007,6 +5020,21 @@ fn driver_path_name(command: &str) -> &str {
+fn spawn_names_driver(command: &str) -> bool {
+    let name = driver_path_name(command);
+    driver::is_static_driver(name) || driver::driver_is_loaded(name)
+}
+
@@ -5033,10 +5061,9 @@ fn opens_a_driver_port(spec: &NameSpec) -> bool {
-        PortName::Spawn => {
-            let name = driver_path_name(&spec.command);
-            driver::is_static_driver(name) || driver::driver_is_loaded(name)
-        }
+        // The same lookup [`spawn_names_driver`] performs — which now also classifies such a
+        // command as `PortName::SpawnDriver` up front, so this arm is the belt to that braces.
+        PortName::Spawn => spawn_names_driver(&spec.command),
```

Verified: with this patch the workload's own `ram_file` module runs end to end —
`file:open(Name,[ram,read,write,binary])` + write + `position` + read + close, and the fixture's
own `file:read_file_info(Name)` ⇒ `{error,enoent}` self-check passes (`rambench:ram/1 3` prints
`3`, exit 0). Two follow-ups it exposes, both `beam_rt`/`tools/bench`:

* the case is **slow**: 210 µs of user CPU and ~12.9 ms of wall per iteration on this host
  (`task-clock:u` 182 ms at `N`=0 vs 392 ms at `N`=1 000), i.e. ~13× the oracle's 15.7 µs — the
  port-command round trip costs far more than the driver's work. Not a `beam_io` figure: the
  driver's replies are staged inside its own `output` callback.
* `tools/bench`'s `ram_file` calibration (`(RAM_FILE, 52_000.0, 20_000)`) therefore cannot stay
  at `N`=20 000 — at ~12.9 ms/op that is 258 s and trips the 60 s run deadline. It needs
  `n_rust` ≈ 1 000 until the round trip is fixed.

## 5. Test evidence for the patches (applied, then reverted)

* `cargo test -p beam_rt --no-fail-fast`: the only red targets are the eight inventoried
  known-red tests in `crates/beam_rt/TEST_STRATEGY.md` — the four `src/runner/` lib tests plus
  `foreign_root_loop`, `gc_immix_payload`, `gc_strategy`, `off_heap_reclaim`. **No new red**
  (lib: 1986 passed / 4 failed, exactly the inventoried four).
* `cargo test -p beam_io --no-fail-fast`: **fully green** — whole crate 1328 passed / 0 failed /
  2 ignored over 28 binaries (lib 1074 passed / 0 failed). No red target.
* Differential, 404 `driver*`/`port*`/`file*`/`inet*`/`spawn*` cases run individually at 16-way and
  merged with `--allowlist tools/differential/known-gaps.txt`: `total 3439 / agree 3438 /
  mismatch 0 / unsupported 1 / skipped 0`, ratchet `allowed 1 / unauthorized 0 / stale 0` — the
  baseline, unchanged.

## 6. Reproduction

```
# fixture (the benchmark's own loop), compiled with the system erlc
$ERLC -o $DIR $DIR/fbench.erl            # write_read/1 handle_rw/1 …
FBENCH_DIR=$DIR perf record -e cpu_core/instructions/u -F 2999 --call-graph dwarf \
  -- beam_rt -pa $DIR run $DIR/fbench.beam fbench:handle_rw/1 200
# paired marginal: alternate (N=5000, N=0) per binary, taskset -c 0
```
Note for this host: `cpu_core/instructions:u` is **not** accepted — use `cpu_core/instructions/u`.
