# compiler/scripts — Compiler Smoke Test

## Intent
Manual, release-time end-to-end smoke test of the Erlang compiler: prove the freshly built compiler can build large real-world Erlang projects (Elixir itself and a set of Mix/hex dependencies) without crashing, catching regressions that the Common Test suites miss.
Not part of `make test` / Common Test / CI — it is run by hand during compiler development and before a release (originally added with "we don't intend to use `smoke` in our daily builds").

## API Surface
- `smoke` — executable escript (`#!/usr/bin/env escript`, `-mode(compile)`), the whole test driver; `main/1` runs `setup → clone_elixir → build_elixir → test_elixir → setup_mix → smoke(main) → smoke(rabbitmq)`.
  - `scripts_dir/0` locates this directory via `code:lib_dir(compiler) ++ "/scripts"`, so the escript must be run with an Erlang whose `compiler` lib dir is this source tree (use `$ERL_TOP/bin/escript` / `$ERL_TOP/bin` first on `PATH` to exercise the newly built compiler).
  - `run/2` spawns commands through `open_port({spawn_executable,…})`, echoes output and aborts (`halt(1)`) on any non-zero exit code — the first failing step terminates the run.
- `smoke-mix.exs` — Mix project definition (`Smoke.MixProject`, `app: :smoke`) copied verbatim into the scratch dir as `mix.exs`; its `deps/0` selects the dependency list from the `SMOKE_DEPS_SET` env var (`main` = ~35 hex/git packages incl. cloudi, cowboy, ecto, phoenix, lager, rabbit; `rabbitmq` = `rabbit_common`; default = no deps).
  - Custom `compile:` hooks `build_scalaris/0` (erlc the Erlang sources only) and `build_wings/0` (stub out `c_src/` and `plugins_src/accel/` Makefiles, git commit/tag, then `make`) are needed because those packages otherwise try to build native code.
- `smoke-build/` — scratch/working directory, created if missing and used as cwd for all steps; holds the `elixir/` clone, generated `mix.exs`, `deps/`, `_build/`.
  - Only `smoke-build/mix.lock` is tracked by git (see `.gitignore`); it records the resolved versions of the `rabbitmq` dependency set.
- `.gitignore` — ignores everything under `smoke-build/` except `mix.lock`.

## How It Is Invoked
- Run the escript directly (`lib/compiler/scripts/smoke`); it takes no arguments and uses the current environment/`PATH`.
- Steps: create/enter `smoke-build/`, `git clone git@github.com:elixir-lang/elixir.git` (or `git pull --ff-only origin main` + `make clean` if the clone already exists and the HEAD moved), `make compile`, `make test_stdlib` in Elixir, install `local.hex` and `local.rebar` with the freshly built Elixir's `bin/` prepended to `PATH`, then per dependency set export `SMOKE_DEPS_SET`, delete `mix.lock`, `touch mix.exs`, `mix deps.clean --all`, `mix deps.get`, `mix deps.compile`.
- Only deps are fetched and compiled; the Mix projects' own test suites are not run (`mix test` is never invoked). Elixir's `test_stdlib` is the only suite executed.

## Dependencies
- Network access: GitHub over SSH (`git@github.com:elixir-lang/elixir.git` — requires a GitHub SSH key) plus hex.pm for `mix deps.get`.
- A working `git`, `make`, and a C toolchain for building Elixir and the deps; `mix`/`elixir` must be obtainable from the clone (they are not preinstalled).
- Erlang: to be meaningful, run with the just-built OTP (`$ERL_TOP/bin` on `PATH`), otherwise the system-installed compiler is exercised instead.

## Known Issues
- `smoke/1` deletes `mix.lock` before `mix deps.get`, so the committed `smoke-build/mix.lock` does not pin versions during a run — deps are re-resolved freshly (and the checked-in lock is overwritten in the working tree).
- Step `clone_elixir/0` runs against Elixir `main`, so the smoke test picks up Elixir changes and can fail for reasons unrelated to the Erlang compiler.
- The Elixir deps set (`main`) is pinned to old version requirements, so it can rot as hex packages are yanked/updated.

## Notes for Agents
- `smoke-mix.exs` is matched by the repo-root `.formatter.exs` (`**/*.exs`), so it must stay `mix format`-clean or the tree-wide format check fails.
- `scripts/` is not in `lib/compiler/Makefile`'s `SUB_DIRECTORIES`, so it is neither built nor installed — it is source-tree-only tooling.
- Nothing in CI, `Makefile.in`, or `lib/compiler/Makefile` invokes `smoke`; the `.github/scripts/otp-compliance.es` reference to `smoke-build` only excludes that directory from `.app.src` scanning.
- `HOWTO/INSTALL.md`'s "smoke test" (`erl -s ts install -s ts smoke_test batch`) is a different mechanism (test_server subset), not this script.

## See Also
- `../CONTEXT.md` — the `compiler` application (build, sources, Common Test suites).
- `../test/` — the Common Test suites that are the normal regression coverage.
