# Common Test — Public Headers

## Intent
Public Erlang headers exported by the `common_test` application to test suites and other applications.
These are the only headers of the app installed into a release (`lib/common_test-<vsn>/include`); sources under `../src/` may not be included by outsiders.

## API Surface
- `ct.hrl` — the suite-facing header: log-importance levels (`?MIN_IMPORTANCE`…`?MAX_IMPORTANCE`), verbosity thresholds (`?MIN_VERBOSITY`…`?MAX_VERBOSITY`), hook process names (`?CT_HOOK_INIT_PROCESS`, `?CT_HOOK_TERMINATE_PROCESS` = `ct_util_server`), and the peer-node macros `?CT_PEER_NAME/0,1`, `?CT_PEER/0,1`, `?CT_PEER_REL/3` (all delegate to `test_server:peer_name/2` and `test_server:start_peer/…`).
- `ct.hrl` also keeps the deprecated Test Server back-compat shims `?line` (no-op) and `?config` (= `test_server:lookup_config`) — do not use in new suites.
- `ct_event.hrl` — the single `#event{name, node, data}` record used by CT's event manager (`ct_event`) and user event handlers.
- `ct_property_test.hrl` — property-testing macros (`?CT_BYTE`, `?CT_NEG_INT`, `?CT_RANGE`, `?CT_INT`, `?CT_SAFE_*`, `?CT_FUNCTION0..4`, `?CT_EXISTING_ATOM`); dispatches on the compile-time defines `EQC` / `PROPER` / `TRIQ` (falling back to `eqc` naming) and includes `eqc`/`proper`/`triq` headers from those tools.
- `cello/` — empty placeholder directory (only a `.gitignore`); nothing in the repo references it.

## Constraints
- These headers are part of the public API — changes are user-visible and must stay backward compatible.
- Macros in `ct.hrl` refer to `test_server` functions at run time, so including it implies a CT run; it is not usable outside Common Test.
- `ct_property_test.hrl` requires exactly one of the property-test tools to be present/defined at compile time; it is not self-contained.

## Notes for Agents
- The CT-internal headers live elsewhere: `../src/ct_util.hrl` (`#testspec{}`, `#conn{}`, `#conn_log{}`), `../src/test_server_internal.hrl`, `../src/ct_netconfc.hrl` — never installed, do not expose them.
- The build adds this dir via `-I../include` from `../src/Makefile`, `../test_server/Makefile` and `../proper_ext/Makefile`.

## See Also
- `../src/` — module implementations (read-only sibling; escalate writes to `../`).
- `../proper_ext/` — implementation of the `ct_proper_ext`/`ct_quickcheck_ext` helpers used by `ct_property_test.hrl` (read-only sibling).
- `../doc/` — the user guide documents these macros (read-only sibling).

## Routing Table
Flat directory, no subdirectories (except the empty `cello/`).
- `ct.hrl` → public suite macros (peer nodes, importance/verbosity, legacy shims)
- `ct_event.hrl` → `#event{}` record for CT event handlers
- `ct_property_test.hrl` → property-testing macros (EQC/PropEr/Triq)
