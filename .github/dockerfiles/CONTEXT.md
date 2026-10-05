# .github/dockerfiles — CI Docker images for building/testing Erlang/OTP

## Intent
Dockerfiles and config files that build the container images in which every Linux
GitHub Actions build/test/analysis job runs (`docker run otp ...`).
Images fall into two tiers: **base images** (toolchain + OS deps, built/pushed to
`ghcr.io/erlang/otp/*` by a scheduled workflow) and **derived images** (copy in a
prebuilt OTP `otp.tar.gz`, configure/build/install Erlang/OTP).

## Base image hierarchy & provenance
Building blocks pulled from public registries:

- `Dockerfile.ubuntu-base` — default `ARG BASE=gitpod/workspace-full`, overridden to
  `ubuntu:22.04` by `build-base-image.sh`. Rich test image (see below).
  Pushed as `ghcr.io/erlang/otp/ubuntu-base:{latest,<branch>}`.
- `Dockerfile.debian-base` — default `ARG BASE=debian:bookworm`. Minimal Debian build
  image. Pushed as `ghcr.io/erlang/otp/debian-base:{latest,<branch>}`.

Base-image tag selection lives in `.github/scripts/build-base-image.sh` (`case ${BASE_TAG}`):
   `*i386-debian-base` → `BASE=i386/debian:bookworm`, built from `Dockerfile.debian-base`
   `*debian-base`      → `BASE=debian:bookworm`,    built from `Dockerfile.debian-base`
   `*ubuntu-base`      → `BASE=ubuntu:22.04`,       built from `Dockerfile.ubuntu-base`
So `ghcr.io/erlang/otp/i386-debian-base` is `Dockerfile.debian-base` built on a 32-bit
`i386/debian:bookworm` root; its `HOST_TRIP` env is what the `cross-compile` image consumes.

Derived images (`Dockerfile.<type>`), each `ARG BASE=` defaulting to the matching base:
- `64-bit` → `ubuntu-base`; `clang` → `ubuntu-base`
- `32-bit`, `cross-compile` → `i386-debian-base`; `cross-compile` final stage → `debian-base`

## API Surface (per-Dockerfile purpose)
- **`Dockerfile.ubuntu-base`** — installs full build+test toolchain: gcc/build-essential,
  autoconf, clang-format, OpenJDK 8 & 11 (Java 8 default; fop docs need it), wx 3.0 GTK3,
  unixodbc, postgresql (user `odbctest`), apache2, sshd keys, Xvfb, tmux 3.3a (terminal tests),
  gdb, valgrind deps. Installs the previous three Erlang releases via `kerl` (for
  backwards-compat testing; `LATEST`/`LATEST_VSN`/`LATEST_RELEASE` files in `$HOME`), plus
  elixir/hex, rebar3, proper. Creates user (`USER=otptest`, `GROUP=uucp`, `uid=$(id -u)`),
  `SUDO NOPASSWD`, `/buildroot /tests /otp` dirs. `ENTRYPOINT ["/bin/bash","-l","-c"]`.
- **`Dockerfile.debian-base`** — minimal Debian base: build-essential, m4, autoconf,
  default-jdk, wx 3.2, sctp, unixodbc. Parameterized by `ARG HOST_TRIP` (default
  `x86_64-linux-gnu`); when it differs from the image's native triplet it installs
  `crossbuild-essential-$HOST_ARCH` + multiarch libs and assembles `/buildroot/sysroot`.
- **`Dockerfile.64-bit`** — the primary image, tagged `otp`, used by nearly all Linux jobs.
  Sets OSSF-recommended hardening `CFLAGS`/`LDFLAGS` (SKIPPED_OSSF_* ENVs list the flags
  deliberately omitted and why), `ERL_TOP=/buildroot/otp`, install prefix `/Erlang ∅⊤℞`,
  `./configure --enable-pie`, `make && make docs`, `install install-docs`, then **resets
  CFLAGS to `-O2 -g`** (testcases don't compile with `-Werror`). `MAKEFLAGS` is a build arg.
- **`Dockerfile.32-bit`** — 32-bit build on `i386-debian-base`; `configure --with-ssl
  --prefix=/otp`, `otp_build download_gdb_tools`, `make install`. `ERLC_USE_SERVER=yes`.
- **`Dockerfile.clang`** — clang toolchain (`CC=clang CXX=clang++`) plus
  `ERL_COMPILER_OPTIONS=ssalint` (SSA lint check). Needs explicit
  `--with-ssl-lib-subdir=lib/x86_64-linux-gnu` (clang doesn't infer it like gcc). Also builds
  the `emu` flavor (`make FLAVOR=emu`).
- **`Dockerfile.cross-compile`** — multi-stage 32→64-bit cross build. `build` stage on
  `i386-debian-base`: builds a bootstrap OTP, then a cross OTP using
  `HOST_TRIP`-prefixed tools + `--sysroot=/buildroot/sysroot`, then compiles the cross test
  suite (`otp_build tests` + `ts:compile_testcases`). Final stage on `debian-base` copies
  `/otp` and `/tests` from `build`.

## Build context & ARGs (constraints)
- **Build context is `.github/`, NOT the repo root.** COPY paths are therefore
  `dockerfiles/<file>`; `ARG ARCHIVE=./otp.tar.gz` resolves to `.github/otp.tar.gz`.
- Never bake repo source in besides the prebuilt `otp.tar.gz` (produced by
  `.github/scripts/restore-from-prebuilt.sh` / `otp_build`); the image build is the only
  consumer of that tar.
- Common `ARG`s: `BASE` (base image), `ARCHIVE` (prebuilt tar path within context),
  `MAKEFLAGS` (parallelism, e.g. `-j$(nproc+2)`), and — for `debian-base` — `HOST_TRIP`.
- All `.github/`-dir build invocations pass `--file ".github/dockerfiles/Dockerfile.${TYPE}" .github/`;
  the Dockerfile suffix is exactly the CI "type" string.
- Never edit generated artifacts; these Dockerfiles are hand-written sources.

## Config files (installed by `Dockerfile.ubuntu-base`)
- `.profile` → `/home/otptest/.profile` — one-shot container init (starts sshd, postgres,
  Xvfb `:99`; prepends kerl Erlang releases to `PATH`). Exported per-run env (MAKEFLAGS,
  ERL_TOP, PATH, ERL_LIBS) is appended to it by `Dockerfile.64-bit`.
- `odbc.ini` → `/etc/odbc.ini`; `odbcinst.ini` → `/etc/odbcinst.ini` — PostgreSQL ODBC DSNs
  (`odbctest`) for the `odbc` app tests (64- and 32-bit driver entries).
- `log4j.properties` → `$HOME/` — log4j config for Apache FOP while building docs; wired via
  `JAVA_ARGS=-Dlog4j.configuration=file://...`.

## Consumers (which workflow uses which image type)
- **`64-bit`** (default `TYPE` in the `build-base-image` action): `reusable-pack.yaml`
  (builds image at `.github/dockerfiles/Dockerfile.64-bit`, `BUILD_IMAGE: false`),
  `reusable-build-flavours.yaml` (`BUILD_IMAGE: true`), `reusable-test.yaml`,
  `reusable-system-test.yaml`, `reusable-documentation.yaml`, `reusable-sbom.yaml`,
  `reusable-static-analysis.yaml`, `ossf-compiler-flags-scanner.yaml`, and
  `.github/scripts/sync-github-releases.sh`.
- **`32-bit` / `cross-compile` / `clang`**: matrix in `reusable-build-otp.yaml`
  (invoked from `main.yaml`).
- **Base images**: `.github/actions/build-base-image/action.yaml` (composite,
  builds/tags/caches) → `.github/scripts/build-base-image.sh`; refreshed daily by
  `update-base.yaml` (matrix over `debian-base`, `ubuntu-base`, `i386-debian-base`).

## Rebuilding locally
Place a prebuilt tar at `.github/otp.tar.gz` (or pass `--build-arg ARCHIVE=<path>`), then:
```
docker build --file .github/dockerfiles/Dockerfile.64-bit \
  --build-arg MAKEFLAGS=-j$(nproc) .github/
```
Base images: `.github/scripts/build-base-image.sh <branch> [<type>]` (e.g. `master 64-bit`).

## Notes for Agents
- `64-bit` is the default type; most Linux jobs run inside that single `otp` image.
- `i386-debian-base` has no Dockerfile of its own — it is `Dockerfile.debian-base` built
  against `i386/debian:bookworm` (see `build-base-image.sh`).
- `.github/scripts/ossf-sarif-generator.es` emits SARIF `uri`s of
  `.github/docker/Dockerfile.64-bit` (path says `docker/`, not `dockerfiles/`) — the OSSF
  flag audit reads `Dockerfile.64-bit`'s CFLAGS; keep the SKIPPED_OSSF_* lists accurate.

## See Also
- `.github/actions/build-base-image/` — composite action that wraps base-image build/cache.
- `.github/scripts/build-base-image.sh` — base-image tag → Dockerfile mapping and build args.
- `.github/scripts/restore-from-prebuilt.sh` — produces the `otp.tar.gz` COPYed as `ARCHIVE`.
- `.github/workflows/` — all workflows that build these images and `docker run otp`.
- `HOWTO/INSTALL.md` — general OTP build/install docs (non-container).
