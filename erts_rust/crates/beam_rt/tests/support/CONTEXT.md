# `beam_rt/tests/support/` — the shared integration-suite harness

## Intent
The plumbing every `beam_rt` integration suite shares: the toolchain probe, the bounded oracle
probe, the loud-skip helpers, and the two live-node pair harnesses.
Each suite pulls it in with `mod support;`; `nif_a3.rs` is the one module that is instead pulled
in by `#[path = "support/nif_a3.rs"] mod nif_a3;` from the three `nif_a3_*` suites.
Nothing here is a test — this directory carries no assertions of its own.

## API Surface
### `mod.rs` — the probes and skip helpers
* `resolve_oracle` / `find_on_path` / `is_executable` — toolchain discovery is a **filesystem
  question** (`$ERTS_ORACLE`, else `erl` on `$PATH`) and never *runs* the program: `erl -noshell`
  with nothing to terminate it is a long-running node, so running it as a probe hangs for ever,
  and cargo runs one binary's tests in parallel threads, so a single hanging probe would hang
  every test in that binary.
* `skip_loudly` / `require_oracle` — a missing toolchain is a **loud skip** plus a `return`,
  never a silent pass; `require_oracle` is `resolve_oracle` + the skip.
* `run_bounded` / `run_bounded_with_env` — spawn, wait with `PROBE_TIMEOUT` (30 s), then kill and
  reap a child that outlives it.
* `oracle_eval` — the standard probe: `erl -noshell [-pa DIR] -eval EXPR -s init stop`, stdout
  trimmed, `/dev/null` stdin, bounded and reaped.
* `assert_badarg`, `check_gc_payload` — small shared assertions.
* `PROBE_TIMEOUT = 30 s`.
### `dist.rs` — the two-live-VM harness (contract A.5)
`Harness` (the one home: private `epmd`, scratch dir, child spawning, `compile_fixture`,
`child_env`, `report_registration`), `PeerKind::{Oracle, Rust}`, `ChildVm`, `PrivateEpmd`,
`Scratch`, `tool`, `local_hostname`, `unique`/`unique_cookie`, and the timeouts
(`EPMD_START_TIMEOUT` 10 s, `READY_TIMEOUT`/`LINE_TIMEOUT` 30 s, `EXIT_TIMEOUT` 60 s,
`KILL_GRACE` 3 s, `POLL` 20 ms).
* The two peer routes — the one place they differ is `spawn_actor`:
  `PeerKind::Oracle` runs `erl -sname N -setcookie C -noshell -pa <scratch> -run <mod> <actor> "<node>" -s init stop`;
  `PeerKind::Rust` runs `beam_rt -noshell -sname N -setcookie C -pa <scratch> run <fixture.beam> <mod>:<actor>/1 "<node>"`.
  `erl -run` wraps its argv in a list (`Mod:Func(["other"])`), while `beam_rt run` hands a
  double-quoted argument as the charlist itself, so the fixture's actors normalize with `arg/1`
  and `spawn_actor` passes the bare node text on the `erl` route and the quoted form on the
  runtime's.
* **Isolation**: an Erlang VM signals its process group (`SIGUSR1`, `erl_child_setup` on the way
  down), so every child (private `epmd`, peer, runtime) starts as `setsid -w <program> …` —
  its own session and process group; `ChildVm::drop` sends `TERM` to the group, waits the grace,
  escalates to `KILL` and reaps, so no `beam_rt`/`erl`/`epmd` is ever left behind. The group id is
  read back from `/proc/<pid>/stat` and only trusted when it really is the pid.
* **A private `epmd`** (`ERL_EPMD_PORT=<port> epmd -daemon -port <port>`, cwd inside the scratch
  dir) keeps the run out of any other node's way; a daemonized `epmd` detaches into its own
  session, so `PrivateEpmd::stop` finds it by its own argv (`-port <port>`), signals that pid and
  proves the port is closed.
* **Drop order is load-bearing twice**: `Harness` declares `epmd_daemon` before `scratch` (the
  daemon's cwd is that directory), and a suite holding VMs next to a `Harness` must declare the
  `Harness` **last** (the VMs' cwd and `-pa` are that same directory).
* **Skip policy**: every precondition is a loud skip — a missing `erl`/`erlc`/`epmd`/`setsid`/`kill`
  (filesystem only), a private `epmd` that does not come up, or a runtime that never prints its
  `READY` line inside the bound.
### `pair.rs` — one responder/actor session
`Pair::start` runs this runtime as the *responder* (`run` action) and a peer as the *actor*, with
every claim a line one VM printed about the other; `finish`/`diagnostics` close and report.
`external_pid_text`/`external_ref_text`/`canonical_ref_text` exist for the **oracle** suite only
(an `erl` peer renders a remote identifier as `<node.number.serial>`, derived from its node table);
a `beam_rt` peer cannot render one at all, so the rust↔rust suite never calls them.
Declared in drop order `runtime`, `actor`, `harness`.
### `two_vm.rs` — the payload-matrix harness
The second pair harness, driven by `dist_payload_matrix.rs` (payloads far larger than a receiver's
nursery). Same `setsid`/private-`epmd`/group-kill isolation, same skip policy, `Scratch`, `Harness`,
`tool`, `unique`/`unique_cookie`.
### `fixture.rs` — the shared two-VM Erlang module
`FIXTURE_ERL` (module `two_vm`) is one source run by either peer kind, compiled at test time into
`<scratch>/two_vm.beam` (never committed): a numbered-step responder/actor protocol — sections 1–3
the peers-observe-us and remote-victim families (`dist_two_vm.rs`), 4 the rust↔rust family
(identity asserted by `=:=` and pattern match only), 5 the inbound-signal family where the peer
links/monitors/exits and the claim is what *our* process received. `peer_drives_links/1`'s barrier
answers a `process_info(Victim, links)` **shape**, because no VM can render those pids.
### `nif_a3.rs` — the C-NIF transcript harness
`toolchain` (banner-gated `cc` + `erl` probe), `build` (compiles the suite's C NIF and its `.erl`
into a scratch dir), `Fixture::rust_transcript`/`oracle_transcript` → `Observed`
(`parse`/`get`/`compared_keys`/`print_notes`), `Expect` + `check` (per-key comparison of the two
transcripts).

## Constraints
* Every fixture here or in a suite is compiled at test time into a **per-case scratch directory**
  that is the only `-pa` either runtime gets; `tests/fixtures/` is never one.
* No module here asserts on its own; a failing precondition is a loud skip, never a silent pass
  and never a panic on a missing toolchain.
* Files stay under the crate-wide size discipline; keep prose leading each item (one sentence per
  line) rather than long paragraph narrations.

## Test Strategy
This directory is exercised transitively: `dist_two_vm.rs`/`dist_two_rust.rs`/
`dist_two_vm_wire.rs` through `dist.rs`+`pair.rs`+`fixture.rs`, `dist_payload_matrix.rs` through
`two_vm.rs`, the three `nif_a3_*` suites through `nif_a3.rs`, and every oracle-comparing suite
through `mod.rs`'s `require_oracle`/`oracle_eval`/`run_bounded`.
A change here is validated by the pair suites serially
(`… -p beam_rt --test dist_two_vm --test dist_two_rust --test dist_two_vm_wire -- --ignored --test-threads=1`).

## Known Issues
* `two_vm.rs`'s module docs name `dist_two_vm.rs` as a consumer; the only importer is
  `dist_payload_matrix.rs` (the main `tests/CONTEXT.md` records the actual routing).

## See Also
* [`../CONTEXT.md`](../CONTEXT.md) → the suite roster, the cross-cutting conventions, the
  two-VM fixture invariants (deadline-bounded receives, the spawned victim, "our side never
  speaks first", the plain `DOP_MONITOR_P_EXIT` form).
* [`../fixtures/`](../fixtures/) own README/recipe docs → the committed fixtures.
