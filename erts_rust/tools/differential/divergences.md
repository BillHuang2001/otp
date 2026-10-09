# Observed runtime divergences (handoffs)

Genuine, verified differences between the Rust runtime (`beam_rt`) and the system-ERTS
oracle that the expanded corpus found. None is a capability gap the runtime signals honestly
(an `unsupported` raise / exit 3), so a case pinning one would be a `mismatch` — the gate's
one hard failure. Each is therefore **deliberately not pinned** as a corpus case; the owning
crate below must close the gap, after which a case can be added to the corpus.

## A.3 — NIF C ABI (owner: `beam_abi` / `beam_rt` NifHost)
- An atom name first interned *during* a NIF call is not readable back in the same call:
  `enif_make_atom(env, "<new>")` then `enif_get_atom` → `0` in the Rust runtime vs `1` in the
  oracle (same for `enif_get_atom_length`, `enif_make_new_atom(_len)`, `enif_make_atom_len`).
  Cause: `NifHostImpl::atom_name` (`crates/beam_rt/src/nif.rs`) reads only the committed
  `AtomTable`, not the in-flight `PendingInterner` (`intern_atom`).
- `enif_clear_env` clears a pending exception in the Rust runtime but not in the oracle
  (`crates/beam_abi/src/nif/enif/exception.rs`). (On the *process* env it is illegal C and
  SIGSEGVs the oracle.)
- Host hooks with no implementation (a case would be a plain-value mismatch):
  `enif_term_to_binary`, `enif_binary_to_term`, `enif_make_ref`, `enif_make_existing_atom`
  (`NifHost::existing_atom` is a `None` stub, `crates/beam_abi/src/nif/host.rs`),
  `enif_whereis_pid`, `enif_make_unique_integer`, `enif_system_info`.

## A.4 — driver C ABI (owner: `beam_rt` / `beam_io`; `beam_abi` for the timer units)
- `erl_drv_output_term` / `erl_drv_send_term` / `driver_output_term` / `driver_send_term` do
  not deliver their `ErlDrvTerm` spec to the receiving process (oracle `7009`, rust `-1`).
- An entry whose only port-command callback is `outputv` never receives `port_command/2`
  (oracle `49`, rust `-1`).
- `driver_async` schedules nothing on the port's pool (oracle `49`, rust `48`).
- `driver_set_timer` refusal without a `timeout` callback differs (oracle `48`, rust `49`).
- `erl_drv_monotonic_time` differs by phase (oracle `48`, rust `49`).
- `driver_enq`/`driver_sizeq` as probed hangs the *oracle* — a harness/probe issue to re-check
  before treating it as a runtime gap.

## A.5 — distribution wire / ETF (owner: `beam_rt`, `beam_engine`)
- An `-eval` integer literal ≥ 2^63 is rejected (a bignum-literal parser gap); a literal of
  2^63-1 raises `system_limit`.
- A multi-element improper list (`[1,2|3]`) fails the `-eval` parser (single-cons `[H|T]` works).
- An integer binary segment wider than 64 bits (`<<0:2048>>`) is unsupported.

## A.9 — preloaded / boot (owner: `beam_rt`, `beam_bifs`)
- `code:get_mode/0` → oracle `interactive`, rust `badarg`.
- `init:get_argument(root|home|progname)` → oracle `{ok, …}`, rust `error`.
- `application:get_application/0` → oracle `undefined`, rust `badarg`.
- `erlang:module_info(exports)` → oracle a list, rust `undef` (`init:module_info/1` works).
- `whereis(code_server|erl_prim_loader|kernel_sup)` → oracle a pid, rust `false`.
