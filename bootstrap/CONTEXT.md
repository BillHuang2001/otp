# bootstrap — Primary Bootstrap (prebuilt Erlang code under version control)

## Intent
Holds the **primary bootstrap**: prebuilt `kernel`, `stdlib` and `compiler` BEAM code plus
boot scripts that are committed to git so a source tree can be built *without* an existing
Erlang/OTP installation. The Erlang compiler and code loader are themselves written in
Erlang, so a seed compiler must be available before anything can be compiled.
Two kinds of derived code are under version control in OTP: this primary bootstrap
(`$ERL_TOP/bootstrap`) and preloaded code (`erts/preloaded/ebin`, linked into the VM).
See `../HOWTO/BOOTSTRAP.md` and `../HOWTO/DEVELOPMENT.md` ("Preloaded and Primary Bootstrap").

## API Surface (tracked contents — 285 files, ~5.5 MB)
- `bin/start.boot` — default boot script; the bootstrap `erl` wrapper uses `$ROOTDIR/bin/start.boot` (ROOTDIR = this directory).
- `bin/start_clean.boot` — byte-identical to `start.boot` (both are built from `erts/start_scripts/start_clean.rel`, whose release name is `"Erlang/OTP"`).
- `bin/no_dot_erlang.boot` — same release spec without `~/.erlang` evaluation; **required by `erlc`**, which boots the compiler with `-boot no_dot_erlang` (`erts/etc/common/erlc.c`).
- `bin/start*.script` — generated siblings of the `.boot` files; **untracked** (`.gitignore` ignores `/bootstrap/bin/*` except `*.boot`).
- `lib/kernel/{ebin,include}` — 104 beams + `kernel.app`/`kernel.appup` + 8 `.hrl` (`dist.hrl`, `file.hrl`, `inet.hrl`, `logger.hrl`, …).
- `lib/stdlib/{ebin,include}` — 99 beams + `stdlib.app`/`stdlib.appup` + 6 `.hrl` (`assert.hrl`, `qlc.hrl`, `ms_transform.hrl`, …).
- `lib/compiler/ebin` — 59 beams + `compiler.app`/`compiler.appup` (no `include/`).
- Preloaded modules (`erlang`, `init`, `erl_prim_loader`, `prim_*`, `atomics`, …) are **not** here — they live in `../erts/preloaded/ebin` and are linked into the emulator.
- Beams are compiled stripped (no `Dbgi`/debug_info chunk) and are deterministic when `ERL_DETERMINISTIC=yes` (`DETERMINISM_FLAG = +deterministic` in `../Makefile.in`).

## How the build consumes it (`../Makefile.in`, `../otp_build`)
- `BOOTSTRAP_TOP = $(ERL_TOP)/bootstrap`, `BOOTSTRAP_ROOT = $(ERL_TOP)`, and `BOOT_PREFIX = $(BOOTSTRAP_ROOT)/bootstrap/bin` is prepended to `PATH` for the entire build, so `erl`/`erlc`/`escript` resolve to the bootstrap copies.
- `bootstrap_setup` creates the **untracked** `bin/erl`, `bin/erlc`, `bin/escript`, `bin/yielding_c_fun`, `bin/$(TARGET)/inet_gethost` wrappers and the `target` marker file (recording `$(TARGET)`). The `erl` wrapper keeps `ROOTDIR=$(BOOTSTRAP_ROOT)/bootstrap` (so `lib/*/ebin` here is the code path) but points the emulator binary at the freshly built `$(ERL_TOP)/bin/$(TARGET)`.
- Bootstrap stages: **primary** = the checked-in kernel/stdlib/compiler here; **secondary** = `parsetools sasl asn1`; **tertiary** = `parsetools wx public_key erl_interface syntax_tools snmp runtime_tools xmerl common_test`. Secondary/tertiary apps are compiled from `../lib/*` and copied into `bootstrap/lib/<app>` at build time (the `$(BOOTSTRAP_ROOT)/bootstrap/lib/%/update` rule).
- `all_bootstraps` = `erl_interface emulator bootstrap_setup secondary_bootstrap_copy tertiary_bootstrap_copy`; `make all` runs `bootstrap` first.
- Recreation for prebuilt source distributions: `check_recreate_primary_bootstrap` runs `./otp_build save_bootstrap` (→ `otp_build copy_primary_bootstrap`) when `../prebuilt.files` exists and `bootstrap/lib` is missing — it copies `../lib/{kernel,stdlib,compiler}/ebin` beams, `include/*.hrl` and `../bin/*` into here. `make clean` triggers the same check so cleaning never destroys the ability to rebuild.

## Constraints (rules for this directory)
- **Never hand-edit anything here.** Regenerate with `./otp_build update_primary [--no-commit]` (commit message `Update primary bootstrap`, made only by Erlang/OTP maintainers); upstream PRs must **not** contain commits touching this directory (binary files cannot be reviewed for malicious data).
- Directory-local build lists in `../prebuild.skip`/`../prebuild.delete` treat these files as prebuilt distribution content: `bootstrap/bin` is restored from source, while `bootstrap/lib` and `bootstrap/target` are stripped from prebuilt tarballs (space) and recreated by `otp_build save_bootstrap`.
- `lib/kernel/ebin/*.beam`, `lib/stdlib/ebin/*.beam` and `lib/compiler/ebin/*.beam` are explicitly whitelisted in `../.gitignore` against the global `*.beam` rule; every other `bootstrap/lib/<app>` directory (asn1, sasl, parsetools, wx, public_key, runtime_tools, xmerl, common_test, syntax_tools, snmp, erl_interface, edoc, erl_docgen) is explicitly ignored — those are build residue, never commit them.
- `lib/*/egen`, `bootstrap/target`, `bootstrap/primary_compiler/` and `bin/*` (except `*.boot`) are build artifacts and ignored.

## Known Issues / Gotchas
- The `update_primary` flow (`git add` then `find bootstrap -name egen -o -name '*.script' -o -name '*.app' -o -name '*.appup' | xargs git reset HEAD`) deliberately **excludes** `.app`, `.appup`, `.script` and `egen` files from the bootstrap commit — they arrive via maint merges/`primary_bootstrap_copy` instead, so version numbers in `lib/*/ebin/*.app` can lag the current release (e.g. `kernel.app` may still say `11.0.3` while the tree is at OTP 30.0-rc0). Never read app versions here as authoritative.
- `make bootstrap_clean` **deletes** the tracked beams, `.app` files, `.hrl` files and `bin/*.boot` (and also `lib/kernel/ebin/{erl_init,init,erl_prim_loader,prim_inet,prim_file,zlib,prim_zip,erlang,erts_code_purger}.beam`). An interrupted bootstrap leaves the tree dirty; restore with git (`git checkout -- bootstrap`) — `otp_build save_bootstrap` only works from a built tree with `prebuilt.files`.
- `bootstrap/lib/stdlib/ebin/otp_internal.beam` is regenerated separately by `./otp_build update_deprecations` (which commits it together with `lib/stdlib/src/otp_internal.erl`).
- `.ort.yml` (license scanner) carries `NOT_DETECTED`/`CODE` exceptions for `bin/*.boot` and a few beams (`beam_opcodes.beam`, `erl_id_trans.beam`, `erl_parse.beam`) because binaries cannot hold license comments; `../scripts/license-header.es` skips `*.boot`, `*.beam` and `prebuild.(skip|keep|delete)`.
- `TAR.include` at the repo root lists `bootstrap/lib/*/ebin`, `bootstrap/lib/*/include`, `bootstrap/bin/start*.script`, `bootstrap/bin/start*.boot`, but no script in this repository consumes it today — it is legacy documentation of tarball content.

## Notes for Agents
- Only `lib/{compiler,kernel,stdlib}/{ebin,include}` and `bin/*.boot` are meaningful; everything else in this subtree is generated (secondary/tertiary app copies, `erl`/`erlc` wrappers, `target`, `primary_compiler`) and must not be committed or "fixed".
- There is nothing to test here: correctness is verified by `./otp_build update_primary --no-commit` producing **no diff** when the checked-in bootstrap already matches `../lib/*/src` (per `../HOWTO/INSTALL.md`).
- CI: `.github/workflows/reusable-pack.yaml` treats any change under `bootstrap/**` as cache-invalidating (`BOOTSTRAP=true`), forcing a fresh secondary/tertiary bootstrap and dropping the cached `.beam` files.
- Editing anything in this directory is almost never the right fix for a build failure; escalate to the relevant `../lib/<app>` source instead.

## See Also
- `../HOWTO/BOOTSTRAP.md` — primary vs. preloaded derived code, how bootstrap updates are performed.
- `../HOWTO/DEVELOPMENT.md` (`### Preloaded and Primary Bootstrap`) — `otp_build update_primary` / `update_preloaded`.
- `../erts/preloaded/ebin` — the other version-controlled derived code (linked into the VM).
- `../erts/start_scripts/` — `.rel` sources and the `bootstrap_scripts` target that (re)generates this directory's `bin/*.boot`.
- `../Makefile.in` (`bootstrap*` targets), `../otp_build` (`update_primary`, `save_bootstrap`, `copy_primary_bootstrap`).
- `../lib/{kernel,stdlib,compiler}/src` — the sources these beams are built from.

## Status
Complete.
