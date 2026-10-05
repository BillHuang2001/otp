# `.github/actions` — In-repo composite GitHub Actions

## Intent
Holds the repository's *local* (in-repo) composite actions.
Workflows consume them by path (`uses: ./.github/actions/<dir>`), so there is no publishing, versioning or semver: the directory name is the action's identity and every change is effective immediately on the default branch.
Both actions exist to support the Linux build/test pipeline: one produces the `otp` Docker image everything else runs inside, the other produces a SARIF report for the OpenSSF compiler-flags scanner.

## API Surface
Each action is a single `action.yaml` (composite) in its own directory.

### `build-base-image/action.yaml` — "Builds the base testing image (using cache if possible)"
Inputs (all strings):
- `BASE_BRANCH` — **required**; branch or tag name (`master`, `maint-27`, `OTP-27.2.1`, …). Passed as `$1` to `build-base-image.sh`.
- `TYPE` — default `64-bit`. Must equal an existing `.github/dockerfiles/Dockerfile.<TYPE>` (current: `64-bit`, `32-bit`, `clang`, `cross-compile`). Passed as `$2`.
- `BUILD_IMAGE` — default `true`; compared as the string `'true'`.
- `github_token` — default `${{ github.token }}`; used for the ghcr.io docker login.
No declared outputs: `build-base-image.sh` writes `BASE`, `BASE_TAG`, `BASE_TYPE`, `BASE_BUILD` to `$GITHUB_OUTPUT`, but the action does not re-export them (only the direct-script caller in `update-base.yaml` can read `steps.<id>.outputs.*`).

Behavior / side effects (in order):
1. `.github/scripts/cleanup_gh_runner.sh` — frees runner disk (removes `/usr/share/dotnet`, `/opt/ghc`, `/usr/local/share/boost`, `$AGENT_TOOLSDIRECTORY`; `docker system prune -a -f`).
2. Computes `date=$(date +'%Y-%m-%d')` for daily cache invalidation.
3. `actions/cache` of `otp_docker_base.tar`, keyed on `hashFiles('.github/dockerfiles/Dockerfile.ubuntu-base', '.github/scripts/build-base-image.sh')` + date + `hashFiles('OTP_VERSION')` — **only for `TYPE` in {`64-bit`, `clang`}**.
4. `docker/login-action` to `ghcr.io` as `github.repository_owner` with the token.
5. `.github/scripts/build-base-image.sh "$BASE_BRANCH" "$TYPE"` — loads `otp_docker_base.tar` if present, else builds/pulls the base image; writes `otp_docker_base.tar` only when the image ID actually changed.
6. If `BUILD_IMAGE == 'true'`: caches/restores `otp_src.tar.gz` (key `prebuilt-src-<ref_name>-<sha>`) falling back to the `otp_prebuilt` artifact, then does the same for `otp_cache.tar.gz` (key `prebuilt-cache-<TYPE>-<ref_name>-<sha>`); finally runs `.github/scripts/restore-from-prebuilt.sh $PWD .github/otp.tar.gz`, removes `otp_{src,cache}.tar.gz` and runs `docker build --tag otp --build-arg MAKEFLAGS=-j$(nproc+2) --file .github/dockerfiles/Dockerfile.${TYPE} .github/`.

**The contract with callers is the docker image tagged `otp`** (built from context `.github/`), which downstream steps use as `docker run otp …`.

### `ossf-compiler-flags-scanner/action.yaml` — "Open Source Security Foundation"
Inputs: `upload` — default `false`; compared as the string `'true'`.
Behavior: `actions/checkout` of the foreign repo `ossf/wg-best-practices-os-developers` (sparse: `docs/Compiler-Hardening-Guides/compiler-options-scraper`, `path: ossf`, `persist-credentials: false`); `pip3 install` of the scraper's `requirements.txt` and `python3 …/main.py` on the runner producing `compiler-options.json`; then `docker run -v $PWD/.github/scripts:/github --entrypoint "" otp bash -c "/github/ossf-sarif-generator.es '<json>'" > results.sarif`; uploads `results.sarif` as the artifact `SARIF file` (always, `if: !cancelled()`); and when `upload == 'true'` runs `github/codeql-action/upload-sarif`.
No declared outputs. Requires the `otp` image to already exist in the job (i.e. `build-base-image` must run first) and `security-events: write` for the codeql step.

## Consumers (who calls what)
`build-base-image`:
- `reusable-build-flavours.yaml`, `reusable-static-analysis.yaml`, `reusable-test.yaml`, `reusable-system-test.yaml`, `reusable-documentation.yaml`, `reusable-sbom.yaml` (twice) — `BASE_BRANCH` only, `BUILD_IMAGE` default `true`.
- `reusable-build-otp.yaml` — with `TYPE: ${{ matrix.type }}`, matrix `[32-bit, cross-compile, clang]`.
- `reusable-pack.yaml` — with `BUILD_IMAGE: false` (it is the job that *produces* the `otp_prebuilt` artifact, so it must not consume it).
- `ossf-compiler-flags-scanner.yaml` — `BASE_BRANCH: master`.
- Not via the action: `update-base.yaml` calls `.github/scripts/build-base-image.sh "<branch>"` directly with `BASE_TAG`/`BASE_USE_CACHE=false` preset and then `docker push`es the result to ghcr.io (that workflow has `packages: write`).

`ossf-compiler-flags-scanner`:
- `reusable-static-analysis.yaml` — `upload: ${{ github.repository == 'erlang/otp' || github.event_name != 'push' }}`.
- `ossf-compiler-flags-scanner.yaml` — `upload: true`.

All of the above are reached from `.github/workflows/main.yaml` (jobs `pack`, `build`, `build-flavors`, `static`, `documentation`, tests) plus the standalone scanner workflow.

## Constraints
- Directory name = action name; one `action.yaml` per directory. Callers use `./.github/actions/<dir>` and must `actions/checkout` first (the action assumes cwd is the repo root and references scripts by repo-relative path).
- Composite steps need an explicit `shell:`; repo convention is `bash` or `bash -eo pipefail {0}`.
- All `uses:` of third-party actions must be pinned to a full commit SHA with a trailing `# ratchet:<owner>/<repo>@<version>` comment (enforced by zizmor, updated by `.github/scripts/update-gh-actions-versions.es` / Renovate). Never edit a pin by hand.
- Inputs are strings — compare with `inputs.X == 'true'`, do not rely on YAML booleans.
- `TYPE` must have a matching `.github/dockerfiles/Dockerfile.<TYPE>`; adding a type means adding the Dockerfile **and** a workflow matrix entry. `Dockerfile.ubuntu-base` / `Dockerfile.debian-base` are the *base* images and are not valid `TYPE` values.
- Pass secrets/tokens via the `github_token` input pattern (`default: ${{ github.token }}`), not inline.

## Notes for Agents
- Daily cache invalidation exists so kerl/`apt-get upgrade` drift inside the base image does not leave a stale cache hit; a cache miss is normal after UTC midnight.
- `BASE_BUILD` in `$GITHUB_OUTPUT` distinguishes `loaded` / `cached` / `re-built`; `otp_docker_base.tar` is only (re)written on `re-built`, which is why the cache entry is keyed partly on the Dockerfile hash.
- The prebuilt path is run-scoped: `otp_prebuilt` is uploaded by the `pack` job of the *same* workflow run, so `build-base-image` cannot be reused outside a run that produced it.
- `docker/login-action` here only authenticates; the action never pushes. Pulls of `ghcr.io/erlang/otp/<base>:<branch>` are best-effort inside the script (`docker pull` guarded by `if`), so runs in forks / without package access still work.
- The ossf action's `SKIPPED_OSSF_CFLAGS` / `SKIPPED_OSSF_LDFLAGS` env vars are defined in `Dockerfile.64-bit` and dereferenced unguarded by `ossf-sarif-generator.es` (`os:getenv(...)` concatenated with `++`), so the scanner only works against the default `64-bit` image.
- The scanner installs Python packages on the runner (not inside the image), i.e. it depends on network + `pip3` on `ubuntu-latest`.

## See Also
- `.github/scripts/` — the scripts these actions drive (`build-base-image.sh`, `cleanup_gh_runner.sh`, `restore-from-prebuilt.sh`, `ossf-sarif-generator.es`) (sibling — read-only, escalate writes to `../`).
- `.github/dockerfiles/` — `Dockerfile.<TYPE>` definitions; the `TYPE` input is a direct filename lookup (sibling — read-only).
- `.github/workflows/` — the consumers listed above; workflow-level `permissions` (e.g. `security-events: write`, `actions: write`) belong there (sibling — read-only).

## Routing Table
- `build-base-image/` → single-file composite action (`action.yaml`): builds/caches the base image and the `otp` Docker image used by all Linux build/test jobs.
- `ossf-compiler-flags-scanner/` → single-file composite action (`action.yaml`): runs the OpenSSF compiler-flags comparison inside the `otp` image and emits/optionally uploads `results.sarif`.
