# asn1 — ASN.1 Compiler

## Intent
The `asn1` OTP application: a compiler that takes ASN.1 module descriptions (`.asn1`, `.asn`, or legacy `.py`) and generates Erlang encoder/decoder modules.
Supports BER, DER (BER variant), aligned PER, unaligned PER (UPER), and (experimental) JER.
Used at build time by other apps (e.g. `public_key`, `eldap`) to generate codec modules, and can also be used interactively from the shell.
`ASN1_VSN = 5.5.2` (see `vsn.mk`); it is a pure-Erlang app except for one optional runtime NIF.

## API Surface
- `asn1ct` (`src/asn1ct.erl`) — the compiler's public entry point.
  - `compile/1`, `compile/2` — compile `.asn1`/`.asn`/`.py` (or a `.set.asn1` multi-file set) to an Erlang module; passes unrecognized options to `erlc`.
  - `test/1..3`, `value/2,3` — encode/decode a type/value interactively (dev-time helpers).
  - Compile options: `ber | per | uper | jer | der`, `maps`, `compact_bit_string`, `legacy_bit_string`, `legacy_erlang_types`, `{n2n,_}`, `noobj`, `{i,Dir}`, `{outdir,Dir}`, `asn1config`, `undec_rest`, `no_ok_wrapper`, `{macro_name_prefix,_}`, `{record_name_prefix,_}`, `verbose`, `warnings_as_errors`, `deterministic`.
  - Generated files per module: `<Mod>.erl` (encode/decode/value functions), `<Mod>.hrl` (when `SET`/`SEQUENCE` exist and `maps` off), `<Mod>.asn1db` (intermediate form for cross-module `IMPORT`).
- `asn1rt_nif` (`src/asn1rt_nif.erl`) — optional runtime NIF wrapper: `encode_per_complete/1`, `decode_ber_tlv/1`, `encode_ber_tlv/1`; loads `priv/lib/asn1rt_nif.{so,dll,a}` via `on_load`.
- Generated codecs (not shipped in this app) call back into the `asn1rtt_*` runtime-template helpers, which are the only encode/decode runtime in Erlang (see below).
- `asn1rt` is a stub module kept solely for `-removed` deprecation warnings; it has no live API.
- `asn1_app` (`start/2`, `stop/1`) is a minimal application callback.

## The `.asn1config` mechanism
- Passing option `asn1config` makes the compiler read a config file with the same base name as the spec plus `.asn1config`.
- It drives *specialized decodes*: exclusive decode and selective decode (which parts of a value to decode), plus link/table ("InfoObject") constraints; see the User's Guide (`doc/guides/asn1_spec.md`).
- The generated module then also exports the specialized decode functions; `asn1ct_partial_decode` and the `*_decode_rest`/`selective_decode` machinery implement this.

## Dependencies
- `asn1.app.src` declares `{applications, [kernel, stdlib]}` and `{runtime_dependencies, ["stdlib-5.0","kernel-9.0","erts-14.0"]}`.
- The only NIF/runtime external dependency is the C library built from `c_src/` (no OpenSSL etc.); JER uses STDLIB `json` (OTP 27+) rather than an external JSON lib.

## Constraints
- Never edit `src/asn1ct_rtt.erl` or `src/asn1ct_eval_ext.erl` by hand: `Makefile` generates them from the `asn1rtt_*.erl` templates and `asn1ct_eval_ext.funcs` via `prepare_templates.erl` (rule `asn1ct_rtt.erl:`/`asn1ct_eval_%.erl:`). Edit the template/`.funcs` sources instead.
- `src/asn1_records.hrl` is an internal record header (`-include("asn1_records.hrl")`), not a published include; there is no `include/` directory in this app (release intentionally installs no public headers).
- Generated/derived artifacts are build output: `ebin/*`, `asn1ct_rtt.erl`, `asn1ct_eval_ext.erl`. `src/Makefile` cleans them.
- `src/Makefile` compiles with `-Werror`; `asn1ct.erl` is compiled with `-Dvsn="$(VSN)"`.
- The NIF module name must equal the shared-object name (`asn1rt_nif`) for static-NIF linking; the .so is installed to `$(RELSYSDIR)/priv/lib`.
- Keep `src/Restrictions.txt` in mind — the ASN.1 2002 standard is only partially supported (BER/DER/aligned PER/unaligned PER).
- Rebuild templates when changing an `asn1rtt_*.erl` template: the generated `asn1ct_rtt.erl` embeds them into emitted code.

## Design Decisions
- Compiler is a pipeline of `asn1ct_tok` → `asn1ct_parser2` (parse) → `asn1ct_check` (semantic check, `.asn1db` store/load via `asn1_db`/`asn1ct_table`) → rule-specific generator (`asn1ct_gen` dispatches to `asn1ct_gen_ber_bin_v2` / `asn1ct_gen_per` / `asn1ct_gen_jer`).
- Per-encoder logic for the generated code lives in *templates* (`asn1rtt_ber`, `asn1rtt_per`, `asn1rtt_uper`, `asn1rtt_jer`, `asn1rtt_check`, `asn1rtt_ext`, `asn1rtt_*_common`) that are compiled then folded into a single generated `asn1ct_rtt.erl`; `asn1ct_imm` emits in-memory code fragments.
- BER encode/decode helper bodies are text-substituted from `asn1ct_eval_ext.funcs` (`asn1ct_func`).
- Runtime NIF exists for performance-critical PER completion and BER TLV handling; the rest of encode/decode is pure Erlang.
- `{maps,...}` representation is opt-in; default is records + `.hrl`.

## Notes for Agents
- `asn1ct_check.erl` (~6k lines) is the largest module — semantic validation; `asn1ct_imm.erl` (~2.9k) builds emitted code.
- `examples/` holds standalone `.asn` specs and helper text (`encode_decode_formats.txt`, `recordnames.txt`), plus `test_driver.erl`.
- `notes.html` (repo-tracked) and `doc/notes.md` are release notes; `src/notes_history.sgml` / `src/notes_latest.sgml` are their SGML sources.
- `priv/bin/` and `release/` contain only placeholder `.gitignore` files (no tracked content); the real runtime artifact is `priv/lib/asn1rt_nif.*`.

## Test Strategy
- Common Test suites live in `test/`; `make test` from this dir runs them (`TEST_NEEDS_RELEASE` not set — tests run in-place against `../ebin`).
- Entry suites: `asn1_SUITE` (the big functional suite), `syntax_SUITE` (parser/tokenizer/error cases, parallel group), `error_SUITE`, `asn1_app_SUITE`.
- Coverage is organized as one `test*/` helper module per feature (type/rule), compiled into `test/` per `test/Makefile` `MODULES`; `asn1_SUITE` drives them across encoding rules (`per, uper, ber, jer`, groups `{ber,[ber,jer]}`).
- `asn1_SUITE` includes a `performance` group (`testTimer_ber/per/uper` and `_maps` variants) and heavy system tests (`testTcapsystem`, `testNBAPsystem`, `testS1AP`, `testRfcs`).
- `test/asn1_SUITE_data/` (~239 files) holds `.asn1`/`.asn` inputs, `.asn1config` files, and golden `.py`/`.ber` fixtures used by the suites.
- `test/asn1.spec` and `test/asn1.cover` are the test spec and coverage config; the whole `test/` tree is installed as `asn1_test` in a release.

## Routing Table
| Path | Description |
| --- | --- |
| `src/` | Erlang compiler sources, templates, and app metadata (see below) |
| `src/asn1ct.erl` | Public compiler API + compile-option handling |
| `src/asn1ct_tok.erl`, `asn1ct_parser2.erl`, `asn1ct_pretty_format.erl` | Lexer, parser, and pretty-printer for ASN.1 syntax |
| `src/asn1ct_check.erl`, `asn1t_check`-family (`asn1ct_gen_check.erl`) | Semantic checker and generated-code check helpers |
| `src/asn1ct_gen.erl` + `asn1ct_gen_ber_bin_v2.erl` / `_per.erl` / `_jer.erl` | Code generators per encoding rule |
| `src/asn1ct_imm.erl`, `asn1ct_constructed_ber_bin_v2.erl`, `asn1ct_constructed_per.erl` | Emitted-code construction for BER / PER |
| `src/asn1rtt_*.erl`, `asn1ct_rtt.erl`, `prepare_templates.erl`, `asn1ct_eval_ext.funcs`, `asn1ct_func.erl` | Runtime templates + code-injection plumbing (generated artifacts) |
| `src/asn1rt_nif.erl`, `../lib/asn1/c_src/` | Optional runtime NIF (Erlang wrapper + C source) |
| `src/asn1_db.erl`, `asn1ct_table.erl` | `.asn1db` intermediate-form store/load |
| `src/asn1ct_partial_decode.erl`, `asn1ct_value.erl`, `asn1ct_name.erl` | Specialized decode, value handling, naming helpers |
| `c_src/` | `asn1_erl_nif.c` — the runtime NIF; builds `asn1rt_nif.so/.dll/.a` |
| `test/` | Common Test suites + `test*/` helpers + `asn1_SUITE_data/` fixtures |
| `doc/` | Documentation: `docs.exs`, `guides/` (intro/overview/getting-started/spec), `notes.md`, legacy `users_guide/`, `src/GUI.asn1[config]` example |
| `examples/` | Standalone `.asn` specs and notes for manual use |
| `priv/` | `priv/lib/` holds the built NIF; `priv/bin/` is a placeholder |
| `release/` | Placeholder (`.gitignore` only) |
| `ebin/` | Build output (ignored) |
| `vsn.mk`, `Makefile`, `AUTHORS`, `notes.html` | App version, build driver, attribution, release notes |

## See Also
- `../../lib/public_key/asn1/` and `../../lib/eldap/` — build-time consumers of the compiler.
- `../CONTEXT.md` — OTP application build conventions; repo-root `CONTEXT.md` for top-level build.
- `doc/guides/asn1_spec.md` — authoritative description of `.asn1config`, exclusive/selective decode, and InfoObject constraints.
