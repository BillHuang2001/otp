# .devcontainer — VS Code Dev Container / Codespaces environment for Erlang/OTP

## Intent
Declares the cloud/remote development environment used to build and test Erlang/OTP in a pre-provisioned container instead of installing the toolchain locally.
It is the VS Code Dev Containers counterpart of the Gitpod config in the repo root's `../.gitpod.yml`; both reuse the same Dockerfile.
Support is explicitly labelled "very early … a bit unstable" in `../HOWTO/DEVELOPMENT.md`.

## API Surface
Exactly one tracked file: `devcontainer.json`. Key settings:
- `build.dockerfile` = `../.github/dockerfiles/Dockerfile.ubuntu-base`, `build.context` = `../.github` (both paths are resolved relative to this directory, so they land in `.github/`).
- `build.args`: `BASE=mcr.microsoft.com/vscode/devcontainers/base:focal`, `USER=vscode`, `GROUP=vscode` — these override the Dockerfile defaults (`gitpod/workspace-full` / `gitpod`) so the image is a Dev Container image with a `vscode` user.
- `runArgs`: `--cap-add=SYS_PTRACE` (gdb on the runtime) and `--security-opt seccomp=unconfined`.
- `remoteUser` = `vscode`; `settings` = `{}`; `extensions` = `ms-vscode.cpptools`, `erlang-ls.erlang-ls` (the latter reads the repo-root `../erlang_ls.config`).
- `forwardPorts` and `postCreateCommand` are commented out — the container starts idle, nothing builds OTP automatically.

## Image contents inherited from the base Dockerfile
The image is built from `.github/dockerfiles/Dockerfile.ubuntu-base` with a Microsoft devcontainer base.
It therefore ships the full OTP build+test toolchain: gcc/build-essential, autoconf, clang-format, flex, pkg-config, OpenJDK 8 & 11 (Java 8 default), wx 3.0 GTK3, unixodbc + postgresql (`odbctest` user), sshd, apache2, Xvfb, gdb, tmux 3.3a, plus `kerl`-installed earlier Erlang releases, Elixir/hex, rebar3 and proper.
Notable env: `MAKEFLAGS=-j4` (build arg), `ERLC_USE_SERVER=yes`, `OTP_STRICT_INSTALL=yes`, `ERL_LIBS=/buildroot/proper`, `LANG/LC_ALL=en_US.UTF-8`.
See `../.github/dockerfiles/CONTEXT.md` for the full per-Dockerfile contract — do not duplicate it here.

## Constraints
- Never edit this file's referenced Dockerfile from here; changes to the image belong in `../.github/dockerfiles/` (which CI also uses — a change affects GitHub Actions jobs too).
- Keep the container configuration consistent with `../.gitpod.yml`; both point at the same Dockerfile and are documented together in `../HOWTO/DEVELOPMENT.md`.
- No build artifacts, scripts or setup steps live in this directory — it is configuration only.

## Design Decisions
- Reusing `Dockerfile.ubuntu-base` via `build.args` instead of a dedicated devcontainer Dockerfile keeps Gitpod, Dev Container and CI images on one toolchain definition.
- `seccomp=unconfined` + `SYS_PTRACE` are needed for the emulator's gdb/debugging and OS-level tests inside the container.
- `erlang-ls` is the only language-server extension, matching the checked-in `../erlang_ls.config` (app dirs `lib/*`, `erts/preloaded`).

## Known Issues
- `settings` and `extensions` are top-level keys in this file; modern Dev Containers expects them under `customizations.vscode`. The file still works with current VS Code but may need migrating.
- Because `USER=vscode` overrides the Dockerfile's `gitpod`/`otptest` user, the container init script `.github/dockerfiles/.profile` (start sshd/postgres/Xvfb, prepend kerl Erlangs to `PATH`) is copied to `/home/otptest/.profile` and never runs for the devcontainer user. Test suites needing sshd, PostgreSQL ODBC or `DISPLAY=:99` require starting those daemons manually.
- The Dockerfile's `WORKDIR /buildroot/` and `ENTRYPOINT` do not apply in Dev Container mode: VS Code mounts the checkout under `/workspaces/otp` and overrides the entrypoint, so all builds happen in the mounted workspace.
- `HOWTO/DEVELOPMENT.md` warns that the VS Code dev container is very slow on macOS due to filesystem limitations; Gitpod or a local build is preferred there.

## Notes for Agents
- This directory is not referenced by any build, test or CI workflow; only VS Code/Codespaces reads it. Changes here cannot break `make` or CI.
- Baseline OTP version is `30.0-rc0` (see root `../OTP_VERSION`); nothing in this directory pins an OTP version.

## Test Strategy
No automated tests — validate by opening the repo in VS Code with the Dev Containers extension or via `https://gitpod.io/#https://github.com/erlang/otp`.
Inside the container the normal workflow applies: `./otp_build configure` then `make` / `make <app>_test` (see `../HOWTO/DEVELOPMENT.md`, `../HOWTO/INSTALL.md`).

## See Also
- `../.gitpod.yml` — Gitpod variant using the same Dockerfile; documents the identical VS Code extension set.
- `../.github/dockerfiles/` — Dockerfile and config files that define this image (shared with CI).
- `../.github/CONTEXT.md`, `../.github/actions/` — CI images/jobs built from the same base.
- `../HOWTO/DEVELOPMENT.md` (sections "Using Docker", "Gitpod.io or VSCode dev container") and `../HOWTO/INSTALL.md` — local build/test commands used inside the container.
- `../erlang_ls.config` — Erlang LS project configuration consumed by the `erlang-ls` extension.
