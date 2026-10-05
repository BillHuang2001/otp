# SASL Examples

## Intent
Small worked example shipped with the `sasl` application: a documented module showing how to create and install a target (release) system.
Built as part of the app — `../Makefile` sets `SUB_DIRECTORIES = src doc examples/src`.

## API Surface
- `src/target_system.erl` — `target_system:create/1,2` and `install/2`; the canonical "how to build a target system" sample (also listed verbatim in System Documentation, `system/doc/system_principles/create_target.md`, section "Listing of target_system.erl"). Compiled to `ebin/target_system.beam`.
- `src/Makefile` — compiles `MODULES = target_system` (flags `+warn_unused_vars +nowarn_missing_spec_documented`); `release_docs_spec` installs `src/` + `ebin/` into `$(RELEASE_PATH)/lib/sasl-$(VSN)/doc/examples/`.
- `ebin/.gitignore` — empty placeholder that keeps the build-output directory tracked.

## Constraints
- The example is NOT part of `make release`; it is installed only by `release_docs`, under `doc/examples`.
- `target_system.erl` is duplicated as a code listing in the System doc — keep both copies in sync.

## Routing Table / See Also
- `src/` → the example module and its build/install makefile
- `../doc/` → SASL application documentation (sibling — read-only, escalate writes to parent)
- `../../system/doc/system_principles/create_target.md` → consumer that reproduces `target_system.erl`
