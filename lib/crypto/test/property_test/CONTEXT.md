# crypto — Property Tests

## Intent
Property-based tests (QuickCheck/PropEr/Triq) for the "new" crypto API: the one-shot `crypto:crypto_one_time/4,5` and the stateful `crypto:crypto_init/3,4` → `crypto_update/2` → `crypto_final/1` → `crypto_get_data/1` flow.
Properties only assert the round-trip property `decrypt(encrypt(Text)) == Text` (modulo padding); they are not known-answer tests — vectors live in `../crypto_SUITE.erl`.
This directory is *not* listed in `../Makefile`'s `MODULES`; it is copied verbatim into `$(RELEASE_PATH)/crypto_test/property_test` by `release_tests_spec` and compiled at test time by `ct_property_test`.
Everything here is driven from `../crypto_property_test_SUITE.erl` via `ct_property_test:quickcheck/2`.

## API Surface
Flat module set — no subdirectories.
- `crypto_ng_api.erl` — tool-neutral stateless properties (`-compile(export_all)`, so no export list to maintain):
  - `prop__crypto_one_time/0` — 10 000 tests of `crypto_one_time` encrypt→decrypt round-trip.
  - `prop__crypto_init_update_final/0` — 10 000 tests of init/update/final encrypt→decrypt round-trip.
  - Both use only portable macros (`?FORALL`, `?LET`, `numtests/2`, `aggregate/2`), so they compile and run under any of the three tools.
- `crypto_ng_api_stateful.erl` — PropEr-only stateful/parallel model over `crypto_init` + `crypto_update`:
  - `prop__crypto_init_multi/1` — 300 `parallel_commands(?MODULE)` runs; the only PropEr-specific property (uses `parallel_commands`, `run_parallel_commands`, `?call`).
  - Entire body is inside `-ifdef(PROPER)`, so under QuickCheck/Triq this compiles to an empty module (and `init_update_multi` is skipped).
- `crypto_prop_generators.erl` — the shared generator library (used by both property modules):
  - `text_plain/0` (alias of `iolist/0`), `iolist/0`, `mybinary/1`, `padding/0`, `cipher/0`, `key/1`, `iv/1`.
  - Cipher-metadata helpers: `non_aead_ciphers/0`, `block_size/1`, `key_length/1`, `iv_length/1` (all read `crypto:cipher_info/1`).
  - What the generators produce: `cipher/0` = `oneof` over `crypto:supports(ciphers)` minus AEAD (excludes `chacha20_poly1305` and every CCM/GCM-mode cipher).
  - `key/1`/`iv/1` = `crypto:strong_rand_bytes(Key|IvLength)` — raw random binaries, not generated/shrinkable terms.
  - `iolist/0` = nested lists and binaries of random bytes (`?CT_BYTE()`), with 1/6 frequency an up-to-50 000-byte single binary.
  - `padding/0` = `oneof([pkcs_padding, none, zero, random, undefined])` — crypto's own padding atoms, including the crypto-specific `zero`/`random` modes.
- `crypto_prop_generators.hrl` — `-import`s the generator functions into the property modules; must be included *after* `ct_property_test.hrl` (it uses `?CT_BYTE()`-style macros).

## Constraints
- Requires at least one property tool on the code path; `ct_property_test:init_per_suite/1` probes `[eqc, proper, triq]` in that order (overridable via `{prop_tools, [...]}` in the CT `Config`) and skips the whole suite if none is found.
- Never add a `Makefile` here and never add these modules to `../Makefile`'s `MODULES`: they are compiled by `ct_property_test:compile_tests/2`, not by the OTP build.
- `ct_property_test:compile_tests/2` `cd`s here, deletes any existing `.beam`, then runs `make:all([load, {ToolMacro,true}])` with `ToolMacro` ∈ `EQC | PROPER | TRIQ`; hence the directory must contain the `.erl`/`.hrl` pair together and must exist relative to the suite's `data_dir`'s parent.
- Write properties with tool-neutral macros only, or guard tool-specific code with `-ifdef(EQC|PROPER|TRIQ)` (see `crypto_ng_api_stateful.erl`); `ct_property_test.hrl` sets `?MOD_eqc` and pulls in the selected tool's header.
- Module names must not collide with anything on the release code path, since `add_code_pathz/1` puts this directory on the path.

## Notes for Agents
- Because `ct_property_test` selects the FIRST tool found, the effective tool in a given run is environment-dependent: with QuickCheck installed the stateful property never runs; with PropEr (or only PropEr) it does.
- To iterate on a property without a full CT run, compile manually with the matching macro (e.g. `erlc -DPROPER -I. -o . crypto_ng_api_stateful.erl`) and call `proper:quickcheck/1` directly; `ct_run -suite crypto_property_test_SUITE` is the supported path.
- `crypto_ng_api.erl` logs every case via `io:format/2` at `?LINE`-tagged points and dumps non-matching results in `equal/3`; expect enormous log output when a property fails or when the 50 kB binary branch is hit.
- `prt_inf/3` builds `aggregate` labels via `ct_property_test:title/3` + `ct:pal/2` to histogram text lengths and cipher usage — these labels are for CT logging only, not assertions.

## Known Issues
- AEAD is deliberately excluded (`non_aead_ciphers/0`), so no GCM/CCM/chacha20_poly1305 path is exercised here; `crypto_one_time` with AAD is only covered in `../crypto_SUITE.erl`.
- `key/1` and `iv/1` are static random binaries ("can't be shrunk"): a counterexample reports an unreproducible random key/IV, so failures cannot be minimized.
- The 50 000-byte branch of `iolist/0` combined with `numtests(10000, ...)` makes `prop__crypto_one_time`/`prop__crypto_init_update_final` heavy in time and memory; timeouts under CT are plausible on slow/loaded machines.
- `equal/3` uses the atom `correct_exception` as a sentinel meaning "the API raised the expected `Padding 'none' but unfilled last block` error" — any other `{error,{"api_ng.c",Line},Msg}` is re-raised and fails the property.
- `prop__crypto_init_update_final` re-derives the padding size from `crypto:crypto_get_data(Cenc)` for `zero`/`random` padding but forces `PadSize = 0` for `pkcs_padding`/`none`/`undefined`, so its assumptions must be revisited if padding accounting in the API changes.
- The stateful model treats symbolic (never-executed) and dynamic states differently via the `symbolic/1` check in `next_state/3`; `postcondition/3` returns `false` for any `error` result, so a crypto error on any call fails the property rather than being expected.

## Dependencies
- External (not shipped with OTP, not a build dependency): QuickCheck (`eqc`), PropEr, or Triq; PropEr is effectively required for full coverage.
- `common_test`'s `ct_property_test`, `ct_property_test.hrl`, `ct_proper_ext`/`ct_quickcheck_ext` support modules, and the `crypto` NIF app itself.
- `crypto_info/1`/`supports/1` results determine which ciphers exist, so coverage shifts with the linked OpenSSL version.

## Test Strategy
- Run with `make test` in `../` (or `ct_run -dir .`-style invocation of `crypto_property_test_SUITE`); there is no separate build target for this directory.
- The suite is skipped (not failed) when no property tool is present — a silent skip in CI is the expected behaviour, so check the CT log to confirm the properties actually ran.
- No per-file unit tests; the properties are the only consumers of these modules.

## See Also
- `../CONTEXT.md` — the `crypto` Common Test suites (`crypto_SUITE` KAT vectors, `engine_SUITE`, benchmark suites) that drive this directory.
- `../../CONTEXT.md` — the `crypto` application (the API under test).
- `../../../common_test/src/ct_property_test.erl` — the framework that discovers the tool, compiles this directory and presents results.
